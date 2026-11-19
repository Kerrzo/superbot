//! Native OpenAI-compatible Chat Completions harness.

use super::{AbortReason, AgentEvent, ModelOption, RunInput};
use eventsource_stream::{EventStreamError, Eventsource};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::watch;
use tokio_stream::StreamExt;

pub const NAME: &str = "custom";
const HISTORY_TURNS: usize = 12;
const MAX_TEXT_BYTES: usize = 256 * 1024;
const MAX_TOOL_ROUNDS: usize = 8;
const MAX_CALLS: usize = 8;

pub struct Provider {
    client: Client,
    endpoint: Url,
    model: String,
    authorization: Option<HeaderValue>,
    tools: bool,
}

impl Provider {
    pub fn from_env() -> Result<Option<Self>, String> {
        Self::from_settings(|name| std::env::var(name).ok())
    }

    fn from_settings(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, String> {
        let setting = |name| {
            get(name)
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        };
        let base = setting("CUSTOM_OPENAI_BASE_URL");
        let model = setting("CUSTOM_OPENAI_MODEL");
        let key_file = setting("CUSTOM_OPENAI_API_KEY_FILE");
        if base.is_none() && model.is_none() && key_file.is_none() {
            return Ok(None);
        }
        let base = base.ok_or("Set CUSTOM_OPENAI_BASE_URL for the custom provider.")?;
        let model = model.ok_or("Set CUSTOM_OPENAI_MODEL for the custom provider.")?;
        let mut endpoint = Url::parse(&base).map_err(|_| "Invalid CUSTOM_OPENAI_BASE_URL.")?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || endpoint.host().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err("CUSTOM_OPENAI_BASE_URL must be an HTTP(S) URL without user info, query or fragment.".into());
        }
        endpoint
            .path_segments_mut()
            .map_err(|_| "Invalid CUSTOM_OPENAI_BASE_URL.")?
            .pop_if_empty()
            .extend(["chat", "completions"]);
        let timeout = |name, default| -> Result<Duration, String> {
            let seconds = match setting(name) {
                Some(s) => s
                    .parse::<u64>()
                    .ok()
                    .filter(|n| (1..=3600).contains(n))
                    .ok_or_else(|| format!("{name} must be between 1 and 3600 seconds."))?,
                None => default,
            };
            Ok(Duration::from_secs(seconds))
        };
        let tools = match setting("CUSTOM_OPENAI_TOOLS").as_deref() {
            None | Some("true") => true,
            Some("false") => false,
            _ => return Err("CUSTOM_OPENAI_TOOLS must be true or false.".into()),
        };
        let authorization = key_file.as_deref().map(read_key).transpose()?;
        let client = Client::builder()
            .connect_timeout(timeout("CUSTOM_OPENAI_CONNECT_TIMEOUT_SECONDS", 10)?)
            .timeout(timeout("CUSTOM_OPENAI_REQUEST_TIMEOUT_SECONDS", 180)?)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .build()
            .map_err(|_| "Could not initialize the custom provider HTTP client.")?;
        Ok(Some(Self {
            client,
            endpoint,
            model,
            authorization,
            tools,
        }))
    }

    pub fn default_model(&self) -> ModelOption {
        ModelOption {
            id: self.model.clone(),
            label: self.model.clone(),
            efforts: vec![],
            aliases: vec![],
        }
    }

    pub fn instructions(&self) -> [&'static str; 4] {
        if self.tools {
            [
                "## Your tools\n- Use the functions provided for this run.",
                "- Use remember to update your own memory when you learn something lasting. Supply the full updated memory, including your job, preferences, rules, and a short work log. Keep it short. Never store passwords, keys, or card numbers.",
                "## Routines\n- Use list_routines and add_routine to manage scheduled work. Cron uses five fields in the server's time zone.",
                "- Use list_bots and ask_bot to consult another Bot. Their reply may arrive later as a new message.",
            ]
        } else {
            [
                "## Your capabilities\n- This run supports text conversation.",
                "- Your existing memory and shared user facts are included below.",
                "",
                "",
            ]
        }
    }

    pub async fn run(
        &self,
        input: RunInput,
        mut abort: watch::Receiver<AbortReason>,
        mut emit: impl FnMut(AgentEvent),
    ) {
        tokio::select! {
            biased;
            _ = abort.wait_for(|reason| reason.is_some()) => {}
            result = self.complete(&input, &mut emit) => {
                match result {
                    Ok(text) => emit(AgentEvent::Text(text)),
                    Err(message) => emit(AgentEvent::Error(message)),
                }
            }
        }
    }

    async fn complete(
        &self,
        input: &RunInput,
        emit: &mut impl FnMut(AgentEvent),
    ) -> Result<String, String> {
        let path = input.state_dir.join("custom-history.json");
        let mut history = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<Vec<Turn>>(&bytes)
                .map_err(|_| "Custom provider conversation history is invalid.")?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(_) => return Err("Could not read custom provider conversation history.".into()),
        };
        let model = input.model.as_deref().unwrap_or(&self.model);
        let mut messages = vec![json!({"role": "system", "content": input.system})];
        for turn in &history {
            messages.push(json!({"role": "user", "content": turn.prompt}));
            messages.push(json!({"role": "assistant", "content": turn.reply}));
        }
        messages.push(json!({"role": "user", "content": input.prompt}));
        let tools = self.tools.then(tool_definitions);
        let mut reply = String::new();
        let mut call_ids = HashSet::new();

        for round in 0..=MAX_TOOL_ROUNDS {
            let mut body = json!({"model": model, "messages": messages, "stream": true});
            if let Some(tools) = &tools {
                body["tools"] = json!(tools);
            }
            let completion = self.stream(&body, &mut reply, emit).await?;
            match completion.finish.as_deref() {
                Some("stop") | None if completion.calls.is_empty() && !reply.trim().is_empty() => {
                    history.push(Turn { prompt: input.prompt.clone(), reply: reply.clone() });
                    trim_history(&mut history);
                    let bytes = serde_json::to_vec(&history)
                        .map_err(|_| "Could not encode custom provider conversation history.")?;
                    write_private(&path, &bytes)
                        .map_err(|_| "Could not save custom provider conversation history.")?;
                    return Ok(reply);
                }
                Some("tool_calls") if !completion.calls.is_empty() && self.tools => {
                    if round == MAX_TOOL_ROUNDS {
                        return Err("Custom provider reached the tool call limit for this turn.".into());
                    }
                    if completion.calls.values().any(|call| {
                        call.id.is_empty() || call.name.is_empty() || !call_ids.insert(call.id.clone())
                    }) {
                        return Err("Custom provider returned incomplete or duplicate tool calls.".into());
                    }
                    let calls: Vec<_> = completion.calls.values().map(ToolCall::message).collect();
                    let content = if completion.text.is_empty() { Value::Null } else { json!(completion.text) };
                    messages.push(json!({"role": "assistant", "content": content, "tool_calls": calls}));
                    for call in completion.calls.into_values() {
                        let result = execute_tool(input, &call, emit).await;
                        messages.push(json!({"role": "tool", "tool_call_id": call.id, "content": result}));
                    }
                }
                Some("length") => return Err("Custom provider response hit the model's output or context limit.".into()),
                Some("content_filter") => return Err("Custom provider filtered the response.".into()),
                _ if !completion.calls.is_empty() || completion.finish.as_deref() == Some("tool_calls") =>
                    return Err("Custom provider returned unsupported or incomplete tool calls.".into()),
                _ => return Err("Custom provider returned no usable text. Check the model and its Chat Completions support.".into()),
            }
        }
        unreachable!("a final tool round always returns")
    }

    async fn stream(
        &self,
        body: &Value,
        reply: &mut String,
        emit: &mut impl FnMut(AgentEvent),
    ) -> Result<Completion, String> {
        let mut request = self
            .client
            .post(self.endpoint.clone())
            .header(ACCEPT, "text/event-stream")
            .json(body);
        if let Some(authorization) = &self.authorization {
            request = request.header(AUTHORIZATION, authorization.clone());
        }
        let response = request.send().await.map_err(transport_error)?;
        let status = response.status();
        if !status.is_success() {
            return Err(status_error(status, self.tools));
        }
        if !response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .is_some_and(|kind| {
                kind.split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .eq_ignore_ascii_case("text/event-stream")
            })
        {
            return Err("Custom provider did not return a Chat Completions event stream.".into());
        }

        let mut stream = response.bytes_stream().eventsource();
        let mut completion = Completion::default();
        while let Some(event) = stream.next().await {
            let event = event.map_err(|e| match e {
                EventStreamError::Transport(e) => transport_error(e),
                _ => "Custom provider sent an invalid event stream.".into(),
            })?;
            if event.data.trim() == "[DONE]" {
                return Ok(completion);
            }
            let chunk: Value = serde_json::from_str(&event.data)
                .map_err(|_| "Custom provider sent an invalid Chat Completions event.")?;
            if event.event == "error" || chunk.get("error").is_some() {
                return Err(
                    "Custom provider reported a streaming error. Check the inference server."
                        .into(),
                );
            }
            completion.push(&chunk, reply, emit)?;
        }
        if completion.finish.is_some() {
            Ok(completion)
        } else {
            Err("Custom provider disconnected before completing its response.".into())
        }
    }
}

fn read_key(path: &str) -> Result<HeaderValue, String> {
    let file = File::open(path).map_err(|_| "Could not open CUSTOM_OPENAI_API_KEY_FILE.")?;
    let meta = file
        .metadata()
        .map_err(|_| "Could not inspect CUSTOM_OPENAI_API_KEY_FILE.")?;
    if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 || meta.len() > 4096 {
        return Err("CUSTOM_OPENAI_API_KEY_FILE must be a private regular file (chmod 600, at most 4096 bytes).".into());
    }
    let mut key = String::new();
    file.take(4097)
        .read_to_string(&mut key)
        .map_err(|_| "Could not read CUSTOM_OPENAI_API_KEY_FILE.")?;
    let key = key.trim();
    if key.is_empty() || key.len() > 4096 || key.chars().any(char::is_whitespace) {
        return Err("CUSTOM_OPENAI_API_KEY_FILE must contain one valid bearer token.".into());
    }
    let mut value = HeaderValue::from_str(&format!("Bearer {key}"))
        .map_err(|_| "CUSTOM_OPENAI_API_KEY_FILE must contain one valid bearer token.")?;
    value.set_sensitive(true);
    Ok(value)
}

fn transport_error(error: reqwest::Error) -> String {
    if error.is_timeout() {
        "Custom provider request timed out. Check the inference server or increase the timeout."
            .into()
    } else if error.is_connect() {
        "Could not connect to the custom provider. Check its address, TLS certificate and availability.".into()
    } else {
        "Custom provider connection failed while receiving a response.".into()
    }
}

fn status_error(status: StatusCode, tools: bool) -> String {
    let hint = match status.as_u16() {
        400 | 422 if tools => {
            "Check the model identifier and context limit. For models without function calling, set CUSTOM_OPENAI_TOOLS=false."
        }
        400 | 422 => "Check the model identifier, context limit and Chat Completions support.",
        401 | 403 => "Check the provider API key and its permissions.",
        404 => "Check the API base URL and verify the model exists and is available.",
        429 => "The provider is busy or has rate-limited requests.",
        500..=599 => {
            "The inference server failed. Check that it is running and the model is available."
        }
        300..=399 => "Use the final API base URL directly.",
        _ => "Check the inference server configuration.",
    };
    format!("Custom provider returned HTTP {}. {hint}", status.as_u16())
}

#[derive(Serialize, Deserialize)]
struct Turn {
    prompt: String,
    reply: String,
}

fn trim_history(history: &mut Vec<Turn>) {
    let mut bytes = 0;
    let keep = history
        .iter()
        .rev()
        .take(HISTORY_TURNS)
        .take_while(|turn| {
            bytes += turn.prompt.len() + turn.reply.len();
            bytes <= MAX_TEXT_BYTES
        })
        .count();
    history.drain(..history.len() - keep);
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", super::new_uuid()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    let result = file.write_all(bytes).and_then(|_| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

#[derive(Default)]
struct ToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl ToolCall {
    fn message(&self) -> Value {
        json!({"id": self.id, "type": "function", "function": {"name": self.name, "arguments": self.arguments}})
    }
}

#[derive(Default)]
struct Completion {
    text: String,
    calls: BTreeMap<u64, ToolCall>,
    finish: Option<String>,
}

impl Completion {
    fn push(
        &mut self,
        chunk: &Value,
        reply: &mut String,
        emit: &mut impl FnMut(AgentEvent),
    ) -> Result<(), String> {
        let choices = chunk["choices"]
            .as_array()
            .ok_or("Custom provider sent a chunk without choices.")?;
        let Some(choice) = choices
            .iter()
            .find(|choice| choice["index"].as_u64().unwrap_or(0) == 0)
        else {
            return Ok(());
        };
        let delta = &choice["delta"];
        if let Some(content) = delta["content"].as_str().filter(|s| !s.is_empty()) {
            if reply.len() + content.len() > MAX_TEXT_BYTES {
                return Err("Custom provider response exceeded the text size limit.".into());
            }
            self.text.push_str(content);
            reply.push_str(content);
            emit(AgentEvent::TextDelta(content.into()));
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for part in calls {
                let index = part["index"].as_u64().filter(|&i| i < MAX_CALLS as u64)
                    .ok_or("Custom provider returned an invalid tool call index or too many tool calls.")?;
                if part["type"].as_str().is_some_and(|kind| kind != "function") {
                    return Err("Custom provider returned an unsupported tool type.".into());
                }
                let call = self.calls.entry(index).or_default();
                call.id.push_str(super::str_of(part, "id"));
                call.name.push_str(super::str_of(&part["function"], "name"));
                call.arguments
                    .push_str(super::str_of(&part["function"], "arguments"));
                if call.id.len() > 256 || call.name.len() > 64 || call.arguments.len() > 64 * 1024 {
                    return Err("Custom provider tool call exceeded the size limit.".into());
                }
            }
        }
        if let Some(finish) = choice["finish_reason"].as_str() {
            self.finish = Some(finish.into());
        }
        Ok(())
    }
}

fn tool_definitions() -> Vec<Value> {
    [
        ("remember", "Replace your own durable memory with the full updated Markdown.", &["markdown"][..]),
        ("list_bots", "List the other Bots on this server.", &[][..]),
        ("ask_bot", "Ask another Bot for help and return its reply.", &["bot", "message"][..]),
        ("list_routines", "List your scheduled routines.", &[][..]),
        ("add_routine", "Schedule a recurring task using five-field local-time cron.", &["name", "cron", "instruction"][..]),
    ].into_iter().map(|(name, description, fields)| {
        let properties: serde_json::Map<String, Value> =
            fields.iter().map(|field| (field.to_string(), json!({"type": "string"}))).collect();
        json!({"type": "function", "function": {
            "name": name, "description": description,
            "parameters": {"type": "object", "properties": properties, "required": fields, "additionalProperties": false}
        }})
    }).collect()
}

async fn execute_tool(
    input: &RunInput,
    call: &ToolCall,
    emit: &mut impl FnMut(AgentEvent),
) -> String {
    let Ok(Value::Object(arguments)) = serde_json::from_str(&call.arguments) else {
        return "Invalid tool arguments. Send a JSON object.".into();
    };
    let field = |name| {
        arguments
            .get(name)
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
    };
    let args: Vec<&str> = match (call.name.as_str(), arguments.len()) {
        ("remember", 1) => {
            let Some(markdown) = field("markdown").filter(|s| s.len() <= 32 * 1024) else {
                return "Provide the complete memory as a Markdown string up to 32 KiB.".into();
            };
            emit(AgentEvent::Tool {
                name: "Updating memory".into(),
                detail: String::new(),
            });
            return match write_private(&input.state_dir.join("MEMORY.md"), markdown.as_bytes()) {
                Ok(()) => "Memory updated.".into(),
                Err(_) => "Could not write the Bot's memory.".into(),
            };
        }
        ("list_bots", 0) => vec!["bots"],
        ("list_routines", 0) => vec!["routines"],
        ("ask_bot", 2) => {
            let (Some(bot), Some(message)) = (field("bot"), field("message")) else {
                return "Provide bot and message strings.".into();
            };
            vec!["ask", bot, message]
        }
        ("add_routine", 3) => {
            let (Some(name), Some(cron), Some(instruction)) =
                (field("name"), field("cron"), field("instruction"))
            else {
                return "Provide name, cron, and instruction strings.".into();
            };
            vec!["routine", "add", name, cron, instruction]
        }
        _ => return "Unknown tool or invalid arguments.".into(),
    };
    emit(AgentEvent::Tool {
        name: call.name.replace('_', " "),
        detail: String::new(),
    });
    let env = input
        .env
        .iter()
        .filter(|(key, _)| matches!(key.as_str(), "BOT_API" | "BOT_ID" | "BOT_TOKEN"));
    let Ok(exe) = std::env::current_exe() else {
        return "Could not locate botctl.".into();
    };
    let output = Command::new(exe.with_file_name("botctl"))
        .args(args)
        .env_clear()
        .envs(env)
        .kill_on_drop(true)
        .output()
        .await;
    match output {
        Ok(output) => {
            let bytes = if output.status.success() {
                &output.stdout
            } else {
                &output.stderr
            };
            String::from_utf8_lossy(&bytes[..bytes.len().min(32 * 1024)])
                .trim()
                .to_owned()
        }
        Err(_) => "Could not run botctl.".into(),
    }
}

#[cfg(test)]
mod tests;

use super::*;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("superbot-test-{}", super::super::new_uuid()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn input(&self, prompt: &str) -> RunInput {
        RunInput {
            prompt: prompt.into(),
            system: "You are a test employee.".into(),
            session_id: None,
            cwd: self.0.clone(),
            model: None,
            effort: None,
            env: Default::default(),
            mcp_available: false,
            state_dir: self.0.clone(),
        }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct Exchange {
    requests: Vec<(HeaderMap, Value)>,
    responses: VecDeque<Response>,
}

struct Mock {
    base_url: String,
    exchange: Arc<Mutex<Exchange>>,
    task: JoinHandle<()>,
}

impl Mock {
    async fn start(responses: Vec<Response>) -> Self {
        let exchange = Arc::new(Mutex::new(Exchange {
            responses: responses.into(),
            ..Default::default()
        }));
        let handler = |State(exchange): State<Arc<Mutex<Exchange>>>, headers, axum::Json(body)| async move {
            let mut exchange = exchange.lock().unwrap();
            exchange.requests.push((headers, body));
            exchange
                .responses
                .pop_front()
                .unwrap_or_else(|| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        };
        let app = Router::new()
            .route("/inference/v1/chat/completions", post(handler))
            .with_state(exchange.clone());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let base_url = format!("http://{}/inference/v1/", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            base_url,
            exchange,
            task,
        }
    }

    fn provider(&self, extra: &[(&str, &str)]) -> Provider {
        settings(
            &[
                &[
                    ("CUSTOM_OPENAI_BASE_URL", self.base_url.as_str()),
                    ("CUSTOM_OPENAI_MODEL", "default-model"),
                ],
                extra,
            ]
            .concat(),
        )
        .unwrap()
        .unwrap()
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn settings(values: &[(&str, &str)]) -> Result<Option<Provider>, String> {
    Provider::from_settings(|name| {
        values
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| (*v).into())
    })
}

fn sse(chunks: impl IntoIterator<Item = Bytes, IntoIter: Send + 'static>, stall: bool) -> Response {
    let parts = tokio_stream::iter(chunks.into_iter().map(Ok::<_, Infallible>));
    let body = if stall {
        Body::from_stream(parts.chain(tokio_stream::pending()))
    } else {
        Body::from_stream(parts)
    };
    ([(CONTENT_TYPE, "text/event-stream; charset=utf-8")], body).into_response()
}

fn event(delta: Value, finish: Option<&str>) -> String {
    format!(
        "data: {}\r\n\r\n",
        json!({"choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
    )
}

fn text_response(text: &str) -> Response {
    sse(
        vec![
            Bytes::from(event(json!({"content": text}), Some("stop"))),
            Bytes::from_static(b"data: [DONE]\n\n"),
        ],
        false,
    )
}

async fn run(provider: &Provider, input: RunInput) -> Vec<AgentEvent> {
    let (_sender, receiver) = watch::channel(None);
    let mut events = Vec::new();
    provider
        .run(input, receiver, |event| events.push(event))
        .await;
    events
}

#[test]
fn configuration_validates_url_timeouts_and_private_credentials() {
    assert!(settings(&[]).unwrap().is_none());
    assert!(settings(&[("CUSTOM_OPENAI_BASE_URL", "http://localhost")]).is_err());
    for base in [
        "https://user:secret@host/v1",
        "https://host/v1?key=secret",
        "https://host/v1#secret",
        "file:///somewhere",
    ] {
        let Err(error) = settings(&[
            ("CUSTOM_OPENAI_BASE_URL", base),
            ("CUSTOM_OPENAI_MODEL", "a"),
        ]) else {
            panic!("accepted invalid URL");
        };
        assert!(!error.contains("secret"));
    }
    let base = [
        ("CUSTOM_OPENAI_BASE_URL", "http://localhost:1234/v1"),
        ("CUSTOM_OPENAI_MODEL", "a"),
    ];
    for invalid in [
        ("CUSTOM_OPENAI_CONNECT_TIMEOUT_SECONDS", "0"),
        ("CUSTOM_OPENAI_REQUEST_TIMEOUT_SECONDS", "3601"),
        ("CUSTOM_OPENAI_TOOLS", "yes"),
    ] {
        assert!(settings(&[base[0], base[1], invalid]).is_err());
    }
    let temp = TempDir::new();
    let key = temp.0.join("provider.key");
    fs::write(&key, "test-key\n").unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(read_key(key.to_str().unwrap()).is_err());
    fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    let auth = read_key(key.to_str().unwrap()).unwrap();
    assert!(auth.is_sensitive());
    assert_eq!(auth, "Bearer test-key");
    fs::write(&key, "test-key\nsecond-line").unwrap();
    assert!(read_key(key.to_str().unwrap()).is_err());
}

#[tokio::test]
async fn streams_unicode_and_restores_completed_history_with_tools_off() {
    let chunks = event(json!({"content": "Hi 🦘"}), None)
        + &event(json!({"content": " there"}), Some("stop"));
    // Split inside a UTF-8 code point and inside the terminating SSE frame.
    let split = chunks.find('🦘').unwrap() + 2;
    let mock = Mock::start(vec![
        sse(
            vec![
                Bytes::copy_from_slice(&chunks.as_bytes()[..split]),
                Bytes::copy_from_slice(&chunks.as_bytes()[split..]),
                Bytes::from_static(b"data: [DO"),
                Bytes::from_static(b"NE]\n\n"),
            ],
            false,
        ),
        text_response("I remember."),
    ])
    .await;
    let temp = TempDir::new();
    let mut first = temp.input("Remember this.");
    first.model = Some("model-override".into());
    let events = run(&mock.provider(&[("CUSTOM_OPENAI_TOOLS", "false")]), first).await;
    assert!(matches!(events.as_slice(), [
        AgentEvent::TextDelta(a), AgentEvent::TextDelta(b), AgentEvent::Text(c)
    ] if a == "Hi 🦘" && b == " there" && c == "Hi 🦘 there"));
    let mut second = temp.input("What did I say?");
    second.system = "These are fresh instructions.".into();
    let events = run(&mock.provider(&[("CUSTOM_OPENAI_TOOLS", "false")]), second).await;
    assert!(matches!(events.last(), Some(AgentEvent::Text(text)) if text == "I remember."));
    let exchange = mock.exchange.lock().unwrap();
    assert_eq!(exchange.requests.len(), 2);
    let (headers, first) = &exchange.requests[0];
    assert!(!headers.contains_key(AUTHORIZATION));
    assert_eq!(headers[ACCEPT], "text/event-stream");
    assert_eq!(first["model"], "model-override");
    assert_eq!(first["messages"][0]["content"], "You are a test employee.");
    assert_eq!(first["stream"], true);
    assert!(first.get("tools").is_none());
    let second = &exchange.requests[1].1;
    assert_eq!(second["model"], "default-model");
    assert_eq!(
        second["messages"],
        json!([
            {"role": "system", "content": "These are fresh instructions."},
            {"role": "user", "content": "Remember this."},
            {"role": "assistant", "content": "Hi 🦘 there"},
            {"role": "user", "content": "What did I say?"}
        ])
    );
}

#[tokio::test]
async fn assembles_tool_calls_and_returns_results_to_the_same_endpoint() {
    let chunks = [
        event(
            json!({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
            "function": {"name": "remember", "arguments": "{\"mark"}}]}),
            None,
        ),
        event(
            json!({"tool_calls": [
                {"index": 1, "id": "call_2", "type": "function", "function": {"name": "unknown", "arguments": "{}"}},
                {"index": 0, "function": {"arguments": "down\":\"# Durable job\"}"}}
            ]}),
            Some("tool_calls"),
        ),
        "data: [DONE]\n\n".into(),
    ];
    let mock = Mock::start(vec![
        sse(chunks.map(Bytes::from), false),
        text_response("Saved."),
    ])
    .await;
    let temp = TempDir::new();
    let key = temp.0.join("provider.key");
    write_private(&key, b"test-key\n").unwrap();
    let events = run(
        &mock.provider(&[("CUSTOM_OPENAI_API_KEY_FILE", key.to_str().unwrap())]),
        temp.input("Set my job."),
    )
    .await;
    assert!(
        matches!(events.as_slice(), [AgentEvent::Tool { .. }, AgentEvent::TextDelta(_), AgentEvent::Text(text)] if text == "Saved.")
    );
    assert_eq!(
        fs::read_to_string(temp.0.join("MEMORY.md")).unwrap(),
        "# Durable job"
    );
    let exchange = mock.exchange.lock().unwrap();
    assert_eq!(exchange.requests.len(), 2);
    assert!(
        exchange
            .requests
            .iter()
            .all(|(headers, _)| headers[AUTHORIZATION] == "Bearer test-key")
    );
    assert!(
        exchange.requests[0].1["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "remember")
    );
    let messages = &exchange.requests[1].1["messages"];
    assert_eq!(
        messages[2]["tool_calls"][0]["function"]["arguments"],
        "{\"markdown\":\"# Durable job\"}"
    );
    assert_eq!(
        messages[3],
        json!({"role": "tool", "tool_call_id": "call_1", "content": "Memory updated."})
    );
    assert_eq!(
        messages[4],
        json!({"role": "tool", "tool_call_id": "call_2", "content": "Unknown tool or invalid arguments."})
    );
}

#[tokio::test]
async fn reports_clean_errors_without_retrying_or_running_truncated_calls() {
    let truncated_tool = event(
        json!({"tool_calls": [{"index": 0, "id": "call", "function": {
            "name": "remember", "arguments": "{\"markdown\":\"must not persist\"}"
        }}]}),
        None,
    );
    let mock = Mock::start(vec![
        (StatusCode::UNAUTHORIZED, "reflected-secret").into_response(),
        (StatusCode::NOT_FOUND, "missing").into_response(),
        sse(vec![Bytes::from(truncated_tool)], false),
        sse(vec![Bytes::from_static(b"data: invalid json\n\n")], false),
        (StatusCode::BAD_REQUEST, "tools are unsupported").into_response(),
    ])
    .await;
    let temp = TempDir::new();
    let provider = mock.provider(&[]);
    for expected in [
        "HTTP 401",
        "HTTP 404",
        "disconnected",
        "invalid Chat Completions event",
        "CUSTOM_OPENAI_TOOLS=false",
    ] {
        let events = run(&provider, temp.input("Try this.")).await;
        assert!(
            matches!(events.as_slice(), [AgentEvent::Error(e)] if e.contains(expected) && !e.contains("reflected-secret"))
        );
    }
    assert_eq!(mock.exchange.lock().unwrap().requests.len(), 5);
    assert!(!temp.0.join("MEMORY.md").exists());
    assert!(!temp.0.join("custom-history.json").exists());
}

#[tokio::test]
async fn times_out_and_can_stop_a_stalled_response() {
    let pending = || {
        sse(
            vec![Bytes::from(event(json!({"content": "partial"}), None))],
            true,
        )
    };
    let mock = Mock::start(vec![pending(), pending()]).await;
    let temp = TempDir::new();
    let provider = mock.provider(&[("CUSTOM_OPENAI_REQUEST_TIMEOUT_SECONDS", "1")]);
    let events = run(&provider, temp.input("Wait.")).await;
    assert!(
        matches!(events.as_slice(), [AgentEvent::TextDelta(_), AgentEvent::Error(error)] if error.contains("timed out"))
    );
    let (sender, receiver) = watch::channel(None);
    let mut seen = 0;
    provider
        .run(temp.input("Stop."), receiver, |event| {
            assert!(matches!(event, AgentEvent::TextDelta(_)));
            seen += 1;
            sender.send(Some("stopped")).unwrap();
        })
        .await;
    assert_eq!(seen, 1);
    assert!(!temp.0.join("custom-history.json").exists());
}

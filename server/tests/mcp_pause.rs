#![cfg(unix)]

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, routing::post};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const PASSWORD: &str = "fixture-admin";
const SECRET: &str = "fixture-credential-not-for-agents";

struct Directory(PathBuf);
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Server {
    child: Child,
    url: String,
    http: reqwest::Client,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    async fn start(root: &Path, inference: &str) -> Self {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let child = Command::new(env!("CARGO_BIN_EXE_botd"))
            .env_clear()
            .env("PATH", format!("{}:/bin:/usr/bin", root.display()))
            .env("FIXTURE", root)
            .env("AUTH_PASSWORD", PASSWORD)
            .env("PORT", port.to_string())
            .env("SCREENS", "off")
            .env("DATA_DIR", root.join("data"))
            .env("WORKSPACE_DIR", root.join("workspace"))
            .env("MCP_SECRETS_DIR", root.join("secrets"))
            .env("CUSTOM_OPENAI_BASE_URL", inference)
            .env("CUSTOM_OPENAI_MODEL", "fixture-model")
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let _ = rustls::crypto::ring::default_provider().install_default();
        let server = Self {
            child,
            url: format!("http://127.0.0.1:{port}"),
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
        };
        for _ in 0..100 {
            if server
                .http
                .get(format!("{}/healthz", server.url))
                .send()
                .await
                .is_ok()
            {
                return server;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("botd did not start")
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Value,
        token: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut req = self
            .http
            .request(method.parse().unwrap(), format!("{}{path}", self.url));
        req = if let Some(token) = token {
            req.bearer_auth(token)
        } else {
            req.basic_auth("operator", Some(PASSWORD))
        };
        if !body.is_null() {
            req = req.json(&body);
        }
        let response = req.send().await.unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn admin(&self, method: &str, path: &str, body: Value) -> Value {
        let (status, result) = self.request(method, path, body, None).await;
        assert!(status.is_success(), "{method} {path}: {status} {result}");
        result
    }

    async fn runtime(&self, token: &str, body: Value) -> (StatusCode, Value) {
        self.request("POST", "/api/mcp-runtime", body, Some(token))
            .await
    }
}

async fn mock_mcp(
    State(calls): State<Arc<AtomicUsize>>,
    headers: HeaderMap,
    Json(req): Json<Value>,
) -> Response {
    assert_eq!(
        headers[header::AUTHORIZATION].to_str().unwrap(),
        format!("Bearer {SECRET}")
    );
    let result = match req["method"].as_str().unwrap() {
        "initialize" => {
            json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}})
        }
        "notifications/initialized" => return StatusCode::ACCEPTED.into_response(),
        "tools/list" => json!({"tools":[{"name":"fixture","inputSchema":{"type":"object"}}]}),
        "tools/call" => {
            calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(req["params"]["arguments"]["prompt"], "hello");
            json!({"content":[{"type":"text","text":format!("Tool response {SECRET}")}]})
        }
        _ => panic!("unexpected method {}", req["method"]),
    };
    Json(json!({"jsonrpc":"2.0","id":req["id"],"result":result})).into_response()
}

async fn mock_openai(Json(req): Json<Value>) -> Response {
    assert_eq!(req["model"], "fixture-model");
    let last = req["messages"].as_array().unwrap().last().unwrap();
    let has_mcp = req["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["function"]["name"] == "call_mcp_tool");
    let text = last["content"].as_str().unwrap();
    let (delta, finish) = if last["role"] == "tool" {
        assert!(text.contains("Tool response [redacted]"));
        assert!(!text.contains(SECRET));
        (json!({"content":"Native MCP completed."}), "stop")
    } else if let Some(id) = text.strip_prefix("Use MCP ") {
        assert!(has_mcp);
        let arguments = json!({"service":id,"tool":"fixture","arguments":{"prompt":"hello"}});
        (
            json!({"tool_calls":[{"index":0,"id":"fixture-call","type":"function","function":{"name":"call_mcp_tool","arguments":arguments.to_string()}}]}),
            "tool_calls",
        )
    } else {
        assert_eq!(text, "Check revoked access");
        assert!(!has_mcp);
        (json!({"content":"Native MCP unavailable."}), "stop")
    };
    let chunk = json!({"choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        format!("data: {chunk}\n\ndata: [DONE]\n\n"),
    )
        .into_response()
}

async fn file(path: &Path) -> String {
    for _ in 0..100 {
        if let Ok(value) = fs::read_to_string(path)
            && !value.is_empty()
        {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("fixture harness did not write {}", path.display())
}

async fn wait_for_reply(server: &Server, bot: &str, text: &str) {
    for _ in 0..100 {
        let messages = server
            .admin("GET", &format!("/api/bots/{bot}/messages"), Value::Null)
            .await;
        if messages
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["text"] == text)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("fixture harness did not finish")
}

#[tokio::test]
async fn manages_mcp_grants_and_pauses_running_and_queued_work() {
    let root =
        Directory(std::env::temp_dir().join(format!("superbot-mcp-test-{}", std::process::id())));
    fs::create_dir(&root.0).unwrap();
    let secrets = root.0.join("secrets");
    fs::create_dir(&secrets).unwrap();
    fs::set_permissions(&secrets, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(secrets.join("fixture.key"), SECRET).unwrap();
    fs::set_permissions(
        secrets.join("fixture.key"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let claude = root.0.join("claude");
    fs::write(&claude, r#"#!/bin/sh
test -z "$AUTH_PASSWORD" || exit 1
printf 'run\n' >> "$FIXTURE/$BOT_ID.runs"
printf '%s' "$BOT_MCP_TOKEN" > "$FIXTURE/$BOT_ID.token"
printf '%s' "$BOT_TOKEN" > "$FIXTURE/shared-token"
while [ ! -f "$FIXTURE/$BOT_ID.finish" ]; do sleep 0.05; done
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"Fixture finished."}]}}'
"#).unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o700)).unwrap();
    let stdio = root.0.join("stdio.sh");
    fs::write(&stdio, r#"test "$FIXTURE_SECRET" = fixture-credential-not-for-agents || exit 2
while IFS= read -r req; do
    id=$(printf '%s' "$req" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
    case "$req" in
        *'"method":"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"stdio fixture","version":"1"}}}\n' "$id";;
        *'"method":"tools/list"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"stdio_tool","inputSchema":{"type":"object"}}]}}\n' "$id";;
    esac
done
"#).unwrap();

    let calls = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let endpoint = format!("{base}/mcp");
    let inference = format!("{base}/v1");
    let mock = tokio::spawn(
        axum::serve(
            listener,
            Router::new()
                .route("/mcp", post(mock_mcp))
                .route("/v1/chat/completions", post(mock_openai))
                .with_state(calls.clone()),
        )
        .into_future(),
    );
    let server = Server::start(&root.0, &inference).await;
    let http_settings = json!({"name":"Remote fixture","transport":"http","enabled":true,"url":endpoint,"bearerFile":"fixture.key"});
    let remote = server
        .admin("POST", "/api/mcp/services", http_settings.clone())
        .await;
    let remote_id = remote["id"].as_str().unwrap();
    let service_path = format!("/api/mcp/services/{remote_id}");
    let tested = server
        .admin("POST", &format!("{service_path}/test"), json!({}))
        .await;
    assert_eq!(tested["status"], "Connected");
    let local = server.admin("POST", "/api/mcp/services", json!({
        "name":"Local fixture","transport":"stdio","enabled":true,"command":"/bin/sh","args":[stdio],"envFiles":{"FIXTURE_SECRET":"fixture.key"}
    })).await;
    let local_id = local["id"].as_str().unwrap();
    let broken = server.admin("POST", "/api/mcp/services", json!({
        "name":"Unavailable","transport":"stdio","enabled":false,"command":"/missing-fixture-executable"
    })).await;
    let broken_test = format!("/api/mcp/services/{}/test", broken["id"].as_str().unwrap());
    assert!(
        server.admin("POST", &broken_test, json!({})).await["status"]
            .as_str()
            .unwrap()
            .contains("Could not start")
    );
    assert!(
        !server
            .admin("GET", "/api/mcp/services", Value::Null)
            .await
            .to_string()
            .contains(SECRET)
    );

    let a = server
        .admin(
            "POST",
            "/api/bots",
            json!({"name":"Fixture A","harness":"claude"}),
        )
        .await;
    let b = server
        .admin(
            "POST",
            "/api/bots",
            json!({"name":"Fixture B","harness":"claude"}),
        )
        .await;
    let a = a["id"].as_str().unwrap();
    let b = b["id"].as_str().unwrap();
    for bot in [a, b] {
        server
            .admin(
                "POST",
                &format!("/api/bots/{bot}/messages"),
                json!({"text":"Active task"}),
            )
            .await;
    }
    let a_token = file(&root.0.join(format!("{a}.token"))).await;
    let b_token = file(&root.0.join(format!("{b}.token"))).await;
    let shared = file(&root.0.join("shared-token")).await;
    let list = json!({"operation":"services"});
    assert_eq!(server.runtime(&a_token, list.clone()).await.1, json!([]));
    assert_eq!(
        server.runtime(&shared, list.clone()).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        server
            .request("GET", "/api/mcp/services", Value::Null, Some(&shared))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let remote_grant = format!("/api/bots/{a}/mcp/{remote_id}");
    server
        .admin("PUT", &remote_grant, json!({"granted":true}))
        .await;
    server
        .admin(
            "PUT",
            &format!("/api/bots/{a}/mcp/{local_id}"),
            json!({"granted":true}),
        )
        .await;
    let call = json!({"operation":"call","service":remote_id,"tool":"fixture","arguments":{"prompt":"hello"},"botId":a});
    assert_eq!(
        server.runtime(&b_token, call.clone()).await.0,
        StatusCode::FORBIDDEN
    );
    let (status, result) = server.runtime(&a_token, call.clone()).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["content"][0]["text"], "Tool response [redacted]");
    let (status, tools) = server
        .runtime(&a_token, json!({"operation":"tools","service":local_id}))
        .await;
    assert_eq!(status, StatusCode::OK, "{tools}");
    assert_eq!(tools[0]["name"], "stdio_tool");
    server
        .admin("PUT", &remote_grant, json!({"granted":false}))
        .await;
    assert_eq!(
        server.runtime(&a_token, call.clone()).await.0,
        StatusCode::FORBIDDEN
    );
    server
        .admin("PUT", &remote_grant, json!({"granted":true}))
        .await;
    let mut disabled = http_settings.clone();
    disabled["enabled"] = json!(false);
    server.admin("PATCH", &service_path, disabled).await;
    assert_eq!(
        server.runtime(&a_token, call).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let messages = format!("/api/bots/{a}/messages");
    let routine_path = format!("/api/bots/{a}/routines");
    let pause = format!("/api/bots/{a}/pause");
    server
        .admin("POST", &messages, json!({"text":"Queued before pause"}))
        .await;
    let running = server.admin("PUT", &pause, json!({"paused":true})).await;
    assert_eq!(running["paused"], true);
    assert_eq!(running["working"], true);
    assert_eq!(
        server
            .request("POST", &messages, json!({"text":"Blocked"}), None)
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        server
            .request(
                "POST",
                "/api/ask",
                json!({"from":b,"to":a,"text":"Blocked delegation"}),
                None
            )
            .await
            .0,
        StatusCode::CONFLICT
    );
    server
        .admin(
            "POST",
            &routine_path,
            json!({"name":"Every minute","cron":"* * * * *","instruction":"Check the fixture"}),
        )
        .await;
    for bot in [a, b] {
        fs::write(root.0.join(format!("{bot}.finish")), "").unwrap();
        wait_for_reply(&server, bot, "Fixture finished.").await;
    }
    assert_eq!(
        fs::read_to_string(root.0.join(format!("{a}.runs"))).unwrap(),
        "run\n"
    );
    assert_eq!(
        server.runtime(&a_token, list).await.0,
        StatusCode::FORBIDDEN
    );
    drop(server);

    let server = Server::start(&root.0, &inference).await;
    let state = server.admin("GET", "/api/state", Value::Null).await;
    assert_eq!(
        state["bots"]
            .as_array()
            .unwrap()
            .iter()
            .find(|bot| bot["id"] == a)
            .unwrap()["paused"],
        true
    );
    let grants = server
        .admin("GET", &format!("/api/bots/{a}/mcp"), Value::Null)
        .await;
    assert!(
        grants
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["id"] == local_id && g["granted"] == true)
    );
    let routines = server.admin("GET", &routine_path, Value::Null).await;
    assert!(routines[0]["last_run_at"].is_null());
    assert_eq!(routines[0]["enabled"], true);
    let seen = server.admin("GET", &messages, Value::Null).await;
    assert!(
        !seen
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "routine")
    );
    assert!(
        seen.as_array()
            .unwrap()
            .iter()
            .any(|m| m["text"] == "Queued before pause")
    );
    server.admin("PUT", &pause, json!({"paused":false})).await;
    server
        .admin("POST", &messages, json!({"text":"New work after resume"}))
        .await;
    for _ in 0..100 {
        if fs::read_to_string(root.0.join(format!("{a}.runs")))
            .unwrap()
            .lines()
            .count()
            == 2
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        fs::read_to_string(root.0.join(format!("{a}.runs"))).unwrap(),
        "run\nrun\n"
    );

    server.admin("PATCH", &service_path, http_settings).await;
    let native = server
        .admin(
            "POST",
            "/api/bots",
            json!({"name":"Native fixture","harness":"custom"}),
        )
        .await;
    let native = native["id"].as_str().unwrap();
    let grant = format!("/api/bots/{native}/mcp/{remote_id}");
    let native_messages = format!("/api/bots/{native}/messages");
    server.admin("PUT", &grant, json!({"granted":true})).await;
    server
        .admin(
            "POST",
            &native_messages,
            json!({"text":format!("Use MCP {remote_id}")}),
        )
        .await;
    wait_for_reply(&server, native, "Native MCP completed.").await;
    server.admin("PUT", &grant, json!({"granted":false})).await;
    server
        .admin(
            "POST",
            &native_messages,
            json!({"text":"Check revoked access"}),
        )
        .await;
    wait_for_reply(&server, native, "Native MCP unavailable.").await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    server.admin("DELETE", &service_path, Value::Null).await;
    assert!(
        !server
            .admin("GET", "/api/mcp/services", Value::Null)
            .await
            .to_string()
            .contains(remote_id)
    );
    drop(server);
    let server = Server::start(&root.0, &inference).await;
    let routines = server.admin("GET", &routine_path, Value::Null).await;
    assert!(routines[0]["last_run_at"].is_number());
    mock.abort();
}

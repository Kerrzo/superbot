//! Managed MCP tools. The server owns the connection and verifies access on
//! every operation. Harnesses receive only a short-lived run token.
use crate::{ApiError, AppState, Res, bad, not_found, operator_password};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, patch, put};
use axum::{Json, Router};
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService, ServiceExt};
use rmcp::transport::{
    StreamableHttpClientTransport, TokioChildProcess,
    streamable_http_client::StreamableHttpClientTransportConfig,
};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RESULT_BYTES: usize = 256 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(
    tag = "transport",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
enum Connection {
    Http {
        url: String,
        bearer_file: Option<String>,
    },
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env_files: BTreeMap<String, String>,
    },
}

#[derive(Deserialize, Serialize)]
struct Settings {
    name: String,
    enabled: bool,
    #[serde(flatten)]
    connection: Connection,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Service {
    id: String,
    #[serde(flatten)]
    settings: Settings,
    #[serde(skip)]
    revision: String,
    status: Option<String>,
    tested_at: Option<i64>,
}

fn private_filename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
}

impl Settings {
    fn validate(mut self) -> Result<Self, ApiError> {
        self.name = self.name.trim().to_owned();
        if self.name.is_empty() || self.name.chars().count() > 60 {
            return Err(bad("Enter a service name of 1 to 60 characters."));
        }
        match &mut self.connection {
            Connection::Http { url, bearer_file } => {
                let parsed = reqwest::Url::parse(url.trim())
                    .map_err(|_| bad("Enter a valid MCP endpoint URL."))?;
                if !matches!(parsed.scheme(), "http" | "https")
                    || parsed.host().is_none()
                    || !parsed.username().is_empty()
                    || parsed.password().is_some()
                    || parsed.query().is_some()
                    || parsed.fragment().is_some()
                {
                    return Err(bad(
                        "Use an HTTP(S) MCP URL without credentials, query parameters, or fragments.",
                    ));
                }
                if let Some(file) = bearer_file {
                    if !private_filename(file) {
                        return Err(bad(
                            "Enter a credential filename from the server's MCP secrets directory.",
                        ));
                    }
                    if parsed.scheme() != "https"
                        && !matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
                    {
                        return Err(bad(
                            "Use HTTPS when sending a bearer token to a remote service.",
                        ));
                    }
                }
                *url = parsed.to_string();
            }
            Connection::Stdio {
                command,
                args,
                env_files,
            } => {
                *command = command.trim().to_owned();
                if command.is_empty()
                    || command.contains('\0')
                    || (command.contains('/') && !std::path::Path::new(command).is_absolute())
                    || args.len() > 64
                    || args
                        .iter()
                        .any(|arg| arg.len() > 4096 || arg.contains('\0'))
                {
                    return Err(bad(
                        "Enter a binary name or absolute path, and up to 64 arguments.",
                    ));
                }
                if env_files.len() > 32
                    || env_files.iter().any(|(key, file)| {
                        key.is_empty()
                            || !key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                            || !private_filename(file)
                    })
                {
                    return Err(bad(
                        "Use environment variable names and credential filenames.",
                    ));
                }
            }
        }
        Ok(self)
    }
}

impl crate::db::Db {
    fn mcp_services(&self, bot: Option<&str>) -> rusqlite::Result<Vec<Service>> {
        let c = self.c();
        let mut s = c.prepare("SELECT id, name, enabled, connection, revision, status, tested_at FROM mcp_services AS m
            WHERE ?1 IS NULL OR (m.enabled AND EXISTS (SELECT 1 FROM bot_mcp WHERE bot_id = ?1 AND service_id = m.id))
            ORDER BY name COLLATE NOCASE, id")?;
        s.query_map([bot], service_row)?.collect()
    }

    fn mcp_service(&self, id: &str, bot: Option<&str>) -> rusqlite::Result<Option<Service>> {
        self.c().query_row("SELECT id, name, enabled, connection, revision, status, tested_at FROM mcp_services AS m
            WHERE id = ?1 AND (?2 IS NULL OR (m.enabled AND EXISTS
            (SELECT 1 FROM bot_mcp WHERE bot_id = ?2 AND service_id = m.id)))",
            params![id, bot], service_row).optional()
    }

    fn mcp_write(&self, id: &str, settings: &Settings, edit: bool) -> rusqlite::Result<bool> {
        let revision = crate::adapters::new_uuid();
        let connection = serde_json::to_string(&settings.connection)
            .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))?;
        let sql = if edit {
            "UPDATE mcp_services SET name=?2, enabled=?3, connection=?4, revision=?5, status=NULL, tested_at=NULL WHERE id=?1"
        } else {
            "INSERT INTO mcp_services (id,name,enabled,connection,revision) VALUES (?1,?2,?3,?4,?5)"
        };
        self.c()
            .execute(
                sql,
                params![id, settings.name, settings.enabled, connection, revision],
            )
            .map(|n| n != 0)
    }

    pub fn has_mcp(&self, bot: &str) -> bool {
        self.c()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM bot_mcp AS g
            JOIN mcp_services AS m ON m.id=g.service_id WHERE g.bot_id=? AND m.enabled)",
                [bot],
                |r| r.get(0),
            )
            .unwrap_or(false)
    }
}

fn service_row(r: &rusqlite::Row) -> rusqlite::Result<Service> {
    let value: String = r.get(3)?;
    let connection = serde_json::from_str(&value).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(err))
    })?;
    Ok(Service {
        id: r.get(0)?,
        settings: Settings {
            name: r.get(1)?,
            enabled: r.get(2)?,
            connection,
        },
        revision: r.get(4)?,
        status: r.get(5)?,
        tested_at: r.get(6)?,
    })
}

fn storage(_: rusqlite::Error) -> ApiError {
    ApiError(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Could not access MCP settings.".into(),
    )
}

fn denied() -> ApiError {
    ApiError(
        StatusCode::FORBIDDEN,
        "This run has no access to that MCP service.".into(),
    )
}

pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .route("/mcp/services", get(catalog).post(create))
        .route("/mcp/services/{id}", patch(update).delete(remove))
        .route(
            "/mcp/services/{id}/test",
            axum::routing::post(test_connection),
        )
        .route("/bots/{bot}/mcp", get(grants))
        .route("/bots/{bot}/mcp/{id}", put(grant))
        .route_layer(middleware::from_fn(admin))
}

async fn admin(request: Request, next: Next) -> Result<Response, ApiError> {
    if !operator_password(request.headers()) {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "Set AUTH_PASSWORD and sign in as the operator to manage MCP services.".into(),
        ));
    }
    Ok(next.run(request).await)
}

async fn catalog(State(app): State<AppState>) -> Res {
    Ok(Json(json!(app.db.mcp_services(None).map_err(storage)?)))
}

async fn create(
    State(app): State<AppState>,
    Json(settings): Json<Settings>,
) -> Res<(StatusCode, Json<Value>)> {
    let id = crate::short_id();
    app.db
        .mcp_write(&id, &settings.validate()?, false)
        .map_err(storage)?;
    Ok((
        StatusCode::CREATED,
        Json(json!(app.db.mcp_service(&id, None).map_err(storage)?)),
    ))
}

async fn update(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(settings): Json<Settings>,
) -> Res {
    if !app
        .db
        .mcp_write(&id, &settings.validate()?, true)
        .map_err(storage)?
    {
        return Err(not_found("MCP service"));
    }
    Ok(Json(json!(app.db.mcp_service(&id, None).map_err(storage)?)))
}

async fn remove(State(app): State<AppState>, Path(id): Path<String>) -> Res {
    if app
        .db
        .c()
        .execute("DELETE FROM mcp_services WHERE id=?", [id])
        .map_err(storage)?
        == 0
    {
        return Err(not_found("MCP service"));
    }
    Ok(Json(json!({"ok": true})))
}

async fn grants(State(app): State<AppState>, Path(bot): Path<String>) -> Res {
    crate::need_bot(&app, &bot)?;
    let c = app.db.c();
    let mut s = c
        .prepare(
            "SELECT m.id, m.name, m.enabled, g.bot_id IS NOT NULL
        FROM mcp_services m LEFT JOIN bot_mcp g ON m.id=g.service_id AND g.bot_id=?
        ORDER BY m.name COLLATE NOCASE, m.id",
        )
        .map_err(storage)?;
    let services: Vec<Value> = s
        .query_map([bot], |r| {
            Ok(json!({
                "id": r.get::<_, String>(0)?, "name": r.get::<_, String>(1)?,
                "enabled": r.get::<_, bool>(2)?, "granted": r.get::<_, bool>(3)?,
            }))
        })
        .map_err(storage)?
        .collect::<rusqlite::Result<_>>()
        .map_err(storage)?;
    Ok(Json(json!(services)))
}

async fn grant(
    State(app): State<AppState>,
    Path((bot, id)): Path<(String, String)>,
    Json(v): Json<Value>,
) -> Res {
    let value = v["granted"]
        .as_bool()
        .ok_or_else(|| bad("granted must be true or false"))?;
    crate::need_bot(&app, &bot)?;
    let c = app.db.c();
    let exists: bool = c
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM mcp_services WHERE id=?)",
            [&id],
            |r| r.get(0),
        )
        .map_err(storage)?;
    if !exists {
        return Err(not_found("MCP service"));
    }
    let sql = if value {
        "INSERT OR IGNORE INTO bot_mcp (bot_id, service_id) VALUES (?, ?)"
    } else {
        "DELETE FROM bot_mcp WHERE bot_id=? AND service_id=?"
    };
    c.execute(sql, params![bot, id]).map_err(storage)?;
    Ok(Json(json!({"granted": value})))
}

fn credential(root: &std::path::Path, name: &str) -> Result<String, &'static str> {
    const PRIVATE: &str =
        "Credential files need a private directory (700) and owner-only file permissions (600).";
    if !private_filename(name) {
        return Err(PRIVATE);
    }
    let dir = std::fs::metadata(root).map_err(|_| "The MCP secrets directory is unavailable.")?;
    if !dir.is_dir() || dir.permissions().mode() & 0o077 != 0 {
        return Err(PRIVATE);
    }
    let path = root.join(name);
    let metadata =
        std::fs::symlink_metadata(&path).map_err(|_| "An MCP credential file is unavailable.")?;
    if !metadata.is_file() {
        return Err(PRIVATE);
    }
    let file = std::fs::File::open(path).map_err(|_| "Could not open an MCP credential file.")?;
    let metadata = file.metadata().map_err(|_| PRIVATE)?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 || metadata.len() > 16384 {
        return Err(PRIVATE);
    }
    let mut value = String::new();
    file.take(16385)
        .read_to_string(&mut value)
        .map_err(|_| "Could not read an MCP credential file.")?;
    let value = value.trim_end_matches(['\r', '\n']);
    if value.is_empty() || value.len() > 16384 || value.contains('\0') {
        return Err("An MCP credential file is invalid.");
    }
    Ok(value.to_owned())
}

type Client = RunningService<RoleClient, ()>;

async fn connect(app: &AppState, service: &Service) -> Result<(Client, Vec<String>), &'static str> {
    const INIT: &str =
        "MCP initialization failed. Check the endpoint or command and its credentials.";
    let root = &app.cfg.mcp_secrets;
    match &service.settings.connection {
        Connection::Http { url, bearer_file } => {
            let mut config = StreamableHttpClientTransportConfig::with_uri(url.clone())
                .reinit_on_expired_session(false);
            config.max_error_response_size = 0;
            config.max_json_response_size = MAX_RESULT_BYTES;
            config.max_sse_event_size = MAX_RESULT_BYTES;
            let mut secrets = Vec::new();
            if let Some(file) = bearer_file {
                let secret = credential(root, file)?;
                let mut authorization =
                    reqwest::header::HeaderValue::from_str(&format!("Bearer {secret}"))
                        .map_err(|_| "The MCP bearer token is invalid.")?;
                authorization.set_sensitive(true);
                config
                    .custom_headers
                    .insert(reqwest::header::AUTHORIZATION, authorization);
                secrets.push(secret);
            }
            let http = app.mcp_http.clone();
            let transport = StreamableHttpClientTransport::with_client(http, config);
            Ok((().serve(transport).await.map_err(|_| INIT)?, secrets))
        }
        Connection::Stdio {
            command,
            args,
            env_files,
        } => {
            let mut child = tokio::process::Command::new(command);
            child
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .args(args)
                .current_dir(&app.cfg.workspace)
                .kill_on_drop(true);
            let mut secrets = Vec::new();
            for (key, file) in env_files {
                let secret = credential(root, file)?;
                child.env(key, &secret);
                secrets.push(secret);
            }
            let (transport, _) = TokioChildProcess::builder(child)
                .stderr(Stdio::null())
                .spawn()
                .map_err(
                    |_| "Could not start the MCP command. Check the executable and arguments.",
                )?;
            Ok((().serve(transport).await.map_err(|_| INIT)?, secrets))
        }
    }
}

async fn test_connection(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(_): Json<Value>,
) -> Res {
    let service = app
        .db
        .mcp_service(&id, None)
        .map_err(storage)?
        .ok_or_else(|| not_found("MCP service"))?;
    let status = match tokio::time::timeout(TIMEOUT, connect(&app, &service)).await {
        Ok(Ok(_)) => "Connected",
        Ok(Err(message)) => message,
        Err(_) => "MCP connection timed out.",
    };
    let tested_at = crate::db::now();
    let changed = app
        .db
        .c()
        .execute(
            "UPDATE mcp_services SET status=?, tested_at=? WHERE id=? AND revision=?",
            params![status, tested_at, id, service.revision],
        )
        .map_err(storage)?;
    if changed == 0 {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "The MCP service changed during the connection test. Test again.".into(),
        ));
    }
    Ok(Json(json!({"status": status, "testedAt": tested_at})))
}

#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum RequestOperation {
    Services,
    Tools {
        service: String,
    },
    Call {
        service: String,
        tool: String,
        arguments: Map<String, Value>,
    },
}

pub async fn runtime(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<RequestOperation>,
) -> Res {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .ok_or_else(denied)?;
    let bot = app.mcp_bot(token).ok_or_else(denied)?;
    let service = match &req {
        RequestOperation::Services => {
            let services: Vec<_> = app
                .db
                .mcp_services(Some(&bot))
                .map_err(storage)?
                .into_iter()
                .map(|s| json!({"id": s.id, "name": s.settings.name}))
                .collect();
            return Ok(Json(json!(services)));
        }
        RequestOperation::Tools { service } | RequestOperation::Call { service, .. } => service,
    };
    let connection = app
        .db
        .mcp_service(service, Some(&bot))
        .map_err(storage)?
        .ok_or_else(denied)?;
    let operation = async {
        let (client, secrets) = connect(&app, &connection)
            .await
            .map_err(|msg| ApiError(StatusCode::BAD_GATEWAY, msg.into()))?;
        // The handshake is async. Check the grant and catalogue revision again
        // before sending a request that can run a tool.
        let current = app
            .db
            .mcp_service(&connection.id, Some(&bot))
            .map_err(storage)?;
        if app.mcp_bot(token).as_deref() != Some(&bot)
            || !current.is_some_and(|s| s.revision == connection.revision)
        {
            return Err(denied());
        }
        let failure = |_| {
            ApiError(
                StatusCode::BAD_GATEWAY,
                "The MCP service could not complete the operation.".into(),
            )
        };
        let mut value = match req {
            RequestOperation::Tools { .. } => {
                json!(client.list_all_tools().await.map_err(failure)?)
            }
            RequestOperation::Call {
                tool, arguments, ..
            } => {
                if tool.is_empty() || tool.len() > 256 {
                    return Err(bad("Provide a tool name."));
                }
                json!(
                    client
                        .call_tool(CallToolRequestParams::new(tool).with_arguments(arguments))
                        .await
                        .map_err(failure)?
                )
            }
            RequestOperation::Services => unreachable!(),
        };
        redact(&mut value, &secrets);
        if value.to_string().len() > MAX_RESULT_BYTES {
            return Err(ApiError(
                StatusCode::BAD_GATEWAY,
                "MCP result exceeds the size limit.".into(),
            ));
        }
        Ok(Json(value))
    };
    tokio::time::timeout(TIMEOUT, operation)
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::GATEWAY_TIMEOUT,
                "The MCP operation timed out.".into(),
            )
        })?
}

fn redact(value: &mut Value, secrets: &[String]) {
    let clean = |text: &mut String| {
        for secret in secrets {
            if text.contains(secret.as_str()) {
                *text = text.replace(secret.as_str(), "[redacted]");
            }
        }
    };
    match value {
        Value::String(text) => clean(text),
        Value::Array(items) => items.iter_mut().for_each(|item| redact(item, secrets)),
        Value::Object(fields) if !secrets.is_empty() => {
            for (mut key, mut item) in std::mem::take(fields) {
                clean(&mut key);
                redact(&mut item, secrets);
                fields.insert(key, item);
            }
        }
        _ => {}
    }
}

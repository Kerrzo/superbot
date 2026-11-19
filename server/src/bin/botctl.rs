//! `botctl`: the CLI Bots use for routines, peer messages, MCP and screen handoffs. Works with any CLI harness,
//! because every harness can run a shell command. Talks to botd over local HTTP.
//! Uses a local HTTP/1.0 connection over a TCP socket.

use std::io::{Read, Write};
use std::net::TcpStream;

const HELP: &str = r#"botctl: manage your routines (scheduled tasks).

  botctl routines                                  List your routines
  botctl routine add "<name>" "<cron>" "<what to do>"
  botctl routine rm <id>                           Delete a routine
  botctl routine pause <id> | resume <id>
  botctl bots                                      List all Bots
  botctl ask "<Bot name or id>" "<message>"        Ask another Bot; prints its reply
  botctl handoff "<what you need>"                 Ask the user to take over your screen; waits for "Done"
  botctl mcp services                              List assigned MCP services
  botctl mcp tools <service-id>                    List a service's tools and schemas
  botctl mcp call <service-id> <tool> '<JSON>'      Call a tool using a JSON object

Cron has 5 fields in the server's local time: minute hour day month weekday.
  "30 7 * * *"    every day at 07:30
  "0 9 * * 1-5"   weekdays at 09:00
  "*/15 * * * *"  every 15 minutes
When a routine fires, you get "<what to do>" as a message and your reply goes to the user's chat.
Confirm the schedule with the user in plain words.

`ask` and `handoff` can take minutes: run them with a long command timeout (10 minutes).
If the other Bot takes longer, its reply arrives later as a new message."#;

fn api(method: &str, path: &str, body: Option<String>) -> Result<String, String> {
    let token_var = if path == "/api/mcp-runtime" { "BOT_MCP_TOKEN" } else { "BOT_TOKEN" };
    let (Ok(base), Ok(token)) = (std::env::var("BOT_API"), std::env::var(token_var)) else {
        return Err("botctl only works inside a Bot run.".into());
    };
    let addr = base.trim_start_matches("http://").trim_end_matches('/');
    let mut s = TcpStream::connect(addr).map_err(|e| format!("Can't reach the server: {e}"))?;
    let body = body.unwrap_or_default();
    let req = format!(
        "{method} {path} HTTP/1.0\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut res = String::new();
    s.read_to_string(&mut res).map_err(|e| e.to_string())?;
    let (head, body) = res.split_once("\r\n\r\n").unwrap_or((&res, ""));
    let code: u16 = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    if (200..300).contains(&code) {
        Ok(body.to_string())
    } else {
        // Pull "error" out of {"error":"..."} without a JSON library.
        let msg = body.split("\"error\":\"").nth(1).and_then(|r| r.split('"').next()).unwrap_or(body);
        Err(format!("{msg} (HTTP {code})"))
    }
}

fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out + "\""
}

fn run(args: &[String]) -> Result<String, String> {
    let bot = std::env::var("BOT_ID").unwrap_or_default();
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        ["routines"] | ["routine", "list"] => api("GET", &format!("/api/bots/{bot}/routines"), None),
        ["routine", "add", name, cron, what] => {
            let body = format!("{{\"name\":{},\"cron\":{},\"instruction\":{}}}", json_str(name), json_str(cron), json_str(what));
            api("POST", &format!("/api/bots/{bot}/routines"), Some(body)).map(|_| format!("Added routine \"{name}\" ({cron})."))
        }
        ["routine", "rm" | "delete", id] => api("DELETE", &format!("/api/routines/{id}"), None).map(|_| "Deleted.".into()),
        ["routine", cmd @ ("pause" | "resume"), id] => {
            let body = format!("{{\"enabled\":{}}}", *cmd == "resume");
            api("PATCH", &format!("/api/routines/{id}"), Some(body)).map(|_| if *cmd == "pause" { "Paused." } else { "Resumed." }.into())
        }
        ["bots"] => api("GET", "/api/bots", None),
        ["ask", to, message] => {
            let body = format!("{{\"from\":{},\"to\":{},\"text\":{}}}", json_str(&bot), json_str(to), json_str(message));
            api("POST", "/api/ask", Some(body))
        }
        ["handoff", text] => {
            let body = format!("{{\"from\":{},\"text\":{}}}", json_str(&bot), json_str(text));
            api("POST", "/api/handoff", Some(body))
        }
        ["mcp", "services"] => mcp(serde_json::json!({"operation": "services"})),
        ["mcp", "tools", service] => mcp(serde_json::json!({"operation": "tools", "service": service})),
        ["mcp", "call", service, tool, arguments] => {
            let Ok(args @ serde_json::Value::Object(_)) = serde_json::from_str(arguments) else {
                return Err("MCP arguments must be a JSON object.".into());
            };
            mcp(serde_json::json!({"operation": "call", "service": service, "tool": tool, "arguments": args}))
        }
        _ => Ok(HELP.into()),
    }
}

fn mcp(body: serde_json::Value) -> Result<String, String> {
    api("POST", "/api/mcp-runtime", Some(body.to_string()))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(out) => println!("{out}"),
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

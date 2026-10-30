//! A minimal MCP server (Streamable HTTP, JSON responses only) inside the
//! app, so claude's tools can reach the background shells and the UI
//! directly. The server is named `va`, so the tools are `mcp__va__*`.

use std::sync::Arc;

use serde_json::{Map, Value, json};

pub struct ToolResult {
    pub text: String,
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(text: impl Into<String>) -> ToolResult {
        ToolResult { text: text.into(), is_error: false }
    }

    pub fn error(text: impl Into<String>) -> ToolResult {
        ToolResult { text: text.into(), is_error: true }
    }
}

pub type Handler = dyn Fn(&Map<String, Value>) -> ToolResult + Send + Sync;

pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
    pub handler: Box<Handler>,
}

pub struct McpServer {
    name: String,
    token: String,
    pub port: u16,
    server: Arc<tiny_http::Server>,
}

fn random_token() -> String {
    crate::util::random_bytes::<24>().iter().map(|x| format!("{x:02x}")).collect()
}

impl McpServer {
    /// Serves the tools on a local port of its own.
    pub fn start(name: &str, tools: Vec<Tool>) -> Result<McpServer, String> {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").map_err(|e| e.to_string())?);
        let port = server.server_addr().to_ip().map(|a| a.port()).ok_or("no port")?;
        let token = random_token();
        let (srv, name_owned, want, tools) = (server.clone(), name.to_string(), format!("Bearer {token}"), Arc::new(tools));
        std::thread::spawn(move || {
            for req in srv.incoming_requests() {
                // Tool calls (long commands, waiting for the user's approval) can
                // take minutes: each request gets a thread
                let (name, want, tools) = (name_owned.clone(), want.clone(), tools.clone());
                std::thread::spawn(move || handle(req, &name, &want, &tools));
            }
        });
        Ok(McpServer { name: name.to_string(), token, port, server })
    }

    pub fn stop(&self) {
        self.server.unblock();
    }

    /// The value for claude --mcp-config.
    pub fn config(&self) -> String {
        json!({"mcpServers": {&self.name: {
            "type": "http",
            "url": format!("http://127.0.0.1:{}/mcp", self.port),
            "headers": {"Authorization": format!("Bearer {}", self.token)},
        }}})
        .to_string()
    }
}

fn respond_json(req: tiny_http::Request, v: &Value) {
    let header = tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap();
    let _ = req.respond(tiny_http::Response::from_string(v.to_string()).with_header(header));
}

fn handle(mut req: tiny_http::Request, name: &str, want: &str, tools: &[Tool]) {
    let auth = req.headers().iter().find(|h| h.field.equiv("Authorization")).map(|h| h.value.to_string()).unwrap_or_default();
    if req.url().split('?').next() != Some("/mcp") {
        let _ = req.respond(tiny_http::Response::empty(404));
        return;
    }
    if !crate::util::constant_time_eq(auth.as_bytes(), want.as_bytes()) {
        let _ = req.respond(tiny_http::Response::empty(401));
        return;
    }
    match req.method() {
        tiny_http::Method::Delete => {
            let _ = req.respond(tiny_http::Response::empty(200));
            return;
        }
        tiny_http::Method::Post => {}
        _ => {
            // No server-initiated stream
            let allow = tiny_http::Header::from_bytes("Allow", "POST, DELETE").unwrap();
            let _ = req.respond(tiny_http::Response::empty(405).with_header(allow));
            return;
        }
    }
    let mut body = Vec::new();
    if req.as_reader().read_to_end(&mut body).is_err() {
        let _ = req.respond(tiny_http::Response::empty(400));
        return;
    }
    let parsed: Result<Value, _> = serde_json::from_slice(&body);
    let (batch, is_batch) = match parsed {
        Ok(Value::Array(items)) => (items, true),
        Ok(v @ Value::Object(_)) => (vec![v], false),
        _ => {
            respond_json(req, &json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "Parse error"}}));
            return;
        }
    };
    let replies: Vec<Value> = std::thread::scope(|s| {
        let handles: Vec<_> = batch.iter().map(|m| s.spawn(move || dispatch(m, name, tools))).collect();
        handles.into_iter().filter_map(|h| h.join().ok().flatten()).collect()
    });
    if replies.is_empty() {
        let _ = req.respond(tiny_http::Response::empty(202));
    } else if is_batch {
        respond_json(req, &Value::Array(replies));
    } else {
        respond_json(req, &replies[0]);
    }
}

fn dispatch(m: &Value, name: &str, tools: &[Tool]) -> Option<Value> {
    // Notifications get no reply
    let id = m.get("id").filter(|id| !id.is_null())?.clone();
    let reply = |result: Value| Some(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    let fail = |code: i64, msg: String| Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}}));
    let method = m.get("method").and_then(Value::as_str).unwrap_or_default();
    let params = m.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "initialize" => {
            let version = params.get("protocolVersion").and_then(Value::as_str).filter(|v| !v.is_empty()).unwrap_or("2025-06-18");
            reply(json!({
                "protocolVersion": version,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": name, "version": env!("CARGO_PKG_VERSION")},
            }))
        }
        "ping" => reply(json!({})),
        "tools/list" => {
            let list: Vec<Value> = tools.iter().map(|t| json!({"name": t.name, "description": t.description, "inputSchema": t.input_schema})).collect();
            reply(json!({"tools": list}))
        }
        "tools/call" => {
            let Some(tool_name) = params.get("name").and_then(Value::as_str) else { return fail(-32602, "Invalid params".into()) };
            let Some(tool) = tools.iter().find(|t| t.name == tool_name) else { return fail(-32602, format!("Unknown tool: {tool_name}")) };
            let empty = Map::new();
            let args = params.get("arguments").and_then(Value::as_object).unwrap_or(&empty);
            let res = (tool.handler)(args);
            reply(json!({"content": [{"type": "text", "text": res.text}], "isError": res.is_error}))
        }
        _ => fail(-32601, format!("Method not found: {method}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net;
    use std::io::Read;
    use std::time::Duration;

    fn echo_tool() -> Tool {
        Tool {
            name: "echo",
            description: "Says it back.",
            input_schema: json!({"type": "object", "properties": {"text": {"type": "string"}}}),
            handler: Box::new(|args| match args.get("text").and_then(Value::as_str) {
                Some(t) => ToolResult::ok(format!("echo: {t}")),
                None => ToolResult::error("Error: no text"),
            }),
        }
    }

    /// POSTs a JSON-RPC message the way claude does (written by hand to add
    /// the header).
    fn rpc(port: u16, auth: &str, body: &str) -> (u16, String) {
        use std::io::Write;
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "POST /mcp HTTP/1.1\r\nHost: localhost\r\nAuthorization: {auth}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        let status = resp.split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, resp.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default())
    }

    #[test]
    fn serves_tools_over_json_rpc() {
        let srv = McpServer::start("va", vec![echo_tool()]).unwrap();
        let cfg: Value = serde_json::from_str(&srv.config()).unwrap();
        let auth = cfg["mcpServers"]["va"]["headers"]["Authorization"].as_str().unwrap().to_string();
        assert_eq!(cfg["mcpServers"]["va"]["url"], format!("http://127.0.0.1:{}/mcp", srv.port));

        let (status, _) = rpc(srv.port, "Bearer wrong", r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
        assert_eq!(status, 401);
        assert_eq!(net::post(&format!("http://127.0.0.1:{}/mcp", srv.port), None, b"{}", Duration::from_secs(5)).unwrap().0, 401);

        let (status, body) = rpc(srv.port, &auth, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!((status, v["result"]["protocolVersion"].as_str(), v["result"]["serverInfo"]["name"].as_str()), (200, Some("2025-03-26"), Some("va")));

        let (_, body) = rpc(srv.port, &auth, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["result"]["tools"][0]["name"], "echo");

        let (_, body) = rpc(srv.port, &auth, r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"echo","arguments":{"text":"hi"}}}"#);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!((v["result"]["content"][0]["text"].as_str(), v["result"]["isError"].as_bool()), (Some("echo: hi"), Some(false)));

        let (_, body) = rpc(srv.port, &auth, r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"nope"}}"#);
        assert!(body.contains("Unknown tool: nope"));

        let (status, body) = rpc(srv.port, &auth, r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        assert_eq!((status, body.as_str()), (202, ""));
        let (_, body) = rpc(srv.port, &auth, r#"[{"jsonrpc":"2.0","id":5,"method":"ping"},{"jsonrpc":"2.0","id":6,"method":"nope"}]"#);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v.as_array().map(Vec::len), Some(2));
        let (_, body) = rpc(srv.port, &auth, "not json");
        assert!(body.contains("-32700"));
        srv.stop();
    }
}

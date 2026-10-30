//! End-to-end JSON-RPC test: drive the server over an in-memory duplex
//! stream with raw MCP messages, exactly as a client would on stdio.

use std::fs;

use code_scope_mcp::mcp::{CodeScope, Workspace};
use rmcp::ServiceExt;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct Client {
    writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    reader: BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
}

impl Client {
    async fn send(&mut self, msg: Value) {
        let mut line = serde_json::to_vec(&msg).unwrap();
        line.push(b'\n');
        self.writer.write_all(&line).await.unwrap();
    }

    /// Read until the response for `id` arrives, skipping notifications.
    async fn response(&mut self, id: i64) -> Value {
        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).await.unwrap();
            assert!(n > 0, "server closed the stream");
            let msg: Value = serde_json::from_str(&line).expect("server wrote invalid JSON");
            assert_eq!(msg["jsonrpc"], "2.0");
            if msg["id"] == id {
                return msg;
            }
        }
    }

    async fn call(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        self.response(id).await
    }
}

fn tool_text(resp: &Value) -> &str {
    resp["result"]["content"][0]["text"].as_str().unwrap()
}

#[tokio::test]
async fn mcp_session_over_jsonrpc() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("app.py"),
        "class App:\n    def run(self):\n        return 1\n",
    )
    .unwrap();

    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = CodeScope::new(Workspace::new(dir.path()).unwrap());
    let handle = tokio::spawn(async move {
        server
            .serve(server_io)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap();
    });

    let (r, w) = tokio::io::split(client_io);
    let mut client = Client {
        writer: w,
        reader: BufReader::new(r),
    };

    let init = client
        .call(
            1,
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }),
        )
        .await;
    assert_eq!(init["result"]["serverInfo"]["name"], "code-scope-mcp");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    client
        .send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await;

    let list = client.call(2, "tools/list", json!({})).await;
    let mut names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "get_file_outline",
            "git_status",
            "list_files",
            "read_file_pruned",
            "read_symbol",
            "search_symbols"
        ]
    );
    let search_tool = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "search_symbols")
        .unwrap();
    assert_eq!(search_tool["inputSchema"]["required"], json!(["query"]));

    let outline = client
        .call(
            3,
            "tools/call",
            json!({"name": "get_file_outline", "arguments": {"path": "app.py"}}),
        )
        .await;
    assert_ne!(outline["result"]["isError"], true);
    assert!(tool_text(&outline).contains("  def run(self) [method] L2-3"));

    let symbol = client
        .call(
            4,
            "tools/call",
            json!({"name": "read_symbol", "arguments": {"path": "app.py", "name": "App::run"}}),
        )
        .await;
    assert!(tool_text(&symbol).contains("3\t        return 1"));

    // Tool failures surface as isError results, not protocol errors.
    let missing = client
        .call(
            5,
            "tools/call",
            json!({"name": "get_file_outline", "arguments": {"path": "nope.rs"}}),
        )
        .await;
    assert_eq!(missing["result"]["isError"], true);
    assert!(tool_text(&missing).contains("no such path"));

    // Bad arguments come back as a tool error the model can correct.
    let invalid = client
        .call(
            6,
            "tools/call",
            json!({"name": "search_symbols", "arguments": {}}),
        )
        .await;
    assert_eq!(invalid["result"]["isError"], true);
    assert!(tool_text(&invalid).contains("missing field `query`"));

    drop(client);
    handle.await.unwrap();
}

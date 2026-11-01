use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tempfile::TempDir;

const ACCOUNT_ID: &str = "11111111-1111-1111-1111-111111111111";

struct McpProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    messages: Receiver<Result<Value, String>>,
}

impl McpProcess {
    fn start(directory: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_exact-mcp"))
            .current_dir(directory)
            .args(["--data-dir"])
            .arg(directory)
            .arg("serve")
            .env("EXACT_CLIENT_SECRET", "fixture-secret")
            .env_remove("HTTP_PROXY")
            .env_remove("HTTPS_PROXY")
            .env_remove("ALL_PROXY")
            .env_remove("http_proxy")
            .env_remove("https_proxy")
            .env_remove("all_proxy")
            .env("NO_PROXY", "*")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, messages) = channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let value = line.map_err(|error| error.to_string()).and_then(|line| {
                    serde_json::from_str(&line)
                        .map_err(|error| format!("Non-protocol stdout: {line}: {error}"))
                });
                if sender.send(value).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin: Some(stdin),
            messages,
        }
    }

    fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    fn request(&mut self, id: u32, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let response = self
                .messages
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert_eq!(response["jsonrpc"], "2.0");
            if response.get("id") == Some(&json!(id)) {
                return response;
            }
        }
    }

    fn shutdown(&mut self) {
        self.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "MCP server failed on EOF: {status}");
                return;
            }
            assert!(Instant::now() < deadline, "MCP server did not exit on EOF");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn initialize(&mut self) -> Value {
        let response = self.request(
            1,
            "initialize",
            json!({
                "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "exact-mcp-test", "version": "1"}
            }),
        );
        assert!(response.get("error").is_none(), "{response}");
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        response
    }
}

impl Drop for McpProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn fixture_session(base_url: &str) -> (TempDir, PathBuf, Value) {
    let root = TempDir::new().unwrap();
    let directory = root.path().join("data");
    std::fs::create_dir(&directory).unwrap();
    let state = json!({
        "base_url": base_url,
        "client_id": "fixture-client",
        "redirect_uri": "https://exact-mcp.localhost:8400/callback",
        "tokens": {
            "access_token": "fixture-access-token", "refresh_token": "fixture-refresh-token",
            "expires_at": SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 3600
        },
        "administration": {"Code": 42, "Description": "Test administration"}
    });
    let file = directory.join("session.json");
    std::fs::write(&file, serde_json::to_vec(&state).unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    (root, directory, state)
}

fn api_server() -> (String, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        let account = json!({"ID": ACCOUNT_ID, "Code": "1001", "Name": "Company X", "IsSupplier": true, "IsSales": false});
        let responses = [
            json!({"d": [account.clone()]}),
            json!({"d": {"results": [account]}}),
            json!({"d": {"results": [
                {"HID": 1, "AccountId": ACCOUNT_ID, "Amount": 0.1, "AmountInTransit": 0.1, "CurrencyCode": "EUR", "DueDate": "2000-01-01", "YourRef": "INV-1"},
                {"HID": 2, "AccountId": ACCOUNT_ID, "Amount": 0.2, "AmountInTransit": 0, "CurrencyCode": "EUR", "DueDate": "2000-01-01", "YourRef": "INV-2"}
            ]}}),
        ];
        let mut targets = Vec::new();
        for response in responses {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "Mock API request timed out");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("Mock API failed: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut reader = BufReader::new((&stream).take(8192));
            let mut target = String::new();
            reader.read_line(&mut target).unwrap();
            assert!(target.starts_with("GET /api/v1/42/"), "{target}");
            targets.push(target);
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
            }
            let body = response.to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
        targets
    });
    (base, handle)
}

#[test]
fn stdio_handshake_discovery_and_financial_queries_are_read_only() {
    let (base, api) = api_server();
    let (_root, directory, _) = fixture_session(&base);
    let mut process = McpProcess::start(&directory);
    let info = process.initialize();
    assert_eq!(info["result"]["serverInfo"]["name"], "exact-mcp");
    assert!(info["result"]["capabilities"]["tools"].is_object());
    let discovery = process.request(2, "tools/list", json!({}));
    let tools = discovery["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 3);
    for tool in tools {
        assert_eq!(tool["annotations"]["readOnlyHint"], true);
        assert_eq!(tool["annotations"]["destructiveHint"], false);
        assert!(matches!(
            tool["name"].as_str(),
            Some("search_accounts" | "get_open_items" | "get_outstanding_balances")
        ));
    }
    let search_schema = &tools
        .iter()
        .find(|tool| tool["name"] == "search_accounts")
        .unwrap()["inputSchema"]["properties"];
    assert!(search_schema["match_by"].is_object());
    assert!(search_schema["offset"].is_object());
    let report_schema = &tools
        .iter()
        .find(|tool| tool["name"] == "get_open_items")
        .unwrap()["inputSchema"]["properties"];
    for field in [
        "due_date_from",
        "due_date_to",
        "overdue_only",
        "summary_only",
    ] {
        assert!(report_schema[field].is_object());
    }
    let balances_schema = &tools
        .iter()
        .find(|tool| tool["name"] == "get_outstanding_balances")
        .unwrap()["inputSchema"]["properties"];
    for field in [
        "side",
        "due_date_from",
        "due_date_to",
        "overdue_only",
        "limit",
        "offset",
    ] {
        assert!(balances_schema[field].is_object());
    }
    let search = process.request(
        3,
        "tools/call",
        json!({"name": "search_accounts", "arguments": {"query": "Company"}}),
    );
    assert_ne!(search["result"]["isError"], true, "{search}");
    assert_eq!(
        search["result"]["structuredContent"]["accounts"][0]["id"],
        ACCOUNT_ID
    );
    let report = process.request(4, "tools/call", json!({"name": "get_open_items", "arguments": {"account_id": ACCOUNT_ID, "side": "payable"}}));
    assert_ne!(report["result"]["isError"], true, "{report}");
    let structured = &report["result"]["structuredContent"];
    assert_eq!(structured["administration"]["Code"], 42);
    assert_eq!(
        structured["totals_by_currency"]["EUR"]["net_open_amount"],
        "0.3"
    );
    assert_eq!(
        structured["totals_by_currency"]["EUR"]["known_in_transit_amount"],
        "0.1"
    );
    assert_eq!(structured["items"][0]["your_ref"], "INV-1");
    let text = report["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(serde_json::from_str::<Value>(text).unwrap(), *structured);
    assert!(!report.to_string().contains("fixture-access-token"));
    assert!(!report.to_string().contains("fixture-refresh-token"));
    let targets = api.join().unwrap();
    assert_eq!(targets.len(), 3);
    assert!(targets[2].contains("PayablesListByAccount"));
    process.shutdown();
}

#[test]
fn stdio_tool_errors_and_administration_changes_do_not_require_api_access() {
    let (_root, directory, mut state) = fixture_session("http://127.0.0.1:1");
    let mut process = McpProcess::start(&directory);
    process.initialize();
    for (id, name, arguments) in [
        (2, "search_accounts", json!({"query": ""})),
        (
            5,
            "get_outstanding_balances",
            json!({"side": "payable", "limit": 0}),
        ),
        (
            6,
            "get_outstanding_balances",
            json!({"side": "receivable", "due_date_from": "2026-02-01", "due_date_to": "2026-01-01"}),
        ),
        (
            3,
            "get_open_items",
            json!({"account_id": "not-a-uuid", "side": "receivable"}),
        ),
    ] {
        let response = process.request(
            id,
            "tools/call",
            json!({"name": name, "arguments": arguments}),
        );
        assert!(response.get("error").is_none(), "{response}");
        assert_eq!(response["result"]["isError"], true);
        assert!(!response.to_string().contains("fixture-refresh-token"));
    }
    state["administration"]["Code"] = json!(99);
    std::fs::write(
        directory.join("session.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    let response = process.request(
        4,
        "tools/call",
        json!({"name": "search_accounts", "arguments": {"query": "Company"}}),
    );
    assert_eq!(response["result"]["isError"], true);
    assert!(
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("restart")
    );
    let response = process.request(
        7,
        "tools/call",
        json!({"name": "get_outstanding_balances", "arguments": {"side": "payable"}}),
    );
    assert_eq!(response["result"]["isError"], true);
    assert!(
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("restart")
    );
    process.shutdown();
}

fn accept_request(listener: &TcpListener) -> (std::net::TcpStream, String) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "Mock request timed out");
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("Mock request failed: {error}"),
        }
    };
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut reader = BufReader::new(&mut stream);
    let mut target = String::new();
    reader.read_line(&mut target).unwrap();
    let mut length = 0;
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    drop(reader);
    (stream, target)
}

fn send_response(stream: &mut std::net::TcpStream, body: Value) {
    let body = body.to_string();
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
}

#[test]
fn stdio_tool_calls_can_read_the_api_concurrently() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let api = thread::spawn(move || {
        let (mut first, target) = accept_request(&listener);
        assert!(target.starts_with("GET /api/v1/42/crm/Accounts?"));
        let (mut second, target) = accept_request(&listener);
        assert!(target.starts_with("GET /api/v1/42/crm/Accounts?"));
        let empty = json!({"d": {"results": []}});
        send_response(&mut first, empty.clone());
        send_response(&mut second, empty);
    });
    let (_root, directory, _) = fixture_session(&base);
    let mut process = McpProcess::start(&directory);
    process.initialize();
    for id in [2, 3] {
        process.send(json!({"jsonrpc":"2.0", "id":id, "method":"tools/call", "params":{"name":"search_accounts","arguments":{"query":"Company"}}}));
    }
    let mut ids = Vec::new();
    for _ in 0..2 {
        let response = process
            .messages
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_ne!(response["result"]["isError"], true, "{response}");
        assert_eq!(
            response["result"]["structuredContent"]["accounts"],
            json!([])
        );
        ids.push(response["id"].as_u64().unwrap());
    }
    ids.sort();
    assert_eq!(ids, [2, 3]);
    process.shutdown();
    api.join().unwrap();
}

#[test]
fn cancelled_mcp_tool_and_stdin_eof_still_save_rotated_tokens_before_exit() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (started, entered) = channel();
    let (release, continue_refresh) = channel();
    let api = thread::spawn(move || {
        let (mut socket, target) = accept_request(&listener);
        assert!(target.starts_with("POST /api/oauth2/token"));
        started.send(()).unwrap();
        continue_refresh
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        send_response(
            &mut socket,
            json!({
                "access_token":"rotated-access", "refresh_token":"rotated-refresh",
                "token_type":"bearer", "expires_in":600
            }),
        );
    });
    let (_root, directory, mut state) = fixture_session(&base);
    state["tokens"]["expires_at"] = json!(0);
    std::fs::write(
        directory.join("session.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    let mut process = McpProcess::start(&directory);
    process.initialize();
    process.send(json!({"jsonrpc":"2.0", "id":2, "method":"tools/call", "params":{"name":"search_accounts","arguments":{"query":"Company"}}}));
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    process.send(json!({"jsonrpc":"2.0", "method":"notifications/cancelled", "params":{"requestId":2,"reason":"test cancellation"}}));
    process.stdin.take();
    let deadline = Instant::now() + Duration::from_millis(100);
    while Instant::now() < deadline {
        assert!(
            process.child.try_wait().unwrap().is_none(),
            "Server exited before saving the refresh"
        );
        thread::sleep(Duration::from_millis(5));
    }
    release.send(()).unwrap();
    process.shutdown();
    let stored: Value =
        serde_json::from_slice(&std::fs::read(directory.join("session.json")).unwrap()).unwrap();
    assert_eq!(stored["tokens"]["refresh_token"], "rotated-refresh");
    assert_eq!(stored["tokens"]["access_token"], "rotated-access");
    assert_eq!(stored["administration"]["Code"], 42);
    api.join().unwrap();
}

#[test]
fn stdio_exact_code_search_and_filtered_summary_return_scoped_totals() {
    let (base, api) = api_server();
    let (_root, directory, _) = fixture_session(&base);
    let mut process = McpProcess::start(&directory);
    process.initialize();
    let search = process.request(2, "tools/call", json!({
        "name": "search_accounts", "arguments": {"query": "1001", "match_by": "code", "offset": 0, "limit": 1}
    }));
    assert_ne!(search["result"]["isError"], true, "{search}");
    let search_result = &search["result"]["structuredContent"];
    assert_eq!(search_result["accounts"][0]["id"], ACCOUNT_ID);
    assert_eq!(search_result["match_by"], "code");
    assert_eq!(search_result["has_more"], false);
    assert_eq!(search_result["next_offset"], Value::Null);
    let report = process.request(
        3,
        "tools/call",
        json!({
            "name": "get_open_items", "arguments": {
                "account_id": ACCOUNT_ID, "side": "receivable", "due_date_from": "2000-01-01",
                "due_date_to": "2000-01-01", "overdue_only": true, "summary_only": true
            }
        }),
    );
    assert_ne!(report["result"]["isError"], true, "{report}");
    let result = &report["result"]["structuredContent"];
    assert!(result.get("items").is_none());
    assert_eq!(result["source_item_count"], 2);
    assert_eq!(result["matched_item_count"], 2);
    assert_eq!(result["amount_basis"], "exact_report_currency");
    assert_eq!(result["filters"]["due_date_to"], "2000-01-01");
    assert_eq!(
        result["totals_by_currency"]["EUR"]["net_open_amount"],
        "0.3"
    );
    let targets = api.join().unwrap();
    let url = reqwest::Url::parse(&format!(
        "{base}{}",
        targets[0].split_whitespace().nth(1).unwrap()
    ))
    .unwrap();
    let parameters: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(parameters["$filter"], "trim(Code) eq '1001'");
    assert!(targets[2].contains("ReceivablesListByAccount"));
    process.shutdown();
}

#[test]
fn stdio_outstanding_balances_cover_all_pages_and_support_invoice_drilldown() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let api = thread::spawn(move || {
        let account = json!({"ID": ACCOUNT_ID, "Code": "1001", "Name": "Company X", "IsSupplier": true, "IsSales": false});
        let first = json!({"HID": 1, "AccountId": ACCOUNT_ID, "AccountCode": "1001", "AccountName": "Company X", "Amount": 0.1, "AmountInTransit": 0, "CurrencyCode": "EUR", "DueDate": "2000-01-01"});
        let mut second = first.clone();
        second["HID"] = json!(2);
        second["Amount"] = json!(0.2);
        let mut other = first.clone();
        other["HID"] = json!(3);
        other["AccountId"] = json!("22222222-2222-2222-2222-222222222222");
        other["AccountCode"] = json!("1002");
        other["AccountName"] = json!("Second company");
        other["Amount"] = json!(0.3);
        let responses = [
            (
                "/read/financial/PayablesList?",
                json!({"d": {"results": [first.clone(), other], "__next": "?page=2"}}),
            ),
            (
                "/read/financial/PayablesList?page=2",
                json!({"d": [second.clone()]}),
            ),
            ("/crm/Accounts?", json!({"d": [account]})),
            (
                "/read/financial/PayablesListByAccount?",
                json!({"d": [first, second]}),
            ),
        ];
        for (path, body) in responses {
            let (mut stream, target) = accept_request(&listener);
            assert!(target.starts_with("GET /api/v1/42/"), "{target}");
            assert!(target.contains(path), "{target}");
            send_response(&mut stream, body);
        }
    });
    let (_root, directory, _) = fixture_session(&base);
    let mut process = McpProcess::start(&directory);
    process.initialize();
    let response = process.request(2, "tools/call", json!({"name": "get_outstanding_balances", "arguments": {"side": "payable", "overdue_only": true, "limit": 1}}));
    assert_ne!(response["result"]["isError"], true, "{response}");
    let result = &response["result"]["structuredContent"];
    assert_eq!(result["administration"]["Code"], 42);
    assert_eq!(result["source_item_count"], 3);
    assert_eq!(result["matched_account_count"], 2);
    assert_eq!(result["accounts"].as_array().unwrap().len(), 1);
    assert_eq!(result["accounts"][0]["account"]["id"], ACCOUNT_ID);
    assert_eq!(
        result["accounts"][0]["totals_by_currency"]["EUR"]["net_open_amount"],
        "0.3"
    );
    assert_eq!(
        result["totals_by_currency"]["EUR"]["net_open_amount"],
        "0.6"
    );
    assert_eq!(result["next_offset"], 1);
    assert_eq!(result["has_more"], true);
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(serde_json::from_str::<Value>(text).unwrap(), *result);
    let account_id = &result["accounts"][0]["account"]["id"];
    let details = process.request(3, "tools/call", json!({"name": "get_open_items", "arguments": {"account_id": account_id, "side": "payable"}}));
    assert_ne!(details["result"]["isError"], true, "{details}");
    assert_eq!(
        details["result"]["structuredContent"]["totals_by_currency"]["EUR"],
        result["accounts"][0]["totals_by_currency"]["EUR"]
    );
    assert_eq!(
        details["result"]["structuredContent"]["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    api.join().unwrap();
    process.shutdown();
}

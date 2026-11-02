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
    assert_eq!(tools.len(), 8);
    for tool in tools {
        assert_eq!(
            tool["annotations"]["readOnlyHint"],
            tool["name"] != "download_invoice_pdf"
        );
        assert_eq!(tool["annotations"]["destructiveHint"], false);
        if tool["name"] == "download_invoice_pdf" {
            assert_eq!(tool["annotations"]["idempotentHint"], false);
        }
        assert!(matches!(
            tool["name"].as_str(),
            Some(
                "search_accounts"
                    | "search_invoices"
                    | "get_open_items"
                    | "get_outstanding_balances"
                    | "get_invoice_details"
                    | "get_invoice_matches"
                    | "list_invoice_documents"
                    | "download_invoice_pdf"
            )
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
    let invoice_schema = &tools
        .iter()
        .find(|tool| tool["name"] == "get_invoice_details")
        .unwrap()["inputSchema"]["properties"];
    for field in ["account_id", "side", "entry_number", "journal_code"] {
        assert!(invoice_schema[field].is_object());
    }
    let download_schema = &tools
        .iter()
        .find(|tool| tool["name"] == "download_invoice_pdf")
        .unwrap()["inputSchema"]["properties"];
    for field in ["invoice", "document_id", "attachment_id"] {
        assert!(download_schema[field].is_object());
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
    assert!(structured["items"][0]["days_overdue"].as_i64().unwrap() > 90);
    assert_eq!(
        structured["totals_by_currency"]["EUR"]["aging_by_due_date"]["overdue_over_90_days"]["net_open_amount"],
        "0.3"
    );
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
            8,
            "get_invoice_details",
            json!({"account_id": ACCOUNT_ID, "side": "payable", "entry_number": 0, "journal_code": "60"}),
        ),
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
    let response = process.request(9, "tools/call", json!({"name": "get_invoice_details", "arguments": {"account_id": ACCOUNT_ID, "side": "payable", "entry_number": 12345, "journal_code": "60"}}));
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
    assert_eq!(
        result["totals_by_currency"]["EUR"]["aging_by_due_date"]["overdue_over_90_days"]["item_count"],
        2
    );
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
    assert_eq!(
        result["totals_by_currency"]["EUR"]["aging_by_due_date"]["overdue_over_90_days"]["net_open_amount"],
        "0.6"
    );
    assert_eq!(
        result["accounts"][0]["totals_by_currency"]["EUR"]["aging_by_due_date"]["overdue_over_90_days"]
            ["net_open_amount"],
        "0.3"
    );
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

#[test]
fn stdio_invoice_accounting_details_preserve_signs_vat_and_currency_context() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let api = thread::spawn(move || {
        let entry_id = "44444444-4444-4444-4444-444444444444";
        let responses = [
            (
                "/purchaseentry/PurchaseEntries?",
                json!({"d": [{
                    "EntryID": entry_id, "Division": 42, "EntryNumber": 12345, "Journal": "60",
                    "Supplier": ACCOUNT_ID, "SupplierName": "Company X", "Currency": "USD",
                    "AmountDC": -90, "AmountFC": -100, "VATAmountDC": 0, "VATAmountFC": 0,
                    "Type": 30, "Reversal": false, "Document": "55555555-5555-5555-5555-555555555555"
                }]}),
            ),
            (
                "/system/Divisions?",
                json!({"d": [{"Code": 42, "Currency": "EUR"}]}),
            ),
            (
                "/purchaseentry/PurchaseEntryLines?",
                json!({"d": [{
                    "ID": "66666666-6666-6666-6666-666666666666", "EntryID": entry_id,
                    "Division": 42, "LineNumber": 1, "AmountDC": 90, "AmountFC": 100,
                    "VATAmountDC": 18.9, "VATAmountFC": 21, "VATPercentage": 0.21, "Quantity": 0.125
                }]}),
            ),
        ];
        for (path, body) in responses {
            let (mut stream, target) = accept_request(&listener);
            assert!(target.starts_with("GET /api/v1/42/"));
            assert!(target.contains(path), "{target}");
            send_response(&mut stream, body);
        }
    });
    let (_root, directory, _) = fixture_session(&base);
    let mut process = McpProcess::start(&directory);
    process.initialize();
    let response = process.request(
        2,
        "tools/call",
        json!({"name": "get_invoice_details", "arguments": {
            "account_id": ACCOUNT_ID, "side": "payable", "entry_number": 12345, "journal_code": "60"
        }}),
    );
    assert_ne!(response["result"]["isError"], true, "{response}");
    let result = &response["result"]["structuredContent"];
    assert_eq!(result["administration"]["Code"], 42);
    assert_eq!(result["amount_basis"], "exact_accounting_entry");
    assert_eq!(result["default_currency"], "EUR");
    assert_eq!(result["entry"]["currency"], "USD");
    assert_eq!(result["entry"]["account_id"], ACCOUNT_ID);
    assert_eq!(result["entry"]["amount_transaction_currency"], "-100");
    assert_eq!(result["entry"]["type_code"], 30);
    assert_eq!(result["entry"]["vat_amount_transaction_currency"], "0");
    assert_eq!(result["line_count"], 1);
    assert_eq!(result["lines"][0]["amount_transaction_currency"], "100");
    assert_eq!(result["lines"][0]["vat_amount_transaction_currency"], "21");
    assert_eq!(result["lines"][0]["vat_percentage"], "0.21");
    assert_eq!(result["lines"][0]["quantity"], "0.125");
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(serde_json::from_str::<Value>(text).unwrap(), *result);
    assert!(!response.to_string().contains("fixture-access-token"));
    api.join().unwrap();
    process.shutdown();
}

#[test]
fn stdio_invoice_pdf_download_writes_only_a_private_local_pdf_and_returns_no_bytes() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let pdf = "%PDF-1.7\nsynthetic-private-pdf-content\n%%EOF\n";
    let api = thread::spawn(move || {
        let entry_id = "44444444-4444-4444-4444-444444444444";
        let document_id = "55555555-5555-5555-5555-555555555555";
        let attachment_id = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let header = json!({"EntryID":entry_id,"Division":42,"EntryNumber":12345,"Journal":"60","Supplier":ACCOUNT_ID,"Currency":"USD","Document":document_id});
        let document = json!({"ID":document_id,"Division":42,"Account":ACCOUNT_ID,"Type":20,"TypeDescription":"Purchase invoice","Subject":"Invoice","FinancialTransactionEntryID":entry_id});
        let attachment = json!({"ID":attachment_id,"Document":document_id,"FileName":"invoice.pdf","FileSize":pdf.len(),"Url":"https://untrusted.invalid/do-not-follow"});
        let responses = [
            (
                "/api/v1/42/purchaseentry/PurchaseEntries?",
                json!({"d":[header]}).to_string(),
            ),
            (
                "/api/v1/42/documents/Documents?",
                json!({"d":[document.clone()]}).to_string(),
            ),
            (
                "/api/v1/42/documents/Documents?",
                json!({"d":[document]}).to_string(),
            ),
            (
                "/api/v1/42/documents/DocumentAttachments?",
                json!({"d":[attachment.clone()]}).to_string(),
            ),
            ("/docs/SysAttachment.aspx?", pdf.to_owned()),
            (
                "/api/v1/42/documents/DocumentAttachments?",
                json!({"d":[attachment]}).to_string(),
            ),
        ];
        for (path, body) in responses {
            let (mut stream, target) = accept_request(&listener);
            assert!(target.starts_with(&format!("GET {path}")), "{target}");
            let content_type = if path.starts_with("/docs/") {
                "application/pdf"
            } else {
                "application/json"
            };
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
    });
    let (_root, directory, _) = fixture_session(&base);
    let mut process = McpProcess::start(&directory);
    process.initialize();
    let response = process.request(2,"tools/call",json!({"name":"download_invoice_pdf","arguments":{
        "invoice":{"account_id":ACCOUNT_ID,"side":"payable","entry_number":12345,"journal_code":"60"},
        "document_id":"55555555-5555-5555-5555-555555555555","attachment_id":"aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"
    }}));
    assert_ne!(response["result"]["isError"], true, "{response}");
    let result = &response["result"]["structuredContent"];
    let path = PathBuf::from(result["saved_path"].as_str().unwrap());
    assert!(path.starts_with(directory.canonicalize().unwrap().join("downloads/42")));
    assert_eq!(std::fs::read(&path).unwrap(), pdf.as_bytes());
    assert_eq!(result["byte_count"], pdf.len());
    assert_eq!(result["media_type"], "application/pdf");
    assert!(
        !response
            .to_string()
            .contains("synthetic-private-pdf-content")
    );
    assert!(!response.to_string().contains("fixture-access-token"));
    assert!(!response.to_string().contains("untrusted.invalid"));
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(serde_json::from_str::<Value>(text).unwrap(), *result);
    api.join().unwrap();
    process.shutdown();
}

#[test]
fn stdio_invoice_matches_return_scoped_allocations_without_claiming_a_payment_audit() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let api = thread::spawn(move || {
        let entry = "44444444-4444-4444-4444-444444444444";
        let ledger = json!({"ID":"66666666-6666-6666-6666-666666666666","EntryID":entry,"Division":42,"Account":ACCOUNT_ID,
            "EntryNumber":12345,"FinancialYear":2025,"FinancialPeriod":12,"JournalCode":"70","GLAccountCode":"1200","LineNumber":0,"AmountDC":100});
        let responses=vec![
            ("/salesentry/SalesEntries?",json!({"d":[{"EntryID":entry,"Division":42,"EntryNumber":12345,"Journal":"70","Customer":ACCOUNT_ID,"Currency":"USD","AmountDC":100,"AmountFC":120}]}).to_string()),
            ("/crm/Accounts?",json!({"d":[{"ID":ACCOUNT_ID,"Division":42,"Code":" 00042 "}]}).to_string()),
            ("/system/Divisions?",json!({"d":[{"Code":42,"Currency":"EUR"}]}).to_string()),
            ("/financialtransaction/TransactionLines?",json!({"d":[ledger]}).to_string()),
            ("/docs/XMLDownload.aspx?","<eExact><MatchSets><MatchSet><GLAccount code=\"1200\"/><Account code=\"00042\"/><MatchLines><MatchLine finyear=\"2025\" finperiod=\"12\" journal=\"70\" entry=\"12345\" amountdc=\"100\"/><MatchLine finyear=\"2025\" finperiod=\"12\" journal=\"20\" entry=\"54321\" amountdc=\"-100\"/></MatchLines></MatchSet></MatchSets><Topics><Topic code=\"MatchSets\" ts_d=\"0x0000000000000001\" count=\"1\" pagesize=\"1000\"/></Topics><Messages/></eExact>".into()),
            ("/financialtransaction/TransactionLines?",json!({"d":[]}).to_string())
        ];
        for (path, body) in responses {
            let (mut stream, target) = accept_request(&listener);
            assert!(target.starts_with("GET "));
            assert!(target.contains(path), "{target}");
            if path.starts_with("/docs/") {
                assert!(target.contains("_Division_=42"));
                assert!(target.contains("Topic=MatchSets"));
                assert!(!target.contains("DownloadID"));
            }
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
    });
    let (_root, directory, _) = fixture_session(&base);
    let mut process = McpProcess::start(&directory);
    process.initialize();
    let response=process.request(2,"tools/call",json!({"name":"get_invoice_matches","arguments":{"account_id":ACCOUNT_ID,"side":"receivable","entry_number":12345,"journal_code":"70"}}));
    assert_ne!(response["result"]["isError"], true, "{response}");
    let result = &response["result"]["structuredContent"];
    assert_eq!(result["matching_group_count"], 1);
    assert_eq!(result["default_currency"], "EUR");
    assert_eq!(result["entry"]["currency"], "USD");
    assert_eq!(
        result["groups"][0]["members"][1]["allocated_amount_default_currency"],
        "-100"
    );
    assert!(
        result["ledger_evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["resolution"] == "not_found")
    );
    assert!(
        result["interpretation"]
            .as_str()
            .unwrap()
            .contains("not bank settlement")
    );
    assert!(!response.to_string().contains("fixture-access-token"));
    assert!(!directory.join("downloads").exists());
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(serde_json::from_str::<Value>(text).unwrap(), *result);
    api.join().unwrap();
    process.shutdown();
}

#[test]
fn stdio_invoice_search_disambiguates_reused_numbers_for_accounting_drilldown() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let first_id = "44444444-4444-4444-4444-444444444444";
    let second_id = "77777777-7777-7777-7777-777777777777";
    let api = thread::spawn(move || {
        let header = |id: &str, date: &str| json!({"EntryID":id,"Division":42,"EntryNumber":12345,"Journal":"70","Customer":ACCOUNT_ID,"Currency":"USD","EntryDate":date,"YourRef":"REF-42","AmountDC":100,"AmountFC":120,"Status":50});
        let old = header(first_id, "2024-01-01");
        let recent = header(second_id, "2025-01-01");
        let responses = vec![
            (
                "/crm/Accounts?",
                json!({"d":[{"ID":ACCOUNT_ID,"Division":42,"Code":" 100 ","Name":"Company X"}]}),
            ),
            (
                "/system/Divisions?",
                json!({"d":[{"Code":42,"Currency":"EUR"}]}),
            ),
            (
                "/salesentry/SalesEntries?",
                json!({"d":[old.clone(),recent]}),
            ),
            ("/salesentry/SalesEntries?", json!({"d":[old]})),
            (
                "/system/Divisions?",
                json!({"d":[{"Code":42,"Currency":"EUR"}]}),
            ),
            ("/salesentry/SalesEntryLines?", json!({"d":[]})),
        ];
        for (index, (path, body)) in responses.into_iter().enumerate() {
            let (mut stream, target) = accept_request(&listener);
            assert!(target.starts_with("GET /api/v1/42/"));
            assert!(target.contains(path));
            if index == 3 {
                let url = reqwest::Url::parse(&format!(
                    "http://localhost{}",
                    target.split_whitespace().nth(1).unwrap()
                ))
                .unwrap();
                let parameters: std::collections::HashMap<_, _> =
                    url.query_pairs().into_owned().collect();
                assert!(parameters["$filter"].contains(&format!("EntryID eq guid'{first_id}'")));
            }
            send_response(&mut stream, body);
        }
    });
    let (_root, directory, _) = fixture_session(&base);
    let mut process = McpProcess::start(&directory);
    process.initialize();
    let response=process.request(2,"tools/call",json!({"name":"search_invoices","arguments":{"account_id":ACCOUNT_ID,"side":"receivable","query":"ref-42"}}));
    assert_ne!(response["result"]["isError"], true, "{response}");
    let result = &response["result"]["structuredContent"];
    assert_eq!(result["matched_entry_count"], 2);
    assert_eq!(result["default_currency"], "EUR");
    assert_eq!(result["invoices"][0]["entry_id"], second_id);
    let selected = result["invoices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["entry_id"] == first_id)
        .unwrap();
    let details=process.request(3,"tools/call",json!({"name":"get_invoice_details","arguments":{"account_id":selected["account_id"],"entry_id":selected["entry_id"],"entry_number":selected["entry_number"],"journal_code":selected["journal_code"],"side":result["side"]}}));
    assert_ne!(details["result"]["isError"], true, "{details}");
    assert_eq!(
        details["result"]["structuredContent"]["entry"]["entry_id"],
        first_id
    );
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(serde_json::from_str::<Value>(text).unwrap(), *result);
    assert!(!response.to_string().contains("fixture-access-token"));
    api.join().unwrap();
    process.shutdown();
}

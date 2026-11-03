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
    assert_eq!(tools.len(), 34);
    for tool in tools {
        assert_eq!(
            tool["annotations"]["readOnlyHint"],
            tool["name"] != "download_invoice_pdf"
                && tool["name"] != "download_purchase_document_pdf"
        );
        assert_eq!(tool["annotations"]["destructiveHint"], false);
        if tool["name"] == "download_invoice_pdf"
            || tool["name"] == "download_purchase_document_pdf"
        {
            assert_eq!(tool["annotations"]["idempotentHint"], false);
        }
        assert!(matches!(
            tool["name"].as_str(),
            Some(
                "search_accounts"
                    | "preview_bank_gl_xml"
                    | "propose_bank_transaction_handling"
                    | "preflight_purchase_booking"
                    | "assess_purchase_duplicates"
                    | "search_purchase_documents"
                    | "get_purchase_document"
                    | "download_purchase_document_pdf"
                    | "search_items"
                    | "get_item_details"
                    | "search_units"
                    | "search_supplier_items"
                    | "get_account_booking_context"
                    | "search_vat_codes"
                    | "search_journals"
                    | "search_payment_conditions"
                    | "search_cost_centers"
                    | "search_cost_units"
                    | "search_projects"
                    | "get_financial_periods"
                    | "get_journal_period_status"
                    | "search_draft_invoices"
                    | "get_draft_invoice_details"
                    | "search_invoices"
                    | "get_open_items"
                    | "get_outstanding_balances"
                    | "get_invoice_details"
                    | "get_invoice_matches"
                    | "get_payment_details"
                    | "get_bank_transactions_to_handle"
                    | "search_gl_accounts"
                    | "get_ledger_report"
                    | "list_invoice_documents"
                    | "download_invoice_pdf"
            )
        ));
    }
    for name in [
        "search_items",
        "search_units",
        "search_vat_codes",
        "search_journals",
        "search_payment_conditions",
        "search_cost_centers",
        "search_cost_units",
        "search_projects",
    ] {
        let schema = &tools.iter().find(|tool| tool["name"] == name).unwrap()["inputSchema"];
        assert_eq!(schema["additionalProperties"], false);
        assert!(schema["properties"]["query"].is_object());
        assert!(schema["properties"]["match_by"].is_object());
        assert!(schema["properties"]["offset"].is_object());
        assert!(
            !schema["required"]
                .as_array()
                .is_some_and(|required| required.contains(&json!("query")))
        );
    }
    let xml_schema = &tools
        .iter()
        .find(|tool| tool["name"] == "preview_bank_gl_xml")
        .unwrap()["inputSchema"];
    assert_eq!(xml_schema["additionalProperties"], false);
    assert_eq!(
        xml_schema["properties"]["expected_pending_amount"]["type"],
        "string"
    );
    assert!(xml_schema["properties"].get("execute").is_none());
    assert!(xml_schema["properties"].get("approval_token").is_none());
    for field in [
        "bank_transaction_line_id",
        "pending_posting_line_id",
        "destination_gl_account_id",
        "no_vat_code",
        "expected_pending_amount",
        "expected_currency",
    ] {
        assert!(
            xml_schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!(field))
        );
    }
    let bank_schema = &tools
        .iter()
        .find(|tool| tool["name"] == "propose_bank_transaction_handling")
        .unwrap()["inputSchema"];
    assert_eq!(bank_schema["additionalProperties"], false);
    for field in ["bank_transaction_line_id", "pending_posting_line_id"] {
        assert!(
            bank_schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!(field))
        );
    }
    assert_eq!(
        bank_schema["$defs"]["InvoiceAllocation"]["properties"]["amount"]["type"],
        "string"
    );
    assert_eq!(
        bank_schema["$defs"]["GlSplit"]["properties"]["net_amount"]["type"],
        "string"
    );
    assert_eq!(
        bank_schema["$defs"]["GlSplit"]["additionalProperties"],
        false
    );
    let preflight_schema = &tools
        .iter()
        .find(|tool| tool["name"] == "preflight_purchase_booking")
        .unwrap()["inputSchema"];
    assert_eq!(preflight_schema["additionalProperties"], false);
    for required in [
        "account_id",
        "supplier_invoice_reference",
        "document_kind",
        "invoice_date",
        "financial_year",
        "financial_period",
        "journal_code",
        "currency",
        "gross_amount_transaction_currency",
        "lines",
    ] {
        assert!(
            preflight_schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!(required))
        );
    }
    assert_eq!(
        preflight_schema["properties"]["gross_amount_transaction_currency"]["type"],
        "string"
    );
    let line_reference = preflight_schema["properties"]["lines"]["items"]["$ref"]
        .as_str()
        .unwrap();
    let line_schema = preflight_schema
        .pointer(line_reference.strip_prefix('#').unwrap())
        .unwrap();
    assert_eq!(line_schema["additionalProperties"], false);
    assert_eq!(
        line_schema["properties"]["net_amount_transaction_currency"]["type"],
        "string"
    );
    assert!(line_schema["properties"].get("project_id").is_none());
    assert!(line_schema["properties"].get("item_id").is_none());
    let duplicate_schema = &tools
        .iter()
        .find(|tool| tool["name"] == "assess_purchase_duplicates")
        .unwrap()["inputSchema"];
    assert_eq!(duplicate_schema["additionalProperties"], false);
    for required in ["account_id", "supplier_invoice_reference"] {
        assert!(
            duplicate_schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!(required))
        );
    }
    for optional in [
        "invoice_date",
        "currency",
        "amount_transaction_currency",
        "source_document_id",
        "limit",
        "offset",
    ] {
        assert!(duplicate_schema["properties"][optional].is_object());
        assert!(
            !duplicate_schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!(optional))
        );
    }
    let intake_schema = &tools
        .iter()
        .find(|tool| tool["name"] == "search_purchase_documents")
        .unwrap()["inputSchema"];
    assert_eq!(intake_schema["additionalProperties"], false);
    for property in [
        "account_id",
        "query",
        "link_filter",
        "proposed_entry_status",
        "document_date_from",
        "document_date_to",
        "limit",
        "offset",
    ] {
        assert!(intake_schema["properties"][property].is_object());
    }
    assert!(
        !intake_schema["required"]
            .as_array()
            .is_some_and(|required| required.contains(&json!("account_id")))
    );
    for name in ["get_purchase_document", "download_purchase_document_pdf"] {
        let schema = &tools.iter().find(|tool| tool["name"] == name).unwrap()["inputSchema"];
        assert_eq!(schema["additionalProperties"], false);
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!("document_id"))
        );
        assert!(schema["properties"].get("invoice").is_none());
        assert!(schema["properties"].get("url").is_none());
        if name == "download_purchase_document_pdf" {
            assert!(
                schema["required"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("attachment_id"))
            );
        }
    }
    for (name, required) in [
        ("get_item_details", "item_id"),
        ("search_supplier_items", "account_id"),
        ("get_account_booking_context", "account_id"),
    ] {
        let schema = &tools.iter().find(|tool| tool["name"] == name).unwrap()["inputSchema"];
        assert_eq!(schema["additionalProperties"], false);
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!(required))
        );
    }
    let lock_schema = &tools
        .iter()
        .find(|tool| tool["name"] == "get_journal_period_status")
        .unwrap()["inputSchema"];
    assert!(
        lock_schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("journal_code"))
    );
    assert!(
        lock_schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("financial_year"))
    );
    let bank_schema = &tools
        .iter()
        .find(|tool| tool["name"] == "get_bank_transactions_to_handle")
        .unwrap()["inputSchema"];
    assert!(bank_schema["properties"]["unassigned_gl_account_ids"].is_object());
    assert!(
        !bank_schema["required"]
            .as_array()
            .is_some_and(|required| required.contains(&json!("unassigned_gl_account_ids")))
    );
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

#[test]
fn stdio_payment_details_are_read_only_posting_evidence_not_payment_execution() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let api = thread::spawn(move || {
        let responses = [
            (
                "/financialtransaction/TransactionLines?",
                json!({"d":[{"ID":"77777777-7777-7777-7777-777777777777","EntryID":"99999999-9999-9999-9999-999999999999","Division":42,"Account":ACCOUNT_ID,"EntryNumber":54321,"FinancialYear":2025,"FinancialPeriod":12,"JournalCode":"20","LineNumber":1,"Type":40,"AmountDC":-100,"AmountFC":-120,"Currency":"USD","Date":"2025-12-20","OffsetID":null}]}),
            ),
            (
                "/system/Divisions?",
                json!({"d":[{"Code":42,"Currency":"EUR"}]}),
            ),
            (
                "/financial/Journals?",
                json!({"d":[{"ID":"aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa","Division":42,"Code":"20","Type":16,"Description":"Payment service journal","Currency":"EUR","PaymentServiceProviderName":"Provider X"}]}),
            ),
        ];
        for (path, body) in responses {
            let (mut stream, target) = accept_request(&listener);
            assert!(target.starts_with("GET /api/v1/42/"));
            assert!(target.contains(path));
            assert!(!target.contains("IBAN"));
            send_response(&mut stream, body);
        }
    });
    let (_root, directory, _) = fixture_session(&base);
    let mut process = McpProcess::start(&directory);
    process.initialize();
    let response=process.request(2,"tools/call",json!({"name":"get_payment_details","arguments":{"account_id":ACCOUNT_ID,"transaction_line_id":"77777777-7777-7777-7777-777777777777"}}));
    assert_ne!(response["result"]["isError"], true, "{response}");
    let result = &response["result"]["structuredContent"];
    assert_eq!(result["journal_kind"], "payment_service");
    assert_eq!(
        result["journal"]["payment_service_provider_name"],
        "Provider X"
    );
    assert_eq!(result["default_currency"], "EUR");
    assert_eq!(result["posting"]["currency"], "USD");
    assert_eq!(result["posting"]["posting_amount_default_currency"], "-100");
    assert_eq!(result["offset_resolution"], "not_reported");
    assert!(
        result["interpretation"]
            .as_str()
            .unwrap()
            .contains("do not prove payment execution")
    );
    assert!(!response.to_string().contains("fixture-access-token"));
    assert!(!directory.join("downloads").exists());
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(serde_json::from_str::<Value>(text).unwrap(), *result);
    api.join().unwrap();
    process.shutdown();
}

#[test]
fn stdio_gl_lookup_and_period_report_keep_all_page_totals_and_status_breakdowns() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let gl = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
    let api = thread::spawn(move || {
        let account = json!({"ID":gl,"Division":42,"Code":" 00100 ","Description":"Compute services","BalanceSide":"C","BalanceType":"W"});
        let line = |id: &str, status: i32, amount: Value, currency: &str| json!({"ID":id,"EntryID":"44444444-4444-4444-4444-444444444444","Division":42,"GLAccount":gl,"GLAccountCode":"00100","EntryNumber":12345,"FinancialYear":2025,"FinancialPeriod":13,"JournalCode":"60","LineNumber":1,"Status":status,"Date":"2025-12-31","AmountDC":amount,"AmountFC":100,"Currency":currency});
        let responses = [
            ("/financial/GLAccounts?", json!({"d":[account.clone()]})),
            ("/financial/GLAccounts?", json!({"d":[account]})),
            (
                "/system/Divisions?",
                json!({"d":[{"Code":42,"Currency":"EUR"}]}),
            ),
            (
                "/financialtransaction/TransactionLines?",
                json!({"d":{"results":[line("11111111-aaaa-aaaa-aaaa-aaaaaaaaaaaa",20,json!(0.1),"USD")],"__next":"?page=2"}}),
            ),
            (
                "/financialtransaction/TransactionLines?page=2",
                json!({"d":[line("22222222-aaaa-aaaa-aaaa-aaaaaaaaaaaa",50,json!(0.2),"GBP")]}),
            ),
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
                assert_eq!(
                    parameters["$filter"],
                    format!(
                        "GLAccount eq guid'{gl}' and FinancialYear eq 2025 and FinancialPeriod ge 13 and FinancialPeriod le 13"
                    )
                );
            }
            send_response(&mut stream, body);
        }
    });
    let (_root, directory, _) = fixture_session(&base);
    let mut process = McpProcess::start(&directory);
    process.initialize();
    let found = process.request(
        2,
        "tools/call",
        json!({"name":"search_gl_accounts","arguments":{"query":"00100","match_by":"code"}}),
    );
    assert_ne!(found["result"]["isError"], true, "{found}");
    let selected = &found["result"]["structuredContent"]["accounts"][0];
    assert_eq!(selected["id"], gl);
    let response=process.request(3,"tools/call",json!({"name":"get_ledger_report","arguments":{"gl_account_id":selected["id"],"financial_year":2025,"period_from":13,"period_to":13,"limit":1}}));
    assert_ne!(response["result"]["isError"], true, "{response}");
    let report = &response["result"]["structuredContent"];
    assert_eq!(report["totals"]["line_count"], 2);
    assert_eq!(report["totals"]["net_movement_default_currency"], "0.3");
    assert_eq!(
        report["totals_by_status"]["20"]["net_movement_default_currency"],
        "0.1"
    );
    assert_eq!(
        report["totals_by_status"]["50"]["net_movement_default_currency"],
        "0.2"
    );
    assert_eq!(report["transactions"].as_array().unwrap().len(), 1);
    assert_eq!(report["has_more"], true);
    assert_eq!(report["next_offset"], 1);
    assert!(report.get("closing_balance").is_none());
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(serde_json::from_str::<Value>(text).unwrap(), *report);
    assert!(!response.to_string().contains("fixture-access-token"));
    api.join().unwrap();
    process.shutdown();
}

#[test]
fn stdio_drafts_are_commercial_read_only_and_distinct_from_posted_invoices() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let invoice_id = "88888888-8888-8888-8888-888888888888";
    let api = thread::spawn(move || {
        let account = json!({"ID":ACCOUNT_ID,"Division":42,"Code":"1001","Name":"Company X"});
        let currency = json!({"Code":42,"Currency":"EUR"});
        let invoice = json!({"InvoiceID":invoice_id,"Division":42,"InvoiceTo":ACCOUNT_ID,"OrderedBy":"22222222-2222-2222-2222-222222222222","Status":20,"Currency":"USD","InvoiceDate":"2025-01-01","Description":"Draft services","AmountDC":10.5,"AmountFC":12.1,"AmountFCExclVat":10,"VATAmountFC":2.1});
        let mut processed = invoice.clone();
        processed["InvoiceID"] = json!("99999999-9999-9999-9999-999999999999");
        processed["Status"] = json!(50);
        let line = |id: &str, number| json!({"ID":id,"InvoiceID":invoice_id,"Division":42,"LineNumber":number,"AmountDC":5.25,"AmountFC":5,"VATAmountFC":1.05,"VATPercentage":0.21,"Quantity":1});
        let responses = [
            ("/crm/Accounts?", json!({"d":[account.clone()]})),
            ("/system/Divisions?", json!({"d":[currency.clone()]})),
            (
                "/salesinvoice/SalesInvoices?",
                json!({"d":{"results":[processed],"__next":"?page=2"}}),
            ),
            (
                "/salesinvoice/SalesInvoices?page=2",
                json!({"d":[invoice.clone()]}),
            ),
            ("/crm/Accounts?", json!({"d":[account]})),
            ("/salesinvoice/SalesInvoices?", json!({"d":[invoice]})),
            ("/system/Divisions?", json!({"d":[currency]})),
            (
                "/salesinvoice/SalesInvoiceLines?",
                json!({"d":{"results":[line("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",2)],"__next":"?page=2"}}),
            ),
            (
                "/salesinvoice/SalesInvoiceLines?page=2",
                json!({"d":[line("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",1)]}),
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
    let found = process.request(
        2,
        "tools/call",
        json!({"name":"search_draft_invoices","arguments":{"account_id":ACCOUNT_ID}}),
    );
    assert_ne!(found["result"]["isError"], true, "{found}");
    let search = &found["result"]["structuredContent"];
    assert_eq!(search["source_invoice_count"], 2);
    assert_eq!(search["matched_invoice_count"], 1);
    assert_eq!(search["invoices"][0]["invoice_id"], invoice_id);
    assert_eq!(search["invoices"][0]["status_code"], 20);
    assert_eq!(search["company_role"], "invoice_to_bill_to_not_ordered_by");
    let details = process.request(3,"tools/call",json!({"name":"get_draft_invoice_details","arguments":{"account_id":ACCOUNT_ID,"invoice_id":invoice_id}}));
    assert_ne!(details["result"]["isError"], true, "{details}");
    let result = &details["result"]["structuredContent"];
    assert_eq!(result["default_currency"], "EUR");
    assert_eq!(result["invoice"]["currency"], "USD");
    assert_eq!(result["invoice"]["amount_transaction_currency"], "12.1");
    assert_eq!(result["line_count"], 2);
    assert_eq!(result["lines"][0]["line_number"], 1);
    assert_eq!(result["lines"][0]["vat_percentage"], "0.21");
    assert!(result.get("entry_id").is_none());
    assert!(result.get("paid_amount").is_none());
    let text = details["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(serde_json::from_str::<Value>(text).unwrap(), *result);
    assert!(!details.to_string().contains("fixture-access-token"));
    api.join().unwrap();
    process.shutdown();
}

#[test]
fn stdio_bank_queue_uses_configured_postings_and_open_items_not_open_status() {
    for use_saved in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let unassigned = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
        let bank = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let payable = "cccccccc-cccc-cccc-cccc-cccccccccccc";
        let control = "22222222-2222-2222-2222-222222222222";
        let ap_control = "33333333-3333-3333-3333-333333333333";
        let api = thread::spawn(move || {
            let entry = "77777777-7777-7777-7777-777777777777";
            let unassigned_posting = "44444444-4444-4444-4444-444444444444";
            let ap_posting = "55555555-5555-5555-5555-555555555555";
            let handled_control = "66666666-6666-6666-6666-666666666666";
            let line = |id: &str,
                        gl: &str,
                        offset: Option<&str>,
                        account: Option<&str>,
                        date: &str,
                        amount: Value| {
                json!({"ID":id,"EntryID":entry,"Division":42,"EntryNumber":123,"FinancialYear":2025,"FinancialPeriod":1,"LineNumber":1,"LineType":0,"JournalCode":"21","GLAccount":gl,"Account":account,"OffsetID":offset,"Type":40,"Status":20,"Date":date,"AmountDC":amount,"AmountFC":amount,"Currency":"EUR","Description":"Invented bank posting"})
            };
            let first = line(control, bank, None, None, "2025-01-02", json!(-4.25));
            let mut second = line(
                ap_control,
                bank,
                None,
                Some(ACCOUNT_ID),
                "2025-01-03",
                json!(-7.5),
            );
            second["Currency"] = json!("USD");
            second["AmountFC"] = json!(-10);
            let unallocated = line(
                unassigned_posting,
                unassigned,
                Some(control),
                None,
                "2025-01-02",
                json!(4.25),
            );
            let ap = line(
                ap_posting,
                payable,
                Some(ap_control),
                Some(ACCOUNT_ID),
                "2025-01-03",
                json!(7.5),
            );
            let handled = line(handled_control, bank, None, None, "2025-01-04", json!(1));
            let handled_booking = line(
                "88888888-8888-8888-8888-888888888888",
                "dddddddd-dddd-dddd-dddd-dddddddddddd",
                Some(handled_control),
                None,
                "2025-01-04",
                json!(-1),
            );
            let responses = [
                (
                    "/system/Divisions?",
                    json!({"d":[{"Code":42,"Currency":"EUR"}]}),
                ),
                (
                    "/financial/Journals?",
                    json!({"d":[{"ID":"99999999-9999-9999-9999-999999999999","Division":42,"Code":"21","Type":12,"GLAccount":bank,"Description":"Fixture bank"}]}),
                ),
                (
                    "/financial/GLAccounts?",
                    json!({"d":[{"ID":unassigned,"Division":42,"Code":"1999","Description":"Confirmed unallocated bank","Type":90},{"ID":payable,"Division":42,"Code":"2000","Description":"Payables","Type":22}]}),
                ),
                (
                    "/financialtransaction/TransactionLines?",
                    json!({"d":[unallocated.clone()]}),
                ),
                (
                    "/read/financial/PayablesList?",
                    json!({"d":[{"HID":1,"AccountId":ACCOUNT_ID,"EntryNumber":123,"JournalCode":"21","Amount":-2.5,"CurrencyCode":"EUR"}]}),
                ),
                (
                    "/financialtransaction/TransactionLines?",
                    json!({"d":[ap.clone(),second.clone()]}),
                ),
                ("/read/financial/ReceivablesList?", json!({"d":[]})),
                (
                    "/financialtransaction/TransactionLines?",
                    json!({"d":[first.clone(),second.clone()]}),
                ),
                (
                    "/financialtransaction/TransactionLines?",
                    json!({"d":{"results":[first,unallocated,second],"__next":"?page=2"}}),
                ),
                (
                    "/financialtransaction/TransactionLines?page=2",
                    json!({"d":[ap,handled,handled_booking]}),
                ),
            ];
            for (index, (path, body)) in responses.into_iter().enumerate() {
                let (mut stream, target) = accept_request(&listener);
                assert!(target.starts_with("GET /api/v1/42/"), "{target}");
                assert!(target.contains(path), "{target}");
                let url = reqwest::Url::parse(&format!(
                    "http://localhost{}",
                    target.split_whitespace().nth(1).unwrap()
                ))
                .unwrap();
                let parameters: std::collections::HashMap<_, _> =
                    url.query_pairs().into_owned().collect();
                if let Some(filter) = parameters.get("$filter") {
                    assert!(
                        !filter.contains("Status"),
                        "Queue must not use accounting status: {filter}"
                    );
                    if index == 3 {
                        assert!(filter.contains(unassigned));
                        assert!(!filter.contains("OffsetID ne null"));
                    }
                    if index == 5 {
                        assert!(filter.contains(ACCOUNT_ID));
                        assert!(filter.contains("EntryNumber eq 123"));
                        assert!(!filter.contains("OffsetID ne null"));
                    }
                }
                if let Some(fields) = parameters.get("$select") {
                    assert!(!fields.contains("BankAccountNumber"));
                }
                send_response(&mut stream, body);
            }
        });
        let (_root, directory, state) = fixture_session(&base);
        let config_path = directory.join("bank-config.json");
        let stored_id = if use_saved {
            unassigned
        } else {
            "eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee"
        };
        std::fs::write(&config_path, serde_json::to_vec(&json!([{
        "scope":{"base_url":state["base_url"],"client_id":state["client_id"],"administration":42},
        "configuration":{"unassigned_gl_account_ids":[stored_id]}
    }])).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let saved_bytes = std::fs::read(&config_path).unwrap();
        let mut process = McpProcess::start(&directory);
        process.initialize();
        let arguments = if use_saved {
            json!({"limit":1})
        } else {
            json!({"unassigned_gl_account_ids":[unassigned],"limit":1})
        };
        let response = process.request(
            2,
            "tools/call",
            json!({"name":"get_bank_transactions_to_handle","arguments":arguments}),
        );
        assert_ne!(response["result"]["isError"], true, "{response}");
        let result = &response["result"]["structuredContent"];
        assert_eq!(
            result["configuration_source"],
            if use_saved {
                "saved_administration"
            } else {
                "arguments"
            }
        );
        assert_eq!(std::fs::read(&config_path).unwrap(), saved_bytes);
        assert_eq!(result["reconstructed_transaction_count"], 2);
        assert_eq!(result["matched_transaction_count"], 2);
        assert_eq!(result["source_bank_outstanding_item_count"], 1);
        assert_eq!(result["coverage_complete"], true);
        assert_eq!(result["unresolved_items"], json!([]));
        assert_eq!(result["default_currency"], "EUR");
        assert_eq!(result["transactions"].as_array().unwrap().len(), 1);
        let transaction = &result["transactions"][0];
        assert_eq!(transaction["id"], ap_control);
        assert_eq!(
            transaction["reasons"],
            json!(["outstanding_payable_posting"])
        );
        assert_eq!(transaction["amount_default_currency"], "-7.5");
        assert_eq!(transaction["amount_transaction_currency"], "-10");
        assert_eq!(transaction["currency"], "USD");
        assert_eq!(
            transaction["evidence"][0]["outstanding_item"]["amount"],
            "-2.5"
        );
        assert_eq!(transaction["booking_lines"].as_array().unwrap().len(), 1);
        assert_eq!(result["has_more"], true);
        assert_eq!(result["next_offset"], 1);
        assert!(!response.to_string().contains("fixture-access-token"));
        api.join().unwrap();
        if use_saved {
            std::fs::write(&config_path, "[]\n").unwrap();
            let missing = process.request(
                3,
                "tools/call",
                json!({"name":"get_bank_transactions_to_handle","arguments":{}}),
            );
            assert_eq!(missing["result"]["isError"], true);
            assert!(
                missing["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("bank-config set")
            );
        }
        process.shutdown();
    }
}

#[test]
fn stdio_booking_master_data_and_journal_locks_are_read_only() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let api = thread::spawn(move || {
        let record = json!({"ID":ACCOUNT_ID,"Division":42,"Code":" 001 ","Description":"Invented master data"});
        let journal = json!({"ID":ACCOUNT_ID,"Division":42,"Code":" P1 ","Description":"Invented purchase journal","Type":22,"IsBlocked":true});
        let period = json!({"ID":ACCOUNT_ID,"Division":42,"FinYear":2025,"FinPeriod":13,"StartDate":"2025-12-31","EndDate":"2025-12-31"});
        let replies = [
            ("/vat/VATCodes?", json!({"d":[record.clone()]})),
            (
                "/vat/VatPercentages?",
                json!({"d":[{"ID":"22222222-2222-2222-2222-222222222222","Division":42,"VATCodeID":ACCOUNT_ID,"Percentage":0.21,"Type":0}]}),
            ),
            ("/financial/Journals?", json!({"d":[journal.clone()]})),
            (
                "/cashflow/PaymentConditions?",
                json!({"d":[record.clone()]}),
            ),
            ("/hrm/Costcenters?", json!({"d":[record.clone()]})),
            ("/hrm/Costunits?", json!({"d":[record.clone()]})),
            ("/project/Projects?", json!({"d":[record]})),
            (
                "/financial/FinancialPeriods?",
                json!({"d":[period.clone()]}),
            ),
            ("/financial/Journals?", json!({"d":[journal]})),
            ("/financial/FinancialPeriods?", json!({"d":[period]})),
            (
                "/read/financial/JournalStatusList?",
                json!({"d":[{"Journal":"P1","Year":2025,"Period":13,"Status":0}]}),
            ),
        ];
        for (path, body) in replies {
            let (mut stream, target) = accept_request(&listener);
            assert!(target.starts_with("GET /api/v1/42/"), "{target}");
            assert!(target.contains(path), "{target}");
            assert!(!target.contains("IBAN") && !target.contains("BankAccount"));
            send_response(&mut stream, body);
        }
    });
    let (_root, directory, _) = fixture_session(&base);
    let mut process = McpProcess::start(&directory);
    process.initialize();
    for (index, name) in [
        "search_vat_codes",
        "search_journals",
        "search_payment_conditions",
        "search_cost_centers",
        "search_cost_units",
        "search_projects",
    ]
    .into_iter()
    .enumerate()
    {
        let response = process.request(
            2 + index as u32,
            "tools/call",
            json!({"name":name,"arguments":{"limit":1}}),
        );
        assert_ne!(response["result"]["isError"], true, "{response}");
        let result = &response["result"]["structuredContent"];
        assert_eq!(result["administration"]["Code"], 42);
        assert_eq!(result["matched_record_count"], 1);
        assert_eq!(result["records"].as_array().unwrap().len(), 1);
        if name == "search_vat_codes" {
            assert_eq!(result["records"][0]["percentages"][0]["percentage"], "0.21");
        }
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert_eq!(serde_json::from_str::<Value>(text).unwrap(), *result);
        assert!(!response.to_string().contains("fixture-access-token"));
    }
    let response = process.request(
        8,
        "tools/call",
        json!({"name":"get_financial_periods","arguments":{"financial_year":2025}}),
    );
    assert_ne!(response["result"]["isError"], true, "{response}");
    assert_eq!(
        response["result"]["structuredContent"]["periods"][0]["financial_period"],
        13
    );
    let response=process.request(9,"tools/call",json!({"name":"get_journal_period_status","arguments":{"journal_code":"P1","financial_year":2025}}));
    assert_ne!(response["result"]["isError"], true, "{response}");
    let result = &response["result"]["structuredContent"];
    assert_eq!(result["journal"]["is_blocked"], true);
    assert_eq!(result["periods"][0]["journal_period_state"], "open");
    assert_eq!(result["periods"][0]["status_code"], 0);
    assert!(result.get("booking_allowed").is_none());
    let response = process.request(
        10,
        "tools/call",
        json!({"name":"search_vat_codes","arguments":{"limit":0}}),
    );
    assert_eq!(response["result"]["isError"], true);
    api.join().unwrap();
    process.shutdown();
    assert!(!directory.join("downloads").exists());
}

#[test]
fn stdio_article_and_account_context_preserve_source_defaults_without_booking() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let item_id = "22222222-2222-2222-2222-222222222222";
    let group_id = "33333333-3333-3333-3333-333333333333";
    let unit_id = "44444444-4444-4444-4444-444444444444";
    let api = thread::spawn(move || {
        let item = json!({"ID":item_id,"Division":42,"Code":" 001 ","Description":"Invented article","ItemGroup":group_id,"ItemGroupCode":"G1","GLCosts":ACCOUNT_ID,"StandardSalesPrice":0.1,"CostPriceStandard":0.2,"CostPriceCurrency":"USD","Unit":"pc","IsPurchaseItem":true});
        let account = json!({"ID":ACCOUNT_ID,"Division":42,"Code":" 0001 ","Name":"Invented supplier","Blocked":true,"IsSupplier":true,"GLAccountPurchase":unit_id,"GLAP":item_id,"PurchaseVATCode":"P1","DiscountPurchase":0.055});
        let responses = [
            ("/logistics/Items?", json!({"d":[item.clone()]})),
            ("/logistics/Items?", json!({"d":[item.clone()]})),
            (
                "/logistics/ItemGroups?",
                json!({"d":[{"ID":group_id,"Division":42,"Code":"G1","GLPurchaseAccount":unit_id}]}),
            ),
            (
                "/logistics/Units?",
                json!({"d":[{"ID":unit_id,"Division":42,"Code":"pc","Description":"Piece","Active":false,"Main":1,"Type":"O"}]}),
            ),
            ("/crm/Accounts?", json!({"d":[account.clone()]})),
            ("/crm/Accounts?", json!({"d":[account]})),
            ("/logistics/Items?", json!({"d":[item]})),
            (
                "/logistics/SupplierItem?",
                json!({"d":[{"ID":group_id,"Division":42,"Supplier":ACCOUNT_ID,"Item":item_id,"ItemCode":"001","SupplierItemCode":" 000REF ","PurchasePrice":0,"Currency":"CHF","PurchaseUnit":"box","PurchaseUnitFactor":12,"ItemUnit":unit_id,"PurchaseVATCode":"P1","StartDate":"2099-01-01"}]}),
            ),
        ];
        for (path, body) in responses {
            let (mut stream, target) = accept_request(&listener);
            assert!(target.starts_with("GET /api/v1/42/"));
            assert!(target.contains(path), "{target}");
            for forbidden in ["BankAccounts", "BSN", "AddressLine1", "Email", "Phone"] {
                assert!(!target.contains(forbidden));
            }
            send_response(&mut stream, body);
        }
    });
    let (_root, directory, _) = fixture_session(&base);
    let mut process = McpProcess::start(&directory);
    process.initialize();
    let calls = [
        ("search_items", json!({"query":"001","match_by":"code"})),
        ("get_item_details", json!({"item_id":item_id})),
        ("search_units", json!({})),
        (
            "get_account_booking_context",
            json!({"account_id":ACCOUNT_ID}),
        ),
        (
            "search_supplier_items",
            json!({"account_id":ACCOUNT_ID,"item_id":item_id,"supplier_item_code":"000REF"}),
        ),
    ];
    for (index, (name, args)) in calls.into_iter().enumerate() {
        let response = process.request(
            index as u32 + 2,
            "tools/call",
            json!({"name":name,"arguments":args}),
        );
        assert_ne!(response["result"]["isError"], true, "{response}");
        let result = &response["result"]["structuredContent"];
        assert_eq!(result["administration"]["Code"], 42);
        match name {
            "search_items" => assert_eq!(result["records"][0]["standard_sales_price"], "0.1"),
            "get_item_details" => {
                assert_eq!(result["item"]["gl_costs_id"], ACCOUNT_ID);
                assert_eq!(result["item_group"]["gl_purchase_account_id"], unit_id);
                assert!(result["standard_sales_price_currency"].is_null());
            }
            "search_units" => assert_eq!(result["records"][0]["active"], false),
            "get_account_booking_context" => {
                assert_eq!(result["account"]["is_blocked"], true);
                assert_eq!(result["account"]["discount_purchase"], "0.055");
                assert_eq!(result["account"]["gl_account_purchase_id"], unit_id);
                assert_eq!(result["account"]["gl_accounts_payable_id"], item_id);
            }
            _ => {
                assert_eq!(result["links"][0]["purchase_price"], "0");
                assert_eq!(result["links"][0]["currency"], "CHF");
                assert_eq!(result["links"][0]["purchase_unit_factor"], "12");
                assert_eq!(result["links"][0]["start_date"], "2099-01-01");
                assert_eq!(result["matched_link_count"], 1);
            }
        }
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert_eq!(serde_json::from_str::<Value>(text).unwrap(), *result);
        assert!(!response.to_string().contains("fixture-access-token"));
        assert!(result.get("booking_allowed").is_none());
    }
    let invalid=process.request(10,"tools/call",json!({"name":"get_account_booking_context","arguments":{"account_id":"00000000-0000-0000-0000-000000000000"}}));
    assert_eq!(invalid["result"]["isError"], true);
    api.join().unwrap();
    process.shutdown();
    assert!(!directory.join("downloads").exists());
}

#[test]
fn stdio_purchase_intake_discovers_documents_before_entries_and_downloads_only_selected_pdf() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let document_id = "22222222-2222-2222-2222-222222222222";
    let attachment_id = "33333333-3333-3333-3333-333333333333";
    let pdf = "%PDF-1.7\nInvented purchase intake PDF\n%%EOF\n";
    let api = thread::spawn(move || {
        let document = json!({"ID":document_id,"Division":42,"Type":20,"Subject":"Invented incoming purchase","Account":null,"DocumentDate":"2025-01-01","ProposedEntryStatus":20,"FinancialTransactionEntryID":null,"AmountFC":123.45,"Currency":"USD","Modified":"2025-01-01T10:01:02"});
        let attachment = json!({"ID":attachment_id,"Document":document_id,"FileName":"incoming.pdf","FileSize":pdf.len(),"Url":"https://untrusted.invalid/never-follow"});
        let responses = [
            (
                "/api/v1/42/documents/Documents?",
                json!({"d":[document.clone()]}).to_string(),
            ),
            (
                "/api/v1/42/documents/Documents?",
                json!({"d":[document.clone()]}).to_string(),
            ),
            (
                "/api/v1/42/documents/DocumentAttachments?",
                json!({"d":[attachment.clone()]}).to_string(),
            ),
            (
                "/api/v1/42/purchaseentry/PurchaseEntries?",
                json!({"d":[]}).to_string(),
            ),
            (
                "/api/v1/42/documents/Documents?",
                json!({"d":[document.clone()]}).to_string(),
            ),
            (
                "/api/v1/42/documents/DocumentAttachments?",
                json!({"d":[attachment.clone()]}).to_string(),
            ),
            ("/docs/SysAttachment.aspx?", pdf.into()),
            (
                "/api/v1/42/documents/DocumentAttachments?",
                json!({"d":[attachment]}).to_string(),
            ),
            (
                "/api/v1/42/documents/Documents?",
                json!({"d":[document]}).to_string(),
            ),
        ];
        for (path, body) in responses {
            let (mut stream, target) = accept_request(&listener);
            assert!(target.starts_with(&format!("GET {path}")), "{target}");
            assert!(
                !target.contains("ActionRegenerateEntryProposal") && !target.contains("MarkAsPaid")
            );
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
    });
    let (_root, directory, _) = fixture_session(&base);
    let mut process = McpProcess::start(&directory);
    process.initialize();
    let response = process.request(
        2,
        "tools/call",
        json!({"name":"search_purchase_documents","arguments":{"link_filter":"without_reference"}}),
    );
    assert_ne!(response["result"]["isError"], true, "{response}");
    let result = &response["result"]["structuredContent"];
    assert_eq!(result["matched_document_count"], 1);
    assert_eq!(result["documents"][0]["id"], document_id);
    assert_eq!(
        result["documents"][0]["amount_transaction_currency"],
        "123.45"
    );
    assert!(
        result["coverage"]
            .as_str()
            .unwrap()
            .contains("not_complete_inbox")
    );
    assert!(result.get("is_unbooked").is_none());
    let response = process.request(
        3,
        "tools/call",
        json!({"name":"get_purchase_document","arguments":{"document_id":document_id}}),
    );
    assert_ne!(response["result"]["isError"], true, "{response}");
    let result = &response["result"]["structuredContent"];
    assert_eq!(result["linked_purchase_entries"], json!([]));
    assert_eq!(result["attachments"][0]["id"], attachment_id);
    assert_eq!(result["attachments"][0]["pdf_filename"], true);
    let response=process.request(4,"tools/call",json!({"name":"download_purchase_document_pdf","arguments":{"document_id":document_id,"attachment_id":attachment_id}}));
    assert_ne!(response["result"]["isError"], true, "{response}");
    let result = &response["result"]["structuredContent"];
    let path = PathBuf::from(result["saved_path"].as_str().unwrap());
    assert_eq!(std::fs::read(&path).unwrap(), pdf.as_bytes());
    assert!(path.starts_with(directory.canonicalize().unwrap().join("downloads/42")));
    assert!(
        !response
            .to_string()
            .contains("Invented purchase intake PDF")
    );
    assert!(!response.to_string().contains("untrusted.invalid"));
    assert!(!response.to_string().contains("fixture-access-token"));
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(serde_json::from_str::<Value>(text).unwrap(), *result);
    api.join().unwrap();
    process.shutdown();
}

#[test]
fn stdio_purchase_duplicate_assessment_returns_evidence_not_booking_clearance() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let entry_id = "22222222-2222-2222-2222-222222222222";
    let document_id = "33333333-3333-3333-3333-333333333333";
    let api = thread::spawn(move || {
        let account =
            json!({"ID":ACCOUNT_ID,"Division":42,"Code":" 001 ","Name":"Invented supplier"});
        let mut entry = json!({"EntryID":entry_id,"Division":42,"Supplier":ACCOUNT_ID,"EntryNumber":12345,"Journal":"P1","EntryDate":"2025-01-01","YourRef":" 000REF ","Currency":"EUR","Status":50,"Type":30,"Document":document_id});
        entry["AmountFC"] = serde_json::from_str("-121.00").unwrap();
        let document = json!({"ID":document_id,"Division":42,"Type":20,"Account":ACCOUNT_ID,"Subject":"Source document","AmountFC":121.00,"Currency":"EUR","FinancialTransactionEntryID":entry_id,"ProposedEntryStatus":50});
        let responses = [
            ("/crm/Accounts?", json!({"d":[account.clone()]})),
            ("/purchaseentry/PurchaseEntries?", json!({"d":[entry]})),
            ("/documents/Documents?", json!({"d":[document]})),
            ("/crm/Accounts?", json!({"d":[account]})),
            ("/purchaseentry/PurchaseEntries?", json!({"d":[]})),
            ("/documents/Documents?", json!({"d":[]})),
        ];
        for (path, body) in responses {
            let (mut stream, target) = accept_request(&listener);
            assert!(target.starts_with("GET /api/v1/42/"));
            assert!(target.contains(path), "{target}");
            assert!(!target.contains("YourRef") || target.contains("%24select="));
            assert!(!target.contains("000REF"));
            assert!(
                !target.contains("SupplierItem")
                    && !target.contains("/project/")
                    && !target.contains("/purchase/PurchaseInvoices")
            );
            send_response(&mut stream, body);
        }
    });
    let (_root, directory, _) = fixture_session(&base);
    let mut process = McpProcess::start(&directory);
    process.initialize();
    let response=process.request(2,"tools/call",json!({"name":"assess_purchase_duplicates","arguments":{"account_id":ACCOUNT_ID,"supplier_invoice_reference":"000REF","invoice_date":"2025-01-01","currency":"EUR","amount_transaction_currency":"121.00"}}));
    assert_ne!(response["result"]["isError"], true, "{response}");
    let result = &response["result"]["structuredContent"];
    assert_eq!(result["assessment"], "candidates_found_review_required");
    assert_eq!(result["can_rule_out_duplicates"], false);
    assert_eq!(result["matched_candidate_count"], 2);
    assert_eq!(result["coverage"]["all_selected_source_pages_read"], true);
    let entry = result["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|candidate| candidate["source"] == "accounting_entry")
        .unwrap();
    assert_eq!(entry["entry"]["amount_transaction_currency"], "-121.00");
    assert_eq!(
        entry["evidence"]["amount_comparison"],
        "same_magnitude_different_sign"
    );
    assert_eq!(
        entry["evidence"]["reference_comparison"],
        "exact_after_outer_trim"
    );
    let document = result["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|candidate| candidate["source"] == "purchase_document")
        .unwrap();
    assert_eq!(
        document["evidence"]["linked_accounting_entry_ids"],
        json!([entry_id])
    );
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(serde_json::from_str::<Value>(text).unwrap(), *result);
    assert!(!response.to_string().contains("fixture-access-token"));
    let response=process.request(3,"tools/call",json!({"name":"assess_purchase_duplicates","arguments":{"account_id":ACCOUNT_ID,"supplier_invoice_reference":"000REF"}}));
    assert_ne!(response["result"]["isError"], true, "{response}");
    assert_eq!(
        response["result"]["structuredContent"]["assessment"],
        "inconclusive_no_candidates_in_checked_sources"
    );
    assert_eq!(
        response["result"]["structuredContent"]["can_rule_out_duplicates"],
        false
    );
    let response=process.request(4,"tools/call",json!({"name":"assess_purchase_duplicates","arguments":{"account_id":ACCOUNT_ID,"supplier_invoice_reference":"000REF","amount_transaction_currency":"121.00"}}));
    assert_eq!(response["result"]["isError"], true);
    api.join().unwrap();
    process.shutdown();
    assert!(!directory.join("downloads").exists());
}

#[test]
fn stdio_purchase_preflight_reports_evidence_unknowns_and_blockers_without_booking() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let gl = "22222222-2222-2222-2222-222222222222";
    let vat = "33333333-3333-3333-3333-333333333333";
    let rate = "44444444-4444-4444-4444-444444444444";
    let api = thread::spawn(move || {
        for _ in 0..2 {
            let responses = [
                (
                    "/crm/Accounts?",
                    json!({"d":[{"ID":ACCOUNT_ID,"Division":42,"Code":" 001 ","IsSupplier":true,"Blocked":false,"StartDate":"2000-01-01","EndDate":"2100-12-31"}]}),
                ),
                ("/purchaseentry/PurchaseEntries?", json!({"d":[]})),
                ("/documents/Documents?", json!({"d":[]})),
                (
                    "/financial/Journals?",
                    json!({"d":[{"ID":ACCOUNT_ID,"Division":42,"Code":"P1","Type":22,"IsBlocked":false,"Currency":"EUR","AllowVariableCurrency":false}]}),
                ),
                (
                    "/financial/FinancialPeriods?",
                    json!({"d":[{"ID":ACCOUNT_ID,"Division":42,"FinYear":2025,"FinPeriod":1,"StartDate":"2025-01-01","EndDate":"2025-01-31"}]}),
                ),
                (
                    "/read/financial/JournalStatusList?",
                    json!({"d":[{"Journal":"P1","Year":2025,"Period":1,"Status":0}]}),
                ),
                (
                    "/system/Divisions?",
                    json!({"d":[{"Code":42,"Currency":"EUR"}]}),
                ),
                (
                    "/financial/GLAccounts?",
                    json!({"d":[{"ID":gl,"Division":42,"Code":"4000","Type":120,"IsBlocked":false,"UseCostcenter":0,"UseCostunit":0}]}),
                ),
                (
                    "/vat/VATCodes?",
                    json!({"d":[{"ID":vat,"Division":42,"Code":"V1","Type":"E","IsBlocked":false,"VATTransactionType":"P","Charged":false,"CalculationBasis":0}]}),
                ),
                (
                    "/vat/VatPercentages?",
                    json!({"d":[{"ID":rate,"Division":42,"VATCodeID":vat,"StartDate":"2000-01-01","EndDate":"2100-12-31","Type":0,"Percentage":0.21}]}),
                ),
                (
                    "/general/Currencies?",
                    json!({"d":[{"Code":"EUR","Description":"Euro"}]}),
                ),
            ];
            for (path, body) in responses {
                let (mut stream, target) = accept_request(&listener);
                assert!(
                    target.starts_with("GET /api/v1/42/") && target.contains(path),
                    "{target}"
                );
                for forbidden in [
                    "/project/",
                    "/logistics/",
                    "/purchase/PurchaseInvoices",
                    "SysAttachment",
                ] {
                    assert!(!target.contains(forbidden));
                }
                send_response(&mut stream, body);
            }
        }
    });
    let (_root, directory, _) = fixture_session(&base);
    let mut process = McpProcess::start(&directory);
    process.initialize();
    let mut args = json!({"account_id":ACCOUNT_ID,"supplier_invoice_reference":"NEW-0001","document_kind":"purchase_invoice","invoice_date":"2025-01-01","financial_year":2025,"financial_period":1,"journal_code":"P1","currency":"EUR","gross_amount_transaction_currency":"121.00","lines":[{"gl_account_id":gl,"net_amount_transaction_currency":"100.00","supplier_vat_amount_transaction_currency":"21.00","vat_code":"V1","vat_percentage":"0.21"}]});
    let response = process.request(
        2,
        "tools/call",
        json!({"name":"preflight_purchase_booking","arguments":args.clone()}),
    );
    assert_ne!(response["result"]["isError"], true, "{response}");
    let result = &response["result"]["structuredContent"];
    assert_eq!(result["status"], "review_required");
    assert_eq!(result["authorizes_booking"], false);
    assert_eq!(result["blockers"], json!([]));
    assert!(!result["unknowns"].as_array().unwrap().is_empty());
    assert_eq!(result["amount_reconciliation"]["exact_totals_match"], true);
    assert_eq!(
        result["line_evidence"][0]["declared_rate_unrounded_supplier_vat"],
        "21"
    );
    assert_eq!(
        result["duplicate_assessment"]["can_rule_out_duplicates"],
        false
    );
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(serde_json::from_str::<Value>(text).unwrap(), *result);
    assert!(!response.to_string().contains("fixture-access-token"));
    assert!(result.get("write_payload").is_none());
    args["gross_amount_transaction_currency"] = json!("120.99");
    let response = process.request(
        3,
        "tools/call",
        json!({"name":"preflight_purchase_booking","arguments":args}),
    );
    assert_ne!(response["result"]["isError"], true, "{response}");
    assert_eq!(response["result"]["structuredContent"]["status"], "blocked");
    assert_eq!(
        response["result"]["structuredContent"]["authorizes_booking"],
        false
    );
    assert_eq!(
        response["result"]["structuredContent"]["proposal"]["gross_amount_transaction_currency"],
        "120.99"
    );
    api.join().unwrap();
    process.shutdown();
    assert!(!directory.join("downloads").exists());
}

#[test]
fn stdio_bank_handling_proposal_checks_explicit_invoice_selection_without_financial_actions() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let id = |n| uuid::Uuid::from_u128(n).to_string();
    let control = id(101);
    let pending = id(201);
    let unassigned = id(10);
    let entry = id(401);
    let api_control = control.clone();
    let api_pending = pending.clone();
    let api_unassigned = unassigned.clone();
    let api_entry = entry.clone();
    let api = thread::spawn(move || {
        let line = |uuid: &str, gl: u128, offset: Option<&str>, amount: i32| json!({"ID":uuid,"EntryID":id(1001),"Division":42,"Account":null,"EntryNumber":1001,"FinancialYear":2025,"FinancialPeriod":1,"JournalCode":"B1","GLAccount":id(gl),"LineNumber":1,"Type":40,"Status":50,"AmountDC":amount,"AmountFC":amount,"Currency":"EUR","Description":"Pay INV-001","OffsetID":offset});
        let bank = line(&api_control, 11, None, -121);
        let posting = line(&api_pending, 10, Some(&api_control), 121);
        for _ in 0..2 {
            let responses = [
                (
                    "/system/Divisions?",
                    json!({"d":[{"Code":42,"Currency":"EUR"}]}),
                ),
                (
                    "/financial/Journals?",
                    json!({"d":[{"ID":id(1),"Division":42,"Code":"B1","Type":12,"GLAccount":id(11)}]}),
                ),
                (
                    "/financial/GLAccounts?",
                    json!({"d":[{"ID":api_unassigned,"Division":42,"Code":"UNASSIGNED","Type":90}]}),
                ),
                (
                    "/financialtransaction/TransactionLines?",
                    json!({"d":[posting]}),
                ),
                (
                    "/read/financial/PayablesList?",
                    json!({"d":[{"HID":1,"AccountId":ACCOUNT_ID,"EntryNumber":3001,"JournalCode":"P1","Amount":121,"AmountInTransit":0,"CurrencyCode":"EUR"}]}),
                ),
                ("/read/financial/ReceivablesList?", json!({"d":[]})),
                (
                    "/financialtransaction/TransactionLines?",
                    json!({"d":[bank]}),
                ),
                (
                    "/financialtransaction/TransactionLines?",
                    json!({"d":[bank,posting]}),
                ),
                (
                    "/crm/Accounts?",
                    json!({"d":[{"ID":ACCOUNT_ID,"Division":42,"Code":"SUP","Name":"Invented supplier"}]}),
                ),
                (
                    "/purchaseentry/PurchaseEntries?",
                    json!({"d":[{"EntryID":api_entry,"Division":42,"Supplier":ACCOUNT_ID,"EntryNumber":3001,"Journal":"P1","Currency":"EUR","YourRef":"INV-001"}]}),
                ),
            ];
            for (resource, body) in responses {
                let (mut stream, target) = accept_request(&listener);
                assert!(
                    target.starts_with("GET /api/v1/42/") && target.contains(resource),
                    "{target}"
                );
                send_response(&mut stream, body);
            }
        }
    });
    let (_root, directory, _) = fixture_session(&base);
    let mut process = McpProcess::start(&directory);
    process.initialize();
    let mut args = json!({"bank_transaction_line_id":control,"pending_posting_line_id":pending,"unassigned_gl_account_ids":[unassigned],"account_ids":[ACCOUNT_ID]});
    let first = process.request(
        2,
        "tools/call",
        json!({"name":"propose_bank_transaction_handling","arguments":args.clone()}),
    );
    assert_ne!(first["result"]["isError"], true, "{first}");
    let data = &first["result"]["structuredContent"];
    assert_eq!(data["candidate_group_count"], 1);
    assert_eq!(data["authorizes_handling"], false);
    assert_eq!(data["allocation_evidence"], json!([]));
    assert_eq!(data["budget"]["residual"], "121");
    args["invoice_allocations"] = json!([{"account_id":ACCOUNT_ID,"side":"payable","entry_id":entry,"open_item_id":"1","amount":"121.00"}]);
    let second = process.request(
        3,
        "tools/call",
        json!({"name":"propose_bank_transaction_handling","arguments":args}),
    );
    assert_ne!(second["result"]["isError"], true, "{second}");
    let data = &second["result"]["structuredContent"];
    assert_eq!(data["status"], "review_required");
    assert_eq!(data["budget"]["exact_totals_match"], true);
    assert_eq!(data["authorizes_handling"], false);
    assert!(!data["unknowns"].as_array().unwrap().is_empty());
    assert!(data.get("write_payload").is_none());
    assert!(!second.to_string().contains("fixture-access-token"));
    assert_eq!(
        serde_json::from_str::<Value>(second["result"]["content"][0]["text"].as_str().unwrap())
            .unwrap(),
        *data
    );
    api.join().unwrap();
    process.shutdown();
    assert!(!directory.join("downloads").exists());
}

#[test]
fn stdio_xml_bank_preview_returns_a_blocked_specimen_and_exposes_no_upload_action() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let id = |n| uuid::Uuid::from_u128(n).to_string();
    let api = thread::spawn(move || {
        let post = |value: u128, gl: u128, code: &str, offset: Option<u128>, amount: &str| json!({"ID":id(value),"EntryID":id(1001),"Division":42,"Account":null,"EntryNumber":1001,"FinancialYear":2025,"FinancialPeriod":1,"JournalCode":"B1","GLAccount":id(gl),"GLAccountCode":code,"LineNumber":2,"Type":40,"Status":20,"Date":"2025-01-01","AmountDC":serde_json::from_str::<Value>(amount).unwrap(),"AmountFC":serde_json::from_str::<Value>(amount).unwrap(),"Currency":"EUR","OffsetID":offset.map(id)});
        let control = post(101, 11, "BANK", None, "-11.3");
        let pending = post(201, 10, "U1", Some(101), "11.3");
        let header = json!({"d":[{"EntryID":id(1001),"Division":42,"EntryNumber":1001,"JournalCode":"B1","FinancialYear":2025,"FinancialPeriod":1,"Currency":"EUR","OpeningBalanceFC":100,"ClosingBalanceFC":88.7,"Status":20}]});
        let native = json!({"d":[{"ID":id(201),"EntryID":id(1001),"Division":42,"EntryNumber":1001,"LineNumber":2,"Date":"2025-01-01","GLAccount":id(10),"GLAccountCode":"U1","OffsetID":id(101),"AmountDC":-11.3,"AmountFC":-11.3,"Description":"Travel & review","Document":id(700)}]});
        let mut responses=vec![
            ("/system/Divisions?",json!({"d":[{"Code":42,"Currency":"EUR"}]})),
            ("/financial/Journals?",json!({"d":[{"ID":id(1),"Division":42,"Code":"B1","Type":12,"GLAccount":id(11)}]})),
            ("/financial/GLAccounts?",json!({"d":[{"ID":id(10),"Division":42,"Code":"U1","Type":90}]})),
            ("/financialtransaction/TransactionLines?",json!({"d":[pending]})),
            ("/read/financial/PayablesList?",json!({"d":[]})),("/read/financial/ReceivablesList?",json!({"d":[]})),
            ("/financialtransaction/TransactionLines?",json!({"d":[control]})),("/financialtransaction/TransactionLines?",json!({"d":[control,pending]})),
            ("/financialtransaction/BankEntries?",header.clone()),("/financialtransaction/BankEntryLines?",native.clone()),
            ("/financial/GLAccounts?",json!({"d":[{"ID":id(10),"Division":42,"Code":"U1","Type":90},{"ID":id(11),"Division":42,"Code":"BANK","Type":12},{"ID":id(200),"Division":42,"Code":"COST","Type":120,"IsBlocked":false,"UseCostcenter":0,"UseCostunit":0}]})),
            ("/vat/VATCodes?",json!({"d":[{"ID":id(500),"Division":42,"Code":"N0","Type":"N","IsBlocked":false,"VATTransactionType":"P"}]})),("/vat/VatPercentages?",json!({"d":[]})),
            ("/financial/Journals?",json!({"d":[{"ID":id(1),"Division":42,"Code":"B1","Type":12,"IsBlocked":false}]})),
            ("/financial/FinancialPeriods?",json!({"d":[{"ID":id(600),"Division":42,"FinYear":2025,"FinPeriod":1,"StartDate":"2025-01-01","EndDate":"2025-01-31"}]})),
            ("/read/financial/JournalStatusList?",json!({"d":[{"Journal":"B1","Year":2025,"Period":1,"Status":0}]})),
        ].into_iter().map(|(path,body)|(path,body.to_string())).collect::<Vec<_>>();
        let line = |offset: &str, code: &str, amount: &str| {
            format!(
                "<GLTransactionLine type=\"40\" linetype=\"0\" line=\"2\"{offset} status=\"20\"><Date>2025-01-01</Date><FinYear number=\"2025\"/><FinPeriod number=\"1\"/><GLAccount code=\"{code}\"/>{}<Amount><Currency code=\"EUR\"/><Value>{amount}</Value></Amount><Note><![CDATA[untrusted <text>]]></Note></GLTransactionLine>",
                if offset.is_empty() {
                    ""
                } else {
                    "<GLOffset code=\"BANK\"/>"
                }
            )
        };
        let xml = format!(
            "<eExact><GLTransactions><GLTransaction entry=\"1001\"><TransactionType number=\"40\"/><Journal code=\"B1\"/><Date>2025-01-01</Date><OpeningBalance>100</OpeningBalance><ClosingBalance>88.7</ClosingBalance>{}{}</GLTransaction></GLTransactions><Topics><Topic code=\"GLTransactions\" ts_d=\"0x0000000000000001\" count=\"1\" pagesize=\"100\"/></Topics><Messages/></eExact>",
            line(" offsetline=\"1\"", "U1", "11.3"),
            line("", "BANK", "-11.3")
        );
        responses.push(("/docs/XMLDownload.aspx?", xml));
        responses.push(("/financialtransaction/BankEntries?", header.to_string()));
        responses.push(("/financialtransaction/BankEntryLines?", native.to_string()));
        for (path, body) in responses {
            let (mut stream, target) = accept_request(&listener);
            assert!(
                target.starts_with("GET ") && target.contains(path),
                "{target}"
            );
            assert!(!target.contains("XMLUpload"));
            assert!(!target.contains("Params_DownloadID"));
            let content = if path.starts_with("/docs/") {
                assert!(target.contains("Params_Year=2025"));
                assert!(target.contains("_Division_=42"));
                "application/xml"
            } else {
                "application/json"
            };
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: {content}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
    });
    let (_root, directory, _) = fixture_session(&base);
    let mut process = McpProcess::start(&directory);
    process.initialize();
    let args = json!({"bank_transaction_line_id":id(101),"pending_posting_line_id":id(201),"destination_gl_account_id":id(200),"no_vat_code":"N0","expected_pending_amount":"11.30","expected_currency":"EUR","unassigned_gl_account_ids":[id(10)]});
    let response = process.request(
        2,
        "tools/call",
        json!({"name":"preview_bank_gl_xml","arguments":args}),
    );
    assert_ne!(response["result"]["isError"], true, "{response}");
    let data = &response["result"]["structuredContent"];
    assert_eq!(data["status"], "blocked");
    assert_eq!(data["authorizes_upload"], false);
    assert_eq!(data["upload_attempted"], false);
    assert_eq!(data["schema_validated"], false);
    assert!(data["approval_token"].is_null());
    assert_eq!(data["candidate_xml_line_count"], 1);
    assert_eq!(data["source_bank_line_count"], 1);
    assert_eq!(data["source_document_mapping_included"], false);
    assert!(
        data["experimental_xml"]
            .as_str()
            .unwrap()
            .contains("<Value>-11.3</Value>")
    );
    assert_eq!(
        serde_json::from_str::<Value>(response["result"]["content"][0]["text"].as_str().unwrap())
            .unwrap(),
        *data
    );
    assert!(!response.to_string().contains("fixture-access-token"));
    api.join().unwrap();
    process.shutdown();
    assert!(!directory.join("downloads").exists());
}

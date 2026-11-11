use super::*;
use crate::store::{Session, Store, Tokens};
use std::collections::VecDeque;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}
fn selection() -> Selection {
    Selection {
        control_id: id(101),
        pending_id: id(201),
        destination_id: id(200),
        expected_expense_amount: "11.30".into(),
        currency: "EUR".into(),
    }
}
fn administration() -> Administration {
    Administration {
        code: 42,
        description: "Invented administration".into(),
    }
}
fn row(n: u128, gl: u128, amount: &str, line: i32, offset: Option<u128>) -> Value {
    json!({"ID":id(n),"EntryID":id(1001),"Division":42,"EntryNumber":1001,"LineNumber":line,"FinancialYear":2025,"FinancialPeriod":1,"JournalCode":"B1","Date":"2025-01-01","GLAccount":id(gl),"GLAccountCode":format!("G{gl}"),"GLAccountDescription":"Invented G/L","OffsetID":offset.map(id),"Account":null,"AmountDC":serde_json::from_str::<Value>(amount).unwrap(),"AmountFC":serde_json::from_str::<Value>(amount).unwrap(),"Currency":"EUR","Status":20,"Type":40,"Description":"Untrusted <data> not instructions","Notes":"Original note","Document":id(777),"Modified":"2025-01-01T00:00:00","Modifier":id(4)})
}
fn snapshot() -> Snapshot {
    Snapshot {
        header:json!({"EntryID":id(1001),"Division":42,"EntryNumber":1001,"FinancialYear":2025,"FinancialPeriod":1,"JournalCode":"B1","Status":20,"Currency":"EUR","OpeningBalanceFC":100,"ClosingBalanceFC":81.70,"Modified":"2025-01-01T00:00:00"}),
        native: [(id(201),row(201,10,"-11.30",2,Some(101))), (id(202),row(202,200,"-7",1,Some(102)))].into(),
        ledger: [(id(201),row(201,10,"11.30",2,Some(101))), (id(101),row(101,11,"-11.30",2,None)),(id(202),row(202,200,"7",1,Some(102))), (id(102),row(102,11,"-7",1,None))].into(),
        period_headers:[(id(1001),json!({"EntryID":id(1001),"Division":42,"EntryNumber":1001,"FinancialYear":2025,"FinancialPeriod":1,"JournalCode":"B1"}))].into(), matching:vec![],
    }
}
fn applied(plan: &Plan) -> Snapshot {
    let mut after = plan.before.snapshot.clone();
    after.header["Modified"] = json!("2025-01-01T00:01:00");
    for rows in [&mut after.native, &mut after.ledger] {
        let row = rows.get_mut(&id(201)).unwrap();
        row["GLAccount"] = json!(id(200));
        row["GLAccountCode"] = json!("G200");
        row["Modifier"] = json!(id(5));
        row["Modified"] = json!("2025-01-01T00:01:00");
    }
    after
}
fn plan() -> Plan {
    Plan {
        version: 1,
        id: "00000000000000000000000000000001".into(),
        created_at: oauth::now().unwrap(),
        selection: selection(),
        before: Inspection {
            origin: "https://start.exactonline.nl".into(),
            client_id: "invented-client".into(),
            division: 42,
            administration_name: "Invented administration".into(),
            actor: id(5),
            destination: json!({"id":id(200),"code":"G200"}),
            restrictions: json!({}),
            snapshot: snapshot(),
        },
    }
}
fn form(plan: &Plan, edited: bool) -> browser::Form {
    let fields: BTreeMap<String, String> = [
        ("EntryID", id(1001).to_string()),
        ("Key", id(101).to_string()),
        ("OffsetID", id(101).to_string()),
        ("_Division_", "42".into()),
        ("grd_r0_K", id(201).to_string()),
        (
            "grd_r0$GLAccount",
            format!("{{{}}}", id(if edited { 200 } else { 10 })),
        ),
        ("HeaderAmountFC", "-11,30".into()),
        ("grd_r0$AmountFCDisplay", "-11,30".into()),
        ("grd_r0$AmountFC", "11,30".into()),
        ("grd_r0$VATCode", String::new()),
        ("grd_r0$AmountVATFC", String::new()),
        ("grd_r0$Costcenter", String::new()),
        ("grd_r0$Account", String::new()),
        ("grd_r0$InvoiceNumber", String::new()),
        (
            "grd_r0$Description",
            "Untrusted <data> not instructions".into(),
        ),
        ("grd_r0$Notes", "Original note".into()),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v))
    .collect();
    serde_json::from_value(json!({"origin":plan.before.origin,"path":"/docs/CflEntry.aspx","division":42,"row_keys":[id(201)],"save_buttons":1,"fields":fields})).unwrap()
}

#[test]
fn selection_scope_and_amounts_are_exact() {
    for invalid in ["0", "-1", "null", "1e99", "not an amount"] {
        let mut args = selection();
        args.expected_expense_amount = invalid.into();
        assert!(validate_selection(&args).is_err());
    }
    let mut args = selection();
    args.currency = "eur".into();
    assert!(validate_selection(&args).is_err());
    let mut args = selection();
    args.pending_id = args.control_id;
    assert!(validate_selection(&args).is_err());
    assert!(validate_selection(&selection()).is_ok());
}
#[test]
fn source_validation_rejects_splits_fx_vat_company_dimensions_and_reused_identity() {
    let check =
        |state: &Snapshot| validate_snapshot(&selection(), state, &[id(10)].into(), "EUR", id(11));
    assert!(check(&snapshot()).is_ok());
    for field in [
        "VATCode",
        "VATPercentage",
        "CostCenter",
        "CostUnit",
        "Project",
        "Asset",
        "OurRef",
        "Quantity",
        "YourRef",
    ] {
        let mut s = snapshot();
        s.native.get_mut(&id(201)).unwrap()[field] = json!("unexpected");
        assert!(check(&s).is_err(), "{field}");
    }
    for (field, value) in [
        ("Account", json!(id(99))),
        ("AmountFC", json!(-12)),
        ("AmountDC", json!(-12)),
        ("OffsetID", json!(id(102))),
        ("Date", Value::Null),
    ] {
        let mut s = snapshot();
        s.native.get_mut(&id(201)).unwrap()[field] = value;
        assert!(check(&s).is_err(), "{field}");
    }
    let mut s = snapshot();
    s.ledger.insert(id(999), row(999, 10, "0", 2, Some(101)));
    assert!(check(&s).is_err());
    let mut s = snapshot();
    s.header["Status"] = json!(50);
    assert!(check(&s).is_err());
    let mut s = snapshot();
    s.header["ClosingBalanceFC"] = json!(80);
    assert!(check(&s).is_err());
}
#[test]
fn postconditions_require_same_records_and_preserve_all_nonapproved_fields() {
    let p = plan();
    let after = applied(&p);
    assert_eq!(outcome(&p, &after).unwrap(), "verified");
    assert_eq!(outcome(&p, &p.before.snapshot).unwrap(), "not_applied");
    for field in [
        "AmountFC",
        "AmountDC",
        "Document",
        "OffsetID",
        "Description",
        "Notes",
        "VATCode",
        "Status",
        "Date",
        "UnknownFutureField",
    ] {
        let mut changed = after.clone();
        changed.native.get_mut(&id(201)).unwrap()[field] = json!("changed");
        assert_ne!(
            outcome(&p, &changed).unwrap_or("uncertain"),
            "verified",
            "{field}"
        );
    }
    let mut s = after.clone();
    s.native.get_mut(&id(202)).unwrap()["Modified"] = json!("changed");
    assert_eq!(outcome(&p, &s).unwrap(), "uncertain");
    let mut s = after.clone();
    s.ledger.remove(&id(101));
    assert_eq!(outcome(&p, &s).unwrap(), "uncertain");
    let mut s = after.clone();
    s.matching.push(json!({"new_group":true}));
    assert_eq!(outcome(&p, &s).unwrap(), "uncertain");
    let mut s = after.clone();
    s.period_headers.insert(id(999), json!({}));
    assert_eq!(outcome(&p, &s).unwrap(), "uncertain");
    let mut s = after;
    s.native.get_mut(&id(201)).unwrap()["Modifier"] = json!(id(99));
    assert_eq!(outcome(&p, &s).unwrap(), "uncertain");
}
#[test]
fn editor_identity_amount_and_preservation_checks_fail_closed() {
    let p = plan();
    assert!(browser::validate_form(&form(&p, false), &p, false).is_ok());
    assert!(browser::validate_form(&form(&p, true), &p, true).is_ok());
    for key in ["origin", "path", "division", "row_keys", "save_buttons"] {
        let mut value = serde_json::to_value(form(&p, false)).unwrap();
        value[key] = match key {
            "division" => json!(43),
            "row_keys" => json!([id(201), id(202)]),
            "save_buttons" => json!(2),
            _ => json!("wrong"),
        };
        let f: browser::Form = serde_json::from_value(value).unwrap();
        assert!(browser::validate_form(&f, &p, false).is_err(), "{key}");
    }
    for key in [
        "Key",
        "EntryID",
        "OffsetID",
        "grd_r0$AmountFC",
        "grd_r0$VATCode",
        "grd_r0$Notes",
        "grd_r0$Account",
    ] {
        let mut value = serde_json::to_value(form(&p, true)).unwrap();
        value["fields"][key] = json!("unexpected");
        let f = serde_json::from_value(value).unwrap();
        assert!(browser::validate_form(&f, &p, true).is_err(), "{key}");
    }
    assert!(browser::preserved_form(&form(&p, false), &form(&p, true)).is_ok());
    let mut value = serde_json::to_value(form(&p, true)).unwrap();
    value["fields"]["grd_r0$RateFC"] = json!("0.5");
    assert!(
        browser::preserved_form(&form(&p, false), &serde_json::from_value(value).unwrap()).is_err()
    );
}
#[test]
fn artifacts_are_single_use_private_and_reject_path_traversal() {
    let d = tempfile::tempdir().unwrap();
    private_directory(&d.path().join("data")).unwrap();
    let root = artifact_root(&d.path().join("data")).unwrap();
    let p = root.join("result.json");
    write_new(&p, &json!({"verified":true})).unwrap();
    assert!(write_new(&p, &json!({"overwritten":true})).is_err());
    for value in [
        "../session.json",
        "../../escape",
        "",
        "ABCDEF01234567890123456789012345",
    ] {
        assert!(plan_path(&root, value).is_err());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let link = root.join("link");
        symlink(&p, &link).unwrap();
        assert!(store::check_private(&link).is_err());
    }
}

#[derive(Clone)]
struct Reply {
    path: &'static str,
    body: String,
}
fn json_reply(path: &'static str, rows: Vec<Value>) -> Reply {
    Reply {
        path,
        body: json!({"d":{"results":rows}}).to_string(),
    }
}
fn capture_replies(s: &Snapshot) -> Vec<Reply> {
    vec![
        json_reply(
            "/api/v1/42/financialtransaction/BankEntries",
            vec![s.header.clone()],
        ),
        json_reply(
            "/api/v1/42/financialtransaction/BankEntryLines",
            s.native.values().cloned().collect(),
        ),
        json_reply(
            "/api/v1/42/financialtransaction/TransactionLines",
            s.ledger.values().cloned().collect(),
        ),
        json_reply(
            "/api/v1/42/financialtransaction/BankEntries",
            s.period_headers.values().cloned().collect(),
        ),
        Reply {
            path: "/docs/XMLDownload.aspx",
            body: "<eExact><MatchSets/><Topics/><Messages/></eExact>".into(),
        },
    ]
}
fn inspect_replies(s: &Snapshot) -> Vec<Reply> {
    let mut replies = capture_replies(s);
    replies.extend([
        json_reply("/api/v1/42/financial/GLAccounts",[10,11,200].into_iter().map(|n|json!({"ID":id(n),"Division":42,"Code":format!("G{n}"),"Description":"Invented G/L","Type":if n==200{120}else{90},"IsBlocked":false,"UseCostcenter":0,"UseCostunit":0})).collect()),
        json_reply("/api/v1/42/financial/Journals",vec![json!({"ID":id(1),"Division":42,"Code":"B1","Type":12,"IsBlocked":false,"Currency":"EUR","GLAccount":id(11)})]),
        json_reply("/api/v1/42/financial/FinancialPeriods",vec![json!({"ID":id(600),"Division":42,"FinYear":2025,"FinPeriod":1,"StartDate":"2025-01-01","EndDate":"2025-01-31"})]),
        json_reply("/api/v1/42/read/financial/JournalStatusList",vec![json!({"Journal":"B1","Year":2025,"Period":1,"Status":0})]),
        json_reply("/api/v1/42/system/Divisions",vec![json!({"Code":42,"Currency":"EUR"})]),
        json_reply("/api/v1/current/Me",vec![json!({"UserID":id(5)})]),
    ]);
    replies
}
pub(crate) struct Server {
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Vec<Url>>,
}
impl Server {
    pub(crate) async fn finish(self) -> Vec<Url> {
        self.stop.send(()).unwrap();
        self.task.await.unwrap()
    }
}
async fn fixture(replies: Vec<Reply>) -> (tempfile::TempDir, Server, ExactClient) {
    let d = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(d.path(), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let server_base = base.clone();
    let (stop, mut stopped) = oneshot::channel();
    let task = tokio::spawn(async move {
        let mut replies: VecDeque<_> = replies.into();
        let mut requests = Vec::new();
        loop {
            let (mut stream, _) = tokio::select! {_=&mut stopped=>break,accepted=listener.accept()=>accepted.unwrap()};
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).await.unwrap();
                request.push(byte[0]);
            }
            let request = String::from_utf8(request).unwrap();
            let mut parts = request.lines().next().unwrap().split_whitespace();
            assert_eq!(parts.next(), Some("GET"));
            let url = server_base.join(parts.next().unwrap()).unwrap();
            let reply = replies.pop_front().expect("Unexpected HTTP request");
            assert_eq!(url.path(), reply.path);
            requests.push(url);
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        reply.body.len(),
                        reply.body
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
        assert!(
            replies.is_empty(),
            "Unconsumed fixture requests: {}",
            replies.len()
        );
        requests
    });
    let store = Store::open(d.path()).await.unwrap();
    let session = Session {
        base_url: base.to_string(),
        client_id: "invented-client".into(),
        redirect_uri: "https://localhost/callback".into(),
        tokens: Tokens {
            access_token: "invented-token".into(),
            refresh_token: "invented-refresh".into(),
            expires_at: oauth::now().unwrap() + 3600,
        },
        administration: Some(administration()),
    };
    store.save(&session).await.unwrap();
    store
        .save_bank_configuration(
            &session,
            BankConfiguration {
                unassigned_gl_account_ids: vec![id(10)],
            },
        )
        .await
        .unwrap();
    drop(store);
    let mut client = ExactClient::new(
        d.path().into(),
        reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .unwrap(),
        None,
    )
    .await
    .unwrap();
    client.pin_administration().unwrap();
    (d, Server { stop, task }, client)
}

#[derive(Default)]
struct Count(AtomicUsize);
impl Count {
    fn get(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
    fn set(&self, value: usize) {
        self.0.store(value, Ordering::SeqCst);
    }
}
struct FakeBrowser {
    forms: Mutex<VecDeque<browser::Form>>,
    saves: Count,
    fills: Count,
    save_error: bool,
}
impl FakeBrowser {
    fn new(plan: &Plan) -> Self {
        Self {
            forms: Mutex::new([form(plan, false), form(plan, true), form(plan, true)].into()),
            saves: Count::default(),
            fills: Count::default(),
            save_error: false,
        }
    }
}
impl Driver for FakeBrowser {
    async fn open_editor(&self, _: &Plan) -> Result<()> {
        Ok(())
    }
    async fn read_form(&self) -> Result<browser::Form> {
        self.forms
            .lock()
            .unwrap()
            .pop_front()
            .context("Missing fake form")
    }
    async fn set_gl(&self, _: &Plan) -> Result<()> {
        self.fills.set(self.fills.get() + 1);
        Ok(())
    }
    async fn diagnostics(&self) -> Result<Vec<Value>> {
        Ok(vec![])
    }
    async fn save(&self) -> Result<()> {
        self.saves.set(self.saves.get() + 1);
        if self.save_error {
            anyhow::bail!("Simulated lost acknowledgement");
        }
        Ok(())
    }
}

#[tokio::test]
async fn captures_complete_scoped_sources_with_get_only() {
    let (d, server, client) = fixture(inspect_replies(&snapshot())).await;
    let inspection = inspect(&client, &administration(), &selection(), id(1001))
        .await
        .unwrap();
    assert!(inspection.snapshot == snapshot());
    assert_eq!(inspection.actor, id(5));
    assert_eq!(inspection.destination["code"], "G200");
    let requests = server.finish().await;
    assert_eq!(requests.len(), 11);
    assert!(!d.path().join("bank-ui").exists());
}

#[tokio::test]
async fn approved_flow_saves_once_and_readback_can_resolve_a_lost_acknowledgement() {
    for save_error in [false, true] {
        let mut replies = inspect_replies(&snapshot());
        replies.extend(inspect_replies(&snapshot()));
        replies.extend(inspect_replies(&snapshot()));
        let mut p = plan();
        replies.extend(capture_replies(&applied(&p)));
        let (d, server, client) = fixture(replies).await;
        p.before = inspect(&client, &administration(), &selection(), id(1001))
            .await
            .unwrap();
        let folder = d.path().join("attempt");
        private_directory(&folder).unwrap();
        write_new(&folder.join("approved.json"), &json!({"preview_id":p.id})).unwrap();
        let mut browser = FakeBrowser::new(&p);
        browser.save_error = save_error;
        run_approved(&browser, &client, &administration(), &p, &folder)
            .await
            .unwrap();
        assert_eq!(browser.saves.get(), 1);
        assert_eq!(browser.fills.get(), 1);
        let result: Value =
            serde_json::from_slice(&fs::read(folder.join("result.json")).unwrap()).unwrap();
        assert_eq!(result["status"], "verified");
        assert_eq!(result["evidence"]["save_command_succeeded"], !save_error);
        assert!(folder.join("save-reserved.json").exists());
        assert!(
            run_approved(&browser, &client, &administration(), &p, &folder)
                .await
                .is_err()
        );
        assert_eq!(browser.saves.get(), 1);
        assert_eq!(server.finish().await.len(), 38);
    }
}

#[tokio::test]
async fn changed_source_stops_before_typing_or_save() {
    let mut changed = snapshot();
    changed.header["Modified"] = json!("changed");
    let mut replies = inspect_replies(&snapshot());
    replies.extend(inspect_replies(&changed));
    let (d, server, client) = fixture(replies).await;
    let mut p = plan();
    p.before = inspect(&client, &administration(), &selection(), id(1001))
        .await
        .unwrap();
    let folder = d.path().join("attempt");
    private_directory(&folder).unwrap();
    write_new(&folder.join("approved.json"), &json!({})).unwrap();
    let browser = FakeBrowser::new(&p);
    assert!(
        run_approved(&browser, &client, &administration(), &p, &folder)
            .await
            .is_err()
    );
    assert_eq!(browser.fills.get(), 0);
    assert_eq!(browser.saves.get(), 0);
    assert!(!folder.join("save-reserved.json").exists());
    assert_eq!(server.finish().await.len(), 22);
}

#[tokio::test]
async fn unchanged_readback_after_click_is_uncertain_not_permission_to_retry() {
    let mut replies = inspect_replies(&snapshot());
    replies.extend(inspect_replies(&snapshot()));
    replies.extend(inspect_replies(&snapshot()));
    for _ in 0..3 {
        replies.extend(capture_replies(&snapshot()));
    }
    let (d, server, client) = fixture(replies).await;
    let mut p = plan();
    p.before = inspect(&client, &administration(), &selection(), id(1001))
        .await
        .unwrap();
    let folder = d.path().join("attempt");
    private_directory(&folder).unwrap();
    write_new(&folder.join("approved.json"), &json!({})).unwrap();
    let browser = FakeBrowser::new(&p);
    assert!(
        run_approved(&browser, &client, &administration(), &p, &folder)
            .await
            .is_err()
    );
    assert_eq!(browser.saves.get(), 1);
    let result: Value =
        serde_json::from_slice(&fs::read(folder.join("result.json")).unwrap()).unwrap();
    assert_eq!(result["status"], "uncertain");
    assert_eq!(result["retry_allowed"], false);
    assert_eq!(server.finish().await.len(), 48);
}

#[tokio::test]
#[ignore = "Opt-in visible agent-browser test against a synthetic localhost form only"]
async fn real_agent_browser_local_form_smoke() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mut p = plan();
    p.before.origin = base.clone();
    let initial = serde_json::to_value(form(&p, false)).unwrap();
    let escape = |s: &str| {
        s.replace('&', "&amp;")
            .replace('"', "&quot;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    let mut inputs = String::new();
    for (key, value) in initial["fields"].as_object().unwrap() {
        inputs.push_str(&format!(
            "<input type=hidden name=\"{}\" value=\"{}\">",
            escape(key),
            escape(value.as_str().unwrap())
        ));
    }
    let html = format!(
        r#"<!doctype html><html><head><title>Synthetic bank UI fixture — no Exact connection</title><link rel="icon" href="data:,"></head><body><h1>Synthetic test only — no financial data</h1><form>{inputs}<label>Grootboekrekening<input id="visible-gl" value="G10" onblur="document.getElementsByName('grd_r0$GLAccount')[0].value='{{{destination}}}'"></label><button type="button" onclick="fetch('/fixture/save',{{method:'POST'}}).then(r=>{{if(r.ok)document.getElementById('result').textContent='Saved fixture'}})">Opslaan</button></form><p id="result"></p></body></html>"#,
        destination = id(200)
    );
    let saves = Arc::new(AtomicUsize::new(0));
    let observed = saves.clone();
    let (stop, mut stopped) = oneshot::channel();
    let task = tokio::spawn(async move {
        loop {
            let (mut stream, _) = tokio::select! {_=&mut stopped=>break,accepted=listener.accept()=>accepted.unwrap()};
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).await.unwrap();
                request.push(byte[0]);
            }
            let request = String::from_utf8(request).unwrap();
            let parts: Vec<_> = request.lines().next().unwrap().split_whitespace().collect();
            let (path, body, kind) = if parts[1].starts_with("/api/v1/current/Me?") {
                assert_eq!(parts[0], "GET");
                (
                    "me",
                    json!({"d":{"results":[{"UserID":id(5),"CurrentDivision":42}]}}).to_string(),
                    "application/json",
                )
            } else if parts[1] == "/fixture/save" {
                assert_eq!(parts[0], "POST");
                observed.fetch_add(1, Ordering::SeqCst);
                ("save", "{}".into(), "application/json")
            } else {
                assert_eq!(parts[0], "GET");
                assert_eq!(parts[1], "/docs/CflEntry.aspx");
                ("editor", html.clone(), "text/html")
            };
            let _ = path;
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
    });
    let browser = browser::AgentBrowser;
    browser
        .open_fixture(&format!("{base}/docs/CflEntry.aspx"))
        .await
        .unwrap();
    let initial = browser.read_form().await.unwrap();
    browser::validate_form(&initial, &p, false).unwrap();
    let baseline = browser.diagnostics().await.unwrap();
    assert!(
        baseline.is_empty(),
        "Pre-existing browser errors: {baseline:?}"
    );
    browser.set_gl(&p).await.unwrap();
    let edited = browser.read_form().await.unwrap();
    browser::validate_form(&edited, &p, true).unwrap();
    browser::preserved_form(&initial, &edited).unwrap();
    browser.save().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while saves.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(saves.load(Ordering::SeqCst), 1);
    assert!(browser.diagnostics().await.unwrap().is_empty());
    stop.send(()).unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn preview_persists_only_a_private_unapproved_expiring_plan() {
    let mut replies = vec![json_reply(
        "/api/v1/42/financialtransaction/TransactionLines",
        vec![json!({"ID":id(201),"EntryID":id(1001),"Division":42})],
    )];
    replies.extend(inspect_replies(&snapshot()));
    replies.extend(capture_replies(&snapshot()));
    let (d, server, _client) = fixture(replies).await;
    prepare(d.path().into(), selection()).await.unwrap();
    let folders: Vec<_> = fs::read_dir(d.path().join("bank-ui"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(folders.len(), 1);
    let plan: Plan =
        serde_json::from_slice(&fs::read(folders[0].join("plan.json")).unwrap()).unwrap();
    assert_eq!(plan.version, 1);
    valid_time(&plan).unwrap();
    assert_eq!(preview(&plan).unwrap()["posting_date"], "2025-01-01");
    assert_eq!(preview(&plan).unwrap()["authorizes_execution"], false);
    assert!(!folders[0].join("approved.json").exists());
    assert!(!folders[0].join("save-reserved.json").exists());
    assert_eq!(server.finish().await.len(), 17);
}

#[test]
fn stale_or_future_previews_are_rejected() {
    let mut p = plan();
    p.created_at = oauth::now().unwrap() - 1801;
    assert!(valid_time(&p).is_err());
    p.created_at = oauth::now().unwrap() + 3600;
    assert!(valid_time(&p).is_err());
}

#[test]
fn browser_dom_line_endings_are_normalized_but_api_preservation_is_exact() {
    let mut p = plan();
    p.before.snapshot.native.get_mut(&id(201)).unwrap()["Notes"] = json!("Original\r\nnote");
    p.before.snapshot.ledger.get_mut(&id(201)).unwrap()["Notes"] = json!("Original\r\nnote");
    let mut value = serde_json::to_value(form(&p, false)).unwrap();
    value["fields"]["grd_r0$Notes"] = json!("Original\nnote");
    let f: browser::Form = serde_json::from_value(value).unwrap();
    assert!(browser::validate_form(&f, &p, false).is_ok());
    let mut after = applied(&p);
    assert_eq!(outcome(&p, &after).unwrap(), "verified");
    after.native.get_mut(&id(201)).unwrap()["Notes"] = json!("Original\nnote");
    assert_eq!(outcome(&p, &after).unwrap(), "uncertain");
    assert!(!include_str!("form.js").contains("fetch("));
}

struct TestAsk {
    answers: Mutex<VecDeque<Option<bool>>>,
    questions: Mutex<Vec<String>>,
}
impl TestAsk {
    fn new(answers: Vec<Option<bool>>) -> Self {
        Self {
            answers: Mutex::new(answers.into()),
            questions: Mutex::new(vec![]),
        }
    }
}
impl conversation::Ask for TestAsk {
    async fn ask(&self, message: &str) -> Result<Option<bool>> {
        self.questions.lock().unwrap().push(message.into());
        self.answers
            .lock()
            .unwrap()
            .pop_front()
            .context("Unexpected confirmation request")
    }
}
struct ConversationTestBrowser {
    inner: FakeBrowser,
    authenticated: std::sync::atomic::AtomicBool,
    opened: Count,
    selected: Count,
}
impl ConversationTestBrowser {
    fn new(p: &Plan, authenticated: bool) -> Self {
        let inner = FakeBrowser::new(p);
        inner.forms.lock().unwrap().push_front(form(p, false));
        Self {
            inner,
            authenticated: std::sync::atomic::AtomicBool::new(authenticated),
            opened: Count::default(),
            selected: Count::default(),
        }
    }
}
impl Driver for ConversationTestBrowser {
    async fn open_editor(&self, p: &Plan) -> Result<()> {
        ensure!(
            self.authenticated.load(Ordering::SeqCst),
            "Manual login needed"
        );
        self.inner.open_editor(p).await
    }
    async fn read_form(&self) -> Result<browser::Form> {
        self.inner.read_form().await
    }
    async fn set_gl(&self, p: &Plan) -> Result<()> {
        self.inner.set_gl(p).await
    }
    async fn diagnostics(&self) -> Result<Vec<Value>> {
        self.inner.diagnostics().await
    }
    async fn save(&self) -> Result<()> {
        self.inner.save().await
    }
}
impl conversation::ConversationBrowser for ConversationTestBrowser {
    async fn open_session(&self, _: &Plan) -> Result<String> {
        self.opened.set(self.opened.get() + 1);
        Ok("0".repeat(32))
    }
    async fn select_session(&self, target: &str) -> Result<()> {
        ensure!(target == "0".repeat(32), "Wrong tab");
        self.selected.set(self.selected.get() + 1);
        Ok(())
    }
    async fn pending_login(&self) -> Result<bool> {
        Ok(!self.authenticated.load(Ordering::SeqCst))
    }
}
fn execute_args(p: &Plan) -> conversation::ExecuteBankUi {
    conversation::ExecuteBankUi {
        preview_id: p.id.clone(),
        confirmation: None,
        resume_after_login: false,
    }
}
fn host_confirmation(approved: bool) -> conversation::HostConfirmation {
    conversation::HostConfirmation {
        approved,
        via: "any-user-ask-plugin".into(),
        user_response: if approved {
            "Approve this exact preview".into()
        } else {
            "Cancel".into()
        },
    }
}
async fn saved_plan(directory: &Path, client: &ExactClient) -> Plan {
    let mut p = plan();
    p.before = inspect(client, &administration(), &selection(), id(1001))
        .await
        .unwrap();
    let root = artifact_root(directory).unwrap();
    let folder = root.join(&p.id);
    private_directory(&folder).unwrap();
    write_new(&folder.join("plan.json"), &p).unwrap();
    p
}

pub(crate) async fn mcp_confirmation_fixture() -> (tempfile::TempDir, Server, Value) {
    let mut replies = vec![json_reply(
        "/api/v1/42/financialtransaction/TransactionLines",
        vec![json!({"ID":id(201),"EntryID":id(1001),"Division":42})],
    )];
    replies.extend(inspect_replies(&snapshot()));
    replies.extend(capture_replies(&snapshot()));
    replies.extend(inspect_replies(&snapshot()));
    let (d, server, _client) = fixture(replies).await;
    (d, server, serde_json::to_value(selection()).unwrap())
}

#[tokio::test]
async fn conversation_without_native_ui_returns_preview_question_and_never_opens_browser() {
    let mut replies = inspect_replies(&snapshot());
    replies.extend(inspect_replies(&snapshot()));
    let (d, server, client) = fixture(replies).await;
    let p = saved_plan(d.path(), &client).await;
    let ask = TestAsk::new(vec![None]);
    let browser = ConversationTestBrowser::new(&p, true);
    let result =
        conversation::execute_with(&client, &administration(), execute_args(&p), &ask, &browser)
            .await
            .unwrap();
    assert_eq!(result["status"], "requires_confirmation");
    assert_eq!(result["preview"]["preview_id"], p.id);
    assert_eq!(browser.opened.get(), 0);
    assert_eq!(browser.inner.saves.get(), 0);
    assert!(
        !d.path()
            .join("bank-ui")
            .join(&p.id)
            .join("approved.json")
            .exists()
    );
    assert!(ask.questions.lock().unwrap()[0].contains(&p.id));
    let mut args = execute_args(&p);
    args.confirmation = Some(host_confirmation(false));
    let result = conversation::execute_with(&client, &administration(), args, &ask, &browser)
        .await
        .unwrap();
    assert_eq!(result["status"], "cancelled");
    assert_eq!(server.finish().await.len(), 22);
}

#[tokio::test]
async fn conversation_native_decline_is_final_without_falling_back_to_terminal_or_host_approval() {
    let mut replies = inspect_replies(&snapshot());
    replies.extend(inspect_replies(&snapshot()));
    let (d, server, client) = fixture(replies).await;
    let p = saved_plan(d.path(), &client).await;
    let ask = TestAsk::new(vec![Some(false)]);
    let browser = ConversationTestBrowser::new(&p, true);
    let result =
        conversation::execute_with(&client, &administration(), execute_args(&p), &ask, &browser)
            .await
            .unwrap();
    assert_eq!(result["status"], "cancelled");
    let result =
        conversation::execute_with(&client, &administration(), execute_args(&p), &ask, &browser)
            .await
            .unwrap();
    assert_eq!(result["status"], "cancelled");
    assert_eq!(browser.opened.get(), 0);
    assert_eq!(server.finish().await.len(), 22);
}

#[tokio::test]
async fn conversation_native_or_any_host_confirmation_executes_once_without_a_terminal() {
    for native in [true, false] {
        let mut replies = inspect_replies(&snapshot());
        for _ in 0..4 {
            replies.extend(inspect_replies(&snapshot()));
        }
        replies.extend(capture_replies(&applied(&plan())));
        let (d, server, client) = fixture(replies).await;
        let p = saved_plan(d.path(), &client).await;
        let ask = TestAsk::new(if native { vec![Some(true)] } else { vec![] });
        let browser = ConversationTestBrowser::new(&p, true);
        let mut args = execute_args(&p);
        if !native {
            args.confirmation = Some(host_confirmation(true));
        }
        let result = conversation::execute_with(&client, &administration(), args, &ask, &browser)
            .await
            .unwrap();
        assert_eq!(result["status"], "verified");
        assert_eq!(browser.inner.saves.get(), 1);
        assert_eq!(browser.opened.get(), 1);
        assert!(!d.path().join("bank-ui/active-conversation.json").exists());
        let result = conversation::execute_with(
            &client,
            &administration(),
            execute_args(&p),
            &ask,
            &browser,
        )
        .await
        .unwrap();
        assert_eq!(result["status"], "verified");
        assert_eq!(browser.inner.saves.get(), 1);
        assert_eq!(ask.questions.lock().unwrap().len(), usize::from(native));
        assert_eq!(server.finish().await.len(), 60);
    }
}

#[tokio::test]
async fn conversation_login_can_pause_and_resume_same_approval_without_reopening_or_reapproving() {
    let mut replies = inspect_replies(&snapshot());
    for _ in 0..5 {
        replies.extend(inspect_replies(&snapshot()));
    }
    replies.extend(capture_replies(&applied(&plan())));
    let (d, server, client) = fixture(replies).await;
    let p = saved_plan(d.path(), &client).await;
    let ask = TestAsk::new(vec![None]);
    let browser = ConversationTestBrowser::new(&p, false);
    let mut args = execute_args(&p);
    args.confirmation = Some(host_confirmation(true));
    let result = conversation::execute_with(&client, &administration(), args, &ask, &browser)
        .await
        .unwrap();
    assert_eq!(result["status"], "requires_login");
    assert_eq!(browser.opened.get(), 1);
    assert_eq!(browser.inner.saves.get(), 0);
    browser.authenticated.store(true, Ordering::SeqCst);
    let mut args = execute_args(&p);
    args.resume_after_login = true;
    let result = conversation::execute_with(&client, &administration(), args, &ask, &browser)
        .await
        .unwrap();
    assert_eq!(result["status"], "verified");
    assert_eq!(browser.opened.get(), 1);
    assert_eq!(browser.selected.get(), 1);
    assert_eq!(browser.inner.saves.get(), 1);
    assert_eq!(ask.questions.lock().unwrap().len(), 1);
    assert_eq!(server.finish().await.len(), 71);
}

#[tokio::test]
async fn conversation_resume_is_not_approval_and_expired_paused_runs_can_be_cancelled() {
    let mut replies = inspect_replies(&snapshot());
    replies.extend(inspect_replies(&snapshot()));
    let (d, server, client) = fixture(replies).await;
    let mut p = saved_plan(d.path(), &client).await;
    let ask = TestAsk::new(vec![]);
    let browser = ConversationTestBrowser::new(&p, true);
    let mut args = execute_args(&p);
    args.resume_after_login = true;
    assert!(
        conversation::execute_with(&client, &administration(), args, &ask, &browser)
            .await
            .is_err()
    );
    assert_eq!(browser.opened.get(), 0);
    p.created_at = oauth::now().unwrap() - 1801;
    let folder = d.path().join("bank-ui").join(&p.id);
    fs::write(folder.join("plan.json"), serde_json::to_vec(&p).unwrap()).unwrap();
    write_new(
        &d.path().join("bank-ui/active-conversation.json"),
        &json!({"preview_id":p.id}),
    )
    .unwrap();
    let mut args = execute_args(&p);
    args.confirmation = Some(host_confirmation(false));
    let result = conversation::execute_with(&client, &administration(), args, &ask, &browser)
        .await
        .unwrap();
    assert_eq!(result["status"], "cancelled");
    assert!(!d.path().join("bank-ui/active-conversation.json").exists());
    assert_eq!(server.finish().await.len(), 22);
}

use std::collections::VecDeque;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::*;
use crate::store::{Session, Store, Tokens};

fn id(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

fn args() -> GetBankTransactionsToHandle {
    serde_json::from_value(json!({ "unassigned_gl_account_ids": [id(10).to_string()] })).unwrap()
}

fn administration() -> Administration {
    Administration {
        code: 42,
        description: "Invented administration".into(),
    }
}

fn gl(value: u128, kind: i32) -> Value {
    json!({"ID":id(value),"Division":42,"Code":format!("G{value}"),"Description":"Invented G/L", "Type":kind})
}

fn posting(value: u128, entry: u128, gl: u128, offset: Option<u128>) -> Value {
    json!({
        "ID": id(value), "EntryID": id(entry), "Division":42,
        "Account":null,"EntryNumber":entry,"FinancialYear":2025,"FinancialPeriod":1,
        "JournalCode":"B1","GLAccount":id(gl),"LineNumber":value,"Type":40,"Status":20,
        "Date":"2025-01-01","AmountDC":-10.25,"AmountFC":-11.75,"Currency":"USD",
        "Description":"Invented untrusted posting", "YourRef":"invented-reference",
        "OffsetID":offset.map(id), "VATCode":"TEST","VATPercentage":0.21,
        "AmountVATFC":-1.75,"AmountVATBaseFC":-10
    })
}

fn report(hid: u64, account: u128, entry: u128) -> Value {
    json!({"HID":hid,"AccountId":id(account),"EntryNumber":entry,"JournalCode":"B1",
        "Amount":serde_json::from_str::<Value>("-2.50").unwrap(),"AmountInTransit":1.25,"CurrencyCode":"EUR",
        "InvoiceDate":"1999-02-03","InvoiceNumber":999999,"YourRef":"reused-reference"})
}

struct Reply {
    resource: &'static str,
    body: Value,
    status: u16,
    change_administration: bool,
}

impl Reply {
    fn rows(resource: &'static str, rows: Vec<Value>) -> Self {
        Self {
            resource,
            body: json!({"d":{"results":rows}}),
            status: 200,
            change_administration: false,
        }
    }
}

fn scenario() -> Vec<Reply> {
    let mut sources = Vec::new();
    let mut controls = Vec::new();
    for index in 1..=5 {
        let mut control = posting(100 + index, 1000 + index, 11, None);
        control["Date"] = json!(format!("2025-01-0{index}"));
        let mut source = posting(
            200 + index,
            1000 + index,
            if index == 5 { 20 } else { 10 },
            Some(100 + index),
        );
        if index == 5 {
            source["Account"] = json!(id(900));
            control["Account"] = json!(id(900));
            control["OffsetID"] = json!(id(205));
        }
        sources.push(source);
        controls.push(control);
    }
    let mut entries = controls.clone();
    entries.extend(sources.clone());
    entries.push(posting(301, 1001, 30, Some(101)));
    entries.push(posting(302, 1001, 11, Some(101)));
    entries.push(posting(888, 1001, 11, None));
    entries.push(posting(889, 1001, 30, Some(888)));
    entries.push(posting(890, 1001, 30, None));
    vec![
        Reply::rows(
            "system/Divisions",
            vec![json!({"Code":42,"Currency":"EUR"})],
        ),
        Reply::rows(
            "financial/Journals",
            vec![
                json!({"ID":id(1),"Division":42,"Code":"B1","Type":12,"GLAccount":id(11),"Description":"Invented bank"}),
            ],
        ),
        Reply::rows(
            "financial/GLAccounts",
            vec![gl(10, 90), gl(20, 22), gl(21, 20)],
        ),
        Reply::rows(
            "financialtransaction/TransactionLines",
            sources[..4].to_vec(),
        ),
        Reply::rows("read/financial/PayablesList", vec![report(1, 900, 1005)]),
        Reply::rows(
            "financialtransaction/TransactionLines",
            vec![sources[4].clone(), controls[4].clone()],
        ),
        Reply::rows("read/financial/ReceivablesList", vec![]),
        Reply::rows("financialtransaction/TransactionLines", controls),
        Reply::rows("financialtransaction/TransactionLines", entries),
    ]
}

fn rows(replies: &mut [Reply], index: usize) -> &mut Vec<Value> {
    replies[index].body["d"]["results"].as_array_mut().unwrap()
}

struct Server {
    stop: oneshot::Sender<()>,
    task: JoinHandle<Vec<Url>>,
}

impl Server {
    async fn finish(self) -> Vec<Url> {
        self.stop.send(()).unwrap();
        self.task.await.unwrap()
    }
}

async fn fixture(replies: Vec<Reply>) -> (TempDir, Server, ExactClient) {
    let directory = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let (stop, mut stopped) = oneshot::channel();
    let server_base = base.clone();
    let store_directory = directory.path().to_owned();
    let task = tokio::spawn(async move {
        let mut replies: VecDeque<_> = replies.into();
        let mut requests = Vec::new();
        loop {
            let (mut stream, _) = tokio::select! {
                _ = &mut stopped => break,
                accepted = listener.accept() => accepted.unwrap(),
            };
            let mut bytes = Vec::new();
            loop {
                let mut byte = [0];
                stream.read_exact(&mut byte).await.unwrap();
                bytes.push(byte[0]);
                if bytes.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(bytes).unwrap();
            let first = request.lines().next().unwrap();
            let mut parts = first.split_whitespace();
            assert_eq!(parts.next(), Some("GET"));
            let url = server_base.join(parts.next().unwrap()).unwrap();
            let reply = replies.pop_front().expect("Unexpected request");
            assert_eq!(url.path(), format!("/api/v1/42/{}", reply.resource));
            requests.push(url);
            if reply.change_administration {
                let store = Store::open(&store_directory).await.unwrap();
                let mut session = store.require_session().await.unwrap();
                session.administration.as_mut().unwrap().code = 43;
                store.save(&session).await.unwrap();
            }
            let body = reply.body.to_string();
            stream.write_all(format!("HTTP/1.1 {} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",reply.status,body.len(),body).as_bytes()).await.unwrap();
        }
        assert!(replies.is_empty(), "Expected replies not consumed");
        requests
    });
    let store = Store::open(directory.path()).await.unwrap();
    store
        .save(&Session {
            base_url: base.to_string(),
            client_id: "invented-client".into(),
            redirect_uri: "https://localhost/callback".into(),
            tokens: Tokens {
                access_token: "invented-token".into(),
                refresh_token: "invented-refresh".into(),
                expires_at: crate::oauth::now().unwrap() + 3600,
            },
            administration: Some(administration()),
        })
        .await
        .unwrap();
    drop(store);
    let http = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let mut client = ExactClient::new(directory.path().into(), http, None)
        .await
        .unwrap();
    client.pin_administration().unwrap();
    (directory, Server { stop, task }, client)
}

async fn run(replies: Vec<Reply>, args: GetBankTransactionsToHandle) -> (Value, Vec<Url>) {
    let (_directory, server, client) = fixture(replies).await;
    let result = get_transactions_to_handle(&client, &administration(), args)
        .await
        .unwrap();
    (serde_json::to_value(result).unwrap(), server.finish().await)
}

async fn fails(replies: Vec<Reply>, args: GetBankTransactionsToHandle, message: &str) {
    let (_directory, server, client) = fixture(replies).await;
    let error = get_transactions_to_handle(&client, &administration(), args)
        .await
        .err()
        .expect("Expected error");
    assert!(
        format!("{error:#}").contains(message),
        "Unexpected error: {error:#}"
    );
    server.finish().await;
}

#[tokio::test]
async fn reconstructs_four_unassigned_plus_one_open_ap_with_explicit_offsets_only() {
    let (result, requests) = run(scenario(), args()).await;
    assert_eq!(result["reconstructed_transaction_count"], 5);
    assert_eq!(result["matched_transaction_count"], 5);
    assert_eq!(result["source_unassigned_posting_count"], 4);
    assert_eq!(result["source_bank_outstanding_item_count"], 1);
    assert_eq!(result["coverage_complete"], true);
    assert_eq!(result["default_currency"], "EUR");
    let transactions = result["transactions"].as_array().unwrap();
    assert_eq!(transactions[0]["id"], json!(id(105)));
    assert_eq!(
        transactions[0]["reasons"],
        json!(["outstanding_payable_posting"])
    );
    assert_eq!(
        transactions[0]["evidence"][0]["outstanding_item"]["amount"],
        "-2.50"
    );
    assert_eq!(transactions[0]["amount_default_currency"], "-10.25");
    assert_eq!(transactions[0]["amount_transaction_currency"], "-11.75");
    assert_eq!(transactions[0]["currency"], "USD");
    let split = transactions
        .iter()
        .find(|row| row["id"] == json!(id(101)))
        .unwrap();
    assert_eq!(split["reasons"], json!(["unassigned_posting"]));
    let booking_ids: Vec<_> = split["booking_lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].clone())
        .collect();
    assert_eq!(
        booking_ids,
        vec![json!(id(201)), json!(id(301)), json!(id(302))]
    );
    assert_eq!(split["booking_lines"][0]["vat_percentage"], "0.21");
    assert_eq!(
        split["booking_lines"][0]["vat_amount_transaction_currency"],
        "-1.75"
    );
    assert!(!transactions.iter().any(|row| row["id"] == json!(id(888))));
    for url in requests {
        let query: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        let selection = &query["$select"];
        assert!(!selection.contains("BankAccount"));
        assert!(!selection.contains("IBAN"));
        assert!(!selection.contains("VATAmountDC"));
        assert!(!selection.contains("VATAmountFC"));
        assert!(
            !query
                .get("$filter")
                .is_some_and(|filter| filter.contains("Status"))
        );
        if url.path().ends_with("Journals") {
            assert!(selection.split(',').any(|field| field == "Description"));
            assert!(!selection.contains("JournalDescription"));
        }
    }
}

#[tokio::test]
async fn negative_partial_ar_and_payment_service_journal_are_supported() {
    let mut replies = scenario();
    rows(&mut replies, 1)[0]["Type"] = json!(16);
    rows(&mut replies, 4).clear();
    let mut candidate_reply = replies.remove(5);
    rows(&mut replies, 5).push(report(9, 900, 1005));
    candidate_reply.body["d"]["results"][0]["GLAccount"] = json!(id(21));
    replies.insert(6, candidate_reply);
    rows(&mut replies, 8)[9]["GLAccount"] = json!(id(21));
    let (result, _) = run(replies, args()).await;
    assert_eq!(result["journals"][0]["type_code"], 16);
    assert_eq!(
        result["transactions"][0]["reasons"],
        json!(["outstanding_receivable_posting"])
    );
    assert_eq!(
        result["transactions"][0]["evidence"][0]["outstanding_item"]["amount"],
        "-2.50"
    );
}

#[tokio::test]
async fn ambiguous_reused_bank_entries_are_unresolved_even_with_equal_amounts() {
    let mut replies = scenario();
    let mut other = rows(&mut replies, 5)[0].clone();
    other["ID"] = json!(id(777));
    other["EntryID"] = json!(id(7777));
    other["FinancialYear"] = json!(2024);
    other["OffsetID"] = json!(id(778));
    rows(&mut replies, 5).push(other);
    rows(&mut replies, 7).pop();
    rows(&mut replies, 8).retain(|line| line["EntryID"] != json!(id(1005)));
    let mut selection = args();
    selection.limit = Some(1);
    selection.offset = 1;
    selection.transaction_date_to = Some(NaiveDate::from_ymd_opt(2025, 1, 3).unwrap());
    let (result, _) = run(replies, selection).await;
    assert_eq!(result["reconstructed_transaction_count"], 4);
    assert_eq!(result["matched_transaction_count"], 3);
    assert_eq!(result["transactions"].as_array().unwrap().len(), 1);
    assert_eq!(result["transactions"][0]["id"], json!(id(102)));
    assert_eq!(result["coverage_complete"], false);
    assert_eq!(
        result["unresolved_items"][0]["resolution"],
        "ambiguous_counterparts"
    );
    assert_eq!(
        result["unresolved_items"][0]["candidate_posting_ids"],
        json!([id(205), id(777)])
    );
    assert_eq!(
        result["unresolved_items"][0]["outstanding_item"]["hid"],
        "1"
    );
    assert_eq!(result["has_more"], true);
    assert_eq!(result["next_offset"], 2);
}

#[tokio::test]
async fn missing_counterpart_remains_visible_even_with_empty_date_window() {
    let mut replies = scenario();
    rows(&mut replies, 5).clear();
    rows(&mut replies, 7).pop();
    rows(&mut replies, 8).retain(|line| line["EntryID"] != json!(id(1005)));
    let mut selection = args();
    selection.transaction_date_from = Some(NaiveDate::from_ymd_opt(2099, 1, 1).unwrap());
    let (result, _) = run(replies, selection).await;
    assert_eq!(result["transactions"], json!([]));
    assert_eq!(
        result["unresolved_items"][0]["resolution"],
        "missing_counterpart"
    );
    assert_eq!(result["coverage_complete"], false);
}

#[tokio::test]
async fn stable_sort_unknown_last_inclusive_bounds_and_windows() {
    let mut replies = scenario();
    for index in [7, 8] {
        for line in rows(&mut replies, index).iter_mut() {
            if line["ID"] == json!(id(101)) {
                line["Date"] = Value::Null;
            }
            if line["ID"] == json!(id(103)) {
                line["Date"] = json!("2025-01-02");
            }
        }
    }
    let (result, _) = run(replies, args()).await;
    let ids: Vec<_> = result["transactions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].clone())
        .collect();
    assert_eq!(
        ids,
        vec![
            json!(id(105)),
            json!(id(104)),
            json!(id(102)),
            json!(id(103)),
            json!(id(101))
        ]
    );
    let mut selection = args();
    selection.transaction_date_from = Some(NaiveDate::from_ymd_opt(2025, 1, 2).unwrap());
    selection.transaction_date_to = Some(NaiveDate::from_ymd_opt(2025, 1, 4).unwrap());
    selection.limit = Some(2);
    let (first, _) = run(scenario(), selection).await;
    assert_eq!(first["matched_transaction_count"], 3);
    assert_eq!(first["next_offset"], 2);
    let mut selection = args();
    selection.offset = 4;
    selection.limit = Some(1);
    let (last, _) = run(scenario(), selection).await;
    assert_eq!(last["has_more"], false);
    assert_eq!(last["next_offset"], Value::Null);
}

#[tokio::test]
async fn invalid_inputs_fail_before_any_network() {
    for input in [
        json!({"unassigned_gl_account_ids":[]}),
        json!({"unassigned_gl_account_ids":["bad"]}),
        json!({"unassigned_gl_account_ids":[id(0)]}),
        json!({"unassigned_gl_account_ids":[id(10),id(10)]}),
        json!({"unassigned_gl_account_ids":(1..=11).map(id).collect::<Vec<_>>()}),
        json!({"unassigned_gl_account_ids":[id(10)],"limit":0}),
        json!({"unassigned_gl_account_ids":[id(10)],"limit":101}),
        json!({"unassigned_gl_account_ids":[id(10)],"offset":u32::MAX}),
        json!({"unassigned_gl_account_ids":[id(10)],"journal_code":" "}),
        json!({"unassigned_gl_account_ids":[id(10)],"transaction_date_from":"2025-02-01","transaction_date_to":"2025-01-01"}),
    ] {
        let (_directory, server, client) = fixture(vec![]).await;
        assert!(
            get_transactions_to_handle(
                &client,
                &administration(),
                serde_json::from_value(input).unwrap()
            )
            .await
            .is_err()
        );
        assert!(server.finish().await.is_empty());
    }
    assert!(serde_json::from_value::<GetBankTransactionsToHandle>(json!({})).is_err());
}

#[tokio::test]
async fn selected_gl_metadata_is_required_verified_and_not_own_bank_control() {
    for (case, message, end) in [
        (0, "metadata is missing", 3),
        (1, "wrong-scope G/L", 3),
        (2, "own-bank control", 2),
        (3, "Duplicate G/L", 3),
        (4, "not a verified bank", 2),
    ] {
        let mut replies = scenario();
        let mut selection = args();
        match case {
            0 => {
                rows(&mut replies, 2).remove(0);
            }
            1 => rows(&mut replies, 2)[0]["Division"] = json!(43),
            2 => selection.unassigned_gl_account_ids = vec![id(11).to_string()],
            3 => {
                let copy = rows(&mut replies, 2)[0].clone();
                rows(&mut replies, 2).push(copy);
            }
            4 => selection.journal_code = Some("missing".into()),
            _ => unreachable!(),
        }
        replies.truncate(end);
        fails(replies, selection, message).await;
    }
}

#[tokio::test]
async fn duplicates_wrong_scope_and_disappearing_sources_never_return_partial_results() {
    for (case, message, end) in [
        (0, "Duplicate unassigned", 4),
        (1, "wrong-scope bank posting", 4),
        (2, "Duplicate outstanding report HID", 5),
        (3, "escaped company/entry", 6),
        (4, "Duplicate outstanding counterpart", 6),
        (5, "control is missing", 8),
        (6, "Duplicate bank control", 8),
        (7, "unrequested UUID", 8),
        (8, "evidence posting disappeared", 9),
        (9, "association changed", 9),
        (10, "Duplicate whole-entry", 9),
        (11, "crosses a bank entry", 9),
        (12, "wrong-scope bank journal", 2),
        (13, "Invalid outstanding report", 5),
        (14, "journal's own-bank control", 8),
        (15, "unrequested entry", 9),
    ] {
        let mut replies = scenario();
        match case {
            0 => {
                let copy = rows(&mut replies, 3)[0].clone();
                rows(&mut replies, 3).push(copy);
            }
            1 => rows(&mut replies, 3)[0]["Division"] = json!(43),
            2 => {
                let copy = rows(&mut replies, 4)[0].clone();
                rows(&mut replies, 4).push(copy);
            }
            3 => rows(&mut replies, 5)[0]["Account"] = json!(id(901)),
            4 => {
                let copy = rows(&mut replies, 5)[0].clone();
                rows(&mut replies, 5).push(copy);
            }
            5 => {
                rows(&mut replies, 7).pop();
            }
            6 => {
                let copy = rows(&mut replies, 7)[0].clone();
                rows(&mut replies, 7).push(copy);
            }
            7 => rows(&mut replies, 7)[0]["ID"] = json!(id(999)),
            8 => {
                rows(&mut replies, 8).remove(5);
            }
            9 => rows(&mut replies, 8)[5]["OffsetID"] = json!(id(102)),
            10 => {
                let copy = rows(&mut replies, 8)[0].clone();
                rows(&mut replies, 8).push(copy);
            }
            11 => rows(&mut replies, 8)[5]["EntryNumber"] = json!(999),
            12 => rows(&mut replies, 1)[0]["Type"] = json!(10),
            13 => rows(&mut replies, 4)[0]["CurrencyCode"] = json!(" "),
            14 => rows(&mut replies, 7)[0]["GLAccount"] = json!(id(10)),
            15 => rows(&mut replies, 8)[0]["EntryID"] = json!(id(9999)),
            _ => unreachable!(),
        }
        replies.truncate(end);
        fails(replies, args(), message).await;
    }
}

#[tokio::test]
async fn all_report_pages_are_validated_before_resolving_and_failed_paging_is_fatal() {
    for failed in [false, true] {
        let mut replies = scenario();
        replies[4].body["d"]["__next"] = json!("?page=2");
        let mut second = Reply::rows("read/financial/PayablesList", vec![report(1, 900, 1005)]);
        if failed {
            second.status = 500;
        }
        replies.insert(5, second);
        replies.truncate(6);
        fails(
            replies,
            args(),
            if failed {
                "HTTP 500"
            } else {
                "Duplicate outstanding report HID"
            },
        )
        .await;
    }
}

#[tokio::test]
async fn discovery_pagination_keeps_every_source_and_final_session_scope_is_checked() {
    let mut replies = scenario();
    let more = rows(&mut replies, 3).split_off(2);
    replies[3].body["d"]["__next"] = json!("?page=2");
    replies.insert(
        4,
        Reply::rows("financialtransaction/TransactionLines", more),
    );
    let (result, requests) = run(replies, args()).await;
    assert_eq!(result["reconstructed_transaction_count"], 5);
    assert_eq!(requests.len(), 10);
    let mut replies = scenario();
    replies.last_mut().unwrap().change_administration = true;
    fails(replies, args(), "Account or administration changed").await;
}

#[tokio::test]
async fn missing_offsets_are_not_hidden_before_ap_ar_identity_resolution() {
    for ambiguous in [false, true] {
        let mut replies = scenario();
        if ambiguous {
            let mut unlinked = rows(&mut replies, 5)[0].clone();
            unlinked["ID"] = json!(id(777));
            unlinked["EntryID"] = json!(id(7777));
            unlinked["FinancialYear"] = json!(2024);
            unlinked["OffsetID"] = Value::Null;
            rows(&mut replies, 5).push(unlinked);
        } else {
            rows(&mut replies, 5)[0]["OffsetID"] = Value::Null;
        }
        rows(&mut replies, 5)[1]["OffsetID"] = Value::Null;
        rows(&mut replies, 7).pop();
        rows(&mut replies, 8).retain(|line| line["EntryID"] != json!(id(1005)));
        let (result, requests) = run(replies, args()).await;
        assert_eq!(result["coverage_complete"], false);
        assert_eq!(result["reconstructed_transaction_count"], 4);
        assert_eq!(
            result["unresolved_items"][0]["resolution"],
            if ambiguous {
                "ambiguous_counterparts"
            } else {
                "missing_offset"
            }
        );
        assert_eq!(
            result["unresolved_items"][0]["candidate_posting_ids"]
                .as_array()
                .unwrap()
                .len(),
            if ambiguous { 2 } else { 1 }
        );
        for url in requests {
            assert!(
                !url.query_pairs()
                    .any(|(key, value)| key == "$filter" && value.contains("OffsetID ne null"))
            );
        }
    }
    let mut replies = scenario();
    rows(&mut replies, 3)[0]["OffsetID"] = Value::Null;
    replies.truncate(4);
    fails(replies, args(), "configured G/L/offset scope").await;
}

#[tokio::test]
async fn whole_entry_pages_validate_even_discarded_movements_without_partial_success() {
    for case in 0..4 {
        let mut replies = scenario();
        let mut more = rows(&mut replies, 8).split_off(12);
        replies[8].body["d"]["__next"] = json!("?page=2");
        match case {
            0 => {}
            1 => more.push(more[0].clone()),
            2 => more[0]["Division"] = json!(43),
            3 => {}
            _ => unreachable!(),
        }
        let mut next = Reply::rows("financialtransaction/TransactionLines", more);
        if case == 3 {
            next.status = 500;
        }
        replies.push(next);
        match case {
            0 => {
                let (result, requests) = run(replies, args()).await;
                assert_eq!(result["matched_transaction_count"], 5);
                assert_eq!(requests.len(), 10);
            }
            1 => fails(replies, args(), "Duplicate whole-entry").await,
            2 => fails(replies, args(), "wrong-scope bank posting").await,
            3 => fails(replies, args(), "HTTP 500").await,
            _ => unreachable!(),
        }
    }
}

#[tokio::test]
async fn source_status_nulls_and_exact_numeric_precision_are_preserved_not_predicates() {
    let mut replies = scenario();
    for index in [3, 5, 7, 8] {
        for row in rows(&mut replies, index) {
            row["Status"] = json!(50);
            if row["ID"] == json!(id(105)) {
                row["AmountDC"] = serde_json::from_str("-9007199254740993.0100").unwrap();
                row["AmountFC"] = Value::Null;
                row["Currency"] = Value::Null;
            }
        }
    }
    let (result, _) = run(replies, args()).await;
    assert_eq!(result["matched_transaction_count"], 5);
    assert_eq!(result["transactions"][0]["status_code"], 50);
    assert_eq!(
        result["transactions"][0]["amount_default_currency"],
        "-9007199254740993.0100"
    );
    assert_eq!(
        result["transactions"][0]["amount_transaction_currency"],
        Value::Null
    );
    assert_eq!(result["transactions"][0]["currency"], Value::Null);
}

#[tokio::test]
async fn control_and_entry_identity_filters_are_bounded_batches() {
    let mut replies = scenario();
    rows(&mut replies, 4).clear();
    replies.remove(5);
    let mut sources = Vec::new();
    let mut controls = Vec::new();
    for index in 1..=21 {
        sources.push(posting(200 + index, 1000 + index, 10, Some(100 + index)));
        controls.push(posting(100 + index, 1000 + index, 11, None));
    }
    *rows(&mut replies, 3) = sources.clone();
    replies.truncate(6);
    for batch in controls.chunks(BATCH_SIZE) {
        replies.push(Reply::rows(
            "financialtransaction/TransactionLines",
            batch.to_vec(),
        ));
    }
    for batch in controls.chunks(BATCH_SIZE) {
        let mut entries = batch.to_vec();
        entries.extend(
            sources
                .iter()
                .filter(|line| {
                    batch
                        .iter()
                        .any(|control| control["EntryID"] == line["EntryID"])
                })
                .cloned(),
        );
        replies.push(Reply::rows(
            "financialtransaction/TransactionLines",
            entries,
        ));
    }
    let (result, requests) = run(replies, args()).await;
    assert_eq!(result["matched_transaction_count"], 21);
    for url in &requests[6..] {
        let filter = url
            .query_pairs()
            .find(|(key, _)| key == "$filter")
            .unwrap()
            .1;
        assert!(filter.matches(" eq guid'").count() <= BATCH_SIZE);
        assert!(filter.contains("ID eq guid'"));
    }
}

#[tokio::test]
async fn final_entry_new_counterparts_disprove_previously_unique_report_evidence() {
    for offset in [Some(105), None, Some(104)] {
        let mut replies = scenario();
        let mut new_counterpart = posting(777, 1005, 20, offset);
        new_counterpart["Account"] = json!(id(900));
        rows(&mut replies, 8).push(new_counterpart);
        fails(
            replies,
            args(),
            "source became ambiguous during reconstruction; retry",
        )
        .await;
    }
}

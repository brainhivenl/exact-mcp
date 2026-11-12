use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use super::*;
use crate::financial::Backend;
use crate::store::{Session, Store, Tokens};

pub(super) const ACCOUNT: &str = "11111111-1111-1111-1111-111111111111";
const ORDERED_BY: &str = "22222222-2222-2222-2222-222222222222";
pub(super) const INVOICE: &str = "33333333-3333-3333-3333-333333333333";
const OTHER_INVOICE: &str = "44444444-4444-4444-4444-444444444444";
pub(super) const LINE: &str = "55555555-5555-5555-5555-555555555555";
const OTHER_LINE: &str = "66666666-6666-6666-6666-666666666666";

pub(super) struct Response {
    pub(super) status: u16,
    pub(super) body: Value,
}

impl Response {
    pub(super) fn rows(rows: Vec<Value>) -> Self {
        Self {
            status: 200,
            body: json!({"d": rows}),
        }
    }

    fn page(rows: Vec<Value>, next: &str) -> Self {
        Self {
            status: 200,
            body: json!({"d": {"results": rows, "__next": next}}),
        }
    }

    fn denied() -> Self {
        Self {
            status: 403,
            body: json!({"error": "denied"}),
        }
    }
}

pub(super) struct Server {
    base: String,
    pub(super) requests: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl Server {
    pub(super) async fn new(responses: Vec<Response>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let mut responses: VecDeque<_> = responses.into();
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut buffer = [0; 4096];
                    let count = socket.read(&mut buffer).await.unwrap();
                    if count == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buffer[..count]);
                    if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                recorded
                    .lock()
                    .unwrap()
                    .push(String::from_utf8(bytes).unwrap());
                let response = responses.pop_front().unwrap_or_else(Response::denied);
                if response.status == 0 {
                    continue;
                }
                let body = response.body.to_string();
                let reply = format!(
                    "HTTP/1.1 {} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.status,
                    body.len(),
                    body
                );
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        Self {
            base,
            requests,
            task,
        }
    }

    fn urls(&self) -> Vec<reqwest::Url> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| {
                let mut words = request.split_whitespace();
                assert_eq!(words.next(), Some("GET"));
                reqwest::Url::parse(&format!("{}{}", self.base, words.next().unwrap())).unwrap()
            })
            .collect()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(super) async fn backend(server: &Server) -> (TempDir, Backend) {
    let directory = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let store = Store::open(directory.path()).await.unwrap();
    store
        .save(&Session {
            base_url: server.base.clone(),
            client_id: "invented-client".into(),
            redirect_uri: "http://localhost/callback".into(),
            tokens: Tokens {
                access_token: "invented-access-token".into(),
                refresh_token: "invented-refresh-token".into(),
                expires_at: crate::oauth::now().unwrap() + 3600,
            },
            administration: Some(Administration {
                code: 42,
                description: "Invented administration".into(),
            }),
        })
        .await
        .unwrap();
    drop(store);
    let backend = Backend::new(directory.path().to_owned()).await.unwrap();
    (directory, backend)
}

fn account_response() -> Response {
    Response::rows(vec![
        json!({"ID": ACCOUNT, "Division": 42, "Code": " 0012 ", "Name": "Invented bill-to"}),
    ])
}

fn currency_response() -> Response {
    Response::rows(vec![json!({"Code": 42, "Currency": "EUR"})])
}

fn header(id: &str, status: Option<i16>, date: Option<&str>) -> Value {
    json!({
        "InvoiceID": id, "Division": 42, "InvoiceTo": ACCOUNT,
        "InvoiceToName": "Invented bill-to", "OrderedBy": ORDERED_BY,
        "OrderedByName": "Invented ordering company", "InvoiceNumber": null,
        "InvoiceDate": date, "DueDate": null, "Journal": "70",
        "Description": "Invented <untrusted> description", "YourRef": "00123-ÉXAMPLE",
        "Status": status, "StatusDescription": null, "Type": 8021,
        "TypeDescription": "Sales credit note", "Currency": "USD",
        "AmountDC": -12.345600, "AmountFC": -13.21, "AmountFCExclVat": -10,
        "VATAmountDC": null, "VATAmountFC": -3.21
    })
}

fn line(id: &str, number: Option<i32>) -> Value {
    json!({
        "ID": id, "InvoiceID": INVOICE, "Division": 42, "LineNumber": number,
        "Description": "Invented line", "Item": null, "GLAccount": null,
        "Quantity": -1.23456789, "UnitCode": "piece", "UnitPrice": 10.12345678,
        "NetPrice": 9.12345678, "Discount": 0.1, "VATPercentage": 0.21,
        "AmountDC": -12.3456, "AmountFC": -10, "VATAmountDC": null,
        "VATAmountFC": -2.1
    })
}

fn search_args() -> SearchDraftInvoices {
    serde_json::from_value(json!({"account_id": ACCOUNT})).unwrap()
}

fn details_args() -> GetDraftInvoiceDetails {
    GetDraftInvoiceDetails {
        account_id: ACCOUNT.into(),
        invoice_id: INVOICE.into(),
    }
}

async fn search_value(responses: Vec<Response>, args: SearchDraftInvoices) -> Value {
    let server = Server::new(responses).await;
    let (_directory, backend) = backend(&server).await;
    let result = serde_json::to_value(backend.search_draft_invoices(args).await.unwrap()).unwrap();
    server.urls();
    result
}

async fn search_error(responses: Vec<Response>, args: SearchDraftInvoices) -> String {
    let server = Server::new(responses).await;
    let (_directory, backend) = backend(&server).await;
    let result = backend.search_draft_invoices(args).await;
    server.urls();
    format!(
        "{:#}",
        result
            .err()
            .expect("search must fail without partial results")
    )
}

async fn details_error(responses: Vec<Response>) -> String {
    let server = Server::new(responses).await;
    let (_directory, backend) = backend(&server).await;
    let result = backend.get_draft_invoice_details(details_args()).await;
    server.urls();
    format!(
        "{:#}",
        result
            .err()
            .expect("details must fail without partial results")
    )
}

#[tokio::test]
async fn search_reads_every_page_before_filter_sort_and_pagination() {
    for offset in 0..=2 {
        let mut args = search_args();
        args.offset = offset;
        args.limit = Some(1);
        let result = search_value(
            vec![
                account_response(),
                currency_response(),
                Response::page(
                    vec![header(INVOICE, Some(10), Some("2025-01-01"))],
                    "?page=2",
                ),
                Response::page(
                    vec![header(OTHER_INVOICE, Some(20), Some("2025-02-01"))],
                    "?page=3",
                ),
                Response::rows(vec![
                    header(LINE, Some(50), Some("2025-03-01")),
                    header(OTHER_LINE, None, None),
                ]),
            ],
            args,
        )
        .await;
        assert_eq!(result["source_invoice_count"], 4);
        assert_eq!(result["matched_invoice_count"], 2);
        assert_eq!(result["company_role"], "invoice_to_bill_to_not_ordered_by");
        assert_eq!(result["account"]["id"], ACCOUNT);
        assert_eq!(result["default_currency"], "EUR");
        if offset == 0 {
            assert_eq!(result["invoices"][0]["invoice_id"], OTHER_INVOICE);
            assert_eq!(result["invoices"][0]["status_code"], 20);
            assert_eq!(result["next_offset"], 1);
        } else if offset == 1 {
            assert_eq!(result["invoices"][0]["invoice_id"], INVOICE);
            assert_eq!(result["invoices"][0]["status_code"], 10);
            assert!(result["next_offset"].is_null());
        } else {
            assert_eq!(result["invoices"], json!([]));
        }
        assert_eq!(result["has_more"], offset == 0);
        assert!(result["status_filter"].is_null());
        let text = result["interpretation"].as_str().unwrap();
        assert!(text.contains("recovery draft") && text.contains("20 is open/editable"));
    }
}

#[tokio::test]
async fn search_scopes_to_bill_to_without_ordered_by_or_payment_filters() {
    let server = Server::new(vec![
        account_response(),
        currency_response(),
        Response::rows(vec![header(INVOICE, Some(20), None)]),
    ])
    .await;
    let (_directory, backend) = backend(&server).await;
    let result =
        serde_json::to_value(backend.search_draft_invoices(search_args()).await.unwrap()).unwrap();
    assert_eq!(result["invoices"][0]["invoice_to"], ACCOUNT);
    assert_eq!(result["invoices"][0]["ordered_by"], ORDERED_BY);
    let urls = server.urls();
    assert_eq!(urls.len(), 3);
    assert_eq!(urls[2].path(), "/api/v1/42/salesinvoice/SalesInvoices");
    let pairs: std::collections::HashMap<_, _> = urls[2].query_pairs().collect();
    assert_eq!(pairs["$filter"], format!("InvoiceTo eq guid'{ACCOUNT}'"));
    assert_eq!(pairs["$select"], HEADER_FIELDS);
    assert!(!pairs.contains_key("$top"));
    assert!(!pairs.contains_key("$skip"));
}

#[tokio::test]
async fn search_status_date_and_literal_query_filters_preserve_reference_zeroes() {
    for (query, expected) in [
        (" éxample ", 2),
        ("00123", 2),
        ("123", 2),
        ("00124", 0),
        ("%", 0),
        ("700", 1),
        ("0700", 0),
    ] {
        let mut args = search_args();
        args.query = Some(query.into());
        args.invoice_date_from = Some("2025-01-01".parse().unwrap());
        args.invoice_date_to = Some("2025-12-31".parse().unwrap());
        args.status = Some(20);
        let mut numbered = header(INVOICE, Some(20), Some("2025-01-01"));
        numbered["InvoiceNumber"] = json!(700);
        let result = search_value(
            vec![
                account_response(),
                currency_response(),
                Response::rows(vec![
                    numbered,
                    header(OTHER_INVOICE, Some(20), Some("2025-12-31")),
                    header(LINE, Some(10), Some("2025-06-01")),
                    header(OTHER_LINE, Some(20), None),
                    header(ORDERED_BY, Some(20), Some("2024-12-31")),
                ]),
            ],
            args,
        )
        .await;
        assert_eq!(result["matched_invoice_count"], expected, "query {query}");
        assert_eq!(result["status_filter"], 20);
    }
    let mut args = search_args();
    args.status = Some(10);
    let result = search_value(
        vec![
            account_response(),
            currency_response(),
            Response::rows(vec![
                header(INVOICE, Some(10), None),
                header(OTHER_INVOICE, Some(20), None),
            ]),
        ],
        args,
    )
    .await;
    assert_eq!(result["matched_invoice_count"], 1);
    assert_eq!(result["invoices"][0]["status_code"], 10);
}

#[tokio::test]
async fn search_stable_ties_and_unknown_dates_last() {
    let mut numbered = header(INVOICE, Some(20), Some("2025-01-01"));
    numbered["InvoiceNumber"] = json!(8);
    let result = search_value(
        vec![
            account_response(),
            currency_response(),
            Response::rows(vec![
                header(LINE, Some(20), None),
                header(OTHER_INVOICE, Some(20), Some("2025-01-01")),
                header(ORDERED_BY, Some(20), Some("2025-01-01")),
                numbered,
            ]),
        ],
        search_args(),
    )
    .await;
    let ids: Vec<_> = result["invoices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["invoice_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [INVOICE, ORDERED_BY, OTHER_INVOICE, LINE]);
}

#[tokio::test]
async fn search_empty_is_success_not_payment_evidence() {
    let result = search_value(
        vec![
            account_response(),
            currency_response(),
            Response::rows(vec![]),
        ],
        search_args(),
    )
    .await;
    assert_eq!(result["matched_invoice_count"], 0);
    assert_eq!(result["invoices"], json!([]));
    assert_eq!(result["has_more"], false);
    assert!(result["paid"].is_null());
}

#[tokio::test]
async fn details_accept_both_statuses_read_all_lines_and_preserve_source_values() {
    for status in [10, 20] {
        let server = Server::new(vec![
            account_response(),
            Response::rows(vec![header(
                INVOICE,
                Some(status),
                Some("/Date(1735689600000)/"),
            )]),
            currency_response(),
            Response::page(vec![line(OTHER_LINE, None)], "?page=2"),
            Response::rows(vec![line(LINE, Some(1))]),
        ])
        .await;
        let (_directory, backend) = backend(&server).await;
        let result = serde_json::to_value(
            backend
                .get_draft_invoice_details(details_args())
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["invoice"]["status_code"], status);
        assert_eq!(result["invoice"]["invoice_date"], "2025-01-01");
        assert_eq!(result["invoice"]["ordered_by"], ORDERED_BY);
        assert_eq!(result["invoice"]["currency"], "USD");
        assert_eq!(result["invoice"]["amount_default_currency"], "-12.3456");
        assert_eq!(result["invoice"]["amount_transaction_currency"], "-13.21");
        assert_eq!(
            result["invoice"]["amount_excluding_vat_transaction_currency"],
            "-10"
        );
        assert!(result["invoice"]["vat_amount_default_currency"].is_null());
        assert!(result["invoice"]["invoice_number"].is_null());
        assert_eq!(result["invoice"]["type_code"], 8021);
        assert_eq!(result["line_count"], 2);
        assert_eq!(result["lines"][0]["id"], LINE);
        assert_eq!(result["lines"][0]["quantity"], "-1.23456789");
        assert_eq!(result["lines"][0]["unit_price"], "10.12345678");
        assert_eq!(result["lines"][0]["net_price"], "9.12345678");
        assert_eq!(result["lines"][0]["vat_percentage"], "0.21");
        assert_eq!(result["lines"][0]["discount"], "0.1");
        assert!(result["lines"][0]["item_id"].is_null());
        assert!(result["lines"][0]["vat_amount_default_currency"].is_null());
        let urls = server.urls();
        assert_eq!(urls.len(), 5);
        let header_filter = urls[1]
            .query_pairs()
            .find(|(name, _)| name == "$filter")
            .unwrap()
            .1;
        assert_eq!(
            header_filter,
            format!("InvoiceID eq guid'{INVOICE}' and InvoiceTo eq guid'{ACCOUNT}'")
        );
        assert_eq!(urls[3].path(), "/api/v1/42/salesinvoice/SalesInvoiceLines");
        let line_filter = urls[3]
            .query_pairs()
            .find(|(name, _)| name == "$filter")
            .unwrap()
            .1;
        assert_eq!(line_filter, format!("InvoiceID eq guid'{INVOICE}'"));
    }
}

#[tokio::test]
async fn details_preserve_null_currency_amounts_status_description_and_empty_lines() {
    let mut invoice = header(INVOICE, Some(20), None);
    for field in [
        "Currency",
        "AmountDC",
        "AmountFC",
        "AmountFCExclVat",
        "VATAmountFC",
        "OrderedBy",
        "Journal",
    ] {
        invoice[field] = Value::Null;
    }
    let server = Server::new(vec![
        account_response(),
        Response::rows(vec![invoice]),
        currency_response(),
        Response::rows(vec![]),
    ])
    .await;
    let (_directory, backend) = backend(&server).await;
    let result = serde_json::to_value(
        backend
            .get_draft_invoice_details(details_args())
            .await
            .unwrap(),
    )
    .unwrap();
    for field in [
        "currency",
        "amount_default_currency",
        "amount_transaction_currency",
        "amount_excluding_vat_transaction_currency",
        "vat_amount_transaction_currency",
        "ordered_by",
        "journal_code",
        "status_description",
    ] {
        assert!(result["invoice"][field].is_null(), "{field}");
    }
    assert_eq!(result["line_count"], 0);
    assert_eq!(result["lines"], json!([]));
    server.urls();
}

#[tokio::test]
async fn invalid_arguments_fail_before_network() {
    let server = Server::new(vec![]).await;
    let (_directory, backend) = backend(&server).await;
    for patch in [
        json!({"account_id": "not-a-uuid"}),
        json!({"account_id": Uuid::nil().to_string()}),
        json!({"status": 50}),
        json!({"status": 0}),
        json!({"limit": 0}),
        json!({"limit": 51}),
        json!({"offset": u32::MAX}),
        json!({"query": " "}),
        json!({"query": "x".repeat(101)}),
        json!({"invoice_date_from": "2025-02-01", "invoice_date_to": "2025-01-01"}),
    ] {
        let mut args = json!({"account_id": ACCOUNT});
        for (key, value) in patch.as_object().unwrap() {
            args[key] = value.clone();
        }
        assert!(
            backend
                .search_draft_invoices(serde_json::from_value(args).unwrap())
                .await
                .is_err()
        );
    }
    for (account, invoice) in [
        ("bad", INVOICE),
        (ACCOUNT, "bad"),
        (ACCOUNT, "00000000-0000-0000-0000-000000000000"),
    ] {
        assert!(
            backend
                .get_draft_invoice_details(GetDraftInvoiceDetails {
                    account_id: account.into(),
                    invoice_id: invoice.into()
                })
                .await
                .is_err()
        );
    }
    assert!(server.urls().is_empty());
}

#[test]
fn arguments_reject_unknown_fields_invalid_dates_and_wrong_types() {
    assert!(
        serde_json::from_value::<SearchDraftInvoices>(
            json!({"account_id": ACCOUNT, "side": "receivable"})
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<SearchDraftInvoices>(
            json!({"account_id": ACCOUNT, "invoice_date_from": "2025-02-30"})
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<SearchDraftInvoices>(
            json!({"account_id": ACCOUNT, "status": "20"})
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<GetDraftInvoiceDetails>(
            json!({"account_id": ACCOUNT, "entry_id": INVOICE})
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<GetDraftInvoiceDetails>(
            json!({"account_id": ACCOUNT, "invoice_id": INVOICE, "print": true})
        )
        .is_err()
    );
}

#[tokio::test]
async fn account_and_currency_must_be_unique_and_in_scope() {
    for response in [
        Response::rows(vec![]),
        Response::rows(vec![json!({"ID": ACCOUNT, "Division": 43, "Code": "1"})]),
        Response::rows(vec![json!({"ID": ORDERED_BY, "Division": 42, "Code": "1"})]),
        Response::page(
            vec![json!({"ID": ACCOUNT, "Division": 42, "Code": "1"})],
            "?page=2",
        ),
    ] {
        let error = search_error(vec![response, account_response()], search_args()).await;
        assert!(error.contains("not found uniquely"), "{error}");
    }
    for response in [
        Response::rows(vec![]),
        Response::rows(vec![json!({"Code": 43, "Currency": "EUR"})]),
        Response::rows(vec![json!({"Code": 42, "Currency": " "})]),
        Response::rows(vec![
            json!({"Code": 42, "Currency": "EUR"}),
            json!({"Code": 42, "Currency": "EUR"}),
        ]),
    ] {
        let error = search_error(vec![account_response(), response], search_args()).await;
        assert!(error.contains("currency"), "{error}");
    }
}

#[tokio::test]
async fn search_validates_even_hidden_or_processed_source_rows() {
    for (field, value, expected) in [
        ("InvoiceID", json!(Uuid::nil()), "wrong-scope"),
        ("Division", json!(43), "wrong-scope"),
        ("InvoiceTo", json!(ORDERED_BY), "wrong-scope"),
        ("InvoiceID", json!(INVOICE), "Duplicate"),
    ] {
        let mut invalid = header(OTHER_INVOICE, Some(50), None);
        invalid[field] = value;
        let error = search_error(
            vec![
                account_response(),
                currency_response(),
                Response::page(vec![header(INVOICE, Some(20), None)], "?page=2"),
                Response::rows(vec![invalid]),
            ],
            search_args(),
        )
        .await;
        assert!(error.contains(expected), "{field}: {error}");
    }
}

#[tokio::test]
async fn details_reject_missing_ambiguous_processed_and_wrong_scope_headers() {
    for rows in [
        vec![],
        vec![
            header(INVOICE, Some(20), None),
            header(INVOICE, Some(20), None),
        ],
    ] {
        let error = details_error(vec![account_response(), Response::rows(rows)]).await;
        assert!(error.contains("not found uniquely"), "{error}");
    }
    let error = details_error(vec![
        account_response(),
        Response::page(vec![header(INVOICE, Some(20), None)], "?page=2"),
        Response::rows(vec![header(INVOICE, Some(20), None)]),
    ])
    .await;
    assert!(error.contains("not found uniquely"));
    for status in [Some(50), Some(0), None] {
        let error = details_error(vec![
            account_response(),
            Response::rows(vec![header(INVOICE, status, None)]),
        ])
        .await;
        assert!(error.contains("not status 10"), "{error}");
    }
    for (field, value) in [
        ("InvoiceID", json!(OTHER_INVOICE)),
        ("Division", json!(43)),
        ("InvoiceTo", json!(ORDERED_BY)),
        ("InvoiceID", json!(Uuid::nil())),
    ] {
        let mut invalid = header(INVOICE, Some(20), None);
        invalid[field] = value;
        let error = details_error(vec![account_response(), Response::rows(vec![invalid])]).await;
        assert!(
            error.contains("wrong-scope") || error.contains("different commercial invoice"),
            "{field}: {error}"
        );
    }
}

#[tokio::test]
async fn details_reject_wrong_scope_or_duplicate_lines_across_pages() {
    for (field, value, expected) in [
        ("ID", json!(Uuid::nil()), "wrong-scope"),
        ("Division", json!(43), "wrong-scope"),
        ("InvoiceID", json!(OTHER_INVOICE), "wrong-scope"),
        ("ID", json!(LINE), "Duplicate"),
    ] {
        let mut invalid = line(OTHER_LINE, Some(2));
        invalid[field] = value;
        let error = details_error(vec![
            account_response(),
            Response::rows(vec![header(INVOICE, Some(20), None)]),
            currency_response(),
            Response::page(vec![line(LINE, Some(1))], "?page=2"),
            Response::rows(vec![invalid]),
        ])
        .await;
        assert!(error.contains(expected), "{field}: {error}");
    }
}

#[tokio::test]
async fn source_failures_never_return_partial_search_or_details() {
    for responses in [
        vec![Response::denied()],
        vec![account_response(), Response::denied()],
        vec![account_response(), currency_response(), Response::denied()],
        vec![
            account_response(),
            currency_response(),
            Response::page(vec![header(INVOICE, Some(20), None)], "?page=2"),
            Response::denied(),
        ],
    ] {
        let error = search_error(responses, search_args()).await;
        assert!(error.contains("403"), "{error}");
    }
    for responses in [
        vec![Response::denied()],
        vec![account_response(), Response::denied()],
        vec![
            account_response(),
            Response::page(vec![header(INVOICE, Some(20), None)], "?page=2"),
            Response::denied(),
        ],
        vec![
            account_response(),
            Response::rows(vec![header(INVOICE, Some(20), None)]),
            Response::denied(),
        ],
        vec![
            account_response(),
            Response::rows(vec![header(INVOICE, Some(20), None)]),
            currency_response(),
            Response::page(vec![line(LINE, Some(1))], "?page=2"),
            Response::denied(),
        ],
    ] {
        let error = details_error(responses).await;
        assert!(error.contains("403"), "{error}");
    }
}

#[tokio::test]
async fn malformed_source_rows_never_return_partial_results() {
    for (field, value) in [
        ("AmountDC", json!("invalid-money")),
        ("InvoiceDate", json!("invalid-date")),
        ("InvoiceTo", Value::Null),
        ("InvoiceID", Value::Null),
    ] {
        let mut invalid = header(OTHER_INVOICE, Some(20), None);
        invalid[field] = value;
        let error = search_error(
            vec![
                account_response(),
                currency_response(),
                Response::page(vec![header(INVOICE, Some(20), None)], "?page=2"),
                Response::rows(vec![invalid]),
            ],
            search_args(),
        )
        .await;
        assert!(
            error.contains("Invalid Exact API response"),
            "{field}: {error}"
        );
    }
    let mut invalid = line(OTHER_LINE, Some(2));
    invalid["Quantity"] = json!("not-a-number");
    let error = details_error(vec![
        account_response(),
        Response::rows(vec![header(INVOICE, Some(20), None)]),
        currency_response(),
        Response::page(vec![line(LINE, Some(1))], "?page=2"),
        Response::rows(vec![invalid]),
    ])
    .await;
    assert!(error.contains("Invalid Exact API response"), "{error}");
}

#[tokio::test]
async fn pagination_loops_and_scope_changes_fail_without_partial_results() {
    for next in [
        "?page=2",
        "/api/v1/43/salesinvoice/SalesInvoices?page=2",
        "https://example.invalid/api/v1/42/salesinvoice/SalesInvoices?page=2",
    ] {
        let error = search_error(
            vec![
                account_response(),
                currency_response(),
                Response::page(vec![header(INVOICE, Some(20), None)], next),
                Response::page(vec![], next),
            ],
            search_args(),
        )
        .await;
        assert!(
            error.contains("pagination loop") || error.contains("outside this account"),
            "{error}"
        );
    }
}

#[tokio::test]
async fn search_large_unsorted_pages_keep_global_window_and_counts() {
    let mut responses = vec![account_response(), currency_response()];
    for page in 0..20 {
        let rows = (page * 500..(page + 1) * 500)
            .map(|number| {
                let id = Uuid::from_u128(1000 + number);
                let mut invoice = header(
                    &id.to_string(),
                    Some(if number % 2 == 0 { 10 } else { 20 }),
                    Some("2025-01-01"),
                );
                invoice["InvoiceNumber"] = json!(number);
                invoice
            })
            .collect();
        responses.push(Response::page(rows, &format!("?page={}", page + 2)));
    }
    responses.push(Response::rows(
        (0..1000)
            .map(|number| header(&Uuid::from_u128(20000 + number).to_string(), Some(50), None))
            .collect(),
    ));
    let mut args = search_args();
    args.limit = Some(3);
    args.offset = 4;
    let result = search_value(responses, args).await;
    assert_eq!(result["source_invoice_count"], 11000);
    assert_eq!(result["matched_invoice_count"], 10000);
    assert_eq!(result["next_offset"], 7);
    assert_eq!(result["has_more"], true);
    let numbers: Vec<_> = result["invoices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["invoice_number"].as_u64().unwrap())
        .collect();
    assert_eq!(numbers, [9995, 9994, 9993]);
}

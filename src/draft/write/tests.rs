use super::*;
use crate::draft::tests::{ACCOUNT, INVOICE, LINE, Response, Server, backend};

const ITEM: &str = "77777777-7777-7777-7777-777777777777";
const GL: &str = "88888888-8888-8888-8888-888888888888";
const VAT: &str = "99999999-9999-9999-9999-999999999999";
const MODIFIED: &str = "/Date(1760000000123)/";

fn line_args() -> Value {
    json!({"item_id": ITEM, "gl_account_id": GL, "description": "Custom consulting",
        "quantity": "1.25", "unit_code": "hour", "unit_price": "10.12345678", "vat_code": "1"})
}

fn create_args() -> Value {
    json!({"request_id": INVOICE, "account_id": ACCOUNT, "ordered_by_id": ACCOUNT,
        "journal_code": "70", "invoice_date": "2025-01-01", "currency": "EUR",
        "lines": [line_args()]})
}

fn customer_response() -> Response {
    Response::rows(vec![json!({"ID": ACCOUNT, "Division":42, "Code":"1001",
        "Name":"Test client", "Blocked":false, "IsSales":true})])
}

fn catalogs() -> Vec<Response> {
    vec![
        Response::rows(vec![json!({"ID":"aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            "Division":42, "Code":"70", "Type":20, "IsBlocked":false,
            "Currency":"EUR", "AllowVariableCurrency":false})]),
        Response::rows(vec![json!({"ID":VAT, "Division":42, "Code":"1",
            "Type":"E", "VATTransactionType":"S", "IsBlocked":false, "Percentage":0.21})]),
        Response::rows(vec![]),
        Response::rows(vec![
            json!({"ID":GL, "Division":42, "Code":"8000", "Type":110,
            "IsBlocked":false, "UseCostcenter":0, "UseCostunit":0}),
        ]),
        Response::rows(vec![json!({"ID":ITEM, "Division":42, "Code":"SERVICE",
            "IsSalesItem":true, "IsFractionAllowedItem":true, "Unit":"hour"})]),
    ]
}

fn header() -> Value {
    json!({"InvoiceID": INVOICE, "Division":42, "InvoiceTo":ACCOUNT, "OrderedBy":ACCOUNT,
        "Journal":"70", "Currency":"EUR", "InvoiceDate":"2025-01-01",
        "Modified":MODIFIED, "Status":20, "Type":8020,
        "AmountFC":15.31, "AmountFCExclVat":12.65, "VATAmountFC":2.66})
}

fn saved_line() -> Value {
    json!({"ID":LINE, "InvoiceID":INVOICE, "Division":42, "LineNumber":1,
        "Item":ITEM, "GLAccount":GL, "Description":"Custom consulting",
        "Quantity":1.25, "UnitCode":"hour", "UnitPrice":10.12345678, "VATCode":"1", "Discount":0,
        "AmountFC":12.65, "VATAmountFC":2.66})
}

fn details(header: Value, lines: Vec<Value>) -> Vec<Response> {
    vec![
        customer_response(),
        Response::rows(vec![header]),
        Response::rows(vec![json!({"Code":42, "Currency":"EUR"})]),
        Response::rows(lines),
    ]
}

fn create_sources() -> Vec<Response> {
    let mut responses = vec![Response::rows(vec![]), customer_response()];
    responses.extend(catalogs());
    responses
}

fn writes(server: &Server) -> Vec<String> {
    server
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| !r.starts_with("GET "))
        .cloned()
        .collect()
}

fn body(request: &str) -> Value {
    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
}

#[tokio::test]
async fn create_saves_editable_draft_once_and_verifies_decimal_lines() {
    let mut replies = create_sources();
    replies.push(Response {
        status: 201,
        body: json!({"d": {"InvoiceID":INVOICE}}),
    });
    replies.extend(details(header(), vec![saved_line()]));
    let server = Server::new(replies).await;
    let (_dir, backend) = backend(&server).await;
    let result = backend
        .create_draft_invoice(serde_json::from_value(create_args()).unwrap())
        .await
        .unwrap();
    assert_eq!(result["status"], "created");
    assert_eq!(result["verified"], true);
    assert_eq!(
        result["details"]["invoice"]["amount_transaction_currency"],
        "15.31"
    );
    let requests = writes(&server);
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("POST /api/v1/42/salesinvoice/SalesInvoices "));
    let payload = body(&requests[0]);
    assert_eq!(payload["InvoiceID"], INVOICE);
    assert_eq!(payload["Status"], 20);
    assert_eq!(payload["Type"], 8020);
    assert_eq!(
        payload["SalesInvoiceLines"][0]["UnitPrice"].to_string(),
        "10.12345678"
    );
    assert_eq!(
        payload["SalesInvoiceLines"][0]["Quantity"].to_string(),
        "1.25"
    );
    assert_eq!(payload["SalesInvoiceLines"][0]["Discount"], 0);
    for forbidden in ["InvoiceNumber", "AmountFC", "VATAmountFC", "Document"] {
        assert!(payload.get(forbidden).is_none());
    }
    assert!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|r| !r.contains("Print") && !r.contains("Process"))
    );
    backend.shutdown().await;
}

#[tokio::test]
async fn dry_run_validates_without_a_write_or_authoritative_tax_total() {
    let server = Server::new(create_sources()).await;
    let (_dir, backend) = backend(&server).await;
    let mut args = create_args();
    args["dry_run"] = json!(true);
    let result = backend
        .create_draft_invoice(serde_json::from_value(args).unwrap())
        .await
        .unwrap();
    assert_eq!(result["status"], "preview");
    assert_eq!(result["write_attempted"], false);
    assert_eq!(result["payload"]["SalesInvoiceLines"][0]["Item"], ITEM);
    assert!(result["payload"].get("VATAmountFC").is_none());
    assert!(writes(&server).is_empty());
}

#[tokio::test]
async fn existing_request_id_never_posts_again() {
    let server = Server::new(vec![Response::rows(vec![
        json!({"InvoiceID":INVOICE,"Division":42}),
    ])])
    .await;
    let (_dir, backend) = backend(&server).await;
    let error = backend
        .create_draft_invoice(serde_json::from_value(create_args()).unwrap())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("already exists"));
    assert!(writes(&server).is_empty());
}

#[tokio::test]
async fn invalid_inputs_fail_before_network_requests() {
    for (field, value) in [
        ("request_id", json!("not-a-uuid")),
        ("request_id", json!(Uuid::nil().to_string())),
        ("currency", json!("eur")),
        ("lines", json!([])),
        ("invoice_date", json!("0000-01-01")),
        ("due_date", json!("2024-12-31")),
    ] {
        let server = Server::new(vec![]).await;
        let (_dir, backend) = backend(&server).await;
        let mut args = create_args();
        args[field] = value;
        assert!(
            backend
                .create_draft_invoice(serde_json::from_value(args).unwrap())
                .await
                .is_err(),
            "{field}"
        );
        assert!(server.requests.lock().unwrap().is_empty());
    }
    for (field, value) in [
        ("quantity", "0"),
        ("quantity", "-1"),
        ("quantity", "NaN"),
        ("unit_price", "-1"),
        ("unit_price", "01.2"),
        ("unit_price", "1e100"),
        ("item_id", ""),
        ("description", ""),
    ] {
        let mut args = line_args();
        args[field] = json!(value);
        assert!(
            serde_json::from_value::<SalesLine>(args)
                .unwrap()
                .payload()
                .is_err(),
            "{field}={value}"
        );
    }
    let mut args = create_args();
    args["Status"] = json!(50);
    assert!(serde_json::from_value::<CreateDraftInvoice>(args).is_err());
}

#[tokio::test]
async fn wrong_scope_blocked_or_unsuitable_catalogs_prevent_writes() {
    for (index, key, value) in [
        (1, "Blocked", json!(true)),
        (1, "IsSales", json!(false)),
        (1, "Division", json!(99)),
        (2, "Type", json!(22)),
        (2, "IsBlocked", json!(true)),
        (2, "Currency", json!("USD")),
        (3, "IsBlocked", json!(true)),
        (3, "VATTransactionType", json!("P")),
        (5, "IsBlocked", json!(true)),
        (5, "UseCostcenter", json!(1)),
        (5, "Type", json!(20)),
        (6, "IsSalesItem", json!(false)),
        (6, "IsFractionAllowedItem", json!(false)),
        (6, "EndDate", json!("2025-01-01")),
        (6, "ID", json!(GL)),
    ] {
        let mut replies = create_sources();
        replies[index].body["d"][0][key] = value;
        let server = Server::new(replies).await;
        let (_dir, backend) = backend(&server).await;
        assert!(
            backend
                .create_draft_invoice(serde_json::from_value(create_args()).unwrap())
                .await
                .is_err(),
            "{index}/{key}"
        );
        assert!(writes(&server).is_empty());
    }
}

#[tokio::test]
async fn write_failures_are_uncertain_and_never_retried() {
    for status in [400, 401, 403, 429, 500, 302] {
        let mut replies = create_sources();
        replies.push(Response {
            status,
            body: json!({"error":"do not echo sensitive remote text"}),
        });
        let server = Server::new(replies).await;
        let (_dir, backend) = backend(&server).await;
        let result = backend
            .create_draft_invoice(serde_json::from_value(create_args()).unwrap())
            .await
            .unwrap();
        assert_eq!(result["status"], "outcome_unknown", "{status}");
        assert_eq!(result["invoice_id"], INVOICE);
        assert_eq!(result["retry_automatically"], false);
        assert_eq!(writes(&server).len(), 1);
        assert!(!result.to_string().contains("sensitive remote text"));
    }
}

#[tokio::test]
async fn failed_or_mismatched_readback_is_not_success() {
    for (field, value) in [
        ("InvoiceTo", json!(ITEM)),
        ("Status", json!(50)),
        ("Currency", json!("USD")),
    ] {
        let mut replies = create_sources();
        replies.push(Response {
            status: 201,
            body: json!({}),
        });
        let mut h = header();
        h[field] = value;
        replies.extend(details(h, vec![saved_line()]));
        let server = Server::new(replies).await;
        let (_dir, backend) = backend(&server).await;
        let result = backend
            .create_draft_invoice(serde_json::from_value(create_args()).unwrap())
            .await
            .unwrap();
        assert_eq!(result["status"], "outcome_unknown", "{field}");
        assert_eq!(writes(&server).len(), 1);
    }
    let mut replies = create_sources();
    replies.push(Response {
        status: 201,
        body: json!({}),
    });
    let mut line = saved_line();
    line["UnitPrice"] = json!(20);
    replies.extend(details(header(), vec![line]));
    let server = Server::new(replies).await;
    let (_dir, backend) = backend(&server).await;
    let result = backend
        .create_draft_invoice(serde_json::from_value(create_args()).unwrap())
        .await
        .unwrap();
    assert_eq!(result["status"], "outcome_unknown");
    assert_eq!(writes(&server).len(), 1);
}

fn update_args() -> Value {
    json!({"account_id": ACCOUNT, "invoice_id": INVOICE, "expected_modified":MODIFIED,
        "description":"Changed description"})
}

#[tokio::test]
async fn header_edit_uses_narrow_put_and_preserves_lines() {
    let mut replies = details(header(), vec![saved_line()]);
    replies.push(customer_response());
    replies.extend(details(header(), vec![saved_line()]));
    replies.push(Response {
        status: 204,
        body: json!(null),
    });
    let mut after = header();
    after["Description"] = json!("Changed description");
    after["Modified"] = json!("/Date(1760000000456)/");
    replies.extend(details(after, vec![saved_line()]));
    let server = Server::new(replies).await;
    let (_dir, backend) = backend(&server).await;
    let result = backend
        .update_draft_invoice(serde_json::from_value(update_args()).unwrap())
        .await
        .unwrap();
    assert_eq!(result["status"], "updated", "{result}");
    let requests = writes(&server);
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with(&format!(
        "PUT /api/v1/42/salesinvoice/SalesInvoices(guid'{INVOICE}') "
    )));
    assert_eq!(
        body(&requests[0]),
        json!({"Description":"Changed description"})
    );
}

#[tokio::test]
async fn stale_or_unsupported_invoice_is_never_edited() {
    for (key, value) in [
        ("Modified", json!("different")),
        ("Modified", json!(null)),
        ("Status", json!(10)),
        ("Status", json!(50)),
        ("Type", json!(8021)),
        ("InvoiceTo", json!(ITEM)),
    ] {
        let mut h = header();
        h[key] = value;
        let server = Server::new(details(h, vec![saved_line()])).await;
        let (_dir, backend) = backend(&server).await;
        assert!(
            backend
                .update_draft_invoice(serde_json::from_value(update_args()).unwrap())
                .await
                .is_err(),
            "{key}"
        );
        assert!(writes(&server).is_empty());
    }
}

#[tokio::test]
async fn concurrent_change_observed_before_save_prevents_put() {
    let mut replies = details(header(), vec![saved_line()]);
    replies.push(customer_response());
    let mut changed = header();
    changed["Modified"] = json!("changed");
    replies.extend(details(changed, vec![saved_line()]));
    let server = Server::new(replies).await;
    let (_dir, backend) = backend(&server).await;
    assert!(
        backend
            .update_draft_invoice(serde_json::from_value(update_args()).unwrap())
            .await
            .is_err()
    );
    assert!(writes(&server).is_empty());
}

fn update_line_args() -> Value {
    json!({"account_id":ACCOUNT,"invoice_id":INVOICE,"line_id":LINE,
        "expected_modified":MODIFIED,"line":line_args()})
}

#[tokio::test]
async fn line_edit_saves_only_selected_line_and_verifies_other_lines() {
    let mut original = saved_line();
    original["UnitPrice"] = json!(5);
    let mut other = saved_line();
    other["ID"] = json!("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
    other["LineNumber"] = json!(2);
    let mut replies = details(header(), vec![original.clone(), other.clone()]);
    replies.push(customer_response());
    replies.extend(catalogs().into_iter().skip(1));
    replies.extend(details(header(), vec![original, other.clone()]));
    replies.push(Response {
        status: 204,
        body: json!(null),
    });
    replies.extend(details(header(), vec![saved_line(), other]));
    let server = Server::new(replies).await;
    let (_dir, backend) = backend(&server).await;
    let result = backend
        .update_draft_invoice_line(serde_json::from_value(update_line_args()).unwrap())
        .await
        .unwrap();
    assert_eq!(result["status"], "updated", "{result}");
    let requests = writes(&server);
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with(&format!(
        "PUT /api/v1/42/salesinvoice/SalesInvoiceLines(guid'{LINE}') "
    )));
    assert_eq!(
        body(&requests[0]),
        serde_json::from_value::<SalesLine>(line_args())
            .unwrap()
            .payload()
            .unwrap()
    );
}

#[tokio::test]
async fn line_from_another_invoice_cannot_be_edited() {
    let server = Server::new(details(header(), vec![saved_line()])).await;
    let (_dir, backend) = backend(&server).await;
    let mut args = update_line_args();
    args["line_id"] = json!(ITEM);
    assert!(
        backend
            .update_draft_invoice_line(serde_json::from_value(args).unwrap())
            .await
            .is_err()
    );
    assert!(writes(&server).is_empty());
}

#[tokio::test]
async fn dry_run_header_and_line_edits_never_put() {
    let mut replies = details(header(), vec![saved_line()]);
    replies.push(customer_response());
    let server = Server::new(replies).await;
    let (_dir, backend) = backend(&server).await;
    let mut args = update_args();
    args["dry_run"] = json!(true);
    let result = backend
        .update_draft_invoice(serde_json::from_value(args).unwrap())
        .await
        .unwrap();
    assert_eq!(result["status"], "preview");
    assert!(writes(&server).is_empty());

    let mut replies = details(header(), vec![saved_line()]);
    replies.push(customer_response());
    replies.extend(catalogs().into_iter().skip(1));
    let server = Server::new(replies).await;
    let (_dir, backend) = crate::draft::tests::backend(&server).await;
    let mut args = update_line_args();
    args["dry_run"] = json!(true);
    let result = backend
        .update_draft_invoice_line(serde_json::from_value(args).unwrap())
        .await
        .unwrap();
    assert_eq!(result["status"], "preview");
    assert!(writes(&server).is_empty());
}

#[tokio::test]
async fn disconnected_write_and_failed_readback_never_trigger_retry() {
    for status in [0, 201] {
        let mut replies = create_sources();
        replies.push(Response {
            status,
            body: json!({}),
        });
        replies.push(Response {
            status: 403,
            body: json!({}),
        });
        let server = Server::new(replies).await;
        let (_dir, backend) = backend(&server).await;
        let result = backend
            .create_draft_invoice(serde_json::from_value(create_args()).unwrap())
            .await
            .unwrap();
        assert_eq!(result["status"], "outcome_unknown");
        assert_eq!(writes(&server).len(), 1);
    }
}

#[tokio::test]
async fn unit_mismatch_and_failed_catalog_pagination_prevent_creation() {
    let mut replies = create_sources();
    replies[6].body["d"][0]["Unit"] = json!("piece");
    let server = Server::new(replies).await;
    let (_dir, backend) = backend(&server).await;
    assert!(
        backend
            .create_draft_invoice(serde_json::from_value(create_args()).unwrap())
            .await
            .is_err()
    );
    assert!(writes(&server).is_empty());

    let mut replies = create_sources();
    let rows = replies[3].body["d"].clone();
    replies[3].body = json!({"d":{"results":rows,"__next":"?page=2"}});
    replies.insert(
        4,
        Response {
            status: 403,
            body: json!({}),
        },
    );
    let server = Server::new(replies).await;
    let (_dir, backend) = crate::draft::tests::backend(&server).await;
    assert!(
        backend
            .create_draft_invoice(serde_json::from_value(create_args()).unwrap())
            .await
            .is_err()
    );
    assert!(writes(&server).is_empty());
}

#[tokio::test]
async fn multiple_lines_and_explicit_header_fields_are_saved_and_verified() {
    let mut args = create_args();
    args["lines"].as_array_mut().unwrap().push(line_args());
    args["due_date"] = json!("2025-02-01");
    args["payment_condition_code"] = json!("30");
    args["your_ref"] = json!("000123-CUSTOM");
    args["remarks"] = json!("User supplied notes");
    let mut replies = create_sources();
    replies.insert(3, Response::rows(vec![json!({"ID":"cccccccc-cccc-cccc-cccc-cccccccccccc", "Division":42,"Code":"30","PaymentDays":30})]));
    let item_reply = catalogs().pop().unwrap();
    replies.push(item_reply);
    replies.push(Response {
        status: 201,
        body: json!({}),
    });
    let mut h = header();
    h["DueDate"] = json!("2025-02-01");
    h["PaymentCondition"] = json!("30");
    h["YourRef"] = json!("000123-CUSTOM");
    h["Remarks"] = json!("User supplied notes");
    let mut second = saved_line();
    second["ID"] = json!("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
    second["LineNumber"] = json!(2);
    replies.extend(details(h, vec![second, saved_line()]));
    let server = Server::new(replies).await;
    let (_dir, backend) = backend(&server).await;
    let result = backend
        .create_draft_invoice(serde_json::from_value(args).unwrap())
        .await
        .unwrap();
    assert_eq!(result["status"], "created", "{result}");
    let requests = writes(&server);
    let payload = body(&requests[0]);
    assert_eq!(payload["YourRef"], "000123-CUSTOM");
    assert_eq!(payload["PaymentCondition"], "30");
    assert_eq!(payload["SalesInvoiceLines"][0]["LineNumber"], 1);
    assert_eq!(payload["SalesInvoiceLines"][1]["LineNumber"], 2);
}

#[tokio::test]
async fn unrelated_changes_during_readback_are_never_reported_as_success() {
    for header_change in [false, true] {
        let mut replies = details(header(), vec![saved_line()]);
        replies.push(customer_response());
        replies.extend(details(header(), vec![saved_line()]));
        replies.push(Response {
            status: 204,
            body: json!(null),
        });
        let mut after = header();
        after["Description"] = json!("Changed description");
        let mut line = saved_line();
        if header_change {
            after["Currency"] = json!("USD");
        } else {
            line["UnitPrice"] = json!(200);
        }
        replies.extend(details(after, vec![line]));
        let server = Server::new(replies).await;
        let (_dir, backend) = backend(&server).await;
        let result = backend
            .update_draft_invoice(serde_json::from_value(update_args()).unwrap())
            .await
            .unwrap();
        assert_eq!(result["status"], "outcome_unknown");
        assert_eq!(writes(&server).len(), 1);
    }
}

#[tokio::test]
async fn clearing_text_accepts_exact_null_readback() {
    let mut before = header();
    before["Description"] = json!("Remove this");
    let mut replies = details(before.clone(), vec![saved_line()]);
    replies.push(customer_response());
    replies.extend(details(before, vec![saved_line()]));
    replies.push(Response {
        status: 204,
        body: json!(null),
    });
    replies.extend(details(header(), vec![saved_line()]));
    let server = Server::new(replies).await;
    let (_dir, backend) = backend(&server).await;
    let mut args = update_args();
    args["description"] = json!("");
    let result = backend
        .update_draft_invoice(serde_json::from_value(args).unwrap())
        .await
        .unwrap();
    assert_eq!(result["status"], "updated", "{result}");
    assert_eq!(body(&writes(&server)[0]), json!({"Description":""}));
}

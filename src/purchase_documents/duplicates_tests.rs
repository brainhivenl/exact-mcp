use super::{Response, Server, financial_backend};
use crate::purchase_documents::AssessPurchaseDuplicates;
use serde_json::{Value, json};
use uuid::Uuid;

fn id(value: u128) -> Uuid {
    Uuid::from_u128(value)
}
fn rows(values: Vec<Value>) -> Response {
    Response::json(json!({"d":{"results":values}}))
}
fn account() -> Value {
    json!({"ID":id(100),"Division":42,"Code":" 001 ","Name":"Invented supplier"})
}
fn entry(value: u128, reference: Option<&str>) -> Value {
    json!({"EntryID":id(value),"Division":42,"Supplier":id(100),"EntryNumber":12345,"Journal":" P1 ",
        "YourRef":reference,"EntryDate":"2025-01-01","AmountFC":-121.00,"Currency":"EUR","Status":50,"Type":30,"Reversal":false})
}
fn document(value: u128) -> Value {
    json!({"ID":id(value),"Division":42,"Type":20,"Account":id(100),"Subject":"Source REF-001 attachment",
        "DocumentDate":"2025-01-01","AmountFC":121.00,"Currency":"EUR","ProposedEntryStatus":20,"Modified":"2025-01-01T10:00:00"})
}
fn args() -> AssessPurchaseDuplicates {
    serde_json::from_value(json!({"account_id":id(100),"supplier_invoice_reference":"REF-001"}))
        .unwrap()
}
async fn assess(
    entries: Vec<Value>,
    documents: Vec<Value>,
    args: AssessPurchaseDuplicates,
) -> Value {
    let server = Server::new(vec![rows(vec![account()]), rows(entries), rows(documents)]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let result =
        serde_json::to_value(backend.assess_purchase_duplicates(args).await.unwrap()).unwrap();
    let requests = server.finish();
    for request in requests {
        assert!(request.target.starts_with("GET /api/v1/42/"));
        assert!(request.body.is_empty());
        assert!(
            !request.target.contains("/purchase/PurchaseInvoices")
                && !request.target.contains("SupplierItem")
                && !request.target.contains("project/")
                && !request.target.contains("PayablesList")
        );
    }
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_references_preserve_zeroes_punctuation_internal_whitespace_and_conflicting_metadata()
 {
    let mut exact = entry(1, Some(" REF-001 "));
    exact["EntryDate"] = json!("2000-01-01");
    exact["Currency"] = json!("USD");
    let mut description_only = entry(5, Some("different"));
    description_only["Description"] = json!("REF-001");
    description_only["EntryNumber"] = json!(1);
    let mut proposed = args();
    proposed.invoice_date = Some("2025-01-01".parse().unwrap());
    let result = assess(
        vec![
            exact,
            entry(2, Some("ref-001")),
            entry(3, Some("REF001")),
            entry(4, Some("REF-0001")),
            description_only,
            entry(6, Some("REF- 001")),
        ],
        vec![],
        proposed,
    )
    .await;
    assert_eq!(result["matched_candidate_count"], 2);
    assert_eq!(
        result["candidates"][0]["evidence"]["reference_comparison"],
        "exact_after_outer_trim"
    );
    assert_eq!(
        result["candidates"][0]["evidence"]["date_comparison"],
        "different_calendar_date"
    );
    assert_eq!(
        result["candidates"][1]["evidence"]["reference_comparison"],
        "case_insensitive_after_outer_trim"
    );
    assert_eq!(result["candidates"][0]["entry"]["your_ref"], " REF-001 ");
    assert_eq!(result["coverage"]["supplier_accounting_entries_read"], 6);
    assert_eq!(result["can_rule_out_duplicates"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_numeric_references_are_not_entry_numbers_or_normalized_supplier_references() {
    let mut proposed = args();
    proposed.supplier_invoice_reference = "000123".into();
    let mut numeric = entry(2, Some("123"));
    numeric["EntryNumber"] = json!(123);
    let result = assess(vec![entry(1, Some("000123")), numeric], vec![], proposed).await;
    assert_eq!(result["matched_candidate_count"], 1);
    assert_eq!(
        result["candidates"][0]["entry"]["entry_id"],
        id(1).to_string()
    );
    let mut proposed = args();
    proposed.supplier_invoice_reference = "ö/001".into();
    let result = assess(vec![entry(1, Some("Ö/001"))], vec![], proposed).await;
    assert_eq!(
        result["candidates"][0]["evidence"]["reference_comparison"],
        "case_insensitive_after_outer_trim"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_amount_evidence_never_nets_rounds_converts_or_conflates_signs() {
    for (source_amount, source_currency, expected) in [
        (json!(-121), json!("EUR"), "same_magnitude_different_sign"),
        (json!(121), json!(" eur "), "same_signed_amount"),
        (
            json!(-121),
            json!("USD"),
            "not_comparable_currency_not_confirmed",
        ),
        (
            json!(-121),
            Value::Null,
            "not_comparable_currency_not_confirmed",
        ),
        (Value::Null, json!("EUR"), "source_unknown"),
        (json!(121.01), json!("EUR"), "different_amount"),
    ] {
        let mut candidate = entry(1, Some("REF-001"));
        candidate["AmountFC"] = source_amount;
        candidate["Currency"] = source_currency;
        let mut proposed = args();
        proposed.amount_transaction_currency = Some("121.00".into());
        proposed.currency = Some("eur".into());
        let result = assess(vec![candidate], vec![], proposed).await;
        assert_eq!(
            result["candidates"][0]["evidence"]["amount_comparison"],
            expected
        );
        assert_eq!(result["proposal"]["amount_transaction_currency"], "121.00");
        assert_eq!(result["proposal"]["currency"], "EUR");
    }
    let mut candidate = entry(1, Some("REF-001"));
    candidate["AmountFC"] = serde_json::from_str("-123.123456789012345678901").unwrap();
    let mut proposed = args();
    proposed.amount_transaction_currency = Some("123.123456789012345678901".into());
    proposed.currency = Some("EUR".into());
    let result = assess(vec![candidate], vec![], proposed).await;
    assert_eq!(
        result["candidates"][0]["entry"]["amount_transaction_currency"],
        "-123.123456789012345678901"
    );
    assert_eq!(
        result["candidates"][0]["evidence"]["amount_comparison"],
        "same_magnitude_different_sign"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_amount_date_support_requires_same_currency_and_exact_calendar_date() {
    let mut different_day = entry(2, None);
    different_day["EntryDate"] = json!("2025-01-02");
    let mut different_currency = entry(3, None);
    different_currency["Currency"] = json!("USD");
    let mut no_date = entry(4, None);
    no_date["EntryDate"] = Value::Null;
    let mut proposed = args();
    proposed.invoice_date = Some("2025-01-01".parse().unwrap());
    proposed.currency = Some("EUR".into());
    proposed.amount_transaction_currency = Some("121.00".into());
    let mut doc = document(10);
    doc["Subject"] = json!("no reference");
    let result = assess(
        vec![
            entry(1, Some("different")),
            different_day,
            different_currency,
            no_date,
        ],
        vec![doc],
        proposed,
    )
    .await;
    assert_eq!(result["matched_candidate_count"], 2);
    assert_eq!(
        result["candidates"][0]["evidence"]["reference_comparison"],
        "different"
    );
    assert_eq!(
        result["candidates"][0]["evidence"]["date_basis"],
        "accounting_entry_date_not_guaranteed_supplier_issue_date"
    );
    assert_eq!(
        result["candidates"][1]["evidence"]["date_basis"],
        "incoming_document_date_not_supplier_invoice_date"
    );
    assert_eq!(
        result["candidates"][1]["evidence"]["reference_comparison"],
        "supplier_invoice_reference_not_exposed_by_source"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_document_subjects_are_hints_not_verified_references_or_supplier_identity() {
    let mut unassigned = document(11);
    unassigned["Account"] = Value::Null;
    let mut other = document(12);
    other["Account"] = json!(id(101));
    let mut unknown_status = document(13);
    unknown_status["ProposedEntryStatus"] = Value::Null;
    let result = assess(
        vec![],
        vec![document(10), unassigned, other, unknown_status],
        args(),
    )
    .await;
    assert_eq!(result["matched_candidate_count"], 3);
    assert_eq!(result["coverage"]["supplier_documents_read"], 2);
    assert_eq!(result["coverage"]["unassigned_documents_read"], 1);
    assert_eq!(result["coverage"]["other_company_documents_read"], 1);
    assert_eq!(
        result["candidates"][0]["evidence"]["reference_comparison"],
        "subject_contains_reference_text_only"
    );
    assert_eq!(
        result["candidates"][1]["evidence"]["supplier_association"],
        "unassigned_document_supplier_not_verified"
    );
    assert!(result["candidates"][2]["document"]["proposed_entry_status"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_explicit_links_include_entry_ids_and_do_not_count_invoices() {
    let mut first = entry(1, Some("REF-001"));
    first["Document"] = json!(id(10));
    let mut second = entry(2, Some("REF-001"));
    second["Document"] = json!(id(10));
    let mut doc = document(10);
    doc["Subject"] = json!("unrelated display title");
    doc["FinancialTransactionEntryID"] = json!(id(1));
    let result = assess(vec![first, second], vec![doc], args()).await;
    let doc = result["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|candidate| candidate["source"] == "purchase_document")
        .unwrap();
    assert_eq!(
        doc["evidence"]["linked_accounting_entry_ids"],
        json!([id(1), id(2)])
    );
    assert_eq!(result["matched_candidate_count"], 3);
    assert!(result.get("duplicate_invoice_count").is_none());
    assert_eq!(result["candidates"][0]["source"], "accounting_entry");
    assert_eq!(result["coverage"]["accounting_candidate_count"], 2);
    assert_eq!(result["coverage"]["document_candidate_count"], 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_selected_document_is_excluded_but_its_already_linked_entry_is_not() {
    let mut source = document(10);
    source["FinancialTransactionEntryID"] = json!(id(1));
    let mut selected_entry = entry(1, Some("DIFFERENT"));
    selected_entry["Document"] = json!(id(10));
    let server = Server::new(vec![
        rows(vec![account()]),
        rows(vec![source.clone()]),
        rows(vec![selected_entry]),
        rows(vec![source.clone()]),
        rows(vec![source]),
    ]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let mut proposed = args();
    proposed.source_document_id = Some(id(10).to_string());
    let result =
        serde_json::to_value(backend.assess_purchase_duplicates(proposed).await.unwrap()).unwrap();
    assert_eq!(result["matched_candidate_count"], 1);
    assert_eq!(result["candidates"][0]["source"], "accounting_entry");
    assert_eq!(
        result["candidates"][0]["evidence"]["reference_comparison"],
        "different"
    );
    assert_eq!(result["coverage"]["input_document_excluded"], 1);
    assert_eq!(result["coverage"]["document_candidate_count"], 0);
    assert_eq!(
        result["coverage"]["source_document_reference_resolution"],
        "resolved_in_supplier_purchase_entries"
    );
    server.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_empty_results_and_late_empty_pages_never_clear_booking() {
    for entries in [vec![], vec![entry(1, Some("REF-001"))]] {
        let count = entries.len();
        let mut proposed = args();
        proposed.offset = 50;
        let result = assess(entries, vec![], proposed).await;
        assert_eq!(result["matched_candidate_count"], count);
        assert_eq!(result["candidates"], json!([]));
        assert_eq!(result["can_rule_out_duplicates"], false);
        assert_eq!(result["has_more"], false);
        assert_eq!(
            result["assessment"],
            if count == 0 {
                "inconclusive_no_candidates_in_checked_sources"
            } else {
                "candidates_found_review_required"
            }
        );
        assert_eq!(result["coverage"]["all_selected_source_pages_read"], true);
        assert!(
            !result["coverage"]["known_gaps"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(result.get("safe_to_book").is_none());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_invalid_arguments_fail_before_any_api_call() {
    let server = Server::new(vec![]);
    let (_directory, backend) = financial_backend(&server.base).await;
    for changes in [
        json!({"account_id":"invalid"}),
        json!({"account_id":Uuid::nil()}),
        json!({"supplier_invoice_reference":" "}),
        json!({"supplier_invoice_reference":"x".repeat(101)}),
        json!({"currency":"EURO"}),
        json!({"currency":"€UR"}),
        json!({"currency":" "}),
        json!({"amount_transaction_currency":"121.00"}),
        json!({"amount_transaction_currency":"NaN","currency":"EUR"}),
        json!({"amount_transaction_currency":"0.00000000000000000000000000001","currency":"EUR"}),
        json!({"source_document_id":Uuid::nil()}),
        json!({"limit":0}),
        json!({"limit":51}),
        json!({"offset":u32::MAX}),
    ] {
        let mut value = json!({"account_id":id(100),"supplier_invoice_reference":"REF-001"});
        for (key, change) in changes.as_object().unwrap() {
            value[key] = change.clone();
        }
        assert!(
            backend
                .assess_purchase_duplicates(serde_json::from_value(value).unwrap())
                .await
                .is_err()
        );
    }
    assert!(serde_json::from_value::<AssessPurchaseDuplicates>(json!({"account_id":id(100),"supplier_invoice_reference":"REF","amount_transaction_currency":121.00})).is_err());
    assert!(serde_json::from_value::<AssessPurchaseDuplicates>(json!({"account_id":id(100),"supplier_invoice_reference":"REF","url":"https://untrusted.invalid"})).is_err());
    server.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_hidden_entry_scope_and_duplicate_ids_fail_without_partial_candidates() {
    for bad in [
        entry(1, Some("unmatched")),
        {
            let mut value = entry(2, None);
            value["Division"] = json!(99);
            value
        },
        {
            let mut value = entry(2, None);
            value["Supplier"] = json!(id(101));
            value
        },
        {
            let mut value = entry(2, None);
            value["EntryID"] = json!(Uuid::nil());
            value
        },
        {
            let mut value = entry(2, None);
            value["Document"] = json!(Uuid::nil());
            value
        },
        {
            let mut value = entry(2, None);
            value["EntryNumber"] = json!(0);
            value
        },
    ] {
        let server = Server::new(vec![
            rows(vec![account()]),
            Response::json(json!({"d":{"results":[entry(1,Some("REF-001"))],"__next":"?page=2"}})),
            rows(vec![bad]),
        ]);
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(backend.assess_purchase_duplicates(args()).await.is_err());
        server.finish();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_document_faults_conflicting_links_and_wrong_scope_never_disappear_silently() {
    for documents in [
        vec![document(10), document(10)],
        vec![{
            let mut value = document(10);
            value["Type"] = json!(15);
            value
        }],
        vec![{
            let mut value = document(10);
            value["Division"] = json!(99);
            value
        }],
        vec![{
            let mut value = document(10);
            value["Account"] = json!(Uuid::nil());
            value
        }],
        vec![{
            let mut value = document(10);
            value["Account"] = json!(id(101));
            value["FinancialTransactionEntryID"] = json!(id(1));
            value
        }],
    ] {
        let server = Server::new(vec![
            rows(vec![account()]),
            rows(vec![entry(1, Some("REF-001"))]),
            rows(documents),
        ]);
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(backend.assess_purchase_duplicates(args()).await.is_err());
        server.finish();
    }
    let server = Server::new(vec![
        rows(vec![account()]),
        rows(vec![entry(1, Some("REF-001"))]),
        Response {
            status: "403 Forbidden",
            body: "private source response".into(),
            location: None,
        },
    ]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let error = backend
        .assess_purchase_duplicates(args())
        .await
        .err()
        .unwrap();
    assert!(!format!("{error:#}").contains("private source response"));
    assert!(format!("{error:#}").contains("403"));
    server.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_input_document_company_changes_disappearance_and_snapshot_changes_fail_closed() {
    let mut other = document(10);
    other["Account"] = json!(id(101));
    let server = Server::new(vec![rows(vec![account()]), rows(vec![other])]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let mut proposed = args();
    proposed.source_document_id = Some(id(10).to_string());
    assert!(backend.assess_purchase_duplicates(proposed).await.is_err());
    server.finish();
    for phase in ["enumeration", "final", "disappeared"] {
        let source = document(10);
        let mut changed = source.clone();
        changed["Modified"] = json!("2025-01-01T10:00:01");
        let mut replies = vec![
            rows(vec![account()]),
            rows(vec![source.clone()]),
            rows(vec![]),
        ];
        if phase == "enumeration" {
            replies.push(rows(vec![changed]));
        } else if phase == "disappeared" {
            replies.push(rows(vec![]));
        } else {
            replies.push(rows(vec![source]));
            replies.push(rows(vec![changed]));
        }
        let server = Server::new(replies);
        let (_directory, backend) = financial_backend(&server.base).await;
        let mut proposed = args();
        proposed.source_document_id = Some(id(10).to_string());
        assert!(
            backend.assess_purchase_duplicates(proposed).await.is_err(),
            "{phase}"
        );
        server.finish();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_source_document_unassigned_and_unresolved_reference_stay_explicit() {
    let mut source = document(10);
    source["Account"] = Value::Null;
    source["FinancialTransactionEntryID"] = json!(id(999));
    let server = Server::new(vec![
        rows(vec![account()]),
        rows(vec![source.clone()]),
        rows(vec![]),
        rows(vec![source.clone()]),
        rows(vec![source]),
    ]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let mut proposed = args();
    proposed.source_document_id = Some(id(10).to_string());
    let result =
        serde_json::to_value(backend.assess_purchase_duplicates(proposed).await.unwrap()).unwrap();
    assert_eq!(
        result["coverage"]["source_document_reference_resolution"],
        "not_found_in_supplier_purchase_entries"
    );
    assert_eq!(
        result["coverage"]["source_document_supplier_association"],
        "unassigned_supplier_not_verified"
    );
    assert_eq!(result["coverage"]["input_document_excluded"], 1);
    assert_eq!(result["can_rule_out_duplicates"], false);
    server.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_pagination_counts_all_sources_and_keeps_late_best_evidence() {
    let mut replies = vec![rows(vec![account()])];
    for page in 0..5 {
        let entries: Vec<_> = (0..400)
            .map(|index| entry(2000 - (page * 400 + index), Some("REF-001")))
            .collect();
        let mut response = json!({"d":{"results":entries}});
        if page < 4 {
            response["d"]["__next"] = json!(format!("?page={}", page + 2));
        }
        replies.push(Response::json(response));
    }
    replies.push(rows((3000..4000).map(document).collect()));
    let server = Server::new(replies);
    let (_directory, backend) = financial_backend(&server.base).await;
    let mut proposed = args();
    proposed.offset = 9;
    proposed.limit = Some(3);
    let result =
        serde_json::to_value(backend.assess_purchase_duplicates(proposed).await.unwrap()).unwrap();
    assert_eq!(result["coverage"]["supplier_accounting_entries_read"], 2000);
    assert_eq!(result["coverage"]["purchase_documents_read"], 1000);
    assert_eq!(result["matched_candidate_count"], 3000);
    assert_eq!(
        result["candidates"][0]["entry"]["entry_id"],
        id(10).to_string()
    );
    assert_eq!(
        result["candidates"][2]["entry"]["entry_id"],
        id(12).to_string()
    );
    assert_eq!(result["next_offset"], 12);
    assert_eq!(result["has_more"], true);
    assert_eq!(server.finish().len(), 7);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_scope_changes_prevent_reads_and_create_no_files() {
    let server = Server::new(vec![]);
    let (directory, backend) = financial_backend(&server.base).await;
    {
        let store = crate::store::Store::open(directory.path()).await.unwrap();
        let mut session = store.require_session().await.unwrap();
        session.administration.as_mut().unwrap().code = 99;
        store.save(&session).await.unwrap();
    }
    assert!(backend.assess_purchase_duplicates(args()).await.is_err());
    assert!(!directory.path().join("downloads").exists());
    server.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_accounting_statuses_and_reversals_are_retained_not_payment_filters() {
    let entries = [5, 20, 50, 77]
        .into_iter()
        .enumerate()
        .map(|(index, status)| {
            let mut value = entry(index as u128 + 1, Some("REF-001"));
            value["Status"] = json!(status);
            value["Type"] = json!(if index % 2 == 0 { 30 } else { 31 });
            value["Reversal"] = json!(index % 2 == 1);
            value
        })
        .collect();
    let result = assess(entries, vec![], args()).await;
    assert_eq!(result["matched_candidate_count"], 4);
    for (index, status) in [5, 20, 50, 77].into_iter().enumerate() {
        assert_eq!(result["candidates"][index]["entry"]["status_code"], status);
        assert_eq!(
            result["candidates"][index]["entry"]["reversal"],
            index % 2 == 1
        );
        assert!(result["candidates"][index].get("paid").is_none());
    }
}

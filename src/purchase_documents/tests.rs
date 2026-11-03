use super::{Response, Server, financial_backend, pdf_response};
use crate::purchase_documents::{
    DownloadPurchaseDocumentPdf, GetPurchaseDocument, SearchPurchaseDocuments,
};
use serde_json::{Value, json};
use uuid::Uuid;

fn id(value: u128) -> Uuid {
    Uuid::from_u128(value)
}
fn rows(values: Vec<Value>) -> Response {
    Response::json(json!({"d":{"results":values}}))
}
fn document(value: u128) -> Value {
    json!({"ID":id(value),"Division":42,"Type":20,"Subject":"ÉTUDE %_ O'Neill","Account":id(100),
        "DocumentDate":"2025-01-01","Modified":"2025-01-01T10:01:02","ProposedEntryStatus":null,
        "FinancialTransactionEntryID":null,"AmountFC":-10.25,"Currency":"USD"})
}
fn attachment(value: u128) -> Value {
    json!({"ID":id(value),"Document":id(1),"FileName":"../../untrusted.pdf","FileSize":55,"Url":"https://untrusted.invalid/never-follow"})
}
fn entry(value: u128) -> Value {
    json!({"EntryID":id(value),"Division":42,"Supplier":id(100),"EntryNumber":12345,"Journal":" P1 ",
        "Document":id(1),"Type":30,"Status":20,"YourRef":"SUPPLIER-0001"})
}
fn account() -> Value {
    json!({"ID":id(100),"Division":42,"Code":" 001 ","Name":"Invented supplier"})
}
fn search_args(value: Value) -> SearchPurchaseDocuments {
    serde_json::from_value(value).unwrap()
}
fn get_args() -> GetPurchaseDocument {
    serde_json::from_value(json!({"document_id":id(1)})).unwrap()
}
fn download_args() -> DownloadPurchaseDocumentPdf {
    serde_json::from_value(json!({"document_id":id(1),"attachment_id":id(10)})).unwrap()
}
fn query(request: &super::Request, name: &str) -> Option<String> {
    let url = reqwest::Url::parse(&format!(
        "http://localhost{}",
        request.target.split_whitespace().nth(1).unwrap()
    ))
    .unwrap();
    url.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intake_search_reads_all_pages_keeps_global_window_unknowns_and_raw_precision() {
    let mut early = document(1);
    early["DocumentDate"] = json!("2020-01-01");
    let mut latest = document(2);
    latest["DocumentDate"] = json!("2026-01-01");
    latest["ProposedEntryStatus"] = json!(77);
    latest["AmountFC"] = serde_json::from_str("-123.123456789012345678901").unwrap();
    let mut unknown = document(3);
    unknown["DocumentDate"] = Value::Null;
    unknown["Account"] = Value::Null;
    let server = Server::new(vec![
        Response::json(json!({"d":{"results":[early,unknown],"__next":"?page=2"}})),
        rows(vec![latest]),
    ]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let result = serde_json::to_value(
        backend
            .search_purchase_documents(search_args(json!({"limit":1})))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["source_document_count"], 3);
    assert_eq!(result["matched_document_count"], 3);
    assert_eq!(result["documents"][0]["id"], id(2).to_string());
    assert_eq!(result["documents"][0]["proposed_entry_status"], 77);
    assert_eq!(
        result["documents"][0]["amount_transaction_currency"],
        "-123.123456789012345678901"
    );
    assert_eq!(result["documents"][0]["currency"], "USD");
    assert_eq!(result["has_more"], true);
    assert_eq!(result["next_offset"], 1);
    assert!(result.get("is_unbooked").is_none());
    let requests = server.finish();
    assert_eq!(query(&requests[0], "$filter"), Some("Type eq 20".into()));
    let fields = query(&requests[0], "$select").unwrap();
    for forbidden in [
        "Body",
        "ActionRegenerateEntryProposal",
        "MarkAsPaid",
        "ScanServiceStatus",
        "DocumentViewUrl",
    ] {
        assert!(!fields.split(',').any(|field| field == forbidden));
    }
    assert!(requests.iter().all(|request| {
        request
            .target
            .starts_with("GET /api/v1/42/documents/Documents")
    }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intake_search_filters_source_metadata_without_claiming_unbooked_or_payment_status() {
    for (args, expected) in [
        (json!({"query":" étude %_ o'neill "}), 2),
        (json!({"query":"SUPPLIER-0001"}), 0),
        (json!({"link_filter":"without_reference"}), 1),
        (json!({"link_filter":"with_reference"}), 1),
        (json!({"proposed_entry_status":20}), 1),
        (json!({"proposed_entry_status":0}), 0),
        (
            json!({"document_date_from":"2025-01-01","document_date_to":"2025-01-01"}),
            1,
        ),
    ] {
        let mut second = document(2);
        second["DocumentDate"] = Value::Null;
        second["ProposedEntryStatus"] = json!(20);
        second["FinancialTransactionEntryID"] = json!(id(200));
        let server = Server::new(vec![rows(vec![document(1), second])]);
        let (_directory, backend) = financial_backend(&server.base).await;
        let result = serde_json::to_value(
            backend
                .search_purchase_documents(search_args(args))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["matched_document_count"], expected);
        assert_eq!(result["source_document_count"], 2);
        assert!(
            result["coverage"]
                .as_str()
                .unwrap()
                .contains("not_complete_inbox")
        );
        server.finish();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intake_search_company_selection_is_verified_and_unassigned_documents_remain_explicit() {
    let mut unknown = document(2);
    unknown["Account"] = Value::Null;
    unknown["AmountFC"] = Value::Null;
    unknown["Currency"] = Value::Null;
    let server = Server::new(vec![
        rows(vec![account()]),
        rows(vec![document(1), unknown]),
    ]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let result = serde_json::to_value(
        backend
            .search_purchase_documents(search_args(json!({"account_id":id(100)})))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["matched_document_count"], 1);
    let requests = server.finish();
    assert!(requests[0].target.contains("/crm/Accounts?"));
    assert_eq!(
        query(&requests[0], "$filter"),
        Some(format!("ID eq guid'{}'", id(100)))
    );
    let mut unknown = document(1);
    unknown["Account"] = Value::Null;
    unknown["AmountFC"] = Value::Null;
    unknown["Currency"] = Value::Null;
    let server = Server::new(vec![rows(vec![unknown])]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let result = serde_json::to_value(
        backend
            .search_purchase_documents(search_args(json!({})))
            .await
            .unwrap(),
    )
    .unwrap();
    for key in [
        "account_id",
        "amount_transaction_currency",
        "currency",
        "proposed_entry_status",
    ] {
        assert!(result["documents"][0][key].is_null());
    }
    server.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intake_search_rejects_hidden_invalid_documents_and_duplicate_pages() {
    for bad in [
        {
            let mut value = document(2);
            value["Division"] = json!(99);
            value
        },
        {
            let mut value = document(2);
            value["Type"] = json!(15);
            value
        },
        {
            let mut value = document(2);
            value["ID"] = json!(Uuid::nil());
            value
        },
        {
            let mut value = document(2);
            value["Account"] = json!(Uuid::nil());
            value
        },
        {
            let mut value = document(2);
            value["FinancialTransactionEntryID"] = json!(Uuid::nil());
            value
        },
        document(1),
    ] {
        let server = Server::new(vec![
            Response::json(json!({"d":{"results":[document(1)],"__next":"?page=2"}})),
            rows(vec![bad]),
        ]);
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(
            backend
                .search_purchase_documents(search_args(json!({"query":"not matched"})))
                .await
                .is_err()
        );
        server.finish();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intake_arguments_fail_before_remote_reads() {
    let server = Server::new(vec![]);
    let (_directory, backend) = financial_backend(&server.base).await;
    for value in [
        json!({"account_id":"invalid"}),
        json!({"account_id":Uuid::nil()}),
        json!({"query":" "}),
        json!({"query":"x".repeat(101)}),
        json!({"proposed_entry_status":10}),
        json!({"limit":0}),
        json!({"limit":51}),
        json!({"offset":u32::MAX}),
        json!({"document_date_from":"2025-02-01","document_date_to":"2025-01-01"}),
    ] {
        assert!(
            backend
                .search_purchase_documents(search_args(value))
                .await
                .is_err()
        );
    }
    assert!(
        serde_json::from_value::<SearchPurchaseDocuments>(json!({"link_filter":"unbooked"}))
            .is_err()
    );
    assert!(serde_json::from_value::<SearchPurchaseDocuments>(json!({"unexpected":true})).is_err());
    for invalid in ["invalid".to_string(), Uuid::nil().to_string()] {
        assert!(
            backend
                .get_purchase_document(
                    serde_json::from_value(json!({"document_id":invalid})).unwrap()
                )
                .await
                .is_err()
        );
        assert!(
            backend
                .download_purchase_document_pdf(
                    serde_json::from_value(json!({"document_id":invalid,"attachment_id":id(10)}))
                        .unwrap()
                )
                .await
                .is_err()
        );
        assert!(
            backend
                .download_purchase_document_pdf(
                    serde_json::from_value(json!({"document_id":id(1),"attachment_id":invalid}))
                        .unwrap()
                )
                .await
                .is_err()
        );
    }
    assert!(
        serde_json::from_value::<DownloadPurchaseDocumentPdf>(
            json!({"document_id":id(1),"attachment_id":id(10),"url":"https://untrusted.invalid"})
        )
        .is_err()
    );
    server.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intake_details_find_reverse_links_even_when_document_has_no_transaction_reference() {
    let server = Server::new(vec![
        rows(vec![document(1)]),
        rows(vec![attachment(10)]),
        rows(vec![entry(200), entry(201)]),
    ]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let result =
        serde_json::to_value(backend.get_purchase_document(get_args()).await.unwrap()).unwrap();
    assert!(result["document"]["financial_transaction_entry_id"].is_null());
    assert_eq!(
        result["linked_purchase_entries"].as_array().unwrap().len(),
        2
    );
    assert_eq!(
        result["linked_purchase_entries"][0]["sources"],
        json!(["purchase_entry_document_reference"])
    );
    assert_eq!(result["attachments"][0]["pdf_filename"], true);
    assert!(result["attachments"][0].get("url").is_none());
    assert!(result.get("is_unbooked").is_none());
    let requests = server.finish();
    assert_eq!(
        query(&requests[2], "$filter"),
        Some(format!("Document eq guid'{}'", id(1)))
    );
    assert!(
        requests
            .iter()
            .all(|request| request.target.starts_with("GET /api/v1/42/"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intake_details_preserve_direct_reference_resolution_and_missing_evidence() {
    for found in [false, true] {
        let mut doc = document(1);
        doc["FinancialTransactionEntryID"] = json!(id(200));
        let mut linked = entry(200);
        linked["Document"] = Value::Null;
        let server = Server::new(vec![
            rows(vec![doc]),
            rows(vec![]),
            rows(if found { vec![linked] } else { vec![] }),
        ]);
        let (_directory, backend) = financial_backend(&server.base).await;
        let result =
            serde_json::to_value(backend.get_purchase_document(get_args()).await.unwrap()).unwrap();
        if found {
            assert!(result["unresolved_transaction_reference"].is_null());
            assert_eq!(
                result["linked_purchase_entries"][0]["sources"],
                json!(["document_transaction_reference"])
            );
        } else {
            assert_eq!(
                result["unresolved_transaction_reference"],
                id(200).to_string()
            );
            assert_eq!(result["linked_purchase_entries"], json!([]));
        }
        let requests = server.finish();
        assert_eq!(
            query(&requests[2], "$filter"),
            Some(format!(
                "Document eq guid'{}' or EntryID eq guid'{}'",
                id(1),
                id(200)
            ))
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intake_details_validate_document_attachment_and_entry_scope_without_partial_success() {
    for docs in [
        vec![],
        vec![document(1), document(1)],
        vec![document(2)],
        vec![{
            let mut value = document(1);
            value["Type"] = json!(15);
            value
        }],
    ] {
        let server = Server::new(vec![rows(docs)]);
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(backend.get_purchase_document(get_args()).await.is_err());
        server.finish();
    }
    for attachments in [
        vec![attachment(10), attachment(10)],
        vec![{
            let mut value = attachment(10);
            value["Document"] = json!(id(2));
            value
        }],
        vec![{
            let mut value = attachment(10);
            value["ID"] = json!(Uuid::nil());
            value
        }],
    ] {
        let server = Server::new(vec![rows(vec![document(1)]), rows(attachments)]);
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(backend.get_purchase_document(get_args()).await.is_err());
        server.finish();
    }
    for entries in [
        vec![entry(200), entry(200)],
        vec![{
            let mut value = entry(200);
            value["Division"] = json!(99);
            value
        }],
        vec![{
            let mut value = entry(200);
            value["Supplier"] = json!(id(101));
            value
        }],
        vec![{
            let mut value = entry(200);
            value["Document"] = json!(id(2));
            value
        }],
        vec![{
            let mut value = entry(200);
            value["EntryID"] = json!(Uuid::nil());
            value
        }],
        vec![{
            let mut value = entry(200);
            value["EntryNumber"] = json!(0);
            value
        }],
    ] {
        let server = Server::new(vec![rows(vec![document(1)]), rows(vec![]), rows(entries)]);
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(backend.get_purchase_document(get_args()).await.is_err());
        server.finish();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intake_search_faults_and_empty_or_exhausted_windows_are_explicit() {
    let server = Server::new(vec![
        Response::json(json!({"d":{"results":[document(1)],"__next":"?page=2"}})),
        Response {
            status: "403 Forbidden",
            body: "private financial source".into(),
            location: None,
        },
    ]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let error = backend
        .search_purchase_documents(search_args(json!({})))
        .await
        .err()
        .unwrap();
    assert!(!format!("{error:#}").contains("private financial source"));
    server.finish();
    for documents in [vec![], vec![document(1)]] {
        let count = documents.len();
        let server = Server::new(vec![rows(documents)]);
        let (_directory, backend) = financial_backend(&server.base).await;
        let result = serde_json::to_value(
            backend
                .search_purchase_documents(search_args(json!({"offset":50})))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["source_document_count"], count);
        assert_eq!(result["matched_document_count"], count);
        assert_eq!(result["documents"], json!([]));
        assert_eq!(result["has_more"], false);
        assert!(result["next_offset"].is_null());
        server.finish();
    }
}

fn download_responses() -> Vec<Response> {
    vec![
        rows(vec![document(1)]),
        rows(vec![attachment(10)]),
        pdf_response(b"%PDF-1.7\nInvented intake PDF\n%%EOF\n"),
        rows(vec![attachment(10)]),
        rows(vec![document(1)]),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intake_pdf_download_needs_no_accounting_entry_uses_safe_private_storage_and_no_urls() {
    let mut replies = download_responses();
    replies.extend(download_responses());
    let server = Server::new(replies);
    let (directory, backend) = financial_backend(&server.base).await;
    let mut paths = Vec::new();
    for _ in 0..2 {
        let result = serde_json::to_value(
            backend
                .download_purchase_document_pdf(download_args())
                .await
                .unwrap(),
        )
        .unwrap();
        let path = std::path::PathBuf::from(result["saved_path"].as_str().unwrap());
        assert!(
            path.starts_with(
                directory
                    .path()
                    .canonicalize()
                    .unwrap()
                    .join("downloads/42")
            )
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"%PDF-1.7\nInvented intake PDF\n%%EOF\n"
        );
        assert!(!path.to_string_lossy().contains("untrusted"));
        assert!(!result.to_string().contains("%PDF"));
        assert!(!result.to_string().contains("untrusted.invalid"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        paths.push(path);
    }
    assert_ne!(paths[0], paths[1]);
    let requests = server.finish();
    for (index, request) in requests.iter().enumerate() {
        assert!(request.target.starts_with("GET "));
        assert!(request.body.is_empty());
        assert!(!request.target.contains("purchaseentry"));
        if index % 5 == 2 {
            assert!(request.target.starts_with("GET /docs/SysAttachment.aspx?"));
        }
        assert!(!request.target.contains("untrusted.invalid"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intake_pdf_download_rejects_ambiguous_or_changed_metadata_and_invalid_content() {
    for mode in [
        "not_linked",
        "wrong_type",
        "invalid_pdf",
        "changed_attachment",
        "changed_proposal",
        "changed_reference",
        "changed_modified",
    ] {
        let mut replies = download_responses();
        match mode {
            "not_linked" => {
                replies[1] = rows(vec![attachment(11)]);
                replies.truncate(2);
            }
            "wrong_type" => {
                let mut doc = document(1);
                doc["Type"] = json!(15);
                replies[0] = rows(vec![doc]);
                replies.truncate(1);
            }
            "invalid_pdf" => {
                replies[2] = pdf_response(b"not a PDF");
                replies.truncate(3);
            }
            "changed_attachment" => {
                let mut value = attachment(10);
                value["Document"] = json!(id(2));
                replies[3] = rows(vec![value]);
                replies.truncate(4);
            }
            _ => {
                let mut doc = document(1);
                match mode {
                    "changed_proposal" => doc["ProposedEntryStatus"] = json!(50),
                    "changed_reference" => doc["FinancialTransactionEntryID"] = json!(id(200)),
                    _ => doc["Modified"] = json!("2025-01-01T10:01:03"),
                };
                replies[4] = rows(vec![doc]);
            }
        }
        let server = Server::new(replies);
        let (directory, backend) = financial_backend(&server.base).await;
        assert!(
            backend
                .download_purchase_document_pdf(download_args())
                .await
                .is_err(),
            "{mode}"
        );
        assert!(!directory.path().join("downloads").exists());
        server.finish();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intake_details_include_all_attachment_and_link_pages_and_keep_missing_accounts_unknown() {
    let mut doc = document(1);
    doc["Account"] = Value::Null;
    let server = Server::new(vec![
        rows(vec![doc]),
        Response::json(json!({"d":{"results":[attachment(10)],"__next":"?page=2"}})),
        rows(vec![attachment(11)]),
        Response::json(json!({"d":{"results":[entry(200)],"__next":"?page=2"}})),
        rows(vec![entry(201)]),
    ]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let result =
        serde_json::to_value(backend.get_purchase_document(get_args()).await.unwrap()).unwrap();
    assert!(result["document"]["account_id"].is_null());
    assert_eq!(result["attachments"].as_array().unwrap().len(), 2);
    assert_eq!(
        result["linked_purchase_entries"].as_array().unwrap().len(),
        2
    );
    server.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intake_large_search_keeps_late_global_window_without_truncating_counts() {
    let mut replies = Vec::new();
    for page in 0..20 {
        let documents: Vec<_> = (0..500)
            .map(|index| document(10000 - (page * 500 + index)))
            .collect();
        let mut body = json!({"d":{"results":documents}});
        if page < 19 {
            body["d"]["__next"] = json!(format!("?page={}", page + 2));
        }
        replies.push(Response::json(body));
    }
    let server = Server::new(replies);
    let (_directory, backend) = financial_backend(&server.base).await;
    let result = serde_json::to_value(
        backend
            .search_purchase_documents(search_args(json!({"limit":3,"offset":9})))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["source_document_count"], 10000);
    assert_eq!(result["matched_document_count"], 10000);
    assert_eq!(result["documents"][0]["id"], id(10).to_string());
    assert_eq!(result["documents"][2]["id"], id(12).to_string());
    assert_eq!(result["has_more"], true);
    assert_eq!(result["next_offset"], 12);
    assert_eq!(server.finish().len(), 20);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intake_scope_change_prevents_reads_and_local_downloads() {
    let server = Server::new(vec![]);
    let (directory, backend) = financial_backend(&server.base).await;
    {
        let store = crate::store::Store::open(directory.path()).await.unwrap();
        let mut session = store.require_session().await.unwrap();
        session.administration.as_mut().unwrap().code = 99;
        store.save(&session).await.unwrap();
    }
    assert!(
        backend
            .search_purchase_documents(search_args(json!({})))
            .await
            .is_err()
    );
    assert!(backend.get_purchase_document(get_args()).await.is_err());
    assert!(
        backend
            .download_purchase_document_pdf(download_args())
            .await
            .is_err()
    );
    assert!(!directory.path().join("downloads").exists());
    server.finish();
}

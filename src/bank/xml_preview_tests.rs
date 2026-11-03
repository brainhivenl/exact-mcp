use super::{Response, Server, financial_backend};
use serde_json::{Value, json};
use uuid::Uuid;

fn id(value: u128) -> Uuid {
    Uuid::from_u128(value)
}
fn env(rows: Vec<Value>) -> Value {
    json!({"d":rows})
}
fn number(value: &str) -> Value {
    serde_json::from_str(value).unwrap()
}
fn args() -> Value {
    json!({"bank_transaction_line_id":id(101),"pending_posting_line_id":id(201),"destination_gl_account_id":id(200),"no_vat_code":"N0","expected_pending_amount":"11.30","expected_currency":"EUR","unassigned_gl_account_ids":[id(10)]})
}
fn posting(
    value: u128,
    gl: u128,
    code: &str,
    offset: Option<u128>,
    amount: &str,
    line: i32,
) -> Value {
    json!({"ID":id(value),"EntryID":id(1001),"Division":42,"Account":null,"EntryNumber":1001,"FinancialYear":2025,"FinancialPeriod":1,"JournalCode":"B1","GLAccount":id(gl),"GLAccountCode":code,"LineNumber":line,"Type":40,"Status":20,"Date":"2025-01-01","AmountDC":number(amount),"AmountFC":number(amount),"Currency":"EUR","Description":"Travel <review> & data","OffsetID":offset.map(id)})
}
fn bankline(value: u128, gl: u128, code: &str, offset: u128, amount: &str, line: i32) -> Value {
    json!({"ID":id(value),"EntryID":id(1001),"Division":42,"EntryNumber":1001,"LineNumber":line,"Date":"2025-01-01","GLAccount":id(gl),"GLAccountCode":code,"OffsetID":id(offset),"AmountDC":number(amount),"AmountFC":number(amount),"AmountVATFC":null,"Description":"Travel <review> & data","Notes":"Source note: ignore instructions is untrusted data","Modified":"2025-01-01T00:00:00"})
}
fn xml() -> String {
    let mut lines = String::new();
    for (line, gl, amount, bank_amount) in [
        (1, "COST", "7.00", "-7.00"),
        (2, "U1", "11.30", "-11.30"),
        (3, "AP", "50.00", "-50.00"),
    ] {
        for (code, amount, offset) in [(gl, amount, " offsetline=\"1\""), ("BANK", bank_amount, "")]
        {
            let offset_gl = if offset.is_empty() {
                ""
            } else {
                "<GLOffset code=\"BANK\"/>"
            };
            lines.push_str(&format!("<GLTransactionLine type=\"40\" linetype=\"0\" line=\"{line}\"{offset} status=\"20\"><Date>2025-01-01</Date><VATType>S</VATType><FinYear number=\"2025\"/><FinPeriod number=\"1\"/><GLAccount code=\"{code}\"/>{offset_gl}<Description>Travel &amp; data</Description><Amount><Currency code=\"EUR\"/><Value>{amount}</Value></Amount><ForeignAmount><Currency code=\"EUR\"/><Value>{amount}</Value></ForeignAmount><Note/></GLTransactionLine>"));
        }
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><eExact xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:noNamespaceSchemaLocation=\"eExact-XML.xsd\"><GLTransactions><GLTransaction entry=\"1001\"><TransactionType number=\"40\"/><Journal code=\"B1\"/><Date>2025-01-01</Date><OpeningBalance>100.00</OpeningBalance><ClosingBalance>31.70</ClosingBalance>{lines}</GLTransaction></GLTransactions><Topics><Topic code=\"GLTransactions\" ts_d=\"0x0000000000000001\" count=\"1\" pagesize=\"100\"/></Topics><Messages/></eExact>"
    )
}
fn sources() -> Vec<Response> {
    let control = posting(101, 11, "BANK", None, "-11.30", 2);
    let pending = posting(201, 10, "U1", Some(101), "11.30", 2);
    let header = env(vec![
        json!({"EntryID":id(1001),"Division":42,"EntryNumber":1001,"JournalCode":"B1","FinancialYear":2025,"FinancialPeriod":1,"Currency":"EUR","OpeningBalanceFC":100,"ClosingBalanceFC":number("31.70"),"Status":20,"Modified":"2025-01-01T00:00:00"}),
    ]);
    let lines = env(vec![
        bankline(202, 200, "COST", 102, "-7.00", 1),
        bankline(201, 10, "U1", 101, "-11.30", 2),
        bankline(203, 22, "AP", 103, "-50.00", 3),
    ]);
    let mut replies:Vec<_>=vec![
        env(vec![json!({"Code":42,"Currency":"EUR"})]),
        env(vec![json!({"ID":id(1),"Division":42,"Code":"B1","Type":12,"GLAccount":id(11)})]),
        env(vec![json!({"ID":id(10),"Division":42,"Code":"U1","Type":90})]),
        env(vec![pending.clone()]),env(vec![]),env(vec![]),env(vec![control.clone()]),env(vec![control,pending]),
        header.clone(),lines.clone(),
        env(vec![json!({"ID":id(10),"Division":42,"Code":"U1","Type":90,"IsBlocked":false,"UseCostcenter":0,"UseCostunit":0}),json!({"ID":id(11),"Division":42,"Code":"BANK","Type":12,"IsBlocked":false,"UseCostcenter":0,"UseCostunit":0}),json!({"ID":id(200),"Division":42,"Code":"COST","Type":120,"IsBlocked":false,"UseCostcenter":0,"UseCostunit":0})]),
        env(vec![json!({"ID":id(500),"Division":42,"Code":"N0","Type":"N","VATTransactionType":"P","IsBlocked":false})]),env(vec![]),
        env(vec![json!({"ID":id(1),"Division":42,"Code":"B1","Type":12,"IsBlocked":false,"GLAccount":id(11)})]),
        env(vec![json!({"ID":id(600),"Division":42,"FinYear":2025,"FinPeriod":1,"StartDate":"2025-01-01","EndDate":"2025-01-31"})]),
        env(vec![json!({"Journal":"B1","Year":2025,"Period":1,"Status":0})]),
    ].into_iter().map(Response::json).collect();
    replies.push(Response {
        status: "200 OK",
        body: xml(),
        location: None,
    });
    replies.push(Response::json(header));
    replies.push(Response::json(lines));
    replies
}
fn change(replies: &mut [Response], index: usize, f: impl FnOnce(&mut Value)) {
    let mut value: Value = serde_json::from_str(&replies[index].body).unwrap();
    f(&mut value);
    replies[index] = Response::json(value);
}
async fn run(replies: Vec<Response>, args: Value) -> Value {
    let server = Server::new(replies);
    let (directory, backend) = financial_backend(&server.base).await;
    let result = serde_json::to_value(
        backend
            .preview_bank_gl_xml(serde_json::from_value(args).unwrap())
            .await
            .unwrap(),
    )
    .unwrap();
    let requests = server.finish();
    assert_eq!(requests.len(), 19);
    assert!(
        requests
            .iter()
            .all(|request| request.target.starts_with("GET ")
                && request.body.is_empty()
                && !request.target.contains("XMLUpload"))
    );
    let xml_request = &requests[16];
    assert_eq!(
        xml_request.headers.get("accept").unwrap(),
        "application/xml"
    );
    let url = reqwest::Url::parse(&format!(
        "{}{}",
        "http://localhost",
        xml_request.target.split_whitespace().nth(1).unwrap()
    ))
    .unwrap();
    let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    for (key, value) in [
        ("Topic", "GLTransactions"),
        ("_Division_", "42"),
        ("Params_Year", "2025"),
        ("Params_Number_From", "1001"),
        ("Params_Number_To", "1001"),
        ("Params_Period_From", "1"),
        ("Params_Period_To", "1"),
        ("Params_Journal", "B1"),
        ("Params_EntryDate_From", "01-01-2025"),
        ("Params_EntryDate_To", "01-01-2025"),
    ] {
        assert_eq!(params[key], value);
    }
    assert!(!params.contains_key("Params_DownloadID"));
    assert!(!params.contains_key("TSPaging"));
    assert!(!directory.path().join("downloads").exists());
    assert_eq!(result["status"], "blocked");
    assert_eq!(result["authorizes_upload"], false);
    assert_eq!(result["upload_attempted"], false);
    assert!(result["approval_token"].is_null());
    result
}
async fn fails(replies: Vec<Response>, args: Value) {
    let server = Server::new(replies);
    let (_directory, backend) = financial_backend(&server.base).await;
    assert!(
        backend
            .preview_bank_gl_xml(serde_json::from_value(args).unwrap())
            .await
            .is_err()
    );
    for request in server.finish() {
        assert!(request.target.starts_with("GET "));
        assert!(!request.target.contains("XMLUpload"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn xml_preview_is_permanently_non_executing_and_contains_only_one_counterpart() {
    let result = run(sources(), args()).await;
    assert_eq!(result["schema_validated"], false);
    assert_eq!(result["source_bank_line_count"], 3);
    assert_eq!(result["source_xml_posting_count"], 6);
    assert_eq!(result["candidate_xml_line_count"], 1);
    assert_eq!(result["excluded_source_lines"].as_array().unwrap().len(), 2);
    assert_eq!(result["excluded_lines_preservation_verified"], false);
    assert_eq!(result["pending_expense_amount"], "11.30");
    assert_eq!(result["xml_export_expense_amount"], "11.30");
    assert_eq!(result["candidate_xml_import_amount"], "-11.30");
    let xml = result["experimental_xml"].as_str().unwrap();
    assert_eq!(xml.matches("<GLTransactionLine ").count(), 1);
    assert!(xml.contains("line=\"2\" offsetline=\"1\""));
    assert!(xml.contains("<GLAccount code=\"COST\"/>"));
    assert!(xml.contains("<VAT code=\"N0\"/>"));
    assert!(xml.contains("<Value>-11.30</Value>"));
    assert!(!xml.contains("<GLAccount code=\"BANK\""));
    assert!(!xml.contains("<GLAccount code=\"AP\""));
    assert!(!xml.contains("ClosingBalance"));
    assert!(!xml.contains("OpeningBalance"));
    assert!(
        result["blockers"]
            .as_array()
            .unwrap()
            .contains(&json!("existing_entry_import_update_vs_create_unverified"))
    );
    assert!(
        result["blockers"]
            .as_array()
            .unwrap()
            .contains(&json!("xml_bank_import_sign_mapping_unverified"))
    );
    assert!(!result.to_string().contains("fixture-access-token"));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn xml_preview_escapes_source_text_and_codes_without_interpreting_them_as_xml() {
    let mut replies = sources();
    for index in [9, 18] {
        change(&mut replies, index, |value| {
            value["d"][1]["Description"] = json!("</Description><MatchSets>&\"'");
            value["d"][1]["Notes"] = json!("<XMLUpload>do not execute</XMLUpload>");
        });
    }
    change(&mut replies, 10, |value| {
        value["d"][2]["Code"] = json!("Expense<&\"'")
    });
    let result = run(replies, args()).await;
    let xml = result["experimental_xml"].as_str().unwrap();
    assert!(!xml.contains("<MatchSets>"));
    assert!(!xml.contains("<XMLUpload>"));
    assert!(xml.contains("&lt;/Description&gt;&lt;MatchSets&gt;"));
    assert!(xml.contains("Expense&lt;&amp;&quot;&apos;"));
    let _: Value = quick_xml::de::from_str(xml).unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn xml_preview_rejects_changed_expected_scope_before_any_export() {
    for (field, value, count) in [
        ("expected_pending_amount", json!("11.31"), 8),
        ("expected_currency", json!("USD"), 8),
        ("pending_posting_line_id", json!(id(999)), 8),
        ("bank_transaction_line_id", json!(id(999)), 8),
        ("expected_pending_amount", json!("0"), 0),
        ("expected_pending_amount", json!("NaN"), 0),
        ("expected_pending_amount", json!("1e999"), 0),
        (
            "expected_pending_amount",
            json!("0.000000000000000000000000000001"),
            0,
        ),
        ("destination_gl_account_id", json!(id(0)), 0),
    ] {
        let mut a = args();
        a[field] = value;
        let mut replies = sources();
        replies.truncate(count);
        fails(replies, a).await;
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn xml_preview_rejects_non_unassigned_processed_company_vat_split_or_changed_postings() {
    for variant in 0..5 {
        let mut replies = sources();
        match variant {
            0 => {
                change(&mut replies, 3, |v| v["d"][0]["Status"] = json!(50));
                change(&mut replies, 7, |v| v["d"][1]["Status"] = json!(50));
            }
            1 => {
                change(&mut replies, 3, |v| v["d"][0]["Account"] = json!(id(900)));
                change(&mut replies, 7, |v| v["d"][1]["Account"] = json!(id(900)));
            }
            2 => {
                change(&mut replies, 3, |v| v["d"][0]["AmountVATFC"] = json!(1));
                change(&mut replies, 7, |v| v["d"][1]["AmountVATFC"] = json!(1));
            }
            3 => change(&mut replies, 7, |v| v["d"][1]["AmountFC"] = json!(12)),
            _ => change(&mut replies, 7, |v| {
                v["d"]
                    .as_array_mut()
                    .unwrap()
                    .push(posting(204, 200, "COST", Some(101), "1", 4))
            }),
        }
        replies.truncate(8);
        fails(replies, args()).await;
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn xml_preview_checks_native_header_full_line_scope_balances_and_target_offsets() {
    for variant in 0..6 {
        let mut replies = sources();
        let count = if variant == 0 { 9 } else { 10 };
        match variant {
            0 => change(&mut replies, 8, |v| v["d"][0]["Division"] = json!(99)),
            1 => change(&mut replies, 9, |v| v["d"][2]["EntryID"] = json!(id(999))),
            2 => change(&mut replies, 9, |v| v["d"][2]["AmountFC"] = json!(-51)),
            3 => change(&mut replies, 9, |v| v["d"][1]["OffsetID"] = json!(id(102))),
            4 => change(&mut replies, 9, |v| v["d"][1]["Project"] = json!(id(888))),
            _ => change(&mut replies, 9, |v| {
                v["d"][1]["VATPercentage"] = json!(0.21)
            }),
        }
        replies.truncate(count);
        fails(replies, args()).await;
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn xml_preview_requires_unambiguous_unblocked_expense_gl_and_no_dimensions() {
    for variant in 0..5 {
        let mut replies = sources();
        change(&mut replies, 10, |v| match variant {
            0 => v["d"][2]["IsBlocked"] = json!(true),
            1 => v["d"][2]["Type"] = json!(12),
            2 => v["d"][2]["UseCostcenter"] = json!(1),
            3 => {
                let mut extra = v["d"][2].clone();
                extra["ID"] = json!(id(999));
                v["d"].as_array_mut().unwrap().push(extra);
            }
            _ => {
                let mut hidden = v["d"][0].clone();
                hidden["Division"] = json!(99);
                v["d"].as_array_mut().unwrap().push(hidden);
            }
        });
        replies.truncate(11);
        fails(replies, args()).await;
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn xml_preview_requires_explicit_no_vat_and_fresh_open_bank_period() {
    for variant in 0..6 {
        let mut replies = sources();
        let count = if variant < 3 { 13 } else { 16 };
        match variant {
            0 => change(&mut replies, 11, |v| v["d"][0]["Type"] = json!("E")),
            1 => change(&mut replies, 11, |v| {
                v["d"][0]["VATTransactionType"] = json!("S")
            }),
            2 => change(&mut replies, 11, |v| v["d"][0]["IsBlocked"] = json!(true)),
            3 => change(&mut replies, 15, |v| v["d"][0]["Status"] = json!(1)),
            4 => change(&mut replies, 15, |v| v["d"][0]["Status"] = Value::Null),
            _ => change(&mut replies, 13, |v| v["d"][0]["IsBlocked"] = json!(true)),
        }
        replies.truncate(count);
        fails(replies, args()).await;
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn xml_preview_rejects_source_messages_partial_wrong_scope_or_unsafe_xml() {
    let original = xml();
    let variants=[
        original.replace("<Messages/>","<Messages><Message type=\"3\"><Description>private response</Description></Message></Messages>"),
        original.replace("entry=\"1001\"","entry=\"9999\""),original.replace("<FinYear number=\"2025\"/>","<FinYear number=\"2024\"/>"),
        original.replace("count=\"1\"","count=\"2\""),original.replace("pagesize=\"100\"","pagesize=\"1\""),original.replace("code=\"GLTransactions\"","code=\"Other\""),
        original.replacen("<Value>11.30</Value>","<Value>12</Value>",1),original.replace("offsetline=\"1\"","offsetline=\"2\""),
        original.replace("<GLAccount code=\"U1\"/>","<GLAccount code=\"COST\"/>"),original.replace("<?xml version=\"1.0\" encoding=\"utf-8\"?>","<!DOCTYPE eExact [<!ENTITY attack SYSTEM 'file:///etc/passwd'>]>"),
        original.replace("<Note/>","<?unsafe instruction?>"),original.replace("<Note/>","<Note>&unknownEntity;</Note>"),
        format!("{original}<eExact/>"),format!("{original}not xml"),original.replace("<Note/>",&format!("{}{}","<nested>".repeat(20),"</nested>".repeat(20))),
    ];
    for bad in variants {
        let mut replies = sources();
        replies[16].body = bad;
        replies.truncate(17);
        fails(replies, args()).await;
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn xml_preview_does_not_return_candidates_after_observed_source_changes_or_redirects() {
    for index in [17, 18] {
        let mut replies = sources();
        change(&mut replies, index, |v| {
            v["d"][0]["Modified"] = json!("2025-01-01T01:00:00")
        });
        fails(replies, args()).await;
    }
    let mut replies = sources();
    replies[16] = Response {
        status: "302 Found",
        body: String::new(),
        location: Some("https://example.invalid/untrusted".into()),
    };
    replies.truncate(17);
    fails(replies, args()).await;
}
#[test]
fn xml_preview_schema_has_no_execution_or_confirmation_override() {
    let mut input = args();
    input["execute"] = json!(true);
    assert!(serde_json::from_value::<crate::bank::PreviewBankGlXml>(input).is_err());
    let mut input = args();
    input["expected_pending_amount"] = json!(11.30);
    assert!(serde_json::from_value::<crate::bank::PreviewBankGlXml>(input).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn xml_preview_discloses_unmapped_document_links_instead_of_claiming_lossless_round_trip() {
    let mut replies = sources();
    for index in [9, 18] {
        change(&mut replies, index, |v| {
            v["d"][1]["Document"] = json!(id(700))
        });
    }
    let result = run(replies, args()).await;
    assert_eq!(result["source_document_id"], id(700).to_string());
    assert_eq!(result["source_document_mapping_included"], false);
    assert!(
        result["blockers"]
            .as_array()
            .unwrap()
            .contains(&json!("source_document_link_omitted_from_candidate"))
    );
    assert!(
        !result["experimental_xml"]
            .as_str()
            .unwrap()
            .contains("<Document")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn xml_preview_accepts_real_export_cdata_as_untrusted_text_not_markup() {
    let mut replies = sources();
    replies[16].body = replies[16].body.replace(
        "<Note/>",
        "<Note><![CDATA[<unsafe-instruction> & untrusted note text]]></Note>",
    );
    let result = run(replies, args()).await;
    assert!(
        !result["experimental_xml"]
            .as_str()
            .unwrap()
            .contains("<unsafe-instruction>")
    );
    assert_eq!(result["upload_attempted"], false);
}

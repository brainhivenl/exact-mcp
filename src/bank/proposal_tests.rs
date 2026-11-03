use super::{Response, Server, financial_backend};
use serde_json::{Value, json};
use uuid::Uuid;

fn id(value: u128) -> Uuid {
    Uuid::from_u128(value)
}
fn env(rows: Vec<Value>) -> Value {
    json!({"d":rows})
}
fn posting(value: u128, gl: u128, offset: Option<u128>, amount: i64) -> Value {
    json!({"ID":id(value),"EntryID":id(1001),"Division":42,"Account":null,"EntryNumber":1001,"FinancialYear":2025,"FinancialPeriod":1,"JournalCode":"B1","GLAccount":id(gl),"LineNumber":value,"Type":40,"Status":50,"Date":"2025-01-01","AmountDC":amount,"AmountFC":amount,"Currency":"EUR","Description":"Payment INV-001","OffsetID":offset.map(id)})
}
fn report(hid: &str, entry: i64, amount: i64) -> Value {
    json!({"HID":hid,"AccountId":id(900),"EntryNumber":entry,"JournalCode":"P1","Amount":amount,"AmountInTransit":0,"CurrencyCode":"EUR","YourRef":"INV-001"})
}
fn header(value: u128, entry: i64) -> Value {
    json!({"EntryID":id(value),"Division":42,"Supplier":id(900),"EntryNumber":entry,"Journal":"P1","Currency":"EUR","YourRef":"INV-001","InvoiceNumber":entry})
}
fn args() -> Value {
    json!({"bank_transaction_line_id":id(101),"pending_posting_line_id":id(201),"unassigned_gl_account_ids":[id(10)],"account_ids":[id(900)]})
}
fn allocation(hid: &str, entry: u128, amount: &str) -> Value {
    json!({"account_id":id(900),"side":"payable","entry_id":id(entry),"open_item_id":hid,"amount":amount})
}
fn gl() -> Value {
    json!({"gl_account_id":id(200),"net_amount":"3.00","vat_amount":"0.00","vat_code":"N","vat_side":"payable","rationale":"Explicit test bank fee rationale, not inferred from a difference"})
}
fn sources() -> Vec<Value> {
    let control = posting(101, 11, None, -224);
    let pending = posting(201, 10, Some(101), 124);
    vec![
        env(vec![json!({"Code":42,"Currency":"EUR"})]),
        env(vec![
            json!({"ID":id(1),"Division":42,"Code":"B1","Type":12,"GLAccount":id(11)}),
        ]),
        env(vec![
            json!({"ID":id(10),"Division":42,"Code":"UNASSIGNED","Type":90}),
            json!({"ID":id(22),"Division":42,"Code":"AP","Type":22}),
            json!({"ID":id(20),"Division":42,"Code":"AR","Type":20}),
        ]),
        env(vec![pending.clone()]),
        env(vec![report("1", 3001, 121)]),
        env(vec![]),
        env(vec![control.clone()]),
        env(vec![control, pending, posting(301, 200, Some(101), 100)]),
        env(vec![
            json!({"ID":id(900),"Division":42,"Code":"SUP","Name":"Invented supplier"}),
        ]),
        env(vec![header(401, 3001)]),
    ]
}
fn gl_sources() -> Vec<Value> {
    vec![
        env(vec![
            json!({"ID":id(200),"Division":42,"Code":"BANK-FEE","Type":120,"IsBlocked":false,"UseCostcenter":0,"UseCostunit":0}),
        ]),
        env(vec![
            json!({"ID":id(500),"Division":42,"Code":"N","Type":"N","IsBlocked":false,"VATTransactionType":"P"}),
        ]),
        env(vec![]),
        env(vec![
            json!({"ID":id(1),"Division":42,"Code":"B1","Type":12,"IsBlocked":false,"GLAccount":id(11)}),
        ]),
        env(vec![
            json!({"ID":id(600),"Division":42,"FinYear":2025,"FinPeriod":1,"StartDate":"2025-01-01","EndDate":"2025-01-31"}),
        ]),
        env(vec![
            json!({"Journal":"B1","Year":2025,"Period":1,"Status":0}),
        ]),
    ]
}
async fn run(bodies: Vec<Value>, args: Value) -> Value {
    let server = Server::new(bodies.into_iter().map(Response::json).collect());
    let (directory, backend) = financial_backend(&server.base).await;
    let result = serde_json::to_value(
        backend
            .propose_bank_transaction_handling(serde_json::from_value(args).unwrap())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["authorizes_handling"], false);
    assert!(!directory.path().join("downloads").exists());
    for request in server.finish() {
        assert!(request.target.starts_with("GET /api/v1/42/"));
        assert!(request.body.is_empty());
        for forbidden in [
            "/project/",
            "/logistics/",
            "/purchase/PurchaseInvoices",
            "/cashflow/Payments",
            "SysAttachment",
            "XMLDownload",
        ] {
            assert!(!request.target.contains(forbidden));
        }
    }
    result
}
fn has(result: &Value, group: &str, code: &str) -> bool {
    result[group]
        .as_array()
        .unwrap()
        .iter()
        .any(|finding| finding["code"] == code)
}
async fn fails(bodies: Vec<Value>, args: Value) {
    let server = Server::new(bodies.into_iter().map(Response::json).collect());
    let (_directory, backend) = financial_backend(&server.base).await;
    assert!(
        backend
            .propose_bank_transaction_handling(serde_json::from_value(args).unwrap())
            .await
            .is_err()
    );
    server.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bank_proposal_candidates_are_not_selected_and_pending_budget_excludes_already_handled_lines()
 {
    let result = run(sources(), args()).await;
    assert_eq!(result["status"], "review_required");
    assert_eq!(result["budget"]["amount"], "124");
    assert_eq!(result["budget"]["residual"], "124");
    assert_eq!(
        result["bank_transaction"]["amount_transaction_currency"],
        "-224"
    );
    assert_eq!(
        result["bank_transaction"]["booking_lines"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(result["candidate_group_count"], 1);
    assert_eq!(
        result["candidate_groups"][0]["reference_literal_present"],
        true
    );
    assert_eq!(
        result["candidate_groups"][0]["header_candidates"][0]["entry_id"],
        id(401).to_string()
    );
    assert!(has(&result, "unknowns", "invoice_candidates_not_selected"));
    assert!(has(&result, "unknowns", "handling_plan_not_specified"));
    assert!(result["allocation_evidence"].as_array().unwrap().is_empty());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bank_proposal_mixed_invoice_and_explicit_gl_splits_reconcile_without_applying_or_authorizing()
 {
    let mut a = args();
    a["invoice_allocations"] = json!([allocation("1", 401, "121.00")]);
    a["gl_splits"] = json!([gl()]);
    let mut bodies = sources();
    bodies.extend(gl_sources());
    let result = run(bodies, a).await;
    assert_eq!(result["status"], "review_required");
    assert_eq!(result["budget"]["exact_totals_match"], true);
    assert_eq!(result["budget"]["proposed_total"], "124.00");
    assert_eq!(result["budget"]["residual"], "0.00");
    assert_eq!(
        result["allocation_evidence"][0]["magnitude_comparison"],
        "same_outstanding_magnitude"
    );
    assert!(has(
        &result,
        "unknowns",
        "direct_gl_tax_and_evidence_unverified"
    ));
    assert!(has(
        &result,
        "unknowns",
        "matching_signs_and_membership_unverified"
    ));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bank_proposal_residuals_overallocations_wrong_signs_and_partial_invoices_stay_explicit() {
    for (amount, block, relation) in [
        ("100.00", true, "partial_outstanding_magnitude"),
        ("121.00", true, "same_outstanding_magnitude"),
        ("124.00", true, "exceeds_outstanding_magnitude"),
        ("-121.00", true, "same_outstanding_magnitude"),
    ] {
        let mut a = args();
        a["invoice_allocations"] = json!([allocation("1", 401, amount)]);
        let result = run(sources(), a).await;
        assert_eq!(
            result["allocation_evidence"][0]["magnitude_comparison"],
            relation
        );
        assert_eq!(
            result["status"],
            if block { "blocked" } else { "review_required" }
        );
        if amount == "124.00" {
            assert!(has(
                &result,
                "blockers",
                "allocation_exceeds_current_outstanding_magnitude"
            ));
        } else {
            assert!(has(
                &result,
                "blockers",
                "handling_budget_does_not_reconcile"
            ));
        }
        assert!(
            result["selected_gl_accounts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bank_proposal_multi_invoice_and_credit_signed_splits_require_explicit_choices_and_never_net_source_balances()
 {
    let mut bodies = sources();
    bodies[4] = env(vec![report("1", 3001, 150), report("2", 3002, -26)]);
    bodies[9] = env(vec![header(401, 3001), header(402, 3002)]);
    let mut a = args();
    a["invoice_allocations"] = json!([allocation("1", 401, "150"), allocation("2", 402, "-26")]);
    let result = run(bodies, a).await;
    assert_eq!(result["budget"]["exact_totals_match"], true);
    assert_eq!(
        result["allocation_evidence"][1]["outstanding_amount"],
        "-26"
    );
    assert!(has(
        &result,
        "unknowns",
        "matching_signs_and_membership_unverified"
    ));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bank_proposal_reused_accounting_numbers_are_not_resolved_by_amount_reference_or_explicit_guessed_header()
 {
    let mut bodies = sources();
    bodies[9] = env(vec![header(401, 3001), header(402, 3001)]);
    let mut a = args();
    a["invoice_allocations"] = json!([allocation("1", 401, "121")]);
    let result = run(bodies, a).await;
    assert_eq!(result["unresolved_header_group_count"], 1);
    assert_eq!(
        result["candidate_groups"][0]["identity_resolution"],
        "ambiguous_invoice_headers"
    );
    assert!(has(
        &result,
        "blockers",
        "allocation_identity_not_unique_current_open_invoice"
    ));
    let mut bodies = sources();
    bodies[9] = env(vec![]);
    let result = run(bodies, args()).await;
    assert_eq!(
        result["candidate_groups"][0]["identity_resolution"],
        "missing_invoice_header"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bank_proposal_foreign_currency_null_amounts_and_transit_are_not_converted_subtracted_or_cleared()
 {
    for (field, value) in [
        ("CurrencyCode", json!("USD")),
        ("Amount", Value::Null),
        ("AmountInTransit", json!(20)),
    ] {
        let mut bodies = sources();
        bodies[4]["d"][0][field] = value;
        let mut a = args();
        a["invoice_allocations"] = json!([allocation("1", 401, "121")]);
        let result = run(bodies, a).await;
        match field {
            "CurrencyCode" => assert!(has(
                &result,
                "blockers",
                "allocation_fx_mapping_outside_scope"
            )),
            "Amount" => assert!(has(
                &result,
                "blockers",
                "allocation_outstanding_amount_unknown"
            )),
            _ => {
                assert!(has(&result, "warnings", "invoice_amount_in_transit"));
                assert_eq!(
                    result["allocation_evidence"][0]["outstanding_amount"],
                    "121"
                );
            }
        }
    }
    let mut bodies = sources();
    bodies[9]["d"][0]["Currency"] = json!("USD");
    let mut a = args();
    a["invoice_allocations"] = json!([allocation("1", 401, "121")]);
    let result = run(bodies, a).await;
    assert!(has(
        &result,
        "blockers",
        "allocation_fx_mapping_outside_scope"
    ));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bank_proposal_literal_reference_boundaries_preserve_punctuation_and_leading_zeros() {
    for (reference, expected) in [
        ("INV-001", true),
        ("INV001", false),
        ("INV-01", false),
        ("XINV-001", false),
        ("inv-001", true),
    ] {
        let mut bodies = sources();
        bodies[9]["d"][0]["YourRef"] = json!(reference);
        let result = run(bodies, args()).await;
        assert_eq!(
            result["candidate_groups"][0]["reference_literal_present"],
            expected
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bank_proposal_allocation_checks_cover_offpage_candidates_and_all_header_pages() {
    let mut bodies = sources();
    bodies[4] = env(vec![report("1", 3001, 121), report("2", 3002, 124)]);
    bodies[9] = json!({"d":{"results":[header(401,3001)],"__next":"?page=2"}});
    bodies.push(env(vec![header(402, 3002)]));
    let mut a = args();
    a["limit"] = json!(1);
    a["invoice_allocations"] = json!([allocation("2", 402, "124")]);
    let result = run(bodies, a).await;
    assert_eq!(result["source_invoice_header_count"], 2);
    assert_eq!(result["candidate_group_count"], 2);
    assert_eq!(result["has_more"], true);
    assert_eq!(result["budget"]["exact_totals_match"], true);
    assert_eq!(
        result["allocation_evidence"][0]["entry_id"],
        id(402).to_string()
    );
    let mut bodies = sources();
    bodies[9] = json!({"d":{"results":[header(401,3001)],"__next":"?page=2"}});
    let mut hidden = header(402, 9999);
    hidden["Division"] = json!(99);
    bodies.push(env(vec![hidden]));
    fails(bodies, args()).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bank_proposal_gl_restrictions_vat_dimensions_and_period_locks_are_not_approval() {
    for variant in 0..6 {
        let mut a = args();
        let mut split = gl();
        split["net_amount"] = json!("124");
        a["gl_splits"] = json!([split]);
        let mut extra = gl_sources();
        let expected = match variant {
            0 => {
                extra[0]["d"][0]["IsBlocked"] = json!(true);
                "gl_blocked"
            }
            1 => {
                extra[0]["d"][0]["Type"] = json!(22);
                "manual_control_or_unassigned_gl_outside_scope"
            }
            2 => {
                extra[0]["d"][0]["UseCostcenter"] = json!(1);
                "required_dimension_missing"
            }
            3 => {
                extra[1]["d"][0]["VATTransactionType"] = json!("S");
                "vat_applicability_conflict"
            }
            4 => {
                extra[1]["d"][0]["IsBlocked"] = json!(true);
                "vat_code_blocked"
            }
            _ => {
                extra[5]["d"][0]["Status"] = json!(1);
                "direct_gl_source_period_closed"
            }
        };
        let mut bodies = sources();
        bodies.extend(extra);
        let result = run(bodies, a).await;
        assert!(has(&result, "blockers", expected));
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bank_proposal_null_or_zero_pending_budgets_are_not_replaced_by_full_bank_amount() {
    for (field, value) in [
        ("AmountFC", Value::Null),
        ("Currency", Value::Null),
        ("AmountFC", json!(0)),
    ] {
        let mut bodies = sources();
        bodies[3]["d"][0][field] = value.clone();
        bodies[7]["d"][1][field] = value;
        let result = run(bodies, args()).await;
        assert_eq!(result["status"], "blocked");
        assert_ne!(
            result["budget"]["amount"],
            result["bank_transaction"]["amount_transaction_currency"]
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bank_proposal_invalid_inputs_fail_before_reads_and_source_identity_errors_never_return_partial_plans()
 {
    for (field, value) in [
        ("bank_transaction_line_id", json!(id(0))),
        ("pending_posting_line_id", json!("not-a-uuid")),
        ("limit", json!(0)),
        ("account_ids", json!([id(900), id(900)])),
    ] {
        let mut a = args();
        a[field] = value;
        fails(vec![], a).await;
    }
    for amount in ["0", "NaN", "1e999", "0.000000000000000000000000000001"] {
        let mut a = args();
        a["invoice_allocations"] = json!([allocation("1", 401, amount)]);
        fails(vec![], a).await;
    }
    let mut a = args();
    a["invoice_allocations"] = json!([allocation("1", 401, "1"), allocation("1", 401, "2")]);
    fails(vec![], a).await;
    let mut bodies = sources();
    bodies[9]["d"][0]["Supplier"] = json!(id(999));
    fails(bodies, args()).await;
    let mut bodies = sources();
    bodies[9] = env(vec![header(401, 3001), header(401, 3001)]);
    fails(bodies, args()).await;
    let mut a = args();
    a["pending_posting_line_id"] = json!(id(301));
    let mut bodies = sources();
    bodies.truncate(8);
    fails(bodies, a).await;
}

fn ap_sources() -> Vec<Value> {
    let mut bodies = sources();
    let control = posting(101, 11, None, -224);
    let mut pending = posting(201, 22, Some(101), 124);
    pending["Account"] = json!(id(900));
    let mut open = report("2", 1001, 40);
    open["JournalCode"] = json!("B1");
    open["AmountInTransit"] = json!(5);
    bodies[3] = env(vec![]);
    bodies[4] = env(vec![report("1", 3001, 121), open]);
    bodies[7] = env(vec![
        control,
        pending.clone(),
        posting(301, 200, Some(101), 100),
    ]);
    bodies.insert(5, env(vec![pending]));
    bodies
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bank_proposal_existing_ap_uses_current_open_budget_not_original_posting_and_never_rebooks_expense()
 {
    let mut a = args();
    a["account_ids"] = json!([]);
    a["invoice_allocations"] = json!([allocation("1", 401, "40")]);
    let result = run(ap_sources(), a).await;
    assert_eq!(result["budget"]["amount"], "40");
    assert_eq!(result["budget"]["amount_in_transit"], "5");
    assert_eq!(result["budget"]["exact_totals_match"], true);
    assert_eq!(
        result["operation_scope"],
        "match_only_selected_existing_ap_ar_posting_without_rebooking_expense"
    );
    assert_eq!(
        result["routes_to_investigate"],
        json!(["match_existing_ap_ar_only"])
    );
    assert_eq!(
        result["allocation_evidence"][0]["magnitude_comparison"],
        "partial_outstanding_magnitude"
    );
    assert!(has(&result, "warnings", "pending_amount_in_transit"));
    assert!(result["journal_period_evidence"].is_null());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bank_proposal_existing_ap_cannot_silently_switch_side_currency_or_reclassify_to_gl() {
    let mut bodies = ap_sources();
    bodies[5]["d"][0]["Currency"] = json!("USD");
    bodies[8]["d"][1]["Currency"] = json!("USD");
    let result = run(bodies, args()).await;
    assert!(has(
        &result,
        "blockers",
        "pending_report_currency_mapping_outside_scope"
    ));
    let mut a = args();
    let mut line = allocation("1", 401, "40");
    line["side"] = json!("receivable");
    a["invoice_allocations"] = json!([line]);
    let result = run(ap_sources(), a).await;
    assert!(has(&result, "blockers", "existing_ap_ar_scope_conflict"));
    let mut a = args();
    let mut split = gl();
    split["net_amount"] = json!("40");
    a["gl_splits"] = json!([split]);
    let mut bodies = ap_sources();
    bodies.extend(gl_sources());
    let result = run(bodies, a).await;
    assert!(has(
        &result,
        "blockers",
        "existing_ap_ar_gl_reclassification_outside_scope"
    ));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bank_proposal_direct_gl_needs_no_guessed_company_or_subscription_specific_catalogs() {
    let mut a = args();
    a["account_ids"] = json!([]);
    let mut split = gl();
    split["net_amount"] = json!("124");
    a["gl_splits"] = json!([split]);
    let mut bodies = sources();
    bodies.truncate(8);
    bodies.extend(gl_sources());
    let result = run(bodies, a).await;
    assert_eq!(result["budget"]["exact_totals_match"], true);
    assert_eq!(result["candidate_group_count"], 0);
    assert!(has(&result, "unknowns", "company_not_selected"));
    assert_eq!(result["status"], "review_required");
    assert!(result["scoped_companies"].as_array().unwrap().is_empty());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bank_proposal_explicit_dimensions_resolve_unique_codes_and_never_apply_defaults() {
    for mode in 0..3 {
        let mut a = args();
        let mut split = gl();
        split["net_amount"] = json!("124");
        split["cost_center_code"] = json!("CC1");
        a["gl_splits"] = json!([split]);
        let mut extra = gl_sources();
        extra[0]["d"][0]["UseCostcenter"] = json!(if mode == 2 { 2 } else { 1 });
        let record = json!({"ID":id(700),"Division":42,"Code":"CC1","EndDate":if mode==1 {"2000-01-01"}else{"2100-01-01"}});
        extra.insert(3, env(vec![record]));
        let mut bodies = sources();
        bodies.extend(extra);
        let result = run(bodies, a).await;
        if mode == 1 {
            assert!(has(&result, "blockers", "dimension_expired"));
        } else if mode == 2 {
            assert!(has(&result, "blockers", "dimension_forbidden"));
        } else {
            assert_eq!(result["status"], "review_required");
        }
        assert_eq!(result["selected_cost_centers"].as_array().unwrap().len(), 1);
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bank_proposal_changed_budget_and_required_source_failures_are_not_partial_clearance() {
    let mut bodies = sources();
    bodies[7]["d"][1]["AmountFC"] = json!(125);
    bodies.truncate(8);
    fails(bodies, args()).await;
    let mut replies: Vec<_> = sources().into_iter().map(Response::json).collect();
    replies[9] = Response {
        status: "403 Forbidden",
        body: "private financial error".into(),
        location: None,
    };
    let server = Server::new(replies);
    let (_directory, backend) = financial_backend(&server.base).await;
    let error = backend
        .propose_bank_transaction_handling(serde_json::from_value(args()).unwrap())
        .await
        .err()
        .unwrap();
    assert!(!format!("{error:#}").contains("private financial error"));
    server.finish();
    let mut a = args();
    a["bank_transaction_line_id"] = json!(id(999));
    let mut bodies = sources();
    bodies.truncate(8);
    fails(bodies, a).await;
}

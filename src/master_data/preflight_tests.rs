use super::{Response, Server, financial_backend};
use crate::master_data::PreflightPurchaseBooking;
use serde_json::{Value, json};
use uuid::Uuid;

fn id(value: u128) -> Uuid {
    Uuid::from_u128(value)
}
fn envelope(values: Vec<Value>) -> Value {
    json!({"d":values})
}
fn args_value() -> Value {
    json!({"account_id":id(100),"supplier_invoice_reference":"PROPOSED-0001","document_kind":"purchase_invoice",
        "invoice_date":"2025-01-01","financial_year":2025,"financial_period":1,"journal_code":" P1 ","currency":"EUR","gross_amount_transaction_currency":"121.00",
        "lines":[{"gl_account_id":id(200),"net_amount_transaction_currency":"100.00","supplier_vat_amount_transaction_currency":"21.00","vat_code":" V1 ","vat_percentage":"0.21"}]})
}
fn sources() -> Vec<Value> {
    vec![
        envelope(vec![
            json!({"ID":id(100),"Division":42,"Code":" 001 ","Name":"Invented supplier","IsSupplier":true,"Blocked":false,"StartDate":"2000-01-01","EndDate":"2100-12-31","PurchaseCurrency":"EUR","GLAccountPurchase":id(200),"GLAP":id(202),"GLAR":id(203),"PurchaseVATCode":"V1","PaymentConditionPurchase":"30"}),
        ]),
        envelope(vec![]),
        envelope(vec![]),
        envelope(vec![
            json!({"ID":id(500),"Division":42,"Code":"P1","Type":22,"IsBlocked":false,"Currency":"EUR","AllowVariableCurrency":false,"AllowVariableExchangeRate":false,"GLAccount":id(202)}),
        ]),
        envelope(vec![
            json!({"ID":id(600),"Division":42,"FinYear":2025,"FinPeriod":1,"StartDate":"2025-01-01","EndDate":"2025-01-31"}),
        ]),
        envelope(vec![
            json!({"Journal":"P1","Year":2025,"Period":1,"Status":0}),
        ]),
        envelope(vec![json!({"Code":42,"Currency":"EUR"})]),
        envelope(vec![
            json!({"ID":id(200),"Division":42,"Code":"4000","Type":120,"IsBlocked":false,"UseCostcenter":0,"UseCostunit":0,"VATCode":"V1"}),
        ]),
        envelope(vec![
            json!({"ID":id(300),"Division":42,"Code":"V1","Type":"E","VATTransactionType":"P","IsBlocked":false,"Charged":false,"CalculationBasis":0,"Percentage":0.21}),
        ]),
        envelope(vec![
            json!({"ID":id(400),"Division":42,"VATCodeID":id(300),"Type":0,"StartDate":"2000-01-01","EndDate":"2100-12-31","Percentage":0.21}),
        ]),
        envelope(vec![
            json!({"Code":"EUR","Description":"Euro"}),
            json!({"Code":"USD","Description":"US Dollar"}),
        ]),
    ]
}
async fn run(bodies: Vec<Value>, args: Value) -> Value {
    let server = Server::new(bodies.into_iter().map(Response::json).collect());
    let (directory, backend) = financial_backend(&server.base).await;
    let result = serde_json::to_value(
        backend
            .preflight_purchase_booking(serde_json::from_value(args).unwrap())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["authorizes_booking"], false);
    assert!(!directory.path().join("downloads").exists());
    for request in server.finish() {
        assert!(request.target.starts_with("GET /api/v1/42/"));
        assert!(request.body.is_empty());
        for forbidden in [
            "/project/",
            "/logistics/",
            "/purchase/PurchaseInvoices",
            "SysAttachment",
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_consistent_proposal_reports_checked_constraints_without_authorizing_booking() {
    let result = run(sources(), args_value()).await;
    assert_eq!(result["status"], "review_required");
    assert_eq!(result["blockers"], json!([]));
    for code in [
        "invoice_amounts_reconcile",
        "invoice_facing_kind_and_sign",
        "supplier_unblocked",
        "journal_period_open",
        "dated_vat_percentage_matches",
        "declared_vat_arithmetic_exact",
    ] {
        assert!(has(&result, "passed_checks", code), "{code}");
    }
    for code in [
        "duplicate_coverage_incomplete",
        "currency_rounding_policy_unverified",
        "payment_condition_not_explicit",
        "due_date_not_explicit",
        "source_document_not_supplied",
        "tax_deductibility_and_write_permissions_unverified",
    ] {
        assert!(has(&result, "unknowns", code), "{code}");
    }
    assert_eq!(
        result["amount_reconciliation"]["summed_net_plus_supplier_vat"],
        "121.00"
    );
    assert_eq!(
        result["amount_reconciliation"]["summed_supplier_vat"],
        "21.00"
    );
    assert_eq!(result["amount_reconciliation"]["exact_totals_match"], true);
    assert_eq!(
        result["line_evidence"][0]["fully_bounded_normal_rate_ids"],
        json!([id(400)])
    );
    assert_eq!(
        result["line_evidence"][0]["declared_rate_unrounded_supplier_vat"],
        "21"
    );
    assert!(result["selected_cost_centers"].is_null());
    assert!(result["selected_cost_units"].is_null());
    assert_eq!(
        result["duplicate_assessment"]["can_rule_out_duplicates"],
        false
    );
    assert_eq!(result["selected_gl_accounts"][0]["use_costcenter"], 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_known_blocked_missing_unknown_and_unfamiliar_source_constraints_stay_distinct() {
    for (stage, field, value, group, code) in [
        (0, "Blocked", json!(true), "blockers", "supplier_blocked"),
        (
            0,
            "IsSupplier",
            json!(false),
            "blockers",
            "supplier_role_not_enabled",
        ),
        (
            0,
            "IsSupplier",
            Value::Null,
            "unknowns",
            "supplier_role_unknown",
        ),
        (
            0,
            "EndDate",
            json!("2000-01-01"),
            "blockers",
            "supplier_activity_ended",
        ),
        (3, "IsBlocked", json!(true), "blockers", "journal_blocked"),
        (
            3,
            "IsBlocked",
            Value::Null,
            "unknowns",
            "journal_block_state_unknown",
        ),
        (
            3,
            "Type",
            json!(90),
            "blockers",
            "journal_outside_direct_purchase_scope",
        ),
        (
            3,
            "Currency",
            json!("USD"),
            "blockers",
            "journal_currency_conflict",
        ),
        (
            3,
            "AllowVariableCurrency",
            Value::Null,
            "unknowns",
            "journal_currency_setting_unknown",
        ),
        (
            4,
            "FinPeriod",
            json!(2),
            "blockers",
            "financial_period_not_found",
        ),
        (5, "Status", json!(1), "blockers", "journal_period_closed"),
        (
            5,
            "Status",
            json!(50),
            "unknowns",
            "journal_period_status_unknown",
        ),
        (
            7,
            "IsBlocked",
            json!(true),
            "blockers",
            "gl_account_blocked",
        ),
        (
            7,
            "IsBlocked",
            Value::Null,
            "unknowns",
            "gl_block_state_unknown",
        ),
        (
            7,
            "Type",
            json!(24),
            "blockers",
            "manual_control_gl_outside_scope",
        ),
        (
            7,
            "Type",
            json!(30),
            "unknowns",
            "asset_inventory_or_year_end_workflow",
        ),
        (7, "Type", json!(999), "unknowns", "gl_type_unknown"),
        (
            7,
            "UseCostcenter",
            json!(1),
            "blockers",
            "gl_cost_center_rule",
        ),
        (
            7,
            "UseCostunit",
            Value::Null,
            "unknowns",
            "gl_cost_unit_rule",
        ),
        (8, "IsBlocked", json!(true), "blockers", "vat_code_blocked"),
        (
            8,
            "VATTransactionType",
            json!("S"),
            "blockers",
            "vat_sales_only",
        ),
        (
            8,
            "VATTransactionType",
            Value::Null,
            "unknowns",
            "vat_purchase_applicability",
        ),
        (
            9,
            "EndDate",
            Value::Null,
            "unknowns",
            "vat_rate_dates_or_selection_unknown",
        ),
        (
            9,
            "EndDate",
            json!("2024-12-31"),
            "blockers",
            "vat_rate_not_found_for_invoice_date",
        ),
    ] {
        let mut bodies = sources();
        bodies[stage]["d"][0][field] = value;
        let result = run(bodies, args_value()).await;
        assert!(has(&result, group, code), "{field}: {code}");
        if group == "blockers" {
            assert_eq!(result["status"], "blocked");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_totals_credit_signs_discount_lines_and_accounting_header_signs_are_not_conflated()
 {
    let mut args = args_value();
    args["gross_amount_transaction_currency"] = json!("120.99");
    let result = run(sources(), args).await;
    assert!(has(&result, "blockers", "invoice_amounts_do_not_reconcile"));
    let mut args = args_value();
    args["gross_amount_transaction_currency"] = json!("-121.00");
    let result = run(sources(), args).await;
    assert!(has(&result, "blockers", "invoice_facing_sign_conflict"));
    let mut args = args_value();
    args["document_kind"] = json!("purchase_credit_note");
    args["gross_amount_transaction_currency"] = json!("-121.00");
    args["lines"][0]["net_amount_transaction_currency"] = json!("-100.00");
    args["lines"][0]["supplier_vat_amount_transaction_currency"] = json!("-21.00");
    let result = run(sources(), args).await;
    assert_eq!(result["blockers"], json!([]));
    assert!(has(
        &result,
        "passed_checks",
        "declared_vat_arithmetic_exact"
    ));
    let mut args = args_value();
    let mut discount = args["lines"][0].clone();
    discount["net_amount_transaction_currency"] = json!("-10.00");
    discount["supplier_vat_amount_transaction_currency"] = json!("-2.10");
    args["lines"].as_array_mut().unwrap().push(discount);
    args["gross_amount_transaction_currency"] = json!("108.90");
    let result = run(sources(), args).await;
    assert!(has(&result, "passed_checks", "invoice_amounts_reconcile"));
    assert_eq!(result["blockers"], json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_rounding_reverse_charge_and_raw_percentages_are_never_guessed_or_double_counted()
{
    let mut args = args_value();
    args["lines"][0]["net_amount_transaction_currency"] = json!("33.33");
    args["lines"][0]["supplier_vat_amount_transaction_currency"] = json!("7.00");
    args["gross_amount_transaction_currency"] = json!("40.33");
    let result = run(sources(), args).await;
    assert!(has(
        &result,
        "unknowns",
        "vat_rounding_or_override_requires_review"
    ));
    assert_eq!(
        result["line_evidence"][0]["declared_rate_unrounded_supplier_vat"],
        "6.9993"
    );
    assert_eq!(result["amount_reconciliation"]["exact_totals_match"], true);
    let mut bodies = sources();
    bodies[8]["d"][0]["Charged"] = json!(true);
    let mut args = args_value();
    args["lines"][0]["supplier_vat_amount_transaction_currency"] = json!("0.00");
    args["gross_amount_transaction_currency"] = json!("100.00");
    let result = run(bodies, args).await;
    assert!(has(&result, "unknowns", "special_or_unknown_vat_treatment"));
    assert_eq!(
        result["amount_reconciliation"]["summed_supplier_vat"],
        "0.00"
    );
    assert!(result["line_evidence"][0]["declared_rate_unrounded_supplier_vat"].is_null());
    let mut args = args_value();
    args["lines"][0]["vat_percentage"] = json!("21");
    let result = run(sources(), args).await;
    assert!(has(&result, "blockers", "dated_vat_percentage_mismatch"));
    assert_eq!(result["proposal"]["lines"][0]["vat_percentage"], "21");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_ambiguous_special_unknown_and_missing_vat_codes_require_evidence_not_defaults() {
    let mut bodies = sources();
    let mut second = bodies[9]["d"][0].clone();
    second["ID"] = json!(id(401));
    bodies[9]["d"].as_array_mut().unwrap().push(second);
    let result = run(bodies, args_value()).await;
    assert!(has(
        &result,
        "unknowns",
        "vat_rate_dates_or_selection_unknown"
    ));
    let mut bodies = sources();
    bodies[9]["d"][0]["Type"] = json!(1);
    let result = run(bodies, args_value()).await;
    assert!(has(
        &result,
        "unknowns",
        "vat_rate_dates_or_selection_unknown"
    ));
    assert!(has(&result, "unknowns", "special_or_unknown_vat_treatment"));
    let mut args = args_value();
    args["lines"][0]["vat_code"] = json!("MISSING");
    let result = run(sources(), args).await;
    assert!(has(&result, "blockers", "vat_code_missing_or_ambiguous"));
    assert_eq!(result["selected_vat_codes"], json!([]));
    let mut bodies = sources();
    bodies[8]["d"][0]["Type"] = json!("N");
    let result = run(bodies.clone(), args_value()).await;
    assert!(has(&result, "blockers", "no_vat_code_with_declared_vat"));
    let mut args = args_value();
    args["lines"][0]["supplier_vat_amount_transaction_currency"] = json!("0.00");
    args["lines"][0]["vat_percentage"] = json!("0");
    args["gross_amount_transaction_currency"] = json!("100.00");
    let result = run(bodies, args).await;
    assert!(has(&result, "passed_checks", "no_supplier_vat_declared"));
    assert_eq!(result["blockers"], json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_dated_reporting_period_mismatches_are_warnings_not_calendar_assumptions() {
    let mut bodies = sources();
    bodies[4]["d"][0]["StartDate"] = json!("2025-02-15");
    bodies[4]["d"][0]["EndDate"] = json!("2025-03-14");
    let result = run(bodies, args_value()).await;
    assert!(has(
        &result,
        "warnings",
        "invoice_date_outside_reporting_period"
    ));
    assert!(!has(
        &result,
        "blockers",
        "invoice_date_outside_reporting_period"
    ));
    assert!(has(&result, "passed_checks", "journal_period_open"));
    let mut bodies = sources();
    bodies[5]["d"] = json!([]);
    let result = run(bodies, args_value()).await;
    assert!(has(&result, "unknowns", "journal_period_status_unknown"));
    let mut bodies = sources();
    bodies[4]["d"][0]["StartDate"] = Value::Null;
    let result = run(bodies, args_value()).await;
    assert!(has(
        &result,
        "unknowns",
        "reporting_period_date_bounds_unknown"
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_dimension_rules_do_not_apply_defaults_or_require_unused_optional_catalogs() {
    let mut bodies = sources();
    bodies[7]["d"][0]["UseCostcenter"] = json!(1);
    bodies[7]["d"][0]["Costcenter"] = json!("DEFAULT");
    let result = run(bodies, args_value()).await;
    assert!(has(&result, "blockers", "gl_cost_center_rule"));
    assert!(result["selected_cost_centers"].is_null());
    for (end, expected, group) in [
        (
            json!("2100-12-31"),
            "cost_center_eligibility",
            "passed_checks",
        ),
        (json!("2000-01-01"), "cost_center_eligibility", "blockers"),
        (Value::Null, "cost_center_eligibility", "unknowns"),
    ] {
        let mut bodies = sources();
        bodies[7]["d"][0]["UseCostcenter"] = json!(1);
        bodies.push(envelope(vec![
            json!({"ID":id(700),"Division":42,"Code":" CC1 ","EndDate":end}),
        ]));
        let mut args = args_value();
        args["lines"][0]["cost_center_code"] = json!("CC1");
        let result = run(bodies, args).await;
        assert!(has(&result, group, expected));
        assert_eq!(result["selected_cost_centers"][0]["code"], " CC1 ");
    }
    let mut bodies = sources();
    bodies[7]["d"][0]["UseCostunit"] = json!(2);
    bodies.push(envelope(vec![
        json!({"ID":id(701),"Division":42,"Code":"U1","EndDate":"2100-12-31"}),
    ]));
    let mut args = args_value();
    args["lines"][0]["cost_unit_code"] = json!("U1");
    let result = run(bodies, args).await;
    assert!(has(&result, "blockers", "gl_cost_unit_rule"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_defaults_are_warnings_not_chosen_accounts_rates_or_due_dates() {
    let mut bodies = sources();
    bodies[0]["d"][0]["GLAccountPurchase"] = json!(id(999));
    bodies[0]["d"][0]["PurchaseVATCode"] = json!("OTHER");
    bodies[0]["d"][0]["PurchaseCurrency"] = json!("USD");
    bodies[7]["d"][0]["VATCode"] = json!("OTHER");
    let result = run(bodies, args_value()).await;
    for code in [
        "supplier_gl_default_differs",
        "supplier_vat_default_differs",
        "supplier_currency_default_differs",
        "gl_vat_default_differs",
    ] {
        assert!(has(&result, "warnings", code));
    }
    assert_eq!(
        result["proposal"]["lines"][0]["gl_account_id"],
        id(200).to_string()
    );
    assert_eq!(result["proposal"]["currency"], "EUR");
    let mut bodies = sources();
    bodies.push(envelope(vec![
        json!({"ID":id(800),"Division":42,"Code":"60","PaymentDays":15,"PaymentEndOfMonths":2}),
    ]));
    let mut args = args_value();
    args["payment_condition_code"] = json!("60");
    args["due_date"] = json!("2025-04-15");
    let result = run(bodies, args).await;
    assert!(has(&result, "warnings", "supplier_payment_default_differs"));
    assert!(has(&result, "unknowns", "due_date_terms_not_calculated"));
    assert_eq!(
        result["selected_payment_conditions"][0]["payment_end_of_months"],
        2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_foreign_currency_requires_review_without_conversion_or_rate_override_assumptions()
 {
    let mut bodies = sources();
    bodies[3]["d"][0]["AllowVariableCurrency"] = json!(true);
    let mut args = args_value();
    args["currency"] = json!("USD");
    args["exchange_rate"] = json!("1.123456789");
    let result = run(bodies, args).await;
    assert!(has(
        &result,
        "unknowns",
        "foreign_currency_rate_not_verified"
    ));
    assert!(has(
        &result,
        "unknowns",
        "journal_rate_override_not_verified"
    ));
    assert_eq!(result["amount_reconciliation"]["currency"], "USD");
    assert_eq!(result["amount_reconciliation"]["proposed_gross"], "121.00");
    assert_eq!(result["default_currency"], "EUR");
    assert!(result.get("converted_amount").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_fresh_duplicate_candidates_block_even_if_original_entry_is_processed() {
    let mut bodies = sources();
    bodies[1] = envelope(vec![
        json!({"EntryID":id(900),"Division":42,"Supplier":id(100),"EntryNumber":12345,"Journal":"P1","YourRef":"PROPOSED-0001","Currency":"EUR","AmountFC":-121,"Status":50}),
    ]);
    let result = run(bodies, args_value()).await;
    assert!(has(
        &result,
        "blockers",
        "duplicate_candidates_require_resolution"
    ));
    assert_eq!(result["duplicate_assessment"]["matched_candidate_count"], 1);
    assert_eq!(
        result["duplicate_assessment"]["can_rule_out_duplicates"],
        false
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_invalid_proposal_shapes_decimals_dates_and_overflow_fail_before_remote_reads() {
    let server = Server::new(vec![]);
    let (_directory, backend) = financial_backend(&server.base).await;
    for (field, value) in [
        ("account_id", json!(Uuid::nil())),
        ("supplier_invoice_reference", json!(" ")),
        ("financial_year", json!(0)),
        ("financial_period", json!(-1)),
        ("journal_code", json!(" ")),
        ("currency", json!("EURO")),
        ("gross_amount_transaction_currency", json!("NaN")),
        ("exchange_rate", json!("0")),
        ("due_date", json!("0000-01-01")),
        ("lines", json!([])),
    ] {
        let mut args = args_value();
        args[field] = value;
        assert!(
            backend
                .preflight_purchase_booking(serde_json::from_value(args).unwrap())
                .await
                .is_err()
        );
    }
    for (field, value) in [
        ("gl_account_id", json!(Uuid::nil())),
        ("vat_code", json!(" ")),
        ("vat_percentage", json!("-0.21")),
        (
            "net_amount_transaction_currency",
            json!("0.00000000000000000000000000001"),
        ),
        ("cost_center_code", json!(" ")),
    ] {
        let mut args = args_value();
        args["lines"][0][field] = value;
        assert!(
            backend
                .preflight_purchase_booking(serde_json::from_value(args).unwrap())
                .await
                .is_err()
        );
    }
    let mut args = args_value();
    args["gross_amount_transaction_currency"] = json!(121);
    assert!(serde_json::from_value::<PreflightPurchaseBooking>(args).is_err());
    let mut args = args_value();
    args["lines"][0]["project_id"] = json!(id(999));
    assert!(serde_json::from_value::<PreflightPurchaseBooking>(args).is_err());
    let mut args = args_value();
    args["unexpected"] = json!(true);
    assert!(serde_json::from_value::<PreflightPurchaseBooking>(args).is_err());
    let mut args = args_value();
    args["lines"][0]["net_amount_transaction_currency"] = json!("79228162514264337593543950335");
    args["lines"]
        .as_array_mut()
        .unwrap()
        .push(args_value()["lines"][0].clone());
    assert!(
        backend
            .preflight_purchase_booking(serde_json::from_value(args).unwrap())
            .await
            .is_err()
    );
    server.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_required_source_scope_failures_never_return_partial_clearance() {
    for (stage, field, value) in [
        (0, "Division", json!(99)),
        (3, "Division", json!(99)),
        (4, "FinYear", json!(2024)),
        (5, "Year", json!(2024)),
        (6, "Code", json!(99)),
        (7, "Division", json!(99)),
        (8, "Division", json!(99)),
        (9, "VATCodeID", json!(id(999))),
    ] {
        let mut bodies = sources();
        bodies[stage]["d"][0][field] = value;
        bodies.truncate(stage + 1);
        let server = Server::new(bodies.into_iter().map(Response::json).collect());
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(
            backend
                .preflight_purchase_booking(serde_json::from_value(args_value()).unwrap())
                .await
                .is_err(),
            "{stage}:{field}"
        );
        server.finish();
    }
    let mut replies: Vec<_> = sources().into_iter().map(Response::json).collect();
    replies.push(Response {
        status: "403 Forbidden",
        body: "private source body".into(),
        location: None,
    });
    let server = Server::new(replies);
    let (_directory, backend) = financial_backend(&server.base).await;
    let mut args = args_value();
    args["payment_condition_code"] = json!("30");
    let error = backend
        .preflight_purchase_booking(serde_json::from_value(args).unwrap())
        .await
        .err()
        .unwrap();
    assert!(format!("{error:#}").contains("403"));
    assert!(!format!("{error:#}").contains("private source body"));
    server.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_scope_changes_prevent_reads_and_create_no_financial_files() {
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
            .preflight_purchase_booking(serde_json::from_value(args_value()).unwrap())
            .await
            .is_err()
    );
    assert!(!directory.path().join("downloads").exists());
    server.finish();
}

#[test]
fn preflight_exact_vat_products_reject_precision_loss_without_rounding() {
    let money = |value: &str| serde_json::from_str::<crate::financial::Money>(value).unwrap();
    assert_eq!(
        serde_json::to_value(money("33.33").exact_product(money("0.21")).unwrap()).unwrap(),
        "6.9993"
    );
    assert!(
        money("0.00000000000000000001")
            .exact_product(money("0.00000000000000000001"))
            .is_err()
    );
    assert_eq!(
        money("0.00").exact_product(money("0.21")).unwrap(),
        money("0")
    );
    assert!(
        money("79228162514264337593543950335")
            .exact_product(money("79228162514264337593543950335"))
            .is_err()
    );
}

fn intake_document() -> Value {
    json!({"ID":id(700),"Division":42,"Type":20,"Account":id(100),"Subject":"Invented source document","DocumentDate":"2025-01-01","ProposedEntryStatus":20,"AmountFC":121,"Currency":"EUR","Modified":"2025-01-01T10:00:00"})
}
fn with_source_document(initial: Value, fresh: Value, entries: Vec<Value>) -> Vec<Value> {
    let mut bodies = sources();
    bodies.insert(1, envelope(vec![initial.clone()]));
    bodies[3] = envelope(vec![initial.clone()]);
    bodies.insert(4, envelope(vec![initial]));
    bodies.extend([envelope(vec![fresh]), envelope(vec![]), envelope(entries)]);
    bodies
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_source_document_open_unassigned_rejected_and_changed_company_states_are_explicit()
 {
    let mut args = args_value();
    args["source_document_id"] = json!(id(700));
    let source = intake_document();
    let result = run(
        with_source_document(source.clone(), source, vec![]),
        args.clone(),
    )
    .await;
    assert!(has(&result, "passed_checks", "source_proposal_open"));
    assert!(has(
        &result,
        "passed_checks",
        "source_document_supplier_reference"
    ));
    assert!(has(
        &result,
        "unknowns",
        "source_invoice_content_not_verified"
    ));
    assert_eq!(
        result["duplicate_assessment"]["coverage"]["input_document_excluded"],
        1
    );
    let mut source = intake_document();
    source["Account"] = Value::Null;
    let result = run(
        with_source_document(source.clone(), source, vec![]),
        args.clone(),
    )
    .await;
    assert!(has(
        &result,
        "unknowns",
        "source_document_supplier_unassigned"
    ));
    let mut source = intake_document();
    source["ProposedEntryStatus"] = json!(5);
    let result = run(
        with_source_document(source.clone(), source, vec![]),
        args.clone(),
    )
    .await;
    assert!(has(&result, "blockers", "source_proposal_void_or_rejected"));
    let initial = intake_document();
    let mut fresh = initial.clone();
    fresh["Account"] = json!(id(101));
    let result = run(with_source_document(initial, fresh, vec![]), args).await;
    assert!(has(
        &result,
        "blockers",
        "source_document_supplier_conflict"
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_fresh_document_reverse_links_block_even_if_earlier_duplicate_scan_was_empty() {
    let source = intake_document();
    let linked = json!({"EntryID":id(900),"Division":42,"Supplier":id(100),"EntryNumber":12345,"Journal":"P1","Document":id(700),"Status":50,"Type":30});
    let mut args = args_value();
    args["source_document_id"] = json!(id(700));
    let result = run(
        with_source_document(source.clone(), source, vec![linked]),
        args,
    )
    .await;
    assert_eq!(result["duplicate_assessment"]["matched_candidate_count"], 0);
    assert!(has(&result, "blockers", "source_document_already_linked"));
    assert_eq!(
        result["source_document"]["linked_purchase_entries"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_all_gl_pages_and_hidden_scope_rows_are_validated_before_line_selection() {
    let mut replies: Vec<_> = sources().into_iter().map(Response::json).collect();
    let mut bodies = sources();
    let valid = bodies[7]["d"][0].take();
    replies[7] = Response::json(json!({"d":{"results":[valid],"__next":"?page=2"}}));
    replies.insert(
        8,
        Response::json(envelope(vec![
            json!({"ID":id(999),"Division":99,"Code":"UNUSED","IsBlocked":false}),
        ])),
    );
    replies.truncate(9);
    let server = Server::new(replies);
    let (_directory, backend) = financial_backend(&server.base).await;
    assert!(
        backend
            .preflight_purchase_booking(serde_json::from_value(args_value()).unwrap())
            .await
            .is_err()
    );
    assert_eq!(server.finish().len(), 9);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_enabled_currency_pages_missing_codes_duplicates_and_access_failures_are_not_guessed()
 {
    let mut bodies = sources();
    bodies[10] = json!({"d":{"results":[{"Code":"EUR"}],"__next":"?page=2"}});
    bodies.push(envelope(vec![json!({"Code":"USD"})]));
    let result = run(bodies, args_value()).await;
    assert_eq!(result["enabled_currency_count"], 2);
    assert_eq!(result["currency_evidence"]["code"], "EUR");
    assert!(has(&result, "passed_checks", "currency_in_enabled_catalog"));
    let mut bodies = sources();
    bodies[10] = envelope(vec![json!({"Code":"USD"})]);
    let result = run(bodies, args_value()).await;
    assert!(has(&result, "blockers", "currency_not_in_enabled_catalog"));
    assert!(result["currency_evidence"].is_null());
    let mut bodies = sources();
    bodies[10] = envelope(vec![json!({"Code":"EUR"}), json!({"Code":" eur "})]);
    let server = Server::new(bodies.into_iter().map(Response::json).collect());
    let (_directory, backend) = financial_backend(&server.base).await;
    assert!(
        backend
            .preflight_purchase_booking(serde_json::from_value(args_value()).unwrap())
            .await
            .is_err()
    );
    server.finish();
    let mut replies: Vec<_> = sources().into_iter().map(Response::json).collect();
    replies[10] = Response {
        status: "403 Forbidden",
        body: "private currency source".into(),
        location: None,
    };
    let server = Server::new(replies);
    let (_directory, backend) = financial_backend(&server.base).await;
    let error = backend
        .preflight_purchase_booking(serde_json::from_value(args_value()).unwrap())
        .await
        .err()
        .unwrap();
    assert!(!format!("{error:#}").contains("private currency source"));
    assert!(format!("{error:#}").contains("Financial currencies"));
    server.finish();
}

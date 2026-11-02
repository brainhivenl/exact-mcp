use super::{Response, Server, financial_backend};
use crate::financial::Backend;
use anyhow::Result;
use reqwest::Url;
use serde_json::{Value, json};
use uuid::Uuid;

const CATALOGS: [&str; 6] = [
    "vat", "journals", "payments", "centers", "units", "projects",
];

fn record(id: u128, code: &str) -> Value {
    json!({"ID": Uuid::from_u128(id), "Division": 42, "Code": code, "Description": "ÉTUDE %_ O'Neill"})
}

fn rows(records: Vec<Value>) -> Response {
    Response::json(json!({"d": {"results": records}}))
}

fn rate(id: u128, code: u128) -> Value {
    json!({"ID": Uuid::from_u128(id), "Division": 42, "VATCodeID": Uuid::from_u128(code), "LineNumber": 1,
        "StartDate": "2025-01-01", "EndDate": null, "Type": 0, "Percentage": 0.21})
}

fn period(id: u128, number: i32) -> Value {
    json!({"ID": Uuid::from_u128(id), "Division": 42, "FinYear": 2025, "FinPeriod": number,
        "StartDate": "2024-12-15", "EndDate": "2025-01-14"})
}

fn status(number: i32, value: Value) -> Value {
    json!({"Journal": " P'1 ", "Year": 2025, "Period": number, "Status": value, "StatusDescription": "Untrusted source status"})
}

fn lock_args() -> Value {
    json!({"journal_code": " P'1 ", "financial_year": 2025})
}

async fn call(backend: &Backend, resource: &str, args: Value) -> Result<Value> {
    let args = serde_json::from_value(args)?;
    let result = match resource {
        "vat" => serde_json::to_value(backend.search_vat_codes(args).await?)?,
        "journals" => serde_json::to_value(backend.search_journals(args).await?)?,
        "payments" => serde_json::to_value(backend.search_payment_conditions(args).await?)?,
        "centers" => serde_json::to_value(backend.search_cost_centers(args).await?)?,
        "units" => serde_json::to_value(backend.search_cost_units(args).await?)?,
        "projects" => serde_json::to_value(backend.search_projects(args).await?)?,
        _ => unreachable!(),
    };
    Ok(result)
}

fn with_rates(mut replies: Vec<Response>, resource: &str) -> Vec<Response> {
    if resource == "vat" {
        replies.push(rows(vec![]));
    }
    replies
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn master_catalogs_read_every_page_before_stable_local_pagination() {
    for resource in CATALOGS {
        let replies = with_rates(
            vec![
                Response::json(
                    json!({"d":{"results":[record(3," 003 "),record(2,"002")],"__next":"?page=2"}}),
                ),
                rows(vec![record(1, "001")]),
            ],
            resource,
        );
        let server = Server::new(replies);
        let (_directory, backend) = financial_backend(&server.base).await;
        let result = call(&backend, resource, json!({"limit":1,"offset":1}))
            .await
            .unwrap();
        assert_eq!(result["administration"]["Code"], 42);
        assert!(result["retrieved_at"].is_string());
        assert_eq!(result["source_record_count"], 3);
        assert_eq!(result["matched_record_count"], 3);
        assert_eq!(result["records"][0]["code"], "002");
        assert_eq!(result["records"].as_array().unwrap().len(), 1);
        assert_eq!(result["has_more"], true);
        assert_eq!(result["next_offset"], 2);
        let requests = server.finish();
        for request in &requests {
            assert!(request.target.starts_with("GET /api/v1/42/"));
            assert!(request.body.is_empty());
            let url = Url::parse(&format!(
                "http://localhost{}",
                request.target.split_whitespace().nth(1).unwrap()
            ))
            .unwrap();
            assert!(!url.query_pairs().any(|(key, _)| key == "$filter"));
            let fields = url
                .query_pairs()
                .find(|(key, _)| key == "$select")
                .map(|(_, value)| value.into_owned())
                .unwrap_or_default();
            assert!(!fields.contains("IBAN") && !fields.contains("BankAccount"));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn master_catalogs_preserve_padding_leading_zeroes_and_literal_unicode_queries() {
    for resource in CATALOGS {
        for args in [
            json!({"query":" 001 ","match_by":"code"}),
            json!({"query":" étude %_ o'neill "}),
        ] {
            let server = Server::new(with_rates(
                vec![rows(vec![record(1, " 001 "), record(2, "1")])],
                resource,
            ));
            let (_directory, backend) = financial_backend(&server.base).await;
            let result = call(&backend, resource, args.clone()).await.unwrap();
            assert_eq!(result["records"][0]["code"], " 001 ");
            assert_eq!(result["records"][0]["description"], "ÉTUDE %_ O'Neill");
            assert_eq!(
                result["matched_record_count"],
                if args.get("match_by").is_some() { 1 } else { 2 }
            );
            server.finish();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn master_catalogs_validate_hidden_rows_and_reject_duplicates() {
    for resource in CATALOGS {
        for invalid in [
            {
                let mut row = record(2, "002");
                row["Division"] = json!(99);
                row
            },
            {
                let mut row = record(2, "002");
                row["ID"] = json!(Uuid::nil());
                row
            },
            record(2, " "),
            record(2, &"a".repeat(101)),
            record(1, "001"),
        ] {
            let server = Server::new(vec![rows(vec![record(1, "001"), invalid])]);
            let (_directory, backend) = financial_backend(&server.base).await;
            assert!(
                call(&backend, resource, json!({"query":"no match","limit":1}))
                    .await
                    .is_err()
            );
            server.finish();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn master_catalogs_reject_invalid_arguments_without_remote_reads() {
    let server = Server::new(vec![]);
    let (_directory, backend) = financial_backend(&server.base).await;
    for resource in CATALOGS {
        for args in [
            json!({"query":" "}),
            json!({"query":"x".repeat(101)}),
            json!({"limit":0}),
            json!({"limit":51}),
            json!({"offset":u32::MAX}),
            json!({"match_by":"id"}),
            json!({"unexpected":true}),
        ] {
            assert!(call(&backend, resource, args).await.is_err());
        }
    }
    for args in [json!({"financial_year":0}), json!({"financial_year":10000})] {
        assert!(
            backend
                .get_financial_periods(serde_json::from_value(args).unwrap())
                .await
                .is_err()
        );
    }
    for args in [
        json!({"financial_year":0,"journal_code":"P1"}),
        json!({"financial_year":2025,"journal_code":" "}),
    ] {
        assert!(
            backend
                .get_journal_period_status(serde_json::from_value(args).unwrap())
                .await
                .is_err()
        );
    }
    server.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn master_catalogs_empty_and_beyond_end_keep_complete_counts() {
    for resource in CATALOGS {
        for source in [vec![], vec![record(1, "001")]] {
            let count = source.len();
            let server = Server::new(with_rates(vec![rows(source)], resource));
            let (_directory, backend) = financial_backend(&server.base).await;
            let result = call(&backend, resource, json!({"offset":50}))
                .await
                .unwrap();
            assert_eq!(result["records"], json!([]));
            assert_eq!(result["source_record_count"], count);
            assert_eq!(result["matched_record_count"], count);
            assert_eq!(result["has_more"], false);
            assert!(result["next_offset"].is_null());
            server.finish();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn master_catalogs_fail_whole_request_on_later_page_errors() {
    for resource in CATALOGS {
        let server = Server::new(vec![
            Response::json(json!({"d":{"results":[record(1,"001")],"__next":"?page=2"}})),
            Response {
                status: "403 Forbidden",
                body: "private response must not leak".into(),
                location: None,
            },
        ]);
        let (_directory, backend) = financial_backend(&server.base).await;
        let error = call(&backend, resource, json!({"limit":1}))
            .await
            .unwrap_err();
        assert!(!format!("{error:#}").contains("private response"));
        server.finish();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vat_history_is_explicit_scoped_deduplicated_and_not_rescaled() {
    let mut code = record(1, "V1");
    code["Type"] = json!("E");
    code["VATTransactionType"] = json!("P");
    code["Charged"] = json!(true);
    code["IsBlocked"] = json!(true);
    code["Percentage"] = serde_json::from_str("0.210000000000000001").unwrap();
    let mut old = rate(10, 1);
    old["StartDate"] = json!("2020-01-01");
    old["EndDate"] = json!("2024-12-31");
    old["Percentage"] = json!(0.19);
    let mut extra = rate(12, 1);
    extra["Type"] = json!(1);
    extra["Percentage"] = Value::Null;
    let server = Server::new(vec![
        rows(vec![code, record(2, "V2")]),
        Response::json(json!({"d":{"results":[rate(11,1)],"__next":"?page=2"}})),
        rows(vec![extra, old]),
    ]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let result = call(&backend, "vat", json!({})).await.unwrap();
    let code = &result["records"][0];
    assert_eq!(code["calculation_type"], "E");
    assert_eq!(code["transaction_type"], "P");
    assert_eq!(code["charged"], true);
    assert_eq!(code["is_blocked"], true);
    assert_eq!(code["percentage"], "0.210000000000000001");
    assert_eq!(code["percentages"][0]["percentage"], "0.19");
    assert_eq!(code["percentages"][1]["percentage"], "0.21");
    assert_eq!(code["percentages"][2]["type_code"], 1);
    assert!(code["percentages"][2]["percentage"].is_null());
    assert_eq!(result["records"][1]["percentages"], json!([]));
    assert!(result["records"][1]["percentage"].is_null());
    let requests = server.finish();
    assert!(requests[1].target.contains("/vat/VatPercentages?"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vat_history_rejects_orphans_duplicates_wrong_scope_and_bad_dates() {
    for rates in [
        vec![rate(10, 2)],
        vec![rate(10, 1), rate(10, 1)],
        vec![{
            let mut row = rate(10, 1);
            row["Division"] = json!(99);
            row
        }],
        vec![{
            let mut row = rate(10, 1);
            row["ID"] = json!(Uuid::nil());
            row
        }],
        vec![{
            let mut row = rate(10, 1);
            row["EndDate"] = json!("2000-01-01");
            row
        }],
    ] {
        let server = Server::new(vec![rows(vec![record(1, "V1")]), rows(rates)]);
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(
            call(&backend, "vat", json!({"query":"not matched"}))
                .await
                .is_err()
        );
        server.finish();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn master_metadata_preserves_nulls_restrictions_and_payment_rules() {
    let mut journal = record(1, "P1");
    journal["IsBlocked"] = json!(true);
    journal["Type"] = json!(22);
    journal["AllowVariableCurrency"] = json!(false);
    journal["Currency"] = json!("USD");
    let mut payment = record(2, "M2");
    payment["PaymentDays"] = json!(15);
    payment["PaymentEndOfMonths"] = json!(2);
    payment["PaymentMethod"] = json!("Q");
    payment["Percentage"] = json!(0.5);
    payment["DiscountPercentage"] = json!(0.02);
    let mut center = record(3, "CC1");
    center["EndDate"] = json!("2000-01-01");
    center["Active"] = json!(true);
    let mut project = record(4, "PR1");
    project["BlockEntry"] = json!(true);
    project["BlockPurchasing"] = json!(false);
    project["IsWBSRequiredForEntry"] = json!(0);
    for (resource, source, checks) in [
        (
            "journals",
            journal,
            json!({"is_blocked":true,"type_code":22,"allow_variable_currency":false,"currency":"USD","allow_vat":null}),
        ),
        (
            "payments",
            payment,
            json!({"payment_days":15,"payment_end_of_months":2,"payment_method":"Q","percentage":"0.5","discount_percentage":"0.02"}),
        ),
        ("centers", center, json!({"end_date":"2000-01-01"})),
        (
            "projects",
            project,
            json!({"block_entry":true,"block_purchasing":false,"block_invoicing":null,"wbs_required_for_entry":0}),
        ),
    ] {
        let server = Server::new(vec![rows(vec![source])]);
        let (_directory, backend) = financial_backend(&server.base).await;
        let result = call(&backend, resource, json!({})).await.unwrap();
        let record = &result["records"][0];
        for (key, value) in checks.as_object().unwrap() {
            assert_eq!(&record[key], value);
        }
        assert!(record.get("active").is_none());
        assert!(record.get("booking_allowed").is_none());
        server.finish();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn financial_periods_preserve_noncalendar_and_missing_dates_and_special_periods() {
    let mut unknown = period(2, 13);
    unknown["StartDate"] = Value::Null;
    unknown["EndDate"] = Value::Null;
    let server = Server::new(vec![
        Response::json(json!({"d":{"results":[unknown],"__next":"?page=2"}})),
        rows(vec![period(1, 0)]),
    ]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let result = serde_json::to_value(
        backend
            .get_financial_periods(serde_json::from_value(json!({"financial_year":2025})).unwrap())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["periods"][0]["financial_period"], 0);
    assert_eq!(result["periods"][0]["start_date"], "2024-12-15");
    assert_eq!(result["periods"][1]["financial_period"], 13);
    assert!(result["periods"][1]["start_date"].is_null());
    assert!(result["periods"][0].get("status_code").is_none());
    let requests = server.finish();
    assert!(requests[0].target.contains("FinYear+eq+2025"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn financial_periods_reject_duplicate_id_number_scope_and_date_failures() {
    for records in [
        vec![period(1, 1), period(1, 2)],
        vec![period(1, 1), period(2, 1)],
        vec![{
            let mut row = period(1, 1);
            row["Division"] = json!(99);
            row
        }],
        vec![{
            let mut row = period(1, 1);
            row["FinYear"] = json!(2024);
            row
        }],
        vec![period(1, -1)],
        vec![period(1, 32768)],
        vec![{
            let mut row = period(1, 1);
            row["ID"] = json!(Uuid::nil());
            row
        }],
        vec![{
            let mut row = period(1, 1);
            row["EndDate"] = json!("2000-01-01");
            row
        }],
    ] {
        let server = Server::new(vec![rows(records)]);
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(
            backend
                .get_financial_periods(
                    serde_json::from_value(json!({"financial_year":2025})).unwrap()
                )
                .await
                .is_err()
        );
        server.finish();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journal_period_status_keeps_missing_and_unknown_locks_unknown() {
    let mut journal = record(1, " P'1 ");
    journal["IsBlocked"] = json!(true);
    let server = Server::new(vec![
        rows(vec![journal]),
        rows(
            (1..=5)
                .map(|number| period(number as u128, number))
                .collect(),
        ),
        Response::json(
            json!({"d":{"results":[status(2,json!(1)),status(1,json!(0))],"__next":"?page=2"}}),
        ),
        rows(vec![
            status(3, Value::Null),
            status(4, json!(77)),
            status(0, json!(1)),
        ]),
    ]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let result = serde_json::to_value(
        backend
            .get_journal_period_status(serde_json::from_value(lock_args()).unwrap())
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["journal"]["is_blocked"], true);
    assert_eq!(result["periods"][0]["journal_period_state"], "open");
    assert_eq!(result["periods"][1]["journal_period_state"], "closed");
    for index in 2..5 {
        assert_eq!(result["periods"][index]["journal_period_state"], "unknown");
    }
    assert_eq!(result["periods"][3]["status_code"], 77);
    assert!(result["periods"][4]["status_code"].is_null());
    assert_eq!(
        result["statuses_without_period_dates"][0]["financial_period"],
        0
    );
    let requests = server.finish();
    let url = Url::parse(&format!(
        "http://localhost{}",
        requests[2].target.split_whitespace().nth(1).unwrap()
    ))
    .unwrap();
    assert_eq!(
        url.query_pairs()
            .find(|(key, _)| key == "$filter")
            .unwrap()
            .1,
        "Year eq 2025 and trim(Journal) eq 'P''1'"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journal_period_status_rejects_ambiguous_journals_and_wrong_scope_statuses() {
    for journals in [vec![], vec![record(1, "P'1"), record(2, " P'1 ")]] {
        let server = Server::new(vec![rows(journals)]);
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(
            backend
                .get_journal_period_status(serde_json::from_value(lock_args()).unwrap())
                .await
                .is_err()
        );
        server.finish();
    }
    for statuses in [
        vec![status(1, json!(0)), status(1, json!(0))],
        vec![{
            let mut row = status(1, json!(0));
            row["Year"] = json!(2024);
            row
        }],
        vec![{
            let mut row = status(1, json!(0));
            row["Journal"] = json!("OTHER");
            row
        }],
        vec![status(-1, json!(0))],
    ] {
        let server = Server::new(vec![
            rows(vec![record(1, "P'1")]),
            rows(vec![period(1, 1)]),
            rows(statuses),
        ]);
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(
            backend
                .get_journal_period_status(serde_json::from_value(lock_args()).unwrap())
                .await
                .is_err()
        );
        server.finish();
    }
}

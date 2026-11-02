use super::{Response, Server, financial_backend};
use serde_json::{Value, json};
use uuid::Uuid;

fn id(value: u128) -> Uuid {
    Uuid::from_u128(value)
}
fn rows(values: Vec<Value>) -> Response {
    Response::json(json!({"d":{"results":values}}))
}
fn item(value: u128) -> Value {
    json!({"ID":id(value),"Division":42,"Code":" 001 ","Description":"Invented article",
        "IsPurchaseItem":true,"IsSalesItem":false,"IsFractionAllowedItem":false,"IsTaxableItem":1,
        "Unit":" pc ","SalesVatCode":" 01 ","StandardSalesPrice":0.1,"CostPriceStandard":0.2,"CostPriceCurrency":"USD"})
}
fn account() -> Value {
    json!({"ID":id(100),"Division":42,"Code":" 00001 ","Name":"Invented supplier",
        "IsSupplier":true,"IsSales":false,"Blocked":true,"Country":"BE","CountryName":"Belgium",
        "VATNumber":"BE0000000000","ChamberOfCommerce":"00123456","Status":"P",
        "GLAccountPurchase":id(201),"GLAccountSales":id(202),"GLAP":id(203),"GLAR":id(204),
        "PurchaseVATCode":" 01 ","SalesVATCode":"02","PaymentConditionPurchase":"30","PaymentConditionSales":"60",
        "PurchaseCurrency":"USD","SalesCurrency":"GBP","DiscountPurchase":0.055,"InvoiceAccount":id(101)})
}
fn link(value: u128, item_id: u128) -> Value {
    json!({"ID":id(value),"Division":42,"Supplier":id(100),"Item":id(item_id),"ItemCode":"001",
        "SupplierItemCode":" 000ABC ","MainSupplier":true,"ItemUnit":id(301),"ItemUnitCode":"pc",
        "PurchaseUnit":"box","PurchaseUnitFactor":12.5,"PurchaseVATCode":"P0","PurchasePrice":0,
        "Currency":"CHF","StartDate":"2099-01-01","EndDate":null,"MinimumQuantity":1.25})
}
fn item_args() -> crate::master_data::GetItemDetails {
    serde_json::from_value(json!({"item_id":id(1)})).unwrap()
}
fn supplier_args() -> crate::master_data::SearchSupplierItems {
    serde_json::from_value(json!({"account_id":id(100)})).unwrap()
}
fn account_args() -> crate::account_context::GetAccountBookingContext {
    serde_json::from_value(json!({"account_id":id(100)})).unwrap()
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
async fn article_details_keep_item_group_purchase_and_cost_defaults_distinct() {
    let mut selected = item(1);
    selected["ItemGroup"] = json!(id(10));
    selected["ItemGroupCode"] = json!(" G1 ");
    selected["GLCosts"] = json!(id(401));
    selected["GLRevenue"] = json!(id(402));
    selected["StandardSalesPrice"] = serde_json::from_str("123.123456789012345678901").unwrap();
    let group = json!({"ID":id(10),"Division":42,"Code":"G1","Description":"Invented group","GLCosts":id(403),"GLRevenue":id(404),"GLPurchaseAccount":id(405)});
    let server = Server::new(vec![rows(vec![selected]), rows(vec![group])]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let result =
        serde_json::to_value(backend.get_item_details(item_args()).await.unwrap()).unwrap();
    assert_eq!(result["item"]["code"], " 001 ");
    assert_eq!(result["item"]["gl_costs_id"], id(401).to_string());
    assert_eq!(result["item_group"]["gl_costs_id"], id(403).to_string());
    assert_eq!(
        result["item_group"]["gl_purchase_account_id"],
        id(405).to_string()
    );
    assert_eq!(
        result["item"]["standard_sales_price"],
        "123.123456789012345678901"
    );
    assert!(result["standard_sales_price_currency"].is_null());
    assert_eq!(result["item"]["cost_price_currency"], "USD");
    assert_eq!(result["item"]["cost_price_standard"], "0.2");
    assert_eq!(result["item"]["unit_code"], " pc ");
    assert!(result.get("effective_gl_account").is_none());
    let requests = server.finish();
    assert_eq!(
        query(&requests[0], "$filter"),
        Some(format!("ID eq guid'{}'", id(1)))
    );
    assert_eq!(
        query(&requests[1], "$filter"),
        Some(format!("ID eq guid'{}'", id(10)))
    );
    for request in requests {
        assert!(request.target.starts_with("GET /api/v1/42/"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn article_details_without_group_do_not_guess_or_read_a_default_group() {
    let server = Server::new(vec![rows(vec![item(1)])]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let result =
        serde_json::to_value(backend.get_item_details(item_args()).await.unwrap()).unwrap();
    assert!(result["item_group"].is_null());
    assert_eq!(result["item"]["is_fraction_allowed_item"], false);
    assert_eq!(result["item"]["is_taxable_item"], 1);
    assert!(result["item"].get("is_blocked").is_none());
    server.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn article_details_reject_missing_ambiguous_wrong_scope_and_bad_links() {
    for values in [
        vec![],
        vec![item(1), item(1)],
        vec![item(2)],
        vec![{
            let mut v = item(1);
            v["Division"] = json!(99);
            v
        }],
        vec![{
            let mut v = item(1);
            v["StartDate"] = json!("2025-01-01");
            v["EndDate"] = json!("2000-01-01");
            v
        }],
        vec![{
            let mut v = item(1);
            v["ItemGroup"] = json!(Uuid::nil());
            v
        }],
    ] {
        let server = Server::new(vec![rows(values)]);
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(backend.get_item_details(item_args()).await.is_err());
        server.finish();
    }
    for values in [
        vec![],
        vec![item(10), item(10)],
        vec![item(11)],
        vec![{
            let mut v = item(10);
            v["Division"] = json!(99);
            v
        }],
    ] {
        let mut selected = item(1);
        selected["ItemGroup"] = json!(id(10));
        selected["ItemGroupCode"] = json!("G1");
        let server = Server::new(vec![rows(vec![selected]), rows(values)]);
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(backend.get_item_details(item_args()).await.is_err());
        server.finish();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn article_search_validates_dates_even_in_excluded_rows_and_preserves_unknowns() {
    let mut invalid = item(2);
    invalid["StartDate"] = json!("2025-01-01");
    invalid["EndDate"] = json!("2000-01-01");
    let server = Server::new(vec![rows(vec![item(1), invalid])]);
    let (_directory, backend) = financial_backend(&server.base).await;
    assert!(
        backend
            .search_items(serde_json::from_value(json!({"query":"not found"})).unwrap())
            .await
            .is_err()
    );
    server.finish();
    let mut unknown = item(1);
    unknown["IsSalesItem"] = Value::Null;
    unknown["IsTaxableItem"] = Value::Null;
    let server = Server::new(vec![rows(vec![unknown])]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let result = serde_json::to_value(
        backend
            .search_items(serde_json::from_value(json!({})).unwrap())
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(result["records"][0]["is_sales_item"].is_null());
    assert!(result["records"][0]["is_taxable_item"].is_null());
    server.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn company_booking_context_preserves_separate_defaults_identifiers_and_fractional_discounts()
{
    let server = Server::new(vec![rows(vec![account()])]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let result = serde_json::to_value(
        backend
            .get_account_booking_context(account_args())
            .await
            .unwrap(),
    )
    .unwrap();
    let data = &result["account"];
    for (key,value) in json!({"code":" 00001 ","is_blocked":true,"sales_allowed":false,"customer_status":"P","country":"BE","vat_number":"BE0000000000","chamber_of_commerce":"00123456","purchase_vat_code":" 01 ","sales_vat_code":"02","purchase_currency":"USD","sales_currency":"GBP","discount_purchase":"0.055","discount_sales":null}).as_object().unwrap() {assert_eq!(&data[key],value);}
    assert_eq!(data["gl_account_purchase_id"], id(201).to_string());
    assert_eq!(data["gl_accounts_payable_id"], id(203).to_string());
    assert_eq!(data["invoice_account_id"], id(101).to_string());
    assert_eq!(data["id"], id(100).to_string());
    assert!(result.get("booking_allowed").is_none());
    let requests = server.finish();
    let fields = query(&requests[0], "$select").unwrap();
    for forbidden in [
        "BankAccounts",
        "BSN",
        "AddressLine1",
        "Email",
        "Phone",
        "Currency",
    ] {
        assert!(!fields.split(',').any(|field| field == forbidden));
    }
    assert_eq!(
        query(&requests[0], "$filter"),
        Some(format!("ID eq guid'{}'", id(100)))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn company_booking_context_rejects_wrong_or_ambiguous_identity_and_bad_dates() {
    for values in [
        vec![],
        vec![account(), account()],
        vec![{
            let mut v = account();
            v["ID"] = json!(id(101));
            v
        }],
        vec![{
            let mut v = account();
            v["Division"] = json!(99);
            v
        }],
        vec![{
            let mut v = account();
            v["Code"] = json!(" ");
            v
        }],
        vec![{
            let mut v = account();
            v["StartDate"] = json!("2025-01-01");
            v["EndDate"] = json!("2000-01-01");
            v
        }],
    ] {
        let server = Server::new(vec![rows(values)]);
        let (_directory, backend) = financial_backend(&server.base).await;
        assert!(
            backend
                .get_account_booking_context(account_args())
                .await
                .is_err()
        );
        server.finish();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supplier_links_read_all_pages_and_preserve_future_zero_prices_units_and_currency() {
    let mut second = link(12, 2);
    second["SupplierItemCode"] = json!("000ABC");
    second["ItemCode"] = json!("002");
    second["PurchasePrice"] = serde_json::from_str("12.1234567890123456789").unwrap();
    let server = Server::new(vec![
        rows(vec![account()]),
        Response::json(json!({"d":{"results":[second],"__next":"?page=2"}})),
        rows(vec![link(11, 1)]),
    ]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let result = serde_json::to_value(
        backend
            .search_supplier_items(
                serde_json::from_value(
                    json!({"account_id":id(100),"limit":1,"supplier_item_code":" 000ABC "}),
                )
                .unwrap(),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["source_link_count"], 2);
    assert_eq!(result["matched_link_count"], 2);
    assert_eq!(result["has_more"], true);
    assert_eq!(result["next_offset"], 1);
    let data = &result["links"][0];
    assert_eq!(data["purchase_price"], "0");
    assert_eq!(data["currency"], "CHF");
    assert_eq!(data["start_date"], "2099-01-01");
    assert_eq!(data["purchase_unit"], "box");
    assert_eq!(data["purchase_unit_factor"], "12.5");
    assert_eq!(data["minimum_quantity"], "1.25");
    assert_eq!(data["item_unit_id"], id(301).to_string());
    assert_eq!(data["purchase_vat_code"], "P0");
    assert_eq!(result["account"]["is_blocked"], true);
    assert!(
        result["coverage"]
            .as_str()
            .unwrap()
            .contains("not_full_history")
    );
    let requests = server.finish();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        query(&requests[1], "$filter"),
        Some(format!("Supplier eq guid'{}'", id(100)))
    );
    assert!(!requests[1].target.contains("000ABC"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supplier_reference_is_exact_literal_and_never_numeric_or_description_matching() {
    for reference in ["000ABC", "0", "000abc", "%_"] {
        let server = Server::new(vec![rows(vec![account()]), rows(vec![link(11, 1)])]);
        let (_directory, backend) = financial_backend(&server.base).await;
        let result = serde_json::to_value(
            backend
                .search_supplier_items(
                    serde_json::from_value(
                        json!({"account_id":id(100),"supplier_item_code":reference}),
                    )
                    .unwrap(),
                )
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            result["matched_link_count"],
            if reference == "000ABC" { 1 } else { 0 }
        );
        server.finish();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supplier_item_selection_revalidates_item_and_requires_matching_source_association() {
    let server = Server::new(vec![
        rows(vec![account()]),
        rows(vec![item(1)]),
        rows(vec![link(11, 1)]),
    ]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let mut args = supplier_args();
    args.item_id = Some(id(1).to_string());
    let result = serde_json::to_value(backend.search_supplier_items(args).await.unwrap()).unwrap();
    assert_eq!(result["item_id"], id(1).to_string());
    let requests = server.finish();
    assert_eq!(
        query(&requests[2], "$filter"),
        Some(format!(
            "Supplier eq guid'{}' and Item eq guid'{}'",
            id(100),
            id(1)
        ))
    );
    for bad in [link(11, 2), {
        let mut v = link(11, 1);
        v["ItemCode"] = json!("different");
        v
    }] {
        let server = Server::new(vec![
            rows(vec![account()]),
            rows(vec![item(1)]),
            rows(vec![bad]),
        ]);
        let (_directory, backend) = financial_backend(&server.base).await;
        let mut args = supplier_args();
        args.item_id = Some(id(1).to_string());
        assert!(backend.search_supplier_items(args).await.is_err());
        server.finish();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supplier_links_validate_hidden_rows_scope_dates_and_duplicates() {
    for values in [
        vec![link(11, 1), link(11, 1)],
        vec![{
            let mut v = link(11, 1);
            v["Supplier"] = json!(id(101));
            v
        }],
        vec![{
            let mut v = link(11, 1);
            v["Division"] = json!(99);
            v
        }],
        vec![{
            let mut v = link(11, 1);
            v["Item"] = json!(Uuid::nil());
            v
        }],
        vec![{
            let mut v = link(11, 1);
            v["ID"] = json!(Uuid::nil());
            v
        }],
        vec![{
            let mut v = link(11, 1);
            v["EndDate"] = json!("2000-01-01");
            v
        }],
    ] {
        let server = Server::new(vec![rows(vec![account()]), rows(values)]);
        let (_directory, backend) = financial_backend(&server.base).await;
        let mut args = supplier_args();
        args.supplier_item_code = Some("not found".into());
        assert!(backend.search_supplier_items(args).await.is_err());
        server.finish();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supplier_links_keep_complete_counts_in_late_or_empty_windows_and_null_prices() {
    for values in [
        vec![],
        vec![{
            let mut v = link(11, 1);
            v["PurchasePrice"] = Value::Null;
            v["Currency"] = Value::Null;
            v
        }],
    ] {
        let count = values.len();
        let server = Server::new(vec![rows(vec![account()]), rows(values)]);
        let (_directory, backend) = financial_backend(&server.base).await;
        let mut args = supplier_args();
        args.offset = 50;
        let result =
            serde_json::to_value(backend.search_supplier_items(args).await.unwrap()).unwrap();
        assert_eq!(result["source_link_count"], count);
        assert_eq!(result["matched_link_count"], count);
        assert_eq!(result["links"], json!([]));
        assert_eq!(result["has_more"], false);
        assert!(result["next_offset"].is_null());
        server.finish();
    }
    let mut value = link(11, 1);
    value["PurchasePrice"] = Value::Null;
    value["Currency"] = Value::Null;
    let server = Server::new(vec![rows(vec![account()]), rows(vec![value])]);
    let (_directory, backend) = financial_backend(&server.base).await;
    let result = serde_json::to_value(
        backend
            .search_supplier_items(supplier_args())
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(result["links"][0]["purchase_price"].is_null());
    assert!(result["links"][0]["currency"].is_null());
    server.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn item_and_account_arguments_fail_before_remote_reads() {
    let server = Server::new(vec![]);
    let (_directory, backend) = financial_backend(&server.base).await;
    for invalid in ["invalid".to_string(), Uuid::nil().to_string()] {
        assert!(
            backend
                .get_item_details(serde_json::from_value(json!({"item_id":invalid})).unwrap())
                .await
                .is_err()
        );
        assert!(
            backend
                .get_account_booking_context(
                    serde_json::from_value(json!({"account_id":invalid})).unwrap()
                )
                .await
                .is_err()
        );
        let mut args = supplier_args();
        args.item_id = Some(invalid.clone());
        assert!(backend.search_supplier_items(args).await.is_err());
        let mut args = supplier_args();
        args.account_id = invalid;
        assert!(backend.search_supplier_items(args).await.is_err());
    }
    for args in [
        json!({"account_id":id(100),"supplier_item_code":" "}),
        json!({"account_id":id(100),"supplier_item_code":"x".repeat(101)}),
        json!({"account_id":id(100),"limit":0}),
        json!({"account_id":id(100),"limit":51}),
        json!({"account_id":id(100),"offset":u32::MAX}),
    ] {
        assert!(
            backend
                .search_supplier_items(serde_json::from_value(args).unwrap())
                .await
                .is_err()
        );
    }
    assert!(
        serde_json::from_value::<crate::master_data::GetItemDetails>(
            json!({"item_id":id(1),"unexpected":true})
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<crate::account_context::GetAccountBookingContext>(
            json!({"account_id":id(100),"unexpected":true})
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<crate::master_data::SearchSupplierItems>(
            json!({"account_id":id(100),"unexpected":true})
        )
        .is_err()
    );
    server.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn item_context_source_errors_never_return_partial_results_or_private_error_bodies() {
    for operation in ["account", "item", "supplier"] {
        let failed = Response {
            status: "403 Forbidden",
            body: "private source body".into(),
            location: None,
        };
        let responses = if operation == "supplier" {
            vec![
                rows(vec![account()]),
                Response::json(json!({"d":{"results":[link(11,1)],"__next":"?page=2"}})),
                failed,
            ]
        } else {
            vec![failed]
        };
        let server = Server::new(responses);
        let (_directory, backend) = financial_backend(&server.base).await;
        let error = match operation {
            "account" => backend
                .get_account_booking_context(account_args())
                .await
                .err()
                .unwrap(),
            "item" => backend.get_item_details(item_args()).await.err().unwrap(),
            _ => backend
                .search_supplier_items(supplier_args())
                .await
                .err()
                .unwrap(),
        };
        assert!(format!("{error:#}").contains("403"));
        assert!(!format!("{error:#}").contains("private source body"));
        server.finish();
    }
}

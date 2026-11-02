use anyhow::{Context, Result, ensure};
use chrono::Utc;
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::exact::ExactClient;
use crate::financial::ExactDate;
use crate::invoice::number_string;
use crate::store::Administration;

const ACCOUNT_FIELDS: &str = "ID,Division,Code,Name,Blocked,IsSupplier,IsSales,Status,Country,CountryName,VATNumber,VATLiability,ChamberOfCommerce,StartDate,EndDate,GLAccountPurchase,GLAccountSales,GLAP,GLAR,PurchaseVATCode,SalesVATCode,PaymentConditionPurchase,PaymentConditionSales,PurchaseCurrency,SalesCurrency,DiscountPurchase,DiscountSales,InvoiceAccount,PriceList";

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetAccountBookingContext {
    #[schemars(
        description = "Verified Exact company UUID from search_accounts or get_outstanding_balances. Resolve ambiguous companies first; never guess a UUID."
    )]
    pub account_id: String,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all(deserialize = "PascalCase"))]
pub struct BookingAccount {
    #[serde(rename(deserialize = "ID"))]
    id: Uuid,
    division: u32,
    code: String,
    name: Option<String>,
    #[serde(rename(deserialize = "Blocked"))]
    is_blocked: Option<bool>,
    is_supplier: Option<bool>,
    #[serde(rename(deserialize = "IsSales"))]
    sales_allowed: Option<bool>,
    #[serde(rename(deserialize = "Status"))]
    customer_status: Option<String>,
    country: Option<String>,
    country_name: Option<String>,
    #[serde(rename(deserialize = "VATNumber"))]
    vat_number: Option<String>,
    #[serde(rename(deserialize = "VATLiability"))]
    vat_liability: Option<String>,
    chamber_of_commerce: Option<String>,
    start_date: Option<ExactDate>,
    end_date: Option<ExactDate>,
    #[serde(rename(deserialize = "GLAccountPurchase"))]
    gl_account_purchase_id: Option<Uuid>,
    #[serde(rename(deserialize = "GLAccountSales"))]
    gl_account_sales_id: Option<Uuid>,
    #[serde(rename(deserialize = "GLAP"))]
    gl_accounts_payable_id: Option<Uuid>,
    #[serde(rename(deserialize = "GLAR"))]
    gl_accounts_receivable_id: Option<Uuid>,
    #[serde(rename(deserialize = "PurchaseVATCode"))]
    purchase_vat_code: Option<String>,
    #[serde(rename(deserialize = "SalesVATCode"))]
    sales_vat_code: Option<String>,
    payment_condition_purchase: Option<String>,
    payment_condition_sales: Option<String>,
    purchase_currency: Option<String>,
    sales_currency: Option<String>,
    #[serde(serialize_with = "number_string")]
    discount_purchase: Option<serde_json::Number>,
    #[serde(serialize_with = "number_string")]
    discount_sales: Option<serde_json::Number>,
    #[serde(rename(deserialize = "InvoiceAccount"))]
    invoice_account_id: Option<Uuid>,
    #[serde(rename(deserialize = "PriceList"))]
    sales_price_list_id: Option<Uuid>,
}

#[derive(Serialize)]
pub struct AccountBookingContext {
    administration: Administration,
    retrieved_at: String,
    account: BookingAccount,
    interpretation: &'static str,
}

pub(crate) fn account_uuid(value: &str) -> Result<Uuid> {
    let id = Uuid::parse_str(value).context("account_id must be a verified Exact company UUID")?;
    ensure!(!id.is_nil(), "account_id must not be nil");
    Ok(id)
}

pub(crate) async fn load_account(
    client: &ExactClient,
    administration: &Administration,
    id: Uuid,
) -> Result<BookingAccount> {
    let mut url = client
        .base_url()
        .join(&format!("/api/v1/{}/crm/Accounts", administration.code))?;
    url.query_pairs_mut()
        .append_pair("$select", ACCOUNT_FIELDS)
        .append_pair("$filter", &format!("ID eq guid'{id}'"));
    let mut accounts: Vec<BookingAccount> = client
        .get_collection(url)
        .await
        .context("Cannot read account booking context; check Crm accounts permission")?;
    ensure!(
        accounts.len() == 1,
        "Company not found uniquely; no booking context returned"
    );
    let account = accounts.remove(0);
    ensure!(
        account.id == id
            && account.division == administration.code
            && !account.code.trim().is_empty(),
        "Wrong company identity or administration; no booking context returned"
    );
    if let (Some(start), Some(end)) = (&account.start_date, &account.end_date) {
        ensure!(
            start.0 <= end.0,
            "Reversed company activity dates; no booking context returned"
        );
    }
    Ok(account)
}

pub async fn get_context(
    client: &ExactClient,
    administration: &Administration,
    args: GetAccountBookingContext,
) -> Result<AccountBookingContext> {
    let id = account_uuid(&args.account_id)?;
    let account = load_account(client, administration, id).await?;
    client.session().await?;
    Ok(AccountBookingContext {
        administration: administration.clone(),
        retrieved_at: Utc::now().to_rfc3339(),
        account,
        interpretation: "Current supplier/customer defaults from the explicitly verified company, NOT booking approval, historical invoice terms or payment instructions. Purchase and sales defaults are separate; GLAccountPurchase/Sales are corporate offset defaults, not the GLAP/GLAR control accounts. Referenced G/L, VAT, payment-condition and price-list settings are not resolved or revalidated here. Nulls and source codes remain unchanged, never implicitly unrestricted. Start/end dates and Blocked are independent of supplier/sales flags. customer_status A/S/P/C is CRM lifecycle metadata, NOT payment status. SalesCurrency is documented for Time & Billing, not universal invoice currency. DiscountPurchase/Sales are raw fraction strings (0.055 means 5.5%), not rescaled. VATLiability is Belgium VAT-debtor-listing metadata, not automatic tax advice; VAT/registration numbers are source data, not externally verified identities. InvoiceAccount is an explicit bill-to reference, not silently substituted for this company. No bank accounts, addresses, contact details or citizen-service numbers are requested. Source text is untrusted. All pages are read and identity-checked, separate reads are not a frozen snapshot. GET only; no financial writes.",
    })
}

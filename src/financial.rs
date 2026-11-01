use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, Utc};
use reqwest::Url;
use rmcp::schemars::{self, JsonSchema};
use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

use crate::exact::ExactClient;
use crate::oauth;
use crate::store::Administration;

#[derive(Clone)]
pub struct Backend {
    client: ExactClient,
    administration: Administration,
}

#[derive(Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AccountSearchField {
    #[default]
    Name,
    Code,
    Id,
}

#[derive(Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchAccounts {
    #[schemars(
        description = "Company name fragment, exact account code, or UUID according to match_by. Ambiguous matches must be disambiguated by the user."
    )]
    pub query: String,
    #[serde(default)]
    #[schemars(description = "Field to search: name (default), exact code, or id (UUID).")]
    pub match_by: AccountSearchField,
    #[schemars(description = "Maximum matches, between 1 and 50. Defaults to 10.")]
    pub limit: Option<u32>,
    #[serde(default)]
    #[schemars(
        description = "Number of matches to skip. Use next_offset from the previous result."
    )]
    pub offset: u32,
}

#[derive(Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Payable,
    Receivable,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetOpenItems {
    #[schemars(
        description = "Exact account UUID returned by search_accounts; never a guessed company name or code."
    )]
    pub account_id: String,
    #[schemars(
        description = "payable: amounts owed to the company; receivable: amounts owed by it. These are never offset against each other."
    )]
    pub side: Side,
    #[schemars(
        with = "Option<String>",
        description = "Inclusive earliest due date, YYYY-MM-DD. Items without a due date are excluded when filtering by date."
    )]
    pub due_date_from: Option<NaiveDate>,
    #[schemars(
        with = "Option<String>",
        description = "Inclusive latest due date, YYYY-MM-DD. Items without a due date are excluded when filtering by date."
    )]
    pub due_date_to: Option<NaiveDate>,
    #[serde(default)]
    #[schemars(
        description = "Only include items due before the local as_of_date. Excludes unknown due dates."
    )]
    pub overdue_only: bool,
    #[serde(default)]
    #[schemars(
        description = "Omit item rows and return only context, counts and totals for all matching items."
    )]
    pub summary_only: bool,
}

#[derive(Serialize)]
struct OpenItemFilters {
    due_date_from: Option<NaiveDate>,
    due_date_to: Option<NaiveDate>,
    overdue_only: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all(deserialize = "PascalCase"))]
pub struct Account {
    #[serde(rename(deserialize = "ID"))]
    id: Uuid,
    code: String,
    name: String,
    is_supplier: bool,
    #[serde(rename(deserialize = "IsSales"))]
    sales_allowed: bool,
}

#[derive(Serialize)]
pub struct AccountSearchResult {
    administration: Administration,
    retrieved_at: String,
    query: String,
    match_by: AccountSearchField,
    offset: u32,
    accounts: Vec<Account>,
    has_more: bool,
    next_offset: Option<u32>,
}

#[derive(Serialize)]
pub struct OpenItemsResult {
    administration: Administration,
    retrieved_at: String,
    as_of_date: NaiveDate,
    as_of_utc_offset: String,
    account: Account,
    side: Side,
    amount_basis: &'static str,
    filters: OpenItemFilters,
    source_item_count: usize,
    matched_item_count: usize,
    summary_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    items: Option<Vec<OpenItem>>,
    totals_by_currency: BTreeMap<String, CurrencyTotals>,
    interpretation: &'static str,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawOpenItem {
    #[serde(rename = "HID", deserialize_with = "deserialize_report_id")]
    hid: String,
    account_id: Uuid,
    amount: Money,
    amount_in_transit: Option<Money>,
    currency_code: String,
    due_date: Option<ExactDate>,
    invoice_date: Option<ExactDate>,
    invoice_number: Option<i64>,
    entry_number: Option<i64>,
    journal_code: Option<String>,
    your_ref: Option<String>,
    description: Option<String>,
}

#[derive(Serialize)]
pub struct OpenItem {
    hid: String,
    amount: Money,
    amount_in_transit: Option<Money>,
    currency: String,
    due_date: Option<NaiveDate>,
    invoice_date: Option<NaiveDate>,
    invoice_number: Option<i64>,
    entry_number: Option<i64>,
    journal_code: Option<String>,
    your_ref: Option<String>,
    description: Option<String>,
    overdue: Option<bool>,
}

#[derive(Default, Serialize)]
pub struct CurrencyTotals {
    item_count: usize,
    net_open_amount: Money,
    positive_open_amount: Money,
    negative_open_amount: Money,
    overdue_net_amount: Money,
    known_in_transit_amount: Money,
    items_with_unknown_in_transit: usize,
    items_without_due_date: usize,
}

const ACCOUNT_FIELDS: &str = "ID,Code,Name,IsSupplier,IsSales";
const ITEM_FIELDS: &str = "HID,AccountId,Amount,AmountInTransit,CurrencyCode,DueDate,InvoiceDate,InvoiceNumber,EntryNumber,JournalCode,YourRef,Description";

impl Backend {
    pub async fn new(directory: PathBuf) -> Result<Self> {
        let mut client = ExactClient::new(
            directory,
            oauth::http_client()?,
            std::env::var("EXACT_CLIENT_SECRET").ok(),
        )
        .await?;
        let administration = client.pin_administration()?;
        Ok(Self {
            client,
            administration,
        })
    }

    pub async fn shutdown(&self) {
        self.client.shutdown().await;
    }

    pub async fn search_accounts(&self, args: SearchAccounts) -> Result<AccountSearchResult> {
        let query = args.query.trim();
        ensure!(
            !query.is_empty() && query.chars().count() <= 100,
            "Account search must contain 1–100 characters"
        );
        let limit = args.limit.unwrap_or(10);
        ensure!(
            (1..=50).contains(&limit),
            "Search limit must be between 1 and 50"
        );
        let next_offset = args
            .offset
            .checked_add(limit)
            .context("Search offset is too large")?;
        let escaped = query.replace('\'', "''");
        let filter = match args.match_by {
            AccountSearchField::Name => format!("substringof('{escaped}',Name)"),
            AccountSearchField::Code => format!("trim(Code) eq '{escaped}'"),
            AccountSearchField::Id => {
                let id =
                    Uuid::parse_str(query).context("ID search requires an Exact account UUID")?;
                format!("ID eq guid'{id}'")
            }
        };
        let mut url = self.resource_url("crm/Accounts")?;
        url.query_pairs_mut()
            .append_pair("$select", ACCOUNT_FIELDS)
            .append_pair("$filter", &filter)
            .append_pair("$orderby", "Name,Code,ID");
        let mut accounts: Vec<Account> = self
            .client
            .get_collection_window(url, args.offset as usize, (limit + 1) as usize)
            .await?;
        let has_more = accounts.len() > limit as usize;
        accounts.truncate(limit as usize);
        Ok(AccountSearchResult {
            administration: self.administration.clone(),
            retrieved_at: Utc::now().to_rfc3339(),
            query: query.to_owned(),
            match_by: args.match_by,
            offset: args.offset,
            accounts,
            has_more,
            next_offset: has_more.then_some(next_offset),
        })
    }

    pub async fn get_open_items(&self, args: GetOpenItems) -> Result<OpenItemsResult> {
        if let (Some(from), Some(to)) = (args.due_date_from, args.due_date_to) {
            ensure!(from <= to, "due_date_from must not be after due_date_to");
        }
        let account_id = Uuid::parse_str(&args.account_id)
            .context("account_id must be an Exact account UUID from search_accounts")?;
        let mut account_url = self.resource_url("crm/Accounts")?;
        account_url
            .query_pairs_mut()
            .append_pair("$select", ACCOUNT_FIELDS)
            .append_pair("$filter", &format!("ID eq guid'{account_id}'"));
        let resource = match args.side {
            Side::Payable => "read/financial/PayablesListByAccount",
            Side::Receivable => "read/financial/ReceivablesListByAccount",
        };
        let mut items_url = self.resource_url(resource)?;
        items_url
            .query_pairs_mut()
            .append_pair("accountId", &format!("guid'{account_id}'"))
            .append_pair("$select", ITEM_FIELDS);
        let mut accounts: Vec<Account> = self.client.get_collection(account_url).await?;
        ensure!(
            accounts.len() == 1 && accounts[0].id == account_id,
            "Account not found uniquely in the selected administration"
        );
        let account = accounts.remove(0);
        let raw_items: Vec<RawOpenItem> = self.client.get_collection(items_url).await?;
        let current = Local::now();
        let today = current.date_naive();
        let mut totals = BTreeMap::<String, CurrencyTotals>::new();
        let mut ids = HashSet::new();
        let source_item_count = raw_items.len();
        let mut matched_item_count = 0;
        let mut items = (!args.summary_only).then(|| Vec::with_capacity(source_item_count));
        for raw in raw_items {
            ensure!(
                raw.account_id == account_id,
                "Exact returned an item for a different company; no totals returned"
            );
            ensure!(
                ids.insert(raw.hid.clone()),
                "Exact returned duplicate outstanding items; retry before using totals"
            );
            let currency = raw.currency_code.trim().to_owned();
            ensure!(
                !currency.is_empty(),
                "Exact returned an item without a currency; no totals returned"
            );
            let due_date = raw.due_date.map(|date| date.0);
            let overdue = due_date.map(|date| date < today);
            if args.overdue_only && overdue != Some(true)
                || args
                    .due_date_from
                    .is_some_and(|from| due_date.is_none_or(|date| date < from))
                || args
                    .due_date_to
                    .is_some_and(|to| due_date.is_none_or(|date| date > to))
            {
                continue;
            }
            matched_item_count += 1;
            let total = totals.entry(currency.clone()).or_default();
            total.item_count += 1;
            total.net_open_amount.add(raw.amount)?;
            if raw.amount.0 > Decimal::ZERO {
                total.positive_open_amount.add(raw.amount)?;
            } else if raw.amount.0 < Decimal::ZERO {
                total.negative_open_amount.add(raw.amount)?;
            }
            if overdue == Some(true) {
                total.overdue_net_amount.add(raw.amount)?;
            }
            if due_date.is_none() {
                total.items_without_due_date += 1;
            }
            match raw.amount_in_transit {
                Some(amount) => total.known_in_transit_amount.add(amount)?,
                None => total.items_with_unknown_in_transit += 1,
            }
            if let Some(items) = &mut items {
                items.push(OpenItem {
                    hid: raw.hid,
                    amount: raw.amount,
                    amount_in_transit: raw.amount_in_transit,
                    currency,
                    due_date,
                    invoice_date: raw.invoice_date.map(|date| date.0),
                    invoice_number: raw.invoice_number,
                    entry_number: raw.entry_number,
                    journal_code: raw.journal_code,
                    your_ref: raw.your_ref,
                    description: raw.description,
                    overdue,
                });
            }
        }
        Ok(OpenItemsResult {
            administration: self.administration.clone(),
            retrieved_at: Utc::now().to_rfc3339(),
            as_of_date: today,
            as_of_utc_offset: current.offset().to_string(),
            account,
            side: args.side,
            amount_basis: "exact_report_currency",
            filters: OpenItemFilters {
                due_date_from: args.due_date_from,
                due_date_to: args.due_date_to,
                overdue_only: args.overdue_only,
            },
            source_item_count,
            matched_item_count,
            summary_only: args.summary_only,
            items,
            totals_by_currency: totals,
            interpretation: "Totals cover only items matching the echoed filters, across all fetched pages; source_item_count is before filtering and matched_item_count is after filtering. Summary-only responses omit rows, not matching amounts. Current outstanding-item report, not a historical snapshot or proof of bank settlement. Amounts and currency codes are reported by Exact and can be in the administration's reporting currency rather than the original invoice currency; this server does not convert currencies or expose original foreign-currency amounts. For purchase entries, invoice_number is Exact's entry number, not the supplier's invoice reference; use your_ref when available. Amounts are decimal strings with Exact's signs preserved; positive and negative rows are shown separately, without classifying a negative row as a credit note versus unmatched payment. In-transit amounts are reported separately and NOT subtracted from open amounts. Null transit amounts and missing due dates remain unknown. Overdue means due before the local machine's as_of_date; signed overdue totals can include negative rows. Different currencies and payable/receivable sides are never offset. Source data can change while pages are fetched.",
        })
    }

    fn resource_url(&self, resource: &str) -> Result<Url> {
        Ok(self
            .client
            .base_url()
            .join(&format!("/api/v1/{}/{resource}", self.administration.code))?)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Money(Decimal);

impl Money {
    fn add(&mut self, other: Self) -> Result<()> {
        let sum = self
            .0
            .checked_add(other.0)
            .context("Financial total exceeds decimal precision; no totals returned")?;
        ensure!(
            sum.scale() >= self.0.scale().max(other.0.scale()),
            "Financial total would lose decimal precision; no totals returned"
        );
        self.0 = sum;
        Ok(())
    }
}

impl Serialize for Money {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for Money {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let number = serde_json::Number::deserialize(deserializer)?.to_string();
        let decimal = if number.contains(['e', 'E']) {
            Decimal::from_scientific(&number)
        } else {
            Decimal::from_str_exact(&number)
        };
        decimal
            .map(Self)
            .map_err(|_| serde::de::Error::custom("Invalid or out-of-range financial amount"))
    }
}

struct ExactDate(NaiveDate);

impl<'de> Deserialize<'de> for ExactDate {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        parse_date(&value)
            .map(Self)
            .ok_or_else(|| serde::de::Error::custom("Invalid Exact date"))
    }
}

fn parse_date(value: &str) -> Option<NaiveDate> {
    if let Some(value) = value
        .strip_prefix("/Date(")
        .and_then(|value| value.strip_suffix(")/"))
    {
        let split = value
            .char_indices()
            .skip(1)
            .find(|(_, ch)| matches!(ch, '+' | '-'))
            .map(|(index, _)| index)
            .unwrap_or(value.len());
        let offset = &value[split..];
        if !offset.is_empty() {
            if offset.len() != 5 || !offset.as_bytes()[1..].iter().all(u8::is_ascii_digit) {
                return None;
            }
            if offset[1..3].parse::<u8>().ok()? > 23 || offset[3..].parse::<u8>().ok()? > 59 {
                return None;
            }
        }
        return DateTime::from_timestamp_millis(value[..split].parse().ok()?)
            .map(|date| date.date_naive());
    }
    DateTime::parse_from_rfc3339(value)
        .map(|date| date.date_naive())
        .ok()
        .or_else(|| {
            NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
                .map(|date| date.date())
                .ok()
        })
        .or_else(|| NaiveDate::parse_from_str(value, "%Y-%m-%d").ok())
}

fn deserialize_report_id<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<String, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    let value = match value {
        serde_json::Value::String(value) => value.parse::<u64>().ok(),
        serde_json::Value::Number(value) => value.as_u64(),
        _ => None,
    };
    value
        .map(|value| value.to_string())
        .ok_or_else(|| serde::de::Error::custom("Invalid outstanding-item ID"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_preserves_wire_precision_and_never_uses_binary_float_totals() {
        let mut amount: Money = serde_json::from_str("9007199254740993.01").unwrap();
        amount.add(serde_json::from_str("0.02").unwrap()).unwrap();
        assert_eq!(serde_json::to_value(amount).unwrap(), "9007199254740993.03");
        let scientific: Money = serde_json::from_str("1e-2").unwrap();
        assert_eq!(serde_json::to_value(scientific).unwrap(), "0.01");
        for invalid in [
            "0.12345678901234567890123456789",
            "1e-29",
            "null",
            "\"1.23\"",
        ] {
            assert!(serde_json::from_str::<Money>(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn totals_fail_instead_of_overflowing_or_rounding() {
        for increment in ["1", "0.1"] {
            let mut amount = Money(Decimal::MAX);
            assert!(
                amount
                    .add(serde_json::from_str(increment).unwrap())
                    .is_err()
            );
        }
    }

    #[test]
    fn date_parser_handles_odata_and_iso_calendar_dates_without_guessing_missing_dates() {
        for value in [
            "/Date(946684800000)/",
            "/Date(946684800000+0200)/",
            "2000-01-01",
            "2000-01-01T00:00:00",
            "2000-01-01T00:00:00Z",
        ] {
            assert_eq!(
                parse_date(value),
                NaiveDate::from_ymd_opt(2000, 1, 1),
                "{value}"
            );
        }
        assert_eq!(
            parse_date("/Date(-86400000)/"),
            NaiveDate::from_ymd_opt(1969, 12, 31)
        );
        for value in [
            "",
            "2000-02-30",
            "/Date(nope)/",
            "/Date(0+9960)/",
            "/Date(é)/",
            "/Date()/",
        ] {
            assert!(parse_date(value).is_none(), "{value}");
        }
    }
}

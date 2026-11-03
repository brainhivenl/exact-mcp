use std::collections::{BTreeMap, BTreeSet, HashSet};

use anyhow::{Context, Result, ensure};
use chrono::{NaiveDate, Utc};
use reqwest::Url;
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::exact::ExactClient;
use crate::financial::{ExactDate, Money, deserialize_report_id};
use crate::invoice;
use crate::store::{Administration, BankConfiguration};

mod proposal;
pub(crate) use proposal::propose;
pub use proposal::{BankHandlingProposal, ProposeBankTransactionHandling};

#[cfg(test)]
mod tests;

const JOURNAL_FIELDS: &str = "ID,Division,Code,Type,GLAccount,Description";
const GL_FIELDS: &str = "ID,Division,Code,Description,Type";
const LINE_FIELDS: &str = "ID,EntryID,Division,Account,AccountCode,AccountName,EntryNumber,FinancialYear,FinancialPeriod,JournalCode,JournalDescription,GLAccount,GLAccountCode,GLAccountDescription,LineNumber,LineType,Type,Status,Date,AmountDC,AmountFC,Currency,InvoiceNumber,YourRef,Description,OffsetID,VATCode,VATPercentage,AmountVATFC,AmountVATBaseFC";
const REPORT_FIELDS: &str = "HID,AccountId,EntryNumber,JournalCode,Amount,AmountInTransit,CurrencyCode,InvoiceDate,InvoiceNumber,YourRef,Description";
const BATCH_SIZE: usize = 20;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetBankTransactionsToHandle {
    #[schemars(
        description = "Optional 1–10 verified G/L UUIDs explicitly confirmed as unassigned-bank allocation accounts. Omit to use saved settings for the selected administration; an explicit list overrides settings for this call without saving. Empty lists are invalid. No automatic code/name/type inference."
    )]
    pub unassigned_gl_account_ids: Option<Vec<String>>,
    #[schemars(
        description = "Optional verified bank/payment-service journal code; checked against current journals of Type 12 or 16."
    )]
    pub journal_code: Option<String>,
    #[schemars(
        with = "Option<String>",
        description = "Inclusive earliest Exact accounting posting date, YYYY-MM-DD; not a due, value or settlement date. Unknown dates are excluded when bounded."
    )]
    pub transaction_date_from: Option<NaiveDate>,
    #[schemars(
        with = "Option<String>",
        description = "Inclusive latest Exact accounting posting date, YYYY-MM-DD; not a due, value or settlement date."
    )]
    pub transaction_date_to: Option<NaiveDate>,
    #[schemars(
        description = "Transactions per response, 1–100; defaults to 50. All source pages and unresolved items are read regardless of pagination."
    )]
    pub limit: Option<u32>,
    #[serde(default)]
    pub offset: u32,
}

impl GetBankTransactionsToHandle {
    fn validate(&self) -> Result<Option<BankConfiguration>> {
        ensure!(
            (1..=100).contains(&self.limit.unwrap_or(50)),
            "Bank transaction limit must be between 1 and 100"
        );
        self.offset
            .checked_add(self.limit.unwrap_or(50))
            .context("Bank transaction offset is too large")?;
        ensure!(
            self.journal_code
                .as_ref()
                .is_none_or(|code| !code.trim().is_empty()),
            "journal_code must not be blank"
        );
        if let (Some(from), Some(to)) = (self.transaction_date_from, self.transaction_date_to) {
            ensure!(
                from <= to,
                "transaction_date_from must not be after transaction_date_to"
            );
        }
        self.unassigned_gl_account_ids
            .as_deref()
            .map(BankConfiguration::from_argument_ids)
            .transpose()
    }

    fn matches_date(&self, date: Option<&ExactDate>) -> bool {
        self.transaction_date_from
            .is_none_or(|from| date.is_some_and(|date| date.0 >= from))
            && self
                .transaction_date_to
                .is_none_or(|to| date.is_some_and(|date| date.0 <= to))
    }
}

impl BankConfiguration {
    fn from_argument_ids(values: &[String]) -> Result<Self> {
        let configuration = Self {
            unassigned_gl_account_ids: values
                .iter()
                .map(|value| {
                    Uuid::parse_str(value)
                        .context("unassigned_gl_account_ids must contain G/L UUIDs")
                })
                .collect::<Result<_>>()?,
        };
        configuration.validate()?;
        Ok(configuration)
    }
}

impl Administration {
    async fn validate_bank_scope(&self, client: &ExactClient) -> Result<()> {
        let session = client.session().await?;
        ensure!(
            session.administration.as_ref().map(|item| item.code) == Some(self.code),
            "Bank queue administration differs from the selected session"
        );
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all(deserialize = "PascalCase"))]
struct Journal {
    #[serde(rename(deserialize = "ID"))]
    id: Uuid,
    division: u32,
    code: String,
    #[serde(rename(deserialize = "Type"))]
    type_code: i32,
    #[serde(rename(deserialize = "GLAccount"))]
    gl_account_id: Uuid,
    description: Option<String>,
}

impl Journal {
    fn validate(&self, division: u32) -> Result<()> {
        ensure!(
            self.division == division
                && !self.id.is_nil()
                && !self.gl_account_id.is_nil()
                && !self.code.trim().is_empty()
                && matches!(self.type_code, 12 | 16),
            "Invalid or wrong-scope bank journal; no queue returned"
        );
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all(deserialize = "PascalCase"))]
struct GlAccount {
    #[serde(rename(deserialize = "ID"))]
    id: Uuid,
    division: u32,
    code: String,
    description: Option<String>,
    #[serde(rename(deserialize = "Type"))]
    type_code: Option<i32>,
}

impl GlAccount {
    fn validate_for(
        &self,
        division: u32,
        selected: &BTreeSet<Uuid>,
        include_outstanding: bool,
    ) -> Result<()> {
        ensure!(
            self.division == division
                && !self.id.is_nil()
                && (selected.contains(&self.id)
                    || (include_outstanding && matches!(self.type_code, Some(20 | 22)))),
            "Invalid or wrong-scope G/L metadata; no queue returned"
        );
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all(deserialize = "PascalCase"))]
struct PostingLine {
    #[serde(rename(deserialize = "ID"))]
    id: Uuid,
    #[serde(rename(deserialize = "EntryID"))]
    entry_id: Uuid,
    division: u32,
    #[serde(rename(deserialize = "Account"))]
    account_id: Option<Uuid>,
    account_code: Option<String>,
    account_name: Option<String>,
    entry_number: i64,
    financial_year: i32,
    financial_period: i32,
    journal_code: String,
    journal_description: Option<String>,
    #[serde(rename(deserialize = "GLAccount"))]
    gl_account_id: Uuid,
    #[serde(rename(deserialize = "GLAccountCode"))]
    gl_account_code: Option<String>,
    #[serde(rename(deserialize = "GLAccountDescription"))]
    gl_account_description: Option<String>,
    line_number: i32,
    line_type: Option<i32>,
    #[serde(rename(deserialize = "Type"))]
    type_code: i32,
    #[serde(rename(deserialize = "Status"))]
    status_code: Option<i32>,
    #[serde(rename(deserialize = "Date"))]
    posting_date: Option<ExactDate>,
    #[serde(rename(deserialize = "AmountDC"))]
    amount_default_currency: Option<Money>,
    #[serde(rename(deserialize = "AmountFC"))]
    amount_transaction_currency: Option<Money>,
    currency: Option<String>,
    invoice_number: Option<i64>,
    your_ref: Option<String>,
    description: Option<String>,
    #[serde(rename(deserialize = "OffsetID"))]
    offset_line_id: Option<Uuid>,
    #[serde(rename(deserialize = "VATCode"))]
    vat_code: Option<String>,
    #[serde(
        rename(deserialize = "VATPercentage"),
        serialize_with = "number_string"
    )]
    vat_percentage: Option<serde_json::Number>,
    #[serde(rename(deserialize = "AmountVATFC"))]
    vat_amount_transaction_currency: Option<Money>,
    #[serde(rename(deserialize = "AmountVATBaseFC"))]
    vat_base_amount_transaction_currency: Option<Money>,
}

impl PostingLine {
    fn validate_for(&self, division: u32, journals: &BTreeMap<String, Journal>) -> Result<()> {
        ensure!(
            self.division == division
                && !self.id.is_nil()
                && !self.entry_id.is_nil()
                && !self.gl_account_id.is_nil()
                && self.type_code == 40
                && journals.contains_key(self.journal_code.trim())
                && self.account_id.is_none_or(|id| !id.is_nil())
                && self
                    .offset_line_id
                    .is_none_or(|id| !id.is_nil() && id != self.id),
            "Invalid or wrong-scope bank posting; no queue returned"
        );
        Ok(())
    }

    fn validate_unassigned(&self, selected: &BTreeSet<Uuid>) -> Result<()> {
        ensure!(
            selected.contains(&self.gl_account_id) && self.offset_line_id.is_some(),
            "Unassigned posting escaped the configured G/L/offset scope; no queue returned"
        );
        Ok(())
    }

    fn validate_report(&self, report: &OutstandingRow) -> Result<()> {
        ensure!(
            self.account_id == Some(report.account_id)
                && Some(self.entry_number) == report.entry_number
                && self.journal_code.trim() == report.journal_code.trim(),
            "Outstanding counterpart escaped company/entry/journal/offset scope; no queue returned"
        );
        Ok(())
    }

    fn validate_control(&self, journals: &BTreeMap<String, Journal>) -> Result<()> {
        let journal = journals
            .get(self.journal_code.trim())
            .context("Control journal is missing")?;
        ensure!(
            self.gl_account_id == journal.gl_account_id,
            "Explicit offset does not identify the journal's own-bank control G/L; no queue returned"
        );
        Ok(())
    }

    fn validate_same_entry(&self, control: &Self) -> Result<()> {
        ensure!(
            self.entry_id == control.entry_id
                && self.entry_number == control.entry_number
                && self.financial_year == control.financial_year
                && self.financial_period == control.financial_period
                && self.journal_code.trim() == control.journal_code.trim(),
            "Explicit offset crosses a bank entry; no queue returned"
        );
        Ok(())
    }

    fn validate_against(&self, previous: &Self) -> Result<()> {
        self.validate_same_entry(previous)?;
        ensure!(
            self.id == previous.id
                && self.gl_account_id == previous.gl_account_id
                && self.account_id == previous.account_id
                && self.offset_line_id == previous.offset_line_id,
            "Bank source association changed during reconstruction; no queue returned"
        );
        Ok(())
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all(deserialize = "PascalCase"))]
struct OutstandingRow {
    #[serde(
        rename(deserialize = "HID"),
        deserialize_with = "deserialize_report_id"
    )]
    hid: String,
    account_id: Uuid,
    entry_number: Option<i64>,
    journal_code: String,
    amount: Option<Money>,
    amount_in_transit: Option<Money>,
    currency_code: String,
    invoice_date: Option<ExactDate>,
    invoice_number: Option<i64>,
    your_ref: Option<String>,
    description: Option<String>,
}

impl OutstandingRow {
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.account_id.is_nil()
                && !self.journal_code.trim().is_empty()
                && !self.currency_code.trim().is_empty(),
            "Invalid outstanding report identity/currency/journal; no queue returned"
        );
        Ok(())
    }

    fn validate_bank(&self) -> Result<()> {
        ensure!(
            self.entry_number.is_some(),
            "Bank outstanding row has no accounting entry number; no queue returned"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
enum Reason {
    #[serde(rename = "unassigned_posting")]
    Unassigned,
    #[serde(rename = "outstanding_payable_posting")]
    OutstandingPayable,
    #[serde(rename = "outstanding_receivable_posting")]
    OutstandingReceivable,
}

impl Reason {
    fn gl_type(self) -> Option<i32> {
        match self {
            Self::Unassigned => None,
            Self::OutstandingPayable => Some(22),
            Self::OutstandingReceivable => Some(20),
        }
    }
}

#[derive(Serialize)]
struct Evidence {
    reason: Reason,
    posting: PostingLine,
    outstanding_item: Option<OutstandingRow>,
}

impl Evidence {
    fn validate_unique_counterpart(
        &self,
        line: &PostingLine,
        gl_accounts: &BTreeMap<Uuid, GlAccount>,
    ) -> Result<()> {
        let Some(expected_type) = self.reason.gl_type() else {
            return Ok(());
        };
        let report = self
            .outstanding_item
            .as_ref()
            .context("Outstanding evidence is missing")?;
        let matches_report = line.account_id == Some(report.account_id)
            && Some(line.entry_number) == report.entry_number
            && line.journal_code.trim() == report.journal_code.trim()
            && gl_accounts
                .get(&line.gl_account_id)
                .is_some_and(|gl| gl.type_code == Some(expected_type));
        ensure!(
            line.id == self.posting.id || !matches_report,
            "Outstanding counterpart source became ambiguous during reconstruction; retry before using this queue"
        );
        Ok(())
    }

    fn validate_for(
        &self,
        line: &PostingLine,
        control: &PostingLine,
        selected: &BTreeSet<Uuid>,
        gl_accounts: &BTreeMap<Uuid, GlAccount>,
    ) -> Result<()> {
        line.validate_against(&self.posting)?;
        line.validate_same_entry(control)?;
        ensure!(
            line.offset_line_id == Some(control.id) && line.id != control.id,
            "Source posting no longer explicitly offsets the selected control; no queue returned"
        );
        if let Some(expected_type) = self.reason.gl_type() {
            ensure!(
                gl_accounts
                    .get(&line.gl_account_id)
                    .is_some_and(|gl| gl.type_code == Some(expected_type)),
                "Outstanding posting is not on its expected AP/AR G/L; no queue returned"
            );
            line.validate_report(
                self.outstanding_item
                    .as_ref()
                    .context("Outstanding evidence is missing")?,
            )?;
        } else {
            line.validate_unassigned(selected)?;
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct UnresolvedItem {
    reason: Reason,
    resolution: &'static str,
    outstanding_item: OutstandingRow,
    candidate_posting_ids: Vec<Uuid>,
    candidate_control_ids: Vec<Uuid>,
}

#[derive(Serialize)]
struct BankTransaction {
    #[serde(flatten)]
    control: PostingLine,
    reasons: BTreeSet<Reason>,
    evidence: Vec<Evidence>,
    booking_lines: Vec<PostingLine>,
}

#[derive(Serialize)]
pub struct BankTransactionsToHandle {
    administration: Administration,
    retrieved_at: String,
    default_currency: String,
    amount_basis: &'static str,
    configuration_source: &'static str,
    unassigned_gl_accounts: Vec<GlAccount>,
    journals: Vec<Journal>,
    transaction_date_from: Option<NaiveDate>,
    transaction_date_to: Option<NaiveDate>,
    source_unassigned_posting_count: usize,
    source_payable_item_count: usize,
    source_receivable_item_count: usize,
    source_bank_outstanding_item_count: usize,
    reconstructed_transaction_count: usize,
    matched_transaction_count: usize,
    offset: u32,
    transactions: Vec<BankTransaction>,
    has_more: bool,
    next_offset: Option<u32>,
    coverage_complete: bool,
    unresolved_items: Vec<UnresolvedItem>,
    interpretation: &'static str,
}

fn number_string<S: serde::Serializer>(
    value: &Option<serde_json::Number>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    value
        .as_ref()
        .map(ToString::to_string)
        .serialize(serializer)
}

fn collection_url(root: &Url, resource: &str, fields: &str, filter: Option<&str>) -> Result<Url> {
    let mut url = root.join(resource)?;
    url.query_pairs_mut().append_pair("$select", fields);
    if let Some(filter) = filter {
        url.query_pairs_mut().append_pair("$filter", filter);
    }
    Ok(url)
}

fn id_filter(field: &str, ids: impl IntoIterator<Item = Uuid>) -> String {
    format!(
        "({})",
        ids.into_iter()
            .map(|id| format!("{field} eq guid'{id}'"))
            .collect::<Vec<_>>()
            .join(" or ")
    )
}

struct BankScope {
    journals: BTreeMap<String, Journal>,
    gl_accounts: BTreeMap<Uuid, GlAccount>,
}

impl BankScope {
    async fn load(
        client: &ExactClient,
        administration: &Administration,
        selected: &BTreeSet<Uuid>,
        journal_code: Option<&str>,
        include_outstanding: bool,
    ) -> Result<Self> {
        administration.validate_bank_scope(client).await?;
        let root = client
            .base_url()
            .join(&format!("/api/v1/{}/", administration.code))?;
        let all_journals: Vec<Journal> = client
            .get_collection(collection_url(
                &root,
                "financial/Journals",
                JOURNAL_FIELDS,
                Some("(Type eq 12 or Type eq 16) and GLAccount ne null"),
            )?)
            .await?;
        let mut journals = BTreeMap::new();
        let mut journal_ids = HashSet::new();
        let mut control_gl_ids = HashSet::new();
        for journal in all_journals {
            journal.validate(administration.code)?;
            ensure!(
                journal_ids.insert(journal.id),
                "Duplicate bank journal UUID; no queue returned"
            );
            control_gl_ids.insert(journal.gl_account_id);
            let code = journal.code.trim().to_owned();
            ensure!(
                journals.insert(code, journal).is_none(),
                "Duplicate bank journal code; no queue returned"
            );
        }
        ensure!(
            selected.iter().all(|id| !control_gl_ids.contains(id)),
            "Selected unassigned G/L is an own-bank control G/L"
        );
        if let Some(code) = journal_code {
            ensure!(
                journals.contains_key(code.trim()),
                "Selected journal is not a verified bank/payment-service journal"
            );
            journals.retain(|key, _| key == code.trim());
        }
        let mut gl_filter = id_filter("ID", selected.iter().copied());
        if include_outstanding {
            gl_filter.push_str(" or Type eq 20 or Type eq 22");
        }
        let accounts: Vec<GlAccount> = client
            .get_collection(collection_url(
                &root,
                "financial/GLAccounts",
                GL_FIELDS,
                Some(&gl_filter),
            )?)
            .await?;
        let mut gl_accounts = BTreeMap::new();
        for account in accounts {
            account.validate_for(administration.code, selected, include_outstanding)?;
            ensure!(
                gl_accounts.insert(account.id, account).is_none(),
                "Duplicate G/L metadata; no queue returned"
            );
        }
        ensure!(
            selected.iter().all(|id| gl_accounts.contains_key(id)),
            "Selected unassigned G/L metadata is missing; no queue returned"
        );
        administration.validate_bank_scope(client).await?;
        Ok(Self {
            journals,
            gl_accounts,
        })
    }
}

pub(crate) async fn validate_configuration(
    client: &ExactClient,
    administration: &Administration,
    unassigned_gl_account_ids: Vec<String>,
) -> Result<BankConfiguration> {
    let configuration = BankConfiguration::from_argument_ids(&unassigned_gl_account_ids)?;
    let selected = configuration.validate()?;
    BankScope::load(client, administration, &selected, None, false).await?;
    Ok(configuration)
}

pub(crate) async fn get_transactions_to_handle(
    client: &ExactClient,
    administration: &Administration,
    args: GetBankTransactionsToHandle,
) -> Result<BankTransactionsToHandle> {
    Ok(read_queue(client, administration, args, None, false)
        .await?
        .queue)
}

struct QueueRead {
    queue: BankTransactionsToHandle,
    invoice_reports: Vec<(crate::financial::Side, OutstandingRow)>,
}

async fn read_queue(
    client: &ExactClient,
    administration: &Administration,
    args: GetBankTransactionsToHandle,
    selected_control: Option<Uuid>,
    retain_invoice_reports: bool,
) -> Result<QueueRead> {
    let explicit_configuration = args.validate()?;
    administration.validate_bank_scope(client).await?;
    let (configuration, configuration_source) = match explicit_configuration {
        Some(configuration) => (configuration, "arguments"),
        None => (
            client.bank_configuration().await?.context(
                "No unassigned bank G/L settings for the selected administration; run `exact-mcp bank-config set --unassigned-gl-account-id <UUID>` or pass unassigned_gl_account_ids",
            )?,
            "saved_administration",
        ),
    };
    let selected = configuration.validate()?;
    let root = client
        .base_url()
        .join(&format!("/api/v1/{}/", administration.code))?;
    let default_currency = invoice::default_currency(client, administration).await?;
    let BankScope {
        journals,
        gl_accounts,
    } = BankScope::load(
        client,
        administration,
        &selected,
        args.journal_code.as_deref(),
        true,
    )
    .await?;
    let mut evidence = Vec::new();
    let mut unassigned_ids = HashSet::new();
    if !journals.is_empty() {
        let journal_filter = journals
            .keys()
            .map(|code| format!("trim(JournalCode) eq '{}'", code.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(" or ");
        let filter = format!(
            "Type eq 40 and {} and ({journal_filter})",
            id_filter("GLAccount", selected.iter().copied())
        );
        client
            .try_for_each_page(
                collection_url(
                    &root,
                    "financialtransaction/TransactionLines",
                    LINE_FIELDS,
                    Some(&filter),
                )?,
                |page: Vec<PostingLine>| {
                    for line in page {
                        line.validate_for(administration.code, &journals)?;
                        line.validate_unassigned(&selected)?;
                        ensure!(
                            unassigned_ids.insert(line.id),
                            "Duplicate unassigned posting; no queue returned"
                        );
                        evidence.push(Evidence {
                            reason: Reason::Unassigned,
                            posting: line,
                            outstanding_item: None,
                        });
                    }
                    Ok(())
                },
            )
            .await?;
    }
    let mut unresolved_items = Vec::new();
    let mut invoice_reports = Vec::new();
    let mut source_counts = [0; 2];
    let mut source_bank_outstanding_item_count = 0;
    for (index, (resource, reason)) in [
        ("read/financial/PayablesList", Reason::OutstandingPayable),
        (
            "read/financial/ReceivablesList",
            Reason::OutstandingReceivable,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let mut reports = Vec::new();
        let mut hids = HashSet::new();
        client
            .try_for_each_page(
                collection_url(&root, resource, REPORT_FIELDS, None)?,
                |page: Vec<OutstandingRow>| {
                    for report in page {
                        report.validate()?;
                        ensure!(
                            hids.insert(report.hid.clone()),
                            "Duplicate outstanding report HID; no queue returned"
                        );
                        source_counts[index] += 1;
                        if journals.contains_key(report.journal_code.trim()) {
                            report.validate_bank()?;
                            reports.push(report);
                        } else if retain_invoice_reports {
                            invoice_reports.push((
                                if index == 0 {
                                    crate::financial::Side::Payable
                                } else {
                                    crate::financial::Side::Receivable
                                },
                                report,
                            ));
                        }
                    }
                    Ok(())
                },
            )
            .await?;
        reports.sort_by(|a, b| a.hid.cmp(&b.hid));
        source_bank_outstanding_item_count += reports.len();
        for report in reports {
            let filter = format!(
                "Type eq 40 and Account eq guid'{}' and EntryNumber eq {} and trim(JournalCode) eq '{}'",
                report.account_id,
                report.entry_number.context("Missing bank entry number")?,
                report.journal_code.trim().replace('\'', "''")
            );
            let lines: Vec<PostingLine> = client
                .get_collection(collection_url(
                    &root,
                    "financialtransaction/TransactionLines",
                    LINE_FIELDS,
                    Some(&filter),
                )?)
                .await?;
            let mut candidates = Vec::new();
            let mut ids = HashSet::new();
            for line in lines {
                line.validate_for(administration.code, &journals)?;
                line.validate_report(&report)?;
                ensure!(
                    ids.insert(line.id),
                    "Duplicate outstanding counterpart posting; no queue returned"
                );
                if gl_accounts
                    .get(&line.gl_account_id)
                    .is_some_and(|gl| gl.type_code == reason.gl_type())
                {
                    candidates.push(line);
                }
            }
            candidates.sort_by_key(|line| line.id);
            if candidates.len() == 1 && candidates[0].offset_line_id.is_some() {
                evidence.push(Evidence {
                    reason,
                    posting: candidates.remove(0),
                    outstanding_item: Some(report),
                });
            } else {
                unresolved_items.push(UnresolvedItem {
                    reason,
                    resolution: match candidates.len() {
                        0 => "missing_counterpart",
                        1 => "missing_offset",
                        _ => "ambiguous_counterparts",
                    },
                    outstanding_item: report,
                    candidate_posting_ids: candidates.iter().map(|line| line.id).collect(),
                    candidate_control_ids: candidates
                        .iter()
                        .filter_map(|line| line.offset_line_id)
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect(),
                });
            }
        }
    }
    let control_ids: Vec<_> = evidence
        .iter()
        .filter_map(|item| item.posting.offset_line_id)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut controls = BTreeMap::new();
    for batch in control_ids.chunks(BATCH_SIZE) {
        let requested: HashSet<_> = batch.iter().copied().collect();
        let lines: Vec<PostingLine> = client
            .get_collection(collection_url(
                &root,
                "financialtransaction/TransactionLines",
                LINE_FIELDS,
                Some(&id_filter("ID", batch.iter().copied())),
            )?)
            .await?;
        let mut ids = HashSet::new();
        for line in lines {
            line.validate_for(administration.code, &journals)?;
            line.validate_control(&journals)?;
            ensure!(
                requested.contains(&line.id),
                "Control lookup returned an unrequested UUID; no queue returned"
            );
            ensure!(
                ids.insert(line.id),
                "Duplicate bank control UUID; no queue returned"
            );
            controls.insert(line.id, line);
        }
        ensure!(
            ids == requested,
            "Explicit bank control is missing; no queue returned"
        );
    }
    let entry_ids: Vec<_> = controls
        .values()
        .map(|line| line.entry_id)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut entry_lines = BTreeMap::new();
    let mut entry_line_ids = HashSet::new();
    for batch in entry_ids.chunks(BATCH_SIZE) {
        let requested: HashSet<_> = batch.iter().copied().collect();
        client
            .try_for_each_page(
                collection_url(
                    &root,
                    "financialtransaction/TransactionLines",
                    LINE_FIELDS,
                    Some(&id_filter("EntryID", batch.iter().copied())),
                )?,
                |page: Vec<PostingLine>| {
                    for line in page {
                        line.validate_for(administration.code, &journals)?;
                        ensure!(
                            requested.contains(&line.entry_id),
                            "Whole-entry lookup returned an unrequested entry; no queue returned"
                        );
                        ensure!(
                            entry_line_ids.insert(line.id),
                            "Duplicate whole-entry posting; no queue returned"
                        );
                        let control = controls
                            .values()
                            .find(|control| control.entry_id == line.entry_id)
                            .context("Whole-entry scope has no selected control")?;
                        line.validate_same_entry(control)?;
                        for item in &evidence {
                            item.validate_unique_counterpart(&line, &gl_accounts)?;
                        }
                        if controls.contains_key(&line.id)
                            || line
                                .offset_line_id
                                .is_some_and(|id| controls.contains_key(&id))
                        {
                            entry_lines.insert(line.id, line);
                        }
                    }
                    Ok(())
                },
            )
            .await?;
    }
    for control in controls.values_mut() {
        let current = entry_lines
            .remove(&control.id)
            .context("Bank control disappeared from its entry; no queue returned")?;
        current.validate_against(control)?;
        *control = current;
    }
    for item in &evidence {
        let current = entry_lines
            .get(&item.posting.id)
            .context("Discovered evidence posting disappeared; no queue returned")?;
        let control = controls
            .get(
                &item
                    .posting
                    .offset_line_id
                    .context("Evidence offset missing")?,
            )
            .context("Evidence control missing")?;
        item.validate_for(current, control, &selected, &gl_accounts)?;
    }
    let mut bookings: BTreeMap<Uuid, Vec<PostingLine>> = BTreeMap::new();
    for line in entry_lines.into_values() {
        if let Some(control) = line.offset_line_id.and_then(|id| controls.get(&id)) {
            line.validate_same_entry(control)?;
            bookings.entry(control.id).or_default().push(line);
        }
    }
    let mut evidence_by_control: BTreeMap<Uuid, Vec<Evidence>> = BTreeMap::new();
    for item in evidence {
        evidence_by_control
            .entry(
                item.posting
                    .offset_line_id
                    .context("Evidence offset missing")?,
            )
            .or_default()
            .push(item);
    }
    let reconstructed_transaction_count = controls.len();
    let mut transactions = Vec::new();
    for (id, control) in controls {
        let mut booking_lines = bookings.remove(&id).unwrap_or_default();
        booking_lines.sort_by_key(|line| (line.line_number, line.id));
        let mut evidence = evidence_by_control.remove(&id).unwrap_or_default();
        evidence.sort_by_key(|item| (item.reason, item.posting.id));
        if args.matches_date(control.posting_date.as_ref())
            && selected_control.is_none_or(|selected| selected == id)
        {
            transactions.push(BankTransaction {
                control,
                reasons: evidence.iter().map(|item| item.reason).collect(),
                evidence,
                booking_lines,
            });
        }
    }
    transactions.sort_by(|a, b| {
        let left = a.control.posting_date.as_ref().map(|date| date.0);
        let right = b.control.posting_date.as_ref().map(|date| date.0);
        left.is_none()
            .cmp(&right.is_none())
            .then_with(|| right.cmp(&left))
            .then_with(|| {
                a.control
                    .journal_code
                    .trim()
                    .cmp(b.control.journal_code.trim())
            })
            .then_with(|| a.control.entry_number.cmp(&b.control.entry_number))
            .then_with(|| a.control.id.cmp(&b.control.id))
    });
    let matched_transaction_count = transactions.len();
    let next = args
        .offset
        .checked_add(args.limit.unwrap_or(50))
        .context("Bank offset is too large")?;
    let has_more = matched_transaction_count > next as usize;
    let transactions = transactions
        .into_iter()
        .skip(args.offset as usize)
        .take(args.limit.unwrap_or(50) as usize)
        .collect();
    administration.validate_bank_scope(client).await?;
    let queue = BankTransactionsToHandle {
        administration: administration.clone(),
        retrieved_at: Utc::now().to_rfc3339(),
        default_currency,
        amount_basis: "signed_bank_control_postings_not_settlement_or_outstanding_amounts",
        configuration_source,
        unassigned_gl_accounts: gl_accounts
            .into_values()
            .filter(|gl| selected.contains(&gl.id))
            .collect(),
        journals: journals.into_values().collect(),
        transaction_date_from: args.transaction_date_from,
        transaction_date_to: args.transaction_date_to,
        source_unassigned_posting_count: unassigned_ids.len(),
        source_payable_item_count: source_counts[0],
        source_receivable_item_count: source_counts[1],
        source_bank_outstanding_item_count,
        reconstructed_transaction_count,
        matched_transaction_count,
        offset: args.offset,
        transactions,
        has_more,
        next_offset: has_more.then_some(next),
        coverage_complete: unresolved_items.is_empty(),
        unresolved_items,
        interpretation: "API-only accounting reconstruction of Af te handelen, dependent on the explicitly configured unassigned G/L roles and current bank/payment-service journal control G/L metadata. NOT a provider completion-state flag, generic undocumented Overige workflow coverage, or proof of payment execution, clearing or settlement. coverage_complete only means all selected bank outstanding report rows resolved uniquely under these configured roles, not guaranteed UI completeness. Status 20/50 is source accounting metadata and is NEVER a queue predicate. Unassigned postings and current open AP/AR report membership discover counterpart postings; only explicit counterpart.OffsetID -> own-bank control.ID links select controls. Missing/multiple AP/AR candidates remain unresolved regardless of amounts, dates or reused references/entry numbers. ALL unresolved items are returned even when transactions are paginated or date-filtered; coverage is incomplete when any remain. Report amounts preserve current partial/open values and report currency; they are not control posting amounts. Default-currency line amounts use default_currency; transaction-currency line amounts use each line's currency (null means unknown). Nulls and signs are preserved; no currency conversion, totals or paid-direction inference. Booking lines are only explicit same-entry reverse-offset counterparts, not all movements sharing an entry. Posting date bounds are inclusive accounting dates, not due/matching/value/settlement dates; unknown dates are excluded only with bounds. Sorted posting date descending, unknown last, then journal, entry number and control UUID. All source pages are read before local date filtering/pagination; reads are not a frozen snapshot and later offsets reread sources. Source associations are revalidated before returning; errors never yield partial results. No bank account number fields are requested; descriptions/references and other source text remain potentially sensitive untrusted data. All remote requests are GET; no financial writes, checkpoints, cache, browser or payment actions.",
    };
    Ok(QueueRead {
        queue,
        invoice_reports,
    })
}

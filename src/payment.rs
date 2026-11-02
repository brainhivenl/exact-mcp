use anyhow::{Context, Result, ensure};
use chrono::Utc;
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::exact::ExactClient;
use crate::financial::{ExactDate, Money};
use crate::invoice;
use crate::store::Administration;

const LINE_FIELDS: &str = "ID,EntryID,Division,Account,AccountCode,AccountName,EntryNumber,FinancialYear,FinancialPeriod,JournalCode,JournalDescription,GLAccount,GLAccountCode,GLAccountDescription,LineNumber,LineType,Type,Status,Date,AmountDC,AmountFC,Currency,InvoiceNumber,YourRef,Description,OffsetID";
const JOURNAL_FIELDS: &str = "ID,Division,Code,Description,Type,Currency,AllowVariableCurrency,GLAccount,GLAccountCode,GLAccountDescription,BankAccountDescription,PaymentServiceProviderName";

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetPaymentDetails {
    #[schemars(
        description = "Verified Exact company UUID, the same account_id used for get_invoice_matches. Never guess between company matches."
    )]
    pub account_id: String,
    #[schemars(
        description = "Explicitly selected cashflow candidate's id UUID from get_invoice_matches ledger_evidence[].candidates[]. This is a transaction LINE ID, not entry_id or a cashflow payment-term ID. Do not silently choose between ambiguous allocation candidates."
    )]
    pub transaction_line_id: String,
}

#[derive(Serialize)]
pub struct PaymentDetailsResult {
    administration: Administration,
    retrieved_at: String,
    account_id: Uuid,
    default_currency: String,
    amount_basis: &'static str,
    posting: PostingLine,
    journal: Journal,
    journal_kind: &'static str,
    offset_resolution: &'static str,
    offset_line: Option<PostingLine>,
    interpretation: &'static str,
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
    gl_account_id: Option<Uuid>,
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
    posting_amount_default_currency: Option<Money>,
    #[serde(rename(deserialize = "AmountFC"))]
    posting_amount_transaction_currency: Option<Money>,
    currency: Option<String>,
    invoice_number: Option<i64>,
    your_ref: Option<String>,
    description: Option<String>,
    #[serde(rename(deserialize = "OffsetID"))]
    offset_line_id: Option<Uuid>,
}

impl PostingLine {
    fn validate(&self, division: u32) -> Result<()> {
        ensure!(
            self.division == division
                && !self.id.is_nil()
                && !self.entry_id.is_nil()
                && (1..=i64::from(i32::MAX)).contains(&self.entry_number)
                && (1..=9999).contains(&self.financial_year)
                && (0..=i32::from(i16::MAX)).contains(&self.financial_period)
                && !self.journal_code.trim().is_empty()
                && self.journal_code.trim().chars().count() <= 100
                && self
                    .offset_line_id
                    .is_none_or(|id| !id.is_nil() && id != self.id),
            "Invalid or wrong-scope cashflow posting; no payment details returned"
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
    description: Option<String>,
    #[serde(rename(deserialize = "Type"))]
    type_code: i32,
    currency: Option<String>,
    allow_variable_currency: Option<bool>,
    #[serde(rename(deserialize = "GLAccount"))]
    gl_account_id: Option<Uuid>,
    #[serde(rename(deserialize = "GLAccountCode"))]
    gl_account_code: Option<String>,
    #[serde(rename(deserialize = "GLAccountDescription"))]
    gl_account_description: Option<String>,
    bank_account_description: Option<String>,
    payment_service_provider_name: Option<String>,
}

pub(crate) async fn get_details(
    client: &ExactClient,
    administration: &Administration,
    args: GetPaymentDetails,
) -> Result<PaymentDetailsResult> {
    let account_id =
        Uuid::parse_str(&args.account_id).context("account_id must be an Exact account UUID")?;
    let line_id = Uuid::parse_str(&args.transaction_line_id)
        .context("transaction_line_id must be an Exact transaction line UUID")?;
    ensure!(
        !account_id.is_nil() && !line_id.is_nil(),
        "Company and transaction line UUIDs must not be nil"
    );
    let root = client
        .base_url()
        .join(&format!("/api/v1/{}/", administration.code))?;
    let mut url = root.join("financialtransaction/TransactionLines")?;
    url.query_pairs_mut()
        .append_pair("$select", LINE_FIELDS)
        .append_pair(
            "$filter",
            &format!("ID eq guid'{line_id}' and Account eq guid'{account_id}'"),
        );
    let mut lines: Vec<PostingLine> = client
        .get_collection(url)
        .await
        .context("Cannot read cashflow posting; check Financial accounting permission")?;
    ensure!(
        lines.len() == 1,
        "Cashflow posting not found uniquely for this company and line UUID"
    );
    let posting = lines.remove(0);
    posting.validate(administration.code)?;
    ensure!(
        posting.id == line_id && posting.account_id == Some(account_id),
        "A different cashflow posting or company was returned; no details returned"
    );
    ensure!(
        posting.type_code == 40,
        "Selected line is not a source type-40 cashflow posting; use invoice accounting details for invoice/credit entries"
    );
    let default_currency = invoice::default_currency(client, administration).await?;
    let mut journal_url = root.join("financial/Journals")?;
    journal_url
        .query_pairs_mut()
        .append_pair("$select", JOURNAL_FIELDS)
        .append_pair(
            "$filter",
            &format!(
                "trim(Code) eq '{}'",
                posting.journal_code.trim().replace('\'', "''")
            ),
        );
    let mut journals: Vec<Journal> = client
        .get_collection(journal_url)
        .await
        .context("Cannot read cashflow journal; check Financial generalledgers permission")?;
    ensure!(
        journals.len() == 1,
        "Cashflow journal not found uniquely; no payment details returned"
    );
    let journal = journals.remove(0);
    ensure!(
        !journal.id.is_nil()
            && journal.division == administration.code
            && journal.code.trim() == posting.journal_code.trim(),
        "A different cashflow journal or administration was returned; no details returned"
    );
    let journal_kind = match journal.type_code {
        10 => "cash",
        12 => "bank",
        16 => "payment_service",
        _ => "other",
    };
    let (offset_resolution, offset_line) = if let Some(offset_id) = posting.offset_line_id {
        let mut offset_url = root.join("financialtransaction/TransactionLines")?;
        offset_url
            .query_pairs_mut()
            .append_pair("$select", LINE_FIELDS)
            .append_pair(
                "$filter",
                &format!(
                    "ID eq guid'{offset_id}' and EntryID eq guid'{}'",
                    posting.entry_id
                ),
            );
        let mut offsets: Vec<PostingLine> = client.get_collection(offset_url).await?;
        ensure!(
            offsets.len() <= 1,
            "Duplicate cashflow offset references; no details returned"
        );
        if let Some(offset) = offsets.pop() {
            offset.validate(administration.code)?;
            ensure!(
                offset.id == offset_id
                    && offset.entry_id == posting.entry_id
                    && offset.entry_number == posting.entry_number
                    && offset.financial_year == posting.financial_year
                    && offset.financial_period == posting.financial_period
                    && offset.journal_code.trim() == posting.journal_code.trim(),
                "Offset reference crosses the selected cashflow entry; no details returned"
            );
            ("resolved_same_entry_reference", Some(offset))
        } else {
            ("not_found", None)
        }
    } else {
        ("not_reported", None)
    };
    client.session().await?;
    Ok(PaymentDetailsResult {
        administration: administration.clone(),
        retrieved_at: Utc::now().to_rfc3339(),
        account_id,
        default_currency,
        amount_basis: "cashflow_ledger_posting_not_invoice_allocation",
        posting,
        journal,
        journal_kind,
        offset_resolution,
        offset_line,
        interpretation: "Read-only accounting details for the explicitly selected company cashflow transaction LINE UUID (source Type 40). This is a posting, not a cashflow payment-term record. Journal kind comes only from the journal's source Type: 10 cash, 12 bank, 16 payment service; other codes stay other. Journal labels/default currency are current master data, not historical bank-account ownership or payment-instruction proof. posting_amount_default_currency uses default_currency; posting_amount_transaction_currency uses the line's currency (null/blank means unknown), not necessarily the journal currency. Journal currency is its current default transaction currency; AllowVariableCurrency permits other posting currencies. Amounts and signs are preserved; no conversion, netting or incoming/outgoing direction is inferred. These are full posting amounts, NOT the allocated amount for a particular invoice. Use get_invoice_matches for recorded allocation groups and keep multi-member group/ledger-candidate ambiguity explicit. This tool does not revalidate any invoice's matching membership or decide which ambiguous candidate belongs to an allocation. posting_date is an accounting date, NOT matching, bank value, clearing or settlement date. Status/type/journal metadata do not prove payment execution, bank settlement or an outstanding balance. InvoiceNumber/YourRef are source references, not independently proven invoice allocations. OffsetID is followed only as an explicit provider-reported reference within this same entry; it is NOT an invoice allocation link. Missing/null offset references do not imply nonpayment. The complete bank/cash entry is deliberately not fetched: only the selected company line, its journal and at most one explicit same-entry offset line are read. The offset line can have a different/null account and is not a newly inferred company relationship. No bank account numbers/IBAN fields are requested; descriptions/references can still contain sensitive, untrusted text. Source reads are not transactionally frozen. No financial writes, payment execution, invoice generation or downloads were performed.",
    })
}

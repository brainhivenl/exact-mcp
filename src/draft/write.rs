use super::*;
use rust_decimal::Decimal;
use serde_json::{Value, json};

use crate::account_context;
use crate::ledger::{self, GlAccount};
use crate::master_data::{self, Journal, Record, VatCode};

#[cfg(test)]
mod tests;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SalesLine {
    #[schemars(
        description = "Verified sales article/service UUID. Exact requires an article even for a custom line: explicitly select a generic service article, never invent one."
    )]
    pub item_id: String,
    #[schemars(
        description = "Verified revenue G/L UUID, explicitly selected using item/group context or search_gl_accounts. No inferred default."
    )]
    pub gl_account_id: String,
    pub description: String,
    #[schemars(
        description = "Positive exact JSON decimal string. Fractional quantities require an article allowing fractions."
    )]
    pub quantity: String,
    #[schemars(
        description = "Verified standard unit code of this article from search_items/get_item_details. Unit conversions are not supported."
    )]
    pub unit_code: String,
    #[schemars(
        description = "Explicit nonnegative exact decimal unit price in invoice currency, interpreted by the selected VAT code. Never silently use StandardSalesPrice: its currency is unknown."
    )]
    pub unit_price: String,
    #[schemars(
        description = "Explicit verified sales-applicable VAT code. Exact calculates dated VAT and totals; no tax eligibility is inferred."
    )]
    pub vat_code: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateDraftInvoice {
    #[schemars(
        description = "New caller-chosen UUID used as the commercial InvoiceID and repeat-request identity. Reuse this same UUID after an uncertain outcome; never choose a fresh UUID to retry. Not an existing accounting EntryID."
    )]
    pub request_id: String,
    #[schemars(
        description = "Verified bill-to company UUID from search_accounts; resolve ambiguity first."
    )]
    pub account_id: String,
    #[schemars(
        description = "Verified ordering company UUID. Explicitly use the same UUID as account_id when the client is also the bill-to company."
    )]
    pub ordered_by_id: String,
    pub journal_code: String,
    #[schemars(with = "String")]
    pub invoice_date: NaiveDate,
    pub currency: String,
    pub description: Option<String>,
    pub your_ref: Option<String>,
    pub remarks: Option<String>,
    pub payment_condition_code: Option<String>,
    #[schemars(with = "Option<String>")]
    pub due_date: Option<NaiveDate>,
    pub lines: Vec<SalesLine>,
    #[serde(default)]
    #[schemars(
        description = "true validates and previews without writing; false (default) creates the requested editable sales draft directly. No separate approval token is required."
    )]
    pub dry_run: bool,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateDraftInvoice {
    pub account_id: String,
    #[schemars(
        description = "Selected commercial InvoiceID from search_draft_invoices/get_draft_invoice_details, never an accounting EntryID."
    )]
    pub invoice_id: String,
    #[schemars(
        description = "Exact raw modified string returned by fresh draft details. Stale edits are rejected; this is an observed concurrency check, not an atomic lock."
    )]
    pub expected_modified: String,
    #[schemars(with = "Option<String>")]
    pub invoice_date: Option<NaiveDate>,
    #[schemars(with = "Option<String>")]
    pub due_date: Option<NaiveDate>,
    pub description: Option<String>,
    pub your_ref: Option<String>,
    pub remarks: Option<String>,
    pub payment_condition_code: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateDraftInvoiceLine {
    pub account_id: String,
    pub invoice_id: String,
    pub expected_modified: String,
    #[schemars(
        description = "Explicit existing line UUID from this invoice's get_draft_invoice_details. Other lines are not submitted or replaced."
    )]
    pub line_id: String,
    #[schemars(
        description = "Explicit replacement of this line's article, revenue G/L, description, quantity, unit price and VAT code. Discount is explicitly reset to zero; unrelated fields are omitted."
    )]
    pub line: SalesLine,
    #[serde(default)]
    pub dry_run: bool,
}

fn text(value: &str, maximum: usize, empty: bool) -> Result<()> {
    ensure!(
        (empty || !value.trim().is_empty()) && value.chars().count() <= maximum,
        "Text must contain {}–{maximum} characters",
        if empty { 0 } else { 1 }
    );
    Ok(())
}

fn date(value: NaiveDate) -> Result<()> {
    use chrono::Datelike;
    ensure!(
        (1..=9999).contains(&value.year()),
        "Date year must be 1–9999"
    );
    Ok(())
}

fn decimal(value: &str) -> Result<(Decimal, serde_json::Number)> {
    ensure!(value.len() <= 100, "Decimal is too long");
    let number: serde_json::Number = serde_json::from_str(value)
        .context("Use an exact JSON decimal string, not a floating-point argument")?;
    let value = Decimal::from_str_exact(value).context("Decimal is out of range or not exact")?;
    Ok((value, number))
}

impl SalesLine {
    fn payload(&self) -> Result<Value> {
        let item = parse_id(&self.item_id, "item_id")?;
        let gl = parse_id(&self.gl_account_id, "gl_account_id")?;
        text(&self.description, 250, false)?;
        text(&self.vat_code, 3, false)?;
        text(&self.unit_code, 8, false)?;
        let (quantity, quantity_number) = decimal(&self.quantity)?;
        let (price, price_number) = decimal(&self.unit_price)?;
        ensure!(quantity > Decimal::ZERO, "Quantity must be positive");
        ensure!(price >= Decimal::ZERO, "Unit price must not be negative");
        quantity
            .checked_mul(price)
            .context("Line amount overflows")?;
        Ok(json!({
            "Item": item, "GLAccount": gl, "Description": self.description,
            "Quantity": quantity_number, "UnitPrice": price_number,
            "VATCode": self.vat_code.trim(), "UnitCode": self.unit_code.trim(), "Discount": 0
        }))
    }
}

fn optional_text(
    payload: &mut Value,
    key: &str,
    value: &Option<String>,
    maximum: usize,
) -> Result<()> {
    if let Some(value) = value {
        text(value, maximum, true)?;
        payload[key] = json!(value);
    }
    Ok(())
}

async fn customer(
    client: &ExactClient,
    administration: &Administration,
    id: Uuid,
    on: NaiveDate,
) -> Result<()> {
    let account = account_context::load_account(client, administration, id).await?;
    ensure!(
        account.is_blocked == Some(false),
        "Customer is blocked or block state is unknown"
    );
    ensure!(
        account.sales_allowed == Some(true),
        "Customer is not explicitly sales-enabled"
    );
    ensure!(
        account.start_date.as_ref().is_none_or(|d| d.0 <= on)
            && account.end_date.as_ref().is_none_or(|d| d.0 > on),
        "Customer is inactive on the invoice date"
    );
    Ok(())
}

async fn payment_condition(
    client: &ExactClient,
    administration: &Administration,
    value: &Option<String>,
) -> Result<()> {
    if let Some(code) = value {
        text(code, 3, false)?;
        let rows: Vec<Record<master_data::PaymentCondition>> = master_data::load_catalog(
            client,
            administration,
            "cashflow/PaymentConditions",
            "PaymentDays",
            "Financial cashflow",
        )
        .await?;
        ensure!(
            rows.iter().filter(|r| r.code.trim() == code.trim()).count() == 1,
            "Payment condition not found uniquely"
        );
    }
    Ok(())
}

async fn journal(
    client: &ExactClient,
    administration: &Administration,
    code: &str,
    currency: &str,
) -> Result<()> {
    text(code, 3, false)?;
    let rows: Vec<Record<Journal>> = master_data::load_catalog(
        client,
        administration,
        "financial/Journals",
        "Type,IsBlocked,Currency,AllowVariableCurrency",
        "Financial generalledgers",
    )
    .await?;
    let matches: Vec<_> = rows
        .iter()
        .filter(|r| r.code.trim() == code.trim())
        .collect();
    ensure!(matches.len() == 1, "Sales journal not found uniquely");
    let journal = &matches[0].details;
    ensure!(
        journal.type_code == Some(20) && journal.is_blocked == Some(false),
        "Journal must be an explicitly unblocked sales journal (type 20)"
    );
    ensure!(
        journal.currency.as_deref().map(str::trim) == Some(currency)
            || journal.allow_variable_currency == Some(true),
        "Journal does not explicitly allow this currency"
    );
    Ok(())
}

async fn validate_lines(
    client: &ExactClient,
    administration: &Administration,
    lines: &[SalesLine],
    on: NaiveDate,
) -> Result<()> {
    let vats = master_data::load_vat_codes(client, administration).await?;
    let mut url = client.base_url().join(&format!(
        "/api/v1/{}/financial/GLAccounts",
        administration.code
    ))?;
    url.query_pairs_mut()
        .append_pair("$select", ledger::ACCOUNT_FIELDS);
    let gls: Vec<GlAccount> = client.get_collection(url).await?;
    let mut ids = HashSet::new();
    for gl in &gls {
        gl.validate(administration.code)?;
        ensure!(ids.insert(gl.id), "Duplicate G/L account UUID");
    }
    for line in lines {
        let item =
            master_data::load_item(client, administration, parse_id(&line.item_id, "item_id")?)
                .await?;
        ensure!(
            item.details.is_sales_item == Some(true),
            "Article is not explicitly sales-enabled"
        );
        ensure!(
            item.details.start_date.as_ref().is_none_or(|d| d.0 <= on)
                && item.details.end_date.as_ref().is_none_or(|d| d.0 > on),
            "Article is inactive on the invoice date"
        );
        ensure!(
            item.details.unit_code.as_deref().map(str::trim) == Some(line.unit_code.trim()),
            "Use the article's verified standard unit; conversions are not supported"
        );
        let (quantity, _) = decimal(&line.quantity)?;
        ensure!(
            quantity.fract().is_zero() || item.details.is_fraction_allowed_item == Some(true),
            "Article does not explicitly permit fractional quantities"
        );
        let gl_id = parse_id(&line.gl_account_id, "gl_account_id")?;
        let gl = gls
            .iter()
            .find(|gl| gl.id == gl_id)
            .context("Revenue G/L not found")?;
        ensure!(
            gl.is_blocked == Some(false),
            "Revenue G/L is blocked or block state is unknown"
        );
        ensure!(
            gl.use_costcenter == Some(0) && gl.use_costunit == Some(0),
            "G/L dimensions are not explicitly optional; dimensioned sales lines are not supported"
        );
        ensure!(
            gl.type_code.is_some()
                && !matches!(
                    gl.type_code,
                    Some(10 | 12 | 14 | 16 | 20 | 21 | 22 | 24 | 25 | 100)
                ),
            "Unknown-type or control G/L accounts cannot be used as sales revenue accounts"
        );
        let matched: Vec<&Record<VatCode>> = vats
            .iter()
            .filter(|v| v.code.trim() == line.vat_code.trim())
            .collect();
        ensure!(matched.len() == 1, "VAT code not found uniquely");
        let vat = &matched[0].details;
        ensure!(
            vat.is_blocked == Some(false)
                && matches!(vat.transaction_type.as_deref(), Some("S" | "B")),
            "VAT code must be unblocked and explicitly sales-applicable"
        );
    }
    Ok(())
}

async fn read(
    client: &ExactClient,
    administration: &Administration,
    account: &str,
    invoice: &str,
) -> Result<DraftInvoiceDetailsResult> {
    get_details(
        client,
        administration,
        GetDraftInvoiceDetails {
            account_id: account.into(),
            invoice_id: invoice.into(),
        },
    )
    .await
}

fn editable(details: &DraftInvoiceDetailsResult, expected: &str) -> Result<()> {
    ensure!(
        details.invoice.status_code == Some(20) && details.invoice.type_code == Some(8020),
        "Only normal open/editable sales invoices (status 20, type 8020) can be changed"
    );
    ensure!(
        !expected.is_empty() && details.invoice.modified.as_deref() == Some(expected),
        "Invoice changed or Modified is unavailable; reload details before editing"
    );
    Ok(())
}

fn verify_header(invoice: &DraftInvoice, payload: &Value) -> Result<()> {
    let checks = [
        ("InvoiceTo", Some(invoice.invoice_to.to_string())),
        ("OrderedBy", invoice.ordered_by.map(|id| id.to_string())),
        ("Journal", invoice.journal_code.clone()),
        ("Currency", invoice.currency.clone()),
        (
            "InvoiceDate",
            invoice.invoice_date.as_ref().map(|d| d.0.to_string()),
        ),
        (
            "DueDate",
            invoice.due_date.as_ref().map(|d| d.0.to_string()),
        ),
        ("Description", invoice.description.clone()),
        ("YourRef", invoice.your_ref.clone()),
        ("Remarks", invoice.remarks.clone()),
        ("PaymentCondition", invoice.payment_condition.clone()),
    ];
    for (key, actual) in checks {
        if let Some(expected) = payload.get(key) {
            let cleared = matches!(key, "Description" | "YourRef" | "Remarks")
                && expected.as_str() == Some("")
                && actual.is_none();
            ensure!(
                cleared || actual.as_deref() == expected.as_str(),
                "Saved invoice field {key} differs from the request"
            );
        }
    }
    ensure!(
        invoice.status_code == Some(20) && invoice.type_code == Some(8020),
        "Saved invoice is not an editable normal sales invoice"
    );
    Ok(())
}

fn verify_preserved_header(
    before: &DraftInvoice,
    after: &DraftInvoice,
    changes: &Value,
) -> Result<()> {
    let before = serde_json::to_value(before)?;
    let after = serde_json::to_value(after)?;
    for (field, key) in [
        ("invoice_to", "InvoiceTo"),
        ("ordered_by", "OrderedBy"),
        ("journal_code", "Journal"),
        ("currency", "Currency"),
        ("invoice_date", "InvoiceDate"),
        ("due_date", "DueDate"),
        ("description", "Description"),
        ("your_ref", "YourRef"),
        ("remarks", "Remarks"),
        ("payment_condition", "PaymentCondition"),
    ] {
        if changes.get(key).is_some()
            || (key == "DueDate"
                && (changes.get("InvoiceDate").is_some()
                    || changes.get("PaymentCondition").is_some()))
        {
            continue;
        }
        ensure!(
            before[field] == after[field],
            "Unselected invoice header field {key} changed during edit"
        );
    }
    Ok(())
}

fn verify_line(line: &DraftLine, payload: &Value) -> Result<()> {
    ensure!(
        line.item_id.map(|id| id.to_string()).as_deref() == payload["Item"].as_str()
            && line.gl_account_id.map(|id| id.to_string()).as_deref()
                == payload["GLAccount"].as_str()
            && line.description.as_deref() == payload["Description"].as_str()
            && line.vat_code.as_deref().map(str::trim) == payload["VATCode"].as_str()
            && line.unit_code.as_deref().map(str::trim) == payload["UnitCode"].as_str(),
        "Saved line identity, description or VAT differs from request"
    );
    for (key, actual) in [
        ("Quantity", &line.quantity),
        ("UnitPrice", &line.unit_price),
        ("Discount", &line.discount),
    ] {
        let actual = actual
            .as_ref()
            .context("Saved line is missing numeric evidence")?;
        let (actual, _) = decimal(&actual.to_string())?;
        let (expected, _) = decimal(&payload[key].to_string())?;
        ensure!(
            actual == expected,
            "Saved line field {key} differs from request"
        );
    }
    Ok(())
}

fn outcome(invoice_id: Uuid, error: anyhow::Error) -> Value {
    json!({"status": "outcome_unknown", "invoice_id": invoice_id,
        "write_attempted": true, "verified": false, "retry_automatically": false,
        "error": format!("{error:#}"),
        "next_action": "Inspect this exact InvoiceID with draft details/search before any further write. Do not create a new UUID to retry or automatically roll back."})
}

pub async fn create(
    client: &ExactClient,
    administration: &Administration,
    args: CreateDraftInvoice,
) -> Result<Value> {
    let id = parse_id(&args.request_id, "request_id")?;
    let account = parse_id(&args.account_id, "account_id")?;
    let ordered_by = parse_id(&args.ordered_by_id, "ordered_by_id")?;
    date(args.invoice_date)?;
    if let Some(due) = args.due_date {
        date(due)?;
        ensure!(
            due >= args.invoice_date,
            "Due date must not precede invoice date"
        );
    }
    ensure!(
        args.currency.len() == 3 && args.currency.bytes().all(|b| b.is_ascii_uppercase()),
        "Currency must be three uppercase ASCII letters"
    );
    ensure!(
        (1..=100).contains(&args.lines.len()),
        "Supply 1–100 invoice lines"
    );
    let lines: Vec<_> = args
        .lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let mut payload = line.payload()?;
            payload["LineNumber"] = json!(index + 1);
            Ok(payload)
        })
        .collect::<Result<_>>()?;
    let mut payload = json!({"InvoiceID": id, "InvoiceTo": account, "OrderedBy": ordered_by,
        "Journal": args.journal_code.trim(), "Currency": args.currency, "InvoiceDate": args.invoice_date,
        "Status": 20, "Type": 8020, "SalesInvoiceLines": lines});
    optional_text(&mut payload, "Description", &args.description, 250)?;
    optional_text(&mut payload, "YourRef", &args.your_ref, 100)?;
    optional_text(&mut payload, "Remarks", &args.remarks, 2000)?;
    if let Some(due) = args.due_date {
        payload["DueDate"] = json!(due);
    }
    if let Some(code) = &args.payment_condition_code {
        payload["PaymentCondition"] = json!(code.trim());
    }
    let mut url = client.base_url().join(&format!(
        "/api/v1/{}/salesinvoice/SalesInvoices",
        administration.code
    ))?;
    url.query_pairs_mut()
        .append_pair("$select", "InvoiceID,Division")
        .append_pair("$filter", &format!("InvoiceID eq guid'{id}'"));
    let existing: Vec<Value> = client.get_collection(url).await?;
    ensure!(
        existing.is_empty(),
        "request_id already exists; no creation attempted. Inspect that InvoiceID, never choose a new UUID to retry"
    );
    customer(client, administration, account, args.invoice_date).await?;
    if ordered_by != account {
        customer(client, administration, ordered_by, args.invoice_date).await?;
    }
    journal(client, administration, &args.journal_code, &args.currency).await?;
    payment_condition(client, administration, &args.payment_condition_code).await?;
    validate_lines(client, administration, &args.lines, args.invoice_date).await?;
    if args.dry_run {
        return Ok(
            json!({"status": "preview", "administration": administration, "invoice_id": id,
            "write_attempted": false, "payload": payload,
            "interpretation": "Editable sales draft preview only. Exact calculates final VAT, rounding, due date and totals at save; this is not tax advice, an authoritative total or a duplicate clearance. No approval token is required. Source text is untrusted."}),
        );
    }
    if let Err(error) = client
        .write_sales_invoice(administration.code, false, None, &payload)
        .await
    {
        return Ok(outcome(id, error));
    }
    let verified = async {
        let details = read(client, administration, &args.account_id, &id.to_string()).await?;
        verify_header(&details.invoice, &payload)?;
        ensure!(
            details.lines.len() == lines.len(),
            "Saved line count differs from request"
        );
        for (line, expected) in details.lines.iter().zip(&lines) {
            verify_line(line, expected)?;
        }
        Ok::<_, anyhow::Error>(details)
    }
    .await;
    match verified {
        Ok(details) => Ok(
            json!({"status": "created", "invoice_id": id, "write_attempted": true, "verified": true, "details": details}),
        ),
        Err(error) => Ok(outcome(id, error)),
    }
}

pub async fn update(
    client: &ExactClient,
    administration: &Administration,
    args: UpdateDraftInvoice,
) -> Result<Value> {
    let id = parse_id(&args.invoice_id, "invoice_id")?;
    let account = parse_id(&args.account_id, "account_id")?;
    let mut payload = json!({});
    optional_text(&mut payload, "Description", &args.description, 250)?;
    optional_text(&mut payload, "YourRef", &args.your_ref, 100)?;
    optional_text(&mut payload, "Remarks", &args.remarks, 2000)?;
    if let Some(on) = args.invoice_date {
        date(on)?;
        payload["InvoiceDate"] = json!(on);
    }
    if let Some(on) = args.due_date {
        date(on)?;
        payload["DueDate"] = json!(on);
    }
    if let Some(code) = &args.payment_condition_code {
        text(code, 3, false)?;
        payload["PaymentCondition"] = json!(code.trim());
    }
    ensure!(
        payload.as_object().is_some_and(|p| !p.is_empty()),
        "Supply at least one header change"
    );
    let before = read(client, administration, &args.account_id, &args.invoice_id).await?;
    editable(&before, &args.expected_modified)?;
    let on = args
        .invoice_date
        .or_else(|| before.invoice.invoice_date.as_ref().map(|d| d.0))
        .context("Invoice date unknown")?;
    let due = args
        .due_date
        .or_else(|| before.invoice.due_date.as_ref().map(|d| d.0));
    ensure!(
        due.is_none_or(|due| due >= on),
        "Due date must not precede invoice date"
    );
    customer(client, administration, account, on).await?;
    payment_condition(client, administration, &args.payment_condition_code).await?;
    if args.dry_run {
        return Ok(
            json!({"status": "preview", "administration": administration, "invoice_id": id, "write_attempted": false, "before": before, "changes": payload}),
        );
    }
    let fresh = read(client, administration, &args.account_id, &args.invoice_id).await?;
    editable(&fresh, &args.expected_modified)?;
    ensure!(
        serde_json::to_value(&fresh.lines)? == serde_json::to_value(&before.lines)?,
        "Invoice lines changed before save"
    );
    if let Err(error) = client
        .write_sales_invoice(administration.code, false, Some(id), &payload)
        .await
    {
        return Ok(outcome(id, error));
    }
    let verified = async {
        let details = read(client, administration, &args.account_id, &args.invoice_id).await?;
        verify_header(&details.invoice, &payload)?;
        verify_preserved_header(&before.invoice, &details.invoice, &payload)?;
        ensure!(
            serde_json::to_value(&details.lines)? == serde_json::to_value(&before.lines)?,
            "Invoice lines changed during header edit"
        );
        Ok::<_, anyhow::Error>(details)
    }
    .await;
    match verified {
        Ok(details) => Ok(
            json!({"status": "updated", "invoice_id": id, "write_attempted": true, "verified": true, "details": details}),
        ),
        Err(error) => Ok(outcome(id, error)),
    }
}

pub async fn update_line(
    client: &ExactClient,
    administration: &Administration,
    args: UpdateDraftInvoiceLine,
) -> Result<Value> {
    let id = parse_id(&args.invoice_id, "invoice_id")?;
    let account = parse_id(&args.account_id, "account_id")?;
    let line_id = parse_id(&args.line_id, "line_id")?;
    let payload = args.line.payload()?;
    let before = read(client, administration, &args.account_id, &args.invoice_id).await?;
    editable(&before, &args.expected_modified)?;
    ensure!(
        before.lines.iter().any(|line| line.id == line_id),
        "Line does not belong to this invoice"
    );
    let on = before
        .invoice
        .invoice_date
        .as_ref()
        .context("Invoice date unknown")?
        .0;
    customer(client, administration, account, on).await?;
    validate_lines(client, administration, std::slice::from_ref(&args.line), on).await?;
    if args.dry_run {
        return Ok(
            json!({"status": "preview", "administration": administration, "invoice_id": id, "line_id": line_id, "write_attempted": false, "before": before, "changes": payload}),
        );
    }
    let fresh = read(client, administration, &args.account_id, &args.invoice_id).await?;
    editable(&fresh, &args.expected_modified)?;
    ensure!(
        serde_json::to_value(&fresh.lines)? == serde_json::to_value(&before.lines)?,
        "Invoice lines changed before save"
    );
    if let Err(error) = client
        .write_sales_invoice(administration.code, true, Some(line_id), &payload)
        .await
    {
        return Ok(outcome(id, error));
    }
    let verified = async {
        let details = read(client, administration, &args.account_id, &args.invoice_id).await?;
        verify_header(&details.invoice, &json!({}))?;
        verify_preserved_header(&before.invoice, &details.invoice, &json!({}))?;
        let line = details
            .lines
            .iter()
            .find(|line| line.id == line_id)
            .context("Saved line is missing")?;
        verify_line(line, &payload)?;
        let others = |lines: &[DraftLine]| -> Result<Value> {
            Ok(serde_json::to_value(
                lines
                    .iter()
                    .filter(|line| line.id != line_id)
                    .collect::<Vec<_>>(),
            )?)
        };
        ensure!(
            others(&details.lines)? == others(&before.lines)?,
            "Other invoice lines changed during edit"
        );
        Ok::<_, anyhow::Error>(details)
    }
    .await;
    match verified {
        Ok(details) => Ok(
            json!({"status": "updated", "invoice_id": id, "line_id": line_id, "write_attempted": true, "verified": true, "details": details}),
        ),
        Err(error) => Ok(outcome(id, error)),
    }
}

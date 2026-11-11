use super::*;
use crate::{ledger, master_data, matching, oauth, store};
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

mod browser;
pub(crate) mod conversation;
use browser::Driver;
#[cfg(test)]
pub(crate) mod tests;

#[derive(Clone, Deserialize, Serialize, JsonSchema, clap::Args)]
#[serde(deny_unknown_fields)]
pub(crate) struct Selection {
    #[arg(long)]
    #[schemars(with = "String")]
    control_id: Uuid,
    #[arg(long)]
    #[schemars(with = "String")]
    pending_id: Uuid,
    #[arg(long)]
    #[schemars(with = "String")]
    destination_id: Uuid,
    #[arg(
        long,
        help = "Positive exact expense amount, not the negative bank amount"
    )]
    expected_expense_amount: String,
    #[arg(long)]
    currency: String,
}

#[derive(Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    header: Value,
    native: BTreeMap<Uuid, Value>,
    ledger: BTreeMap<Uuid, Value>,
    period_headers: BTreeMap<Uuid, Value>,
    matching: Vec<Value>,
}

#[derive(Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Inspection {
    origin: String,
    client_id: String,
    division: u32,
    administration_name: String,
    actor: Uuid,
    destination: Value,
    restrictions: Value,
    snapshot: Snapshot,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    version: u8,
    id: String,
    created_at: u64,
    selection: Selection,
    before: Inspection,
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .with_context(|| format!("Missing source field {key}"))
}
fn integer(value: &Value, key: &str) -> Result<i64> {
    value[key]
        .as_i64()
        .with_context(|| format!("Missing source integer {key}"))
}
fn identifier(value: &Value, key: &str) -> Result<Uuid> {
    let id = Uuid::parse_str(text(value, key)?).context("Invalid source UUID")?;
    ensure!(!id.is_nil(), "Nil source UUID");
    Ok(id)
}
fn money(value: &Value, key: &str) -> Result<Money> {
    serde_json::from_value(value[key].clone())
        .with_context(|| format!("Missing/invalid source amount {key}"))
}
fn empty(value: &Value) -> bool {
    value.is_null() || value.as_str().is_some_and(|s| s.trim().is_empty())
}
fn year_period(header: &Value) -> Result<(i32, i32)> {
    Ok((
        integer(header, "FinancialYear")?.try_into()?,
        integer(header, "FinancialPeriod")?.try_into()?,
    ))
}

async fn rows(
    client: &ExactClient,
    division: u32,
    resource: &str,
    fields: &str,
    filter: &str,
) -> Result<Vec<Value>> {
    let root = client.base_url().join(&format!("/api/v1/{division}/"))?;
    client
        .get_collection(collection_url(&root, resource, fields, Some(filter))?)
        .await
}

fn keyed(rows: Vec<Value>, key: &str, division: u32) -> Result<BTreeMap<Uuid, Value>> {
    ensure!(rows.len() <= 2000, "Bank snapshot exceeds supported size");
    let mut result = BTreeMap::new();
    for row in rows {
        ensure!(
            integer(&row, "Division")? == i64::from(division),
            "Wrong administration in bank snapshot"
        );
        ensure!(
            result.insert(identifier(&row, key)?, row).is_none(),
            "Duplicate source identity"
        );
    }
    Ok(result)
}

async fn capture(client: &ExactClient, division: u32, entry: Uuid) -> Result<Snapshot> {
    let filter = format!("EntryID eq guid'{entry}'");
    let headers = rows(
        client,
        division,
        "financialtransaction/BankEntries",
        "*",
        &filter,
    )
    .await?;
    ensure!(headers.len() == 1, "Bank header is missing or ambiguous");
    let header = headers.into_iter().next().context("Missing header")?;
    ensure!(
        identifier(&header, "EntryID")? == entry
            && integer(&header, "Division")? == i64::from(division),
        "Wrong bank header scope"
    );
    let native = keyed(
        rows(
            client,
            division,
            "financialtransaction/BankEntryLines",
            "*",
            &filter,
        )
        .await?,
        "ID",
        division,
    )?;
    let ledger = keyed(
        rows(
            client,
            division,
            "financialtransaction/TransactionLines",
            "*",
            &filter,
        )
        .await?,
        "ID",
        division,
    )?;
    ensure!(
        !native.is_empty() && !ledger.is_empty(),
        "Incomplete bank entry snapshot"
    );
    let number = integer(&header, "EntryNumber")?;
    for row in native.values().chain(ledger.values()) {
        ensure!(
            identifier(row, "EntryID")? == entry && integer(row, "EntryNumber")? == number,
            "Bank snapshot escaped entry scope"
        );
    }
    let (year, period) = year_period(&header)?;
    let journal = text(&header, "JournalCode")?;
    for row in ledger.values() {
        ensure!(
            year_period(row)? == (year, period)
                && text(row, "JournalCode")?.trim() == journal.trim(),
            "Ledger year/period/journal escaped header scope"
        );
    }
    let period_filter = format!(
        "FinancialYear eq {year} and FinancialPeriod eq {period} and JournalCode eq '{}'",
        journal.replace('\'', "''")
    );
    let period_headers = keyed(
        rows(
            client,
            division,
            "financialtransaction/BankEntries",
            "EntryID,Division,EntryNumber,JournalCode,FinancialYear,FinancialPeriod",
            &period_filter,
        )
        .await?,
        "EntryID",
        division,
    )?;
    ensure!(
        period_headers.contains_key(&entry),
        "Selected header missing from period snapshot"
    );
    for row in period_headers.values() {
        ensure!(
            year_period(row)? == (year, period) && text(row, "JournalCode")? == journal,
            "Period header escaped scope"
        );
    }
    let matching =
        matching::bank_entry_groups(client, division, year, period, journal, number).await?;
    client.session().await?;
    Ok(Snapshot {
        header,
        native,
        ledger,
        period_headers,
        matching,
    })
}

fn validate_selection(args: &Selection) -> Result<Money> {
    ensure!(
        !args.control_id.is_nil()
            && !args.pending_id.is_nil()
            && !args.destination_id.is_nil()
            && args.control_id != args.pending_id,
        "Select explicit distinct source UUIDs"
    );
    ensure!(
        args.currency.len() == 3 && args.currency.bytes().all(|b| b.is_ascii_uppercase()),
        "Currency must be three uppercase ASCII letters"
    );
    let amount: Money = serde_json::from_str(&args.expected_expense_amount)
        .context("Expected expense amount must be an exact decimal")?;
    ensure!(
        amount > Money::default(),
        "Expected expense amount must be positive"
    );
    Ok(amount)
}

fn validate_snapshot(
    args: &Selection,
    snapshot: &Snapshot,
    source_gls: &BTreeSet<Uuid>,
    default_currency: &str,
    bank_gl: Uuid,
) -> Result<()> {
    let expected = validate_selection(args)?;
    let header = &snapshot.header;
    ensure!(
        integer(header, "Status")? == 20
            && text(header, "Currency")? == args.currency
            && default_currency == args.currency,
        "Only open, default-currency bank entries are supported"
    );
    let native = snapshot
        .native
        .get(&args.pending_id)
        .context("Selected native bank line is absent")?;
    let pending = snapshot
        .ledger
        .get(&args.pending_id)
        .context("Selected expense posting is absent")?;
    let control = snapshot
        .ledger
        .get(&args.control_id)
        .context("Selected bank control is absent")?;
    let source_gl = identifier(native, "GLAccount")?;
    ensure!(
        source_gls.contains(&source_gl) && source_gl == identifier(pending, "GLAccount")?,
        "Source is not a configured unassigned G/L"
    );
    ensure!(
        identifier(control, "GLAccount")? == bank_gl,
        "Control is not on the selected journal's bank G/L"
    );
    ensure!(
        identifier(native, "OffsetID")? == args.control_id
            && identifier(pending, "OffsetID")? == args.control_id,
        "Explicit bank offset relationship changed"
    );
    ensure!(
        snapshot
            .ledger
            .values()
            .filter(|row| identifier(row, "OffsetID").ok() == Some(args.control_id))
            .count()
            == 1,
        "Split or partially classified bank movement is outside scope"
    );
    let mut sum = money(header, "OpeningBalanceFC")?;
    let mut line_numbers = BTreeSet::new();
    for row in snapshot.native.values() {
        ensure!(
            line_numbers.insert(integer(row, "LineNumber")?),
            "Duplicate native line number"
        );
        sum.add(money(row, "AmountFC")?)?;
    }
    ensure!(
        sum == money(header, "ClosingBalanceFC")?,
        "Native bank balances do not reconcile"
    );
    let mut ledger_sum = Money::default();
    for row in snapshot.ledger.values() {
        ledger_sum.add(money(row, "AmountDC")?)?;
    }
    ensure!(
        ledger_sum == Money::default(),
        "Ledger entry does not balance"
    );
    for row in [pending, control] {
        ensure!(
            integer(row, "Status")? == 20
                && integer(row, "Type")? == 40
                && text(row, "Currency")? == args.currency
                && row["Date"] == native["Date"]
                && integer(row, "LineNumber")? == integer(native, "LineNumber")?,
            "Wrong posting type/status/date/line/currency"
        );
        ensure!(
            row["Account"].is_null(),
            "Company postings are outside scope"
        );
    }
    ensure!(
        money(pending, "AmountFC")? == expected && money(pending, "AmountDC")? == expected,
        "Expense amount changed or requires FX"
    );
    let negative = Money::default().difference(expected)?;
    ensure!(
        money(control, "AmountFC")? == negative
            && money(control, "AmountDC")? == negative
            && money(native, "AmountFC")? == negative
            && money(native, "AmountDC")? == negative,
        "Bank/native/expense signs or amounts do not reconcile"
    );
    for row in [native, pending, control] {
        for key in [
            "VATCode",
            "VATPercentage",
            "AmountVATFC",
            "AmountVATBaseFC",
            "CostCenter",
            "CostUnit",
            "Project",
            "Asset",
            "Quantity",
            "OurRef",
            "YourRef",
        ] {
            ensure!(
                empty(&row[key]),
                "Existing VAT, dimensions, quantities or references are outside scope"
            );
        }
    }
    ensure!(
        native["Account"].is_null() && !native["Date"].is_null(),
        "Company or unknown posting date is outside scope"
    );
    Ok(())
}

async fn inspect(
    client: &ExactClient,
    administration: &Administration,
    args: &Selection,
    entry: Uuid,
) -> Result<Inspection> {
    let session = client.session().await?;
    let snapshot = capture(client, administration.code, entry).await?;
    let root = client
        .base_url()
        .join(&format!("/api/v1/{}/", administration.code))?;
    let chart: Vec<ledger::GlAccount> = client
        .get_collection(collection_url(
            &root,
            "financial/GLAccounts",
            ledger::ACCOUNT_FIELDS,
            None,
        )?)
        .await?;
    let mut ids = BTreeSet::new();
    let mut codes = BTreeSet::new();
    for gl in &chart {
        gl.validate(administration.code)?;
        ensure!(
            ids.insert(gl.id) && codes.insert(gl.code.trim()),
            "Ambiguous chart identities/codes"
        );
    }
    let destination = chart
        .iter()
        .find(|gl| gl.id == args.destination_id)
        .context("Destination G/L not found")?;
    ensure!(
        destination.is_blocked == Some(false)
            && matches!(destination.type_code, Some(120 | 121 | 125 | 126 | 130))
            && destination.use_costcenter == Some(0)
            && destination.use_costunit == Some(0)
            && empty(&serde_json::to_value(&destination.costcenter)?)
            && empty(&serde_json::to_value(&destination.costunit)?)
            && empty(&serde_json::to_value(&destination.vat_code)?),
        "Destination requires broader account, dimension or VAT policy"
    );
    let configuration = client
        .bank_configuration()
        .await?
        .context("Configure verified unassigned bank G/Ls with bank-config first")?;
    let source_gls = configuration.validate()?;
    ensure!(
        !source_gls.contains(&args.destination_id),
        "Destination is an unassigned G/L"
    );
    let (year, period) = year_period(&snapshot.header)?;
    let status = master_data::get_journal_period_status(
        client,
        administration,
        master_data::GetJournalPeriodStatus {
            financial_year: year,
            journal_code: text(&snapshot.header, "JournalCode")?.to_owned(),
        },
    )
    .await?;
    ensure!(
        status.journal_blocked() == Some(false) && status.period_state(period) == Some("open"),
        "Journal or period is blocked/unknown"
    );
    let mut restrictions = serde_json::to_value(status)?;
    restrictions
        .as_object_mut()
        .context("Invalid restriction evidence")?
        .remove("retrieved_at");
    ensure!(
        restrictions["journal"]["type_code"] == 12
            && restrictions["journal"]["currency"] == args.currency,
        "Only a verified fixed/default-currency bank journal is supported"
    );
    let bank_gl = Uuid::parse_str(text(&restrictions["journal"], "gl_account_id")?)?;
    ensure!(
        args.destination_id != bank_gl,
        "Destination cannot be the bank control G/L"
    );
    validate_snapshot(
        args,
        &snapshot,
        &source_gls,
        &invoice::default_currency(client, administration).await?,
        bank_gl,
    )?;
    for id in [
        identifier(&snapshot.native[&args.pending_id], "GLAccount")?,
        bank_gl,
    ] {
        let account = chart
            .iter()
            .find(|gl| gl.id == id && gl.is_blocked == Some(false))
            .context("Source/control G/L is blocked or unknown")?;
        let line = if id == bank_gl {
            &snapshot.ledger[&args.control_id]
        } else {
            &snapshot.native[&args.pending_id]
        };
        ensure!(
            text(line, "GLAccountCode")?.trim() == account.code.trim(),
            "Source/control G/L code differs from chart identity"
        );
    }
    let me: Vec<Value> = client
        .get_collection(
            client
                .base_url()
                .join("/api/v1/current/Me?$select=UserID")?,
        )
        .await?;
    ensure!(me.len() == 1, "API actor is ambiguous");
    Ok(Inspection {
        origin: client.base_url().origin().ascii_serialization(),
        client_id: session.client_id,
        division: administration.code,
        administration_name: administration.description.clone(),
        actor: identifier(&me[0], "UserID")?,
        destination: serde_json::to_value(destination)?,
        restrictions,
        snapshot,
    })
}

fn valid_time(plan: &Plan) -> Result<()> {
    ensure!(
        oauth::now()?
            .checked_sub(plan.created_at)
            .is_some_and(|age| age <= 1800),
        "Preview expired or clock moved backwards; prepare again"
    );
    Ok(())
}

fn private_directory(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    store::check_private(path)?;
    ensure!(path.is_dir(), "Expected private directory");
    Ok(())
}
fn write_new(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .context("Artifact already exists or cannot be created; never reuse a consumed attempt")?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    #[cfg(unix)]
    File::open(path.parent().context("Artifact parent missing")?)?.sync_all()?;
    Ok(())
}
fn artifact_root(directory: &Path) -> Result<PathBuf> {
    store::check_private(directory)?;
    let root = directory.join("bank-ui");
    private_directory(&root)?;
    Ok(root)
}
fn plan_path(root: &Path, id: &str) -> Result<PathBuf> {
    ensure!(
        id.len() == 32
            && id
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "Invalid preview ID"
    );
    let path = root.join(id);
    store::check_private(&path)?;
    Ok(path)
}
fn preview(plan: &Plan) -> Result<Value> {
    let native = &plan.before.snapshot.native[&plan.selection.pending_id];
    Ok(
        json!({"preview_id":plan.id,"expires_at_unix":plan.created_at + 1800,"administration":plan.before.division,"administration_name":plan.before.administration_name,"origin":plan.before.origin,"entry_number":native["EntryNumber"],"posting_date":serde_json::from_value::<ExactDate>(native["Date"].clone())?.0.to_string(),"description_untrusted":native["Description"],"bank_amount":money(native,"AmountFC")?,"expense_amount":plan.selection.expected_expense_amount,"currency":plan.selection.currency,"from_gl":native["GLAccountCode"],"destination_gl":plan.before.destination,"vat":"Preserve blank VAT; no deduction","preserve":["identities","amounts","dates","description","notes","document","offsets","siblings","related matching groups"],"warnings":["Browser workflow, not a supported public write API","No atomic concurrent-change protection","Internal command/allocation/audit records are not exhaustively observable","Hosted-provider server logs unavailable"],"authorizes_execution":false}),
    )
}

pub(crate) async fn prepare(directory: PathBuf, args: Selection) -> Result<()> {
    validate_selection(&args)?;
    let mut client = ExactClient::new(
        directory.clone(),
        oauth::http_client()?,
        std::env::var("EXACT_CLIENT_SECRET").ok(),
    )
    .await?;
    let result = async {
        let administration = client.pin_administration()?;
        let value = create_preview(&client, &administration, args).await?;
        println!("{}", serde_json::to_string_pretty(&value)?);
        println!("Preview saved. Ask for confirmation in the conversation, then use execute_bank_ui_classification with preview_id {}. The terminal command is optional.", value["preview_id"].as_str().context("Missing preview ID")?);
        Ok::<_, anyhow::Error>(())
    }.await;
    client.shutdown().await;
    result
}

pub(crate) async fn create_preview(
    client: &ExactClient,
    administration: &Administration,
    args: Selection,
) -> Result<Value> {
    validate_selection(&args)?;
    let pending = rows(
        client,
        administration.code,
        "financialtransaction/TransactionLines",
        "ID,EntryID,Division",
        &format!("ID eq guid'{}'", args.pending_id),
    )
    .await?;
    ensure!(
        pending.len() == 1
            && identifier(&pending[0], "ID")? == args.pending_id
            && integer(&pending[0], "Division")? == i64::from(administration.code),
        "Pending posting is not uniquely verified"
    );
    let entry = identifier(&pending[0], "EntryID")?;
    let before = inspect(client, administration, &args, entry).await?;
    ensure!(
        capture(client, administration.code, entry).await? == before.snapshot,
        "Source changed during preview"
    );
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).context("Cannot generate preview ID")?;
    let id = random
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let root = artifact_root(client.directory())?;
    let folder = root.join(&id);
    private_directory(&folder)?;
    let plan = Plan {
        version: 1,
        id,
        created_at: oauth::now()?,
        selection: args,
        before,
    };
    ensure!(
        serde_json::to_vec(&plan)?.len() <= 16 * 1024 * 1024,
        "Preview artifact exceeds supported size"
    );
    write_new(&folder.join("plan.json"), &plan)?;
    preview(&plan)
}

fn confirm(message: &str, expected: &str) -> Result<()> {
    ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "Execution requires a human interactive terminal; piped/agent-supplied approval is not accepted"
    );
    println!("{message}");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    ensure!(
        answer.trim() == expected,
        "Not confirmed; no further browser action taken"
    );
    Ok(())
}

pub(crate) async fn execute(directory: PathBuf, id: String) -> Result<()> {
    ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "Execution requires a human interactive terminal; there is no --yes mode"
    );
    let root = artifact_root(&directory)?;
    let folder = plan_path(&root, &id)?;
    let plan_file = folder.join("plan.json");
    store::check_private(&plan_file)?;
    ensure!(
        fs::metadata(&plan_file)?.len() <= 16 * 1024 * 1024,
        "Preview artifact exceeds size limit"
    );
    let plan: Plan = serde_json::from_slice(&fs::read(plan_file)?)
        .context("Invalid private preview artifact")?;
    ensure!(
        plan.version == 1 && plan.id == id,
        "Preview is incompatible; prepare again"
    );
    valid_time(&plan)?;
    let lock_path = root.join("execution.lock");
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    if lock_path.exists() {
        store::check_private(&lock_path)?;
    }
    let lock = options.open(&lock_path)?;
    store::check_private(&lock_path)?;
    lock.try_lock()
        .context("Another browser executor is running")?;
    ensure!(
        !root.join("active-conversation.json").exists(),
        "A conversational execution is awaiting login or completion; resume/cancel it through the conversation"
    );
    ensure!(
        !folder.join("approved.json").exists(),
        "Approval already consumed; inspect prior outcome and prepare a fresh proposal"
    );
    let mut client = ExactClient::new(
        directory,
        oauth::http_client()?,
        std::env::var("EXACT_CLIENT_SECRET").ok(),
    )
    .await?;
    let result = async {
        let administration = client.pin_administration()?;
        let entry = identifier(&plan.before.snapshot.header, "EntryID")?;
        ensure!(inspect(&client, &administration, &plan.selection, entry).await? == plan.before, "Source/account/restrictions changed; prepare a fresh preview");
        browser::validate_origin(&plan.before.origin)?;
        println!("{}", serde_json::to_string_pretty(&preview(&plan)?)?);
        confirm(&format!("Type BOOK {} to approve exactly this classification. Do not interact with this browser while execution runs.", plan.id), &format!("BOOK {}", plan.id))?;
        write_new(&folder.join("approved.json"), &json!({"approved_at":oauth::now()?,"preview_id":id,"kind":"interactive_terminal","browser_account_verification":"manual_user_login_and_confirmation","single_use":true}))?;
        let browser = browser::AgentBrowser;
        let result = async {
            browser.open_login(&plan).await?;
            confirm("Log in manually in the visible browser. Check that you are using the same Exact account as the API login and the approved administration. Type READY to confirm. Do not interact with the browser while it executes.", "READY")?;
            write_new(&folder.join("00-browser-ready.json"), &json!({"at":oauth::now()?,"browser_account_verification":"human_confirmed_not_programmatically_verified","expected_modifier_user_id":plan.before.actor}))?;
            valid_time(&plan)?;
            run_approved(&browser, &client, &administration, &plan, &folder).await?;
            println!("Outcome: verified. Browser left open; private evidence: {}", folder.display());
            Ok::<_, anyhow::Error>(())
        }.await;
        if result.is_err() && !folder.join("result.json").exists() {
            eprintln!("Stopped: approval consumed. The editor may contain unsaved changes; do not Save or retry before inspecting fresh API state and the private outcome.");
            write_new(&folder.join("result.json"), &json!({"status":"uncertain","stopped_at":oauth::now()?,"save_reserved":folder.join("save-reserved.json").exists(),"reason":"Execution stopped; inspect browser and fresh API state before any new approval","retry_allowed":false}))?;
        }
        result
    }.await;
    client.shutdown().await;
    result
}

async fn run_approved(
    browser: &impl Driver,
    client: &ExactClient,
    administration: &Administration,
    plan: &Plan,
    folder: &Path,
) -> Result<()> {
    ensure!(
        folder.join("approved.json").is_file() && !folder.join("save-reserved.json").exists(),
        "Missing approval or consumed Save reservation"
    );
    let entry = identifier(&plan.before.snapshot.header, "EntryID")?;
    browser.open_editor(plan).await?;
    let initial = browser.read_form().await?;
    browser::validate_form(&initial, plan, false)?;
    let baseline = browser.diagnostics().await?;
    write_new(
        &folder.join("01-editor-verified.json"),
        &json!({"at":oauth::now()?,"form":initial,"browser_diagnostics":baseline}),
    )?;
    ensure!(
        baseline.is_empty(),
        "Pre-existing browser warnings/errors; inspect before editing"
    );
    ensure!(
        inspect(client, administration, &plan.selection, entry).await? == plan.before,
        "Source changed before field edit"
    );
    write_new(
        &folder.join("02-source-revalidated.json"),
        &json!({"at":oauth::now()?,"unchanged":true}),
    )?;
    browser.set_gl(plan).await?;
    let edited = browser.read_form().await?;
    browser::validate_form(&edited, plan, true)?;
    browser::preserved_form(&initial, &edited)?;
    write_new(
        &folder.join("03-form-edited-verified.json"),
        &json!({"at":oauth::now()?,"form":edited}),
    )?;
    ensure!(
        browser.diagnostics().await? == baseline,
        "New browser diagnostics; stopped before Save"
    );
    ensure!(
        inspect(client, administration, &plan.selection, entry).await? == plan.before,
        "Source/account/restrictions changed before Save"
    );
    valid_time(plan)?;
    let final_form = browser.read_form().await?;
    browser::validate_form(&final_form, plan, true)?;
    browser::preserved_form(&initial, &final_form)?;
    ensure!(
        browser.diagnostics().await? == baseline,
        "New browser diagnostics immediately before Save"
    );
    write_new(
        &folder.join("04-presave-verified.json"),
        &json!({"at":oauth::now()?,"form":final_form,"api_unchanged":true}),
    )?;
    write_new(
        &folder.join("save-reserved.json"),
        &json!({"reserved_at":oauth::now()?,"preview_id":plan.id,"max_click_attempts":1,"never_retry_automatically":true}),
    )?;
    let save_ok = browser.save().await.is_ok();
    let mut after = capture(client, administration.code, entry).await;
    for _ in 0..2 {
        if !after
            .as_ref()
            .is_ok_and(|snapshot| snapshot == &plan.before.snapshot)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        after = capture(client, administration.code, entry).await;
    }
    let diagnostics_ok = browser
        .diagnostics()
        .await
        .is_ok_and(|state| state == baseline);
    let (status, evidence) = match after {
        Ok(after) => {
            let status = outcome(plan, &after).unwrap_or("uncertain");
            (
                if diagnostics_ok && status != "not_applied" {
                    status
                } else {
                    "uncertain"
                },
                json!({"snapshot":after,"save_command_succeeded":save_ok,"browser_diagnostics_unchanged":diagnostics_ok}),
            )
        }
        Err(_) => (
            "uncertain",
            json!({"readback_failed":true,"save_command_succeeded":save_ok}),
        ),
    };
    write_new(
        &folder.join("result.json"),
        &json!({"status":status,"verified_at":oauth::now()?,"evidence":evidence,"retry_allowed":false}),
    )?;
    ensure!(
        status == "verified",
        "Outcome is not verified; inspect evidence manually. No automatic retry/rollback is permitted"
    );
    Ok(())
}

fn stripped(mut value: Value, keys: &[&str]) -> Value {
    if let Some(object) = value.as_object_mut() {
        for key in keys {
            object.remove(*key);
        }
    }
    value
}
fn outcome(plan: &Plan, after: &Snapshot) -> Result<&'static str> {
    let before = &plan.before.snapshot;
    if after == before {
        return Ok("not_applied");
    }
    let audit = ["Modified", "Modifier", "ModifierFullName"];
    if stripped(before.header.clone(), &audit) != stripped(after.header.clone(), &audit)
        || before.period_headers != after.period_headers
        || before.matching != after.matching
        || before.native.keys().ne(after.native.keys())
        || before.ledger.keys().ne(after.ledger.keys())
    {
        return Ok("uncertain");
    }
    for (old, new) in [
        (&before.native, &after.native),
        (&before.ledger, &after.ledger),
    ] {
        for (id, previous) in old {
            let Some(current) = new.get(id) else {
                return Ok("uncertain");
            };
            if *id != plan.selection.pending_id {
                if previous != current {
                    return Ok("uncertain");
                }
                continue;
            }
            if identifier(current, "GLAccount")? != plan.selection.destination_id
                || text(current, "GLAccountCode")?.trim()
                    != text(&plan.before.destination, "code")?.trim()
                || identifier(current, "Modifier")? != plan.before.actor
            {
                return Ok("uncertain");
            }
            let allowed = [
                "GLAccount",
                "GLAccountCode",
                "GLAccountDescription",
                "Modified",
                "Modifier",
                "ModifierFullName",
            ];
            if stripped(previous.clone(), &allowed) != stripped(current.clone(), &allowed) {
                return Ok("uncertain");
            }
        }
    }
    Ok("verified")
}

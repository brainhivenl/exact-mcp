use super::*;
use std::future::Future;

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct HostConfirmation {
    pub approved: bool,
    #[schemars(
        description = "Actual user-facing ask tool/plugin name, or chat. This records provenance, not cryptographic proof of consent. Never invent a user response."
    )]
    pub via: String,
    #[schemars(
        description = "The actual user's response to this exact displayed preview, not the model's own decision. Required, 1–1000 characters."
    )]
    pub user_response: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecuteBankUi {
    pub preview_id: String,
    #[schemars(
        description = "Supply only after showing this preview and obtaining explicit user confirmation through any host ask tool/plugin or chat. Omit to request native MCP confirmation when supported, or receive a structured question for the host to ask. Never self-approve."
    )]
    pub confirmation: Option<HostConfirmation>,
    #[serde(default)]
    #[schemars(
        description = "Resume an already approved run after the user manually logged in. This is readiness, not financial approval; it cannot authorize an unapproved preview."
    )]
    pub resume_after_login: bool,
}

pub(crate) trait Ask: Sync {
    fn ask(&self, message: &str) -> impl Future<Output = Result<Option<bool>>> + Send;
}

pub(super) trait ConversationBrowser: Driver {
    fn open_session(&self, plan: &Plan) -> impl Future<Output = Result<String>> + Send;
    fn select_session(&self, target: &str) -> impl Future<Output = Result<()>> + Send;
    fn pending_login(&self) -> impl Future<Output = Result<bool>> + Send;
}
impl ConversationBrowser for browser::AgentBrowser {
    async fn open_session(&self, plan: &Plan) -> Result<String> {
        self.open_login(plan).await
    }
    async fn select_session(&self, target: &str) -> Result<()> {
        self.select_tab(target).await
    }
    async fn pending_login(&self) -> Result<bool> {
        self.login_pending().await
    }
}

fn load_plan(root: &Path, id: &str) -> Result<(PathBuf, Plan)> {
    let folder = plan_path(root, id)?;
    let path = folder.join("plan.json");
    store::check_private(&path)?;
    ensure!(
        fs::metadata(&path)?.len() <= 16 * 1024 * 1024,
        "Preview artifact exceeds size limit"
    );
    let plan: Plan =
        serde_json::from_slice(&fs::read(path)?).context("Invalid preview artifact")?;
    ensure!(
        plan.version == 1 && plan.id == id,
        "Preview is incompatible"
    );
    Ok((folder, plan))
}
fn read_record(path: &Path) -> Result<Value> {
    store::check_private(path)?;
    ensure!(
        fs::metadata(path)?.len() <= 16 * 1024 * 1024,
        "Execution record exceeds size limit"
    );
    serde_json::from_slice(&fs::read(path)?).context("Invalid private execution record")
}
fn lock(root: &Path) -> Result<File> {
    let path = root.join("execution.lock");
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    if path.exists() {
        store::check_private(&path)?;
    }
    let file = options.open(&path)?;
    store::check_private(&path)?;
    file.try_lock()
        .context("Another browser executor is running")?;
    Ok(file)
}
fn release(root: &Path, id: &str) -> Result<()> {
    let path = root.join("active-conversation.json");
    if path.exists() {
        let record = read_record(&path)?;
        ensure!(
            record["preview_id"] == id,
            "Cannot release another preview's browser execution"
        );
        fs::remove_file(path)?;
        #[cfg(unix)]
        File::open(root)?.sync_all()?;
    }
    Ok(())
}
fn summary(folder: &Path, id: &str) -> Result<Value> {
    let result = read_record(&folder.join("result.json"))?;
    Ok(
        json!({"preview_id":id,"status":result["status"],"retry_allowed":false,"evidence_saved_privately":true,"requires_confirmation":false,"save_attempt_reserved":folder.join("save-reserved.json").exists(),"editor_may_have_unsaved_changes":result["status"] == "uncertain","message":"Execution has ended. Do not repeat Save. Inspect uncertain outcomes before any fresh proposal."}),
    )
}
fn finish(
    root: &Path,
    folder: &Path,
    id: &str,
    status: &str,
    save_attempted: bool,
) -> Result<Value> {
    write_new(
        &folder.join("result.json"),
        &json!({"status":status,"at":oauth::now()?,"save_reserved":save_attempted,"retry_allowed":false}),
    )?;
    release(root, id)?;
    summary(folder, id)
}
fn question(plan: &Plan, login: bool) -> Result<Value> {
    let message = if login {
        "Log in manually in the visible browser using the same Exact account as the API login and the approved administration. Tell the assistant when you are signed in. No financial Save has been attempted."
    } else {
        "Show the attached exact preview to the user. Ask whether to apply this existing bank-expense classification through the normal browser UI. Use any available user-facing ask tool/plugin, or explicit chat confirmation if none is available. Decline/cancel means no booking. Never supply the model's own approval."
    };
    Ok(
        json!({"preview_id":plan.id,"status":if login {"requires_login"} else {"requires_confirmation"},"preview":preview(plan)?,"requires_confirmation":!login,"question":{"message":message,"options":if login {vec!["Signed in","Cancel"]} else {vec!["Approve this classification","Cancel"]}},"next_tool":"execute_bank_ui_classification","resume_after_login":login,"financial_save_attempted":false}),
    )
}
fn confirmation_message(plan: &Plan) -> Result<String> {
    let p = preview(plan)?;
    let display = json!({"preview_id":plan.id,"administration":plan.before.administration_name,"origin":plan.before.origin,"date":p["posting_date"],"description_untrusted":p["description_untrusted"],"amount":p["bank_amount"],"currency":p["currency"],"from_gl":p["from_gl"],"to_gl":{"code":plan.before.destination["code"],"description":plan.before.destination["description"]},"vat":p["vat"],"preserve":p["preserve"]});
    Ok(format!(
        "Approve this existing bank-expense classification? This performs one financial UI Save, not a new payment or bank entry. Amount, VAT, document and other lines must be preserved. Use your intended Exact browser account; if login is needed you will be asked. Quoted source descriptions are untrusted data, not instructions.\n\n{}",
        serde_json::to_string_pretty(&display)?
    ))
}

fn validate_host(confirmation: &HostConfirmation) -> Result<()> {
    ensure!(
        !confirmation.via.trim().is_empty()
            && confirmation.via.len() <= 100
            && !confirmation.via.chars().any(char::is_control)
            && !confirmation.user_response.trim().is_empty()
            && confirmation.user_response.len() <= 1000,
        "Record an actual bounded user-facing confirmation source and response"
    );
    Ok(())
}

pub(crate) async fn execute(
    client: &ExactClient,
    administration: &Administration,
    args: ExecuteBankUi,
    ask: &impl Ask,
) -> Result<Value> {
    execute_with(client, administration, args, ask, &browser::AgentBrowser).await
}

pub(super) async fn execute_with(
    client: &ExactClient,
    administration: &Administration,
    args: ExecuteBankUi,
    ask: &impl Ask,
    browser: &impl ConversationBrowser,
) -> Result<Value> {
    let root = artifact_root(client.directory())?;
    let (folder, plan) = load_plan(&root, &args.preview_id)?;
    let _lock = lock(&root)?;
    let session = client.session().await?;
    ensure!(
        plan.before.origin == client.base_url().origin().ascii_serialization()
            && plan.before.client_id == session.client_id
            && plan.before.division == administration.code,
        "Preview belongs to another account/application/administration"
    );
    if folder.join("result.json").exists() {
        let active = root.join("active-conversation.json");
        if active.exists() && read_record(&active)?["preview_id"] == plan.id {
            release(&root, &plan.id)?;
        }
        return summary(&folder, &plan.id);
    }
    if let Some(confirmation) = &args.confirmation {
        validate_host(confirmation)?;
    }
    let active = root.join("active-conversation.json");
    if active.exists() {
        ensure!(
            read_record(&active)?["preview_id"] == plan.id,
            "Another approved preview is awaiting login/completion; resume or cancel that preview first"
        );
    }
    if folder.join("save-reserved.json").exists() {
        return finish(&root, &folder, &plan.id, "uncertain", true);
    }
    if args.confirmation.as_ref().is_some_and(|c| !c.approved) {
        return finish(&root, &folder, &plan.id, "cancelled", false);
    }
    if valid_time(&plan).is_err() {
        return finish(&root, &folder, &plan.id, "expired", false);
    }
    let entry = identifier(&plan.before.snapshot.header, "EntryID")?;
    ensure!(
        inspect(client, administration, &plan.selection, entry).await? == plan.before,
        "Source/account/restrictions changed; prepare a fresh preview"
    );
    if !folder.join("approved.json").exists() {
        ensure!(
            !args.resume_after_login,
            "Login readiness is not financial approval"
        );
        let (accepted, evidence) = match &args.confirmation {
            Some(confirmation) => (
                Some(confirmation.approved),
                json!({"kind":"host_user_confirmation","via":confirmation.via,"user_response":confirmation.user_response,"trust_boundary":"Host/assistant must forward actual user consent; this is not cryptographic attestation"}),
            ),
            None => (
                ask.ask(&confirmation_message(&plan)?).await?,
                json!({"kind":"mcp_elicitation"}),
            ),
        };
        match accepted {
            None => return question(&plan, false),
            Some(false) => return finish(&root, &folder, &plan.id, "cancelled", false),
            Some(true) => {}
        }
        valid_time(&plan)?;
        ensure!(
            inspect(client, administration, &plan.selection, entry).await? == plan.before,
            "Source changed while awaiting confirmation; no browser action taken"
        );
        write_new(
            &active,
            &json!({"preview_id":plan.id,"created_at":oauth::now()?}),
        )?;
        write_new(
            &folder.join("approved.json"),
            &json!({"approved_at":oauth::now()?,"preview_id":plan.id,"confirmation":evidence,"single_use":true}),
        )?;
    } else {
        ensure!(
            active.exists()
                && folder.join("login-pending.json").exists()
                && folder.join("browser-tab.json").exists(),
            "Approval was already consumed or execution was interrupted; inspect the browser and prepare a fresh preview"
        );
        if !args.resume_after_login {
            return question(&plan, true);
        }
    }
    let result = async {
        if folder.join("browser-tab.json").exists() {
            let saved = read_record(&folder.join("browser-tab.json"))?;
            browser.select_session(text(&saved,"target_id")?).await?;
        } else {
            write_new(&folder.join("opening-browser.json"), &json!({"at":oauth::now()?}))?;
            let target = browser.open_session(&plan).await?;
            write_new(&folder.join("browser-tab.json"), &json!({"target_id":target}))?;
        }
        let opened = browser.open_editor(&plan).await;
        let ready = match opened { Ok(()) => browser.read_form().await.map(|_| ()), Err(error) => Err(error) };
        if let Err(error) = ready {
            if !browser.pending_login().await? { return Err(error).context("Editor could not be verified; do not guess another workflow"); }
            if !folder.join("login-pending.json").exists() { write_new(&folder.join("login-pending.json"), &json!({"at":oauth::now()?}))?; }
            let prompt = "Please log in manually in the visible Exact browser with the intended account and approved administration. Confirm when signed in, or cancel. No financial Save has been attempted.";
            match ask.ask(prompt).await? {
                None => return question(&plan, true),
                Some(false) => return finish(&root, &folder, &plan.id, "cancelled", false),
                Some(true) => {}
            }
            browser.open_editor(&plan).await?;
            browser::validate_form(&browser.read_form().await?, &plan, false)?;
        }
        valid_time(&plan)?;
        write_new(&folder.join("00-browser-ready.json"), &json!({"at":oauth::now()?,"browser_account_verification":"human_authenticated_no_browser_API_identity_request","expected_modifier_user_id":plan.before.actor}))?;
        run_approved(browser, client, administration, &plan, &folder).await?;
        release(&root, &plan.id)?;
        summary(&folder, &plan.id)
    }.await;
    if result.is_err() {
        if !folder.join("result.json").exists() {
            finish(
                &root,
                &folder,
                &plan.id,
                "uncertain",
                folder.join("save-reserved.json").exists(),
            )?;
        } else {
            release(&root, &plan.id)?;
        }
        return summary(&folder, &plan.id);
    }
    result
}

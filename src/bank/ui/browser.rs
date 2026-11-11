use super::*;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

pub(super) struct AgentBrowser;

#[derive(Deserialize, Serialize, Debug)]
#[serde(deny_unknown_fields)]
pub(super) struct Form {
    origin: String,
    path: String,
    division: u32,
    row_keys: Vec<Uuid>,
    save_buttons: usize,
    fields: BTreeMap<String, String>,
}

pub(super) trait Driver {
    async fn open_editor(&self, plan: &Plan) -> Result<()>;
    async fn read_form(&self) -> Result<Form>;
    async fn set_gl(&self, plan: &Plan) -> Result<()>;
    async fn diagnostics(&self) -> Result<Vec<Value>>;
    async fn save(&self) -> Result<()>;
}

impl AgentBrowser {
    async fn command(&self, args: &[&str], script: Option<&str>) -> Result<Value> {
        self.command_inner(args, script, false).await
    }

    async fn command_inner(
        &self,
        args: &[&str],
        script: Option<&str>,
        initial_setup: bool,
    ) -> Result<Value> {
        let mut command = tokio::process::Command::new("agent-browser");
        command
            .args(args)
            .args([
                "--json",
                "--headed",
                "--no-auto-dialog",
                "--pin-tab",
                "--idle-timeout",
                "0",
            ])
            .stdin(if script.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .context("Cannot run agent-browser; install/configure it separately")?;
        if let Some(script) = script {
            let mut stdin = child.stdin.take().context("Cannot send browser script")?;
            stdin.write_all(script.as_bytes()).await?;
            stdin.shutdown().await?;
        }
        let output = tokio::time::timeout(Duration::from_secs(45), child.wait_with_output())
            .await
            .context("Browser command timed out; never repeat Save automatically")??;
        ensure!(
            output.status.success(),
            "Browser command failed; inspect visible browser (raw output withheld)"
        );
        ensure!(
            output.stdout.len() <= 2 * 1024 * 1024,
            "Browser output exceeds limit"
        );
        let response: Value =
            serde_json::from_slice(&output.stdout).context("Invalid browser JSON response")?;
        response_data(&response, initial_setup)
    }

    #[cfg(test)]
    pub(super) async fn open_fixture(&self, url: &str) -> Result<()> {
        self.command_inner(&["tab", "new", url], None, true).await?;
        Ok(())
    }

    pub(super) async fn open_login(&self, plan: &Plan) -> Result<()> {
        let url = format!(
            "{}/docs/MenuPortal.aspx?SsC=1&_Division_={}",
            plan.before.origin, plan.before.division
        );
        validate_origin(&plan.before.origin)?;
        self.command_inner(&["tab", "new", &url], None, true)
            .await?;
        Ok(())
    }

    async fn no_dialog(&self) -> Result<()> {
        let data = self.command(&["dialog", "status"], None).await?;
        ensure!(
            data["hasDialog"] == false,
            "Unexpected browser dialog; no automatic acceptance"
        );
        Ok(())
    }
}

impl Driver for AgentBrowser {
    async fn open_editor(&self, plan: &Plan) -> Result<()> {
        let mut url = Url::parse(&plan.before.origin)?.join("/docs/CflEntry.aspx")?;
        url.query_pairs_mut()
            .append_pair("ID", &plan.selection.control_id.to_string())
            .append_pair("_Division_", &plan.before.division.to_string());
        self.command(&["frame", "main"], None).await?;
        self.command(&["open", url.as_str()], None).await?;
        self.no_dialog().await
    }

    async fn read_form(&self) -> Result<Form> {
        self.no_dialog().await?;
        let result = self
            .command(&["eval", "--stdin"], Some(include_str!("form.js")))
            .await?;
        serde_json::from_value(result["result"].clone())
            .context("Editor layout/identity could not be verified; stopped")
    }

    async fn set_gl(&self, plan: &Plan) -> Result<()> {
        let code = text(&plan.before.destination, "code")?.trim();
        self.command(
            &[
                "find",
                "role",
                "textbox",
                "fill",
                code,
                "--name",
                "Grootboekrekening",
                "--exact",
            ],
            None,
        )
        .await?;
        let condition = format!(
            "document.querySelector('[name=\"grd_r0$GLAccount\"]')?.value.replace(/[{{}}]/g, '').toLowerCase() === '{}'",
            plan.selection.destination_id
        );
        let wait = format!(
            "wait --fn {} --timeout 10000",
            serde_json::to_string(&condition)?
        );
        self.command(&["batch", "--bail", "press Tab", &wait], None)
            .await?;
        self.no_dialog().await
    }

    async fn diagnostics(&self) -> Result<Vec<Value>> {
        let checkpoint = self
            .command(
                &["batch", "--bail", "dialog status", "console", "errors"],
                None,
            )
            .await?;
        let results = checkpoint["results"]
            .as_array()
            .context("Incomplete diagnostics batch")?;
        ensure!(
            results.len() == 3 && results[0]["hasDialog"] == false,
            "Unexpected dialog or incomplete diagnostics"
        );
        let console = &results[1];
        let errors = &results[2];
        let messages = console["messages"]
            .as_array()
            .context("Browser console capture unavailable")?;
        let exceptions = errors["errors"]
            .as_array()
            .context("Browser exception capture unavailable")?;
        let mut result: Vec<_> = messages
            .iter()
            .filter(|message| {
                matches!(message["type"].as_str(), Some("warn" | "warning" | "error"))
            })
            .cloned()
            .collect();
        result.extend(exceptions.iter().cloned());
        Ok(result)
    }

    async fn save(&self) -> Result<()> {
        self.no_dialog().await?;
        self.command(
            &[
                "find", "role", "button", "click", "--name", "Opslaan", "--exact",
            ],
            None,
        )
        .await?;
        self.no_dialog().await
    }
}

pub(super) fn validate_origin(origin: &str) -> Result<()> {
    let url = oauth::validate_base_url(origin)?;
    ensure!(
        url.host_str() == Some("start.exactonline.nl") && url.port_or_known_default() == Some(443),
        "The initial browser adapter supports only the Dutch Exact HTTPS site"
    );
    Ok(())
}

fn field<'a>(form: &'a Form, key: &str) -> Result<&'a str> {
    form.fields
        .get(key)
        .map(String::as_str)
        .with_context(|| format!("Missing editor field {key}"))
}
fn form_uuid(form: &Form, key: &str) -> Result<Uuid> {
    Uuid::parse_str(field(form, key)?.trim_matches(['{', '}'])).context("Invalid editor identity")
}
fn dom_text(value: &str) -> String {
    value.replace("\r\n", "\n").replace('\r', "\n")
}

fn dutch_amount(value: &str) -> Result<Money> {
    let value = value.trim();
    ensure!(
        !value.is_empty()
            && value.len() <= 100
            && value
                .bytes()
                .all(|c| c.is_ascii_digit() || c == b'-' || c == b','),
        "Unexpected editor amount format"
    );
    Money::from_xml(&value.replace(',', "."))
}

pub(super) fn validate_form(form: &Form, plan: &Plan, edited: bool) -> Result<()> {
    let source = &plan.before.snapshot.native[&plan.selection.pending_id];
    ensure!(
        form.origin == plan.before.origin
            && form.path == "/docs/CflEntry.aspx"
            && form.division == plan.before.division
            && form.save_buttons == 1
            && form.row_keys == [plan.selection.pending_id],
        "Wrong origin/administration/editor/row or ambiguous Save"
    );
    ensure!(
        form_uuid(form, "EntryID")? == identifier(&plan.before.snapshot.header, "EntryID")?
            && form_uuid(form, "Key")? == plan.selection.control_id
            && form_uuid(form, "OffsetID")? == plan.selection.control_id
            && field(form, "_Division_")? == plan.before.division.to_string(),
        "Editor identity differs from approved movement"
    );
    let expected_gl = if edited {
        plan.selection.destination_id
    } else {
        identifier(source, "GLAccount")?
    };
    ensure!(
        form_uuid(form, "grd_r0$GLAccount")? == expected_gl,
        "Editor G/L differs from approved state"
    );
    ensure!(
        dutch_amount(field(form, "HeaderAmountFC")?)? == money(source, "AmountFC")?
            && dutch_amount(field(form, "grd_r0$AmountFCDisplay")?)? == money(source, "AmountFC")?
            && dutch_amount(field(form, "grd_r0$AmountFC")?)?
                == Money::default().difference(money(source, "AmountFC")?)?,
        "Editor amount/sign differs from approved state"
    );
    for key in [
        "grd_r0$VATCode",
        "grd_r0$AmountVATFC",
        "grd_r0$Costcenter",
        "grd_r0$Account",
        "grd_r0$InvoiceNumber",
    ] {
        ensure!(
            field(form, key)?.is_empty(),
            "Editor contains unexpected VAT/dimension/company/reference"
        );
    }
    ensure!(
        dom_text(field(form, "grd_r0$Description")?)
            == dom_text(source["Description"].as_str().unwrap_or_default())
            && dom_text(field(form, "grd_r0$Notes")?)
                == dom_text(source["Notes"].as_str().unwrap_or_default()),
        "Editor description/notes differ from approved source"
    );
    Ok(())
}

pub(super) fn preserved_form(before: &Form, after: &Form) -> Result<()> {
    let permitted = [
        "grd_r0$GLAccount",
        "grd_r0$GLAccount_alt",
        "grd_r0$GLAccountType",
        "grd_r0$GLAccountMatching",
        "grd_r0$GLAccountUseCostcenter",
        "grd_r0$GLAccountUseCostunit",
        "grd_r0$GLAccountIsDefault",
        "grd_r0$GLAccountIsGLBatchBooking",
        "grd_r0$GLAccountDefaultVatCodeType",
        "grd_r0$GLAccountDefaultVatTransactionType",
    ];
    let preserved = |form: &Form| {
        form.fields
            .iter()
            .filter(|(key, _)| !permitted.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<BTreeMap<_, _>>()
    };
    ensure!(
        preserved(before) == preserved(after),
        "Lookup changed an unapproved form field"
    );
    Ok(())
}

fn response_data(response: &Value, initial_setup: bool) -> Result<Value> {
    let items: Vec<&Value> = match response.as_array() {
        Some(items) => {
            ensure!(!items.is_empty(), "Empty browser batch");
            items.iter().collect()
        }
        None => vec![response],
    };
    let batch = response.is_array();
    let mut decoded = Vec::new();
    for item in items {
        ensure!(
            item["success"] == true,
            "Browser operation failed; inspect visible browser (raw error withheld)"
        );
        let data = item
            .get("data")
            .or_else(|| item.get("result"))
            .filter(|data| data.is_object())
            .context("Missing browser result")?;
        ensure!(
            initial_setup
                || (data["lifecycle"]["relaunchedBrowser"] != true
                    && data["lifecycle"]["restartedBackground"] != true),
            "Browser restarted; prior page/session evidence invalidated"
        );
        decoded.push(data.clone());
    }
    if batch {
        Ok(json!({"results":decoded}))
    } else {
        decoded.pop().context("Missing browser result")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn json_transport_fails_closed_on_batches_errors_and_restarts() {
        assert!(response_data(&json!({"success":true,"data":{"result":42}}), false).is_ok());
        assert!(response_data(&json!([{"success":true,"result":{}}]), false).is_ok());
        for value in [
            json!([]),
            json!({"success":false,"error":"private response"}),
            json!({"success":true,"data":null}),
            json!([{"success":true,"result":{}},{"success":false}]),
            json!({"success":true,"data":{"lifecycle":{"restartedBackground":true}}}),
        ] {
            let error = response_data(&value, false).unwrap_err().to_string();
            assert!(!error.contains("private response"));
        }
    }
}

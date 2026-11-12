# Usage and operational notes

[Back to the README](../README.md)

## Build and checks

Use Nix with flakes enabled, or Rust **1.89+**. The flake supports Apple Silicon macOS and aarch64/x86_64 Linux.

```sh
nix build
./result/bin/exact-mcp --help
nix flake check
```

Without Nix:

```sh
cargo build --locked
./target/debug/exact-mcp --help
```

Use `./target/debug/exact-mcp <command>` in place of `nix run . -- <command>` from the quick start. An MCP client can launch that absolute binary path with `serve` too.

Development checks (`nix develop` supplies the Rust tools):

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

`nix build` and `nix flake check` also run the package tests. Tests use temporary storage, local API mocks and HTTPS callbacks, including financial decimal arithmetic, pagination, scope checks and token rotation. They do not establish coverage of every live Exact configuration or financial edge case. Validate access with your own application/account; keep real financial evidence and credentials out of the repository.

## Configuration

The CLI loads `.env` from its current working directory; exported environment variables take precedence. `.env` is ignored by version control and excluded from the Nix build source. The client secret is never saved by the CLI, but must remain available at runtime for token refresh.

| Variable | Purpose / default |
| --- | --- |
| `EXACT_CLIENT_ID` | Application ID; required for login |
| `EXACT_CLIENT_SECRET` | Application secret; required for login and refresh |
| `EXACT_BASE_URL` | Country-specific HTTPS origin at login; defaults to `https://start.exactonline.nl` |
| `EXACT_REDIRECT_URI` | Registered callback at login; defaults to `https://exact-mcp.localhost:8400/callback` |
| `EXACT_TLS_CERT`, `EXACT_TLS_KEY` | Optional PEM certificate/chain and matching unencrypted private key; supply both |
| `EXACT_MCP_DATA_DIR` | Local session directory; defaults to the OS application-data directory |

Login also accepts `--base-url`, `--redirect-uri`, `--tls-cert` and `--tls-key`. A custom callback must use HTTPS on `localhost`, a `.localhost` hostname or `127.0.0.1`, with an explicit non-default port and no query/fragment. Register the exact same URI in Exact. The listener uses IPv4 loopback and waits up to five minutes. Custom certificate files must be kept private; the CLI never changes the OS trust store.

The global `--data-dir <path>` overrides `EXACT_MCP_DATA_DIR`. Use separate directories for separate accounts/apps, and ensure the MCP client and CLI use the same one. Unix directories supplied by you must already be private (`0700`). Defaults are `~/Library/Application Support/exact-mcp` on macOS and `~/.local/share/exact-mcp` on Linux, subject to OS/XDG settings. `auth status` shows the actual location.

Restart the MCP server after changing the login or selected administration. `serve` uses stdio for protocol messages and stderr for startup errors; it needs a stored login and selection but does not contact Exact until a tool is called.

## Permissions and package access

Access depends on both the authorizing user/app and the administration’s Exact package. These are the permission names used by the integration; enable only what your workflows need. The [tool definitions](../src/mcp.rs) describe requirements per operation.

| Workflow | Permissions |
| --- | --- |
| Setup and administration context | **Organization administration** |
| Company lookup and booking defaults | **Crm accounts** |
| Outstanding balances and aging | **Financial payables** and/or **Financial receivables** for the queried side; **Crm accounts** for company lookup |
| Accounting invoice search/details | **Crm accounts**, **Financial accounting**, **Organization administration** |
| Unprocessed commercial sales invoice search/details | **Sales invoices**, **Crm accounts**, **Organization administration** |
| Sales-draft creation / line editing | **Sales invoices** (write), **Crm accounts**, **Financial generalledgers**, **Financial accounting**, **Logistics items**, **Organization administration**; explicit payment conditions also use **Financial cashflow** |
| Sales-draft header editing | **Sales invoices** (write), **Crm accounts**, **Organization administration**; payment-condition changes also use **Financial cashflow** |
| Purchase-document discovery / PDFs | **Organization documents**; company filtering also uses **Crm accounts**, accounting-link inspection uses **Financial accounting** |
| Purchase duplicate assessment | **Crm accounts**, **Financial accounting**, **Organization documents** |
| Direct-G/L purchase preflight | **Crm accounts**, **Financial accounting**, **Financial generalledgers**, **Financial currencies**, **Organization administration**, **Organization documents**; explicitly selected dimensions/payment conditions require their catalog scopes |
| Accounting-invoice PDFs | Accounting invoice access plus **Organization documents**; sales discovery also uses **Sales invoices** |
| Recorded invoice matching | XML **MatchSets** download access, **Crm accounts**, **Financial accounting**, **Organization administration** |
| Cashflow posting details and G/L reports | **Financial accounting**, **Financial generalledgers**, **Organization administration** |
| Reconstructed bank queue | **Financial accounting**, **Financial generalledgers**, **Financial payables**, **Financial receivables**, **Organization administration** |
| Approved browser expense classification | **Financial accounting**, **Financial generalledgers**, **Organization administration**, XML **MatchSets** download access, plus the signed-in user’s normal UI bank-edit rights |
| VAT and financial periods | **Financial accounting** |
| Journals / journal-period locks | **Financial generalledgers**; period locks also use **Financial accounting** |
| Payment conditions | **Financial cashflow** |
| Cost centres and cost units | **Financial costcenters** |
| Projects | **Projects projects** |
| Items, units and supplier-item links | **Logistics items**; supplier-item queries also use **Crm accounts** |

An unsupported package or denied permission returns an error, not an empty catalog. Optional project and supplier-item features are not prerequisites for direct G/L booking preparation. Purchase-document intake reads Type-20 documents; it does not depend on the separate commercial `purchase/PurchaseInvoices` workflow.

## Sales-draft creation and editing

1. Resolve clients using `search_accounts`; explicitly choose bill-to (`account_id`) and ordering (`ordered_by_id`) companies. They may be the same UUID. These tools do not create clients.
2. Find articles/services using `search_items` and `get_item_details`. Select a revenue G/L using explicit item/group context or `search_gl_accounts`, a sales VAT code with `search_vat_codes`, and a sales journal with `search_journals`. Clarify ambiguous matches. These tools do not create articles.
3. Supply explicit quantity, standard unit and unit price in the invoice currency. `StandardSalesPrice` has unknown currency and is not a negotiated quote. No unit conversion, price-list lookup, discount inference or tax-eligibility decision is performed.
4. Call `create_draft_invoice`. `dry_run: true` validates and previews without saving; omitted/false saves directly when the user requested creation. There is no mandatory preview approval token. The new caller-chosen `request_id` becomes the commercial `InvoiceID`; retain it for reconciliation.

Example shape (replace UUID placeholders with verified selections; generate a new request UUID only for a genuinely new invoice):

```json
{
  "request_id": "<new request UUID>",
  "account_id": "<verified bill-to UUID>",
  "ordered_by_id": "<verified ordering-company UUID>",
  "journal_code": "<verified sales journal>",
  "invoice_date": "2026-11-12",
  "currency": "EUR",
  "description": "Consulting services",
  "lines": [{
    "item_id": "<verified service-article UUID>",
    "gl_account_id": "<verified revenue G/L UUID>",
    "description": "Custom consulting description",
    "quantity": "8",
    "unit_code": "<article standard unit>",
    "unit_price": "125.00",
    "vat_code": "<verified sales VAT code>"
  }],
  "dry_run": true
}
```

Exact requires an article even on a custom description/price line. Explicitly select an existing generic service article for that purpose; none is guessed or created. Creation supports 1–100 positive-quantity lines, nonnegative prices and zero discount, with no dimensions or unit conversions. Optional header fields include `your_ref`, `remarks`, `due_date` and an explicitly verified `payment_condition_code`. Omitted settings remain subject to Exact's defaults. VAT-code calculation type governs price interpretation; Exact calculates dated VAT, rounding, totals and default due dates. A preview is not an authoritative tax total or duplicate clearance.

“Draft” here means a normal sales invoice (`Type=8020`) in open/editable **status 20**. Exact's status 10 is an abnormal-entry recovery draft, not the normal creation target. Open drafts can be present in financial reporting; this workflow does not promise financial invisibility. No processing, printing, PDF generation, sending, credit notes, direct/stock invoices, purchase booking, matching or payments are exposed.

For edits, select an invoice with `search_draft_invoices` and retrieve `get_draft_invoice_details`. Pass its commercial `invoice_id`, bill-to `account_id` and **exact raw `modified` string** as `expected_modified`:

- `update_draft_invoice` changes only supplied description/reference/remarks/date/payment-condition fields. Omitted/null fields remain untouched; empty strings clear text. Company, currency, journal, type and status cannot be changed.
- `update_draft_invoice_line` changes one explicitly selected existing `line_id` with a complete explicit line specification. It resets discount to zero and leaves other lines out of the PUT. Adding/removing lines is not exposed.
- Both support `dry_run: true`. Processed, recovery, credit and direct invoices are rejected. Stale/unknown Modified evidence is rejected. Reads are checked again before one PUT, but these are **observed checks, not an atomic cross-user lock**.

Writes never retry automatically, including on HTTP 429, authorization errors, timeouts or failed readback. Successful results include verified readback details and Exact's totals. `outcome_unknown` means the write may have happened: inspect that same InvoiceID before doing anything else, never silently retry, roll back or create a fresh request UUID. An existing request UUID is rejected without another POST. This is repeat-request identity, **not a business duplicate-invoice check**. Tests use invented local mocks; live writes/package-specific behavior have not been validated by these tests.

API references: [SalesInvoices](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=SalesInvoiceSalesInvoices), [SalesInvoiceLines](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=SalesInvoiceSalesInvoiceLines).

## Bank queue setup

First use `search_gl_accounts` to identify and verify the administration’s **unassigned-bank** G/L accounts. Their role cannot be inferred reliably from a name, generic account type or universal code. Save the selected UUIDs locally:

```sh
nix run . -- bank-config set --unassigned-gl-account-id <verified-gl-uuid>
nix run . -- bank-config show
```

Repeat `--unassigned-gl-account-id` for multiple accounts (maximum ten). `set` validates account metadata through Exact GET requests and saves only local settings. `show --json` inspects them without contacting Exact; `bank-config clear` removes only the current scope’s settings.

Then call `get_bank_transactions_to_handle` with `{}`. Explicit `unassigned_gl_account_ids` override settings for that call only. Configuration is scoped by Exact origin, application and administration; changes take effect without restarting MCP. There is no configuration-write MCP tool.

The queue combines configured unassigned postings with current open payable/receivable bank postings, following explicit offset links to bank control lines. Inspect `unresolved_items` and the reported reconstruction basis: an empty page with unresolved evidence does not mean nothing needs handling. Accounting status 20 is not a queue predicate. No assignment, matching or handling is executed.

For read-only proposals, duplicate coverage, preflight checks and the separately approved browser executor, see [booking preparation and bank handling](booking.md). Browser execution uses an expiring preview and confirmation in the conversation, not a terminal prompt.

## Reading results correctly

- Resolve ambiguous companies, invoices and attachments explicitly, rather than taking the first match. Reuse the returned UUIDs and accounting identity for drill-down. A commercial sales `InvoiceID` is not an accounting `EntryID`.
- Open balances use Exact’s report currency, which can differ from the original invoice currency. Transit amounts are separate, not subtracted from balances or treated as settlement. Aging uses due dates: due today is current; missing dates stay unknown.
- Purchase `invoice_number` is Exact’s entry number; `your_ref` is the supplier reference when available. Normal negative purchase headers do not establish credit notes, and reverse-charge VAT can make header and line totals differ.
- Recorded matching uses the current XML MatchSets export, not an immutable matching/unmatching event log. Multi-member groups do not establish pairwise invoice-to-payment allocations. Cashflow posting amounts are full postings, not invoice-specific allocations; posting dates are not clearing dates.
- Purchase documents are Type 20 only. Missing accounting references or empty explicit-link results do not prove uniqueness or an unbooked invoice. Proposal statuses are workflow metadata, not payment status. No OCR or proposal generation is performed.
- Commercial sales statuses 10 (recovery draft) and 20 (open/editable) are distinct from financial-entry statuses 20/50. Journal-period locks use another scheme: 0=open, 1=closed; unknown stays unknown. An open period is not booking approval.
- Master data is current context, not historical terms or a complete preflight. Preserve blocked/expired/unknown restrictions and dated VAT rates; do not invent an effective G/L account from item defaults.
- Follow `has_more` / `next_offset` with unchanged filters. Response limits often reduce returned rows, not source API reads. Counts/totals cover the tool’s full filtered source selection; failed pagination or validation returns an error rather than partial successful totals. Separate calls can observe different source states.

## Storage, downloads and failures

Tokens live in plaintext `session.json`, not Keychain. On Unix, storage uses `0700` directories and `0600` files; equivalent non-Unix ACL protection is not implemented. A cross-process lock serializes token rotation. Refresh is on demand, persisted before API reads proceed, and drained during graceful MCP shutdown; a crash after rotation may still require login again. Token exchanges are never retried automatically.

Ordinary GET requests retry HTTP 429 at most twice, with at most 60 seconds of cumulative retry sleep per request/page. This is not a whole-tool deadline or a global rate scheduler. Sales-draft POST/PUT requests never retry, even on HTTP 429. Other HTTP/transport errors are not retried automatically. If refresh fails, verify the runtime client secret and authorize again if needed. `auth status` alone does not verify live token acceptance.

PDF downloads fetch only an explicitly selected existing attachment, using checked associations and a fixed authenticated endpoint without redirects. Files are limited to 20 MiB, checked for PDF markers, and saved without overwriting under `<data-dir>/downloads/<division>/`. Returned paths refer to the MCP server’s machine. Files are not opened automatically and remain until removed; marker checks are not authenticity verification or malware scanning. Repeated downloads create new files.

There is no reusable financial query cache. Browser approval plans and audit records contain private frozen financial snapshots under `<data-dir>/bank-ui/`; they are not a general query cache, remain sensitive and must not be shared raw. See [the browser workflow](booking.md#conversational-preview-and-confirmation) for approval, cancellation and uncertain-outcome safeguards. API pages are restricted to the saved origin’s `/api/v1/` path. Financial source text and downloaded content can be sensitive and remain untrusted even when retrieved successfully.

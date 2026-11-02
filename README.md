# exact-mcp

Local-first Exact Online MCP server with OAuth, explicit administration selection, company/invoice and unprocessed commercial sales-invoice search, outstanding-balance and aging reports, invoice accounting details, recorded matching allocations, cashflow posting drill-down, G/L period reports and existing invoice PDF downloads. Exact financial operations are read-only; no invoice creation, printing, sending or payment tools are exposed. Downloading writes a private local file only.

## Setup

Use Nix with flakes enabled, or Rust 1.89 or newer.

1. Register an application in the [Exact Online App Center](https://apps.exactonline.com) and obtain its client ID and client secret.
2. Register this redirect URI **exactly**, including the port and path:

   ```text
   https://exact-mcp.localhost:8400/callback
   ```

3. Make sure the authorizing user/app has access to the administrations you need. Setup requires **Organization administration**. Financial tools additionally require **Crm accounts**, **Financial payables**, and **Financial receivables** for the corresponding queries. Invoice accounting drill-down additionally requires **Financial accounting**; it also uses **Organization administration** to resolve the default currency. Existing outstanding-balance tools do not require the additional accounting scope. PDF discovery/download also requires **Organization documents**, and receivable PDF discovery additionally uses **Sales invoices** to resolve the original commercial invoice UUID. Payment drill-down and G/L lookup/reporting additionally require **Financial generalledgers** for journal or chart metadata. G/L reports also use **Financial accounting** and **Organization administration**. Existing invoice/matching/company-search tools do not need the extra generalledgers scope.
4. With Nix, run the CLI directly (the first invocation builds it):

   ```sh
   nix run . -- --help
   ```

   Without Nix, build it with `cargo build --locked`.

Set the credentials in your shell. For example, in Bash, enter the secret without putting it in shell history:

```bash
export EXACT_CLIENT_ID='your-client-id'
read -r -s -p 'Exact client secret: ' EXACT_CLIENT_SECRET
export EXACT_CLIENT_SECRET
printf '\n'
```

The CLI also loads `.env` from the current working directory before starting Tokio. Already-exported environment variables take precedence. `.env` is ignored by version control and excluded from the Nix build source; keep it private. The CLI never saves the client secret itself. Both normal commands and the Pi launcher use these credentials for automatic token refresh.

## Login and select an administration

```sh
nix run . -- auth login
```

The CLI automatically generates a self-signed certificate for the callback hostname. No certificate files, mkcert, or local CA installation are needed. When the browser redirects to `https://exact-mcp.localhost:8400/callback`, accept the certificate warning for this local address only. The certificate and key stay in memory and are regenerated each login, so the browser may ask again next time. If your browser policy prevents certificate exceptions, use a browser-trusted certificate through the optional settings below.

The hostname must resolve to `127.0.0.1`; if your browser does not resolve `.localhost` names automatically, configure that mapping locally.

Open the printed URL in your browser and authorize the application. The CLI serves HTTPS directly on the IPv4 loopback interface for up to five minutes and validates the callback's random OAuth state. TLS uses rustls with the ring provider, both for the callback server and outgoing Exact API requests; OpenSSL and aws-lc are not enabled. No public server, tunnel or reverse proxy is needed. The live App Center/login flow has not been verified against an account yet.

Then:

```sh
nix run . -- administrations list
nix run . -- administrations select 123456
nix run . -- auth status
```

Replace `123456` with a code from the list. Selection is checked against accessible administrations before saving. Listing never selects an administration automatically. Logging in again clears the previous selection, so it cannot silently carry over to another user.

`auth status` only inspects local storage; it does not contact Exact or prove the refresh token is still accepted. `administrations list --json` returns machine-readable administration records.

Without Nix, use `cargo run -- <command>` or `./target/debug/exact-mcp <command>` instead.

## Use as an MCP server

After logging in and selecting an administration, build once so MCP initialization does not have to wait for compilation:

```sh
nix build
nix run . -- serve
```

`serve` uses stdin/stdout exclusively for MCP protocol messages. Startup errors go to stderr. It requires a stored login and selected administration but does not contact Exact until a tool is called.

### Launch Pi directly

With Pi installed on `PATH`, run this from the repository directory:

```sh
nix run .#pi
```

This builds the MCP binary, loads credentials from the environment or the repository's `.env`, registers its exact Nix store path as the project-local `exact` server with direct tool exposure, and starts a fresh interactive Pi session. The secret is inherited through the process environment, not saved in the MCP configuration or requested again at a prompt. Existing Pi credentials, models and user-level configuration are reused unchanged.

The generated `.pi/mcp.json` is ignored by version control, and unrelated project MCP entries are preserved. Accept Pi's project-trust prompt if shown. Use `/mcp` inside Pi to verify that `exact` is connected with fourteen tools. Additional Pi options are forwarded, for example `nix run .#pi -- --name "Exact MCP"`.

### Other MCP clients

For MCP clients using a `mcpServers` configuration:

```json
{
  "mcpServers": {
    "exact": {
      "command": "nix",
      "args": ["run", "/absolute/path/to/exact-mcp", "--", "serve"]
    }
  }
}
```

Ensure the MCP host can find `nix` and inherits `EXACT_CLIENT_SECRET` for automatic token refresh. Desktop clients may not inherit your interactive shell environment; configure their runtime environment securely. Do not put credentials in tracked files. You can alternatively launch `result/bin/exact-mcp serve` directly after `nix build`.

### Financial tools

- `search_accounts`: `{ "query": "Company X", "limit": 10 }`. Searches name fragments within the selected administration; returns UUIDs, account codes, names, supplier flags and whether sales are allowed. Set `match_by` to `code` for an exact account-code lookup ignoring surrounding padding, or `id` for a UUID lookup; the default is `name`. Exact pads account codes with spaces; returned codes preserve that source value. Limit is 1–50. When `has_more` is true, repeat the same query, field and limit with `offset` set to `next_offset`. Ambiguous matches must be clarified, never silently resolved to the first result. Results use a stable name/code/UUID ordering, but searches are not frozen snapshots: changes between calls can move matches. Offsets are applied while following Exact's pagination links, so later searches reread earlier pages rather than relying on server-side offset support.
- `search_invoices`: `{ "account_id": "<verified company UUID>", "side": "payable", "query": "SUPPLIER-REF", "entry_date_from": "2026-01-01", "entry_date_to": "2026-12-31" }`. Finds recorded purchase/sales accounting entries regardless of whether they remain open. Resolve the company first with `search_accounts`. All filters are optional except account/side. `query` is a literal Unicode-lowercase substring of `YourRef` or `Description`, or the exact decimal rendering of `EntryNumber`/`InvoiceNumber`; leading zeroes in textual references are preserved and `%`/`_` are not wildcards. Date bounds are inclusive accounting `EntryDate` calendar dates, not due/matching/clearing dates; unknown dates are excluded only when a date bound is active. Reads all company header pages before local filtering, so large company histories require additional API calls. Returns `source_entry_count`, `matched_entry_count`, up to `limit` entries (default 10, max 50), `has_more` and `next_offset`. Repeat unchanged filters with that offset. Ordering is entry date descending (unknown last), entry number descending, trimmed journal ascending, UUID ascending; source changes can move matches between calls. There is no paid/unpaid filter, no open-balance inference and no search of commercial drafts without accounting entries. It requires **Crm accounts**, **Financial accounting** and **Organization administration**.

- `search_draft_invoices`: `{ "account_id": "<verified bill-to company UUID>" }`. Finds unprocessed commercial sales invoices; optional `status: 10` selects recovery drafts and `status: 20` selects open/editable invoices. Both are included by default. This is separate from posted accounting-entry search.
- `get_draft_invoice_details`: `{ "account_id": "<same company UUID>", "invoice_id": "<selected commercial InvoiceID>" }`. Reads the unprocessed commercial header and all lines using GET only.

- `search_gl_accounts`: `{ "query": "Services" }` searches a literal, Unicode-lowercase description fragment. Use `{ "query": "004000", "match_by": "code" }` for an exact code ignoring surrounding padding but preserving leading zeroes. Reads/validates all chart pages before matching, orders by trimmed code then UUID, and returns `matched_account_count`, account UUIDs, `has_more` and `next_offset`. Limit defaults to 10, maximum 50. Blocked accounts are included for historical reports; ambiguous results must be clarified, not silently selected. Requires **Financial generalledgers**.
- `get_ledger_report`: `{ "gl_account_id": "<selected G/L UUID>", "financial_year": 2026, "period_from": 1, "period_to": 3 }`. Returns transaction rows and signed default-currency **period movement**, not an opening/closing balance. Financial year/period bounds are required; periods are not assumed to be calendar months. Optional `status: 20` (Open) or `status: 50` (Processed) narrows financialtransaction status; omit it for all source statuses. These are not payment statuses. Totals cover all source pages before row pagination; `totals_by_status` retains separate status subtotals. Transaction `limit` defaults to 50, maximum 200; use `next_offset` with unchanged arguments for further rows. `summary_only: true` omits `transactions` and ignores row pagination without reducing source reads/totals. Requires **Financial generalledgers**, **Financial accounting** and **Organization administration**.

- `get_outstanding_balances`: `{ "side": "receivable" }`. Summarizes all companies with matching outstanding items in the selected administration, without a company lookup. Use `payable` for amounts owed to suppliers. Returns account UUIDs, codes and names, per-account totals and overall totals by reported currency. Supports the same `due_date_from`, `due_date_to` and `overdue_only` filters as `get_open_items`. It always returns summaries, not invoice rows. Use a returned UUID with `get_open_items` to drill down.
- `get_open_items`: `{ "account_id": "<UUID from search_accounts or get_outstanding_balances>", "side": "payable" }`. Use `receivable` for amounts owed by the company. Returns all pages of current open items with invoice/entry references, due dates, descriptions and per-currency totals. Optional `due_date_from` and `due_date_to` are inclusive `YYYY-MM-DD` due-date bounds; `overdue_only: true` selects items due before the local `as_of_date`. These filters intersect, and any active due-date/overdue filter excludes items without a due date. Set `summary_only: true` to omit the `items` field while retaining complete matching totals.

- `get_invoice_details`: `{ "account_id": "<UUID>", "side": "payable", "entry_number": 12345, "journal_code": "60" }`. Use the account UUID, side, entry number and journal code from the **same** `search_invoices` or `get_open_items` result; include `entry_id` from invoice search when available. Accounting numbers can be reused across years, so the optional UUID disambiguates the exact header while account/number/journal are still checked; the example numbers are placeholders, not values to guess. Returns a purchase (`payable`) or sales (`receivable`) accounting header, all entry lines, both currency contexts and the header's linked-document reference. It does not download documents or determine current open balances.

- `get_invoice_matches`: the same `{ "account_id", "side", "entry_number", "journal_code" }` inputs and optional `entry_id` as `get_invoice_details`, using verified references (including closed invoices). Returns recorded matching groups with signed default-currency allocations and counterpart ledger evidence. Requires XML `MatchSets` download access plus **Crm accounts**, **Financial accounting** and **Organization administration**. This is not a match/unmatch event audit, current open balance, or proof of bank settlement.

- `get_bank_transactions_to_handle`: `{ "unassigned_gl_account_ids": ["<verified unassigned-bank G/L UUID>"] }`. Reconstructs bank transactions to handle from configured unassigned postings and current open AP/AR bank postings, following explicit same-entry offset references. Unresolved open-item links stay visible; this is not a provider completion-state flag. See the bank-queue section below.

- `get_payment_details`: `{ "account_id": "<same verified company UUID>", "transaction_line_id": "<explicitly selected cashflow candidate id>" }`. Use `get_invoice_matches` → `ledger_evidence[].candidates[]`; select a source `type_code: 40` cashflow line and pass its **`id`**, not `entry_id` or a payment-term UUID. Never silently select an ambiguous matching candidate. Returns full posting amounts/currencies, references, posting date, source status/type and current journal metadata. It requires **Financial accounting**, **Financial generalledgers** and **Organization administration**; no payment actions or writes are exposed.

- `list_invoice_documents`: the same `{ "account_id", "side", "entry_number", "journal_code" }` inputs and optional `entry_id` as `get_invoice_details`. Returns linked document types/subjects, association provenance, attachment IDs and filename-only PDF hints without downloading bytes. `invoice_type_match` is true for Exact purchase document type 20 or sales type 10; reminders and other types remain distinguishable and cannot be downloaded as invoices.
- `download_invoice_pdf`: `{ "invoice": { "account_id": "<UUID>", "side": "payable", "entry_number": 12345, "journal_code": "60" }, "document_id": "<UUID from catalog>", "attachment_id": "<UUID from that document>" }`. Revalidates the selected invoice/document/attachment, downloads one existing PDF and returns its private local path. Never guess IDs or silently pick the first of multiple eligible originals/PDFs.

Open-item results echo `filters`, `summary_only`, `source_item_count` (before filtering), and `matched_item_count` (after filtering). **All totals cover only matching items**, never the excluded balance. Filters and summary mode do not reduce source API reads: all pages are fetched and checked before totals are returned. For example, a compact overdue receivables query is:

```json
{
  "account_id": "<UUID from search_accounts>",
  "side": "receivable",
  "overdue_only": true,
  "summary_only": true
}
```

### Administration-wide balances

`get_outstanding_balances` reads Exact's dedicated `PayablesList` or `ReceivablesList` endpoint, rather than searching or requesting every company separately. For example:

```json
{ "side": "receivable", "overdue_only": true }
```

This answers "Who owes us overdue money?"; use `side: "payable"` for "Who do we need to pay?". Due-date bounds can select upcoming balances, such as payments due this month.

The `accounts` array contains `{ "account": { "id", "code", "name" }, "totals_by_currency": { ... } }` summaries. Each currency total includes item counts, signed net/positive/negative balances, signed overdue balance, due-date aging buckets, separate known transit and unknown-date/transit counts. Accounts with matching rows remain visible even when their signed net balance is zero. These summaries do not invent supplier/customer flags or classify negative rows.

Account summaries are ordered by name, code and UUID. `limit` is 1–50 (default 50); repeat the same side, filters and limit with `offset: next_offset` while `has_more` is true. **Overall `totals_by_currency` includes every matching account across all source pages, not just the summaries returned on the current page.** Per-account totals include that account's matching rows. `source_item_count`/`source_account_count` describe the complete source report before filtering; `matched_item_count`/`matched_account_count` describe the complete filtered report before response pagination. A page beyond the end has no account summaries but retains complete matching totals and counts.

Each call fetches the full source report before filtering, grouping and pagination. Later calls reread it, so changing source data can move accounts between pages; this is not a frozen snapshot. Duplicate rows, missing account/currency information, conflicting names/codes for the same UUID, or decimal overflow fail the whole request instead of returning partial totals. Payable/receivable sides and report currencies are never offset; transit is never subtracted from balances or treated as settlement.

The server is bound to the stored account configuration and administration at startup. If either changes, subsequent tools fail with a restart instruction rather than silently reading another administration.

The CLI and MCP share one multithreaded Tokio runtime and async reqwest clients. MCP requests reuse one connection pool, and independent API reads can overlap. HTTPS callbacks use async tokio-rustls with ring. Session locks cover short state snapshots/updates and token exchanges, never ordinary API reads. Lock contention yields asynchronously; only bounded filesystem operations (including atomic save/fsync) use Tokio's blocking pool.

### Invoice accounting drill-down

Ask "What is on this invoice?", "Show its ledger lines and VAT", or "What was its original transaction-currency amount?" after resolving the company and selecting an outstanding item. The tool reads `PurchaseEntries`/`PurchaseEntryLines` or `SalesEntries`/`SalesEntryLines` using GET only. It requires one uniquely matching header for the account, journal and entry number, then validates every line's entry UUID and administration. Missing or ambiguous headers, wrong-account/administration responses, duplicate line UUIDs and failed source pagination return an error, not partial details. Bank/cash open items may not resolve to an invoice entry; there is no guessed fallback lookup.

Results identify `amount_basis: "exact_accounting_entry"`. `default_currency` is fetched from the selected administration; `entry.currency` is the original transaction currency. Header and line fields ending in `_default_currency` use the former, while those ending in `_transaction_currency` use the latter. Amounts remain decimal strings with source signs unchanged. `line_count` counts every returned entry line across all source pages, ordered by line number and UUID. Ledger codes/descriptions, VAT codes/base amounts, raw VAT percentage, quantities and rate are returned where reported; missing fields remain `null`. Rate, quantity and VAT percentage are source numeric strings, with no server-side rescaling (for example, raw VAT percentage `0.21` is not returned as `21`).

**Accounting signs and VAT are not outstanding-balance semantics.** A normal purchase was observed with a negative header, positive expense line and positive open payable. Do not classify credit notes or payments from header signs. Header and line VAT can differ, including reverse-charge entries where header VAT is zero but line VAT is nonzero. Do not automatically add VAT to header totals, treat line VAT as VAT charged by the supplier, or sum header and lines together. Entry lines are not necessarily product-level invoice lines or a complete balanced ledger transaction dump: the UI can also show automatically generated control/VAT rows absent from the entry-line endpoint. Type, status and reversal are source metadata; retrieval alone does not imply the entry is processed or approved.

These entry amounts are **not remaining balances or proof of payment**; use `get_open_items` for current balances. `document_id`, `document_number` and `document_subject` describe only the header's linked document, not every document related to the transaction. A missing header reference does not prove no related documents exist, and a reference does not prove a PDF is available. No files are downloaded and no Documents permission is needed for this metadata-only scope. Header, default currency and lines are separate reads, not a frozen snapshot.

### Download existing invoice PDFs

Ask "Download the original PDF for this invoice" after selecting an invoice. The assistant first calls `list_invoice_documents` and then `download_invoice_pdf` with explicit document and attachment UUIDs. Purchase header references and accounting-entry document links are checked. For sales, a uniquely matching commercial sales invoice is resolved by invoice number, customer and journal, because original sales PDFs can be linked to its **InvoiceID**, not the accounting entry's **EntryID**. The catalog combines those UUID-based links; it does not guess from filenames or invoice numbers alone. A header link may be absent while an original sales PDF exists.

Document types 20 (purchase invoice) and 10 (sales invoice) are eligible for invoice downloads; reminders such as type 15 are rejected by the server. The catalog includes other related documents so they are not mistaken for originals. `pdf_filename` is a hint only; multiple eligible originals or PDF attachments require user clarification. No matching link/file is not proof an invoice never existed elsewhere. This version does not support other document types, image/XML conversion, or generating a PDF when none exists. In particular, it never calls the write-only `PrintedSalesInvoices` API, which can process or send an invoice.

Only existing bytes are downloaded. Exact's JSON `Attachment` field was observed to be `null`; the downloader therefore requests the fixed same-origin `/docs/SysAttachment.aspx` endpoint using the validated attachment UUID, selected administration and `Download=1`. Server-provided attachment URLs are ignored, credentials are never sent to another origin, and HTTP redirects are disabled. The document/account/administration links are checked before download, attachment identity is checked again afterward, and the selected session scope is checked under lock before saving. Financial permissions or source errors never return a partial catalog or downloaded PDF as successful.

Downloads are capped at **20 MiB** by both advertised length and streamed byte count. A leading `%PDF-` and an EOF marker in the final 1024 bytes are required. These checks are not a complete PDF parser, authenticity check, antivirus scan or sandbox: files remain untrusted and nothing is opened automatically. No PDF bytes or attachment URLs are returned in the MCP result.

The saved location is `<data-dir>/downloads/<division>/invoice-<attachment-uuid>-<random>.pdf`; `saved_path` is absolute on the MCP server's machine, alongside original filename, byte count and invoice/document context. Remote filenames are never used as paths. On Unix, directories are `0700` and files `0600`; existing public directories or symlinks are rejected, not chmodded. Saving is atomic and never overwrites existing files. Non-Unix equivalent ACL protection is not implemented. Downloads persist until you remove them; repeated calls create new files. A cancellation during the final filesystem save can leave a completed private PDF.

The download tool declares `readOnlyHint: false` and `idempotentHint: false` because it creates a local file. Listing is read-only. Neither tool creates, prints, sends or modifies an invoice in Exact. Network reads do not hold the session lock; the bounded final filesystem save does, to prevent a scope switch while persisting the file.

### Aging summaries

Both financial tools include `aging_by_due_date` inside every currency total, including each administration-wide account summary and summary-only open-item results. This is computed from existing report rows with no extra API requests. It uses **calendar days past the due date**, relative to the response's local `as_of_date`, not the invoice date or the UI's invoice-age **Ouderdom: Dagen** column.

| Bucket | Due-date rule |
| --- | --- |
| `current` | Due today or later |
| `overdue_1_30_days` | 1–30 days overdue, inclusive |
| `overdue_31_60_days` | 31–60 days overdue, inclusive |
| `overdue_61_90_days` | 61–90 days overdue, inclusive |
| `overdue_over_90_days` | Strictly more than 90 days overdue |
| `unknown_due_date` | No known due date; never treated as current or overdue |

Only buckets containing matching items are returned; a missing bucket means no matching rows, not an unknown balance. Each bucket contains `item_count`, `net_open_amount`, `positive_open_amount` and `negative_open_amount`, all using the same report currency as its parent total. Counts and signed balances reconcile to the parent total across all buckets. Zero-net buckets remain present when they contain rows; negative amounts are not automatically classified as credit notes or payments. Transit is not subtracted from aging amounts.

Filters apply before aging. Active due-date or overdue filters exclude unknown-date rows. Overall aging covers all matching accounts across all source pages, not just the account summaries on the current response page. Per-account aging covers that account's matching rows. Accounts remain name/code/UUID ordered, not ranked by debt or risk; retrieve all relevant pages before claiming a largest balance. Use a smaller account `limit` for compact detailed aging responses.

Open-item rows also include `days_overdue`: zero for due today/future, positive calendar days for overdue rows, and `null` for unknown dates. No historical snapshot or original-currency conversion is implied. Example questions:

- "Give me an aging breakdown of our outstanding receivables."
- "Which customers have amounts more than 90 days overdue?"
- "Show the aging of what we owe this supplier, without listing invoices."

### Invoice search

Invoice search returns original accounting amounts, not current balances: default-currency amounts use `default_currency`, transaction-currency amounts use each entry's `currency`, and signs are preserved. Normal negative purchase headers do not identify credits/payments. Accounting status is not payment status. Ask the user to select ambiguous results rather than choosing the first match. Pass the selected `account_id`, `entry_id`, `entry_number`, `journal_code` and result `side` to invoice details, matching and document tools; PDF download nests those fields under `invoice`. No files are downloaded by search. Live validation covered purchase/sales numeric searches, reference/date searches and exact-UUID accounting drill-down; regression fixtures are invented and live evidence stays outside the repository.

### Unprocessed commercial sales invoices

`search_draft_invoices` takes a verified `account_id` from company lookup, optional literal `query`, inclusive `invoice_date_from`/`invoice_date_to`, `status`, `limit` (default 10, maximum 50), and `offset`. Company means **InvoiceTo (bill-to)**, not OrderedBy. It reads and validates every company header page before returning matching counts and the requested result window, ordered by commercial invoice date descending (unknown last), invoice number descending, then commercial UUID. Reuse unchanged arguments with `next_offset`.

Exact distinguishes **10 recovery draft** (abnormally interrupted invoice entry) from **20 open/editable** (the default for new invoices, commonly called a draft). Both are included by default; pass `status: 10` or `20` to narrow the result. Processed status 50 is excluded. These are commercial lifecycle statuses, not accounting paid/unpaid statuses. An empty result does not prove no invoice exists outside this bill-to company/source selection.

Pass the explicitly selected `invoice_id` and the same `account_id` to `get_draft_invoice_details`. This UUID is commercial **InvoiceID**, not accounting EntryID. Details verifies the header remains unprocessed and returns all scoped, deduplicated lines in line-number/UUID order. Permission, identity, pagination or source failures return errors, never partial details. Header and lines are separate reads, not a frozen snapshot; source changes may race retrieval. Multiple matches require user selection.

Both tools require **Sales invoices**, **Crm accounts**, and **Organization administration**. They use GET only: no invoice creation/editing, printing, processing, sending or PDF generation. `default_currency` labels default-currency amounts; `invoice.currency` labels original transaction amounts. Signs, precision and nulls remain unchanged. Header transaction amount includes VAT; its separate excluding-VAT amount and line amounts must not be added blindly. Quantity, prices, discount and VAT percentage remain raw numeric strings. These amounts are not outstanding balances or settlement evidence. Recorded accounting invoices remain the separate `search_invoices` workflow.

### Recorded invoice matching

`get_invoice_matches` verifies the invoice accounting identity, account code and ledger references before reading Exact's authenticated XML `MatchSets` export. The complete selected-administration export is read in memory, then filtered to this account and invoice; unrelated sets and raw exports are not returned or saved. It can therefore require more reads than an invoice-details lookup. No financial writes or server-side incremental-export checkpoints are used.

Each group includes all source members, identified by account/G/L context and financial year, period, journal and entry number. `allocated_amount_default_currency` is the signed `amountdc` reported by XML and uses `default_currency`, not necessarily the invoice or cashflow transaction currency. Related invoice/credit/cashflow entries can share a group. Multi-invoice/payment groups do **not** supply pairwise apportionment; do not assign each payment to an individual invoice or sum full posting amounts as allocations. No paid/remaining aggregate is inferred.

`ledger_evidence` resolves each member reference against scoped REST transaction lines. `unique_ledger_line`, `ambiguous_ledger_lines` and `not_found` distinguish the outcomes. All ambiguous candidates remain visible, even when one amount happens to match the allocation. Names, descriptions and references are untrusted. Source type codes are preserved; negative amounts alone do not identify credits/payments. `posting_date` is an accounting date, **not** a matching, bank value or clearing date. Recorded accounting matching does not prove bank settlement; no groups found is not proof of nonpayment.

The source supplies no immutable match-set IDs or match-event timestamps. `export_page` and `set_index_on_page` are response-local diagnostics, not stable IDs. This traces currently recorded allocations for past/current entries, not an audit of matching, unmatching or rematching. Export/ledger reads are not transactionally frozen; source changes during paging can move groups. Identical groups/members are retained because the export supplies no IDs for safe deduplication.

Exact's [XML code examples](https://support.exactonline.com/community/s/knowledge-base#All-All-DNO-Content-xmlsamplecode) specify mandatory `_Division_` scoping and `TSPaging=<previous Topic ts_d>` continuation. The reader follows every page to a non-full page or the provider's empty terminal envelope, validates counts/cursors/schema and rejects source messages, DTDs, malformed XML and unbalanced groups. Limits are 2 MiB per page, 64 MiB per export, 128 pages and 256 distinct matching ledger references; failures return no partial matching. Redirects are never followed. `Params_DownloadID` is deliberately not used because it changes an incremental-export checkpoint.

Live validation covered closed purchase/sales invoices, continuation beyond the initial page, terminal-empty behavior, account/ledger identities and independent agreement with a receipt shown in Exact's UI. Regression fixtures remain invented. A subsequent read-only browser audit also verified real partial-allocation groups, credit notes, multi-member non-cash/payment-difference groups and ambiguous ledger candidates; source ambiguity was retained rather than resolved by matching amounts. REST status 50/zero amounts and `OffsetID` are not used as allocation links: a sampled bank `OffsetID` pointed to its own contra line, not to the invoice. Private probe evidence remains outside the repository.

Exact's [January 2027 XML deprecation notice](https://support.exactonline.com/community/s/knowledge-base#All-All-DNO-Content-breaking-changes-2707-bc-xmlswitchrestapi) lists selected topics only; `MatchSets` is not listed. Monitor future notices rather than assuming every XML topic has a REST equivalent.

### Matching history and independent settlement: investigation outcome

The public Exact REST catalog and relevant official endpoint descriptions were reviewed for an immutable matching/unmatching history or independent bank-settlement evidence. No documented source with those guarantees was identified; this is an investigation limit, not a claim that no private/provider-specific source exists.

| Candidate source | What it supplies | Why it does not close the gap |
| --- | --- | --- |
| XML MatchSets | Current recorded group membership and allocations | No immutable group/event IDs or match/unmatch timestamps; not a historical event stream |
| Cashflow Payments / sync PaymentTerms | Current payment-term status and references | Selected/processed/matched states are not bank-authenticated settlement; `EndDate` is the highest matched invoice date, not a clearing timestamp |
| sync PaymentTerms timestamp | Row version for incremental synchronization | Not a calendar timestamp or retained event history; a division move can reset it |
| BankEntries / BankEntryLines | Accounting entries for received statements, line dates and optional statement-document reference | Entries can also be created through the API; accounting status/date/reference alone does not independently authenticate bank settlement |
| BankEntries webhooks | Notifications of resource changes | Not documented immutable matching events or a retrospective audit |
| BankStatementUploads | Uploaded statement metadata | Currently documented for Belgian CODA files only; upload/processing status is not per-invoice settlement evidence |

No new history/settlement tool, local financial archive, webhook subscription or bank connection was added. Comparing future snapshots would show observed differences only, not reconstruct missing historical events or prove when matching occurred. A genuine immutable audit needs an authoritative event source and retention/identity guarantees. Independent settlement needs a verified bank-origin source plus explicit transaction/invoice linkage; a new bank integration remains a separate approval and design decision.

Sources: [REST catalog](https://start.exactonline.nl/docs/HlpRestAPIResources.aspx), [Payments](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=CashflowPayments), [sync PaymentTerms](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=SyncCashflowPaymentTerms), [BankEntries](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=FinancialTransactionBankEntries), [BankEntryLines](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=FinancialTransactionBankEntryLines), [BankStatementUploads](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=CashflowBankStatementUploads).

### Bank transactions to handle

`get_bank_transactions_to_handle` reconstructs the bank transactions requiring attention using two sources: postings on explicitly selected unassigned-bank G/L accounts, and current open payable/receivable bank postings. It follows **counterpart `OffsetID` → bank control transaction-line UUID**, then returns the control's signed amounts/date/currency and the directly linked booking lines. Different movements sharing one bank entry are kept separate. Full posting amounts are not invoice allocations, balances or proof of bank settlement.

Supply `unassigned_gl_account_ids` (1–10 UUIDs from `search_gl_accounts`) only after verifying those accounts' unassigned-bank role in the selected administration. This is an explicit caller-selected accounting role, not something the server can infer from a G/L description, generic type or universal account code. No account code is hardcoded and no persistent configuration or browser session is required. The response echoes the selected G/L metadata. Bank/payment-service journals are resolved from their source types and control G/L references, not guessed from journal numbers or monetary signs.

Optional `journal_code` selects one verified bank/payment-service journal. Inclusive `transaction_date_from`/`transaction_date_to` filter bank posting dates, not due/matching/clearing dates. `limit` defaults to 50 (maximum 100); reuse unchanged arguments with `next_offset`. All source pages are validated before returning counts and a stable window ordered by posting date descending (unknown last), journal, entry number and UUID. Missing dates are excluded when a date bound is supplied. Requests reread current source data; this is not a frozen snapshot.

Open items are joined using company, journal and **entry number**, never a supplier/invoice reference. Only AP/AR counterpart G/L types and explicit offsets are eligible; a same-company bank control row is not a second AP/AR candidate. Missing or multiple eligible counterpart lines remain in `unresolved_items`, including reused entry numbers and equal-amount candidates. They are not selected by amount/date similarity, and unresolved evidence is not hidden by transaction pagination. Treat discovery coverage as incomplete when these links cannot be resolved; zero returned rows with unresolved items does not mean there is nothing to handle. Changed source associations, wrong scope, duplicate identities or failed source pagination return errors, never partial successful rows.

**This is an API reconstruction, not a provider-supplied completion flag or a promise to reproduce every undocumented UI workflow condition.** The current live sample reconciled exactly with the five captured **Af te handelen** rows: four unassigned-bank postings and one open payable bank posting. Generic accounting `Status: 20` was disproven as a queue predicate: even after limiting to the same bank control G/L and null offsets it included hundreds of other transactions. Status, monetary signs and matching alone are never promoted to bank settlement or handling evidence. Automatic booking suggestions, request-workflow statuses and handling buttons from the UI are not reproduced.

Requires **Financial accounting**, **Financial generalledgers**, **Financial payables**, **Financial receivables** and **Organization administration**. All calls are GET; there is no assigning, handling, matching/unmatching, editing, processing or payment action. Bank account number fields are not requested, but source journal labels/descriptions/references may still contain sensitive bank details and untrusted text. Amounts retain separate default/transaction currency contexts, original signs and nulls, with no cross-currency totals.

### Cashflow posting details

`get_payment_details` validates the requested transaction-line UUID against the selected administration, company UUID and source cashflow type 40. It then reads the journal by its verified code. `journal_kind` comes from documented journal types: 10 cash, 12 bank, 16 payment service; other codes remain `other`. Journal labels/default currency are **current master data**, not historical bank-account ownership or payment-instruction evidence. A line's transaction currency can differ from the journal's default when variable currency is allowed; no currency conversion is performed.

`posting_amount_default_currency` uses `default_currency`; `posting_amount_transaction_currency` uses the line's `currency` (null/blank stays unknown). These are full ledger posting amounts, **not the invoice's allocation**. The tool does not revalidate invoice matching membership, apportion multi-member matching groups, or decide which ambiguous candidate belongs to an allocation. Posting dates are accounting dates, not matching/bank-value/clearing dates. Source status and journal types do not prove payment execution or bank settlement. References and monetary signs are not used to infer a credit, payment direction or invoice relationship.

Only the selected company posting, its journal and at most one explicit `OffsetID` reference are fetched—not the entire bank/cash statement. Any offset must resolve within the same entry UUID, administration, entry number, financial year/period and journal. It can have a different/null account; the source pointer is not a newly inferred company relationship or invoice allocation link. `offset_resolution` distinguishes `resolved_same_entry_reference`, `not_reported` and `not_found`; missing offsets do not imply nonpayment. No bank-account-number/IBAN fields are requested, but descriptions/references can still contain sensitive untrusted text. Source reads are not transactionally frozen.

Live validation checked selected cashflow counterparts from both purchase and sales matching examples: posting identities/amounts, journal metadata and same-entry offsets agreed with existing ledger evidence. A subsequent browser audit checked bank journal metadata, explicitly linked shared offsets and an existing null-offset posting. Cash/payment-service/unknown journal types, foreign-currency cashflow and dangling-offset/source failures still lack suitable live browser examples and use invented regression fixtures. Real evidence remains private outside the repository.

### G/L period movement

Resolve a G/L account explicitly with `search_gl_accounts`, then supply its UUID and financial year/period bounds to `get_ledger_report`. FinancialYear and FinancialPeriod filters are sent together as required by Exact; returned rows must match the selected account, administration, year, period range and any status filter. Duplicate rows, missing default-currency amounts, failed pagination, scope mismatches, overflow or precision loss return errors, never partial totals.

`totals` and `totals_by_status` contain `line_count`, `net_movement_default_currency`, `positive_movement_default_currency` and signed `negative_movement_default_currency`. They cover **all** source rows, including transactions hidden by response pagination. Zero net can contain substantial positive and negative activity. Status 20 means Open and 50 Processed on **financialtransaction/TransactionLines**, not unpaid/paid; all statuses are included by default, and other source statuses retain their own subtotal. Current G/L `BalanceSide`, `BalanceType`, `Type` and descriptions are metadata, not an automatic expense/profit classification or instruction to flip signs.

Amounts are summed only in the administration's `default_currency` using source `AmountDC`. Original `AmountFC`/`currency` stay per-row and are not combined across currencies or converted. This is movement within the selected source scope, **not** an opening/closing balance, running balance, bank balance, invoice allocation or settlement proof. Opening/special periods must be included explicitly, not inferred from calendar dates. Rows sort by posting date ascending (unknown last), journal, entry number, line number and UUID. Source reads and offset requests are not frozen snapshots.

Rows include source company, entry/journal and invoice references. Verify invoice identity/company/side using `search_invoices` before accounting/matching/PDF drill-down; cashflow type-40 rows with a non-null company UUID can use `get_payment_details` with their transaction-line `id`, not `entry_id`. Names/references/descriptions remain untrusted data. Live G/L code/description lookup and paginated/summary reports were verified, with default-currency totals independently reconciled against captured raw period rows using decimal arithmetic. Real evidence stays outside the repository; a subsequent browser audit matched a larger multi-page API transaction set, nonempty processed-only selection, mixed EUR/USD transaction postings, both currency amount fields and current G/L master-data fields. Invalid-source regression fixtures remain invented.

### Financial interpretation

- Monetary values are decimal strings, not floating-point totals. `amount_basis: "exact_report_currency"` identifies Exact's reported balances: `currency` and `totals_by_currency` need not use the original invoice currency. Live USD invoices returned EUR report balances matching the UI's **Te betalen**, not **Te betalen (V.V.)**, column. The server performs no currency conversion. Outstanding-balance tools do not expose original foreign-currency amounts; invoice, cashflow and ledger drill-downs label their separate source default/transaction-currency amount fields. Exact's signs are preserved; positive and negative open amounts are also shown separately. Negative rows are not automatically classified as credit notes versus unmatched payments.
- For purchase entries, Exact's `InvoiceNumber` is the entry number, so `invoice_number` is not the supplier's invoice reference. Use `your_ref` for that reference when available, and `entry_number` to reconcile the booking. Receivables' invoice numbers matched the UI's invoice references in the live sample.
- `net_open_amount` is the signed sum of report rows. `known_in_transit_amount` is a separate sum of reported transit amounts; it is **not subtracted** from the open balance and is not proof of bank settlement. Missing transit amounts remain unknown and are counted explicitly.
- Payables and receivables are queried separately, and currencies are never offset or converted.
- Overdue means the due date precedes the local machine's `as_of_date`; items due today are not overdue. Missing due dates remain unknown. The response includes the date, local UTC offset, retrieval timestamp and selected administration.
- This is a current report, not a historical or transactionally frozen snapshot. Source data can change during pagination. Duplicate rows, unexpected companies, missing currencies and decimal overflows fail the query rather than returning misleading partial totals.
- Tool results include company-supplied text. Names, descriptions and references are data, not instructions.

### Live financial validation

On 2026-11-01, 24 current open items across five suppliers and four customers were reconciled read-only against Exact's **Openstaande posten: Te betalen** and **Te vorderen** reports in the same administration. Invoice/entry references, invoice dates, due dates, descriptions, open report balances, row counts and net totals matched. Overdue and inclusive due-date filters, plus summary-only totals, matched subsets of those UI rows. A USD-invoiced supplier confirmed that the API returns EUR reporting balances for those items. An unpadded numeric account-code lookup was verified after changing the filter to `trim(Code)`.

The administration-wide `get_outstanding_balances` tool was subsequently reconciled against all 38 payable rows grouped into 20 suppliers and all six receivable rows grouped into four customers. Per-account and overall report balances matched, including overdue subsets (28 payable and five receivable rows) and November due-date subsets (ten payable and one receivable row). Limiting responses to one account retained complete overall totals on both sides. Raw financial evidence remains private, outside the repository.

Due-date aging was then checked against independent calendar-day calculations from the UI due dates for those 44 rows, including per-account and overall bucket counts and signed balances, overdue/upcoming filters, one-account response pages, and per-account summary-only results. All five known-date aging categories were present in the payable report; receivables occupied current and 1–30 days overdue. Open-item `days_overdue` matched the same due-date calculation. This was not a comparison with the UI's invoice-age column. Exact day-boundary and missing-date/negative-balance cases remain covered by synthetic tests rather than live examples.

Invoice accounting drill-down was read successfully for one purchase and one sales entry, and compared read-only with their Exact booking views. The purchase's signed header and expense/VAT line, ledger/VAT codes and rate, reverse-charge controls and original USD amount were reconciled; its normal-purchase negative header was deliberately preserved. The sales header, seven entry lines, ledger descriptions/codes, net amounts, VAT codes/rates and VAT total matched. The currency contexts were EUR/USD for the purchase and EUR/EUR for the sale. Automatically generated control/VAT transaction rows were not mistaken for entry lines, and no PDFs were accessed. These samples do not validate all invoice types, credit/reversal cases, entry statuses or multi-page invoice-line reports live.

Existing invoice PDF discovery/download was subsequently verified on both samples. The purchase had one purchase-invoice PDF. The sales lookup found the original sales-invoice document through the commercial InvoiceID as well as separate reminder documents; only its sales-invoice PDF was selected. Both PDFs were saved privately outside the repository, with size, PDF markers and Unix file permissions checked. No invoice was generated, printed or sent and no PDF content was included in a tool response or test fixture. This does not validate every document type, country endpoint, large PDF, historical invoice or multi-page document catalog live.

This validates the observed current balances and sampled accounting entries, not every financial case. The full visible reports contained 38 payable and six receivable rows, with no negative balances. The queried sample had no nonzero in-transit amounts. Those initial samples did not cover negative/credit rows, partial allocations, nonzero transit, missing due dates, multiple reporting currencies or large source reports. The subsequent audit below closes several of those gaps without treating synthetic tests as live evidence. Neither the open-items report nor a zero transit amount proves bank settlement. No company identifiers, invoice references or real balances are committed as fixtures.

#### Subsequent read-only browser audit

The browser session was restored and fresh MCP responses collected when the visible balances differed from the earlier snapshots. Existing records only were inspected; no entries/master data were saved, matched/unmatched, created, deleted, printed, sent or paid. Full evidence, identifiers, references and balances remain private outside the repository.

- Current payable UI rows reconciled with per-company and overall signed net/positive/negative balances, including real negative open items. Their meaning was not inferred from signs. Aging buckets were independently recomputed from UI due dates, including negative buckets. A real due-today item was current and excluded by the UI overdue filter; the complete overdue row set and totals matched.
- Closed purchase and sales search results matched browser accounting identities, references, dates and original accounting amounts. The purchase's normal negative accounting header was compared with its control posting, not flipped to the positive amount shown by the generic search summary.
- Purchase and sales cashflow postings, explicit same-entry offset UUIDs/amounts/dates and bank-journal metadata matched. A batched bank entry demonstrated that its shared bank offset is not an individual invoice allocation. An existing source-null offset posting matched the browser without inventing an offset relationship or nonpayment claim.
- A batched-payment example had many candidate ledger lines for the same matching entry reference; the entire candidate UUID set matched the UI and remained ambiguous. A three-entry non-cash group contained two browser-labelled sales credit notes and a correcting journal entry with opposite ledger candidates. Another group included a browser-labelled **Betalingsverschil** posting; its cause was not assumed to be exchange rates and it was not treated as cashflow type 40. A purchase invoice with two recorded cashflow allocations matched both source groups and the UI postings; this does not prove bank settlement or matching time.
- G/L code/description/type, balance-side/type and blocked-state fields matched the corresponding existing master-data view without saving it. All transaction references/dates/journals and signed totals in a larger multi-page API report matched the UI. Nonempty processed-only rows matched with **Na boeken** disabled. A mixed EUR/USD expense-ledger sample matched default-currency totals and a selected transaction's separate default/foreign amount and currency fields. The balance-sheet view's **Totaal** included opening balance, whereas the sampled profit/loss view displayed no opening row; only period movement was compared, not an assumed closing balance.

Some source API amounts retain sub-cent precision while the UI shows two EUR decimals. Checks distinguish raw equality from equality at that visible precision. MCP values were not rounded, normalized or altered to force agreement, and full posting amounts were never substituted for allocated invoice amounts.

Remaining live gaps include nonzero transit, unknown due dates and multiple reporting currencies in outstanding reports; eligible company cash-journal and non-EUR cashflow postings; payment-service journals; true reversal/storno and reused invoice-number identities; dangling offsets and invalid/missing-source fault cases. The bounded availability checks found no current outstanding unknown due dates/nonzero transit, only EUR outstanding report currency, no eligible company cash posting, no non-EUR cashflow posting and no configured payment-service journal. Cases not found remain unverified, not assumed correct from unrelated examples. Large PDFs, multi-page document catalogs, other country endpoints and deliberate cancellation/rate-limit/security faults were not induced in production. An immutable matching-event audit and bank-settlement evidence are outside these tools' supplied data.

No new browser warnings or uncaught page exceptions appeared after the resumed baseline; pre-existing browser warnings/exceptions were retained, not treated as fixed. Local MCP stderr stayed empty. Hosted Exact server logs were unavailable. An earlier browser/authentication interruption was recorded as a capture gap and verification resumed from a fresh baseline. Only verification documentation changed; no application behavior was changed to fit the UI.

#### Follow-up availability checks and remaining blockers

A subsequent bounded GET-only availability pass rechecked both complete current outstanding reports and existing journal/cashflow/invoice records in the selected administration. All outstanding rows still used EUR report currency, with no unknown due dates, unknown transit amounts or nonzero transit rows. A cash journal existed but no eligible company cashflow posting was found; no payment-service journal or non-EUR company cashflow posting was found. Purchase/sales reversal queries and commercial sales status-10/status-20 queries returned no examples. No entries were created or altered to obtain coverage, and these checks do **not** close the corresponding live edge-case gaps.

The session-isolated browser reached Exact's sign-in page, not an authenticated accounting view. Independent UI reconciliation, true reused-number identity examples and dangling-offset/fault cases remain unverified in this follow-up. API-only availability checks are not an independent browser comparison. No login credentials, raw invoices, balances, identifiers or source responses were committed. A rebuilt server also advertised all thirteen tools and successfully ran `search_draft_invoices` against a verified bill-to company: existing commercial headers were read, with no unprocessed matches and empty stderr on clean shutdown. This validates the live search path/field selection, not nonempty draft details. Draft-tool nonempty cases, retry faults and large-administration behavior are covered by invented local fixtures, not claimed as production validation.

#### Authenticated follow-up validation

Browser access was subsequently restored by attaching to the user's signed-in Chrome for Testing tab, replacing the earlier UI authentication blocker. The visible administration matched the MCP selection. Existing records only were inspected; no financial entries, matching, master data or invoices were created, edited, printed, sent or paid.

- All 37 current payable rows across 19 accounts and both receivable rows across two accounts reconciled with the source reports and fresh MCP summaries. Accounting entry/journal identities, supplier/customer invoice references, descriptions, invoice/due dates and default-currency report amounts matched. Per-account and overall signed net/positive/negative totals and calendar-due-date aging matched independent decimal/date calculations from the UI rows. This included a real negative payable row and both EUR/USD original invoice currencies; outstanding report currency remained EUR, not multiple reporting currencies.
- The payable UI's overdue filter returned exactly the independently calculated 25-row subset. Every row and signed total matched `get_outstanding_balances` with `overdue_only: true`; returning only one account summary retained the complete filtered totals. The browser was left on the unfiltered payable report afterward.
- The commercial sales overview, with all dates, invoices and credit notes included, Open checked and processing/processed unchecked, showed no sales invoices. The API's administration-wide status-10/status-20 query also returned no examples. The current MCP server advertised all thirteen tools and read an existing bill-to company's commercial headers successfully, excluding its processed invoices and returning no draft/open matches. This validates the empty search path and processed exclusion, **not nonempty draft details or their lines**.
- Fresh bounded availability queries again found no nonzero-transit or unknown-due-date outstanding rows, eligible company cash-journal posting, non-EUR company cashflow posting, configured payment-service journal, or true purchase/sales reversal. A reused account/entry-number/journal identity scan found no examples in the complete sales-header set or the inspected purchase-header prefix; the purchase scan stopped at its twelve-page bound, so absence across all purchase history was not established. Those live cases, dangling offsets and deliberate fault scenarios remain unverified rather than manufactured in production.

Browser console/page-exception baselines were captured before the authenticated checks. No warning/error-level console messages or uncaught page exceptions were observed; informational JQMIGRATE messages, including repeated plugin-load messages, were retained. Local MCP stderr was empty and the subprocess exited cleanly. Hosted Exact server logs were unavailable. Early navigation attempts targeted Exact's hidden help iframe rather than its visible main iframe; those failed waits were capture/navigation gaps, not evidence of an application failure, and the report comparisons were collected only after the correct frame and rendered report were verified.

Raw UI/API/MCP evidence and identifiers remain in private temporary storage outside the repository. Only verification documentation changed; no application behavior was adjusted to fit the observations. The earlier limitations on settlement evidence, immutable matching history, unavailable edge-case records and synthetic retry/large-source testing remain in force.

To repeat or extend the check:

1. Confirm the administration in the UI matches the MCP result. Open the current outstanding-items report with no reference date and disable **Achterstallig** when comparing unfiltered results. Keep payable and receivable checks separate.
2. Resolve the account with `search_accounts`, fetch `get_open_items`, and reconcile the complete item set using entry numbers and invoice/supplier references. Check invoice dates, due dates, signs and remaining open balances, not original invoice totals.
3. Compare amounts and currency against the report-balance column, not the original-currency **(V.V.)** column. Reconcile totals per reported currency without converting or offsetting sides.
4. Compare filtered and summary-only responses with the corresponding UI rows; make sure excluded and missing-date rows are treated consistently. Compare transit separately against independent payment-status evidence, without subtracting it from open balances.
5. Keep raw evidence private and outside the repository. Record the date, scope, mismatches and untested cases; use invented values for regression tests. If source data changes during the check, retrieve both sides again rather than forcing totals to agree.

### Nix build and development

The flake supports Apple Silicon macOS and aarch64/x86_64 Linux. `flake.lock` pins nixpkgs; `Cargo.lock` pins the Rust dependencies.

```sh
nix build
./result/bin/exact-mcp --help
nix flake check
nix develop
```

`nix build` and `nix flake check` run the test suite as part of the package build. `nix develop` provides Cargo, rustc, rustfmt and Clippy for the checks below. Credentials remain runtime environment variables; do not put them in the flake or tracked files.

### Configuration

| Variable | Purpose | Default |
| --- | --- | --- |
| `EXACT_CLIENT_ID` | Registered application ID; required for login | None |
| `EXACT_CLIENT_SECRET` | Application secret; required for login and refresh | None |
| `EXACT_BASE_URL` | Country-specific Exact HTTPS origin; used at login | `https://start.exactonline.nl` |
| `EXACT_REDIRECT_URI` | Registered HTTPS localhost callback; used at login | `https://exact-mcp.localhost:8400/callback` |
| `EXACT_TLS_CERT` | Optional PEM certificate/chain file; must be paired with `EXACT_TLS_KEY` | Generated self-signed certificate |
| `EXACT_TLS_KEY` | Optional matching unencrypted PEM private-key file | Generated in-memory key |
| `EXACT_MCP_DATA_DIR` | Override the local session directory | OS application-data directory |

To use your own certificate instead, set both `EXACT_TLS_CERT` and `EXACT_TLS_KEY`, or pass `--tls-cert <path> --tls-key <path>`. Keep private-key files outside the repository with restricted permissions. The CLI never modifies your OS trust store; outgoing Exact HTTPS certificate verification remains enabled.

Login also accepts `--base-url` and `--redirect-uri`. Redirects must use HTTPS with `localhost`, a `.localhost` hostname, or `127.0.0.1`, an explicit non-default port, and no query or fragment. Plain HTTP callbacks are not supported. Use a different registered port if 8400 is occupied. The origin, client ID and redirect URI are saved with the session; subsequent administration commands use that saved account configuration.

The global `--data-dir <path>` option overrides the data directory, including for `auth status`. Use separate directories for separate accounts/apps. On Unix, an existing directory must already be private (`0700`); the CLI does not change permissions on a directory you supply.

## Token storage and request behavior

- `auth status` prints the data-directory location. Defaults are `~/Library/Application Support/exact-mcp` on macOS and `~/.local/share/exact-mcp` on Linux, subject to OS/XDG configuration.
- Tokens are **plaintext**, protected on Unix by a `0700` directory and `0600` session file. They are not encrypted or stored in Keychain. Other platforms use the current user's filesystem defaults; equivalent ACL protection has not been implemented or tested.
- A file lock serializes session access and token exchanges across processes using the same directory. A refresh reloads the session after acquiring the lock, so another process's newly rotated tokens are reused rather than refreshed again. Ordinary API reads release the lock before network I/O.
- Tokens are refreshed on demand with a ten-second expiry margin. Every successful refresh is atomically persisted before an API request proceeds. Refreshes run as tracked async tasks: cancelling a tool cancels ordinary reads but does not interrupt an already-started token refresh/save. MCP shutdown drains these tasks before exiting. There is no background refresh daemon.
- Token exchanges are not retried automatically: an uncertain response may mean Exact already consumed the code or refresh token. A hard process kill, crash or disk failure after token rotation can still require another login; graceful shutdown cannot protect against those cases.
- API pagination is followed only within the saved origin's `/api/v1/` path. HTTP redirects are disabled, and server response bodies/tokens are not included in errors.
- HTTP 429 on GET requests (JSON pages, matching XML and PDF reads) permits at most **two retries**, with at most **60 seconds cumulative retry sleep per HTTP request/page**. Valid `Retry-After` seconds/HTTP dates and applicable Exact daily/minutely reset headers are respected; the longest applicable delay wins. A server-requested wait beyond the remaining budget fails immediately rather than retrying early. Missing/invalid hints use 1-second then 2-second backoff. The wait budget excludes network time and is not a whole-tool deadline; each source page has its own budget.
- Retry sleeps yield asynchronously and are cancellable; the response is released before waiting, and scope/token validity is checked again before each retry. No session lock is held while waiting. Retry exhaustion returns an error, never a partial successful report. HTTP 401/403, redirects, transport errors and other failures are not retried. **OAuth token exchanges are never retried**, including HTTP 429, because codes/refresh tokens may already have been consumed. If refresh fails, verify the client secret and authorize again if needed.

### Large-administration behavior

Outstanding reports aggregate each source page as it arrives instead of retaining every raw row. Summary-only company reports keep no invoice-row payloads; administration-wide reports keep account/currency summaries. Invoice search, draft search and G/L row reports retain only the best `offset + limit + 1` matching rows needed for their stable result window. G/L summary mode retains no transaction-row payloads. All source pages still contribute to counts/totals, and validation includes excluded/discarded rows. Later-page failures discard the result, not return partial totals.

Synthetic 10,000-row, 20-page tests check complete outstanding totals/aging, G/L summary and late sorted windows, and invoice-search windows where the best matches arrive on later pages. They are correctness/scale regressions, not a production performance benchmark. Duplicate-detection UUID/row-ID sets still grow with source size, deep offsets need larger windows, and non-summary company open-item results and complete invoice line details still contain all requested rows. Matching XML export limits remain unchanged.

There is no persistent cache, financial export checkpoint, snapshot token or new stale-data policy. Offset requests still reread source pages and can shift when Exact changes; this reduces retained row payloads, not source API call counts. Bulk/sync endpoint migration is deferred because endpoint/filter semantics and incremental storage would need separate verification. Concurrent tool calls have independent retry budgets; this is not a global/per-app rate scheduler.

## Checks

Additional regressions cover 429 retry bounds/reset hints, cancellation and scope changes during retry sleeps, no-retry token exchanges, streamed page failures, large sorted result windows, and read-only commercial invoice search/details over statuses 10/20 with bill-to identity checks. MCP subprocess discovery checks all fourteen tools and exercises draft and reconstructed bank-queue workflows separately from accounting invoices.

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Tests use temporary storage, local HTTP API mocks and HTTPS callbacks with temporary certificates. They cover state validation, automatic self-signed TLS, optional certificate/key loading, callback handling, token exchange/rotation, serialized refresh, storage permissions, scoped pagination, account-code/UUID lookup, search windows beyond 50 matches, inclusive due-date/overdue filters, summary totals, administration-wide grouping and account pagination, zero-net accounts, aging boundaries and bucket reconciliation, scoped purchase/sales invoice details, invoice-line pagination, separate currency/sign/VAT semantics, document/attachment association, commercial-invoice UUID mapping, reminder rejection, bounded binary downloads, PDF markers, safe private filenames/permissions, redirects and changed-scope rejection, decimal cancellation/overflow, financial totals, safe errors and administration selection. Subprocess tests exercise MCP initialization, tool discovery, parallel tool calls, company search, open-item queries, structured results, tool errors, scope changes, cancellation during token rotation and clean shutdown on stdin EOF. Additional tests verify overlapping API reads, released query locks, cancelled HTTP requests, single-refresh concurrency and asynchronous lock contention. Test certificates are trusted only by the test client; no CA is installed into the OS trust store. Live Exact OAuth, app permissions and administration access still require testing with your own application/account.

## References

- [Exact: current user](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=SystemSystemMe)
- [Exact: accessible administrations](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=SystemSystemDivisions)
- [Exact: accounts](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=CRMAccounts)
- [Exact: administration-wide payables](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=ReadFinancialPayablesList)
- [Exact: administration-wide receivables](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=ReadFinancialReceivablesList)
- [Exact: payables by account](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=ReadFinancialPayablesListByAccount)
- [Exact: receivables by account](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=ReadFinancialReceivablesListByAccount)
- [Exact: purchase accounting entries](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=PurchaseEntryPurchaseEntries)
- [Exact: purchase accounting lines](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=PurchaseEntryPurchaseEntryLines)
- [Exact: sales accounting entries](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=SalesEntrySalesEntries)
- [Exact: sales accounting lines](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=SalesEntrySalesEntryLines)
- [Exact: documents](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=DocumentsDocuments)
- [Exact: document attachments](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=DocumentsDocumentAttachments)
- [Exact: commercial sales invoices](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=SalesInvoiceSalesInvoices)
- [Picqer Exact client: authorization and refresh behavior](https://github.com/picqer/exact-php-client#authentication)

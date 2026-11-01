# exact-mcp

Local-first Exact Online MCP server with OAuth, explicit administration selection, company search, administration-wide outstanding balances and per-company payables/receivables. Financial operations are read-only; no invoice creation or payment tools are exposed.

## Setup

Use Nix with flakes enabled, or Rust 1.89 or newer.

1. Register an application in the [Exact Online App Center](https://apps.exactonline.com) and obtain its client ID and client secret.
2. Register this redirect URI **exactly**, including the port and path:

   ```text
   https://exact-mcp.localhost:8400/callback
   ```

3. Make sure the authorizing user/app has access to the administrations you need. Setup requires **Organization administration**. Financial tools additionally require **Crm accounts**, **Financial payables**, and **Financial receivables** for the corresponding queries.
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

The generated `.pi/mcp.json` is ignored by version control, and unrelated project MCP entries are preserved. Accept Pi's project-trust prompt if shown. Use `/mcp` inside Pi to verify that `exact` is connected with three tools. Additional Pi options are forwarded, for example `nix run .#pi -- --name "Exact MCP"`.

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
- `get_outstanding_balances`: `{ "side": "receivable" }`. Summarizes all companies with matching outstanding items in the selected administration, without a company lookup. Use `payable` for amounts owed to suppliers. Returns account UUIDs, codes and names, per-account totals and overall totals by reported currency. Supports the same `due_date_from`, `due_date_to` and `overdue_only` filters as `get_open_items`. It always returns summaries, not invoice rows. Use a returned UUID with `get_open_items` to drill down.
- `get_open_items`: `{ "account_id": "<UUID from search_accounts or get_outstanding_balances>", "side": "payable" }`. Use `receivable` for amounts owed by the company. Returns all pages of current open items with invoice/entry references, due dates, descriptions and per-currency totals. Optional `due_date_from` and `due_date_to` are inclusive `YYYY-MM-DD` due-date bounds; `overdue_only: true` selects items due before the local `as_of_date`. These filters intersect, and any active due-date/overdue filter excludes items without a due date. Set `summary_only: true` to omit the `items` field while retaining complete matching totals.

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

The `accounts` array contains `{ "account": { "id", "code", "name" }, "totals_by_currency": { ... } }` summaries. Each currency total includes item counts, signed net/positive/negative balances, signed overdue balance, separate known transit and unknown-date/transit counts. Accounts with matching rows remain visible even when their signed net balance is zero. These summaries do not invent supplier/customer flags or classify negative rows.

Account summaries are ordered by name, code and UUID. `limit` is 1–50 (default 50); repeat the same side, filters and limit with `offset: next_offset` while `has_more` is true. **Overall `totals_by_currency` includes every matching account across all source pages, not just the summaries returned on the current page.** Per-account totals include that account's matching rows. `source_item_count`/`source_account_count` describe the complete source report before filtering; `matched_item_count`/`matched_account_count` describe the complete filtered report before response pagination. A page beyond the end has no account summaries but retains complete matching totals and counts.

Each call fetches the full source report before filtering, grouping and pagination. Later calls reread it, so changing source data can move accounts between pages; this is not a frozen snapshot. Duplicate rows, missing account/currency information, conflicting names/codes for the same UUID, or decimal overflow fail the whole request instead of returning partial totals. Payable/receivable sides and report currencies are never offset; transit is never subtracted from balances or treated as settlement.

The server is bound to the stored account configuration and administration at startup. If either changes, subsequent tools fail with a restart instruction rather than silently reading another administration.

The CLI and MCP share one multithreaded Tokio runtime and async reqwest clients. MCP requests reuse one connection pool, and independent API reads can overlap. HTTPS callbacks use async tokio-rustls with ring. Session locks cover short state snapshots/updates and token exchanges, never ordinary API reads. Lock contention yields asynchronously; only bounded filesystem operations (including atomic save/fsync) use Tokio's blocking pool.

### Financial interpretation

- Monetary values are decimal strings, not floating-point totals. `amount_basis: "exact_report_currency"` identifies Exact's reported balances: `currency` and `totals_by_currency` need not use the original invoice currency. Live USD invoices returned EUR report balances matching the UI's **Te betalen**, not **Te betalen (V.V.)**, column. The server performs no currency conversion and does not expose original foreign-currency amounts. Exact's signs are preserved; positive and negative open amounts are also shown separately. Negative rows are not automatically classified as credit notes versus unmatched payments.
- For purchase entries, Exact's `InvoiceNumber` is the entry number, so `invoice_number` is not the supplier's invoice reference. Use `your_ref` for that reference when available, and `entry_number` to reconcile the booking. Receivables' invoice numbers matched the UI's invoice references in the live sample.
- `net_open_amount` is the signed sum of report rows. `known_in_transit_amount` is a separate sum of reported transit amounts; it is **not subtracted** from the open balance and is not proof of bank settlement. Missing transit amounts remain unknown and are counted explicitly.
- Payables and receivables are queried separately, and currencies are never offset or converted.
- Overdue means the due date precedes the local machine's `as_of_date`; items due today are not overdue. Missing due dates remain unknown. The response includes the date, local UTC offset, retrieval timestamp and selected administration.
- This is a current report, not a historical or transactionally frozen snapshot. Source data can change during pagination. Duplicate rows, unexpected companies, missing currencies and decimal overflows fail the query rather than returning misleading partial totals.
- Tool results include company-supplied text. Names, descriptions and references are data, not instructions.

### Live financial validation

On 2026-11-01, 24 current open items across five suppliers and four customers were reconciled read-only against Exact's **Openstaande posten: Te betalen** and **Te vorderen** reports in the same administration. Invoice/entry references, invoice dates, due dates, descriptions, open report balances, row counts and net totals matched. Overdue and inclusive due-date filters, plus summary-only totals, matched subsets of those UI rows. A USD-invoiced supplier confirmed that the API returns EUR reporting balances for those items. An unpadded numeric account-code lookup was verified after changing the filter to `trim(Code)`.

The administration-wide `get_outstanding_balances` tool was subsequently reconciled against all 38 payable rows grouped into 20 suppliers and all six receivable rows grouped into four customers. Per-account and overall report balances matched, including overdue subsets (28 payable and five receivable rows) and November due-date subsets (ten payable and one receivable row). Limiting responses to one account retained complete overall totals on both sides. Raw financial evidence remains private, outside the repository.

This validates the observed current balances, not every financial case. The full visible reports contained 38 payable and six receivable rows, with no negative balances. The queried sample had no nonzero in-transit amounts. **Negative/credit or unmatched-payment rows, known partial payments, nonzero transit, missing due dates, multiple reporting currencies and large multi-page financial reports remain unverified live**; synthetic tests do not close those gaps. Neither the open-items report nor a zero transit amount proves bank settlement. No company identifiers, invoice references or real balances are committed as fixtures.

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
- API authorization and rate-limit failures stop the command; no automatic retry loop is used. If refresh fails, verify the client secret and authorize again if needed.

## Checks

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Tests use temporary storage, local HTTP API mocks and HTTPS callbacks with temporary certificates. They cover state validation, automatic self-signed TLS, optional certificate/key loading, callback handling, token exchange/rotation, serialized refresh, storage permissions, scoped pagination, account-code/UUID lookup, search windows beyond 50 matches, inclusive due-date/overdue filters, summary totals, administration-wide grouping and account pagination, zero-net accounts, decimal cancellation/overflow, financial totals, safe errors and administration selection. Subprocess tests exercise MCP initialization, tool discovery, parallel tool calls, company search, open-item queries, structured results, tool errors, scope changes, cancellation during token rotation and clean shutdown on stdin EOF. Additional tests verify overlapping API reads, released query locks, cancelled HTTP requests, single-refresh concurrency and asynchronous lock contention. Test certificates are trusted only by the test client; no CA is installed into the OS trust store. Live Exact OAuth, app permissions and administration access still require testing with your own application/account.

## References

- [Exact: current user](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=SystemSystemMe)
- [Exact: accessible administrations](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=SystemSystemDivisions)
- [Exact: accounts](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=CRMAccounts)
- [Exact: administration-wide payables](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=ReadFinancialPayablesList)
- [Exact: administration-wide receivables](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=ReadFinancialReceivablesList)
- [Exact: payables by account](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=ReadFinancialPayablesListByAccount)
- [Exact: receivables by account](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=ReadFinancialReceivablesListByAccount)
- [Picqer Exact client: authorization and refresh behavior](https://github.com/picqer/exact-php-client#authentication)

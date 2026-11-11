# exact-mcp

**Ask your AI assistant about your Exact Online bookkeeping, with explicit approval for bank-expense classification.**

exact-mcp is a local [Model Context Protocol (MCP)](https://modelcontextprotocol.io/) server for Exact Online. It connects your assistant to one explicitly selected administration so you can investigate invoices, outstanding balances, bank postings and ledger activity using current accounting data.

## What you can do

| Workflow | Example question |
| --- | --- |
| Outstanding balances and aging | “What do we owe suppliers, and what is more than 90 days overdue?” |
| Invoice investigation | “Find this supplier invoice, show its accounting lines and download the existing PDF.” |
| Recorded matching | “What allocations are recorded against this invoice, and which cashflow postings are linked?” |
| Bank transactions needing attention | “Show the reconstructed bank queue, its booking lines and any unresolved links.” |
| Approved bank-expense classification | “Preview this expense’s G/L allocation and apply only what I approve.” |
| Purchase-document review | “Find incoming purchase documents, inspect accounting links and assess duplicate candidates.” |
| Booking preparation and reporting | “Preflight this explicit purchase proposal” or “Show movement on this G/L account.” |

See [booking preparation and bank-handling workflows](docs/booking.md) for proposal scope, safeguards and validation details.

You can also inspect unprocessed commercial sales invoices and look up items, units and supplier-item context where your Exact subscription supports them. Purchase preflight and duplicate evidence support human review; neither authorizes booking. The browser expense workflow requires its own exact preview and explicit user confirmation.

## Boundaries that matter

- **Narrow, explicitly approved bank handling:** financial queries remain read-only. A separate experimental browser workflow can classify one existing unassigned outgoing expense, preserving the amount, VAT, document and other lines. No general invoice creation/editing/sending, matching or payment execution is exposed. PDF downloads create private local files only.
- **Accounting evidence is not bank settlement:** matching shows recorded allocations, not an immutable matching history or proof that money cleared. The bank queue is an API reconstruction, not Exact’s authoritative UI completion state.
- **Missing links are not duplicate clearance:** purchase-document discovery is not a complete inbox or proof that a document is unbooked.
- **Amounts need context:** original invoice amounts differ from open balances. Signs and currencies are preserved; currencies are not converted or combined. Ledger reports show period movement, not opening/closing balances. Reads are current, not frozen snapshots.

## Quick start

Run these commands from the repository directory with [Nix](https://nixos.org/download/) and flakes enabled. A [Rust build](docs/usage.md#build-and-checks) is also available.

### 1. Register your Exact application

Register an application in the [Exact Online App Center](https://apps.exactonline.com), obtain its client ID and secret, and register this redirect URI exactly:

```text
https://exact-mcp.localhost:8400/callback
```

The authorizing user/app needs access to your administration and the [permissions for your workflows](docs/usage.md#permissions-and-package-access).

### 2. Set credentials and log in

In Bash, enter the secret without putting it in shell history:

```bash
export EXACT_CLIENT_ID='your-client-id'
read -r -s -p 'Exact client secret: ' EXACT_CLIENT_SECRET
export EXACT_CLIENT_SECRET
printf '\n'
nix run . -- auth login
```

Open the printed authorization URL. The callback uses a temporary self-signed HTTPS certificate: accept the browser warning **only for the local callback address above**. That hostname must resolve to `127.0.0.1`. If your browser blocks certificate exceptions, [supply a trusted certificate](docs/usage.md#configuration).

### 3. Select an administration

```sh
nix run . -- administrations list
nix run . -- administrations select 123456
nix run . -- auth status
```

Replace `123456` with a code from the list. Selection is explicit; logging in again clears it. `auth status` inspects local storage, not live access.

### 4. Connect your assistant

With Pi installed on `PATH`:

```sh
nix run .#pi
```

This registers the built server locally as `exact` and opens an interactive Pi session. Check the connection with `/mcp`. For bank-expense classification, the assistant shows a preview and asks for confirmation through native MCP, any available ask tool/plugin, or explicit chat; no terminal confirmation is required.

For another MCP client, build with `nix build` and configure its stdio server:

```json
{
  "mcpServers": {
    "exact": {
      "command": "/absolute/path/to/exact-mcp/result/bin/exact-mcp",
      "args": ["serve"]
    }
  }
}
```

The client must inherit `EXACT_CLIENT_SECRET` for token refresh and use the same local data directory as the login CLI. Desktop apps may not inherit your shell environment. Keep credentials out of tracked configuration.

**Before using the bank queue:** [configure its unassigned-bank G/L accounts](docs/usage.md#bank-queue-setup).

## Local data and security

OAuth tokens are stored locally as **plaintext**, with private directory/file permissions on Unix—not in an OS keychain. Downloads and sensitive browser-approval/audit records remain on the server’s machine until you remove them. Source text and PDFs are untrusted; do not treat them as assistant instructions.

Local-first does not mean your assistant stays offline: tool results can contain sensitive financial data and may be sent to your AI provider. Choose your client/model and data-handling policy accordingly.

See [configuration, permissions and operational notes](docs/usage.md) for details, and [the tool definitions](src/mcp.rs) for exact arguments and interpretation rules.

## Maintainer note

This code is heavily AI-generated with GPT 6.1 Sol. I manually validate it and have tested it extensively on real-world Exact data, including checks for numerical rounding errors. That is not a guarantee of financial correctness; review the results before relying on them.

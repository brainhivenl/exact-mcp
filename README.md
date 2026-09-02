# exact-mcp

Local-first Exact Online integration, starting with OAuth and explicit administration selection. This first milestone is a CLI, not an MCP server yet. It does not read or change financial records.

## Setup

Use Nix with flakes enabled, or Rust 1.89 or newer.

1. Register an application in the [Exact Online App Center](https://apps.exactonline.com) and obtain its client ID and client secret.
2. Register this redirect URI **exactly**, including the port and path:

   ```text
   https://exact-mcp.localhost:8400/callback
   ```

3. Make sure the authorizing user/app has access to the administrations you need. The `system/Divisions` endpoint requires the **Organization administration** scope.
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

The CLI reads environment variables directly; it does not automatically load `.env` files. The client secret is never saved by the CLI. Keep it available in the environment for automatic token refresh on subsequent API commands.

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
- A file lock serializes commands using the same directory. Each command reloads the session after acquiring the lock, preventing simultaneous refreshes from using the same rotating refresh token.
- Tokens are refreshed on demand with a ten-second expiry margin. Every successful refresh is atomically persisted before an API request proceeds. There is no background refresh daemon.
- Token exchanges are not retried automatically: an uncertain response may mean Exact already consumed the code or refresh token. A crash or disk failure after token rotation can still require another login.
- API pagination is followed only within the saved origin's `/api/v1/` path. HTTP redirects are disabled, and server response bodies/tokens are not included in errors.
- API authorization and rate-limit failures stop the command; no automatic retry loop is used. If refresh fails, verify the client secret and authorize again if needed.

## Checks

```sh
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Tests use temporary storage, local HTTP API mocks and HTTPS callbacks with temporary certificates. They cover state validation, automatic self-signed TLS, optional certificate/key loading, callback handling, token exchange/rotation, serialized refresh, storage permissions, pagination, safe errors and administration selection. Test certificates are trusted only by the test client; no CA is installed into the OS trust store. Live Exact OAuth, app permissions and administration access still require testing with your own application/account.

## References

- [Exact: current user](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=SystemSystemMe)
- [Exact: accessible administrations](https://start.exactonline.nl/docs/HlpRestAPIResourcesDetails.aspx?name=SystemSystemDivisions)
- [Picqer Exact client: authorization and refresh behavior](https://github.com/picqer/exact-php-client#authentication)

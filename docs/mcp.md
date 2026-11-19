# Managed MCP services

Set `AUTH_PASSWORD` and sign in with the browser operator password. Open **MCP Services** using the bottom-left Apps button, then add a service and test the connection. A connection test checks initialization. Use **Settings → MCP Services** on a Bot to grant access. Every service starts unassigned, including for existing Bots. Global disable blocks use while preserving individual grants. Removing a service clears its grants.

- **Streamable HTTP** uses the service's MCP endpoint and optionally a bearer credential file. Use HTTPS for authenticated remote endpoints.
- **Local stdio** uses an executable and an argument list. Enter one argument per line. To pass a credential, add a `VARIABLE=filename` entry. SuperBot passes `PATH` and the specified credentials to the executable. Only configure commands you trust to run under the SuperBot service account.

Put credential files in `DATA_DIR/mcp-secrets` or set `MCP_SECRETS_DIR` to an administrator-controlled directory. Use mode `700` for the directory and `600` for each file. Enter the filename in SuperBot. Keep credentials out of URLs, executable arguments, and the repository. To rotate a credential, replace its file. Future connections read the updated file. SuperBot stores the filename in SQLite and uses the value only when it connects.

Claude Code, Codex, and Pi discover and call managed tools through `botctl mcp services`, `botctl mcp tools <service-id>`, and `botctl mcp call <service-id> <tool> '<JSON object>'`. The custom provider exposes these operations as functions when `CUSTOM_OPENAI_TOOLS=true` and the Bot has access at the start of its turn. The broker checks the live grant and service revision before calling a tool. Revocation applies to subsequent calls in the same run. An MCP call already admitted can finish.

The broker uses the [official Rust MCP SDK](https://github.com/modelcontextprotocol/rust-sdk) and opens a fresh connection for each operation. It supports tool discovery and ordinary tool calls. OAuth login, legacy HTTP+SSE endpoints, resources, prompts, and tools that require a persistent connection or interactive MCP flow are outside this version. A remote provider must supply a usable bearer token if it requires authentication.

Managed grants cover the SuperBot broker. Existing harness-local MCP settings and the shared operating-system account retain their own access. Run mutually untrusted Bots under separate accounts or instances. File permissions and the broker do not isolate programs running as the same service user.

## Development checks

Run `cargo test --workspace --locked` on a host that permits loopback networking. The integration test starts fake MCP services and an isolated `botd`. To check the browser controls, build the workspace and run `CHROME_BIN=<chromium-executable> node server/tests/browser-ux.mjs` with Node.js 22 or later.

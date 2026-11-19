# 0005 Managed MCP broker

Use the Rust MCP SDK in `botd` to own transports and credential loading. Give each execution an expiring token. The broker maps the token to the active Bot and checks the persisted service and grant on every request. Keep catalogue management behind browser operator authentication. Use credential file references so the database and browser forms handle metadata only.

CLI harnesses reuse `botctl` and their existing shell capability. The custom harness exposes the same operations as functions. All use the same broker, including after harness session resumption.

//! mag-client — the seat-facing MCP stdio shim. Speaks MCP on stdio and
//! forwards every call to the resident mag daemon, so all seats share one
//! warm pool. `mag` with no args behaves identically (default --lane forward);
//! this bin exists so the MCP server id in opencode.json reads `mag-client`.

fn main() -> anyhow::Result<std::process::ExitCode> {
    mag::run_main()
}

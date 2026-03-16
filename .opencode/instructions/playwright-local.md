Use the project-local `playwright` MCP server for browser work in this repo.

- Do not call Playwright's browser installer or rely on Playwright-downloaded browser bundles.
- Assume the browser comes from the Nix dev shell via `PLAYWRIGHT_BROWSER_EXECUTABLE_PATH`.
- Start the app with `trunk serve --address 127.0.0.1 --port 8080` before browser automation if it is not already running.
- Prefer `http://127.0.0.1:8080` for local testing and use accessibility snapshots before screenshots.
- The wrapper already runs headless, blocks service workers, and writes traces under `.playwright-mcp/`.

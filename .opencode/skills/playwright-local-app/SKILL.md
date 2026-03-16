---
name: playwright-local-app
description: Drive the local Fedigents app through the project Playwright MCP wrapper on NixOS.
license: MIT
compatibility: opencode
---

## When to use me

Use this skill when you need browser automation against the local app during development or debugging.

## Workflow

1. Start the app from the repo root with `trunk serve --address 127.0.0.1 --port 8080` unless it is already running.
2. Use the `playwright` MCP server configured in `opencode.json`.
3. Navigate to `http://127.0.0.1:8080` and prefer `browser_snapshot` for inspection.
4. Do not use `browser_install`; this repo is set up to use the system Chromium from the Nix dev shell.
5. If browser startup fails, verify `nix develop`, `npm install`, and `PLAYWRIGHT_BROWSER_EXECUTABLE_PATH`.

## Notes

- The wrapper blocks service workers to avoid stale PWA state during local testing.
- Session artifacts and traces are stored under `.playwright-mcp/`.

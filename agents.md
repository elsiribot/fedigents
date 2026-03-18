# Fedigents

Chat-first Fedimint wallet PWA with an LLM-powered agent.

## Build Environment

The project uses a **Nix flake** (`flake.nix`) for a reproducible dev shell. Enter it with:

```sh
nix develop          # if experimental features are enabled globally
# or
nix --extra-experimental-features 'nix-command flakes' develop
```

The shell provides:

- **Rust stable** (via `rust-overlay`) with the `wasm32-unknown-unknown` target
- **Trunk 0.21** for building and serving the WASM app
- **wasm-bindgen-cli**, **binaryen** for post-processing
- **clang** as the C/C++ compiler for wasm32 native dependencies
- **cargo-leptos**, **leptosfmt**, **cargo-nextest**
- **Node.js 22** + **Chromium** for Playwright-based E2E tests

Key environment variables set by the shell:

| Variable | Purpose |
|---|---|
| `CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS` | `--cfg getrandom_backend="wasm_js"` |
| `CC_wasm32_unknown_unknown` | Points to unwrapped clang for wasm linking |
| `PLAYWRIGHT_BROWSER_EXECUTABLE_PATH` | Nix-provided Chromium |

### Building

```sh
trunk build          # production build to dist/
trunk serve          # dev server on http://127.0.0.1:8080
```

Trunk reads `Trunk.toml` which points at `crates/fedigents-web/index.html` as the entry point. The HTML uses `data-trunk` links to pull in the Rust binary, CSS, and static assets (manifest, service worker, icon, skills directory).

## Architecture

```
┌─────────────────────────────────────────────────┐
│  Browser main thread                            │
│                                                 │
│  main.rs ─── app.rs (Leptos CSR UI)             │
│                 │                               │
│                 ├── agent.rs (WalletAgent)       │
│                 │     Uses rig-core 0.30 to      │
│                 │     drive an LLM tool-use loop │
│                 │     via PPQ (OpenAI-compat API) │
│                 │                               │
│                 └── wallet_runtime.rs            │
│                       WalletRuntime (thin client)│
│                       ↕ postMessage (JSON)       │
├─────────────────────────────────────────────────┤
│  Web Worker thread                              │
│                                                 │
│  wallet_runtime.rs ── run_worker_entrypoint()   │
│                         ↕                       │
│  fedimint.rs ── WalletRuntimeCore               │
│                   Fedimint client SDK            │
│                   OPFS-backed redb database      │
└─────────────────────────────────────────────────┘
```

### Entrypoint (`main.rs`)

The same WASM binary serves both contexts. On load it checks `web_sys::window()`:

- **Window present** &rarr; main thread: mounts the Leptos app.
- **Window absent** &rarr; worker thread: runs `run_worker_entrypoint()`, which listens for `postMessage` commands from the main thread.

### UI Layer (`app.rs`)

Leptos 0.7 in CSR (client-side rendering) mode. A single `App` component manages:

- Wallet bootstrap flow (connect &rarr; first deposit &rarr; PPQ funding)
- Chat interface with markdown rendering and inline QR codes
- Payment confirmation card for outgoing Lightning payments
- QR scanner via `leptos-qr-scanner`
- Background receive watchers that push notifications into the chat

### Agent (`agent.rs`)

Uses **rig-core 0.30** with its `ToolDyn` trait for dynamic tool dispatch. The agent talks to **claude-haiku-4.5** through PPQ (OpenAI-compatible API).

A single `WalletTool` enum implements `ToolDyn` with six variants:

| Tool | Description |
|---|---|
| `get_balance` | Returns current wallet balance in sats |
| `create_invoice` | Creates a BOLT11 invoice to receive a payment |
| `propose_payment` | Proposes an outgoing Lightning payment for user confirmation |
| `list_operations` | Lists recent wallet operations (waits 200ms for non-final ops to settle) |
| `show_receive_code` | Shows the wallet's LNURL receive code |
| `load_skill` | Loads a skill prompt by slug from the skills catalog |

Tool parameter schemas are auto-generated from `schemars::JsonSchema` derives with doc comments providing descriptions.

### Worker Bridge (`wallet_runtime.rs`)

The main thread's `WalletRuntime` is a thin async client that serializes `Command` enums to JSON, posts them to the Web Worker, and awaits responses via `oneshot` channels. The worker deserializes commands, calls `WalletRuntimeCore`, and posts back `ResponseEnvelope` messages.

Two event streams flow worker &rarr; main thread outside the request/response cycle:

- **Bootstrap events**: progress notes, receive code, balance updates during initial setup
- **Operation events**: payment settlement notifications from background watchers

### Fedimint Runtime (`fedimint.rs`)

`WalletRuntimeCore` wraps the Fedimint client SDK. It:

- Joins a federation and initializes wallet/mint/LN modules
- Uses OPFS `FileSystemSyncAccessHandle` for the redb database (via `fedimint-cursed-redb`)
- Falls back to `LocalStorage` when sync access handles are unavailable
- Manages LNURL recurring receive subscriptions
- Spawns background watchers for incoming LN payments that trigger cached outcome settlement

### PPQ Integration (`ppq.rs`)

PPQ provides an OpenAI-compatible LLM API funded via Lightning. On first run, Fedigents:

1. Creates a PPQ account (stored in Fedimint client metadata)
2. Funds it with a $0.10 Lightning payment
3. Uses the API key for all subsequent agent calls

### Static Assets

- `public/sw.js` &mdash; Service worker for PWA offline support
- `public/manifest.webmanifest` &mdash; PWA manifest
- `public/icon.svg` &mdash; App icon
- `skills/` &mdash; Skill catalog (index.json + prompt files) loaded at runtime
- `crates/fedigents-web/src/browser.js` &mdash; JS glue for OPFS, service worker registration, clipboard, and worker creation
- `crates/fedigents-web/src/wallet-worker.js` &mdash; Worker bootstrap script

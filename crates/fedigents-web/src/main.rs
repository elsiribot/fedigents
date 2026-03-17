#[cfg(target_family = "wasm")]
mod agent;
#[cfg(target_family = "wasm")]
mod app;
#[cfg(target_family = "wasm")]
mod browser;
#[cfg(target_family = "wasm")]
mod fedimint;
#[cfg(target_family = "wasm")]
mod ppq;
#[cfg(target_family = "wasm")]
mod wallet_runtime;

#[cfg(target_family = "wasm")]
fn main() {
    console_error_panic_hook::set_once();
    if wallet_runtime::run_worker_entrypoint() {
        return;
    }
    tracing_wasm::set_as_global_default();
    leptos::mount::mount_to_body(app::App);
}

#[cfg(not(target_family = "wasm"))]
fn main() {
    println!("Fedigents is a wasm-only app. Use `trunk serve` or `trunk build`.");
}

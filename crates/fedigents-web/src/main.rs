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
fn main() {
    tracing_wasm::set_as_global_default();
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(app::App);
}

#[cfg(not(target_family = "wasm"))]
fn main() {
    println!("Fedigents is a wasm-only app. Use `trunk serve` or `trunk build`.");
}

use js_sys::{Function, Promise};
use wasm_bindgen::prelude::*;
use web_sys::{FileSystemSyncAccessHandle, HtmlVideoElement};

#[wasm_bindgen(module = "/src/browser.js")]
extern "C" {
    #[wasm_bindgen(catch, js_name = openWalletDb)]
    pub fn open_wallet_db(file_name: &str) -> Result<Promise, JsValue>;

    #[wasm_bindgen(catch, js_name = registerServiceWorker)]
    pub fn register_service_worker() -> Result<Promise, JsValue>;

    #[wasm_bindgen(catch, js_name = copyText)]
    pub fn copy_text(value: &str) -> Result<Promise, JsValue>;

    #[wasm_bindgen(catch, js_name = startQrScanner)]
    pub fn start_qr_scanner(video: &HtmlVideoElement, callback: &Function) -> Result<Promise, JsValue>;

    #[wasm_bindgen(catch, js_name = stopQrScanner)]
    pub fn stop_qr_scanner(video: &HtmlVideoElement) -> Result<Promise, JsValue>;
}

pub async fn open_wallet_handle(file_name: &str) -> anyhow::Result<FileSystemSyncAccessHandle> {
    let promise = open_wallet_db(file_name).map_err(js_error)?;
    let value = wasm_bindgen_futures::JsFuture::from(promise)
        .await
        .map_err(js_error)?;
    Ok(value.unchecked_into())
}

pub async fn ensure_service_worker() -> anyhow::Result<()> {
    let promise = register_service_worker().map_err(js_error)?;
    wasm_bindgen_futures::JsFuture::from(promise)
        .await
        .map_err(js_error)?;
    Ok(())
}

pub async fn copy_to_clipboard(value: &str) -> anyhow::Result<()> {
    let promise = copy_text(value).map_err(js_error)?;
    wasm_bindgen_futures::JsFuture::from(promise)
        .await
        .map_err(js_error)?;
    Ok(())
}

pub async fn begin_qr_scanner(
    video: &HtmlVideoElement,
    callback: &Function,
) -> anyhow::Result<()> {
    let promise = start_qr_scanner(video, callback).map_err(js_error)?;
    wasm_bindgen_futures::JsFuture::from(promise)
        .await
        .map_err(js_error)?;
    Ok(())
}

pub async fn end_qr_scanner(video: &HtmlVideoElement) -> anyhow::Result<()> {
    let promise = stop_qr_scanner(video).map_err(js_error)?;
    wasm_bindgen_futures::JsFuture::from(promise)
        .await
        .map_err(js_error)?;
    Ok(())
}

fn js_error(err: JsValue) -> anyhow::Error {
    anyhow::anyhow!(err.as_string().unwrap_or_else(|| format!("{err:?}")))
}

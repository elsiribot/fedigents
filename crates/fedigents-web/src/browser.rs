use js_sys::Promise;
use wasm_bindgen::prelude::*;
use web_sys::{FileSystemSyncAccessHandle, Worker};

#[wasm_bindgen(module = "/src/browser.js")]
extern "C" {
    #[wasm_bindgen(catch, js_name = openWalletDb)]
    pub fn open_wallet_db(file_name: &str) -> Result<Promise, JsValue>;

    #[wasm_bindgen(catch, js_name = registerServiceWorker)]
    pub fn register_service_worker() -> Result<Promise, JsValue>;

    #[wasm_bindgen(catch, js_name = copyText)]
    pub fn copy_text(value: &str) -> Result<Promise, JsValue>;

    #[wasm_bindgen(catch, js_name = createWalletWorker)]
    pub fn create_wallet_worker() -> Result<Promise, JsValue>;

    #[wasm_bindgen(js_name = supportsSyncAccessHandles)]
    pub fn supports_sync_access_handles_js() -> bool;

    #[wasm_bindgen(catch, js_name = startRecording)]
    pub fn start_recording() -> Result<Promise, JsValue>;

    #[wasm_bindgen(catch, js_name = stopRecording)]
    pub fn stop_recording() -> Result<Promise, JsValue>;

    #[wasm_bindgen(catch, js_name = transcribeAudio)]
    pub fn transcribe_audio(blob: &JsValue, api_key: &str) -> Result<Promise, JsValue>;
}

pub async fn open_wallet_handle(file_name: &str) -> anyhow::Result<FileSystemSyncAccessHandle> {
    let promise = open_wallet_db(file_name).map_err(js_error)?;
    let value = wasm_bindgen_futures::JsFuture::from(promise)
        .await
        .map_err(js_error)?;
    Ok(value.unchecked_into())
}

pub async fn spawn_wallet_worker() -> anyhow::Result<Worker> {
    let promise = create_wallet_worker().map_err(js_error)?;
    let value = wasm_bindgen_futures::JsFuture::from(promise)
        .await
        .map_err(js_error)?;
    Ok(value.unchecked_into())
}

pub fn supports_sync_access_handles() -> bool {
    supports_sync_access_handles_js()
}

pub fn is_worker_context() -> bool {
    web_sys::window().is_none()
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

pub async fn begin_recording() -> anyhow::Result<()> {
    let promise = start_recording().map_err(js_error)?;
    wasm_bindgen_futures::JsFuture::from(promise)
        .await
        .map_err(js_error)?;
    Ok(())
}

pub async fn cancel_recording() {
    if let Ok(promise) = stop_recording() {
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }
}

pub async fn finish_recording_and_transcribe(api_key: &str) -> anyhow::Result<String> {
    let promise = stop_recording().map_err(js_error)?;
    let blob = wasm_bindgen_futures::JsFuture::from(promise)
        .await
        .map_err(js_error)?;
    if blob.is_null() || blob.is_undefined() {
        anyhow::bail!("No audio recorded");
    }
    let promise = transcribe_audio(&blob, api_key).map_err(js_error)?;
    let result = wasm_bindgen_futures::JsFuture::from(promise)
        .await
        .map_err(js_error)?;
    result
        .as_string()
        .ok_or_else(|| anyhow::anyhow!("Transcription returned non-string"))
}

fn js_error(err: JsValue) -> anyhow::Error {
    anyhow::anyhow!(err.as_string().unwrap_or_else(|| format!("{err:?}")))
}

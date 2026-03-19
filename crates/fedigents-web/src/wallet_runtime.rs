use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use anyhow::Context;
use futures::channel::oneshot;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::{DedicatedWorkerGlobalScope, MessageEvent, Worker};

use crate::browser;
use crate::fedimint::WalletRuntimeCore;
use crate::ppq::PpqAccount;

#[derive(Clone, Debug)]
pub enum BootstrapEvent {
    Note(String),
    ReceiveCode(String),
    Balance(u64),
}

#[derive(Clone, Debug)]
pub enum OperationEvent {
    PaymentReceived {
        amount_sats: Option<u64>,
        invoice: Option<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InvoiceResponse {
    pub operation_id: String,
    pub invoice: String,
}

#[derive(Clone)]
pub struct WalletRuntime {
    worker: WorkerClient,
    storage_notice: Option<String>,
}

impl WalletRuntime {
    pub async fn connect() -> anyhow::Result<Self> {
        let worker = browser::spawn_wallet_worker().await?;
        let client = WorkerClient::new(worker);
        let connect: ConnectResponse = client.request(Command::Connect).await?;
        Ok(Self {
            worker: client,
            storage_notice: connect.storage_notice,
        })
    }

    pub fn storage_notice(&self) -> Option<String> {
        self.storage_notice.clone()
    }

    pub async fn bootstrap<F>(&self, on_event: F) -> anyhow::Result<()>
    where
        F: FnMut(BootstrapEvent) + 'static,
    {
        let callback = Rc::new(RefCell::new(on_event));
        self.worker.set_bootstrap_listener(Some(Rc::new({
            let callback = Rc::clone(&callback);
            move |event| {
                callback.borrow_mut()(event);
            }
        })));

        let result = self.worker.request::<()>(Command::Bootstrap).await;
        self.worker.set_bootstrap_listener(None);
        result
    }

    pub async fn mark_ppq_ready(&self) -> anyhow::Result<()> {
        self.worker.request(Command::MarkPpqReady).await
    }

    pub async fn is_ppq_ready(&self) -> anyhow::Result<bool> {
        self.worker.request(Command::IsPpqReady).await
    }

    pub async fn get_balance(&self) -> anyhow::Result<u64> {
        self.worker.request(Command::GetBalance).await
    }

    pub async fn cached_receive_code(&self) -> anyhow::Result<Option<String>> {
        self.worker.request(Command::CachedReceiveCode).await
    }

    pub async fn ppq_account(&self) -> anyhow::Result<Option<PpqAccount>> {
        self.worker.request(Command::PpqAccount).await
    }

    pub async fn ensure_ppq_account(&self) -> anyhow::Result<PpqAccount> {
        self.worker.request(Command::EnsurePpqAccount).await
    }

    pub async fn ppq_funding_in_flight(&self) -> anyhow::Result<bool> {
        self.worker.request(Command::PpqFundingInFlight).await
    }

    pub async fn begin_ppq_funding_attempt(&self) -> anyhow::Result<()> {
        self.worker.request(Command::BeginPpqFundingAttempt).await
    }

    pub async fn repair_ppq_account(&self) -> anyhow::Result<PpqAccount> {
        self.worker.request(Command::RepairPpqAccount).await
    }

    pub async fn create_invoice(
        &self,
        amount_sats: u64,
        description: &str,
    ) -> anyhow::Result<InvoiceResponse> {
        self.worker
            .request(Command::CreateInvoice {
                amount_sats,
                description: description.to_owned(),
            })
            .await
    }

    pub async fn pay(&self, payment: &str, amount_sats: Option<u64>) -> anyhow::Result<String> {
        self.worker
            .request(Command::Pay {
                payment: payment.to_owned(),
                amount_sats,
            })
            .await
    }

    pub async fn list_operations(&self, limit: usize) -> anyhow::Result<String> {
        self.worker.request(Command::ListOperations { limit }).await
    }

    pub fn set_operation_listener(&self, listener: Option<Rc<dyn Fn(OperationEvent)>>) {
        self.worker.set_operation_listener(listener);
    }

    pub async fn watch_pending_receives(&self) -> anyhow::Result<()> {
        self.worker.request(Command::WatchPendingReceives).await
    }
}

pub fn run_worker_entrypoint() -> bool {
    if !browser::is_worker_context() {
        return false;
    }

    let scope: DedicatedWorkerGlobalScope = js_sys::global().unchecked_into();
    let runtime = Rc::new(RefCell::new(None::<WalletRuntimeCore>));
    let on_message = Closure::wrap(Box::new({
        let runtime = Rc::clone(&runtime);
        let scope = scope.clone();
        move |event: MessageEvent| {
            let Some(raw) = event.data().as_string() else {
                return;
            };

            let Ok(request) = serde_json::from_str::<RequestEnvelope>(&raw) else {
                return;
            };

            let runtime = Rc::clone(&runtime);
            let scope = scope.clone();
            spawn_local(async move {
                let response = handle_request(runtime, scope.clone(), request).await;
                if let Err(err) = post_message(&scope, &response) {
                    let _ = post_message(
                        &scope,
                        &ResponseEnvelope {
                            id: response.id,
                            payload: ResponsePayload::Err {
                                message: format!("Failed to post worker response: {err}"),
                            },
                        },
                    );
                }
            });
        }
    }) as Box<dyn FnMut(MessageEvent)>);
    scope.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message.forget();
    true
}

async fn handle_request(
    runtime: Rc<RefCell<Option<WalletRuntimeCore>>>,
    scope: DedicatedWorkerGlobalScope,
    request: RequestEnvelope,
) -> ResponseEnvelope {
    let result = match request.command {
        Command::Connect => match WalletRuntimeCore::connect().await {
            Ok(wallet) => {
                let payload = ConnectResponse {
                    storage_notice: wallet.storage_notice(),
                };
                runtime.borrow_mut().replace(wallet);
                serialize_ok(payload)
            }
            Err(err) => Err(err),
        },
        Command::Bootstrap => with_runtime(&runtime, |wallet| async move {
            wallet
                .bootstrap(|event| {
                    let event = WireBootstrapEvent::from_core(event);
                    let _ = post_message(&scope, &WorkerEventEnvelope::bootstrap(event));
                })
                .await
        })
        .await
        .and_then(|_| serialize_ok(())),
        Command::MarkPpqReady => {
            with_runtime(
                &runtime,
                |wallet| async move { wallet.mark_ppq_ready().await },
            )
            .await
            .and_then(|_| serialize_ok(()))
        }
        Command::IsPpqReady => {
            with_runtime(
                &runtime,
                |wallet| async move { wallet.is_ppq_ready().await },
            )
            .await
            .and_then(serialize_ok)
        }
        Command::GetBalance => with_runtime(&runtime, |wallet| async move {
            Ok(wallet.get_balance().await?.sats_round_down())
        })
        .await
        .and_then(serialize_ok),
        Command::CachedReceiveCode => with_runtime(&runtime, |wallet| async move {
            wallet.cached_receive_code().await
        })
        .await
        .and_then(serialize_ok),
        Command::PpqAccount => {
            with_runtime(&runtime, |wallet| async move { wallet.ppq_account().await })
                .await
                .and_then(serialize_ok)
        }
        Command::EnsurePpqAccount => with_runtime(&runtime, |wallet| async move {
            wallet.ensure_ppq_account().await
        })
        .await
        .and_then(serialize_ok),
        Command::PpqFundingInFlight => with_runtime(&runtime, |wallet| async move {
            wallet.ppq_funding_in_flight().await
        })
        .await
        .and_then(serialize_ok),
        Command::BeginPpqFundingAttempt => with_runtime(&runtime, |wallet| async move {
            wallet.begin_ppq_funding_attempt().await
        })
        .await
        .and_then(|_| serialize_ok(())),
        Command::RepairPpqAccount => with_runtime(&runtime, |wallet| async move {
            wallet.repair_ppq_account().await
        })
        .await
        .and_then(serialize_ok),
        Command::CreateInvoice {
            amount_sats,
            description,
        } => {
            let scope = scope.clone();
            with_runtime(&runtime, move |wallet| async move {
                let (op_id, response) = wallet.create_invoice(amount_sats, &description).await?;
                let invoice_str = response.invoice.clone();
                wallet.spawn_receive_watcher(op_id, false, Some(amount_sats), move |amt| {
                    let event = WireOperationEvent::PaymentReceived {
                        amount_sats: amt,
                        invoice: Some(invoice_str),
                    };
                    let _ = post_message(&scope, &WorkerEventEnvelope::operation(event));
                });
                Ok(InvoiceResponse {
                    operation_id: response.operation_id,
                    invoice: response.invoice,
                })
            })
            .await
            .and_then(serialize_ok)
        }
        Command::Pay {
            payment,
            amount_sats,
        } => with_runtime(&runtime, |wallet| async move {
            wallet.pay(&payment, amount_sats).await
        })
        .await
        .and_then(serialize_ok),
        Command::ListOperations { limit } => with_runtime(&runtime, |wallet| async move {
            wallet.list_operations(limit).await
        })
        .await
        .and_then(serialize_ok),
        Command::WatchPendingReceives => {
            let scope = scope.clone();
            with_runtime(&runtime, move |wallet| async move {
                wallet
                    .watch_pending_receives(move |amount_sats| {
                        let event = WireOperationEvent::PaymentReceived {
                            amount_sats,
                            invoice: None,
                        };
                        let _ =
                            post_message(&scope, &WorkerEventEnvelope::operation(event));
                    })
                    .await
            })
            .await
            .and_then(|_| serialize_ok(()))
        }
    };

    ResponseEnvelope {
        id: request.id,
        payload: match result {
            Ok(value) => ResponsePayload::Ok { value },
            Err(err) => ResponsePayload::Err {
                message: format!("{err:#}"),
            },
        },
    }
}

async fn with_runtime<F, Fut, T>(
    runtime: &Rc<RefCell<Option<WalletRuntimeCore>>>,
    call: F,
) -> anyhow::Result<T>
where
    F: FnOnce(WalletRuntimeCore) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    if runtime.borrow().is_none() {
        let connected = WalletRuntimeCore::connect().await?;
        runtime.borrow_mut().replace(connected);
    }

    let wallet = runtime
        .borrow()
        .clone()
        .ok_or_else(|| anyhow::anyhow!("Wallet runtime is unavailable"))?;
    call(wallet).await
}

fn serialize_ok<T: Serialize>(value: T) -> anyhow::Result<serde_json::Value> {
    serde_json::to_value(value).map_err(Into::into)
}

fn post_message<T: Serialize>(
    scope: &DedicatedWorkerGlobalScope,
    message: &T,
) -> anyhow::Result<()> {
    let raw = serde_json::to_string(message)?;
    scope
        .post_message(&wasm_bindgen::JsValue::from_str(&raw))
        .map_err(|err| anyhow::anyhow!(format!("{err:?}")))
}

type ResponseSender = oneshot::Sender<anyhow::Result<serde_json::Value>>;
type BootstrapListener = Rc<dyn Fn(BootstrapEvent)>;
type OperationListener = Rc<dyn Fn(OperationEvent)>;

#[derive(Clone)]
struct WorkerClient {
    inner: Rc<WorkerClientInner>,
}

struct WorkerClientInner {
    worker: Worker,
    next_id: Cell<u64>,
    pending: Rc<RefCell<HashMap<u64, ResponseSender>>>,
    bootstrap_listener: Rc<RefCell<Option<BootstrapListener>>>,
    operation_listener: Rc<RefCell<Option<OperationListener>>>,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
}

impl WorkerClient {
    fn new(worker: Worker) -> Self {
        let pending = Rc::new(RefCell::new(HashMap::<u64, ResponseSender>::new()));
        let bootstrap_listener = Rc::new(RefCell::new(None::<BootstrapListener>));
        let operation_listener = Rc::new(RefCell::new(None::<OperationListener>));
        let on_message = Closure::wrap(Box::new({
            let pending = Rc::clone(&pending);
            let bootstrap_listener = Rc::clone(&bootstrap_listener);
            let operation_listener = Rc::clone(&operation_listener);
            move |event: MessageEvent| {
                let Some(raw) = event.data().as_string() else {
                    return;
                };

                if let Ok(envelope) = serde_json::from_str::<ResponseEnvelope>(&raw) {
                    let result = match envelope.payload {
                        ResponsePayload::Ok { value } => Ok(value),
                        ResponsePayload::Err { message } => Err(anyhow::anyhow!(message)),
                    };
                    if let Some(sender) = pending.borrow_mut().remove(&envelope.id) {
                        let _ = sender.send(result);
                    }
                    return;
                }

                if let Ok(event) = serde_json::from_str::<WorkerEventEnvelope>(&raw) {
                    match event.payload {
                        WorkerEventPayload::Bootstrap { data } => {
                            if let Some(callback) = bootstrap_listener.borrow().as_ref() {
                                callback(data.into_public());
                            }
                        }
                        WorkerEventPayload::OperationSettled { data } => {
                            if let Some(callback) = operation_listener.borrow().as_ref() {
                                callback(data.into_public());
                            }
                        }
                    }
                }
            }
        }) as Box<dyn FnMut(MessageEvent)>);
        worker.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

        let inner = WorkerClientInner {
            worker,
            next_id: Cell::new(1),
            pending,
            bootstrap_listener,
            operation_listener,
            _on_message: on_message,
        };
        Self {
            inner: Rc::new(inner),
        }
    }

    fn set_bootstrap_listener(&self, listener: Option<Rc<dyn Fn(BootstrapEvent)>>) {
        self.inner.bootstrap_listener.replace(listener);
    }

    fn set_operation_listener(&self, listener: Option<Rc<dyn Fn(OperationEvent)>>) {
        self.inner.operation_listener.replace(listener);
    }

    async fn request<T: DeserializeOwned>(&self, command: Command) -> anyhow::Result<T> {
        let id = self.inner.next_id.get();
        self.inner.next_id.set(id + 1);
        let (sender, receiver) = oneshot::channel();
        self.inner.pending.borrow_mut().insert(id, sender);

        let request = RequestEnvelope { id, command };
        let raw = serde_json::to_string(&request)?;
        if let Err(err) = self
            .inner
            .worker
            .post_message(&wasm_bindgen::JsValue::from_str(&raw))
        {
            self.inner.pending.borrow_mut().remove(&id);
            return Err(anyhow::anyhow!(format!("{err:?}")));
        }

        let value = receiver
            .await
            .context("Wallet worker response channel closed")??;
        Ok(serde_json::from_value(value)?)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum Command {
    Connect,
    Bootstrap,
    MarkPpqReady,
    IsPpqReady,
    GetBalance,
    CachedReceiveCode,
    PpqAccount,
    EnsurePpqAccount,
    PpqFundingInFlight,
    BeginPpqFundingAttempt,
    RepairPpqAccount,
    CreateInvoice {
        amount_sats: u64,
        description: String,
    },
    Pay {
        payment: String,
        amount_sats: Option<u64>,
    },
    ListOperations {
        limit: usize,
    },
    WatchPendingReceives,
}

#[derive(Debug, Serialize, Deserialize)]
struct RequestEnvelope {
    id: u64,
    #[serde(flatten)]
    command: Command,
}

#[derive(Debug, Serialize, Deserialize)]
struct ConnectResponse {
    storage_notice: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ResponseEnvelope {
    id: u64,
    #[serde(flatten)]
    payload: ResponsePayload,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ResponsePayload {
    Ok { value: serde_json::Value },
    Err { message: String },
}

#[derive(Debug, Serialize, Deserialize)]
struct WorkerEventEnvelope {
    #[serde(flatten)]
    payload: WorkerEventPayload,
}

impl WorkerEventEnvelope {
    fn bootstrap(event: WireBootstrapEvent) -> Self {
        Self {
            payload: WorkerEventPayload::Bootstrap { data: event },
        }
    }

    fn operation(event: WireOperationEvent) -> Self {
        Self {
            payload: WorkerEventPayload::OperationSettled { data: event },
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum WorkerEventPayload {
    Bootstrap { data: WireBootstrapEvent },
    OperationSettled { data: WireOperationEvent },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireBootstrapEvent {
    Note { note: String },
    ReceiveCode { code: String },
    Balance { sats: u64 },
}

impl WireBootstrapEvent {
    fn from_core(event: crate::fedimint::BootstrapEvent) -> Self {
        match event {
            crate::fedimint::BootstrapEvent::Note(note) => Self::Note { note },
            crate::fedimint::BootstrapEvent::ReceiveCode(code) => Self::ReceiveCode { code },
            crate::fedimint::BootstrapEvent::Balance(balance) => Self::Balance {
                sats: balance.sats_round_down(),
            },
        }
    }

    fn into_public(self) -> BootstrapEvent {
        match self {
            Self::Note { note } => BootstrapEvent::Note(note),
            Self::ReceiveCode { code } => BootstrapEvent::ReceiveCode(code),
            Self::Balance { sats } => BootstrapEvent::Balance(sats),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireOperationEvent {
    PaymentReceived {
        amount_sats: Option<u64>,
        #[serde(default)]
        invoice: Option<String>,
    },
}

impl WireOperationEvent {
    fn into_public(self) -> OperationEvent {
        match self {
            Self::PaymentReceived {
                amount_sats,
                invoice,
            } => OperationEvent::PaymentReceived {
                amount_sats,
                invoice,
            },
        }
    }
}

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::str::FromStr;

use fedimint_bip39::{Bip39RootSecretStrategy, Language, Mnemonic};
use fedimint_client::secret::RootSecretStrategy;
use fedimint_client::{Client, ClientHandle, RootSecret};
use fedimint_connectors::ConnectorRegistry;
use fedimint_core::Amount;
use fedimint_core::config::FederationId;
use fedimint_core::core::OperationId;
use fedimint_core::db::mem_impl::MemDatabase;
use fedimint_core::db::{Database, IDatabaseTransactionOpsCoreTyped};
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::invite_code::InviteCode;
use fedimint_core::impl_db_record;
use fedimint_core::secp256k1::PublicKey;
use fedimint_core::task::sleep;
use fedimint_core::util::SafeUrl;
use fedimint_cursed_redb::MemAndRedb;
use fedimint_derive_secret::{ChildId, DerivableSecret};
use fedimint_eventlog::EventLogId;
use fedimint_ln_client::get_invoice;
use fedimint_ln_client::recurring::{RecurringInvoiceCreatedEvent, RecurringPaymentProtocol};
use fedimint_ln_client::{LightningClientInit, LightningClientModule, LnReceiveState};
use fedimint_meta_client::MetaClientInit;
use fedimint_meta_common::DEFAULT_META_KEY;
use fedimint_mint_client::MintClientInit;
use fedimint_wallet_client::WalletClientInit;
use futures::StreamExt;
use gloo_storage::{LocalStorage, Storage};
use rand::thread_rng;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::info;

use crate::browser;
use crate::ppq::{PpqAccount, PpqClient};

pub const DEFAULT_INVITE_CODE: &str = "fed11qgqzggnhwden5te0v9cxjtn9vd3jue3wvfkxjmnyva6kzunyd9skutnwv46z7qqpyzhv5mxgpl79xz7j649sj6qldmde5s2uxchy4uh7840qgymsqmazzp6sn43";

const DB_FILE: &str = "fedigents.redb";
const APP_STATE_DB_PREFIX: &[u8] = b"fedigents.app-state";
const MNEMONIC_KEY: &str = "fedigents.wallet.mnemonic";
const PPQ_READY_KEY: &str = "fedigents.ppq.ready";
const PPQ_ACCOUNT_KEY: &str = "fedigents.ppq.account";
const PPQ_FUNDING_KEY: &str = "fedigents.ppq.funding";
const LNURL_KEY: &str = "fedigents.receive.lnurl";

#[repr(u8)]
enum AppStateDbKeyPrefix {
    Mnemonic = 0x00,
    PpqReady = 0x01,
    PpqAccount = 0x02,
    ReceiveCode = 0x03,
    PpqFundingInFlight = 0x04,
}

#[derive(Debug, Encodable, Decodable)]
struct MnemonicStateKey;

#[derive(Debug, Encodable, Decodable)]
struct MnemonicStateValue(pub String);

impl_db_record!(
    key = MnemonicStateKey,
    value = MnemonicStateValue,
    db_prefix = AppStateDbKeyPrefix::Mnemonic,
);

#[derive(Debug, Encodable, Decodable)]
struct PpqReadyStateKey;

#[derive(Debug, Encodable, Decodable)]
struct PpqReadyStateValue(pub bool);

impl_db_record!(
    key = PpqReadyStateKey,
    value = PpqReadyStateValue,
    db_prefix = AppStateDbKeyPrefix::PpqReady,
);

#[derive(Debug, Encodable, Decodable)]
struct PpqFundingInFlightStateKey;

#[derive(Debug, Encodable, Decodable)]
struct PpqFundingInFlightStateValue(pub bool);

impl_db_record!(
    key = PpqFundingInFlightStateKey,
    value = PpqFundingInFlightStateValue,
    db_prefix = AppStateDbKeyPrefix::PpqFundingInFlight,
);

#[derive(Debug, Encodable, Decodable)]
struct PpqAccountStateKey;

impl_db_record!(
    key = PpqAccountStateKey,
    value = PpqAccount,
    db_prefix = AppStateDbKeyPrefix::PpqAccount,
);

#[derive(Debug, Encodable, Decodable)]
struct ReceiveCodeStateKey;

impl_db_record!(
    key = ReceiveCodeStateKey,
    value = ReceiveCode,
    db_prefix = AppStateDbKeyPrefix::ReceiveCode,
);

#[derive(Clone, Debug, Serialize, Deserialize, Encodable, Decodable)]
pub struct ReceiveCode {
    pub payment_code_idx: u64,
    pub lnurl: String,
}

#[derive(Clone)]
pub struct WalletRuntime {
    connectors: ConnectorRegistry,
    database: Database,
    app_state: Database,
    client: Rc<RefCell<Option<Rc<ClientHandle>>>>,
    storage_notice: Option<String>,
}

#[derive(Clone, Debug)]
pub enum BootstrapEvent {
    Note(String),
    ReceiveCode(String),
    Balance(Amount),
}

#[derive(Clone, Debug, Serialize)]
pub struct InvoiceResponse {
    pub operation_id: String,
    pub invoice: String,
}

impl WalletRuntime {
    pub async fn connect() -> anyhow::Result<Self> {
        let (database, storage_notice) = match browser::open_wallet_handle(DB_FILE).await {
            Ok(handle) => {
                let cursed_db = MemAndRedb::new(handle)?;
                (Database::new(cursed_db, Default::default()), None)
            }
            Err(err) => {
                let notice = format!(
                    "Persistent wallet storage is unavailable in this browser session ({err}). Falling back to in-memory mode. Your wallet state will reset when this tab closes."
                );
                (Database::new(MemDatabase::new(), Default::default()), Some(notice))
            }
        };

        let app_state = database.with_prefix(APP_STATE_DB_PREFIX.to_vec());
        let connectors = ConnectorRegistry::build_from_client_defaults().bind().await?;
        let runtime = Self {
            connectors,
            database,
            app_state,
            client: Rc::new(RefCell::new(None)),
            storage_notice,
        };

        runtime.migrate_legacy_local_storage().await?;
        Ok(runtime)
    }

    pub fn storage_notice(&self) -> Option<String> {
        self.storage_notice.clone()
    }

    pub async fn bootstrap<F>(&self, mut on_event: F) -> anyhow::Result<()>
    where
        F: FnMut(BootstrapEvent),
    {
        let client = self.ensure_client().await?;
        let balance = client.get_balance_for_btc().await?;
        on_event(BootstrapEvent::Balance(balance));

        if self.is_ppq_ready().await? {
            return Ok(());
        }

        let receive_code = match self.ensure_receive_code(&client).await {
            Ok(receive_code) => {
                on_event(BootstrapEvent::ReceiveCode(receive_code.lnurl.clone()));
                Some(receive_code)
            }
            Err(err) => {
                on_event(BootstrapEvent::Note(format!(
                    "LNURL receive setup is unavailable in this federation right now ({err}). Falling back to a starter invoice."
                )));
                None
            }
        };

        if balance.msats == 0 {
            if let Some(receive_code) = receive_code {
                on_event(BootstrapEvent::Note(
                    "Wallet joined. Waiting for the first LNURL deposit before funding PPQ.".to_owned(),
                ));
                self.wait_for_first_deposit(&client, receive_code.payment_code_idx, &mut on_event)
                    .await?;
            } else {
                let (operation_id, invoice) = self
                    .create_invoice_internal(&client, 1_000, "Initial Fedigents funding")
                    .await?;
                on_event(BootstrapEvent::Note(
                    "Paste this starter BOLT11 invoice into your wallet and pay it to continue setup:".to_owned(),
                ));
                on_event(BootstrapEvent::Note(invoice.to_string()));
                self.wait_for_first_invoice_deposit(&client, operation_id, &mut on_event)
                    .await?;
            }
        } else {
            on_event(BootstrapEvent::Note(
                "Existing wallet balance detected. Continuing with PPQ funding.".to_owned(),
            ));
        }

        let refreshed_balance = client.get_balance_for_btc().await?;
        on_event(BootstrapEvent::Balance(refreshed_balance));
        Ok(())
    }

    pub async fn mark_ppq_ready(&self) -> anyhow::Result<()> {
        let mut dbtx = self.app_state.begin_transaction().await;
        dbtx.insert_entry(&PpqReadyStateKey, &PpqReadyStateValue(true)).await;
        dbtx.insert_entry(
            &PpqFundingInFlightStateKey,
            &PpqFundingInFlightStateValue(false),
        )
        .await;
        dbtx.commit_tx_result().await?;
        Ok(())
    }

    pub async fn is_ppq_ready(&self) -> anyhow::Result<bool> {
        Ok(self
            .read_app_state(&PpqReadyStateKey)
            .await?
            .map(|value| value.0)
            .unwrap_or(false))
    }

    pub async fn get_balance(&self) -> anyhow::Result<Amount> {
        let client = self.ensure_client().await?;
        client.get_balance_for_btc().await
    }

    pub async fn cached_receive_code(&self) -> anyhow::Result<Option<String>> {
        Ok(self
            .read_app_state(&ReceiveCodeStateKey)
            .await?
            .map(|code| code.lnurl))
    }

    pub async fn ppq_account(&self) -> anyhow::Result<Option<PpqAccount>> {
        self.read_app_state(&PpqAccountStateKey).await
    }

    pub async fn ensure_ppq_account(&self, ppq: &PpqClient) -> anyhow::Result<PpqAccount> {
        if let Some(account) = self.ppq_account().await? {
            return Ok(account);
        }

        let account = ppq.create_account().await?;
        self.write_app_state(&PpqAccountStateKey, &account).await?;
        Ok(account)
    }

    pub async fn ppq_funding_in_flight(&self) -> anyhow::Result<bool> {
        Ok(self
            .read_app_state(&PpqFundingInFlightStateKey)
            .await?
            .map(|value| value.0)
            .unwrap_or(false))
    }

    pub async fn begin_ppq_funding_attempt(&self) -> anyhow::Result<()> {
        self.write_app_state(
            &PpqFundingInFlightStateKey,
            &PpqFundingInFlightStateValue(true),
        )
        .await
    }

    pub async fn repair_ppq_account(&self, ppq: &PpqClient) -> anyhow::Result<PpqAccount> {
        if let Some(account) = self.ppq_account().await? {
            return Ok(account);
        }

        let replacement = ppq.create_account().await?;
        self.write_app_state(&PpqAccountStateKey, &replacement).await?;
        Ok(replacement)
    }

    pub async fn create_invoice(
        &self,
        amount_sats: u64,
        description: &str,
    ) -> anyhow::Result<InvoiceResponse> {
        let client = self.ensure_client().await?;
        let (operation_id, invoice) = self
            .create_invoice_internal(&client, amount_sats, description)
            .await?;

        Ok(InvoiceResponse {
            operation_id: format!("{operation_id:?}"),
            invoice: invoice.to_string(),
        })
    }

    pub async fn pay(&self, payment: &str, amount_sats: Option<u64>) -> anyhow::Result<String> {
        let client = self.ensure_client().await?;
        let ln = client.get_first_module::<LightningClientModule>()?.inner();
        let invoice = get_invoice(payment, amount_sats.map(Amount::from_sats), None).await?;
        let gateway = ln.get_gateway(None::<PublicKey>, false).await?;
        let payment_result = ln.pay_bolt11_invoice(gateway, invoice, ()).await?;
        let operation_id = payment_result.payment_type.operation_id();
        let outcome = ln.await_outgoing_payment(operation_id).await?;
        Ok(serde_json::to_string(&outcome)?)
    }

    pub async fn list_operations(&self, limit: usize) -> anyhow::Result<String> {
        let client = self.ensure_client().await?;
        let operations = client.operation_log().paginate_operations_rev(limit, None).await;
        Ok(serde_json::to_string_pretty(&operations)?)
    }

    async fn ensure_client(&self) -> anyhow::Result<Rc<ClientHandle>> {
        if let Some(client) = self.client.borrow().clone() {
            return Ok(client);
        }

        let mnemonic = self.ensure_mnemonic().await?;
        let builder = Self::client_builder().await?;
        let client = if Client::is_initialized(&self.database).await {
            info!("Opening existing Fedimint client");
            let federation_id = Client::get_config_from_db(&self.database)
                .await
                .ok_or_else(|| anyhow::anyhow!("Client config not found in database"))?
                .calculate_federation_id();
            let root_secret = RootSecret::StandardDoubleDerive(
                derive_federation_secret(&mnemonic, &federation_id),
            );
            Rc::new(
                builder
                    .open(self.connectors.clone(), self.database.clone(), root_secret)
                    .await?,
            )
        } else {
            info!("Joining federation for the first time");
            let invite = InviteCode::from_str(DEFAULT_INVITE_CODE)?;
            let federation_id = invite.federation_id();
            let root_secret = RootSecret::StandardDoubleDerive(
                derive_federation_secret(&mnemonic, &federation_id),
            );
            let preview = builder.preview(self.connectors.clone(), &invite).await?;
            Rc::new(preview.join(self.database.clone(), root_secret).await?)
        };

        self.client.borrow_mut().replace(client.clone());
        Ok(client)
    }

    async fn client_builder() -> anyhow::Result<fedimint_client::ClientBuilder> {
        let mut builder = Client::builder().await?;
        builder.with_module(MintClientInit);
        builder.with_module(LightningClientInit::default());
        builder.with_module(WalletClientInit(None));
        builder.with_module(MetaClientInit);
        Ok(builder)
    }

    async fn ensure_mnemonic(&self) -> anyhow::Result<Mnemonic> {
        if let Some(words) = self
            .read_app_state(&MnemonicStateKey)
            .await?
            .map(|value| value.0)
        {
            return Ok(Mnemonic::parse_in_normalized(Language::English, &words)?);
        }

        let mnemonic = Bip39RootSecretStrategy::<12>::random(&mut thread_rng());
        let words = mnemonic.words().map(|word| word.to_string()).collect::<Vec<_>>().join(" ");
        self.write_app_state(&MnemonicStateKey, &MnemonicStateValue(words))
            .await?;
        Ok(mnemonic)
    }

    async fn ensure_receive_code(&self, client: &Rc<ClientHandle>) -> anyhow::Result<ReceiveCode> {
        if let Some(receive) = self.read_app_state(&ReceiveCodeStateKey).await? {
            LocalStorage::delete(LNURL_KEY);
            return Ok(receive);
        }

        let ln = client.get_first_module::<LightningClientModule>()?.inner();
        if let Some(receive) = self.migrate_legacy_receive_code(client).await? {
            return Ok(receive);
        }
        let existing = ln.list_recurring_payment_codes().await;
        let receive = if let Some((payment_code_idx, code)) = existing.into_iter().next() {
            ReceiveCode {
                payment_code_idx,
                lnurl: code.code,
            }
        } else {
            let recurringd_api = self.lookup_recurringd_api(client).await?;
            let meta = serde_json::to_string(&serde_json::json!([
                ["text/plain", "Deposit into your Fedigents wallet"]
            ]))?;
            let code = ln
                .register_recurring_payment_code(RecurringPaymentProtocol::LNURL, recurringd_api, &meta)
                .await?;
            let payment_code_idx = ln
                .list_recurring_payment_codes()
                .await
                .into_iter()
                .find_map(|(idx, item)| (item.code == code.code).then_some(idx))
                .ok_or_else(|| anyhow::anyhow!("Unable to determine LNURL payment code index"))?;

            ReceiveCode {
                payment_code_idx,
                lnurl: code.code,
            }
        };

        self.write_app_state(&ReceiveCodeStateKey, &receive).await?;
        Ok(receive)
    }

    async fn migrate_legacy_local_storage(&self) -> anyhow::Result<()> {
        self.migrate_local_storage_entry::<MnemonicStateKey, String, _>(
            MNEMONIC_KEY,
            &MnemonicStateKey,
            MnemonicStateValue,
        )
        .await?;
        self.migrate_local_storage_entry::<PpqReadyStateKey, bool, _>(
            PPQ_READY_KEY,
            &PpqReadyStateKey,
            PpqReadyStateValue,
        )
        .await?;
        self.migrate_local_storage_entry::<PpqAccountStateKey, PpqAccount, _>(
            PPQ_ACCOUNT_KEY,
            &PpqAccountStateKey,
            |account| account,
        )
        .await?;
        self.migrate_local_storage_entry::<PpqFundingInFlightStateKey, bool, _>(
            PPQ_FUNDING_KEY,
            &PpqFundingInFlightStateKey,
            PpqFundingInFlightStateValue,
        )
        .await?;
        Ok(())
    }

    async fn migrate_legacy_receive_code(
        &self,
        client: &Rc<ClientHandle>,
    ) -> anyhow::Result<Option<ReceiveCode>> {
        if let Ok(receive) = LocalStorage::get::<ReceiveCode>(LNURL_KEY) {
            self.write_app_state(&ReceiveCodeStateKey, &receive).await?;
            LocalStorage::delete(LNURL_KEY);
            return Ok(Some(receive));
        }

        let Ok(lnurl) = LocalStorage::get::<String>(LNURL_KEY) else {
            return Ok(None);
        };

        let ln = client.get_first_module::<LightningClientModule>()?.inner();
        let receive = ln
            .list_recurring_payment_codes()
            .await
            .into_iter()
            .find_map(|(payment_code_idx, code)| {
                (code.code == lnurl).then_some(ReceiveCode {
                    payment_code_idx,
                    lnurl: code.code,
                })
            });

        if let Some(receive) = receive {
            self.write_app_state(&ReceiveCodeStateKey, &receive).await?;
            LocalStorage::delete(LNURL_KEY);
            return Ok(Some(receive));
        }

        Ok(None)
    }

    async fn migrate_local_storage_entry<K, T, F>(
        &self,
        legacy_key: &str,
        db_key: &K,
        map_value: F,
    ) -> anyhow::Result<()>
    where
        K: fedimint_core::db::DatabaseKey + fedimint_core::db::DatabaseRecord,
        T: for<'de> Deserialize<'de>,
        F: FnOnce(T) -> K::Value,
    {
        if self.read_app_state(db_key).await?.is_some() {
            LocalStorage::delete(legacy_key);
            return Ok(());
        }

        if let Ok(value) = LocalStorage::get::<T>(legacy_key) {
            self.write_app_state(db_key, &map_value(value)).await?;
            LocalStorage::delete(legacy_key);
        }

        Ok(())
    }

    async fn read_app_state<K>(&self, key: &K) -> anyhow::Result<Option<K::Value>>
    where
        K: fedimint_core::db::DatabaseKey + fedimint_core::db::DatabaseRecord,
    {
        let mut dbtx = self.app_state.begin_transaction_nc().await;
        Ok(dbtx.get_value(key).await)
    }

    async fn write_app_state<K>(&self, key: &K, value: &K::Value) -> anyhow::Result<()>
    where
        K: fedimint_core::db::DatabaseKey + fedimint_core::db::DatabaseRecord,
    {
        let mut dbtx = self.app_state.begin_transaction().await;
        dbtx.insert_entry(key, value).await;
        dbtx.commit_tx_result().await?;
        Ok(())
    }

    async fn lookup_recurringd_api(&self, client: &Rc<ClientHandle>) -> anyhow::Result<SafeUrl> {
        let meta = client
            .get_first_module::<fedimint_meta_client::MetaClientModule>()?
            .inner();
        let value = meta
            .get_consensus_value(DEFAULT_META_KEY)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Federation meta values are unavailable"))?
            .value
            .to_json_lossy()?;

        let recurringd_api = value
            .as_object()
            .and_then(|map| map.get("recurringd_api"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Federation meta does not expose `recurringd_api`"))?;

        SafeUrl::from_str(recurringd_api).map_err(Into::into)
    }

    async fn wait_for_first_deposit<F>(
        &self,
        client: &Rc<ClientHandle>,
        payment_code_idx: u64,
        on_event: &mut F,
    ) -> anyhow::Result<()>
    where
        F: FnMut(BootstrapEvent),
    {
        let ln = client.get_first_module::<LightningClientModule>()?.inner();
        let mut last_event_id: Option<EventLogId> = None;
        let mut seen_operations = HashSet::new();

        loop {
            let balance = client.get_balance_for_btc().await?;
            if balance.msats > 0 {
                on_event(BootstrapEvent::Balance(balance));
                on_event(BootstrapEvent::Note(
                    "First deposit detected in the wallet.".to_owned(),
                ));
                return Ok(());
            }

            if let Some(invoice_map) = ln.list_recurring_payment_code_invoices(payment_code_idx).await {
                for operation_id in invoice_map.into_values() {
                    if seen_operations.insert(operation_id) {
                        on_event(BootstrapEvent::Note(
                            "LNURL receive request noticed in the event log. Waiting for settlement.".to_owned(),
                        ));
                        if self.await_recurring_receive(client, operation_id).await? {
                            let refreshed = client.get_balance_for_btc().await?;
                            on_event(BootstrapEvent::Balance(refreshed));
                            return Ok(());
                        }
                    }
                }
            }

            for entry in client.get_event_log(last_event_id, 64).await {
                last_event_id = Some(entry.id());
                let Some(event) = entry.as_raw().to_event::<RecurringInvoiceCreatedEvent>() else {
                    continue;
                };
                if event.payment_code_idx != payment_code_idx {
                    continue;
                }
                if !seen_operations.insert(event.operation_id) {
                    continue;
                }
                on_event(BootstrapEvent::Note(
                    "A new LNURL receive invoice was created. Waiting for payment finality.".to_owned(),
                ));
                if self.await_recurring_receive(client, event.operation_id).await? {
                    let refreshed = client.get_balance_for_btc().await?;
                    on_event(BootstrapEvent::Balance(refreshed));
                    return Ok(());
                }
            }

            sleep(std::time::Duration::from_secs(2)).await;
        }
    }

    async fn await_recurring_receive(
        &self,
        client: &Rc<ClientHandle>,
        operation_id: OperationId,
    ) -> anyhow::Result<bool> {
        let ln = client.get_first_module::<LightningClientModule>()?.inner();
        let mut updates = ln
            .subscribe_ln_recurring_receive(operation_id)
            .await?
            .into_stream();
        while let Some(update) = updates.next().await {
            match update {
                LnReceiveState::Claimed => return Ok(true),
                LnReceiveState::Canceled { .. } => return Ok(false),
                _ => {}
            }
        }
        Ok(false)
    }

    async fn create_invoice_internal(
        &self,
        client: &Rc<ClientHandle>,
        amount_sats: u64,
        description: &str,
    ) -> anyhow::Result<(OperationId, lightning_invoice::Bolt11Invoice)> {
        let ln = client.get_first_module::<LightningClientModule>()?.inner();
        let gateway = ln.get_gateway(None::<PublicKey>, false).await?;
        let amount = Amount::from_sats(amount_sats);
        let (operation_id, invoice, _) = ln
            .create_bolt11_invoice(
                amount,
                lightning_invoice::Bolt11InvoiceDescription::Direct(
                    lightning_invoice::Description::new(description.to_owned())?,
                ),
                None,
                (),
                gateway,
            )
            .await?;
        Ok((operation_id, invoice))
    }

    async fn wait_for_first_invoice_deposit<F>(
        &self,
        client: &Rc<ClientHandle>,
        operation_id: OperationId,
        on_event: &mut F,
    ) -> anyhow::Result<()>
    where
        F: FnMut(BootstrapEvent),
    {
        let ln = client.get_first_module::<LightningClientModule>()?.inner();
        let mut updates = ln.subscribe_ln_receive(operation_id).await?.into_stream();
        while let Some(update) = updates.next().await {
            match update {
                LnReceiveState::Claimed => {
                    on_event(BootstrapEvent::Note(
                        "First deposit received and claimed.".to_owned(),
                    ));
                    return Ok(());
                }
                LnReceiveState::Canceled { reason } => {
                    return Err(anyhow::anyhow!("Starter invoice canceled: {reason:?}"));
                }
                _ => {}
            }
        }

        Err(anyhow::anyhow!(
            "Starter invoice stream ended before a settled payment"
        ))
    }
}

fn derive_federation_secret(mnemonic: &Mnemonic, federation_id: &FederationId) -> DerivableSecret {
    let global_root_secret = Bip39RootSecretStrategy::<12>::to_root_secret(mnemonic);
    let multi_federation_root_secret = global_root_secret.child_key(ChildId(0));
    let federation_root_secret = multi_federation_root_secret.federation_key(federation_id);
    let federation_wallet_root_secret = federation_root_secret.child_key(ChildId(0));
    federation_wallet_root_secret.child_key(ChildId(0))
}

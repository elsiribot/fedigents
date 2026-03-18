use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use anyhow::Context;
use gloo_storage::{LocalStorage, Storage};
use reqwest::Method;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use rig::completion::{Chat, Message, ToolDefinition};
use rig::prelude::*;
use rig::providers::openai;
use rig::tool::{ToolDyn, ToolError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use web_sys::window;

use crate::wallet_runtime::WalletRuntime;

const PPQ_API_BASE: &str = "https://api.ppq.ai";
const MODEL: &str = "claude-haiku-4.5";
const CUSTOM_SKILLS_KEY: &str = "fedigents.skills.custom";

const PREAMBLE: &str = "\
You are the wallet agent inside a chat-only Fedimint wallet. \
All wallet actions must happen through tools. Keep answers short and practical. \
For outgoing payments, ALWAYS use propose_payment. Never pay directly. \
The UI shows a confirm button and only that button can actually send funds.";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillSummary {
    pub slug: String,
    pub title: String,
    pub summary: String,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredSkill {
    slug: String,
    title: String,
    summary: String,
    prompt: String,
}

impl StoredSkill {
    fn summary(&self) -> SkillSummary {
        SkillSummary {
            slug: self.slug.clone(),
            title: self.title.clone(),
            summary: self.summary.clone(),
            path: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ChatRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub body: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingPaymentProposal {
    pub payment: String,
    pub amount_sats: Option<u64>,
    pub summary: String,
}

#[derive(Clone, Debug)]
pub struct AgentResponse {
    pub messages: Vec<ChatMessage>,
    pub pending_payment: Option<PendingPaymentProposal>,
}

struct ToolLog {
    outputs: RefCell<Vec<ChatMessage>>,
    pending_payment: RefCell<Option<PendingPaymentProposal>>,
}

impl ToolLog {
    fn new() -> Rc<Self> {
        Rc::new(Self {
            outputs: RefCell::new(Vec::new()),
            pending_payment: RefCell::new(None),
        })
    }

    fn push(&self, role: ChatRole, body: String) {
        tracing::info!("{body}");
        self.outputs.borrow_mut().push(ChatMessage { role, body });
    }
}

// ── Tool args ───────────────────────────────────────────────────────────

/// Get the current wallet balance in sats.
#[derive(Deserialize, JsonSchema)]
struct GetBalanceArgs {}

/// Create a BOLT11 invoice to receive a payment.
#[derive(Deserialize, JsonSchema)]
struct CreateInvoiceArgs {
    /// Amount in satoshis
    amount_sats: u64,
    /// Invoice description
    #[serde(default = "default_description")]
    description: String,
}

fn default_description() -> String {
    "Fedigents request".into()
}

/// Propose an outgoing Lightning payment for user confirmation. The UI shows a confirm button; only that button sends funds.
#[derive(Deserialize, JsonSchema)]
struct ProposePaymentArgs {
    /// BOLT11 invoice or LNURL
    payment: String,
    /// Amount in sats (required for amountless invoices/LNURL)
    amount_sats: Option<u64>,
    /// Short human-readable description
    #[serde(default = "default_payment_summary")]
    summary: String,
}

fn default_payment_summary() -> String {
    "Lightning payment awaiting confirmation".into()
}

/// List recent wallet operations.
#[derive(Deserialize, JsonSchema)]
struct ListOperationsArgs {
    /// Max operations to return (default 10)
    #[serde(default = "default_limit")]
    limit: u64,
}

fn default_limit() -> u64 {
    10
}

/// Show the wallet's LNURL receive code.
#[derive(Deserialize, JsonSchema)]
struct ShowReceiveCodeArgs {}

/// Load a skill's full prompt by slug.
#[derive(Deserialize, JsonSchema)]
struct LoadSkillArgs {
    /// Skill identifier
    slug: String,
}

/// Evaluate a mathematical expression. Supports +, -, *, /, parentheses, and ^ (power). Example: "1500 * 0.03 + 20"
#[derive(Deserialize, JsonSchema)]
struct CalculateArgs {
    /// The mathematical expression to evaluate
    expression: String,
}

/// Convert an amount in satoshis to a fiat currency using live exchange rates.
#[derive(Deserialize, JsonSchema)]
struct ConvertSatsArgs {
    /// Amount in satoshis
    amount_sats: u64,
    /// Target currency code, e.g. "USD", "EUR", "GBP"
    currency: String,
}

/// Make an HTTP request. Defaults to GET with no headers or body.
#[derive(Deserialize, JsonSchema)]
struct HttpRequestArgs {
    /// Absolute URL to request
    url: String,
    /// HTTP method such as GET, POST, PUT, PATCH, or DELETE. Defaults to GET.
    #[serde(default = "default_http_method")]
    method: String,
    /// Optional request headers
    #[serde(default)]
    headers: BTreeMap<String, String>,
    /// Optional request body. Strings are sent as-is; objects and arrays are sent as JSON.
    #[serde(default)]
    body: Option<serde_json::Value>,
}

fn default_http_method() -> String {
    "GET".into()
}

/// Store, load, or delete a JSON value in browser localStorage. Use this for persistent credentials or config.
#[derive(Deserialize, JsonSchema)]
struct KvStoreArgs {
    /// Action to perform: set, get, delete, or list. Defaults to set.
    #[serde(default = "default_kv_action")]
    action: String,
    /// Storage key. Required for set, get, and delete.
    #[serde(default)]
    key: Option<String>,
    /// JSON value to store. Required for set.
    #[serde(default)]
    value: Option<serde_json::Value>,
    /// Optional prefix filter for list.
    #[serde(default)]
    prefix: Option<String>,
}

fn default_kv_action() -> String {
    "set".into()
}

/// Manage the skill catalog. Custom skills are saved in browser localStorage and merged with shipped defaults by slug.
#[derive(Deserialize, JsonSchema)]
struct SkillsArgs {
    /// Action to perform: list, save, or delete. Defaults to list.
    #[serde(default = "default_skills_action")]
    action: String,
    /// Skill identifier. Required for save and delete.
    #[serde(default)]
    slug: Option<String>,
    /// Skill title. Required for save.
    #[serde(default)]
    title: Option<String>,
    /// Short human-readable summary. Required for save.
    #[serde(default)]
    summary: Option<String>,
    /// Full prompt or skill content. Required for save.
    #[serde(default)]
    prompt: Option<String>,
}

fn default_skills_action() -> String {
    "list".into()
}

// ── Schema → ToolDefinition helper ──────────────────────────────────────

fn tool_definition<A: JsonSchema>(name: &str) -> ToolDefinition {
    let root = schemars::schema_for!(A);
    let mut schema = serde_json::to_value(&root).unwrap_or(json!({}));

    // Extract the description from the root schema (populated from doc comments)
    let description = schema
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();

    // Remove fields that aren't part of the parameters object schema
    if let Some(obj) = schema.as_object_mut() {
        obj.remove("$schema");
        obj.remove("title");
        obj.remove("description");
    }

    ToolDefinition {
        name: name.into(),
        description,
        parameters: schema,
    }
}

#[derive(Debug)]
struct ToolCallMessage(String);

impl std::fmt::Display for ToolCallMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ToolCallMessage {}

fn tool_call_error<E>(error: E) -> ToolError
where
    E: Into<anyhow::Error>,
{
    ToolError::ToolCallError(Box::new(ToolCallMessage(error.into().to_string())))
}

// ── WalletTool enum ─────────────────────────────────────────────────────

enum WalletTool {
    GetBalance {
        wallet: WalletRuntime,
        log: Rc<ToolLog>,
    },
    CreateInvoice {
        wallet: WalletRuntime,
        log: Rc<ToolLog>,
    },
    ProposePayment {
        log: Rc<ToolLog>,
    },
    ListOperations {
        wallet: WalletRuntime,
        log: Rc<ToolLog>,
    },
    ShowReceiveCode {
        wallet: WalletRuntime,
        log: Rc<ToolLog>,
    },
    LoadSkill {
        skills: Vec<SkillSummary>,
        log: Rc<ToolLog>,
    },
    Calculate {
        log: Rc<ToolLog>,
    },
    ConvertSats {
        log: Rc<ToolLog>,
    },
    HttpRequest {
        log: Rc<ToolLog>,
    },
    KvStore {
        log: Rc<ToolLog>,
    },
    Skills {
        log: Rc<ToolLog>,
    },
}

impl ToolDyn for WalletTool {
    fn name(&self) -> String {
        match self {
            Self::GetBalance { .. } => "get_balance",
            Self::CreateInvoice { .. } => "create_invoice",
            Self::ProposePayment { .. } => "propose_payment",
            Self::ListOperations { .. } => "list_operations",
            Self::ShowReceiveCode { .. } => "show_receive_code",
            Self::LoadSkill { .. } => "load_skill",
            Self::Calculate { .. } => "calculate",
            Self::ConvertSats { .. } => "convert_sats",
            Self::HttpRequest { .. } => "http_request",
            Self::KvStore { .. } => "kv_store",
            Self::Skills { .. } => "skills",
        }
        .into()
    }

    fn definition<'a>(
        &'a self,
        _prompt: String,
    ) -> rig::wasm_compat::WasmBoxedFuture<'a, ToolDefinition> {
        let def = match self {
            Self::GetBalance { .. } => tool_definition::<GetBalanceArgs>("get_balance"),
            Self::CreateInvoice { .. } => tool_definition::<CreateInvoiceArgs>("create_invoice"),
            Self::ProposePayment { .. } => tool_definition::<ProposePaymentArgs>("propose_payment"),
            Self::ListOperations { .. } => tool_definition::<ListOperationsArgs>("list_operations"),
            Self::ShowReceiveCode { .. } => {
                tool_definition::<ShowReceiveCodeArgs>("show_receive_code")
            }
            Self::LoadSkill { .. } => tool_definition::<LoadSkillArgs>("load_skill"),
            Self::Calculate { .. } => tool_definition::<CalculateArgs>("calculate"),
            Self::ConvertSats { .. } => tool_definition::<ConvertSatsArgs>("convert_sats"),
            Self::HttpRequest { .. } => tool_definition::<HttpRequestArgs>("http_request"),
            Self::KvStore { .. } => tool_definition::<KvStoreArgs>("kv_store"),
            Self::Skills { .. } => tool_definition::<SkillsArgs>("skills"),
        };
        Box::pin(async move { def })
    }

    fn call<'a>(
        &'a self,
        args: String,
    ) -> rig::wasm_compat::WasmBoxedFuture<'a, Result<String, ToolError>> {
        Box::pin(async move {
            match self {
                Self::GetBalance { wallet, log } => {
                    log.push(ChatRole::Tool, "[tool call] get_balance".into());
                    let balance = wallet.get_balance().await.map_err(tool_call_error)?;
                    let result = format!("{balance} sats");
                    log.push(ChatRole::Tool, format!("get_balance => {result}"));
                    Ok(result)
                }
                Self::CreateInvoice { wallet, log } => {
                    let args: CreateInvoiceArgs =
                        serde_json::from_str(&args).map_err(ToolError::JsonError)?;
                    log.push(
                        ChatRole::Tool,
                        format!("[tool call] create_invoice({})", args.amount_sats),
                    );
                    let invoice = wallet
                        .create_invoice(args.amount_sats, &args.description)
                        .await
                        .map_err(tool_call_error)?;
                    let result = serde_json::to_string(&invoice).map_err(ToolError::JsonError)?;
                    log.push(ChatRole::Tool, format!("create_invoice => {result}"));
                    Ok(result)
                }
                Self::ProposePayment { log } => {
                    let args: ProposePaymentArgs =
                        serde_json::from_str(&args).map_err(ToolError::JsonError)?;
                    log.push(
                        ChatRole::Tool,
                        format!("[tool call] propose_payment({})", args.summary),
                    );
                    let proposal = PendingPaymentProposal {
                        payment: args.payment,
                        amount_sats: args.amount_sats,
                        summary: args.summary,
                    };
                    let result = json!({
                        "status": "pending_confirmation",
                        "summary": proposal.summary,
                        "amount_sats": proposal.amount_sats,
                        "payment": proposal.payment,
                    })
                    .to_string();
                    log.pending_payment.replace(Some(proposal));
                    log.push(ChatRole::Tool, format!("propose_payment => {result}"));
                    Ok(result)
                }
                Self::ListOperations { wallet, log } => {
                    let args: ListOperationsArgs =
                        serde_json::from_str(&args).map_err(ToolError::JsonError)?;
                    log.push(
                        ChatRole::Tool,
                        format!("[tool call] list_operations({})", args.limit),
                    );
                    let result = wallet
                        .list_operations(args.limit as usize)
                        .await
                        .map_err(tool_call_error)?;
                    log.push(ChatRole::Tool, format!("list_operations => {result}"));
                    Ok(result)
                }
                Self::ShowReceiveCode { wallet, log } => {
                    log.push(ChatRole::Tool, "[tool call] show_receive_code".into());
                    let result = wallet
                        .cached_receive_code()
                        .await
                        .map_err(tool_call_error)?
                        .unwrap_or_else(|| "No receive code available yet".into());
                    log.push(ChatRole::Tool, format!("show_receive_code => {result}"));
                    Ok(result)
                }
                Self::Calculate { log } => {
                    let args: CalculateArgs =
                        serde_json::from_str(&args).map_err(ToolError::JsonError)?;
                    log.push(
                        ChatRole::Tool,
                        format!("[tool call] calculate({})", args.expression),
                    );
                    let result =
                        crate::calc::evaluate(&args.expression).map_err(tool_call_error)?;
                    let result_str = format!("{result}");
                    log.push(ChatRole::Tool, format!("calculate => {result_str}"));
                    Ok(result_str)
                }
                Self::ConvertSats { log } => {
                    let args: ConvertSatsArgs =
                        serde_json::from_str(&args).map_err(ToolError::JsonError)?;
                    let currency = args.currency.to_uppercase();
                    log.push(
                        ChatRole::Tool,
                        format!(
                            "[tool call] convert_sats({}, {})",
                            args.amount_sats, currency
                        ),
                    );
                    let result = convert_sats_to_currency(args.amount_sats, &currency)
                        .await
                        .map_err(tool_call_error)?;
                    log.push(ChatRole::Tool, format!("convert_sats => {result}"));
                    Ok(result)
                }
                Self::HttpRequest { log } => {
                    let args: HttpRequestArgs =
                        serde_json::from_str(&args).map_err(ToolError::JsonError)?;
                    let method = args.method.to_uppercase();
                    log.push(
                        ChatRole::Tool,
                        format!("[tool call] http_request({}, {})", method, args.url),
                    );
                    let result = http_request(&args.url, &method, &args.headers, args.body)
                        .await
                        .map_err(tool_call_error)?;
                    log.push(ChatRole::Tool, format!("http_request => {result}"));
                    Ok(result)
                }
                Self::KvStore { log } => {
                    let args: KvStoreArgs =
                        serde_json::from_str(&args).map_err(ToolError::JsonError)?;
                    let action = args.action.to_lowercase();
                    log.push(
                        ChatRole::Tool,
                        format!(
                            "[tool call] kv_store({}, key={:?}, prefix={:?})",
                            action, args.key, args.prefix
                        ),
                    );
                    let result = kv_store(&action, args.key.as_deref(), args.value, args.prefix)
                        .map_err(tool_call_error)?;
                    log.push(ChatRole::Tool, format!("kv_store => {result}"));
                    Ok(result)
                }
                Self::Skills { log } => {
                    let args: SkillsArgs =
                        serde_json::from_str(&args).map_err(ToolError::JsonError)?;
                    let action = args.action.to_lowercase();
                    log.push(
                        ChatRole::Tool,
                        format!("[tool call] skills({}, {:?})", action, args.slug),
                    );
                    let result =
                        manage_skills(&action, args.slug, args.title, args.summary, args.prompt)
                            .await
                            .map_err(tool_call_error)?;
                    log.push(ChatRole::Tool, format!("skills => {result}"));
                    Ok(result)
                }
                Self::LoadSkill { skills, log } => {
                    let args: LoadSkillArgs =
                        serde_json::from_str(&args).map_err(ToolError::JsonError)?;
                    log.push(
                        ChatRole::Tool,
                        format!("[tool call] load_skill({})", args.slug),
                    );
                    let custom_skill = load_custom_skills()
                        .map_err(tool_call_error)?
                        .into_iter()
                        .find(|skill| skill.slug == args.slug);
                    let skill = skills.iter().find(|s| s.slug == args.slug);
                    let result = if let Some(custom_skill) = custom_skill {
                        custom_skill.prompt
                    } else if let Some(skill) = skill {
                        if let Some(path) = &skill.path {
                            reqwest::get(&asset_url(path).map_err(tool_call_error)?)
                                .await
                                .map_err(tool_call_error)?
                                .error_for_status()
                                .map_err(tool_call_error)?
                                .text()
                                .await
                                .map_err(tool_call_error)?
                        } else {
                            serde_json::to_string(skill).map_err(ToolError::JsonError)?
                        }
                    } else {
                        return Err(tool_call_error(anyhow::anyhow!(
                            "Unknown skill: {}",
                            args.slug
                        )));
                    };
                    log.push(ChatRole::Tool, format!("load_skill => {result}"));
                    Ok(result)
                }
            }
        })
    }
}

// ── Sats conversion ─────────────────────────────────────────────────────

const PRICE_FEED_URL: &str = "https://price-feed.dev.fedibtc.com/latest";

async fn convert_sats_to_currency(amount_sats: u64, currency: &str) -> anyhow::Result<String> {
    let resp: serde_json::Value = reqwest::get(PRICE_FEED_URL)
        .await?
        .error_for_status()?
        .json()
        .await?;

    let prices = resp
        .get("prices")
        .ok_or_else(|| anyhow::anyhow!("Missing 'prices' in response"))?;

    let btc_usd = prices
        .get("BTC/USD")
        .and_then(|v| v.get("rate"))
        .and_then(|v| v.as_f64())
        .ok_or_else(|| anyhow::anyhow!("BTC/USD rate not found"))?;

    let btc_value = amount_sats as f64 / 100_000_000.0;

    if currency == "BTC" {
        return Ok(json!({
            "amount_sats": amount_sats,
            "amount_btc": btc_value,
            "currency": "BTC"
        })
        .to_string());
    }

    let usd_value = btc_value * btc_usd;

    if currency == "USD" {
        return Ok(json!({
            "amount_sats": amount_sats,
            "amount_usd": format!("{usd_value:.2}"),
            "currency": "USD",
            "btc_usd_rate": btc_usd
        })
        .to_string());
    }

    // For other currencies, look up CURRENCY/USD rate and convert
    let key = format!("{currency}/USD");
    let currency_per_usd = prices
        .get(&key)
        .and_then(|v| v.get("rate"))
        .and_then(|v| v.as_f64())
        .ok_or_else(|| anyhow::anyhow!("Currency {currency} not found in price feed"))?;

    // rate is CURRENCY/USD, meaning 1 unit of currency = rate USD
    // So to convert USD to currency: amount_currency = usd_value / currency_per_usd
    let converted = usd_value / currency_per_usd;

    Ok(json!({
        "amount_sats": amount_sats,
        "converted": format!("{converted:.2}"),
        "currency": currency,
        "btc_usd_rate": btc_usd,
        "currency_usd_rate": currency_per_usd
    })
    .to_string())
}

// ── HTTP request ────────────────────────────────────────────────────────

async fn http_request(
    url: &str,
    method: &str,
    headers: &BTreeMap<String, String>,
    body: Option<serde_json::Value>,
) -> anyhow::Result<String> {
    let method = Method::from_bytes(method.as_bytes())
        .with_context(|| format!("Invalid HTTP method: {method}"))?;

    let mut request = reqwest::Client::new()
        .request(method.clone(), url)
        .headers(build_headers(headers)?);

    if let Some(body) = body {
        request = match body {
            serde_json::Value::String(text) => request.body(text),
            other => request.json(&other),
        };
    }

    let response = request.send().await?;
    let status = response.status();
    let response_headers = header_map_to_json(response.headers());
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let text = response.text().await?;
    let body = parse_response_body(content_type.as_deref(), &text);

    Ok(json!({
        "url": url,
        "method": method.as_str(),
        "status": status.as_u16(),
        "ok": status.is_success(),
        "content_type": content_type,
        "headers": response_headers,
        "body": body,
    })
    .to_string())
}

fn build_headers(headers: &BTreeMap<String, String>) -> anyhow::Result<HeaderMap> {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .with_context(|| format!("Invalid header name: {name}"))?;
        let value = HeaderValue::from_str(value)
            .with_context(|| format!("Invalid header value for {name}"))?;
        map.insert(name, value);
    }
    Ok(map)
}

fn header_map_to_json(headers: &HeaderMap) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                value.to_str().unwrap_or("<non-utf8>").to_owned(),
            )
        })
        .collect()
}

fn parse_response_body(content_type: Option<&str>, text: &str) -> serde_json::Value {
    let is_json = content_type
        .map(|value| {
            let value = value.to_ascii_lowercase();
            value.contains("application/json") || value.contains("+json")
        })
        .unwrap_or(false);

    if is_json {
        serde_json::from_str(text).unwrap_or_else(|_| serde_json::Value::String(text.to_owned()))
    } else {
        serde_json::Value::String(text.to_owned())
    }
}

// ── Local storage KV ────────────────────────────────────────────────────

fn kv_store(
    action: &str,
    key: Option<&str>,
    value: Option<serde_json::Value>,
    prefix: Option<String>,
) -> anyhow::Result<String> {
    match action {
        "set" => {
            let key = key.ok_or_else(|| anyhow::anyhow!("kv_store set requires a key"))?;
            let value = value.ok_or_else(|| anyhow::anyhow!("kv_store set requires a value"))?;
            LocalStorage::set(key, &value)?;
            Ok(json!({
                "action": "set",
                "key": key,
                "stored": true,
            })
            .to_string())
        }
        "get" => {
            let key = key.ok_or_else(|| anyhow::anyhow!("kv_store get requires a key"))?;
            let value = LocalStorage::get::<serde_json::Value>(key).ok();
            Ok(json!({
                "action": "get",
                "key": key,
                "found": value.is_some(),
                "value": value,
            })
            .to_string())
        }
        "delete" => {
            let key = key.ok_or_else(|| anyhow::anyhow!("kv_store delete requires a key"))?;
            LocalStorage::delete(key);
            Ok(json!({
                "action": "delete",
                "key": key,
                "deleted": true,
            })
            .to_string())
        }
        "list" => {
            let keys = list_local_storage_keys(prefix.as_deref())?;
            Ok(json!({
                "action": "list",
                "prefix": prefix,
                "count": keys.len(),
                "keys": keys,
            })
            .to_string())
        }
        _ => Err(anyhow::anyhow!(
            "Unsupported kv_store action: {action}. Use set, get, delete, or list."
        )),
    }
}

fn list_local_storage_keys(prefix: Option<&str>) -> anyhow::Result<Vec<String>> {
    let storage = window()
        .ok_or_else(|| anyhow::anyhow!("window is unavailable"))?
        .local_storage()
        .map_err(|err| anyhow::anyhow!(format!("{err:?}")))?
        .ok_or_else(|| anyhow::anyhow!("localStorage is unavailable"))?;

    let mut keys = Vec::new();
    for idx in 0..storage
        .length()
        .map_err(|err| anyhow::anyhow!(format!("{err:?}")))?
    {
        if let Some(key) = storage
            .key(idx)
            .map_err(|err| anyhow::anyhow!(format!("{err:?}")))?
        {
            if prefix.is_none_or(|prefix| key.starts_with(prefix)) {
                keys.push(key);
            }
        }
    }
    keys.sort();
    Ok(keys)
}

// ── Skills ──────────────────────────────────────────────────────────────

async fn manage_skills(
    action: &str,
    slug: Option<String>,
    title: Option<String>,
    summary: Option<String>,
    prompt: Option<String>,
) -> anyhow::Result<String> {
    match action {
        "list" => Ok(json!({
            "action": "list",
            "skills": load_skills().await?,
        })
        .to_string()),
        "save" => {
            let stored = StoredSkill {
                slug: slug.ok_or_else(|| anyhow::anyhow!("skills save requires a slug"))?,
                title: title.ok_or_else(|| anyhow::anyhow!("skills save requires a title"))?,
                summary: summary
                    .ok_or_else(|| anyhow::anyhow!("skills save requires a summary"))?,
                prompt: prompt.ok_or_else(|| anyhow::anyhow!("skills save requires a prompt"))?,
            };
            let mut custom_skills = load_custom_skills()?;
            match custom_skills
                .iter_mut()
                .find(|skill| skill.slug == stored.slug)
            {
                Some(existing) => *existing = stored.clone(),
                None => custom_skills.push(stored.clone()),
            }
            save_custom_skills(&custom_skills)?;
            let skills = load_skills().await?;
            Ok(json!({
                "action": "save",
                "skill": stored.summary(),
                "skills": skills,
            })
            .to_string())
        }
        "delete" => {
            let slug = slug.ok_or_else(|| anyhow::anyhow!("skills delete requires a slug"))?;
            let mut custom_skills = load_custom_skills()?;
            let original_len = custom_skills.len();
            custom_skills.retain(|skill| skill.slug != slug);
            save_custom_skills(&custom_skills)?;
            let skills = load_skills().await?;
            Ok(json!({
                "action": "delete",
                "slug": slug,
                "deleted": custom_skills.len() != original_len,
                "skills": skills,
            })
            .to_string())
        }
        _ => Err(anyhow::anyhow!(
            "Unsupported skills action: {action}. Use list, save, or delete."
        )),
    }
}

fn load_custom_skills() -> anyhow::Result<Vec<StoredSkill>> {
    Ok(LocalStorage::get::<Vec<StoredSkill>>(CUSTOM_SKILLS_KEY).unwrap_or_default())
}

fn save_custom_skills(skills: &[StoredSkill]) -> anyhow::Result<()> {
    if skills.is_empty() {
        LocalStorage::delete(CUSTOM_SKILLS_KEY);
    } else {
        LocalStorage::set(CUSTOM_SKILLS_KEY, skills)?;
    }
    Ok(())
}

fn merge_skill_summaries(
    defaults: Vec<SkillSummary>,
    custom: Vec<StoredSkill>,
) -> Vec<SkillSummary> {
    let mut merged = defaults;
    for stored in custom {
        let summary = stored.summary();
        match merged.iter_mut().find(|skill| skill.slug == summary.slug) {
            Some(existing) => *existing = summary,
            None => merged.push(summary),
        }
    }
    merged
}

// ── WalletAgent ─────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct WalletAgent {
    wallet: WalletRuntime,
    ppq_api_key: String,
    skills: Vec<SkillSummary>,
}

impl WalletAgent {
    pub fn new(
        wallet: WalletRuntime,
        _ppq: crate::ppq::PpqClient,
        ppq_api_key: String,
        skills: Vec<SkillSummary>,
    ) -> Self {
        Self {
            wallet,
            ppq_api_key,
            skills,
        }
    }

    pub async fn respond(
        &self,
        history: &[ChatMessage],
        prompt: &str,
    ) -> anyhow::Result<AgentResponse> {
        let log = ToolLog::new();

        let skills = load_skills().await.unwrap_or_else(|_| self.skills.clone());
        let skills_ctx = serde_json::to_string(&skills).unwrap_or_else(|_| "[]".into());
        let preamble = format!("{PREAMBLE}\n\nSkills available: {skills_ctx}");

        let client: openai::CompletionsClient = openai::CompletionsClient::builder()
            .api_key(&self.ppq_api_key)
            .base_url(PPQ_API_BASE)
            .build()
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let tools: Vec<Box<dyn ToolDyn>> = vec![
            Box::new(WalletTool::GetBalance {
                wallet: self.wallet.clone(),
                log: Rc::clone(&log),
            }),
            Box::new(WalletTool::CreateInvoice {
                wallet: self.wallet.clone(),
                log: Rc::clone(&log),
            }),
            Box::new(WalletTool::ProposePayment {
                log: Rc::clone(&log),
            }),
            Box::new(WalletTool::ListOperations {
                wallet: self.wallet.clone(),
                log: Rc::clone(&log),
            }),
            Box::new(WalletTool::ShowReceiveCode {
                wallet: self.wallet.clone(),
                log: Rc::clone(&log),
            }),
            Box::new(WalletTool::LoadSkill {
                skills: skills.clone(),
                log: Rc::clone(&log),
            }),
            Box::new(WalletTool::Calculate {
                log: Rc::clone(&log),
            }),
            Box::new(WalletTool::ConvertSats {
                log: Rc::clone(&log),
            }),
            Box::new(WalletTool::HttpRequest {
                log: Rc::clone(&log),
            }),
            Box::new(WalletTool::KvStore {
                log: Rc::clone(&log),
            }),
            Box::new(WalletTool::Skills {
                log: Rc::clone(&log),
            }),
        ];

        let agent = client
            .agent(MODEL)
            .preamble(&preamble)
            .default_max_turns(10)
            .temperature(0.2)
            .tools(tools)
            .build();

        let chat_history = history
            .iter()
            .filter_map(|m| match m.role {
                ChatRole::User => Some(Message::user(m.body.clone())),
                ChatRole::Assistant => Some(Message::assistant(m.body.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();

        let response = agent
            .chat(prompt, chat_history)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let mut outputs = log.outputs.take();
        outputs.push(ChatMessage {
            role: ChatRole::Assistant,
            body: response,
        });

        Ok(AgentResponse {
            messages: outputs,
            pending_payment: log.pending_payment.take(),
        })
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

pub async fn load_skills() -> anyhow::Result<Vec<SkillSummary>> {
    let defaults = load_default_skills().await?;
    let custom = load_custom_skills()?;
    Ok(merge_skill_summaries(defaults, custom))
}

async fn load_default_skills() -> anyhow::Result<Vec<SkillSummary>> {
    let response = reqwest::get(&asset_url("skills/index.json")?)
        .await?
        .error_for_status()?;
    Ok(response.json().await?)
}

fn asset_url(path: &str) -> anyhow::Result<String> {
    if path.starts_with("http://") || path.starts_with("https://") {
        return Ok(path.to_owned());
    }

    let window = window().ok_or_else(|| anyhow::anyhow!("window is unavailable"))?;
    let origin = window
        .location()
        .origin()
        .map_err(|err| anyhow::anyhow!(format!("{err:?}")))?;
    let pathname = window
        .location()
        .pathname()
        .map_err(|err| anyhow::anyhow!(format!("{err:?}")))?;
    let mut base_path = if pathname.ends_with('/') {
        pathname
    } else {
        match pathname.rfind('/') {
            Some(idx) => pathname[..=idx].to_owned(),
            None => "/".to_owned(),
        }
    };

    if !base_path.starts_with('/') {
        base_path.insert(0, '/');
    }

    let clean = path.trim_start_matches('/');
    Ok(format!("{origin}{base_path}{clean}"))
}

pub fn onboarding_message(body: impl Into<String>) -> ChatMessage {
    ChatMessage {
        role: ChatRole::System,
        body: body.into(),
    }
}

pub fn user_message(body: impl Into<String>) -> ChatMessage {
    ChatMessage {
        role: ChatRole::User,
        body: body.into(),
    }
}

pub fn assistant_message(body: impl Into<String>) -> ChatMessage {
    ChatMessage {
        role: ChatRole::Assistant,
        body: body.into(),
    }
}

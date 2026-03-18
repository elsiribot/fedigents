use std::cell::RefCell;
use std::rc::Rc;

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
                    let balance = wallet
                        .get_balance()
                        .await
                        .map_err(|e| ToolError::ToolCallError(e.into()))?;
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
                        .map_err(|e| ToolError::ToolCallError(e.into()))?;
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
                        .map_err(|e| ToolError::ToolCallError(e.into()))?;
                    log.push(ChatRole::Tool, format!("list_operations => {result}"));
                    Ok(result)
                }
                Self::ShowReceiveCode { wallet, log } => {
                    log.push(ChatRole::Tool, "[tool call] show_receive_code".into());
                    let result = wallet
                        .cached_receive_code()
                        .await
                        .map_err(|e| ToolError::ToolCallError(e.into()))?
                        .unwrap_or_else(|| "No receive code available yet".into());
                    log.push(ChatRole::Tool, format!("show_receive_code => {result}"));
                    Ok(result)
                }
                Self::LoadSkill { skills, log } => {
                    let args: LoadSkillArgs =
                        serde_json::from_str(&args).map_err(ToolError::JsonError)?;
                    log.push(
                        ChatRole::Tool,
                        format!("[tool call] load_skill({})", args.slug),
                    );
                    let skill = skills
                        .iter()
                        .find(|s| s.slug == args.slug)
                        .ok_or_else(|| {
                            ToolError::ToolCallError(
                                anyhow::anyhow!("Unknown skill: {}", args.slug).into(),
                            )
                        })?;
                    let result = if let Some(path) = &skill.path {
                        reqwest::get(
                            &asset_url(path).map_err(|e| ToolError::ToolCallError(e.into()))?,
                        )
                        .await
                        .map_err(|e| ToolError::ToolCallError(Box::new(e)))?
                        .error_for_status()
                        .map_err(|e| ToolError::ToolCallError(Box::new(e)))?
                        .text()
                        .await
                        .map_err(|e| ToolError::ToolCallError(Box::new(e)))?
                    } else {
                        serde_json::to_string(skill).map_err(ToolError::JsonError)?
                    };
                    log.push(ChatRole::Tool, format!("load_skill => {result}"));
                    Ok(result)
                }
            }
        })
    }
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

        let skills_ctx = serde_json::to_string(&self.skills).unwrap_or_else(|_| "[]".into());
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
                skills: self.skills.clone(),
                log: Rc::clone(&log),
            }),
        ];

        let agent = client
            .agent(MODEL)
            .preamble(&preamble)
            .default_max_turns(4)
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

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use rig::completion::{Chat, Message, ToolDefinition};
use rig::prelude::*;
use rig::providers::openai;
use rig::tool::Tool;
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

#[derive(Debug)]
struct ToolError(anyhow::Error);

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#}", self.0)
    }
}

impl std::error::Error for ToolError {}

impl From<anyhow::Error> for ToolError {
    fn from(err: anyhow::Error) -> Self {
        Self(err)
    }
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

// ── GetBalance ──────────────────────────────────────────────────────────

struct GetBalanceTool {
    wallet: WalletRuntime,
    log: Rc<ToolLog>,
}

#[derive(Deserialize)]
struct GetBalanceArgs {}

impl Tool for GetBalanceTool {
    const NAME: &'static str = "get_balance";
    type Error = ToolError;
    type Args = GetBalanceArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "get_balance".into(),
            description: "Get the current wallet balance in sats.".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    async fn call(&self, _args: Self::Args) -> Result<String, ToolError> {
        self.log.push(ChatRole::Tool, "[tool call] get_balance".into());
        let balance = self.wallet.get_balance().await.map_err(ToolError)?;
        let result = format!("{balance} sats");
        self.log.push(ChatRole::Tool, format!("get_balance => {result}"));
        Ok(result)
    }
}

// ── CreateInvoice ───────────────────────────────────────────────────────

struct CreateInvoiceTool {
    wallet: WalletRuntime,
    log: Rc<ToolLog>,
}

#[derive(Deserialize)]
struct CreateInvoiceArgs {
    amount_sats: u64,
    #[serde(default = "default_description")]
    description: String,
}

fn default_description() -> String {
    "Fedigents request".into()
}

impl Tool for CreateInvoiceTool {
    const NAME: &'static str = "create_invoice";
    type Error = ToolError;
    type Args = CreateInvoiceArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "create_invoice".into(),
            description: "Create a BOLT11 invoice to receive a payment.".into(),
            parameters: json!({
                "type": "object",
                "required": ["amount_sats"],
                "properties": {
                    "amount_sats": { "type": "integer", "description": "Amount in satoshis" },
                    "description": { "type": "string", "description": "Invoice description" }
                }
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<String, ToolError> {
        self.log.push(ChatRole::Tool, format!("[tool call] create_invoice({})", args.amount_sats));
        let invoice = self.wallet.create_invoice(args.amount_sats, &args.description).await.map_err(ToolError)?;
        let result = serde_json::to_string(&invoice).map_err(|e| ToolError(e.into()))?;
        self.log.push(ChatRole::Tool, format!("create_invoice => {result}"));
        Ok(result)
    }
}

// ── ProposePayment ──────────────────────────────────────────────────────

struct ProposePaymentTool {
    log: Rc<ToolLog>,
}

#[derive(Deserialize)]
struct ProposePaymentArgs {
    payment: String,
    amount_sats: Option<u64>,
    #[serde(default = "default_payment_summary")]
    summary: String,
}

fn default_payment_summary() -> String {
    "Lightning payment awaiting confirmation".into()
}

impl Tool for ProposePaymentTool {
    const NAME: &'static str = "propose_payment";
    type Error = ToolError;
    type Args = ProposePaymentArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "propose_payment".into(),
            description: "Propose an outgoing Lightning payment for user confirmation. The UI shows a confirm button; only that button sends funds.".into(),
            parameters: json!({
                "type": "object",
                "required": ["payment", "summary"],
                "properties": {
                    "payment": { "type": "string", "description": "BOLT11 invoice or LNURL" },
                    "amount_sats": { "type": "integer", "description": "Amount in sats (required for amountless invoices/LNURL)" },
                    "summary": { "type": "string", "description": "Short human-readable description" }
                }
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<String, ToolError> {
        self.log.push(ChatRole::Tool, format!("[tool call] propose_payment({})", args.summary));
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
        }).to_string();
        self.log.pending_payment.replace(Some(proposal));
        self.log.push(ChatRole::Tool, format!("propose_payment => {result}"));
        Ok(result)
    }
}

// ── ListOperations ──────────────────────────────────────────────────────

struct ListOperationsTool {
    wallet: WalletRuntime,
    log: Rc<ToolLog>,
}

#[derive(Deserialize)]
struct ListOperationsArgs {
    #[serde(default = "default_limit")]
    limit: u64,
}

fn default_limit() -> u64 {
    10
}

impl Tool for ListOperationsTool {
    const NAME: &'static str = "list_operations";
    type Error = ToolError;
    type Args = ListOperationsArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "list_operations".into(),
            description: "List recent wallet operations.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "description": "Max operations to return (default 10)" }
                }
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<String, ToolError> {
        self.log.push(ChatRole::Tool, format!("[tool call] list_operations({})", args.limit));
        let result = self.wallet.list_operations(args.limit as usize).await.map_err(ToolError)?;
        self.log.push(ChatRole::Tool, format!("list_operations => {result}"));
        Ok(result)
    }
}

// ── ShowReceiveCode ─────────────────────────────────────────────────────

struct ShowReceiveCodeTool {
    wallet: WalletRuntime,
    log: Rc<ToolLog>,
}

#[derive(Deserialize)]
struct ShowReceiveCodeArgs {}

impl Tool for ShowReceiveCodeTool {
    const NAME: &'static str = "show_receive_code";
    type Error = ToolError;
    type Args = ShowReceiveCodeArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "show_receive_code".into(),
            description: "Show the wallet's LNURL receive code.".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    async fn call(&self, _args: Self::Args) -> Result<String, ToolError> {
        self.log.push(ChatRole::Tool, "[tool call] show_receive_code".into());
        let result = self.wallet.cached_receive_code().await.map_err(ToolError)?
            .unwrap_or_else(|| "No receive code available yet".into());
        self.log.push(ChatRole::Tool, format!("show_receive_code => {result}"));
        Ok(result)
    }
}

// ── LoadSkill ───────────────────────────────────────────────────────────

struct LoadSkillTool {
    skills: Vec<SkillSummary>,
    log: Rc<ToolLog>,
}

#[derive(Deserialize)]
struct LoadSkillArgs {
    slug: String,
}

impl Tool for LoadSkillTool {
    const NAME: &'static str = "load_skill";
    type Error = ToolError;
    type Args = LoadSkillArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "load_skill".into(),
            description: "Load a skill's full prompt by slug.".into(),
            parameters: json!({
                "type": "object",
                "required": ["slug"],
                "properties": {
                    "slug": { "type": "string", "description": "Skill identifier" }
                }
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<String, ToolError> {
        self.log.push(ChatRole::Tool, format!("[tool call] load_skill({})", args.slug));
        let skill = self.skills.iter()
            .find(|s| s.slug == args.slug)
            .ok_or_else(|| ToolError(anyhow::anyhow!("Unknown skill: {}", args.slug)))?;
        let result = if let Some(path) = &skill.path {
            reqwest::get(&asset_url(path).map_err(ToolError)?)
                .await.map_err(|e| ToolError(e.into()))?
                .error_for_status().map_err(|e| ToolError(e.into()))?
                .text().await.map_err(|e| ToolError(e.into()))?
        } else {
            serde_json::to_string(skill).map_err(|e| ToolError(e.into()))?
        };
        self.log.push(ChatRole::Tool, format!("load_skill => {result}"));
        Ok(result)
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

        let agent = client
            .agent(MODEL)
            .preamble(&preamble)
            .default_max_turns(4)
            .temperature(0.2)
            .tool(GetBalanceTool { wallet: self.wallet.clone(), log: Rc::clone(&log) })
            .tool(CreateInvoiceTool { wallet: self.wallet.clone(), log: Rc::clone(&log) })
            .tool(ProposePaymentTool { log: Rc::clone(&log) })
            .tool(ListOperationsTool { wallet: self.wallet.clone(), log: Rc::clone(&log) })
            .tool(ShowReceiveCodeTool { wallet: self.wallet.clone(), log: Rc::clone(&log) })
            .tool(LoadSkillTool { skills: self.skills.clone(), log: Rc::clone(&log) })
            .build();

        let chat_history = history.iter().filter_map(|m| match m.role {
            ChatRole::User => Some(Message::user(m.body.clone())),
            ChatRole::Assistant => Some(Message::assistant(m.body.clone())),
            _ => None,
        }).collect::<Vec<_>>();

        let response = agent.chat(prompt, chat_history).await
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

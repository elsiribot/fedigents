use serde::{Deserialize, Serialize};

use crate::fedimint::WalletRuntime;
use crate::ppq::{PpqClient, PpqMessage};

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

#[derive(Clone)]
pub struct WalletAgent {
    wallet: WalletRuntime,
    ppq: PpqClient,
    ppq_api_key: String,
    skills: Vec<SkillSummary>,
}

#[derive(Debug, Deserialize)]
struct AgentPlan {
    assistant: String,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Deserialize)]
struct ToolCall {
    tool: String,
    #[serde(default)]
    arguments: serde_json::Value,
}

impl WalletAgent {
    pub fn new(
        wallet: WalletRuntime,
        ppq: PpqClient,
        ppq_api_key: String,
        skills: Vec<SkillSummary>,
    ) -> Self {
        Self {
            wallet,
            ppq,
            ppq_api_key,
            skills,
        }
    }

    pub async fn respond(&self, history: &[ChatMessage], _prompt: &str) -> anyhow::Result<AgentResponse> {
        let mut transcript = history.to_vec();

        let mut outputs = Vec::new();
        let mut pending_payment = None;
        for _ in 0..4 {
            let request = build_messages(&transcript, &self.skills);
            let raw = self.ppq.chat(&self.ppq_api_key, &request).await?;
            let plan = parse_plan(&raw)?;

            if !plan.assistant.trim().is_empty() {
                let message = ChatMessage {
                    role: ChatRole::Assistant,
                    body: plan.assistant.clone(),
                };
                transcript.push(message.clone());
                outputs.push(message);
            }

            if plan.tool_calls.is_empty() || plan.done {
                return Ok(AgentResponse {
                    messages: outputs,
                    pending_payment,
                });
            }

            for tool_call in plan.tool_calls {
                let result = self.execute_tool(&tool_call).await?;
                let tool_message = ChatMessage {
                    role: ChatRole::Tool,
                    body: format!("{} => {}", tool_call.tool, result.summary()),
                };
                if let ToolOutcome::PendingPayment(proposal) = &result {
                    pending_payment = Some(proposal.clone());
                }
                transcript.push(tool_message.clone());
                outputs.push(tool_message);
            }
        }

        outputs.push(ChatMessage {
            role: ChatRole::Assistant,
            body: "I hit my tool step limit. Please refine the request or try again.".to_owned(),
        });
        Ok(AgentResponse {
            messages: outputs,
            pending_payment,
        })
    }

    async fn execute_tool(&self, tool_call: &ToolCall) -> anyhow::Result<ToolOutcome> {
        match tool_call.tool.as_str() {
            "get_balance" => {
                let balance = self.wallet.get_balance().await?;
                Ok(ToolOutcome::Message(format!("{} sats", balance.sats_round_down())))
            }
            "create_invoice" => {
                let amount_sats = read_u64(&tool_call.arguments, "amount_sats")?;
                let description = tool_call
                    .arguments
                    .get("description")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("Fedigents request");
                let invoice = self.wallet.create_invoice(amount_sats, description).await?;
                Ok(ToolOutcome::Message(serde_json::to_string(&invoice)?))
            }
            "pay_lightning" | "propose_payment" => {
                let payment = read_string(&tool_call.arguments, "payment")?;
                let amount_sats = tool_call
                    .arguments
                    .get("amount_sats")
                    .and_then(serde_json::Value::as_u64);
                let summary = tool_call
                    .arguments
                    .get("summary")
                    .or_else(|| tool_call.arguments.get("description"))
                    .or_else(|| tool_call.arguments.get("request"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .unwrap_or("Lightning payment awaiting confirmation")
                    .to_owned();
                Ok(ToolOutcome::PendingPayment(PendingPaymentProposal {
                    payment: payment.to_owned(),
                    amount_sats,
                    summary,
                }))
            }
            "list_operations" => {
                let limit = tool_call
                    .arguments
                    .get("limit")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(10) as usize;
                Ok(ToolOutcome::Message(self.wallet.list_operations(limit).await?))
            }
            "show_receive_code" => Ok(ToolOutcome::Message(
                self.wallet
                    .cached_receive_code()
                    .await?
                    .unwrap_or_else(|| "No receive code available yet".to_owned()),
            )),
            "load_skill" => {
                let slug = read_string(&tool_call.arguments, "slug")?;
                let skill = self
                    .skills
                    .iter()
                    .find(|skill| skill.slug == slug)
                    .ok_or_else(|| anyhow::anyhow!("Unknown skill: {slug}"))?;
                if let Some(path) = &skill.path {
                    let body = reqwest::get(path).await?.error_for_status()?.text().await?;
                    Ok(ToolOutcome::Message(body))
                } else {
                    Ok(ToolOutcome::Message(serde_json::to_string(skill)?))
                }
            }
            other => Err(anyhow::anyhow!("Unknown tool: {other}")),
        }
    }
}

fn build_messages(history: &[ChatMessage], skills: &[SkillSummary]) -> Vec<PpqMessage> {
    let system_prompt = format!(
        concat!(
            "You are the wallet agent inside a chat-only Fedimint wallet. ",
            "All wallet actions must happen through tools. Keep answers short and practical. ",
            "Available tools: get_balance, create_invoice, propose_payment, pay_lightning, list_operations, show_receive_code, load_skill. ",
            "For outgoing payments, never execute a payment directly in chat. Use propose_payment (or pay_lightning for compatibility) to create a pending confirmation with payment, amount_sats when known, and a short summary. ",
            "The UI shows a confirm button and only that button can actually send funds. ",
            "When you need a tool, respond with strict JSON in this shape: ",
            "{{\"assistant\":\"short text\",\"done\":false,\"tool_calls\":[{{\"tool\":\"name\",\"arguments\":{{}}}}]}}. ",
            "When you are done and do not need tools, respond with strict JSON in this shape: ",
            "{{\"assistant\":\"final text\",\"done\":true,\"tool_calls\":[]}}. ",
            "Never wrap JSON in markdown. Skills available: {}"
        ),
        serde_json::to_string(skills).unwrap_or_else(|_| "[]".to_owned())
    );

    let mut messages = vec![PpqMessage {
        role: "system".to_owned(),
        content: system_prompt,
    }];

    messages.extend(history.iter().map(|message| PpqMessage {
        role: match message.role {
            ChatRole::System => "system",
            ChatRole::User => "user",
            ChatRole::Assistant => "assistant",
            ChatRole::Tool => "tool",
        }
        .to_owned(),
        content: message.body.clone(),
    }));

    messages
}

#[derive(Clone, Debug)]
enum ToolOutcome {
    Message(String),
    PendingPayment(PendingPaymentProposal),
}

impl ToolOutcome {
    fn summary(&self) -> String {
        match self {
            Self::Message(message) => message.clone(),
            Self::PendingPayment(proposal) => serde_json::json!({
                "status": "pending_confirmation",
                "summary": proposal.summary,
                "amount_sats": proposal.amount_sats,
                "payment": proposal.payment,
            })
            .to_string(),
        }
    }
}

fn parse_plan(raw: &str) -> anyhow::Result<AgentPlan> {
    serde_json::from_str(raw).or_else(|_| {
        let trimmed = raw.trim();
        let start = trimmed.find('{').ok_or_else(|| anyhow::anyhow!("No JSON object returned by the agent"))?;
        let end = trimmed.rfind('}').ok_or_else(|| anyhow::anyhow!("No JSON object returned by the agent"))?;
        serde_json::from_str(&trimmed[start..=end]).map_err(Into::into)
    })
}

fn read_string<'a>(value: &'a serde_json::Value, key: &str) -> anyhow::Result<&'a str> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("Missing string argument: {key}"))
}

fn read_u64(value: &serde_json::Value, key: &str) -> anyhow::Result<u64> {
    value
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("Missing numeric argument: {key}"))
}

pub async fn load_skills() -> anyhow::Result<Vec<SkillSummary>> {
    let response = reqwest::get("/skills/index.json").await?.error_for_status()?;
    Ok(response.json().await?)
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

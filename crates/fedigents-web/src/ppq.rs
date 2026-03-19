use fedimint_core::encoding::{Decodable, Encodable};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const API_BASE: &str = "https://api.ppq.ai";

#[derive(Clone, Debug, Serialize, Deserialize, Encodable, Decodable)]
pub struct PpqAccount {
    pub credit_id: String,
    pub api_key: String,
}

#[derive(Clone, Debug)]
pub struct PpqTopup {
    pub invoice: String,
}

#[derive(Clone, Debug)]
pub struct PpqBalance {
    pub amount_usd: f64,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct PpqModel {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub popular: bool,
    #[serde(default)]
    pub pricing: PpqModelPricing,
    #[serde(default)]
    pub owned_by: String,
    #[serde(rename = "type", default)]
    pub model_type: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[allow(non_snake_case)]
pub struct PpqModelPricing {
    #[serde(default)]
    pub input_per_1M_tokens: f64,
    #[serde(default)]
    pub output_per_1M_tokens: f64,
}

impl PpqModel {
    pub fn is_expensive(&self) -> bool {
        self.pricing.output_per_1M_tokens >= 8.0
    }

}

#[derive(Clone, Debug, Serialize)]
pub struct PpqMessage {
    pub role: String,
    pub content: String,
}

#[derive(Clone, Debug)]
pub struct PpqClient {
    client: reqwest::Client,
}

impl PpqClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    pub async fn create_account(&self) -> anyhow::Result<PpqAccount> {
        let response = self
            .client
            .post(format!("{API_BASE}/accounts/create"))
            .send()
            .await?
            .error_for_status()?;

        let body: Value = response.json().await?;
        let object = unwrap_data(&body);
        let account = PpqAccount {
            credit_id: get_string(object, &["credit_id", "creditId"])?.to_owned(),
            api_key: get_string(object, &["api_key", "apiKey"])?.to_owned(),
        };

        Ok(account)
    }

    pub async fn create_lightning_topup(
        &self,
        account: &PpqAccount,
        amount_usd: f64,
    ) -> anyhow::Result<PpqTopup> {
        let response = self
            .client
            .post(format!("{API_BASE}/topup/create/btc-lightning"))
            .headers(auth_headers(&account.api_key)?)
            .json(&json!({
                "amount": amount_usd,
                "currency": "USD"
            }))
            .send()
            .await?
            .error_for_status()?;

        let body: Value = response.json().await?;
        let object = unwrap_data(&body);
        let invoice = get_string(
            object,
            &[
                "invoice",
                "bolt11",
                "payment_request",
                "paymentRequest",
                "lightning_invoice",
                "lightningInvoice",
            ],
        )?;
        Ok(PpqTopup {
            invoice: invoice.to_owned(),
        })
    }

    pub async fn balance(&self, account: &PpqAccount) -> anyhow::Result<PpqBalance> {
        let response = self
            .client
            .post(format!("{API_BASE}/credits/balance"))
            .json(&json!({
                "credit_id": account.credit_id,
            }))
            .send()
            .await?
            .error_for_status()?;

        let body: Value = response.json().await?;
        let object = unwrap_data(&body);
        let amount_usd = get_f64(object, &["balance", "usd_balance", "amount", "credits"])?;
        Ok(PpqBalance { amount_usd })
    }

    pub async fn list_models(&self) -> anyhow::Result<Vec<PpqModel>> {
        let response = self
            .client
            .get(format!("{API_BASE}/models"))
            .send()
            .await?
            .error_for_status()?;
        let body: Value = response.json().await?;
        let all: Vec<PpqModel> = serde_json::from_value(
            body.get("data").cloned().unwrap_or(Value::Array(vec![])),
        )?;
        Ok(all
            .into_iter()
            .filter(|m| m.model_type == "chat" && m.pricing.input_per_1M_tokens >= 0.0)
            .collect())
    }

    pub async fn chat(&self, api_key: &str, messages: &[PpqMessage]) -> anyhow::Result<String> {
        let response = self
            .client
            .post(format!("{API_BASE}/v1/chat/completions"))
            .headers(auth_headers(api_key)?)
            .json(&json!({
                "model": "claude-haiku-4.5",
                "messages": messages,
                "temperature": 0.2
            }))
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable response body>".to_owned());
            anyhow::bail!("PPQ chat error {status}: {body}");
        }

        let body: Value = response.json().await?;
        let content = body
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("content"))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("PPQ response did not contain message content"))?;

        match content {
            Value::String(text) => Ok(text),
            Value::Array(parts) => {
                let text = parts
                    .into_iter()
                    .filter_map(|part| {
                        part.get("text")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned)
                            .or_else(|| part.as_str().map(ToOwned::to_owned))
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if text.is_empty() {
                    Err(anyhow::anyhow!("PPQ response content array was empty"))
                } else {
                    Ok(text)
                }
            }
            other => Err(anyhow::anyhow!("Unsupported PPQ content payload: {other}")),
        }
    }
}

fn auth_headers(api_key: &str) -> anyhow::Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let value = HeaderValue::from_str(&format!("Bearer {api_key}"))?;
    headers.insert(AUTHORIZATION, value);
    Ok(headers)
}

fn unwrap_data(value: &Value) -> &Value {
    value.get("data").unwrap_or(value)
}

fn get_string<'a>(value: &'a Value, keys: &[&str]) -> anyhow::Result<&'a str> {
    for key in keys {
        if let Some(found) = value.get(key).and_then(Value::as_str) {
            return Ok(found);
        }
    }
    Err(anyhow::anyhow!(
        "Missing expected PPQ field; looked for one of: {}",
        keys.join(", ")
    ))
}

fn get_f64(value: &Value, keys: &[&str]) -> anyhow::Result<f64> {
    for key in keys {
        if let Some(found) = value.get(key) {
            if let Some(number) = found.as_f64() {
                return Ok(number);
            }
            if let Some(text) = found.as_str() {
                if let Ok(number) = text.parse::<f64>() {
                    return Ok(number);
                }
            }
        }
    }
    Err(anyhow::anyhow!(
        "Missing expected numeric PPQ field; looked for one of: {}",
        keys.join(", ")
    ))
}

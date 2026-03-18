# Generate images with PPQ

When the user wants to generate or edit images:

## Step 1 — Get or create a PPQ account

Use `kv_store` with `action: "get"`, `key: "PPQ_ACCOUNT"`.

If missing, create one:

```
http_request
  url: https://api.ppq.ai/accounts/create
  method: POST
  headers: {"Content-Type": "application/json"}
```

The response contains `api_key` (starts with `sk-`) and `credit_id`. Save the full object:

```
kv_store  action: "set", key: "PPQ_ACCOUNT", value: <the response object>
```

## Step 2 — Check balance

**Every** PPQ API call after account creation needs these headers:

```
headers: {
  "Authorization": "Bearer <api_key from PPQ_ACCOUNT>",
  "Content-Type": "application/json"
}
```

Check the balance:

```
http_request
  url: https://api.ppq.ai/credits/balance
  method: POST
  headers: {"Authorization": "Bearer <api_key>", "Content-Type": "application/json"}
  body: {"credit_id": "<credit_id from PPQ_ACCOUNT>"}
```

## Step 3 — Top up if needed

If the balance is too low (image generation typically costs $0.01–$0.10), create a Lightning invoice to top up:

```
http_request
  url: https://api.ppq.ai/topup/create/btc-lightning
  method: POST
  headers: {"Authorization": "Bearer <api_key>", "Content-Type": "application/json"}
  body: {"amount": <cents>, "credit_id": "<credit_id>"}
```

Then use `propose_payment` with the returned Lightning invoice so the user can confirm.

## Step 4 — Generate the image

```
http_request
  url: https://api.ppq.ai/v1/images/generations
  method: POST
  headers: {"Authorization": "Bearer <api_key>", "Content-Type": "application/json"}
  body: {"model": "openai/gpt-5-image", "prompt": "<user prompt>", "size": "1024x1024", "n": 1}
```

Default model: `openai/gpt-5-image`. The response contains `data[].url` — return the image URL(s) to the user. Mention that signed URLs expire after ~24 hours.

If PPQ returns 402, explain the balance is insufficient and repeat the top-up flow.

## Important

- **Always include both `Authorization` and `Content-Type` headers** on every PPQ API call.
- Do not ask the user to create a PPQ account manually.
- If a model is rejected, query `GET https://api.ppq.ai/v1/models?type=image` (with auth headers) to find alternatives.

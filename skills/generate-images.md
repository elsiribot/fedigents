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
  body: {"model": "<model>", "prompt": "<user prompt>", "quality": "low", "n": 1}
```

### Available models

| Model ID | Description | Quality options |
|---|---|---|
| `gpt-image-1` | GPT Image 1 (4o) | low, medium, high |
| `gpt-image-1.5` | GPT Image 1.5 | medium, high |
| `nano-banana-pro` | Gemini 3.0 | standard, 4k |
| `flux-2-pro` | Flux 2 Pro | 1k, 2k |
| `flux-2-flex` | Flux 2 Flex | 1k, 2k |
| `flux-2-pro-i2i` | Flux 2 Pro Image-to-Image | requires `image_url` param |
| `flux-kontext-pro` | Flux Kontext Pro | flat pricing |
| `flux-kontext-max` | Flux Kontext Max | flat pricing |

### Parameters

- **model** (required) — one of the models above
- **prompt** (required) — text description of the image
- **n** — number of images, 1–10 (default 1)
- **quality** — quality tier, model-specific (see table above)
- **size** — image size or aspect ratio (e.g. `"1:1"`, `"16:9"`), model-specific
- **image_url** — source image URL, required for `flux-2-pro-i2i`

### Defaults

- Default model: `gpt-image-1` with quality `"low"` (smaller download size)
- Unless the user asks for a specific model, use `gpt-image-1`
- For image-to-image requests, use `flux-2-pro-i2i` with the `image_url` parameter

### Response

The response contains `data[].url` with signed image URLs. Return them as markdown images so the user sees them inline. Mention that signed URLs expire after ~24 hours.

Example response:
```json
{
  "created": 1709234567,
  "model": "gpt-image-1",
  "cost": 0.0805,
  "data": [{"url": "https://api.ppq.ai/v1/media/gen_abc123/0?sig=...&exp=..."}]
}
```

If PPQ returns 402, explain the balance is insufficient and repeat the top-up flow.

To check current model availability and pricing, query:
```
http_request
  url: https://api.ppq.ai/v1/media/models
  method: GET
  headers: {"Authorization": "Bearer <api_key>", "Content-Type": "application/json"}
```

## Important

- **Always include both `Authorization` and `Content-Type` headers** on every PPQ API call.
- Do not ask the user to create a PPQ account manually.
- Return image URLs as `![image](url)` markdown so they render inline.

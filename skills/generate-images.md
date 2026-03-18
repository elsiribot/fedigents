# Generate images with PPQ

When the user wants to generate or edit images with PPQ:

1. Check `kv_store` with `action: "get"` and `key: "PPQ_ACCOUNT"`.
2. If `PPQ_ACCOUNT` is missing, create one yourself with `http_request`:
   `POST https://api.ppq.ai/accounts/create`
3. Save the full account object returned by PPQ into `kv_store` with `action: "set"` and `key: "PPQ_ACCOUNT"`.
4. Use the saved `api_key` from `PPQ_ACCOUNT` for all PPQ API requests with the header:
   `Authorization: Bearer <api_key>`
5. Before generating, query:
   `GET https://api.ppq.ai/v1/models?type=image`
   Use this to find the current model id, pricing, and supported image options.
6. Unless the user specifies a model, default to `nano banana`. Because model ids can change, first look for the current image model whose id or name matches `nano banana`. If that exact model is unavailable, pick the closest current image model and say which one you used.
7. Check PPQ account credit balance with:
   `POST https://api.ppq.ai/credits/balance`
   Body:
   `{"credit_id":"<credit_id from PPQ_ACCOUNT>"}`
8. Compare the current balance to the selected model's current pricing from the models endpoint. If balance is too low, do not try image generation yet.
9. If balance is too low, create a Lightning top-up invoice with:
   `POST https://api.ppq.ai/topup/create/btc-lightning`
   Use the PPQ API key for authorization and choose a top-up amount that safely covers the estimated image cost.
10. Use `propose_payment` with the returned invoice so the wallet UI can ask the user to confirm the top-up payment.
11. After the user confirms and the balance should be funded, continue with image generation:
   `POST https://api.ppq.ai/v1/images/generations`
12. Send a JSON body with at least:
   `model`, `prompt`, and a reasonable default `size` such as `1024x1024`.
13. If the model is rejected or the user asks for alternatives, query the models endpoint again, choose a current image model, and tell the user which one you used.
14. Return the generated image URL or URLs clearly. Mention that signed PPQ image URLs expire after about 24 hours.
15. If PPQ returns `402 Payment Required`, explain that the PPQ account still needs more credits and repeat the top-up flow instead of failing silently.

Be direct. Do not ask the user to create a PPQ account manually unless the PPQ API itself fails. Always check pricing and balance dynamically instead of assuming old prices.

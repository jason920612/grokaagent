# ADR 0002 — xAI Grok 以 OAuth 登入

- 日期：2026-08-13
- 狀態：已確認

## 決策

第一個模型適配器走 **xAI 訂閱 OAuth**（RFC 8628 device code），不是 `XAI_API_KEY`。

- 授權：`https://auth.x.ai/oauth2/device/code` + `/oauth2/token`
- 公開 client（無 secret）：Grok CLI allowlist `b1a00492-073a-47ea-816f-4c329264a828`
- 推論：`POST https://cli-chat-proxy.grok.com/v1/responses`（訂閱面）。`api.x.ai` 對同一顆訂閱 token 常回 402/403。
- 憑證：`~/.grokaagent/xai-auth.json`（`GROKA_XAI_AUTH_FILE` 可改），atomic write；token 不進 log。
- refresh token 每次都會旋轉，必須把新的寫回，否則下次 refresh 失敗。

## 後果

- 本機 `grokaagent login` 開瀏覽器核准即可，不必 console API key。
- CLI proxy 會檢查 Grok-CLI 身分標頭（`X-XAI-Token-Auth` 等）；OAuth 端點則用本專案自己的 User-Agent。
- 帳號若未被允許走訂閱推論面，login 成功也會在推論時 402/403；那是 xAI 權限，不是登入壞了。

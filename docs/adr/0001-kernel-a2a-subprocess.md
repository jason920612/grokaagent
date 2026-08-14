# ADR 0001 — Kernel, A2A, 子行程

- 日期：2026-08-13
- 狀態：已確認

## 已鎖定

| 決策 | 選擇 | 後果 |
| --- | --- | --- |
| 語言 | Rust（tokio） | 單 crate 模組化；先不拆 workspace |
| 對話協議 | A2A v1（ACP 的活規格） | 型別用官方 `a2a-lf` / `a2a-client-lf` / `a2a-server-lf`。不做歷史 ACP REST，除非日後明確要 shim |
| 子 agent 隔離 | 子行程 | 子崩潰不拖垮父；Windows 用 Job Object、Unix 用 process group 回收孫行程 |
| 對話形態 | 樹狀委派 + 多方 session，**同一套 A2A `Message`** | `spawn_agent` 與 `send_message` 不分裂型別 |
| 監控 | 事件管道，模型自己寫腳本掛上 | 框架不提供監控 UI；腳本失敗不得殺死 run |

## 核心與邊界

核心只做：事件迴圈、指令、本機工具、觀測伺服器端工具、把 typed event 寫進管道。

A2A 是適配層，不是核心。每個 agent 行程都是一個 A2A server（loopback）。父用 A2A client 對子說話；遠端對等方同一套 client。

內部不發明第二套「對話物件」。委派與輪流發言都是 `Message` + `Task` + `contextId`。

- 樹：新子行程、新 `Task`，等到 `COMPLETED` / `FAILED` / `CANCELED`，收回 artifacts。
- Session：共用 `contextId`，父把 `Message` 轉給指定子行程。子進入 `INPUT_REQUIRED` 時暫停，由父模型決定回覆或轉給另一個 agent。
- 沒有內建議長。要議長就再 spawn 一個名叫 moderator 的 agent。

框架不自動拆工。要不要開子 agent 由父模型決定。

## 子行程握手（本機）

協議走 A2A HTTP，不走 stdio RPC（避免跟事件 JSONL 搶 stdout）。

1. 父 spawn 同一支 binary：`grokaagent worker --name <id> --depth <n> --parent-run <id> --listen 127.0.0.1:0 --events <path>`
2. 子綁定 loopback 隨機埠，stdout **第一行**輸出握手 JSON，然後不再寫協議流量：
   `{"v":1,"agent_card_url":"http://127.0.0.1:<port>/.well-known/agent-card.json"}`
3. 父在逾時內讀到握手，抓 Agent Card，之後只走 A2A（`message/send`、stream、`tasks/cancel`）。
4. 子的 kernel 事件寫到 `--events` 的 JSONL。父可 tail 後轉發到自己的管道（帶 `parent_run_id`）。
5. 取消：先 `CancelTask`，寬限後殺行程（含 Job Object / process group）。

遠端 agent：不 spawn，只吃對方的 Agent Card URL。

## 硬限制（不是建議）

- `max_depth` 預設 2（worker 收到的 `--depth` 用完就不能再 spawn）
- `max_children` 預設 4（同一父）
- 每個子 Task 有 timeout
- 子的工具集是允許清單，預設比父窄
- v1 與父共用 workspace，不做 filesystem jail（之後可加）

## 事件

每筆事件必帶 `ts`、`agent_name`、`run_id`，子事件再帶 `parent_run_id`。管道是 JSONL 檔（run 目錄）與可選的 stdout/named pipe。Hook 是外部行程讀這個流；核心不解釋腳本內容。

## 假設（未反對就照此做）

- 第一個模型適配器：xAI（專案名 grokaagent）；`Provider` trait 可換
- 官方 A2A crate 若 API 不穩，就自己 serde 對規格，仍不引入社群 fork
- Zed ACP 不是本階段範圍（那是編輯器適配器）

## 不做

CrewAI 式編排器、監控產品、Rust plugin、歷史 ACP REST、深層 agent 管 agent。

## 下一步切片

1. 單一 agent：事件迴圈 + 本機工具 + 事件 JSONL（還不要 spawn）
2. 同一 binary 的 `worker`：loopback A2A server + 握手
3. 父 `spawn_agent`：子行程、一個 Task、收回 artifact
4. 同一 `contextId` 對兩個 worker 轉送 `Message`（session）

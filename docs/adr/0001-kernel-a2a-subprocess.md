# ADR 0001 — Kernel, A2A, 子行程

- 日期：2026-08-13
- 更新日期：2026-09-24
- 狀態：已確認

## 已鎖定

| 決策 | 選擇 | 後果 |
| --- | --- | --- |
| 語言 | Rust（tokio） | 單 crate 模組化；先不拆 workspace |
| 對話協議 | A2A v1（ACP 的活規格） | 目標仍用官方 `a2a-lf` / `a2a-client-lf` / `a2a-server-lf`。目前實作為手寫子集（見「缺口」）。不做歷史 ACP REST，除非日後明確要 shim |
| 子 agent 隔離 | 子行程 | 子崩潰不拖垮父；Windows 用 Job Object、Unix 用 process group 回收孫行程 |
| 對話形態 | 樹狀委派 + 多方 session，**同一套 A2A `Message`** | `spawn_agent` 與 `send_message` 不分裂型別 |
| 監控 | 事件管道，模型自己寫腳本掛上 | 框架不提供監控產品 UI；腳本失敗不得殺死 run。可選的 Hub web 鏡像是 TUI 操作面，不是監控產品 |
| 子工具集 | 與父相同（同一 `kit` 工具表） | 僅以 `max_depth`／`max_children` 限制委派規模；不再維持「比父窄的允許清單」 |
| Task mode | **正式例外**：可選監督者 checklist | 不是通用編排器／內建議長；只在使用者開啟 task mode 時，對 stop／wait 再打一輪無工具監督 JSON |

## 核心與邊界

核心只做：事件迴圈、指令、本機工具、觀測伺服器端工具、把 typed event 寫進管道。

A2A 是適配層，不是核心。每個 agent 行程都是一個 A2A server（loopback）。父用 A2A client 對子說話；遠端對等方同一套 client（遠端 Card 見缺口）。

內部不發明第二套「對話物件」。委派與輪流發言都是 `Message` + `Task` + `contextId`。

- 樹：新子行程，裡面是**一個長壽 session**。`spawn_agent` **立即回傳**；每則 A2A `Message` 都進同一個 session 的 inbox（保留歷史、孫代理與背景），每則對應一個 `Task`，在子 session 下一次閒置時完成。
- 回報：父端 tail 子的事件檔，從子自己的事件（`run_id` 相符、`path` 為空）追蹤狀態。子閒置時，回覆以 `[child agent \`name\` replied]` 推進父 session（與背景結束通知同一條通道，會喚醒等待中的父）；若父正以 `wait_agents` 等這個子，改由該呼叫收下，不重複推送。
- 管理工具：`spawn_agent`、`send_message`（立即回傳）、`wait_agents`、`list_agents`、`stop_agent`（中斷目前回合，或 `kill=true` 結束行程並釋放名額）。父按 Esc 時，正在工作的子也會被中斷（保留 session，不喚醒父）。
- Session：共用 `contextId`，父把 `Message` 轉給指定子行程。子進入 `INPUT_REQUIRED` 時暫停、由父模型決定回覆或轉給另一個 agent——**尚未實作**（見缺口）。
- 沒有內建議長。要議長就再 spawn 一個名叫 moderator 的 agent。
- Task mode 監督者不是議長：它只產出 checklist／continue／complete／impossible，不轉送多方 Message。

框架不自動拆工。要不要開子 agent 由父模型決定。

## 子行程握手（本機）

協議走 A2A HTTP，不走 stdio RPC（避免跟事件 JSONL 搶 stdout）。

1. 父 spawn 同一支 binary：`grokaagent worker --name <id> --depth <n> --parent-run <id> --run-id <uuid> --listen 127.0.0.1:0 --events <dir>/groka-children/<name>-<run8>.jsonl`（每次 spawn 一個新檔，不重播舊事件）
2. 子綁定 loopback 隨機埠，stdout **第一行**輸出握手 JSON，然後不再寫協議流量：
   `{"v":1,"agent_card_url":"http://127.0.0.1:<port>/.well-known/agent-card.json"}`
3. 父在逾時內讀到握手，抓 Agent Card，之後只走 A2A（`message/send`、`tasks/cancel`）。
4. 子的 kernel 事件寫到 `--events` 的 JSONL。父可 tail 後轉發到自己的管道（帶 `parent_run_id`）。
5. 取消：`CancelTask` 中斷子 session 目前的回合（與 Esc 相同），session 保留；要結束行程用 `stop_agent kill=true`（含 Job Object / process group）。

遠端 agent：不 spawn，只吃對方的 Agent Card URL（缺口）。

## 硬限制（不是建議）

- `max_depth` 預設 2（worker 收到的 `--depth` 用完就不能再 spawn）
- `max_children` 預設 4（同一父）
- `max_children` 算的是**存活中的**子（被 kill 或行程結束即釋放）；子行程意外結束會發 `ChildExited` 並通知父
- 等待有上限（`wait_agents` 預設 300s、最多 3600s），逾時只停止等待，不取消子
- 子工具集與父相同；預算只靠 depth／children
- v1 與父共用 workspace，不做 filesystem jail（之後可加）

## 事件

每筆事件必帶 `ts`、`agent_name`、`run_id`，子事件再帶 `parent_run_id`。轉發時每一層把 `path` 前綴上子名稱（`coder`、`coder/lint`），並把 `parent_run_id` 改成轉發者自己的 run，所以最上層看到的每筆後代事件都掛在根 session 下、用 `path` 表示樹。管道是 JSONL 檔（run 目錄）與可選的 stdout/named pipe。Hook 是外部行程讀這個流；核心不解釋腳本內容。

## 假設（未反對就照此做）

- 第一個模型適配器：xAI（專案名 grokaagent）；`Provider` trait 可換
- 官方 A2A crate 若 API 不穩，就自己 serde 對規格，仍不引入社群 fork
- Zed ACP 不是本階段範圍（那是編輯器適配器）

## 不做

CrewAI 式通用編排器、監控產品、Rust plugin、歷史 ACP REST、深層 agent 管 agent。  
（Task mode 監督 checklist 是上表正式例外，不是這條的編排器。）

## 缺口（仍為目標，尚未落地）

- 改用官方 A2A crates（或等價完整規格覆蓋），取代手寫子集
- `INPUT_REQUIRED` + 父轉送多方 session
- 遠端 Agent Card（不 spawn，只吃 URL）
- 串流 A2A（目前 `streaming: false`）

## 下一步切片

1. 單一 agent：事件迴圈 + 本機工具 + 事件 JSONL（還不要 spawn）— 已做
2. 同一 binary 的 `worker`：loopback A2A server + 握手 — 已做
3. 父 `spawn_agent`：子行程、非阻塞啟動、可 cancel — 進行中／已對齊定案
4. 同一 `contextId` 對兩個 worker 轉送 `Message`（session）+ `INPUT_REQUIRED` — 未做

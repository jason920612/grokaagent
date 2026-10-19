# ADR 0003 — 工作流程（workflow）

- 日期：2026-10-18
- 狀態：已確認
- 修訂：ADR 0001「不做：CrewAI 式通用編排器」——工作流程是第二個正式例外（第一個是 task mode 用戶代理）

## 背景

`spawn_agent` 讓主代理一回合一回合地指揮子代理：每個子代理要由模型寫訊息、等回覆、再決定下一步。要做大範圍的掃描、平行審查、對抗式驗證時，這樣做又慢又貴，主代理的上下文也裝不下。主代理更適合事先寫好一段**確定的**編排程式，交給框架執行。

## 決策

| 決策 | 選擇 | 理由 |
| --- | --- | --- |
| 腳本語言 | JavaScript，嵌入 QuickJS（`rquickjs`） | 模型最熟；原生 async/await，`parallel` 就是 `Promise.all`；預設沒有檔案／網路／計時器；約 1–1.5 MB；用 cc 編譯，CI 三個平台本來就有 C 編譯器 |
| 腳本執行位置 | 主程序內，一個獨立執行緒（current-thread tokio runtime） | QuickJS 值不能跨執行緒；中斷器讓停止能打斷忙迴圈；記憶體上限 512 MB |
| 每個 `agent()` | 一次性 worker 子程序（沿用 `grokaagent worker`），A2A 任務結束就 kill | 子代理要真的改檔、跑指令，需要完整 kernel；與 ADR 0001 子行程原則一致；用 A2A task id 對應「這次呼叫」的回覆，避開 nursery「最後回覆」語意 |
| 子代理深度 | 以 `--depth 2` 啟動 | 不能再 spawn、也沒有 workflow 工具：扇出就是腳本寫的那些 |
| 併發 | 每個 run 最多 `CPU 數 − 2`（1 到 16）個同時執行，其餘排隊；每個 run 最多 1000 個代理；`parallel`／`pipeline` 一次最多 4096 項 | 與 Claude Code 的 Workflow 相同 |
| 結構化輸出 | `schema` → worker 多一個 `submit_result` 工具（參數就是 schema），kernel 的「終止工具」：被接受就結束回合，參數即答案；不合就回錯誤讓代理修；沒呼叫就追問兩次，還是沒有則結果為 `null` | 兩種供應商都沒有 `response_format`；驗證用自寫的 JSON Schema 子集（`jschema`） |
| 背景執行 | `workflow` 立即回傳 run id；結束時一則 `[workflow … finished]` 通知（bg 通道），進度只走事件 | 工具呼叫是逐一執行的，阻塞會讓主代理停擺；每則通知都會喚醒模型 |
| Esc | 不停止工作流程 | 跟 `run_background` 一樣；用 `stop_workflow` 停止 |
| 續跑 | 每個 run 在 `.groka/workflow-runs/<id>/` 留 `script.js`、`journal.jsonl`、`result.json`、`agents/*.jsonl`。`resume_from_run_id` 以「內容雜湊 + 第幾次出現」對應已完成的呼叫 | `pipeline` 讓呼叫的發出順序取決於誰先跑完，用位置對應不穩定；上游結果變了，下游 prompt 跟著變而重跑 |
| 可重播 | `Date.now()`、無參數的 `new Date()`、`Math.random()` 會丟錯 | 否則續跑時腳本走不同路徑 |
| 隔離 | `isolation: 'worktree'` → `git worktree add --detach HEAD` 到 `.groka/worktrees/`；沒變更就移除，有變更就留在 `groka-workflow/<名稱>` 分支 | 平行寫入同一工作區會互相覆蓋 |
| Token 預算 | 使用者訊息裡的 `+500k`／`+2m` 設定本回合目標；計算每次模型呼叫（回合、上下文摺疊、審查）的 output tokens + 沒命中快取的 input tokens，主代理、子代理、工作流程代理共用；用完後 `agent()` 丟錯、不再啟動新代理，已在工作的代理會做完（可能超出）；使用者的下一則訊息開始新回合，目標換成它的（或沒有） | 使用者決定的計算方式；Grok 訂閱與按量計費的端點都適用 |
| 何時使用 | 只在使用者選擇時：訊息含「ultracode」、ultracode 模式開啟、使用者明說要工作流程／多代理編排、或指名已存的工作流程 | 與 Claude Code 相同；工作流程很耗資源 |
| 已存的工作流程 | `.groka/workflows/<name>.js`（專案）與 `~/.grokaagent/workflows/<name>.js`（個人）；`workflow({name})` 或腳本內 `workflow(name, args)`（巢狀一層） | |

## 介面

腳本以 `export const meta = { name, description, phases }`（純字面值）開頭，其餘是 async 函式本體。全域：`agent`、`parallel`、`pipeline`、`phase`、`log`、`args`、`budget`、`workflow`。完整撰寫說明在 `src/workflow/reference.md`，模型用 `workflow {"reference": true}` 取得。

## 顯示

工作流程是代理樹根層的一個節點 `wf-<id>`，它的代理掛在底下（`wf-<id>/<label>-<n>`），每個代理有完整的唯讀紀錄；`phase()`／`log()` 寫進節點自己的分頁。新工作流程開始時，只保留最近 5 個已結束的工作流程節點。

## 連帶修正

- Token：解析 output tokens（`CacheUsage.output_tokens`、`ModelFinished.output_tokens`）。
- 重試退避加上隨機延遲，避免平行代理同時撞上限流後同步重試。
- Worker `--lifeline`：stdin 是父程序持有的管線，關閉就結束。子代理（含 nursery 的子代理）不會在父程序被強制結束後繼續執行、繼續呼叫模型。
- 無頭 `run`：還有工作流程在跑時不結束，等它的通知。

## 不做（目前）

- 自訂代理類型（`agentType`）：每個代理都有完整工具，傳了也只會忽略並記一行。
- 跨工作流程共用指令審查快取。
- 從腳本詢問使用者。

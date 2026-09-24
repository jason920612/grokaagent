# grokaagent

Rust agent kernel：事件迴圈、本機工具、xAI Grok OAuth 或 OpenAI 相容 API、A2A 子行程、JSONL 事件管道、終端 TUI。

## 一鍵安裝

不需要先裝 Rust。腳本會下載 GitHub Release 二進位、核對 SHA-256，並放到 `~/.grokaagent/bin`。

**Windows（amd64）** PowerShell：

```powershell
irm https://github.com/jason920612/grokaagent/releases/latest/download/install.ps1 | iex
```

**Linux（amd64）／macOS（Apple Silicon）**：

```sh
curl -fsSL https://github.com/jason920612/grokaagent/releases/latest/download/install.sh | sh
```

新開一個終端，進到專案目錄後執行：

```text
grokaagent
```

目前發佈的平台：Windows amd64、Linux amd64、macOS arm64。可用 `GROKA_INSTALL_DIR` 改安裝路徑。Linux 截圖功能需要系統上的 PipeWire（多數桌面發行版已有）。

## 工作控制台

TUI 與網頁是同一套 VS Code 式工作台：

- **活動列**（最左）：對話、代理、檔案變更、背景工作、任務；底部齒輪開設定。
- **側欄**：目前選的檢視。「代理」以樹狀列出主代理、子代理與孫代理，並顯示狀態（◌ 啟動中、◐ 工作中、✓ 閒置、‖ 暫停、⊘ 已中斷、○ 已結束）。
- **編輯區分頁**：「主對話」加上每個開啟的子代理。子代理分頁是**唯讀**的完整工作紀錄，包含它收到的指示、思考、每次工具呼叫與差異；子代理只由主代理指揮。TUI 的設定也開成一個分頁（`F2` / `Ctrl+G`）。
- **底部面板**：「工具」是所有代理的工具時間軸（點選跳到該次呼叫），「輸出」是背景行程輸出，「事件」記錄啟動、訊息、錯誤。
- **狀態列**：目前進度、代理數、工具數、快取、任務、模型（點選開設定）。

送出方式：

- **接著做**：目前回覆完成後依序處理。
- **調整目前工作**：下一輪模型呼叫時處理，不會撤回已執行的工具操作。
- **停止目前工作**（Esc）：取消目前模型、工具或監督檢查，**正在工作的子代理也會中斷目前回合**（保留記憶，不會被關掉）；待處理佇列保留，等下一個指示。

快捷鍵（TUI 與網頁相同）：`Ctrl+B` 側欄、`Ctrl+J` 面板、`Alt+1`–`Alt+5` 切換側欄檢視、`Alt+0` 回主對話、`Alt+←/→` 切換分頁、`Ctrl+W` 關閉子代理分頁（TUI）。TUI 另有 `F3`／`F4` 對應側欄／面板、`F2` 開關設定分頁；在子代理分頁直接打字會切回主對話的輸入框。`Shift+Enter` 換行，`Ctrl+Enter` 調整目前工作。

網頁以增量同步：只送出有變動的列與面板資料，斷線或漏收時自動重新取得完整狀態。網址可帶 `#side=agents&tab=coder&panel=tools` 直接開到指定檢視。網頁的草稿、分頁、面板與詳情視窗各自保留在瀏覽器分頁中；工作切換、問題回答與圖片附件和終端共用。

子代理：主代理用 `spawn_agent` 開子代理後立即繼續；子代理在自己的行程裡是一個長壽 session，後續 `send_message` 會記得先前內容。子代理閒置時，回覆會自動送回主代理（主代理若在等你，也會被喚醒）；需要同步時主代理用 `wait_agents`，`list_agents` 看狀態，`stop_agent` 中斷或結束（`kill=true` 釋放名額）。

## 自動更新

TUI 啟動時會向 GitHub Releases 查最新版（最多每 6 小時一次）。若有新版本會下載、核對 checksum、取代目前的執行檔，然後重啟。也可手動：

```text
grokaagent update
```

不想自動更新時設 `GROKA_NO_UPDATE=1`。

## 需要

- 一鍵安裝：上述三平台之一
- 從原始碼編譯：Rust 1.75+
- Grok：有效的 SuperGrok / X Premium+（訂閱推論走 `cli-chat-proxy.grok.com`）
- 或自訂 OpenAI 相容端點（設定裡切「自訂 API」，或 `--base-url` / `--model` / `--context`）

## 從原始碼安裝

第一次（在這個 repo）：

```text
cargo install --path . --force --locked
```

之後任意目錄都可以用 `grokaagent`，工作區就是你執行時所在的目錄：

```text
cd D:\work\some-project
grokaagent              # TUI
grokaagent login
grokaagent run "現在 UTC 幾點？請用 now 工具"
```

TUI 對話預設不限輪次（`--max-turns 0`）。若你手動設了上限，互動模式撞到時會暫停等人，不會結束 session。

開發時更新 binary：在 repo 再跑一次 `cargo install --path . --force --locked`，或 `cargo run --release -- install`。發佈版請用 `grokaagent update` 或重跑安裝腳本。

```text
cargo build
cargo test
cargo run -- login
cargo run                 # TUI（無子命令即進入）
cargo run -- tui
cargo run -- run "現在 UTC 幾點？請用 now 工具"
```

子 agent 由模型呼叫 `spawn_agent` / `send_message` 拉起，同一支 binary 的 `worker` 在 loopback 上講 A2A。測試用 echo worker（`--mode echo`，回覆整段對話的使用者訊息），不必打 Grok。子代理事件檔寫在事件檔旁的 `groka-children/`。

登入後 token 存在 `~/.grokaagent/xai-auth.json`（`GROKA_XAI_AUTH_FILE` 可改）。不要 commit。

事件預設寫到目前目錄的 `groka-events.jsonl`。TUI 只是操作面；腳本仍可自己讀 JSONL。

## License

[MIT](LICENSE)

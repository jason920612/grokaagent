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

網頁在寬螢幕顯示工作列表、對話紀錄與工作詳情三欄；窄螢幕可用「對話列表」與「工作詳情」開啟面板。右側提供任務目標、檢查清單、監督結果、子代理／背景工作，以及可點選查看差異的檔案變更。

- **接著做**：目前回覆完成後依序處理。
- **調整目前工作**：下一輪模型呼叫時處理，不會撤回已執行的工具操作。
- **停止目前工作**：取消目前模型、工具或監督檢查；停止後保留待處理佇列，等下一個指示。獨立子代理與背景行程仍可在工作詳情中查看。
- 網頁文字草稿、工具展開、詳情、設定與任務表單各自保留在分頁中；重新整理會清除未送出的草稿。工作切換、問題回答與圖片附件仍共用工作狀態。
- 網頁送出會等待接收確認，斷線時保留文字；重新確認同一則訊息不會重複加入佇列。
- TUI：`F3` 工作列表、`F4` 工作詳情，面板中用方向鍵瀏覽、`Esc` 返回。`Shift+Enter` 換行，`Ctrl+Enter` 調整目前工作，`Esc` 停止。

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

子 agent 由模型呼叫 `spawn_agent` / `send_message` 拉起，同一支 binary 的 `worker` 在 loopback 上講 A2A。測試用 echo worker，不必打 Grok。

登入後 token 存在 `~/.grokaagent/xai-auth.json`（`GROKA_XAI_AUTH_FILE` 可改）。不要 commit。

事件預設寫到目前目錄的 `groka-events.jsonl`。TUI 只是操作面；腳本仍可自己讀 JSONL。

## License

[MIT](LICENSE)

# grokaagent

Rust agent kernel：事件迴圈、本機工具、xAI Grok OAuth、A2A 子行程、JSONL 事件管道、終端 TUI。

## 需要

- Rust 1.75+
- 有效的 SuperGrok / X Premium+（訂閱推論走 `cli-chat-proxy.grok.com`）

## 指令

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

更新 binary：在 repo 再跑一次 `cargo install --path . --force --locked`，或 `cargo run --release -- install`。

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

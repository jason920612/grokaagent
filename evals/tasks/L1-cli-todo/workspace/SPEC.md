# todo.py — 命令列待辦清單規格

以 Python 3（只用標準函式庫）實作單一檔案 `todo.py`。

## 呼叫方式

```
python todo.py [--db PATH] <command> [options]
```

- `--db PATH`：資料檔路徑。未指定時使用與 `todo.py` 同一資料夾的 `todo.json`。
- 資料檔不存在時視為空清單；第一次寫入時建立。
- 所有錯誤訊息寫到 **stderr**，以 `error: ` 開頭，不可印出 Python traceback。

## 資料格式（JSON）

```json
{
  "next_id": 3,
  "tasks": [
    {"id": 1, "title": "買牛奶", "priority": "medium", "due": "2026-10-01", "tags": ["home"], "done": false}
  ]
}
```

- `id` 從 1 開始遞增，**刪除後不重用**（靠 `next_id`）。
- `priority` 為 `low` / `medium` / `high`，預設 `medium`。
- `due` 為 `YYYY-MM-DD` 字串或 `null`。
- `tags` 依加入順序保存，不重複。

## 指令

### add

```
python todo.py add TITLE [--priority P] [--due YYYY-MM-DD] [--tag TAG]...
```

- 成功輸出：`Added #<id>: <title>`，exit code 0。
- `TITLE` 去掉前後空白後不可為空；`--priority` 必須是三種之一；`--due` 必須是合法日期（例如 `2026-02-30` 不合法）。違反時輸出 `error: ...`，exit code **2**，不寫入資料。
- 同一個 tag 重複給只保存一次。

### list

```
python todo.py list [--all] [--priority P] [--tag TAG] [--overdue] [--today YYYY-MM-DD] [--sort id|priority|due]
```

- 預設只列未完成；`--all` 也列已完成。
- `--priority`、`--tag` 為篩選條件（可同時使用，皆需符合）。
- `--overdue`：只列未完成且 `due` 早於「今天」的項目；今天預設為系統日期，可用 `--today` 覆寫（`--today` 也影響 `stats`）。
- 排序：
  - `id`（預設）：id 由小到大。
  - `priority`：high → medium → low，同級依 id。
  - `due`：有 due 的依日期由早到晚，沒有 due 的排最後，同日期依 id。
- 每一項一行，格式：

```
#<id> [<x 或空白>] <title> (<priority>)[ due:<YYYY-MM-DD>][ tags:<tag1>,<tag2>]
```

  例如 `#2 [ ] 繳電費 (high) due:2026-10-05 tags:bill,home`、`#1 [x] 買牛奶 (medium)`。

- 沒有符合的項目時輸出 `No tasks.`，exit code 0。

### done

```
python todo.py done ID
```

- 成功輸出 `Completed #<id>`。已完成的再做一次：輸出 `Task #<id> already completed`，exit code 0。
- 找不到：`error: task #<id> not found`，exit code **1**。

### remove

```
python todo.py remove ID
```

- 成功輸出 `Removed #<id>`；找不到同上（exit 1）。

### edit

```
python todo.py edit ID [--title T] [--priority P] [--due YYYY-MM-DD|none] [--add-tag TAG]... [--remove-tag TAG]...
```

- `--due none` 清除到期日。
- 成功輸出 `Updated #<id>`。
- 沒給任何修改選項：`error: nothing to update`，exit code 2。欄位驗證規則同 `add`（exit 2）。找不到 id：exit 1。

### stats

```
python todo.py stats [--today YYYY-MM-DD]
```

輸出剛好五行：

```
total: <全部>
open: <未完成>
done: <已完成>
overdue: <未完成且 due 早於今天>
by priority: high=<n> medium=<n> low=<n>
```

`by priority` 只計算**未完成**的項目。

## 資料檔損毀

資料檔不是合法 JSON 時，任何指令都輸出 `error: ...` 並以 exit code 1 結束，不可覆寫原檔。

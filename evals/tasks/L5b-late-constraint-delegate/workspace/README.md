# notes

小型筆記工具。資料存在一個 JSON 檔（`--db`，預設 `notes.json`）。

```
python -m notes.cli --db notes.json add 標題 內容
python -m notes.cli --db notes.json list
python -m notes.cli --db notes.json show 1
python -m notes.cli --db notes.json delete 1
```

- `notes/store.py`：資料存取（NoteStore）。
- `notes/cli.py`：命令列介面。
- `legacy_api.py`：給外部帳務系統（billing exporter）用的相容層，函式簽名與行為凍結，外部程式直接 import 它。
- 測試：`python -m unittest discover -s tests -t .`

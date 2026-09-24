# 三個小模組與整合 CLI

全部只用 Python 3 標準函式庫，檔案都放在工作目錄根目錄。

## roman.py

- `to_roman(n: int) -> str`：1 ≤ n ≤ 3999，輸出標準大寫羅馬數字（`1994 → "MCMXCIV"`、`4 → "IV"`、`3999 → "MMMCMXCIX"`）。範圍外或非 int（含 bool）丟 `ValueError`。
- `from_roman(s: str) -> int`：只接受**標準寫法**的大寫羅馬數字，也就是 `to_roman(from_roman(s)) == s` 必須成立。
  非標準寫法（`"IIII"`、`"VX"`、`"IC"`、`"MMMM"`、`"IIV"`）、小寫、空字串、含其他字元都丟 `ValueError`。

## luhn.py

- `is_valid(number: str) -> bool`：Luhn 校驗。可以含空白（忽略）；去掉空白後必須全是數字且長度 ≥ 2，否則回傳 `False`（不丟例外）。
- `check_digit(partial: str) -> str`：回傳要接在 `partial` 後面的一位校驗碼（字串）。`partial` 可含空白（忽略），去掉空白後須為至少 1 位的全數字，否則丟 `ValueError`。
- `complete(partial: str) -> str`：回傳去掉空白的 `partial` 接上校驗碼。例如 `complete("7992739871") == "79927398713"`。

## rle.py

行程長度編碼，每一段寫成「次數 + 字元」，次數一定要寫（1 也要寫）：

- `encode(s: str) -> str`：`"aaab" → "3a1b"`，`"" → ""`，段長可超過 9：12 個 `x` → `"12x"`。
- 字元本身是數字或反斜線 `\` 時，要在字元前加一個反斜線跳脫：`"33" → "2\3"`、`"a\\" (a 後接一個反斜線) → "1a1\\"`。
- `decode(s: str) -> str`：`encode` 的反函式。格式錯誤丟 `ValueError`，包括：缺次數（`"a"`）、次數為 0 或有前導零（`"0a"`、`"03a"`）、結尾只有次數（`"3a2"`）、反斜線後沒有字元（`"2\"`）、數字字元沒跳脫（`"23"` 會被讀成次數 23 然後缺字元，也是錯誤）。
- 任何字串都要滿足 `decode(encode(s)) == s`（含中文、空白、換行）。

## main.py（整合 CLI）

```
python main.py roman to <整數>        → 印出羅馬數字
python main.py roman from <羅馬數字>   → 印出整數
python main.py luhn check <號碼>       → 印出 valid 或 invalid
python main.py luhn complete <號碼>    → 印出補上校驗碼的號碼
python main.py rle encode <文字>       → 印出編碼
python main.py rle decode <編碼>       → 印出解碼結果
```

- 成功時只印結果一行，exit code 0。
- 任何錯誤（格式錯、範圍外、未知子指令）：在 stderr 印出以 `error: ` 開頭的訊息，exit code 1，不可有 traceback。

# ratelimit API 文件

## 概覽

`ratelimit` 是一個在單一行程內使用的限流工具組，用來控制某段程式碼在一段時間內可以被呼叫幾次，例如保護外部 API 不被打爆、限制使用者重試頻率。模組提供兩種演算法：`TokenBucket`（權杖桶，允許短時間爆量、長期平均受限）與 `SlidingWindowCounter`（滑動視窗，任何連續時間區間內的次數都不超過上限）。`parse_rate` 把 `"10/min"` 這種人類可讀的字串轉成（次數, 秒數），方便從設定檔建立限流器；`retry_after` 計算權杖桶還要等幾秒才夠用；`limited` 是裝飾器，把權杖桶套在函式上，額度不足時丟出 `RateLimitExceeded`。所有時間單位都是秒，時鐘預設為 `time.monotonic`，測試時可以注入自訂的 clock 函式來模擬時間流逝。

### RateLimitExceeded

`limited` 裝飾的函式在權杖不足、且 `raise_on_limit=True` 時丟出的例外，繼承自 `Exception`。

**參數**

- `retry_after`（float）：還要等待幾秒才可能有足夠的權杖，會存成同名屬性 `retry_after`。

**回傳**

無（例外類別）。例外訊息格式為 `rate limit exceeded; retry after X.XXXs`。

**例外**

無。建構本身不會丟出例外。

```python
from ratelimit import RateLimitExceeded

try:
    raise RateLimitExceeded(1.5)
except RateLimitExceeded as e:
    print(e.retry_after)  # 1.5
    print(e)
```

### TokenBucket

權杖桶。建立時桶子是滿的，之後以每秒 `refill_rate` 個的速度連續補充，最多補到 `capacity`。

**參數**

- `capacity`（float）：桶子容量，必須大於 0。
- `refill_rate`（float）：每秒補充的權杖數，必須大於或等於 0；為 0 時用完就不再補充。
- `clock`（無參數函式，回傳秒數）：時間來源，預設為 `time.monotonic`。
- 方法 `consume(n=1)`：嘗試取走 `n` 個權杖。
- 屬性 `tokens`：目前可用的權杖數（讀取時會先補充）。

**回傳**

- 建構子回傳 `TokenBucket` 物件。
- `consume(n)` 回傳 `bool`：權杖足夠時扣除並回傳 `True`，不足時不扣除並回傳 `False`。
- `tokens` 回傳 `float`。

**例外**

- 建構時 `capacity <= 0` 丟出 `ValueError`（capacity must be > 0）。
- 建構時 `refill_rate < 0` 丟出 `ValueError`。
- `consume(n)` 在 `n <= 0` 時丟出 `ValueError`；`n` 大於 `capacity` 時也丟出 `ValueError`（永遠不可能滿足）。

```python
from ratelimit import TokenBucket

now = [0.0]
bucket = TokenBucket(capacity=3, refill_rate=1.0, clock=lambda: now[0])
print([bucket.consume() for _ in range(4)])  # [True, True, True, False]
now[0] += 2.0  # 模擬經過 2 秒，補回 2 個權杖
print(bucket.tokens)  # 2.0
try:
    bucket.consume(10)
except ValueError as e:
    print("錯誤：", e)
```

### SlidingWindowCounter

滑動視窗計數器：在任何往回推 `window` 秒的區間內，最多允許 `limit` 次。

**參數**

- `limit`（int）：視窗內允許的最大次數，必須大於或等於 1。
- `window`（float）：視窗長度，單位秒，預設 60 秒（`60.0`）。
- `clock`：時間來源，預設 `time.monotonic`。
- 方法 `hit()`：記錄一次呼叫。
- 方法 `remaining()`：目前視窗內還剩幾次額度。

**回傳**

- `hit()` 回傳 `bool`：未超過上限時記錄並回傳 `True`，否則回傳 `False`（不記錄）。
- `remaining()` 回傳 `int`。

**例外**

- `limit < 1` 丟出 `ValueError`。
- `window <= 0` 丟出 `ValueError`。

```python
from ratelimit import SlidingWindowCounter

now = [100.0]
counter = SlidingWindowCounter(limit=2, window=10, clock=lambda: now[0])
print(counter.hit(), counter.hit(), counter.hit())  # True True False
now[0] += 10  # 最早的紀錄滿 10 秒後離開視窗
print(counter.remaining())  # 2
print(counter.hit())  # True
```

### parse_rate

把 `"次數/[倍數]單位"` 格式的字串解析成 `(次數, 秒數)`，例如 `"10/min"` → `(10, 60.0)`、`"100/5m"` → `(100, 300.0)`。單位不分大小寫，可用 `s`、`sec`、`second`、`m`、`min`、`minute`、`h`、`hour`、`d`、`day`，也接受結尾加 s 的複數形（例如 `hours`）。前後與斜線兩側可以有空白。

**參數**

- `spec`（str）：速率字串。

**回傳**

`tuple[int, float]`：（次數, 期間秒數）。例如 `"3/day"` 回傳 `(3, 86400.0)`，一小時（hour）是 3600 秒。

**例外**

- 格式不符（包含空字串或 `None`）丟出 `ValueError`。
- 未知單位丟出 `ValueError`。
- 次數或倍數為 0 丟出 `ValueError`。

```python
from ratelimit import parse_rate

print(parse_rate("10/min"))    # (10, 60.0)
print(parse_rate("100/5m"))    # (100, 300.0)
print(parse_rate("2 / hours")) # (2, 3600.0)
for bad in ["abc", "5/fortnight", "0/s"]:
    try:
        parse_rate(bad)
    except ValueError as e:
        print("無效：", e)
```

### retry_after

計算權杖桶還要等幾秒才會有 `n` 個權杖。

**參數**

- `bucket`（TokenBucket）：要查詢的權杖桶。
- `n`（float）：需要的權杖數，預設 1。

**回傳**

`float`：需要等待的秒數；權杖已經足夠時回傳 `0.0`；若 `refill_rate` 為 0 且權杖不足，回傳無限大 `float("inf")`。

**例外**

無。本函式不會自行丟出例外（不會檢查 `n` 是否大於容量）。

```python
from ratelimit import TokenBucket, retry_after

now = [0.0]
bucket = TokenBucket(capacity=2, refill_rate=0.5, clock=lambda: now[0])
print(retry_after(bucket))  # 0.0
bucket.consume(2)
print(retry_after(bucket))  # 2.0，每秒補 0.5 個，要 2 秒才有 1 個
frozen = TokenBucket(capacity=1, refill_rate=0, clock=lambda: now[0])
frozen.consume()
print(retry_after(frozen))  # inf
```

### limited

裝飾器工廠：每次呼叫被裝飾的函式時，先從權杖桶扣 `cost` 個權杖。

**參數**

- `bucket`（TokenBucket）：共用的權杖桶。
- `cost`（float，僅限關鍵字）：每次呼叫消耗的權杖數，預設 1。
- `raise_on_limit`（bool，僅限關鍵字）：預設 `True`，權杖不足時丟出例外；設為 `False` 時改為不執行函式並回傳 `None`。

**回傳**

回傳一個裝飾器；被裝飾的函式在額度足夠時回傳原函式的結果，額度不足且 `raise_on_limit=False` 時回傳 `None`。

**例外**

- 權杖不足且 `raise_on_limit=True` 時丟出 `RateLimitExceeded`，其 `retry_after` 屬性為建議等待秒數。
- `cost` 小於等於 0 或大於桶子容量時，呼叫時會由 `TokenBucket.consume` 丟出 `ValueError`。

```python
from ratelimit import TokenBucket, limited, RateLimitExceeded

now = [0.0]
bucket = TokenBucket(capacity=1, refill_rate=1, clock=lambda: now[0])

@limited(bucket)
def ping():
    return "pong"

print(ping())
try:
    ping()
except RateLimitExceeded as e:
    print("被限流，", e.retry_after, "秒後再試")

quiet_bucket = TokenBucket(capacity=1, refill_rate=0, clock=lambda: now[0])

@limited(quiet_bucket, raise_on_limit=False)
def job():
    return 42

print(job(), job())  # 42 None
```

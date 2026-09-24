# calc：快取與算式計算小函式庫

## calc.cache.LRUCache

```python
LRUCache(capacity: int, ttl: float | None = None, clock=time.monotonic)
```

- 最多存 `capacity` 個項目（capacity ≥ 1，否則 `ValueError`）。
- `get(key, default=None)`：命中時回傳值，並把該項目標記為「最近使用」。過期項目視同不存在（並移除）。
- `put(key, value)`：寫入或覆寫。覆寫已存在的 key 時**不會**淘汰其他項目，並重設該項目的存活時間與最近使用順序。
  新 key 且已滿時，淘汰「最久沒被使用」的項目。
- `delete(key)`：移除，回傳是否存在過。
- `len(cache)`：目前未過期的項目數。
- `ttl`：秒數；項目寫入後經過 **ttl 秒（含剛好 ttl 秒）** 就過期。`None` 表示不過期。

## calc.expr.evaluate

```python
evaluate(text: str) -> int | float
```

- 支援整數、小數（`1.5`、`.5`、`2.`）、`+ - * /`、括號、一元正負號（`-3`、`2*-3`、`--1`）。
- 優先順序：括號 > 一元正負 > `* /` > `+ -`；同級運算**由左到右**結合（`10-3-2 == 5`、`8/4/2 == 1`）。
- `/` 是真除法；結果若是整數值的 float 仍回傳 float（例如 `4/2 == 2.0`）。只有整數與 `+ - *` 時回傳 int。
- 語法錯誤、除以零都丟 `calc.expr.EvalError`（不可丟出 `ZeroDivisionError` 等其他例外）。

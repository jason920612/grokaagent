"""Small in-process rate limiting helpers."""
from __future__ import annotations

import functools
import re
import time
from collections import deque
from typing import Callable, Deque, Tuple

__all__ = [
    "RateLimitExceeded",
    "TokenBucket",
    "SlidingWindowCounter",
    "parse_rate",
    "retry_after",
    "limited",
]

_UNITS = {
    "s": 1.0, "sec": 1.0, "second": 1.0,
    "m": 60.0, "min": 60.0, "minute": 60.0,
    "h": 3600.0, "hour": 3600.0,
    "d": 86400.0, "day": 86400.0,
}


class RateLimitExceeded(Exception):
    """Raised by `limited` when a call is rejected."""

    def __init__(self, retry_after: float):
        super().__init__(f"rate limit exceeded; retry after {retry_after:.3f}s")
        self.retry_after = retry_after


class TokenBucket:
    """Token bucket that starts full.

    Tokens refill continuously at `refill_rate` per second up to `capacity`.
    """

    def __init__(self, capacity: float, refill_rate: float, clock: Callable[[], float] = time.monotonic):
        if capacity <= 0:
            raise ValueError("capacity must be > 0")
        if refill_rate < 0:
            raise ValueError("refill_rate must be >= 0")
        self.capacity = float(capacity)
        self.refill_rate = float(refill_rate)
        self._clock = clock
        self._tokens = float(capacity)
        self._last = clock()

    def _refill(self) -> None:
        now = self._clock()
        elapsed = max(0.0, now - self._last)
        self._last = now
        self._tokens = min(self.capacity, self._tokens + elapsed * self.refill_rate)

    @property
    def tokens(self) -> float:
        self._refill()
        return self._tokens

    def consume(self, n: float = 1) -> bool:
        if n <= 0:
            raise ValueError("n must be > 0")
        if n > self.capacity:
            raise ValueError("n exceeds bucket capacity")
        self._refill()
        if self._tokens >= n:
            self._tokens -= n
            return True
        return False


class SlidingWindowCounter:
    """Allow at most `limit` hits in any trailing `window` seconds."""

    def __init__(self, limit: int, window: float = 60.0, clock: Callable[[], float] = time.monotonic):
        if limit < 1:
            raise ValueError("limit must be >= 1")
        if window <= 0:
            raise ValueError("window must be > 0")
        self.limit = limit
        self.window = window
        self._clock = clock
        self._hits: Deque[float] = deque()

    def hit(self) -> bool:
        now = self._clock()
        while self._hits and now - self._hits[0] >= self.window:
            self._hits.popleft()
        if len(self._hits) < self.limit:
            self._hits.append(now)
            return True
        return False

    def remaining(self) -> int:
        now = self._clock()
        return self.limit - sum(1 for t in self._hits if now - t < self.window)


def parse_rate(spec: str) -> Tuple[int, float]:
    """Parse '10/min' style specs into (count, seconds)."""
    m = re.fullmatch(r"\s*(\d+)\s*/\s*(\d*)\s*([a-zA-Z]+)\s*", spec or "")
    if not m:
        raise ValueError(f"bad rate spec: {spec!r}")
    count = int(m.group(1))
    mult = int(m.group(2)) if m.group(2) else 1
    unit = m.group(3).lower()
    if unit.endswith("s") and unit not in _UNITS:
        unit = unit[:-1]
    if unit not in _UNITS:
        raise ValueError(f"unknown unit: {m.group(3)!r}")
    if count == 0 or mult == 0:
        raise ValueError("count and period must be > 0")
    return count, mult * _UNITS[unit]


def retry_after(bucket: TokenBucket, n: float = 1) -> float:
    """Seconds until `n` tokens are available (0.0 if already available)."""
    have = bucket.tokens
    if have >= n:
        return 0.0
    if bucket.refill_rate == 0:
        return float("inf")
    return (n - have) / bucket.refill_rate


def limited(bucket: TokenBucket, *, cost: float = 1, raise_on_limit: bool = True):
    """Decorator: consume `cost` tokens per call.

    When the bucket is empty, raise RateLimitExceeded, or return None if
    raise_on_limit is False.
    """

    def deco(fn):
        @functools.wraps(fn)
        def wrapper(*args, **kwargs):
            if bucket.consume(cost):
                return fn(*args, **kwargs)
            if raise_on_limit:
                raise RateLimitExceeded(retry_after(bucket, cost))
            return None

        return wrapper

    return deco

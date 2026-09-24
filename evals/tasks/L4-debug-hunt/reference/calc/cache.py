import time
from collections import OrderedDict


class LRUCache:
    def __init__(self, capacity, ttl=None, clock=time.monotonic):
        if capacity < 1:
            raise ValueError("capacity must be >= 1")
        self.capacity = capacity
        self.ttl = ttl
        self._clock = clock
        self._data = OrderedDict()  # key -> (value, stored_at); oldest first

    def _expired(self, stored_at):
        return self.ttl is not None and self._clock() - stored_at >= self.ttl

    def _purge(self):
        for key in [k for k, (_, t) in self._data.items() if self._expired(t)]:
            del self._data[key]

    def get(self, key, default=None):
        item = self._data.get(key)
        if item is None:
            return default
        value, stored_at = item
        if self._expired(stored_at):
            del self._data[key]
            return default
        self._data.move_to_end(key)
        return value

    def put(self, key, value):
        self._purge()
        if key not in self._data and len(self._data) >= self.capacity:
            self._data.popitem(last=False)
        self._data[key] = (value, self._clock())
        self._data.move_to_end(key)

    def delete(self, key):
        return self._data.pop(key, None) is not None

    def __len__(self):
        self._purge()
        return len(self._data)

    def __contains__(self, key):
        return self.get(key, _MISSING) is not _MISSING


_MISSING = object()

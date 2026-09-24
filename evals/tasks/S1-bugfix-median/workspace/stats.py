"""Small descriptive-statistics helpers used by the reporting job."""


def mean(values):
    if not values:
        raise ValueError("mean of empty data")
    return sum(values) / len(values)


def median(values):
    if not values:
        raise ValueError("median of empty data")
    data = sorted(values)
    mid = len(data) // 2
    if len(data) % 2:
        return data[mid]
    return data[mid]


def mode(values):
    """Most common value; ties go to the smallest value."""
    if not values:
        raise ValueError("mode of empty data")
    counts = {}
    for v in values:
        counts[v] = counts.get(v, 0) + 1
    best = max(counts.values())
    return min(v for v, c in counts.items() if c == best)


def percentile(values, p):
    """Linear interpolation between closest ranks (like numpy's default)."""
    if not values:
        raise ValueError("percentile of empty data")
    if not 0 <= p <= 100:
        raise ValueError("p must be in [0, 100]")
    data = sorted(values)
    k = (len(data) - 1) * p / 100
    lo = int(k)
    hi = min(lo + 1, len(data) - 1)
    return data[lo] + (data[hi] - data[lo]) * (k - lo)

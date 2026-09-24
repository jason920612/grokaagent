_TABLE = [(1000, "M"), (900, "CM"), (500, "D"), (400, "CD"), (100, "C"), (90, "XC"),
          (50, "L"), (40, "XL"), (10, "X"), (9, "IX"), (5, "V"), (4, "IV"), (1, "I")]


def to_roman(n):
    if isinstance(n, bool) or not isinstance(n, int) or not 1 <= n <= 3999:
        raise ValueError(f"out of range: {n!r}")
    out = []
    for value, sym in _TABLE:
        while n >= value:
            out.append(sym)
            n -= value
    return "".join(out)


def from_roman(s):
    if not isinstance(s, str) or not s or any(c not in "MDCLXVI" for c in s):
        raise ValueError(f"not a roman numeral: {s!r}")
    values = {"M": 1000, "D": 500, "C": 100, "L": 50, "X": 10, "V": 5, "I": 1}
    total = 0
    for i, c in enumerate(s):
        v = values[c]
        if i + 1 < len(s) and values[s[i + 1]] > v:
            total -= v
        else:
            total += v
    if not 1 <= total <= 3999 or to_roman(total) != s:
        raise ValueError(f"non-canonical roman numeral: {s!r}")
    return total

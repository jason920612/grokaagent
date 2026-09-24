def _digits(s):
    return s.replace(" ", "")


def _sum(digits, double_first):
    total = 0
    double = double_first
    for c in reversed(digits):
        d = int(c)
        if double:
            d *= 2
            if d > 9:
                d -= 9
        total += d
        double = not double
    return total


def is_valid(number):
    if not isinstance(number, str):
        return False
    d = _digits(number)
    if len(d) < 2 or not d.isascii() or not d.isdigit():
        return False
    return _sum(d, False) % 10 == 0


def check_digit(partial):
    if not isinstance(partial, str):
        raise ValueError("partial must be a string")
    d = _digits(partial)
    if not d or not d.isascii() or not d.isdigit():
        raise ValueError(f"not digits: {partial!r}")
    return str((10 - _sum(d, True) % 10) % 10)


def complete(partial):
    return _digits(partial) + check_digit(partial)

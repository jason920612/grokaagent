def encode(s):
    out = []
    i = 0
    while i < len(s):
        j = i
        while j < len(s) and s[j] == s[i]:
            j += 1
        c = s[i]
        out.append(str(j - i))
        if c.isdigit() or c == "\\":
            out.append("\\")
        out.append(c)
        i = j
    return "".join(out)


def decode(s):
    out = []
    i = 0
    while i < len(s):
        j = i
        while j < len(s) and s[j] in "0123456789":
            j += 1
        count = s[i:j]
        if not count or count[0] == "0":
            raise ValueError(f"bad count at {i}")
        if j >= len(s):
            raise ValueError("count without character")
        if s[j] == "\\":
            if j + 1 >= len(s):
                raise ValueError("dangling escape")
            c = s[j + 1]
            i = j + 2
        else:
            c = s[j]
            i = j + 1
        out.append(c * int(count))
    return "".join(out)

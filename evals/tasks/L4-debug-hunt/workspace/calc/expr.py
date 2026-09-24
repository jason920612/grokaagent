class EvalError(Exception):
    pass


def tokenize(text):
    tokens = []
    i = 0
    while i < len(text):
        c = text[i]
        if c.isspace():
            i += 1
        elif c.isdigit() or c == ".":
            j = i
            while j < len(text) and (text[j].isdigit() or text[j] == "."):
                j += 1
            raw = text[i:j]
            if raw == "." or raw.count(".") > 1:
                raise EvalError(f"bad number {raw!r}")
            tokens.append(float(raw) if "." in raw else int(raw))
            i = j
        elif c in "+-*/()":
            tokens.append(c)
            i += 1
        else:
            raise EvalError(f"unexpected {c!r}")
    return tokens


class _Parser:
    def __init__(self, tokens):
        self.tokens = tokens
        self.pos = 0

    def peek(self):
        return self.tokens[self.pos] if self.pos < len(self.tokens) else None

    def take(self):
        tok = self.peek()
        self.pos += 1
        return tok

    def expr(self):
        left = self.term()
        op = self.peek()
        if op in ("+", "-"):
            self.take()
            right = self.expr()
            return left + right if op == "+" else left - right
        return left

    def term(self):
        left = self.factor()
        while self.peek() in ("*", "/"):
            op = self.take()
            right = self.factor()
            if op == "*":
                left = left * right
            else:
                if right == 0 and op == "//":
                    raise EvalError("division by zero")
                left = left / right
        return left

    def factor(self):
        tok = self.take()
        if tok == "+":
            return self.factor()
        if tok == "-":
            return -self.factor()
        if tok == "(":
            value = self.expr()
            if self.take() != ")":
                raise EvalError("missing )")
            return value
        if isinstance(tok, (int, float)) and not isinstance(tok, bool):
            return tok
        raise EvalError(f"unexpected token {tok!r}")


def evaluate(text):
    parser = _Parser(tokenize(text))
    if parser.peek() is None:
        raise EvalError("empty expression")
    value = parser.expr()
    if parser.peek() is not None:
        raise EvalError(f"unexpected token {parser.peek()!r}")
    return value

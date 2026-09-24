import sys

import luhn
import rle
import roman


def run(argv):
    if len(argv) != 3:
        raise ValueError("usage: main.py <roman|luhn|rle> <action> <value>")
    tool, action, value = argv
    if tool == "roman" and action == "to":
        try:
            n = int(value)
        except ValueError:
            raise ValueError(f"not an integer: {value}")
        return roman.to_roman(n)
    if tool == "roman" and action == "from":
        return str(roman.from_roman(value))
    if tool == "luhn" and action == "check":
        return "valid" if luhn.is_valid(value) else "invalid"
    if tool == "luhn" and action == "complete":
        return luhn.complete(value)
    if tool == "rle" and action == "encode":
        return rle.encode(value)
    if tool == "rle" and action == "decode":
        return rle.decode(value)
    raise ValueError(f"unknown command: {tool} {action}")


def main():
    try:
        print(run(sys.argv[1:]))
        return 0
    except ValueError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())

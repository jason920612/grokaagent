"""Reference FASTA analysis per the D3 rules."""
from decimal import ROUND_HALF_UP, Decimal
from pathlib import Path

STOPS = {"TAA", "TAG", "TGA"}


def parse(path):
    recs, rid, buf = [], None, []
    for line in Path(path).read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line:
            continue
        if line.startswith(">"):
            if rid is not None:
                recs.append((rid, "".join(buf)))
            rid, buf = line[1:].split()[0], []
        else:
            buf.append(line)
    if rid is not None:
        recs.append((rid, "".join(buf)))
    return recs


def revcomp(s):
    return s.upper()[::-1].translate(str.maketrans("ACGTN", "TGCAN"))


def gc_percent(s):
    s = s.upper()
    acgt = sum(s.count(c) for c in "ACGT")
    if acgt == 0:
        return Decimal("0.00")
    return (Decimal(s.count("G") + s.count("C")) * 100 / Decimal(acgt)).quantize(Decimal("0.01"), rounding=ROUND_HALF_UP)


def longest_orf_one_strand(s):
    best = 0
    for i in range(len(s) - 2):
        if s[i:i + 3] != "ATG":
            continue
        for j in range(i + 3, len(s) - 2, 3):
            if s[j:j + 3] in STOPS:
                best = max(best, j + 3 - i)
                break
    return best


def longest_orf(s):
    s = s.upper()
    return max(longest_orf_one_strand(s), longest_orf_one_strand(revcomp(s)))


def report(ws):
    recs = parse(Path(ws) / "samples.fasta")
    rows = [(rid, len(s), gc_percent(s), longest_orf(s)) for rid, s in recs]
    shortest = sorted(recs, key=lambda r: (len(r[1]), r[0]))[:3]
    rc = [(f"{rid}_rc", revcomp(s)) for rid, s in shortest]
    return rows, rc


def write(ws):
    rows, rc = report(ws)
    ws = Path(ws)
    out = ["id\tlength\tgc_percent\tlongest_orf_nt"]
    out += [f"{r}\t{n}\t{gc}\t{o}" for r, n, gc, o in rows]
    (ws / "report.tsv").write_text("\n".join(out) + "\n", encoding="utf-8")
    fa = []
    for rid, s in rc:
        fa.append(f">{rid}")
        fa += [s[i:i + 60] for i in range(0, len(s), 60)]
    (ws / "revcomp.fasta").write_text("\n".join(fa) + "\n", encoding="utf-8")

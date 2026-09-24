"""Write samples.fasta (seeded) into the workspace given as argv[1]."""
import random
import sys
from pathlib import Path

rng = random.Random(4242)
ws = Path(sys.argv[1])


def rand_dna(n, gc=0.5):
    out = []
    for _ in range(n):
        if rng.random() < gc:
            out.append(rng.choice("GC"))
        else:
            out.append(rng.choice("AT"))
    return "".join(out)


def orf(codons):
    stops = ["TAA", "TAG", "TGA"]
    body = []
    for _ in range(codons):
        c = rand_dna(3)
        while c in stops:
            c = rand_dna(3)
        body.append(c)
    return "ATG" + "".join(body) + rng.choice(stops)


def revcomp(s):
    return s[::-1].translate(str.maketrans("ACGTNacgtn", "TGCANtgcan"))


records = []
lengths = [45, 60, 72, 90, 120, 150, 200, 240, 300, 360, 420, 500, 600, 750, 900, 1000, 1100, 1250, 1400, 1500]
rng.shuffle(lengths)
for i, n in enumerate(lengths, 1):
    s = list(rand_dna(n, gc=rng.uniform(0.3, 0.7)))
    # Plant ORFs of different sizes on either strand in some sequences.
    if n >= 120 and rng.random() < 0.8:
        k = rng.randrange(8, max(9, n // 3 - 10))
        o = orf(k)
        if len(o) < n - 5:
            if rng.random() < 0.5:
                o = revcomp(o)
            pos = rng.randrange(0, n - len(o))
            s[pos:pos + len(o)] = list(o)
    s = "".join(s)
    # Runs of N and lowercase (soft-masked) stretches.
    if rng.random() < 0.5:
        a = rng.randrange(0, max(1, n - 12))
        s = s[:a] + "N" * rng.randrange(3, 12) + s[a + 12:]
        s = s[:n]
    if rng.random() < 0.5:
        a = rng.randrange(0, max(1, n - 40))
        b = a + rng.randrange(10, 40)
        s = s[:a] + s[a:b].lower() + s[b:]
    records.append((f"seq{i:02d}", f"sample={rng.choice(['liver', 'leaf', 'soil', 'gut'])} run={rng.randrange(100, 999)}", s))

lines = []
for rid, desc, s in records:
    lines.append(f">{rid} {desc}")
    width = rng.choice([60, 70, 80])
    for j in range(0, len(s), width):
        lines.append(s[j:j + width])
(ws / "samples.fasta").write_text("\n".join(lines) + "\n", encoding="utf-8")

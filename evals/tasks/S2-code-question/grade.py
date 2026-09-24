import sys; from pathlib import Path; sys.path.insert(0, str(Path(__file__).resolve().parents[2])); from gradelib import *
import hashlib

ws, ev = args()
g = Grade()
SOURCE_SHA = {
    "legacy/__init__.py": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    "legacy/old_rates.py": "5421e8e3100d1c85acb1955d61b3fa30bd1d32c5da91df45e2cfe3fcc99e6d52",
    "shipping/__init__.py": "24323d86f565a899ece91769f53b2965c6921dd6a03865af8015f4e4f6d35531",
    "shipping/fees.py": "c40d6d49d7fb05fec6be44c8516c3c958793ff7f1ff446d0b8fda65e8c3f5ab4",
    "shipping/rates.py": "1a4efb4b008d90abf0c66f237c0791aa0887d19abdb18890cdbcce1ed958b336",
    "shipping/zones.py": "2e275584aff2dd41e8088f7d90d940dc21c3f32e7459d7aafe6da2b75053bdd4",
    "shop/__init__.py": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    "shop/cart.py": "a5149a29e218935f2009320a14b3f1267e8a828b4622fe98322d9f2f79f27039",
    "shop/checkout.py": "3904517c5ad30bc566e0db13270a5f159ed6a47140f282019a30ae877fcca4c2"
}

ans = read_json(ws, "answer.json")
if not isinstance(ans, dict):
    ans = {}
fee = ans.get("fee")
try:
    fee_ok = abs(float(fee) - 590) < 1e-9
except (TypeError, ValueError):
    fee_ok = False
g.check("fee is 590 (12.5 kg, zone B, gold)", fee_ok, weight=3, detail=f"got {fee!r}")
fn = str(ans.get("function", "")).strip()
g.check("function names shipping.fees.shipping_fee", fn in ("shipping.fees.shipping_fee", "shipping_fee"), weight=1, detail=fn)
f = str(ans.get("file", "")).strip().replace("\\", "/").lstrip("./")
g.check("file is shipping/fees.py", f == "shipping/fees.py", weight=1, detail=f)
changed = []
for rel, sha in SOURCE_SHA.items():
    p = ws / rel
    got = hashlib.sha256(p.read_bytes().replace(b"\r\n", b"\n")).hexdigest() if p.exists() else ""
    if got != sha:
        changed.append(rel)
g.check("no source file modified", not changed, weight=1, detail=", ".join(changed))
g.emit()

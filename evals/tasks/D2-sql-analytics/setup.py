"""Build shop.db (seeded, deterministic) in the workspace given as argv[1]."""
import datetime as dt
import random
import sqlite3
import sys
from pathlib import Path

ws = Path(sys.argv[1])
db = ws / "shop.db"
if db.exists():
    db.unlink()
rng = random.Random(20260924)
con = sqlite3.connect(db)
con.executescript(
    """
    CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT NOT NULL, city TEXT NOT NULL, signup_date TEXT NOT NULL);
    CREATE TABLE products (id INTEGER PRIMARY KEY, name TEXT NOT NULL, category TEXT NOT NULL, list_price REAL NOT NULL);
    CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER NOT NULL REFERENCES customers(id),
                         order_date TEXT NOT NULL, status TEXT NOT NULL CHECK (status IN ('paid','cancelled')));
    CREATE TABLE order_items (id INTEGER PRIMARY KEY, order_id INTEGER NOT NULL REFERENCES orders(id),
                              product_id INTEGER NOT NULL REFERENCES products(id), quantity INTEGER NOT NULL,
                              unit_price REAL NOT NULL);
    CREATE TABLE refunds (id INTEGER PRIMARY KEY, order_id INTEGER NOT NULL REFERENCES orders(id),
                          refund_date TEXT NOT NULL, amount REAL NOT NULL);
    """
)
cities = ["台北", "新北", "台中", "高雄", "台南", "桃園", "新竹"]
customers = []
start = dt.date(2024, 1, 1)
for cid in range(1, 701):
    signup = start + dt.timedelta(days=rng.randrange(0, 700))
    customers.append((cid, f"客戶{cid:04d}", rng.choice(cities), signup.isoformat()))
con.executemany("INSERT INTO customers VALUES (?,?,?,?)", customers)

cats = {"3C": (800, 30000), "家電": (1500, 25000), "服飾": (200, 3000), "美妝": (150, 2500),
        "食品": (50, 800), "書籍": (150, 900), "運動": (300, 6000)}
products = []
pid = 0
for cat, (lo, hi) in cats.items():
    for k in range(12):
        pid += 1
        products.append((pid, f"{cat}商品{k + 1:02d}", cat, round(rng.uniform(lo, hi), 0)))
con.executemany("INSERT INTO products VALUES (?,?,?,?)", products)

orders, items, refunds = [], [], []
oid = iid = rid = 0
end = dt.date(2025, 12, 31)
for cid, _, _, signup in customers:
    sd = dt.date.fromisoformat(signup)
    n = rng.choices([0, 1, 2, 3, 4, 6, 9], weights=[8, 30, 20, 14, 10, 6, 3])[0]
    for _ in range(n):
        span = (end - sd).days
        if span <= 0:
            continue
        od = sd + dt.timedelta(days=rng.randrange(0, span + 1))
        oid += 1
        status = "cancelled" if rng.random() < 0.08 else "paid"
        orders.append((oid, cid, od.isoformat(), status))
        total = 0.0
        for _ in range(rng.choice([1, 1, 2, 2, 3, 4])):
            iid += 1
            p = rng.choice(products)
            qty = rng.choice([1, 1, 1, 2, 2, 3])
            price = round(p[3] * rng.choice([1.0, 1.0, 0.9, 0.85, 0.95]), 2)
            items.append((iid, oid, p[0], qty, price))
            total += qty * price
        if status == "paid" and rng.random() < 0.12:
            rid += 1
            rd = od + dt.timedelta(days=rng.randrange(1, 40))
            amt = round(total * rng.choice([1.0, 0.5, 0.3, 0.2]), 2)
            refunds.append((rid, oid, rd.isoformat(), amt))
# Planted ties for question 2 are unlikely with floats; the rule is still stated.
con.executemany("INSERT INTO orders VALUES (?,?,?,?)", orders)
con.executemany("INSERT INTO order_items VALUES (?,?,?,?,?)", items)
con.executemany("INSERT INTO refunds VALUES (?,?,?,?)", refunds)
con.commit()
con.close()

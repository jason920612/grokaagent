"""Write seeded nginx access logs with planted incidents into argv[1]/logs/."""
import datetime as dt
import gzip
import random
import sys
from pathlib import Path

rng = random.Random(9001)
ws = Path(sys.argv[1])
logs = ws / "logs"
logs.mkdir(parents=True, exist_ok=True)

T0 = dt.datetime(2026, 3, 9, 0, 0, 0, tzinfo=dt.timezone.utc)
SPAN = 2 * 24 * 3600
UAS = [
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/128.0 Safari/537.36",
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Safari/605.1.15",
    "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 Mobile/15E148",
    "Mozilla/5.0 (X11; Linux x86_64; rv:129.0) Gecko/20100101 Firefox/129.0",
    "Mozilla/5.0 (Linux; Android 14) AppleWebKit/537.36 Chrome/128.0 Mobile Safari/537.36",
]
PATHS = ["/", "/products", "/products/42", "/products/7?ref=home", "/cart", "/search?q=shoes", "/about",
         "/static/app.js", "/static/site.css", "/api/items?page=2", "/blog/2026/03/launch", "/help"]
MISSING = ["/favicon.ico", "/old-page", "/products/9999", "/wp-login.php", "/.env", "/robots.txt.bak"]


def ip():
    return f"{rng.choice([23, 45, 61, 88, 101, 114, 140, 172, 185, 203])}.{rng.randrange(256)}.{rng.randrange(256)}.{rng.randrange(1, 255)}"


normal_ips = [ip() for _ in range(400)]
UA_OF = {a: UAS[i % len(UAS)] for i, a in enumerate(normal_ips)}
entries = []  # (datetime, line)


def add(t, addr, method, target, status, ua, size=None):
    size = size if size is not None else rng.randrange(200, 40000)
    stamp = t.strftime("%d/%b/%Y:%H:%M:%S +0000")
    entries.append((t, f'{addr} - - [{stamp}] "{method} {target} HTTP/1.1" {status} {size} "-" "{ua}"'))


def rt(lo=0, hi=SPAN):
    return T0 + dt.timedelta(seconds=rng.randrange(lo, hi))


# Normal browsing.
for _ in range(18500):
    addr = rng.choice(normal_ips)
    ua = UA_OF[addr]
    r = rng.random()
    if r < 0.93:
        add(rt(), addr, "GET", rng.choice(PATHS), 200, ua)
    elif r < 0.97:
        add(rt(), addr, "GET", rng.choice(MISSING), 404, ua, 153)
    elif r < 0.985:
        add(rt(), addr, "POST", "/login", 302, ua, 0)
    else:
        add(rt(), addr, "POST", "/login", 401, ua, 45)

# Brute force: 3 IPs, >= 10 failed logins inside 5 minutes.
brute = ["185.220.101.7", "45.155.205.99", "103.75.201.4"]
for b in brute:
    start = rt(3600, SPAN - 3600)
    for k in range(rng.randrange(14, 40)):
        add(start + dt.timedelta(seconds=k * rng.randrange(3, 12)), b, "POST", "/login", 401, "python-requests/2.32.3", 45)
# Near misses: 9 in 5 minutes, and 15 spread over 2 hours.
start = rt(3600, SPAN - 3600)
for k in range(9):
    add(start + dt.timedelta(seconds=k * 30), "61.216.5.20", "POST", "/login", 401, UAS[0], 45)
start = rt(3600, SPAN - 9000)
for k in range(15):
    add(start + dt.timedelta(seconds=k * 480), "114.34.9.200", "POST", "/login", 401, UAS[2], 45)
# 10 in a window but one is a success in between -> still 10 failures -> counts.
start = rt(3600, SPAN - 3600)
for k in range(11):
    add(start + dt.timedelta(seconds=k * 20), "88.12.44.3", "POST", "/login", 302 if k == 5 else 401, UAS[3], 45)

# Path traversal (raw, encoded, backslash) and a double-encoded one that must NOT count.
trav = {"91.240.118.17": "/download?file=../../../../etc/passwd",
        "193.32.162.8": "/static/..%2f..%2f..%2fetc%2fshadow",
        "5.188.62.140": "/img?name=..%5c..%5cwindows%5cwin.ini",
        "212.70.149.66": "/files/%2e%2e/%2e%2e/etc/passwd"}
for a, p in trav.items():
    for _ in range(rng.randrange(1, 4)):
        add(rt(), a, "GET", p, rng.choice([400, 403, 404]), "curl/8.7.1", 150)
add(rt(), "77.83.36.9", "GET", "/static/%252e%252e%252fetc%252fpasswd", 404, "curl/8.7.1", 150)

# SQL injection probes.
sqli = {"194.26.192.71": "/products?id=1%27%20OR%20%271%27%3D%271",
        "80.94.95.115": "/search?q=x%27%20UNION%20SELECT%20username,password%20FROM%20users--",
        "146.70.53.2": "/api/items?page=1;SELECT%20pg_sleep(5)",
        "45.9.148.35": "/products?id=5%20and%20sleep(5)"}
for a, p in sqli.items():
    for _ in range(rng.randrange(1, 3)):
        add(rt(), a, "GET", p, rng.choice([200, 500, 403]), "sqlmap/1.8.4#stable (https://sqlmap.org)", 300)
# Looks similar but is not SQLi by the stated rules.
add(rt(), "140.1.2.3", "GET", "/blog/union-selection-guide", 200, UAS[1])

# Scanner: many 404s with a distinctive UA.
scanner_ua = "Mozilla/5.0 (compatible; Nuclei - Open-source project (github.com/projectdiscovery/nuclei))"
for _ in range(260):
    add(rt(), "167.94.138.60", "GET", rng.choice(MISSING + ["/admin", "/phpmyadmin/", "/server-status", "/.git/config"]), 404, scanner_ua, 153)

# 5xx burst inside one minute, plus background 5xx.
burst = T0 + dt.timedelta(hours=31, minutes=17)
for _ in range(37):
    add(burst + dt.timedelta(seconds=rng.randrange(0, 60)), rng.choice(normal_ips), "GET", "/api/items?page=2", 502, UAS[0], 0)
for _ in range(60):
    add(rt(), rng.choice(normal_ips), "GET", rng.choice(PATHS), rng.choice([500, 503, 504]), UAS[4], 0)
# A near-peak minute to make the max unambiguous but not trivial.
near = T0 + dt.timedelta(hours=9, minutes=3)
for _ in range(30):
    add(near + dt.timedelta(seconds=rng.randrange(0, 60)), rng.choice(normal_ips), "GET", "/cart", 503, UAS[1], 0)

entries.sort(key=lambda e: e[0])
lines = [l for _, l in entries]
# Garbage lines that must be skipped.
for k, junk in enumerate(["", "GARBAGE LINE WITHOUT FORMAT", '127.0.0.1 - - [bad-date] "GET / HTTP/1.1" 200 1 "-" "x"']):
    lines.insert(1000 + k * 5000, junk)
n = len(lines)
a, b = n // 3, 2 * n // 3
with gzip.open(logs / "access.log.2.gz", "wt", encoding="utf-8") as fh:
    fh.write("\n".join(lines[:a]) + "\n")
(logs / "access.log.1").write_text("\n".join(lines[a:b]) + "\n", encoding="utf-8")
(logs / "access.log").write_text("\n".join(lines[b:]) + "\n", encoding="utf-8")

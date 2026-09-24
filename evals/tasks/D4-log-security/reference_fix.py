import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import refcalc  # noqa: E402

refcalc.write(sys.argv[1])

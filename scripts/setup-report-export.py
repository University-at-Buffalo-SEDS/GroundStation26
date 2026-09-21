#!/usr/bin/env python3
"""Install server-side Excel/PDF dependencies without changing system Python."""
from pathlib import Path
import os
import subprocess
import sys
import venv

root = Path(__file__).resolve().parents[1]
environment = root / ".venv-reports"
venv.EnvBuilder(with_pip=True).create(environment)
python = environment / ("Scripts/python.exe" if os.name == "nt" else "bin/python")
subprocess.run([str(python), "-m", "pip", "install", "-r", str(root / "backend/report-requirements.txt")], check=True)
print(f"Report exports ready. For services whose working directory differs, set GS_REPORT_PYTHON={python}")

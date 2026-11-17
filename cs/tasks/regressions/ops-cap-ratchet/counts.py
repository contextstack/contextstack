#!/usr/bin/env python3
"""Debt-clock assertions are suspended; run the current real-hook fixture."""
import runpy
from pathlib import Path
runpy.run_path(str(Path(__file__).with_name("check")), run_name="__main__")

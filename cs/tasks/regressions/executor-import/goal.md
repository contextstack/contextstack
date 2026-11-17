# Regression: `_executor.py` is importable without side effects

Before 0.1.0, `_executor.py` ran its gateway work order at import time. Tests and the
lead host had to ast-exec helper functions out of the module. Importing the module must
not read stdin, write files, or start subprocesses.

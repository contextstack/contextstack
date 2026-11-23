"""Runtime assets for partial estates that copy the native executor."""
from pathlib import Path
import shutil
import os


def stage_native_runtime(ops):
    code_root = Path(ops).parents[1]
    source = Path(__file__).resolve().parents[3]
    policy = code_root / 'policy/sandbox.json'
    if not policy.exists():
        policy.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source / 'policy/sandbox.json', policy)
    binary = code_root.parent / 'bin/cs-sandbox'
    if not binary.exists() and not binary.is_symlink():
        binary.parent.mkdir(parents=True, exist_ok=True)
        binary.symlink_to(os.environ.get('CS_SANDBOX_BIN', str(source.parent / 'target/release/cs-sandbox')))

"""Exercise the same toolchain fixture through the separate land profile."""
import os
from pathlib import Path
import subprocess

import _toolchains
from _estate_io import land_check_request, render_land_check_request


def check_land_toolchain(root, kit, command, arguments, env, output):
    home, worktree = root / "home", root / "ws"
    run = root / "land-run"
    target, tmp, scratch = run / "target", run / "tmp", run / "scratch"
    for path in (target, tmp, scratch, root / "main"):
        path.mkdir(parents=True, exist_ok=True)
    darwin_temp = subprocess.check_output(
        ["/usr/bin/getconf", "DARWIN_USER_TEMP_DIR"], text=True).strip()
    request = land_check_request(
        root=root, home=home, main_checkout=root / "main", worktree=worktree,
        run_tmp=run, cargo_target_dir=target, tmpdir=tmp, scratch=scratch,
        darwin_user_temp=darwin_temp, toolchain_roots=_toolchains.discover(),
        toolchain_links=_toolchains.discover_sandbox_links(), env=env)
    policy = run / "land.sb"
    binary = os.environ.get("CS_SANDBOX_BIN") or kit.parent / "target/release/cs-sandbox"
    policy.write_bytes(render_land_check_request(binary, request))
    env = {**env, **_toolchains.toolchain_lane_env(scratch), "HOME": str(scratch),
           "TMPDIR": str(tmp), "CARGO_TARGET_DIR": str(target)}
    result = subprocess.run(["/usr/bin/sandbox-exec", "-f", str(policy), "/bin/sh",
                             "-ec", command, "land", *map(str, arguments)], cwd=worktree,
                            env=env, capture_output=True, text=True, timeout=30)
    assert result.returncode == 0 and output in result.stdout, result

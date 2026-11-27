"""Node-owned hosted release execution and durable receipts."""
import difflib
import fcntl
import hashlib
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
import uuid
from pathlib import Path

HERE = Path(__file__).resolve()
sys.path.insert(0, str(HERE.parent))
sys.path.insert(0, str(HERE.parents[2] / "scripts"))
import _node_layout as layout  # noqa: E402
from _director_client import Client, DirectorUnreachable, receiver_config  # noqa: E402
from _job_reason import clip, redact_obj  # noqa: E402
from _node_control import read_json, write_json  # noqa: E402
from _node_env import node_env  # noqa: E402
from _process_group import stop_group  # noqa: E402

ID = re.compile(r"act_[0-9a-z]{26}")
NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,99}")
OID = re.compile(r"[0-9a-f]{40}|[0-9a-f]{64}")
WAIT_TIMEOUT = 600
BUILD_TIMEOUT = 3600
NOTES = ("RELEASES.json", "docs/release/prerelease-notes.md")


def name(value):
    if not isinstance(value, str) or not NAME.fullmatch(value) or ".." in value:
        raise ValueError("invalid release name")
    return value


def git(repo, *args):
    return subprocess.check_output(["git", "-C", str(repo), *args], stderr=subprocess.DEVNULL,
                                   text=True, timeout=30)


def source_schema(repo, commit):
    file, constant = "director/src/store.rs", "SCHEMA_VERSION"
    for _ in range(5):
        source = git(repo, "show", f"{commit}:{file}")
        value = re.search(rf"\bconst {constant}: u32 = ([\w:]+);", source)[1]
        if value.isdecimal():
            return int(value)
        module, constant = value.split("::")
        file = f"director/src/store/{module}.rs"
    raise ValueError("unknown Director schema")


def bundle_digest(root, *, installed=False):
    """Include installer inputs outside the manifest, including uv and the Python mirror."""
    if root.is_symlink() or not root.is_dir():
        raise ValueError("bundle must be a local directory")
    digest = hashlib.sha256()
    for path in sorted(root.rglob("*")):
        info = path.lstat()
        digest.update(os.fsencode(path.relative_to(root)) + b"\0" + str(info.st_mode).encode() + b"\0")
        if stat.S_ISREG(info.st_mode):
            digest.update(file_digest(path))
        elif stat.S_ISLNK(info.st_mode):
            target = os.readlink(path)
            if not installed and (os.path.isabs(target) or
                                  not (path.parent / target).resolve().is_relative_to(root.resolve())):
                raise ValueError("bundle has an external link")
            digest.update(os.fsencode(target) + b"\0")
        elif not stat.S_ISDIR(info.st_mode):
            raise ValueError("bundle contains a special file")
    return digest.hexdigest()


def file_digest(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.digest()


def metadata(directory):
    meta = json.loads((directory / "bundle.json").read_text())
    if not OID.fullmatch(meta["code_commit"]):
        raise ValueError("bundle commit is missing")
    name(meta["release"])
    return meta


def installed_digest(directory):
    """Bind all installed files and symlink targets, including generated runtime files."""
    return bundle_digest(directory, installed=True)


def local_schema():
    return layout.director_db_schema(layout.DIRECTOR_DB) if layout.DIRECTOR_DB.is_file() else None


def installed_schema(directory):
    # A named bundle's binaries are executable only after approval. Prefer the
    # version emitted by node-bundle or read the pinned source from the local repo.
    return metadata(directory).get("director_schema")


def notes_diff(repo, old, commit, source):
    try:
        previous = metadata(old)
        if repo:
            try:
                return clip(git(repo, "diff", "--no-ext-diff", "--no-textconv",
                                previous["code_commit"], commit, "--", *NOTES), 16000)
            except subprocess.SubprocessError:
                pass
        before, after = previous.get("release_notes"), metadata(source).get("release_notes") if source else None
        if isinstance(before, str) and isinstance(after, str):
            return clip("".join(difflib.unified_diff(before.splitlines(True), after.splitlines(True),
                                                     fromfile="current", tofile="target")), 16000)
    except (OSError, ValueError, KeyError):
        pass
    return "Release notes diff unavailable on this node."


def inspect(row, config):
    settings = read_json(config / "policy/lead-release.json") or {}
    repo = Path(settings["source_repo"]) if settings.get("source_repo") else None
    current = name(layout.current_id())
    old = layout.RELEASES / current
    record = layout.load_record()
    source, release = None, None
    schema = local_schema()
    if row["kind"] == "rollback":
        release = name(record.get("previous"))
        source = layout.RELEASES / release
        meta = metadata(source)
        commit, pin = meta["code_commit"], installed_digest(source)
    elif row.get("bundle"):
        source = layout.ROOT / "bundles" / name(row["bundle"])
        meta = metadata(source)
        release = meta["release"]
        commit, pin = meta["code_commit"], bundle_digest(source)
    else:
        if repo is None or not repo.is_absolute():
            raise ValueError("configure a local source repository")
        commit = git(repo, "rev-parse", "--verify", "refs/heads/main^{commit}").strip()
        pin = commit
    target = schema
    if record.get("serve_director_rs"):
        target = installed_schema(source) if source else None
        if target is None:
            if repo is None:
                raise ValueError("cannot determine the target Director schema")
            target = source_schema(repo, commit)
    notes = notes_diff(repo, old, commit, source)
    return {"commit": commit, "target_hash": pin, "schema_current": schema, "schema_target": target,
            "release_notes_diff": notes, "before": current,
            "backup": "local_and_s3" if (config / "backup.json").is_file() else "local_only",
            "commit_provenance": "bundle_json" if source else "local_main", "release": release, "repo": str(repo) if repo else None,
            "serve_director_rs": bool(record.get("serve_director_rs")),
            "serve_client_web": bool(record.get("serve_client_web"))}


def build(pin, destination):
    repo = pin["repo"]
    if not OID.fullmatch(pin["commit"]):
        raise ValueError("invalid approved commit")
    export = destination.parent / "source"
    export.mkdir()
    archive = destination.parent / "source.tar"
    try:
        with archive.open("wb") as stream:
            subprocess.run(["git", "-C", repo, "archive", pin["commit"]], stdout=stream,
                           stderr=subprocess.PIPE, check=True, timeout=30)
    except subprocess.SubprocessError as error:
        tail = (error.stderr or b"").decode(errors="replace")[-2000:]
        raise ValueError(f"Git archive did not complete: {error}. {tail}") from error
    with tarfile.open(archive) as contents:
        contents.extractall(export, filter="data")
    argv = [sys.executable, "-I", str(export / "cs/scripts/node-bundle"), "build",
            "--commit", pin["commit"], str(destination)]
    if pin["serve_director_rs"]:
        argv.append("--director-rs")
    if pin["serve_client_web"]:
        argv.append("--client-web")
    with tempfile.TemporaryFile(dir=destination.parent) as stderr:
        with subprocess.Popen(argv, cwd=export, stdout=subprocess.DEVNULL, stderr=stderr,
                              start_new_session=True) as process:
            try:
                code = process.wait(timeout=BUILD_TIMEOUT)
                if code == 0:
                    return
                reason = f"exited with status {code}"
            except subprocess.TimeoutExpired:
                stop_group(process)
                reason = f"timed out after {BUILD_TIMEOUT} seconds"
        stderr.seek(max(0, stderr.tell() - 4096))
        tail = stderr.read().decode(errors="replace").strip()[-2000:]
        raise ValueError(f"Bundle build {reason}. {tail}")


def stage(row, pin, directory):
    if row["kind"] == "rollback":
        target = layout.RELEASES / name(pin["release"])
        if installed_digest(target) != row["target_hash"]:
            raise ValueError("rollback image changed after inspection")
        return target
    target = directory / "bundle"
    if row.get("bundle"):
        source = layout.ROOT / "bundles" / name(row["bundle"])
        if bundle_digest(source) != row["target_hash"]:
            raise ValueError("bundle changed after inspection")
        shutil.copytree(source, target, symlinks=True)
        if bundle_digest(target) != row["target_hash"]:
            raise ValueError("staged bundle does not match approval")
    else:
        build(pin, target)
    meta = metadata(target)
    if meta["code_commit"] != row["commit"]:
        raise ValueError("bundle commit does not match approval")
    return target


def switch(row, pin, target, installer, worker_lock, validate):
    rid = name(pin["release"] if row["kind"] == "rollback" else metadata(target)["release"])
    deadline = time.monotonic() + WAIT_TIMEOUT
    state = layout.ROOT / "state"
    layout.ensure_private_root(state)
    if row["kind"] == "install" and (state / "release-rollback-pending").exists():
        raise ValueError("Resolve the failed release by rolling back before installing again.")
    with (state / "restart-intake.lock").open("a") as freeze:
        frozen = False
        while time.monotonic() < deadline:
            try:
                if not frozen:
                    fcntl.flock(freeze, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    frozen = True
                with layout.install_lock("hosted release") as lock:
                    if validate:
                        validate(row)
                    if time.monotonic() >= deadline:
                        break
                    code, result = attempt(row, pin, target, installer, rid, lock, deadline, freeze, worker_lock)
                if code != 2:
                    return result
            except BlockingIOError:
                if frozen:
                    raise
            except layout.Refused as error:
                if not str(error).startswith("install lock held by "):
                    raise
            remaining = deadline - time.monotonic()
            if remaining > 0:
                time.sleep(min(5, remaining))
    return {"status": "timed_out", "reason": "Release switch timed out waiting for the install lock or active jobs."}



def attempt(row, pin, target, installer, rid, lock, deadline, freeze, worker_lock):
    if layout.current_id() != pin["before"] or local_schema() != pin["schema_current"]:
        raise ValueError("release or schema changed since inspection")
    rollback = row["kind"] == "rollback"
    if rollback:
        if installed_digest(target) != row["target_hash"]:
            raise ValueError("rollback target changed")
        argv = ["switch", rid, "--no-wait"]
    else:
        existing = layout.RELEASES / rid
        cached = layout.RELEASES / f".hosted-{rid}.json"
        if existing.exists() or existing.is_symlink():
            approved = read_json(cached) or {}
            if (rid == pin["before"] or approved.get("target_hash") != row["target_hash"]
                    or approved.get("installed_hash") != installed_digest(existing)):
                raise ValueError("release ID is already installed with an unapproved image")
        argv = ["install", str(target), "--no-wait", "--hosted-trial", "--skip-path-setup", "--yes"]
    fd = lock.fileno()
    env = {**os.environ, layout.INSTALL_LOCK_FD: str(fd), "CS_HOSTED_RELEASE": "1",
           "CS_HOSTED_RELEASE_DEADLINE": str(deadline),
           layout.RESTART_INTAKE_FD: str(freeze.fileno())}
    if pin["serve_director_rs"]:
        env["CS_HOSTED_RELEASE_SCHEMA"] = str(pin["schema_target"])
    done = subprocess.run([installer, *argv], text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          env=env, pass_fds=(fd, freeze.fileno(), worker_lock.fileno()))
    try:
        result = json.loads(done.stdout)
    except ValueError:
        result = {}
    if not isinstance(result, dict):
        result = {}
    if not rollback and existing.is_dir() and done.returncode and layout.current_id() == pin["before"]:
        # Only an image prepared by this worker for this exact approval may be reused.
        try:
            write_json(cached, {"target_hash": row["target_hash"], "installed_hash": installed_digest(existing)})
        except (OSError, ValueError):
            cached.unlink(missing_ok=True)
    if done.returncode == 0 and result.get("release") == rid and layout.current_id() == rid:
        if rollback:
            return 0, {"status": "restored"}
        if (result.get("smoke") or {}).get("ok") is True:
            return 0, {"status": "installed", "smoke": result["smoke"]}
    failure = result.get("refused", "")
    if failure.startswith("hosted post-install smoke failed") and layout.current_id() == pin["before"]:
        return 1, {"status": "rolled_back", "reason": failure, "smoke": result.get("smoke")}
    if failure.startswith("hosted post-install smoke failed") or result.get("waiting_jobs") is not None:
        return 1, {"status": "rollback_failed", "reason": failure, "smoke": result.get("smoke"),
                   "waiting_jobs": result.get("waiting_jobs")}
    if result.get("timed_out") is True:
        return 1, {"status": "timed_out", "reason": failure}
    if done.returncode < 0 or layout.current_id() != pin["before"]:
        return 1, {"status": "interrupted", "reason": "Installer exited before confirming the switch outcome.",
                   "switched_without_smoke": not rollback and layout.current_id() == rid}
    return done.returncode, {"status": "failed", "reason": failure or clip(done.stderr.strip()[-2000:], 2000)}


def execute(path, installer, validate=None):
    with path.with_suffix(".lock").open("a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return
        receipt = read_json(path)
        if not receipt:
            return
        if receipt.get("outcome"):
            return receipt["outcome"]
        pin, row = receipt["pin"], receipt["row"]
        live = layout.current_id() or "unknown"
        result = {"status": "interrupted", "reason": "Release worker exited before writing its receipt.",
                  "switched_without_smoke": row["kind"] == "install" and live != pin["before"]}
        if not receipt.get("started"):
            receipt["started"] = True
            write_json(path, receipt)
            try:
                with tempfile.TemporaryDirectory(prefix="stage-", dir=path.parent) as work:
                    if validate:
                        validate(row)
                    target = stage(row, pin, Path(work))
                    result = switch(row, pin, target, installer, lock, validate)
            except DirectorUnreachable as error:
                result = {"status": "interrupted", "reason": f"Could not revalidate release claim: {error}"}
            except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError, layout.Refused) as error:
                result = {"status": "failed", "reason": str(error)}
        receipt["outcome"] = {**result, "target_hash": row["target_hash"],
                              "release": layout.current_id() or "unknown"}
        receipt["outcome"] = redact_obj(receipt["outcome"])
        write_json(path, receipt)
        return receipt["outcome"]


def launch(path, installer, config):
    argv = [sys.executable, "-I", str(HERE), str(path), str(installer), str(config)]
    if sys.platform.startswith("linux"):
        subprocess.run(["systemd-run", "--user", "--collect", "--unit=cs-release-" + path.stem + "-" + uuid.uuid4().hex[:8],
                        "--setenv=PATH=" + os.environ["PATH"], "--quiet", *argv],
                       stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                       timeout=10, check=False)
    else:
        subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                         start_new_session=True, cwd=path.parent)


def poll(client, state, code, config):
    directory = state / "node/release-requests"
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    rows = client.call("GET", "release-requests").get("release_requests", [])
    for row in rows:
        if not ID.fullmatch(row["id"]):
            continue
        path = directory / (row["id"] + ".json")
        prefix = "release-requests/" + row["id"]
        try:
            poll_row(client, row, path, prefix, code, config)
        except DirectorUnreachable as error:
            if error.status != 409:
                raise


def poll_row(client, row, path, prefix, code, config):
    if row["status"] == "pending":
        try:
            pin = inspect(row, config)
        except (OSError, ValueError, KeyError, TypeError, IndexError, subprocess.SubprocessError, layout.Refused):
            client.call("POST", prefix + "/unavailable", {})
            return
        write_json(path, {"pin": pin})
        client.call("POST", prefix + "/inspect", pin)
    else:
        receipt = read_json(path) or {}
        if row.get("outcome", {}).get("status") == "interrupted":
            with path.with_suffix(".lock").open("a") as lock:
                try:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    return
                live = layout.current_id() or "unknown"
                old = (receipt.get("pin") or {}).get("before")
                client.call("POST", prefix + "/outcome", {
                    "status": "interrupted", "release": live, "target_hash": row["target_hash"],
                    "reason": row["outcome"]["reason"],
                    "switched_without_smoke": row["kind"] == "install" and old is not None
                    and old != live and not (receipt.get("outcome") or {}).get("smoke"),
                })
                path.unlink(missing_ok=True)
            return
        if receipt.get("outcome"):
            client.call("POST", prefix + "/outcome", receipt["outcome"])
            path.unlink(missing_ok=True)
            return
        if not receipt.get("row"):
            client.call("POST", prefix + "/claim", {"target_hash": row["target_hash"]})
            if (receipt.get("pin") or {}).get("target_hash") != row["target_hash"]:
                client.call("POST", prefix + "/outcome", {"status": "failed",
                            "target_hash": row["target_hash"], "release": layout.current_id() or "unknown"})
                return
            receipt["row"] = {key: row[key] for key in ("kind", "commit", "target_hash")}
            if row.get("bundle"):
                receipt["row"]["bundle"] = row["bundle"]
            write_json(path, receipt)
        with path.with_suffix(".lock").open("a") as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                return
        launch(path, code / "scripts/node-install", config)


if __name__ == "__main__":
    path, installer, config = map(Path, sys.argv[1:])
    try:
        receiver = receiver_config(read_json(config / "policy/spool.json")["receiver"])
        token = node_env(config / "node.env").get(receiver["token_env"], "")
        client = Client(receiver, token=token, timeout=5)
        release_path = "release-requests/" + path.stem
        result = execute(path, installer, lambda row: client.call(
            "POST", release_path + "/claim", {"target_hash": row["target_hash"]}))
        if result:
            client.call("POST", release_path + "/outcome", result)
            path.unlink(missing_ok=True)
    except (DirectorUnreachable, OSError, TypeError, KeyError, ValueError):
        pass

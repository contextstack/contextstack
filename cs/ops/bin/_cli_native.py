"""Per-template runtime staging and native terminal checks for the plain executor's CLIs. Not an op.

A template may name where its CLI keeps runtime state, so the sandbox's write-deny on personal config
(~/.cursor, ~/.gemini, ~/.codex, ...) never stalls a run:
- "env": {NAME: value}; `{{tmp}}` is the run's private tmp dir, and a value under it is created as a dir.
  Codex needs `CODEX_HOME` under `{{tmp}}` and a sibling `TMPDIR` (also under `{{tmp}}`) so release builds
  still create PATH aliases: they refuse when `CODEX_HOME` lies under `std::env::temp_dir()`.
- "home": "private": HOME is `{{tmp}}/home`.
- "keychain": "login": stage a login-Keychain link on macOS (into `{{tmp}}/home/Library/Keychains` when
  HOME is private, else `{{tmp}}/login-keychain`). Only templates whose CLI reads the login Keychain set it.
- "links": {path under {{tmp}}: "~/..."}: read-only symlinks to personal files the CLI needs (its login
  metadata), made only when the source exists and is a regular file (or goose OAuth dir) owned by the user.
  Writes remain denied except for explicitly declared login_writes.
- "copies": {private path: "~/..."}: isolated login files; the host guards refresh write-back.
"login_writes": [{"literal"|"subpath"|"regex": "~/..."}] grants only the login file and its
CLI lock/temp siblings, with symlink creation denied. Regexes are anchored to the resolved home directory and final filename.
Links are removed before evidence is published, so the run never keeps a path into personal files.

"terminal" names the CLI's own completion receipt: "result_event" (a successful result event),
"cursor_result" (a result event or a final assistant text message) or "antigravity" (a final
{"event":"result","result":{"status":"SUCCESS"}}) or "gemini_result" (a final {"type":"result","status":"success"}).
A zero exit without it is not done. Harness kinds (pi_json, grok_json, goose_json) are checked
in _harness_usage.terminal_stop. Usage-limit stderr (Cursor ActionRequiredError, Gemini IneligibleTierError, …) is
classified in _provider_limits.detect_limit.
"""
import json
import subprocess
import os
import re
import shutil
import sys
import stat
import tempfile
from pathlib import Path


CLAUDE_LOGIN_PATH = ".claude/.credentials.json"

HOSTED_PATHS = (
    ".contextstack/director-db-backup",
    ".contextstack/browser",
    ".contextstack/state/browser-state",
    ".contextstack/config/lead-credential",
    ".contextstack/config/hosted-lead.json",
    ".contextstack/state/lead-sessions",
    ".contextstack/dev-state/lead-sessions",
)

LOGIN_PATHS = (
    CLAUDE_LOGIN_PATH,
    ".codex/auth.json",
    ".gemini/settings.json",
    ".gemini/oauth_creds.json",
    ".gemini/google_accounts.json",
    ".gemini/gemini-credentials.json",
    ".gemini/installation_id",
    ".pi/agent/auth.json",
    ".pi/agent/settings.json",
    ".pi/agent/models.json",
    ".config/goose/config.yaml",
    ".config/goose/chatgpt_codex",
    ".config/goose/gemini_oauth",
    ".config/goose/xai_oauth",
    ".grok/auth.json",
)
_DIR_LOGIN_LINKS = {
    "~/.config/goose/chatgpt_codex",
    "~/.config/goose/gemini_oauth",
    "~/.config/goose/xai_oauth",
}


def _sub(value, tmp):
    return str(value).replace("{{tmp}}", str(tmp))


def _login_targets(tmpl, real_home):
    real_home = Path(real_home)
    out = []
    seen = {}
    for raw_src in (tmpl.get("links") or {}).values():
        raw_str = str(raw_src)
        src = real_home / raw_str[2:] if raw_str.startswith("~/") else Path(raw_str)
        if not src.is_absolute():
            continue
        allow_dir = raw_str in _DIR_LOGIN_LINKS
        if src not in seen:
            seen[src] = len(out)
            out.append((src, allow_dir))
    for entry in tmpl.get("login_writes") or []:
        raw = entry.get("literal")
        if raw and str(raw).startswith("~/"):
            src = real_home / str(raw)[2:]
            if src in seen:
                out[seen[src]] = (src, False)
            else:
                seen[src] = len(out)
                out.append((src, False))
    return out


def _check_login_source(src, real_home, allow_dir=False):
    uid = os.getuid()
    real_home_abs = Path(real_home).absolute()
    cur = Path(src).absolute()
    while cur != real_home_abs and cur != cur.parent:
        if cur.is_symlink():
            raise ValueError("login path is a symlink: %s" % cur)
        cur = cur.parent
    if not src.exists():
        return
    st = src.lstat()
    if st.st_uid != uid:
        raise ValueError("login file not owned by current user: %s" % src)
    if allow_dir and src.is_dir():
        for child in src.rglob("*"):
            if child.is_symlink():
                raise ValueError("login path is a symlink: %s" % child)
            if child.lstat().st_uid != uid:
                raise ValueError("login file not owned by current user: %s" % child)
            if child.is_file() and child.lstat().st_nlink != 1:
                raise ValueError("login file has multiple hardlinks: %s" % child)
            if not (child.is_file() or child.is_dir()):
                raise ValueError("login file is not a regular file: %s" % child)
        return
    if src.is_file() and st.st_nlink != 1:
        raise ValueError("login file has multiple hardlinks: %s" % src)
    if not src.is_file():
        raise ValueError("login file is not a regular file: %s" % src)


def stage_claude_login(runtime_home, real_home):
    """Stage Claude's optional file login using the guarded private-copy lifecycle."""
    runtime_home = Path(runtime_home).resolve()
    return stage_copies({"copies": {
        str(runtime_home / CLAUDE_LOGIN_PATH): "~/" + CLAUDE_LOGIN_PATH,
    }}, runtime_home, real_home)


def snapshot_login_files(tmpl, real_home):
    """Validate and snapshot pre-run login files in memory so tampered files can be restored after the run."""
    real_home = Path(real_home)
    snapshot = {}
    for src, allow_dir in _login_targets(tmpl, real_home):
        _check_login_source(src, real_home, allow_dir=allow_dir)
        if src.is_file():
            st = src.lstat()
            snapshot[src] = ("file", st.st_mode & 0o777,
                             None if src == real_home / CLAUDE_LOGIN_PATH else src.read_bytes(),
                             (st.st_dev, st.st_ino))
        elif allow_dir and src.is_dir():
            snapshot[src] = ("dir", None, None, None)
        else:
            snapshot[src] = ("missing", None, None, None)
    for entry in tmpl.get("login_writes") or []:
        raw = entry.get("subpath")
        if raw and str(raw).startswith("~/"):
            _check_login_source(real_home / str(raw)[2:], real_home, allow_dir=True)
    return snapshot


def verify_login_files(tmpl, real_home, snapshot=None):
    """Require single-link, user-owned regular login files; fail on tampering. A changed inode is a refresh."""
    real_home = Path(real_home)
    uid = os.getuid()
    real_home_abs = real_home.absolute()
    errors = []
    for src, allow_dir in _login_targets(tmpl, real_home):
        prev_kind, prev_mode, prev_bytes, prev_identity = (snapshot or {}).get(src, (None, None, None, None))
        tampered = False
        reason = None
        cur = src.absolute()
        while cur != real_home_abs and cur != cur.parent:
            if cur.is_symlink():
                tampered = True
                reason = "login path is a symlink: %s" % cur
                break
            cur = cur.parent
        if not tampered:
            if not src.exists():
                if prev_kind in ("file", "dir"):
                    tampered = True
                    reason = "login file removed during run: %s" % src
            else:
                st = src.lstat()
                if st.st_uid != uid:
                    tampered = True
                    reason = "login file not owned by current user: %s" % src
                elif src.is_file() and st.st_nlink != 1:
                    tampered = True
                    reason = "login file has multiple hardlinks: %s" % src
                elif prev_kind == "dir" or (prev_kind is None and allow_dir and src.is_dir()):
                    if not src.is_dir():
                        tampered = True
                        reason = "login directory changed type: %s" % src
                    else:
                        for child in src.rglob("*"):
                            if child.is_symlink() or child.lstat().st_uid != uid or (child.is_file() and child.lstat().st_nlink != 1) or not (child.is_file() or child.is_dir()):
                                tampered = True
                                reason = "login directory child invalid: %s" % child
                                break
                elif not src.is_file():
                    tampered = True
                    reason = "login file is not a regular file: %s" % src
        if tampered:
            errors.append(reason)
            # The owner's own CLI shares these files: never delete a regular login that has no
            # snapshot to put back (6th pass R2 deleted ~/.claude/.credentials.json this way).
            if src.is_file() and not src.is_symlink() and prev_bytes is None:
                continue
            try:
                if src.is_symlink() or src.is_file():
                    src.unlink()
                elif src.is_dir() and prev_kind == "file":
                    shutil.rmtree(src)
                if prev_kind == "file" and prev_bytes is not None:
                    src.parent.mkdir(parents=True, exist_ok=True)
                    src.write_bytes(prev_bytes)
                    os.chmod(src, prev_mode or 0o600)
            except OSError as exc:
                errors.append("restore failed for %s: %s" % (src, exc))
    for entry in tmpl.get("login_writes") or []:
        if "subpath" in entry and str(entry["subpath"]).startswith("~/"):
            lock = real_home / str(entry["subpath"])[2:]
            if lock.is_symlink():
                lock.unlink(missing_ok=True)
                errors.append("login lock path is a symlink: %s" % lock)
            elif lock.is_dir():
                for child in lock.rglob("*"):
                    if child.is_symlink():
                        child.unlink(missing_ok=True)
                        errors.append("login lock child is a symlink: %s" % child)
        elif "regex" in entry and str(entry["regex"]).startswith("~/"):
            parent, name = str(entry["regex"])[2:].rsplit("/", 1)
            directory = real_home / parent
            if directory.is_dir():
                pat = re.compile("^" + name + "$")
                for child in directory.iterdir():
                    if pat.match(child.name) and child.is_symlink():
                        child.unlink(missing_ok=True)
                        errors.append("login temp path is a symlink: %s" % child)
    if errors:
        raise ValueError("; ".join(errors))


def _credential_read(path, *, trusted_directory=None):
    """Read one owned regular inode without following links below a trusted directory."""
    path = Path(path)
    if not path.is_absolute() or ".." in path.parts:
        raise ValueError("invalid credential path")
    anchor = Path(trusted_directory or path.anchor)
    parts = path.relative_to(anchor).parts
    directory = os.open(anchor, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        for part in parts[:-1]:
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=directory)
            os.close(directory)
            directory = child
        fd = os.open(path.name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=directory)
        with os.fdopen(fd, "rb") as stream:
            before = os.fstat(stream.fileno())
            if not stat.S_ISREG(before.st_mode) or before.st_nlink != 1 or before.st_uid != os.getuid():
                raise ValueError("invalid credential inode")
            data = stream.read()
            after = os.fstat(stream.fileno())
            if _credential_revision(before) != _credential_revision(after):
                raise ValueError("credential changed while reading")
        return data, _credential_revision(after)
    finally:
        os.close(directory)


def _credential_revision(st):
    return (st.st_dev, st.st_ino, st.st_size, st.st_mtime_ns, st.st_ctime_ns, st.st_mode, st.st_nlink)


def _credential_identity(source, data):
    # Codex: codex-rs/login/src/token_data.rs, TokenData.account_id (openai/codex).
    # Pi: packages/ai/src/auth/oauth/openai-codex.ts, credentialsFromToken.accountId.
    value = json.loads(data, parse_constant=_reject_json_constant)
    if not isinstance(value, dict):
        return None
    if source == "~/.codex/auth.json":
        tokens = value.get("tokens")
        identity = tokens.get("account_id") if isinstance(tokens, dict) else None
        return identity if isinstance(identity, str) and identity else None
    if source == "~/.grok/auth.json":
        # xai-org/grok-build: xai-grok-login/src/model.rs, GrokAuth.
        identity = value.get("user_id")
        if not isinstance(identity, str) or not identity:
            return None
        return {key: value.get(key) for key in ("user_id", "auth_mode", "principal_type",
                "principal_id", "team_id", "oidc_issuer", "oidc_client_id")}
    if source == "~/.pi/agent/auth.json":
        identities = {}
        for provider, credential in value.items():
            identity = credential.get("accountId") if isinstance(credential, dict) else None
            if not isinstance(identity, str) or not identity:
                return None  # Unknown identities fail closed, including API-key stores.
            identities[provider] = identity
        return identities or None
    return None


def stage_copies(tmpl, tmp, real_home):
    """Stage private 0600 copies and retain host-only originals, never evidence contents."""
    records = []
    tmp = Path(tmp).resolve()
    real_home = Path(real_home).resolve()
    for raw_dest, raw_src in (tmpl.get("copies") or {}).items():
        if raw_src not in ("~/" + CLAUDE_LOGIN_PATH, "~/.codex/auth.json", "~/.pi/agent/auth.json", "~/.grok/auth.json"):
            raise ValueError("unsupported copied login source")
        src = real_home / raw_src[2:]
        _check_login_source(src, real_home)
        if not src.exists():
            continue
        dest = Path(_sub(raw_dest, tmp))
        # Check the parent, never the leaf: a persistent HOME (the hosted lead's) can still hold
        # the old symlink to the real login, which must be replaced, not followed (2026-11-12).
        if not dest.is_absolute() or not dest.parent.resolve().is_relative_to(tmp):
            raise ValueError("copied login leaves private tmp")
        data, revision = _credential_read(src)
        dest.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        if dest.is_symlink() or dest.is_file():
            dest.unlink()  # a stale link or a copy left by a crashed run, inside the private tmp
        elif dest.exists():
            raise ValueError("copied login destination is not a file: %s" % dest)
        fd = os.open(dest, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, "wb") as stream:
            os.fchmod(stream.fileno(), 0o600)
            stream.write(data)
        records.append(dict(source=raw_src, real=src, copy=dest, tmp=tmp, original=data, revision=revision))
    return records


def _discard_copy(record):
    """Remove private credentials or a substituted parent link without following it."""
    directory = os.open(record["tmp"], os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        parts = record["copy"].relative_to(record["tmp"]).parts
        for part in parts[:-1]:
            st = os.stat(part, dir_fd=directory, follow_symlinks=False)
            if not stat.S_ISDIR(st.st_mode):
                os.unlink(part, dir_fd=directory)
                return
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=directory)
            os.close(directory)
            directory = child
        st = os.stat(parts[-1], dir_fd=directory, follow_symlinks=False)
        if stat.S_ISDIR(st.st_mode):
            shutil.rmtree(parts[-1], dir_fd=directory)
        else:
            os.unlink(parts[-1], dir_fd=directory)
    finally:
        os.close(directory)


def _reject_json_constant(value):
    raise ValueError("non-JSON constant")


def finish_copies(records, *, claude_refresh=False, trusted_refresh=None):
    """Destroy task copies; writeback is reserved for fixed host trusted refreshes.

    The host must verify the fixed probe response (or refresh RPC) before opting in.
    Only content-free status enums may enter evidence.
    """
    results = []
    for record in records:
        status = "invalid_copy"
        temporary = None
        try:
            data, _ = _credential_read(record["copy"])
            if trusted_refresh is True and record["source"] != "~/" + CLAUDE_LOGIN_PATH and (
                _credential_identity(record["source"], data) is None or
                _credential_identity(record["source"], record["original"]) is None
            ):
                status = "identity_missing"
            elif data == record["original"]:
                status = "unchanged"
                if trusted_refresh is True:
                    current, revision = _credential_read(record["real"])
                    if current != record["original"] or revision != record["revision"]:
                        status = "owner_changed"
            elif not (trusted_refresh is True or (record["source"] == "~/" + CLAUDE_LOGIN_PATH and claude_refresh)):
                # A job's copy is agent-controlled data: never written back (8th pass #64).
                # Only a trusted no-tools refresh run may write back.
                status = "task_copy"
                try:
                    current, revision = _credential_read(record["real"])
                    if current != record["original"] or revision != record["revision"]:
                        status = "owner_changed"  # evidence only: the owner refreshed meanwhile
                except (OSError, ValueError):
                    pass
            else:
                status = "invalid_json"
                if record["source"] == "~/" + CLAUDE_LOGIN_PATH:
                    original = json.loads(record["original"], parse_constant=_reject_json_constant)
                    refreshed = json.loads(data, parse_constant=_reject_json_constant)
                    original_oauth = original.get("claudeAiOauth") if isinstance(original, dict) else None
                    refreshed_oauth = refreshed.get("claudeAiOauth") if isinstance(refreshed, dict) else None
                    if not isinstance(original_oauth, dict) or not isinstance(refreshed_oauth, dict):
                        status = "oauth_missing"
                    elif original_oauth.keys() != refreshed_oauth.keys():
                        status = "oauth_keys_changed"
                    else:
                        status = "validated"
                else:
                    identity = _credential_identity(record["source"], data)
                    original_identity = _credential_identity(record["source"], record["original"])
                    if identity is None or original_identity is None:
                        status = "identity_missing"
                    elif identity != original_identity:
                        status = "identity_changed"
                    else:
                        status = "validated"
                if status == "validated":
                    status = "owner_changed"
                    current, revision = _credential_read(record["real"])
                    if current == record["original"] and revision == record["revision"]:
                        fd, temporary = tempfile.mkstemp(prefix=".auth-refresh-", dir=record["real"].parent)
                        with os.fdopen(fd, "wb") as stream:
                            os.fchmod(stream.fileno(), 0o600)
                            stream.write(data)
                            stream.flush()
                            os.fsync(stream.fileno())
                        # Recheck after preparing the replacement to preserve observed owner refreshes.
                        current, revision = _credential_read(record["real"])
                        if current == record["original"] and revision == record["revision"]:
                            os.replace(temporary, record["real"])
                            status = "written_back"
        except (OSError, ValueError):
            # Reasons are enums only: exception strings may expose credential contents.
            pass
        finally:
            if temporary is not None:
                Path(temporary).unlink(missing_ok=True)
            try:
                _discard_copy(record)
            except OSError:
                pass
            results.append({"source": record["source"], "status": status})
    records.clear()
    return results


def stage(tmpl, tmp, real_home, copy_records=None):
    """(env overrides, links made). Raises ValueError on a path that leaves the private tmp dir or a symlinked login file."""
    tmp, real_home = Path(tmp), Path(real_home)
    env, links = {}, []

    def inside(path):
        path = Path(path)
        if not path.resolve().is_relative_to(tmp.resolve()):
            raise ValueError("template path leaves the run's tmp dir: %s" % path)
        return path

    for src, allow_dir in _login_targets(tmpl, real_home):
        _check_login_source(src, real_home, allow_dir=allow_dir)
    for entry in tmpl.get("login_writes") or []:
        raw = entry.get("subpath")
        if raw and str(raw).startswith("~/"):
            _check_login_source(real_home / str(raw)[2:], real_home, allow_dir=True)

    for key, raw in (tmpl.get("env") or {}).items():
        value = _sub(raw, tmp)
        if "{{tmp}}" in str(raw):
            inside(value).mkdir(parents=True, exist_ok=True)
        env[str(key)] = value
    if tmpl.get("home") == "private":
        home = inside(tmp / "home")
        home.mkdir(mode=0o700, exist_ok=True)
        if tmpl.get("keychain") == "login" and sys.platform == "darwin":
            (home / "Library").mkdir(exist_ok=True)
            link = home / "Library/Keychains"
            link.symlink_to(real_home / "Library/Keychains", target_is_directory=True)
            links.append(link)
        env["HOME"] = str(home)
    if tmpl.get("keychain") == "login" and sys.platform == "darwin" and tmpl.get("home") != "private":
        link = inside(tmp / "login-keychain")
        link.symlink_to(real_home / "Library/Keychains", target_is_directory=True)
        links.append(link)
    for raw_dest, raw_src in (tmpl.get("links") or {}).items():
        dest = inside(_sub(raw_dest, tmp))
        src = real_home / str(raw_src)[2:] if str(raw_src).startswith("~/") else Path(raw_src)
        if not src.is_absolute() or not src.exists():
            continue
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.symlink_to(src)
        links.append(dest)
    records = stage_copies(tmpl, tmp, real_home)
    if copy_records is not None:
        copy_records.extend(records)
    return env, links


SHARED_HOME_READS = (".cargo/registry", ".cargo/git", ".rustup", ".npm", "Library/Caches/pip", "go/pkg/mod")


def declared_home_reads(tmpl, real_home, tmp):
    """Expand declared roots without consulting personal configuration."""
    result = []
    for raw in tmpl.get("home_reads", []):
        if not isinstance(raw, str) or not (raw.startswith("~/") or raw.startswith("{{tmp}}/")):
            raise ValueError("home_reads must name ~/ or {{tmp}}/ paths")
        path = Path(real_home) / raw[2:] if raw.startswith("~/") else Path(_sub(raw, tmp))
        result.append(path.resolve())
    return result


def unstage(links):
    for link in links:
        Path(link).unlink(missing_ok=True)


def _objs(text):
    out = []
    for line in (text or "").splitlines():
        try:
            value = json.loads(line)
        except ValueError:
            continue
        if isinstance(value, dict):
            out.append(value)
    return out


def terminal_stop(kind, stdout):
    """None when the CLI reported its own successful completion; else the stop reason."""
    if not kind:
        return None
    objs = _objs(stdout)
    if kind == "cursor_result" and not any(o.get("type") == "result" for o in objs):
        # Cursor now ends with an assistant message, without a separate result. Inspect the
        # physical last record: skipping a malformed tail would accept a truncated stream.
        lines = [line for line in (stdout or "").splitlines() if line.strip()]
        try:
            final = json.loads(lines[-1]) if lines else None
        except ValueError:
            final = None
        if isinstance(final, dict) and final.get("type") == "assistant":
            message = final.get("message")
            blocks = message.get("content") if isinstance(message, dict) else None
            if (isinstance(message, dict) and message.get("role") == "assistant" and isinstance(final.get("session_id"), str)
                    and final["session_id"] and isinstance(blocks, list) and blocks
                    and all(isinstance(b, dict) and b.get("type") == "text"
                            and isinstance(b.get("text"), str) for b in blocks)
                    and any(b["text"].strip() for b in blocks)):
                return None
        return "native_terminal_missing"
    if kind in ("result_event", "cursor_result"):
        results = [o for o in objs if o.get("type") == "result"]
        if not results:
            return "native_terminal_missing"
        return None if results[-1].get("is_error") is False and results[-1].get("subtype") == "success" \
            else "native_terminal_error"
    if kind == "antigravity":
        results = [o.get("result") for o in objs if o.get("event") == "result" and isinstance(o.get("result"), dict)]
        if not results:
            return "native_terminal_missing"
        return None if str(results[-1].get("status") or "").upper() == "SUCCESS" else "native_terminal_error"
    if kind == "gemini_result":
        results = [o for o in objs if o.get("type") == "result"]
        if not results:
            return "native_terminal_missing"
        return None if results[-1].get("status") == "success" else "native_terminal_error"
    if kind in ("pi_json", "grok_json", "goose_json"):
        from _harness_usage import terminal_stop as harness_terminal
        return harness_terminal(kind, stdout)
    raise ValueError("unknown terminal kind %s" % kind)


def stage_git(tmp, home=None, *, run=None, env=None, home_dir=None):
    """Copy owner identity and ignore patterns, never executable Git settings."""
    run = subprocess.run if run is None else run
    env = os.environ if env is None else env
    home_dir = Path.home if home_dir is None else home_dir
    tmp = Path(tmp).resolve()
    tmp.mkdir(parents=True, exist_ok=True)
    home = Path(home) if home is not None else home_dir()
    # Host git sees only what it needs: never the parent's credentials or tokens.
    host = {k: env[k] for k in ('PATH', 'LANG', 'LC_ALL', 'XDG_CONFIG_HOME') if k in env}
    host.setdefault('PATH', '/usr/bin:/bin')
    host['HOME'] = str(home)
    host['GIT_CONFIG_NOSYSTEM'] = '1'
    # A global setting lookup needs no repository discovery above the staging directory.
    host['GIT_CEILING_DIRECTORIES'] = str(tmp.parent)

    def read(key, path=False):
        args = ['git', 'config', '--global'] + (['--path'] if path else []) + ['--get', key]
        result = run(args, env=host, cwd=tmp, capture_output=True, text=True,
                                stdin=subprocess.DEVNULL)
        if result.returncode not in (0, 1):
            raise ValueError('cannot read owner Git setting: ' + key)
        return result.stdout.rstrip('\n') if result.returncode == 0 else None

    config = tmp / 'gitconfig'
    config.unlink(missing_ok=True)
    config.touch(mode=0o600)
    env = {'GIT_CONFIG_GLOBAL': str(config), 'GIT_CONFIG_NOSYSTEM': '1'}
    # Commit's auto-maintenance can pack/prune every shared ref. Keep that host-only.
    # Command-scope settings override repository config, including on older gc-based Git.
    env.update(GIT_CONFIG_COUNT='2', GIT_CONFIG_KEY_0='maintenance.auto', GIT_CONFIG_VALUE_0='false',
               GIT_CONFIG_KEY_1='gc.auto', GIT_CONFIG_VALUE_1='0')
    values = {key: read(key) for key in ('user.name', 'user.email')}
    excludes = read('core.excludesFile', path=True)
    source = Path(excludes) if excludes else Path(host.get('XDG_CONFIG_HOME', str(home / '.config'))) / 'git/ignore'
    if not source.is_absolute():
        source = tmp / source
    if source.is_file():
        dest = tmp / 'gitignore'
        dest.unlink(missing_ok=True)
        dest.touch(mode=0o600)
        dest.write_bytes(source.read_bytes())
        values['core.excludesFile'] = str(dest)
    for key, value in values.items():
        if value is not None:
            run(['git', 'config', '--file', str(config), key, value],
                           env={**host, **env}, cwd=tmp, check=True, capture_output=True,
                           stdin=subprocess.DEVNULL)
    return env

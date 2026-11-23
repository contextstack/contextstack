"""Encrypted off-machine backups of the Director database and evals."""
import base64
from contextlib import closing
from datetime import datetime, timezone
import fcntl
import json
import os
from pathlib import Path
import re
import secrets
import shlex
import shutil
import socket
import sqlite3
import stat
import subprocess
import sys
import tarfile
import tempfile

from _atomic_write import write_atomic
from _backup_s3 import BackupError, S3
import _node_services as services

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "ops/bin"))
from _cli_native import _credential_read

KEY = "backup-encryption-key"
ACCESS = "backup-s3-access-key"
SECRET = "backup-s3-secret-key"
STAMP = "%Y%m%dT%H%M%S.%fZ"
SUFFIX = ".tar.csenc"
STATUS = "backup-status.json"
DB = "director.sqlite"
COMPONENT = re.compile(r"[A-Za-z0-9._-]+")
OBJECT = re.compile(r"\d{8}T\d{6}\.\d{6}Z" + re.escape(SUFFIX))


def add_arguments(parser):
    actions = parser.add_subparsers(dest="action", required=True)
    init = actions.add_parser("init", help="create the recovery key and install the nightly schedule")
    init.add_argument("--no-load", action="store_true", help="write the schedule without loading it")
    actions.add_parser("run", help="snapshot, encrypt, upload, and prune backups")
    actions.add_parser("status", help="show the nightly backup result (JSON)")
    restore = actions.add_parser("restore", help="restore a backup into a new or empty directory")
    restore.add_argument("object", help="latest, object basename, key, or s3:// URI")
    restore.add_argument("--to", type=Path, required=True)


def read_json(path):
    try:
        value = json.loads(path.read_text())
    except FileNotFoundError:
        return {}
    except (OSError, ValueError):
        raise BackupError(f"cannot read {path.name}") from None
    if not isinstance(value, dict):
        raise BackupError(f"invalid {path.name}")
    return value


def credential(root, name):
    try:
        home = root.parent.resolve()
        value, revision = _credential_read(home / root.name / "credentials" / name, trusted_directory=home)
        if revision[5] & 0o077:
            raise BackupError(f"credential {name} must be private (chmod 600)")
    except FileNotFoundError:
        raise BackupError(f"missing credential {name}") from None
    except (OSError, ValueError):
        raise BackupError(f"cannot read credential {name}") from None
    if not value.strip():
        raise BackupError(f"empty credential {name}")
    return value.strip()


def recovery_key(root):
    try:
        key = base64.b64decode(credential(root, KEY), validate=True)
    except ValueError:
        key = b""
    if len(key) != 32:
        raise BackupError(f"invalid credential {KEY} (expected a base64-encoded 32-byte key)")
    return key


def initialize(root, no_load):
    directory = root / "credentials"
    directory.mkdir(parents=True, mode=0o700, exist_ok=True)
    if directory.is_symlink() or root.is_symlink():
        raise BackupError("credential store must be a local directory")
    try:
        fd = os.open(directory / KEY, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except FileExistsError:
        recovery_key(root)
    else:
        with os.fdopen(fd, "wb") as stream:
            stream.write(base64.b64encode(secrets.token_bytes(32)) + b"\n")
            stream.flush()
            os.fsync(stream.fileno())
    print(f"Save a copy of {directory / KEY} in your password manager. Without it the backups are unreadable.")
    if not no_load and not (services.CURRENT / "cs/scripts/cs").is_file():
        raise BackupError("install ContextStack before loading the nightly schedule (or use --no-load)")
    services.install_backup_schedule(no_load)
    print("Nightly backup at 03:00 local time" + (" (schedule files written)" if no_load else " installed") + ".")


def component(value):
    return isinstance(value, str) and COMPONENT.fullmatch(value) and value not in (".", "..")


def destination(root):
    config_dir = root / "config"
    config = read_json(config_dir / "backup.json")
    if not config:
        raise BackupError("configure endpoint, bucket and prefix in config/backup.json")
    if not isinstance(config.get("endpoint"), str) or not component(config.get("bucket")):
        raise BackupError("backup.json requires an endpoint and a valid bucket name")
    prefix = config.get("prefix")
    if not isinstance(prefix, str):
        raise BackupError("backup.json requires a prefix")
    parts = prefix.strip("/").split("/") if prefix.strip("/") else []
    if any(not component(part) for part in parts):
        raise BackupError("backup prefix components must use letters, digits, dots, underscores or hyphens")
    machine = config.get("machine") or read_json(config_dir / "install.json").get("node") or socket.gethostname()
    if not component(machine):
        raise BackupError("set a valid machine name in backup.json")
    access = credential(root, ACCESS)
    if not re.fullmatch(rb"[A-Za-z0-9_-]+", access):
        raise BackupError(f"invalid credential {ACCESS}")
    s3 = S3(config, access.decode("ascii"), credential(root, SECRET))
    return s3, "/".join([*parts, machine, ""])


def stamped(key, prefix):
    name = key.removeprefix(prefix)
    if key.startswith(prefix) and OBJECT.fullmatch(name):
        try:
            return datetime.strptime(name.removesuffix(SUFFIX), STAMP).date()
        except ValueError:
            pass
    return None


def objects(s3, prefix):
    return sorted({key for key in s3.list(prefix) if stamped(key, prefix)}, reverse=True)


def prune(s3, prefix, uploaded):
    keys = objects(s3, prefix)
    if uploaded not in keys:
        raise BackupError("uploaded backup is missing from the S3 listing")
    days, weeks, keep = set(), set(), set()
    for key in keys:
        day = stamped(key, prefix)
        week = day.isocalendar()[:2]
        if len(days) < 14 and day not in days:
            days.add(day)
            keep.add(key)
        if len(weeks) < 8 and week not in weeks:
            weeks.add(week)
            keep.add(key)
    if uploaded not in keep:
        raise BackupError("uploaded backup is older than retention permits; check the machine clock")
    for key in keys:
        if key not in keep:
            with s3.request("DELETE", key):
                pass
    return len(keep)


def snapshot(source, target):
    if not source.is_file():
        raise BackupError("Director database is missing")
    with closing(sqlite3.connect(source.as_uri() + "?mode=ro", uri=True)) as live:
        # Pin the read view so a continuously writing WAL cannot restart the backup.
        live.execute("BEGIN")
        live.execute("SELECT 1 FROM sqlite_master LIMIT 1").fetchone()
        with closing(sqlite3.connect(target)) as copy:
            live.backup(copy, pages=256)
            copy.execute("PRAGMA journal_mode=DELETE")


def archive_evals(tar, evals):
    if not evals.is_symlink() and not evals.exists():
        return
    if evals.is_symlink() or not evals.is_dir():
        raise BackupError("evals must be a local directory")
    def walk_error(error):
        raise error

    for path, dirs, files, fd in os.fwalk(evals, onerror=walk_error, follow_symlinks=False):
        base = Path("evals") / Path(path).relative_to(evals)
        tar.addfile(directory_entry(base))
        for name in dirs:
            if not stat.S_ISDIR(os.stat(name, dir_fd=fd, follow_symlinks=False).st_mode):
                raise BackupError("unsupported evals entry: %s" % (base / name))
        for name in files:
            info = os.stat(name, dir_fd=fd, follow_symlinks=False)
            if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
                raise BackupError("unsupported evals entry: %s" % (base / name))
            source_fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=fd)
            with os.fdopen(source_fd, "rb") as stream:
                current = os.fstat(stream.fileno())
                if (not stat.S_ISREG(current.st_mode) or current.st_nlink != 1
                        or (current.st_dev, current.st_ino) != (info.st_dev, info.st_ino)):
                    raise BackupError("evals changed while reading")
                entry = tarfile.TarInfo((base / name).as_posix())
                entry.size = current.st_size
                entry.mode = 0o600
                tar.addfile(entry, stream)


def directory_entry(base):
    entry = tarfile.TarInfo(base.as_posix())
    entry.type = tarfile.DIRTYPE
    entry.mode = 0o700
    return entry


def crypto(root, mode, key, source, target):
    binary = root / "current/bin/cs-director"
    if not binary.is_file():
        binary = Path(__file__).resolve().parents[2] / "target/release/cs-director"
    if not binary.is_file():
        raise BackupError("cs-director is unavailable (build or install the current release)")
    with target.open("wb") as output, source.open("rb") as archive:
        with subprocess.Popen([binary, "backup-crypto", mode], stdin=subprocess.PIPE,
                              stdout=output, stderr=subprocess.DEVNULL) as process:
            try:
                with process.stdin as pipe:
                    pipe.write(key)
                    shutil.copyfileobj(archive, pipe)
            except BrokenPipeError:
                pass
            if process.wait():
                raise BackupError("backup encryption failed" if mode == "encrypt"
                                  else "backup decryption failed (wrong key or damaged archive)")


def run_backup(root, state, db):
    key = recovery_key(root)
    s3, prefix = destination(root)
    with tempfile.TemporaryDirectory(prefix=".backup-", dir=state) as temporary:
        work = Path(temporary)
        snapshot(db, work / DB)
        archive = work / "snapshot.tar"
        with tarfile.open(archive, "w") as tar:
            tar.add(work / DB, arcname=DB)
            archive_evals(tar, root / "evals")
        cipher = work / "snapshot.csenc"
        crypto(root, "encrypt", key, archive, cipher)
        now = datetime.now(timezone.utc)
        object_key = prefix + now.strftime(STAMP) + SUFFIX
        with cipher.open("rb") as body, s3.request("PUT", object_key, body=body):
            pass
        return {"last_success": now.isoformat(), "last_error": None, "last_error_at": None,
                "size_bytes": cipher.stat().st_size, "object_count": prune(s3, prefix, object_key),
                "object": f"s3://{s3.bucket}/{object_key}"}


def extract(archive, target):
    with tarfile.open(archive, "r:") as tar:
        for entry in tar:
            parts = entry.name.split("/")
            if (any(part in ("", ".", "..") for part in parts)
                    or (parts[0] != "evals" and entry.name != DB)
                    or (entry.name in ("evals", DB) and entry.isdir() != (entry.name == "evals"))
                    or not (entry.isdir() or entry.isreg())):
                raise BackupError("invalid member in backup archive")
            path = target.joinpath(*parts)
            if entry.isdir():
                path.mkdir(mode=0o700, parents=True, exist_ok=True)
            else:
                path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
                fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
                with os.fdopen(fd, "wb") as output, tar.extractfile(entry) as source:
                    shutil.copyfileobj(source, output)
    with closing(sqlite3.connect((target / DB).as_uri() + "?mode=ro", uri=True)) as restored:
        if restored.execute("PRAGMA integrity_check").fetchall() != [("ok",)]:
            raise BackupError("restored database failed SQLite integrity_check")


def restore(root, db, requested, target):
    if target.is_symlink():
        raise BackupError("restore destination must be a new or empty directory")
    target = target.resolve()
    live = db.parent.resolve()
    if target.is_relative_to(live) or live.is_relative_to(target):
        raise BackupError("restore destination would overlap the live Director database")
    if target.exists() and (not target.is_dir() or any(target.iterdir())):
        raise BackupError("restore destination must be a new or empty directory")
    key = recovery_key(root)
    s3, prefix = destination(root)
    if requested == "latest":
        keys = objects(s3, prefix)
        if not keys:
            raise BackupError("no backups found for this machine")
        object_key = keys[0]
    else:
        object_key = requested.removeprefix(f"s3://{s3.bucket}/")
        if "/" not in object_key:
            object_key = prefix + object_key
        if not stamped(object_key, prefix):
            raise BackupError("object must be a backup under this machine's configured prefix")
    with tempfile.TemporaryDirectory(prefix=".restore-", dir=target.parent) as temporary:
        work = Path(temporary)
        s3.download(object_key, work / "snapshot.csenc")
        crypto(root, "decrypt", key, work / "snapshot.csenc", work / "snapshot.tar")
        extracted = work / "extracted"
        extracted.mkdir(mode=0o700)
        extract(work / "snapshot.tar", extracted)
        extracted.rename(target)
    print(f"Restored to {target}")
    stop, start = "cs stop director", "cs start director"
    if services.host_platform() == "linux":
        unit = services.unit_name(services.DIRECTOR_RS_LABEL)
        stop, start = f"systemctl --user stop {unit}", f"systemctl --user start {unit}"
    print(f"To swap in the database, run '{stop}'. Move the existing database and any")
    print(f"{DB}-wal and {DB}-shm sidecars out of {live}, then copy")
    print(f"{shlex.quote(str(target / DB))} to {shlex.quote(str(db))} with mode 0600.")
    print(f"Run '{start}' and copy the restored evals into your evals directory as needed.")


def report_error(error):
    if isinstance(error, BackupError):
        return str(error)
    if isinstance(error, sqlite3.Error):
        return f"SQLite backup or restore failed ({getattr(error, 'sqlite_errorname', 'SQLite error')})"
    if isinstance(error, OSError):
        return f"local backup or restore operation failed (errno {error.errno})"
    return "invalid backup configuration or archive"


ERRORS = (BackupError, OSError, ValueError, TypeError, sqlite3.Error, tarfile.TarError)


def run_with_status(root, state, db, status_path):
    state.mkdir(mode=0o700, parents=True, exist_ok=True)
    with (state / "backup.lock").open("a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise BackupError("a backup is already running") from None
        previous = read_json(status_path)
        try:
            result = run_backup(root, state, db)
        except ERRORS as error:
            previous.update(last_error=report_error(error), last_error_at=datetime.now(timezone.utc).isoformat())
            write_atomic(status_path, json.dumps(previous) + "\n", mode=0o600)
            raise
        write_atomic(status_path, json.dumps(result) + "\n", mode=0o600)
        print(result["object"])


def main(args, root):
    state = Path(os.environ.get("CS_STATE_ROOT") or root / "state").resolve()
    db = state / "director" / DB
    status_path = state / STATUS
    try:
        if args.action == "init":
            initialize(root, args.no_load)
        elif args.action == "status":
            status = dict.fromkeys(("last_success", "last_error", "last_error_at", "size_bytes", "object_count", "object"))
            status.update(read_json(status_path))
            print(json.dumps(status, indent=2))
        elif args.action == "restore":
            restore(root, db, args.object, args.to)
        else:
            run_with_status(root, state, db, status_path)
    except ERRORS as error:
        print("cs backup: " + report_error(error), file=sys.stderr)
        return 1
    except services.Refused as error:
        print("cs backup: " + str(error), file=sys.stderr)
        return 1
    return 0

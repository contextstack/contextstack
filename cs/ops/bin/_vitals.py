"""Memory, swap, disk vitals and a resource guard for fleet nodes. Not an op.

read_vitals() is cheap (well under 100 ms) and never raises: every field it cannot read is null.
resource_held() applies policy/resources.json (and policy/spool.json min_free_bytes for disk when unset).
"""
import json
import shutil

from _provider_limits import status_limits
import os
import platform
import re
import subprocess
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
_PRESSURE = {1: "normal", 2: "warn", 4: "critical", 8: "critical"}
_VM_LINE = re.compile(r"^(.+?):\s+(\d+)\.")
_SWAP = re.compile(
    r"total\s*=\s*([\d.]+)\s*M\s+used\s*=\s*([\d.]+)\s*M\s+free\s*=\s*([\d.]+)\s*M",
    re.I,
)
_MEM = re.compile(r"^(\w+(?:\([^)]*\))?):\s+(\d+)\s*kB", re.M)
_PSI = re.compile(r"^some avg10=([\d.]+)", re.M)
_PSI_FULL = re.compile(r"^full avg10=([\d.]+)", re.M)


def _run(argv, timeout=0.25):
    try:
        return subprocess.run(
            argv,
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
        ).stdout or ""
    except (OSError, subprocess.TimeoutExpired):
        return ""


def _opt_int(value):
    try:
        if value is None:
            return None
        n = int(value)
        return n if n >= 0 else None
    except (TypeError, ValueError):
        return None


def _opt_float(value):
    try:
        if value is None:
            return None
        n = float(value)
        return n if n >= 0 else None
    except (TypeError, ValueError):
        return None


def parse_vm_stat(text, page_size=4096):
    """Pages from macOS vm_stat(1) output into available and derived byte counts."""
    pages = {}
    for line in (text or "").splitlines():
        match = _VM_LINE.match(line.strip())
        if match:
            pages[match.group(1).strip()] = int(match.group(2))
    if not pages:
        return None, None
    free = pages.get("Pages free", 0) + pages.get("Pages speculative", 0)
    inactive = pages.get("Pages inactive", 0) + pages.get("Pages purgeable", 0)
    avail_pages = free + inactive
    total_pages = sum(pages.values())
    return avail_pages * page_size, total_pages * page_size


def parse_meminfo(text):
    """Linux /proc/meminfo into (available_bytes, total_bytes)."""
    fields = {m.group(1): int(m.group(2)) * 1024 for m in _MEM.finditer(text or "")}
    total = fields.get("MemTotal")
    avail = fields.get("MemAvailable")
    if avail is None and total is not None:
        free = fields.get("MemFree", 0)
        cache = fields.get("Cached", 0) + fields.get("SReclaimable", 0)
        buffers = fields.get("Buffers", 0)
        avail = free + cache + buffers
    return _opt_int(avail), _opt_int(total)


def parse_swapusage(text):
    """macOS sysctl vm.swapusage into (used_bytes, total_bytes)."""
    match = _SWAP.search(text or "")
    if not match:
        return None, None
    scale = 1024 * 1024
    total = int(float(match.group(1)) * scale)
    used = int(float(match.group(2)) * scale)
    return used, total


def parse_proc_swap(text):
    """Linux /proc/swaps first device line into (used_bytes, total_bytes)."""
    for line in (text or "").splitlines()[1:]:
        parts = line.split()
        if len(parts) >= 4:
            try:
                used = int(parts[3]) * 1024
                total = int(parts[2]) * 1024
                return used, total
            except ValueError:
                continue
    return None, None


def parse_pressure_psi(text):
    """Linux PSI memory line into a coarse memory_pressure label."""
    some = _PSI.search(text or "")
    full = _PSI_FULL.search(text or "")
    if not some and not full:
        return None
    s = float(some.group(1)) if some else 0.0
    f = float(full.group(1)) if full else 0.0
    if f >= 1.0:
        return "critical"
    if f > 0 or s >= 25.0:
        return "warn"
    if s >= 5.0:
        return "warn"
    return "normal"


def parse_kern_pressure(text):
    try:
        level = int((text or "").strip())
    except ValueError:
        return None
    return _PRESSURE.get(level, "unknown")


def disk_free_bytes(path):
    try:
        st = os.statvfs(str(path))
        return _opt_int(st.f_bavail * st.f_frsize)
    except (OSError, AttributeError):
        return None


def read_vitals(state_root=None, config_root=None, *, disk_free=None):
    """Snapshot vitals for the state root; every field is a number, a string label, or null."""
    state_root = Path(state_root or os.environ.get("LAB_STATE_ROOT") or HERE.parent.parent / "ledger")
    out = {
        "memory_total_bytes": None,
        "memory_available_bytes": None,
        "memory_pressure": None,
        "swap_total_bytes": None,
        "swap_used_bytes": None,
        "disk_free_bytes": _opt_int(disk_free) if disk_free is not None else disk_free_bytes(state_root),
    }
    system = platform.system()
    if system == "Darwin":
        try:
            total = int(_run(["sysctl", "-n", "hw.memsize"]).strip())
            out["memory_total_bytes"] = _opt_int(total)
        except ValueError:
            pass
        page = 4096
        try:
            page = int(_run(["sysctl", "-n", "hw.pagesize"]).strip() or page)
        except ValueError:
            pass
        vm = _run(["vm_stat"])
        avail, _approx = parse_vm_stat(vm, page)
        out["memory_available_bytes"] = avail
        out["memory_pressure"] = parse_kern_pressure(_run(["sysctl", "-n", "kern.memorystatus_vm_pressure_level"]))
        used, total = parse_swapusage(_run(["sysctl", "vm.swapusage"]))
        out["swap_used_bytes"] = used
        out["swap_total_bytes"] = total
    elif system == "Linux":
        try:
            avail, total = parse_meminfo(Path("/proc/meminfo").read_text(encoding="utf-8", errors="replace"))
            out["memory_available_bytes"] = avail
            out["memory_total_bytes"] = total
        except OSError:
            pass
        psi_path = Path("/proc/pressure/memory")
        if psi_path.is_file():
            try:
                out["memory_pressure"] = parse_pressure_psi(
                    psi_path.read_text(encoding="utf-8", errors="replace")
                )
            except OSError:
                pass
        try:
            used, total = parse_proc_swap(Path("/proc/swaps").read_text(encoding="utf-8", errors="replace"))
            out["swap_used_bytes"] = used
            out["swap_total_bytes"] = total
        except OSError:
            pass
    return out


def load_resources_policy(config_root=None):
    config_root = Path(config_root or os.environ.get("LAB_CONFIG_ROOT") or HERE.parent.parent)
    path = config_root / "policy/resources.json"
    defaults = {
        "min_available_memory_fraction": 0.10,
        "max_swap_used_fraction": 0.90,
        "min_free_disk_bytes": None,
        "hold_at_memory_pressure": "critical",
        "job_memory_fraction": 0.25,
    }
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError, TypeError):
        return dict(defaults)
    out = dict(defaults)
    for key in defaults:
        if key in raw and raw[key] is not None:
            out[key] = raw[key]
    if out["min_free_disk_bytes"] is None:
        spool = config_root / "policy/spool.json"
        try:
            out["min_free_disk_bytes"] = int(json.loads(spool.read_text(encoding="utf-8")).get("min_free_bytes") or 0)
        except (OSError, ValueError, TypeError):
            out["min_free_disk_bytes"] = 0
    return out


def _darwin_pressure_hold(pressure, hold_at):
    if pressure is None:
        return False
    if hold_at == "warn":
        return pressure in ("warn", "critical")
    return pressure == "critical"


def _memory_fraction_low(vitals, min_frac):
    total = vitals.get("memory_total_bytes")
    avail = vitals.get("memory_available_bytes")
    if min_frac is None or not total or avail is None or total <= 0:
        return False
    return avail / total < min_frac


def _refresh_memory_pressure(vitals):
    """Re-read kernel pressure on a cached snapshot (cheap; enough for held transitions)."""
    vitals = dict(vitals or {})
    system = platform.system()
    if system == "Darwin":
        vitals["memory_pressure"] = parse_kern_pressure(
            _run(["sysctl", "-n", "kern.memorystatus_vm_pressure_level"])
        )
    elif system == "Linux":
        psi_path = Path("/proc/pressure/memory")
        if psi_path.is_file():
            try:
                vitals["memory_pressure"] = parse_pressure_psi(
                    psi_path.read_text(encoding="utf-8", errors="replace")
                )
            except OSError:
                pass
    return vitals


def _vitals_cache_may_change(vitals, policy):
    """True when a TTL cache might hide a held↔clear transition (node-listen refresh)."""
    if resource_held(vitals, policy):
        return True
    if vitals.get("memory_pressure") in ("warn", "critical"):
        return True
    min_frac = _opt_float(policy.get("min_available_memory_fraction"))
    total = vitals.get("memory_total_bytes")
    avail = vitals.get("memory_available_bytes")
    if min_frac and total and avail is not None and total > 0 and avail / total < min_frac * 1.15:
        return True
    swap_total = vitals.get("swap_total_bytes")
    swap_used = vitals.get("swap_used_bytes")
    max_swap = _opt_float(policy.get("max_swap_used_fraction"))
    if platform.system() == "Linux" and max_swap and swap_total and swap_used is not None and swap_total > 0:
        if swap_used / swap_total >= max_swap * 0.95:
            return True
    floor = _opt_int(policy.get("min_free_disk_bytes"))
    disk = vitals.get("disk_free_bytes")
    if floor and disk is not None and disk < floor * 1.05:
        return True
    return False


_VITALS_CACHE = {}


def resource_held(vitals, policy=None):
    """None when the node may accept work; else {"reason": "<code>"}."""
    vitals = vitals or {}
    policy = policy or load_resources_policy()
    system = platform.system()
    min_frac = _opt_float(policy.get("min_available_memory_fraction"))
    pressure = vitals.get("memory_pressure")
    if system == "Darwin":
        hold_at = str(policy.get("hold_at_memory_pressure") or "critical")
        if pressure is not None:
            if _darwin_pressure_hold(pressure, hold_at):
                return {"reason": "memory_pressure"}
        elif _memory_fraction_low(vitals, min_frac):
            return {"reason": "memory_low"}
    else:
        if _memory_fraction_low(vitals, min_frac):
            return {"reason": "memory_low"}
    swap_total = vitals.get("swap_total_bytes")
    swap_used = vitals.get("swap_used_bytes")
    max_swap = _opt_float(policy.get("max_swap_used_fraction"))
    # Darwin's vm.swapusage stays nearly full even when memory is healthy; guard on Linux only.
    if platform.system() == "Linux" and max_swap is not None and swap_total and swap_total > 0 and swap_used is not None:
        if swap_used / swap_total >= max_swap:
            return {"reason": "swap_high"}
    floor = _opt_int(policy.get("min_free_disk_bytes"))
    disk = vitals.get("disk_free_bytes")
    if floor and disk is not None and disk < floor:
        return {"reason": "disk_low"}
    return None


def agent_capabilities(config_root):
    """Report init's sign-in checks, filtering binaries removed since setup."""
    try:
        agents = json.loads((Path(config_root) / "agents.json").read_text())
        templates = json.loads((HERE / "executor-templates.json").read_text())
    except (OSError, ValueError):
        return {}
    try:
        install = json.loads((Path(config_root) / "install.json").read_text())
    except (OSError, ValueError):
        install = {}
    ready = {a["name"] for a in agents if a.get("signed_in") is True and a.get("path")
             and shutil.which(a["path"])}
    return {"agent_capabilities": {"templates": [name for name, template in templates.items()
            if Path(template.get("binary") or "").name in ready], "preferred": install.get("default_model")}}


def status_extras(state_root=None, config_root=None, *, cache_ttl=0, disk_free=None):
    """Fields for POST status: vitals always; held when under pressure.

    cache_ttl: reuse a vitals snapshot for this many seconds (node-listen); 0 = always fresh.
    """
    state_root = Path(state_root or os.environ.get("LAB_STATE_ROOT") or HERE.parent.parent / "ledger")
    config_root = Path(config_root or os.environ.get("LAB_CONFIG_ROOT") or HERE.parent.parent)
    policy = load_resources_policy(config_root)
    vitals = None
    if cache_ttl > 0 and disk_free is None:
        key = (str(state_root.resolve()), str(config_root.resolve()))
        now = time.time()
        entry = _VITALS_CACHE.get(key)
        if entry and now - entry["at"] < cache_ttl:
            vitals = _refresh_memory_pressure(entry["vitals"])
            if _vitals_cache_may_change(vitals, policy):
                vitals = None
    if vitals is None:
        vitals = read_vitals(state_root=state_root, config_root=config_root, disk_free=disk_free)
        if cache_ttl > 0 and disk_free is None:
            _VITALS_CACHE[key] = {"vitals": vitals, "at": time.time()}
    held = resource_held(vitals, policy)
    body = {"vitals": vitals}
    if held:
        body["held"] = held
    body.update(agent_capabilities(config_root))
    body.update(status_limits(state_root))
    return body


if __name__ == "__main__":
    print(json.dumps(status_extras(), sort_keys=True))
    raise SystemExit(0)

"""Platforms that complete `cs init` after install (install.sh preflight source of truth)."""
import platform

INIT_SETUP_TARGETS = frozenset({"macos-aarch64"})

# Bundle + unpack + install headroom before the channel manifest is fetched.
INSTALL_MIN_BYTES = 2 * 1024 * 1024 * 1024

LINUX_FOLLOW_URL = "https://github.com/contextstack/contextstack#documentation"

_RELEASE_TARGETS = {
    ("Darwin", "arm64"): "macos-aarch64",
    ("Linux", "x86_64"): "linux-x86_64",
    ("Linux", "aarch64"): "linux-aarch64",
    ("Linux", "arm64"): "linux-aarch64",
}


def uname_target(system=None, machine=None):
    system = system if system is not None else platform.system()
    machine = machine if machine is not None else platform.machine()
    return _RELEASE_TARGETS.get((system, machine))


def init_setup_refusal(system=None, machine=None):
    """Return (summary, alternative) when cs init setup is unsupported, else None."""
    system = system if system is not None else platform.system()
    machine = machine if machine is not None else platform.machine()
    # Open the public Linux gate after the first-land smoke.
    if system == "Linux":
        return (
            "ContextStack's setup currently supports Apple Silicon Macs.",
            "Linux support is coming; you can follow progress at %s" % LINUX_FOLLOW_URL,
        )
    if system == "Darwin" and machine == "x86_64":
        return (
            "ContextStack's setup requires an Apple Silicon Mac (this Mac has an Intel processor).",
            "Use a Mac with Apple silicon (M1 or later), or follow %s for other options." % LINUX_FOLLOW_URL,
        )
    target = uname_target(system, machine)
    if target is None:
        return (
            "ContextStack's setup does not support %s/%s yet." % (system, machine),
            "See %s for supported platforms." % LINUX_FOLLOW_URL,
        )
    if target not in INIT_SETUP_TARGETS:
        return (
            "ContextStack's setup currently supports Apple Silicon Macs.",
            "This machine is not supported yet; follow %s for updates." % LINUX_FOLLOW_URL,
        )
    return None


def assert_init_setup_host():
    refusal = init_setup_refusal()
    if refusal is None:
        return
    summary, alternative = refusal
    raise SystemExit("cs init: %s %s" % (summary, alternative))

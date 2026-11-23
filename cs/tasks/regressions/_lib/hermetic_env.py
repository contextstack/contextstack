"""Subprocess environments for fixtures that must not locate the caller's estate."""
import os

# Keep scripts/check's estate override cleanup in sync. Fixture-specific overrides
# belong after hermetic_env(), so tests can still exercise relocated roots.
ESTATE_OVERRIDES = frozenset({
    "CS_CODE_ROOT", "CS_CONFIG_ROOT", "CS_STATE_ROOT", "CS_WORKSPACES_ROOT",
    "CS_ROOT", "CS_INSTALL_ROOT", "CS_AGENTS_DIR",
})


def hermetic_env():
    """Copy the caller's environment without root, install, or agent-directory overrides."""
    return {name: value for name, value in os.environ.items() if name not in ESTATE_OVERRIDES}

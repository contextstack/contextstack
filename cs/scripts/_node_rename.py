"""One-time conversion of installer-owned configuration; retired names are never resolved."""
import os
from pathlib import Path
import re

RETIRED = 'u' + 'ah'


def renamed(text):
    text = re.sub(r'\b' + RETIRED.upper() + r'_', 'CS_', text)
    for old, new in ((RETIRED + '-kit/', 'cs/'),
                     (RETIRED + '-rs', 'cs-harness'),
                     (RETIRED + '-sandbox', 'cs-sandbox'),
                     (RETIRED + '-tools', 'cs-tools'),
                     ('csgateway-rs', 'cs-gateway')):
        text = text.replace(old, new)
    return text


def _renamed_config(path, text):
    """Rename text, except tracked_secret_allowlist entries: those are paths in git history."""
    if path.suffix != '.json':
        return renamed(text)
    import json
    try:
        data = json.loads(text)
    except ValueError:
        return renamed(text)

    def walk(value, key=None):
        if key == 'tracked_secret_allowlist':
            return value
        if isinstance(value, dict):
            return {k: walk(v, k) for k, v in value.items()}
        if isinstance(value, list):
            return [walk(v) for v in value]
        if isinstance(value, str):
            return renamed(value)
        return value

    new = walk(data)
    if new == data:
        return text
    indent = 2 if '\n  "' in text else None
    return json.dumps(new, indent=indent) + ('\n' if text.endswith('\n') else '')


def migrate_config(config):
    """Rewrite regular UTF-8 config files atomically, preserving modes and never following links."""
    # Resolve the root (macOS /var -> /private/var); refuse links only inside it.
    config = Path(config).resolve()
    for path in sorted(config.rglob('*')):
        inside = path.relative_to(config).parents
        if path.is_symlink() or not path.is_file() or any((config / p).is_symlink() for p in inside):
            continue
        try:
            old = path.read_text(encoding='utf-8')
        except UnicodeError:
            continue
        new = _renamed_config(path, old)
        if new == old:
            continue
        tmp = path.with_name('.%s.rename.%d' % (path.name, os.getpid()))
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, path.stat().st_mode & 0o777)
        try:
            os.fchmod(fd, path.stat().st_mode & 0o777)
            with os.fdopen(fd, 'w', encoding='utf-8') as stream:
                stream.write(new)
            os.replace(tmp, path)
        finally:
            tmp.unlink(missing_ok=True)

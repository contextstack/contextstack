"""Isolated OpenCode/pi/Grok launch profiles.

opencode-go (OpenCode Go subscription, opencode v2) keeps its login in the user's own opencode database (v2 stores
provider credentials in its `credential` table; auth.json is only a legacy import). The run stages a private copy in
its isolated XDG_DATA_HOME (only OpenCode Go's credential rows; unknown schemas are refused) and points
OPENCODE_DB there; the real database path stays denied."""
import hashlib
import json
import os
import sqlite3
from datetime import datetime, timezone
from pathlib import Path


import _muse
from _cli_profile import stage, stage_git


from _cli_pins import changed_message, ensure_pin, file_sha256 as sha, load_pins, min_version_ok, package_sha256, resolve_binary


def pinned(folder, name, real_home):
    profile = json.loads((folder / 'cli-profiles.json').read_text())[name]
    binary = resolve_binary(profile['binary'], real_home)
    pkg_sha = package_sha256(profile['package']) if profile.get('package') else None
    if pkg_sha is not None:
        from _cli_pins import load_pins
        entry = (load_pins(os.environ).get(name) or {})
        if entry.get('package_sha256') and entry['package_sha256'] != pkg_sha:
            raise ValueError('%s: the installed CLI runtime package changed since it was pinned; '
                             'run `cs doctor --repin %s` after checking the new package' % (name, name))
    profile = {**profile, '_package_sha': pkg_sha}
    return profile, binary


def grok_auth(home):
    data = json.loads((home / '.grok/auth.json').read_text())
    if not isinstance(data, dict) or len(data) != 1:
        raise ValueError('Grok requires exactly one OAuth login; run grok login')
    record = next(iter(data.values()))
    if record.get('auth_mode') != 'oidc' or record.get('principal_type') != 'User':
        raise ValueError('Grok requires user OAuth; API/team credential refused')
    expiry = datetime.fromisoformat(record['expires_at'].replace('Z', '+00:00'))
    if (expiry - datetime.now(timezone.utc)).total_seconds() < 600:
        raise ValueError('Grok OAuth expired or expires within ten minutes; run grok login')
    return {'auth_mode': 'oidc', 'expires_at': record['expires_at']}


def opencode_db(real_home):
    path = real_home / '.local/share/opencode/opencode.db'
    cur = path
    while cur != real_home and cur != cur.parent:
        if cur.is_symlink():
            raise ValueError('OpenCode login store is a symlink (%s)' % cur)
        cur = cur.parent
    if not path.is_file() or path.lstat().st_uid != os.getuid():
        raise ValueError('OpenCode login store missing (%s); run opencode auth login and choose OpenCode Go' % path)
    return path


def stage_opencode_db(real_home, home):
    src = opencode_db(real_home)
    dest = home / 'data/opencode/opencode.db'
    dest.parent.mkdir(parents=True, exist_ok=True)
    if dest.exists() or dest.is_symlink():
        dest.unlink()
    dest.touch(mode=0o600, exist_ok=False)
    try:
        # Never copy the full store: it may contain credentials in unrelated tables,
        # deleted pages, WAL records, or a schema this executor does not understand.
        # SQL copies the selected rows without exposing credential values to Python.
        with sqlite3.connect(dest, uri=True) as conn:
            conn.execute("ATTACH DATABASE ? AS login", (src.as_uri() + '?mode=ro',))
            schema = conn.execute("SELECT sql FROM login.sqlite_master WHERE type='table' AND name='credential'").fetchone()
            cols = [r[1] for r in conn.execute('PRAGMA login.table_info(credential)')]
            provider = next((c for c in ('provider_id', 'provider', 'integration_id', 'id') if c in cols), None)
            if not schema or not provider:
                raise ValueError('OpenCode credential schema unsupported; private staging refused')
            # OpenCode 2.3 refuses a non-empty database without its session table, so the staged store carries
            # the full schema (no rows) and the migration history; only the opencode-go login rows are copied.
            objects = conn.execute(
                "SELECT name, sql FROM login.sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' "
                "ORDER BY CASE type WHEN 'table' THEN 0 WHEN 'index' THEN 1 ELSE 2 END").fetchall()
            for _, sql in objects:
                conn.execute(sql)
            for history in ('__drizzle_migrations', 'migration'):
                if any(name == history for name, _ in objects):
                    conn.execute('INSERT INTO main."%s" SELECT * FROM login."%s"' % (history, history))
            copied = conn.execute('INSERT INTO main.credential SELECT * FROM login.credential WHERE "%s" = ?' % provider,
                                  ('opencode-go',)).rowcount
            if not copied:
                raise ValueError('OpenCode Go login missing; private staging refused')
            conn.commit()
    except (sqlite3.Error, ValueError):
        dest.unlink(missing_ok=True)
        # SQLite errors can include values in constraints; keep the receipt generic.
        raise ValueError('OpenCode Go private login staging failed') from None
    dest.chmod(0o600)
    return dest


def command(profile, binary, ws, home, real_home, protected, args, extra=()):
    auth = profile['auth']
    record = dict(auth_mode=auth, auth_paths=[str(real_home / '.grok/auth.json'), str(home / 'auth.json')]
                  if auth == 'oauth_file' else [],
                  database_parent=str(opencode_db(real_home).parent) if auth == 'opencode_db' else None,
                  localhost_port=str(profile['network_port']) if profile.get('network_port') else None,
                  project_config_paths=[str(ws / p) for p in
                      ('.grok', '.opencode', '.pi', '.claude', '.cursor', 'opencode.json', 'opencode.jsonc')])
    return _muse.sandbox_command([str(binary), *profile.get('prefix', []), *args], ws, real_home,
                                 protected, extra=extra, adapter=record)


def prepare(c):
    name, ws, real_home = c['name'], c['ws'].resolve(), c['HOME'].resolve()
    profile, binary = pinned(c['HERE'], name, real_home)
    for key in profile.get('refuse_environment', []):
        if key in os.environ:
            raise ValueError(key + ' present; subscription launch refused')
    if profile['cost_class'] != c['cost_class'] or profile['auth'] != c['auth']:
        raise ValueError('profile auth/cost differs from executor template')
    # Managed configuration is outside the frozen task and cannot be silently merged.
    for path in (Path('/etc/grok'), Path('/Library/Application Support/opencode'),
                 Path('/Library/Managed Preferences/ai.opencode.managed.plist'),
                 Path('/Library/Managed Preferences') / real_home.name / 'ai.opencode.managed.plist'):
        if path.exists():
            raise ValueError('unversioned managed CLI configuration present: ' + str(path))
    auth_metadata = grok_auth(real_home) if name == 'grok' else {}
    overrides, protected = stage(ws, profile, "cli-home-" + c["run_id"] if c.get("lane") else "cli-home")
    home = Path(overrides['HOME'])
    if name == 'grok':
        (home / 'auth.json').symlink_to(real_home / '.grok/auth.json')
    if profile['auth'] == 'opencode_db':
        staged_db = stage_opencode_db(real_home, home)
        auth_metadata = {'store': str(staged_db), 'checked': 'isolated opencode-go rows'}
    env = {k: v for k, v in c['allowlist_env']([], ws / '.lab-tmp', ws).items()
           if k in ('PATH', 'LANG', 'LC_ALL', 'TERM', 'SHELL', 'USER', 'LOGNAME')}
    env.update(overrides,
               NO_PROXY='localhost,127.0.0.1', BASH_ENV='/dev/null', ENV='/dev/null')
    env.update(stage_git(c.get('cli_tmp') or home.parent, real_home))  # private tmp; fixtures may omit cli_tmp
    if profile['auth'] == 'opencode_db':
        env['OPENCODE_DB'] = auth_metadata['store']
    pin_entry = load_pins(os.environ).get(name)  # a changed binary is refused before it runs at all (not even --version)
    if pin_entry and pin_entry.get('sha256') != sha(binary):
        raise ValueError(changed_message(name, pin_entry.get('version'), None))
    c['on_probe']()
    probe = _muse.run_process(command(profile, binary, ws, home, real_home, protected, ['--version'], c.get('sandbox_extra', ())),
                              ws, env, min(c['timeout'], 15), memory_cap_mb=c.get('memory_cap_mb'))
    _muse.require_resources(probe)
    version = probe.stdout.strip()
    if probe.returncode:
        raise ValueError('CLI version probe failed: ' + probe.stderr)
    if not min_version_ok(profile.get('min_version'), version):
        raise ValueError('CLI version below profile minimum: ' + version)
    ensure_pin(name, binary, version, env=os.environ, package_sha=profile.get('_package_sha'))
    identity = {'provider': name, 'model': profile['model'], 'version': version,
                'binary_path': str(binary), 'binary_sha256': sha(binary),
                'package_sha256': profile.get('_package_sha'), 'home_mode': 'private_workspace',
                'auth_source': profile['auth'], 'auth_metadata': auth_metadata,
                'config_files': {str(p.relative_to(ws)): sha(p) for p in protected}}
    return binary, env, protected, identity


def execute(folder, ws, real_home, binary, env, protected, identity, prompt, timeout, on_started=None, extra=(), memory_cap_mb=None, on_evidence=None):
    profile, resolved = pinned(folder, identity['provider'], real_home)
    if resolved != binary:
        raise ValueError('CLI identity changed after capture')
    for key in profile.get('refuse_environment', []):
        if key in env:
            raise ValueError(key + ' present; subscription launch refused')
    for relative, digest in identity['config_files'].items():
        if sha(ws / relative) != digest:
            raise ValueError('staged CLI configuration changed after capture')
    home = Path(env['HOME'])
    if identity['provider'] == 'grok':
        auth = home / 'auth.json'
        if not auth.is_symlink() or auth.readlink() != real_home / '.grok/auth.json':
            raise ValueError('Grok OAuth link changed after capture')
        if grok_auth(real_home) != identity['auth_metadata']:
            raise ValueError('Grok OAuth metadata changed after capture')
    if profile['auth'] == 'opencode_db':
        opencode_db(real_home)
        staged_db = home / 'data/opencode/opencode.db'
        if env.get('OPENCODE_DB') != str(staged_db) or not staged_db.is_file() or staged_db.is_symlink():
            raise ValueError('OpenCode login store changed after capture')
    args = [part.replace('{{prompt}}', prompt) for part in profile['args']]
    return _muse.run_process(command(profile, binary, ws, home, real_home, protected, args, extra),
                             ws, env, timeout, on_started=on_started, memory_cap_mb=memory_cap_mb, on_evidence=on_evidence)

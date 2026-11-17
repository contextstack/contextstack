estate-commit operates on paths relative to the lab root whatever the git
toplevel is. With lab/ a subdirectory of the monorepo, git porcelain and
diff print toplevel-relative paths (lab/ledger/ledger.jsonl); the op must
strip its prefix, ignore every path outside lab/, resolve HEAD blobs by
the cwd-relative ./ form, and still commit facts, rules, and refuse a
truncated ledger. Files outside lab/ are never classified, tracked as
dirty, staged, or alerted. Behaviour when lab is the toplevel (prod-1) is
unchanged.

Demanded-by: GENERAL DEV monorepo import 2026-11-07; read-only writers inventory 2026-11-08

writes: ops/bin/estate-commit
external_effects: none; two isolated temporary repositories only

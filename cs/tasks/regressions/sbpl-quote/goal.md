# Regression: Seatbelt path quoting (Rust renderer)

Python profiles must embed filesystem paths as Seatbelt string literals: backslash
and double-quote escaped, UTF-8 preserved. `json.dumps` JSON-escapes non-ASCII and
Seatbelt matches the escape literally (fail open). Unescaped `%` formatting breaks
profiles when paths contain `"` or `\`.

Unit checks cover `quote()` and refuse control characters. On Darwin with
`--darwin-probe`, a deny rule on a folder whose name contains non-ASCII or `"` must
block `touch` under that folder when applied through `sandbox-exec`.

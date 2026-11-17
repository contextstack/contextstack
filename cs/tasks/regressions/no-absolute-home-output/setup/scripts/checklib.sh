#!/bin/sh
# Source from a task check: . "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)/scripts/checklib.sh"
# cwd is the workspace. False-pass class: substring Pest --filter (ambiguous numeric suffixes).

required_path() {
  test -f "$1" || { echo "check: missing $1"; exit 1; }
}

# screenshot_fields heading "Flowchart v3" badge "check-green" ...
# Exact answer.md lines. claude-code 10/10.
screenshot_fields() {
  required_path answer.md
  while [ "$#" -ge 2 ]; do
    name=$1
    want=$2
    shift 2
    grep -q "^${name}: ${want}$" answer.md || { echo "check: $name"; exit 1; }
  done
}

# route_equiv dotted.name [dir]
# route('concerns.end') and /concerns/end are the same fact (equivalent URL spellings).
route_equiv() {
  name=$1
  dir=${2:-.}
  path=/$(echo "$name" | tr . / | tr _ -)
  grep -R -E -q "route\\(['\"]${name}['\"]\\)|${path}" "$dir" \
    || { echo "check: no route('$name') or $path under $dir"; exit 1; }
}

# pest_class RecordingTest — class token, not --filter=Recording.
pest_class() {
  cls=$1
  if [ -x ./artisan ]; then
    ./artisan test --compact --filter="$cls"
  else
    php artisan test --compact --filter="$cls"
  fi
}

# Laravel 13 dropped --columns; empty output made leftover-route greps
# pass. Ban it in the calling check.
if [ -n "${0:-}" ] && [ -f "$0" ] && grep -E -q 'route:list.*--columns|--columns.*route:list' "$0" 2>/dev/null; then
  echo "check: route:list --columns is banned (Laravel 13)"
  exit 1
fi

# required_path "$var" is not a dest path (merge cannot assert it).
if [ -n "${0:-}" ] && [ -f "$0" ] && grep -E -q 'required_path[[:space:]]+["'"'"']?\$' "$0" 2>/dev/null; then
  echo "check: required_path must be a literal path, not \$var"
  exit 1
fi

# assertSee(..., false) misses &#039;. Scan workspace tests.
if [ -d tests ] && grep -R -E --include='*.php' -q 'assertSee\([^)]*,[[:space:]]*false' tests 2>/dev/null; then
  echo "check: assertSee(..., false) is banned (HTML-escape)"
  exit 1
fi

# PHPUnit 12: @test is invisible; methods must be test_*.
if [ -d tests ]; then
  if grep -R --include='*.php' -qE '(^|[[:space:]*])@test([[:space:]]|$)' tests 2>/dev/null; then
    echo "check: PHPUnit @test is banned (PHPUnit 12); use test_*()"
    exit 1
  fi
  hits=$(grep -R --include='*.php' --exclude=Pest.php --exclude=TestCase.php \
    -E 'public function [a-zA-Z_][a-zA-Z0-9_]*\(' tests 2>/dev/null \
    | grep -vE 'public function (test_|setUp|tearDown|setUpBeforeClass|tearDownAfterClass|__)' || true)
  if [ -n "$hits" ]; then
    echo "check: PHPUnit 12 methods must be test_*"
    echo "$hits" | head -5
    exit 1
  fi
fi

# Candidate outputs must be portable across estate hosts. Compare the live
# workspace with its immutable task setup so unchanged inputs remain evidence,
# while modified, staged, and generated files are scanned.
if [ -n "${HOME:-}" ] && [ "$HOME" != "/" ]; then
  task_setup=
  if [ -n "${LAB_CHECK:-}" ]; then
    task_setup=$(dirname "$LAB_CHECK")/setup
  fi
  home_hits=$(
    find . -type f \
      ! -path './.git/*' ! -path '*/vendor/*' ! -path '*/node_modules/*' \
      ! -path '*/.lab-tmp/*' ! -name '.lab-*' -print 2>/dev/null \
    | while IFS= read -r candidate; do
        relative=${candidate#./}
        if [ -n "$task_setup" ] && [ -f "$task_setup/$relative" ] \
             && cmp -s "$candidate" "$task_setup/$relative"; then
          continue
        fi
        if grep -I -F -q "$HOME/" "$candidate" 2>/dev/null; then
          printf '%s\n' "$relative"
        fi
      done \
    | head -5
  )
  if [ -n "$home_hits" ]; then
    echo "check: absolute home path in candidate output"
    printf '%s\n' "$home_hits"
    exit 1
  fi
fi

# Escaped-entity class: literal \u003c in product files (dest-unsafe);
# &#039; as a test needle (cousin).
if grep -R --include='*.php' --include='*.blade.php' --include='*.js' --include='*.vue' \
     --exclude-dir=vendor --exclude-dir=node_modules --exclude-dir=.lab-tmp --exclude-dir=.git \
     -F '\u003c' . 2>/dev/null | grep -q .; then
  echo "check: literal \\\\u003c is banned — write real HTML tags"
  exit 1
fi
if [ -d tests ] && grep -R --include='*.php' -F '&#039;' tests 2>/dev/null | grep -q .; then
  echo "check: &#039; in tests is banned — assert the apostrophe"
  exit 1
fi

# assertSee on a 500 debug page is a false-pass.
# HTTP 200 (assertOk / assertStatus(200) / assertSuccessful) first.
if [ -d tests ]; then
  hits=$(find tests -name '*.php' ! -name Pest.php ! -name TestCase.php -print \
    | while IFS= read -r f; do
        awk '
          /->(get|post|put|patch|delete)\(/ { ok=0 }
          /assert(Ok|Successful|Created|NoContent)[[:space:]]*\(/ { ok=1 }
          /assertStatus[[:space:]]*\([[:space:]]*200/ { ok=1 }
          /assert(See|SeeText|SeeHtml|SeeInOrder|DontSee|DontSeeText)[[:space:]]*\(/ {
            if (!ok) print FILENAME ":" NR
          }
        ' "$f"
      done)
  if [ -n "$hits" ]; then
    echo "check: page assertSee must assert HTTP 200 first"
    echo "$hits" | head -5
    exit 1
  fi
fi

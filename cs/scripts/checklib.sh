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
# The name is the contract: route('students.progress'),
# ->name('students.progress'), and /students/progress are the same fact
# (equivalent URL spellings). Parameter segments in the path do not matter.
route_equiv() {
  name=$1
  dir=${2:-.}
  path=/$(echo "$name" | tr . / | tr _ -)
  grep -R -E -q "route\\(['\"]${name}['\"]\\)|->name\\(['\"]${name}['\"]\\)|${path}" "$dir" \
    || { echo "check: no route('$name'), ->name('$name'), or $path under $dir"; exit 1; }
}

# pest_class RecordingTest — class token, not --filter=Recording.
# Install caches are APFS clones (cp -c -R), so Composer $baseDir and Pest
# see the workspace. No autoloader edits.
pest_class() {
  cls=$1
  mkdir -p storage/framework/sessions storage/framework/views \
    storage/framework/cache/data storage/logs bootstrap/cache
  export APP_ENV="${APP_ENV:-testing}"
  if [ -z "${APP_KEY:-}" ]; then
    export APP_KEY='base64:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA='
  fi
  if [ -x ./artisan ]; then
    ./artisan test --compact --filter="$cls"
  else
    php artisan test --compact --filter="$cls"
  fi
}

# Files the candidate diff adds or modifies, plus untracked run outputs.
# .git and inherited dest files are never in scope.
candidate_paths() {
  {
    if git rev-parse --is-inside-work-tree >/dev/null 2>&1 \
         && git rev-parse --verify HEAD >/dev/null 2>&1; then
      git diff --name-only --diff-filter=ACMR HEAD
      git diff --name-only --cached --diff-filter=ACMR
      git ls-files --others --exclude-standard
    else
      task_setup=
      if [ -n "${CS_CHECK:-}" ]; then
        task_setup=$(dirname "$CS_CHECK")/setup
      fi
      find . -type f \
        ! -path './.git' ! -path './.git/*' \
        ! -path '*/.git/*' \
        ! -path '*/vendor/*' ! -path '*/node_modules/*' \
        ! -path '*/.cs-tmp/*' ! -name '.cs-*' -print \
      | while IFS= read -r candidate; do
          relative=${candidate#./}
          if [ -n "$task_setup" ] && [ -f "$task_setup/$relative" ] \
               && cmp -s "$candidate" "$task_setup/$relative"; then
            continue
          fi
          printf '%s\n' "$relative"
        done
    fi
  } 2>/dev/null | while IFS= read -r relative; do
    case "$relative" in
      .git|.git/*|*/.git|*/.git/*) continue ;;
      vendor/*|*/vendor/*) continue ;;
      node_modules/*|*/node_modules/*) continue ;;
      .cs-*|.cs-tmp/*|*/.cs-tmp/*) continue ;;
    esac
    [ -f "$relative" ] || continue
    printf '%s\n' "$relative"
  done | sort -u
}

candidate_test_php() {
  candidate_paths | grep -E '^tests/.*\.php$' | grep -vE '/(Pest|TestCase)\.php$' || true
}

# Laravel 13 dropped --columns; empty output made leftover-route greps
# pass. Ban it in the calling check.
if [ -n "${0:-}" ] && [ -f "$0" ] && grep -E -q 'route:list.*--columns|--columns.*route:list' "$0" 2>/dev/null; then
  echo "check: route:list --columns is banned (Laravel 13)"
  echo "$0"
  exit 1
fi

# required_path "$var" is not a dest path (merge cannot assert it).
if [ -n "${0:-}" ] && [ -f "$0" ] && grep -E -q 'required_path[[:space:]]+["'"'"']?\$' "$0" 2>/dev/null; then
  echo "check: required_path must be a literal path, not \$var"
  echo "$0"
  exit 1
fi

# assertSee(..., false) misses &#039;. Candidate tests only.
assertsee_hits=$(
  candidate_test_php | while IFS= read -r f; do
    grep -E -q 'assertSee\([^)]*,[[:space:]]*false' "$f" 2>/dev/null && printf '%s\n' "$f"
  done
)
if [ -n "$assertsee_hits" ]; then
  echo "check: assertSee(..., false) is banned (HTML-escape)"
  printf '%s\n' "$assertsee_hits"
  exit 1
fi

# PHPUnit 12: @test is invisible; methods must be test_*.
at_hits=$(
  candidate_test_php | while IFS= read -r f; do
    grep -qE '(^|[[:space:]*])@test([[:space:]]|$)' "$f" 2>/dev/null && printf '%s\n' "$f"
  done
)
if [ -n "$at_hits" ]; then
  echo "check: PHPUnit @test is banned (PHPUnit 12); use test_*()"
  printf '%s\n' "$at_hits"
  exit 1
fi
method_hits=$(
  candidate_test_php | while IFS= read -r f; do
    grep -E 'public function [a-zA-Z_][a-zA-Z0-9_]*\(' "$f" 2>/dev/null \
      | grep -vE 'public function (test_|setUp|tearDown|setUpBeforeClass|tearDownAfterClass|__)' \
      | grep -q . && printf '%s\n' "$f"
  done
)
if [ -n "$method_hits" ]; then
  echo "check: PHPUnit 12 methods must be test_*"
  printf '%s\n' "$method_hits"
  exit 1
fi

# Candidate outputs must be portable across estate hosts.
if [ -n "${HOME:-}" ] && [ "$HOME" != "/" ]; then
  home_hits=$(
    candidate_paths | while IFS= read -r f; do
      grep -I -F -q "$HOME/" "$f" 2>/dev/null && printf '%s\n' "$f"
    done | head -5
  )
  if [ -n "$home_hits" ]; then
    echo "check: absolute home path in candidate output"
    printf '%s\n' "$home_hits"
    exit 1
  fi
fi

# Escaped-entity class: literal \u003c in product files (dest-unsafe);
# &#039; as a test needle (cousin).
entity_hits=$(
  candidate_paths | grep -E '\.(php|js|vue)$' | while IFS= read -r f; do
    grep -F -q '\u003c' "$f" 2>/dev/null && printf '%s\n' "$f"
  done
)
if [ -n "$entity_hits" ]; then
  echo "check: literal \\\\u003c is banned — write real HTML tags"
  printf '%s\n' "$entity_hits"
  exit 1
fi
apos_hits=$(
  candidate_test_php | while IFS= read -r f; do
    grep -F -q '&#039;' "$f" 2>/dev/null && printf '%s\n' "$f"
  done
)
if [ -n "$apos_hits" ]; then
  echo "check: &#039; in tests is banned — assert the apostrophe"
  printf '%s\n' "$apos_hits"
  exit 1
fi

# assertSee on a 500 debug page is a false-pass.
see200_hits=$(
  candidate_test_php | while IFS= read -r f; do
    awk '
      /->(get|post|put|patch|delete)\(/ { ok=0 }
      /assert(Ok|Successful|Created|NoContent)[[:space:]]*\(/ { ok=1 }
      /assertStatus[[:space:]]*\([[:space:]]*200/ { ok=1 }
      /assert(See|SeeText|SeeHtml|SeeInOrder|DontSee|DontSeeText)[[:space:]]*\(/ {
        if (!ok) print FILENAME
      }
    ' "$f"
  done | sort -u
)
if [ -n "$see200_hits" ]; then
  echo "check: page assertSee must assert HTTP 200 first"
  printf '%s\n' "$see200_hits"
  exit 1
fi

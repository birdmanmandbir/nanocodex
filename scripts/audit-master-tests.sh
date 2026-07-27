#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repository_root"

baseline="${1:-ad2952b9c8a4e6946440c7501783e814fa72a215}"
temporary_dir="$(mktemp -d 2>/dev/null)" || {
  echo "failed to create a parity-audit directory" >&2
  exit 1
}
cleanup() {
  find "$temporary_dir" -depth -delete
}
trap cleanup EXIT

mkdir "$temporary_dir/baseline"
git archive "$baseline" | tar -x -C "$temporary_dir/baseline"

extract_tests() {
  find "$1" \
    -type d \( \
      -name .git -o \
      -name .cache -o \
      -name .nanocodex -o \
      -name .venv -o \
      -name node_modules -o \
      -name target \
    \) -prune -o \
    -type f -name '*.rs' -print0 |
    xargs -0 perl -0777 -ne \
      'while (/#\[(?:[^\]]*::)?test(?:\([^\]]*\))?\]\s*(?:#\[[^\]]+\]\s*)*(?:async\s+)?fn\s+(\w+)/g) { print "$1\n" }' |
    sort -u
}

extract_tests "$temporary_dir/baseline" > "$temporary_dir/baseline-tests"
extract_tests "$repository_root" > "$temporary_dir/current-tests"
comm -23 \
  "$temporary_dir/baseline-tests" \
  "$temporary_dir/current-tests" > "$temporary_dir/missing-tests"
sort -u scripts/master-test-replacements.txt > "$temporary_dir/expected-replacements"

baseline_count="$(wc -l < "$temporary_dir/baseline-tests" | tr -d ' ')"
current_count="$(wc -l < "$temporary_dir/current-tests" | tr -d ' ')"
echo "Rust test inventory: baseline=$baseline_count current=$current_count"

if ! diff -u "$temporary_dir/expected-replacements" "$temporary_dir/missing-tests"; then
  echo "master test inventory changed without a classified replacement" >&2
  exit 1
fi

echo "Classified replacements:"
sed 's/^/  - /' "$temporary_dir/missing-tests"

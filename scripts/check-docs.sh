#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repository_root"

version="$(cargo metadata --no-deps --format-version 1 | jq -er '.packages[] | select(.name == "nanocodex") | .version')"
crates=()
while IFS= read -r crate; do
  crates+=("$crate")
done < scripts/release-crates.txt
temporary_dir="$(mktemp -d 2>/dev/null)" || {
  echo "failed to create a temporary docs.rs check directory" >&2
  exit 1
}
trap 'rm -rf -- "$temporary_dir"' EXIT
documentation_target="$repository_root/target"

for crate in "${crates[@]}"; do
  archive="target/package/${crate}-${version}.crate"
  test -f "$archive" || {
    echo "missing $archive; package the release crates first" >&2
    exit 1
  }
  tar -xzf "$archive" -C "$temporary_dir"
done

release_config="$temporary_dir/release.toml"
{
  echo '[patch.crates-io]'
  for crate in "${crates[@]}"; do
    printf '%s = { path = "%s/%s-%s" }\n' \
      "$crate" "$temporary_dir" "$crate" "$version"
  done
} > "$release_config"

for crate in "${crates[@]}"; do
  echo "Documenting packaged $crate $version..."
  RUSTDOCFLAGS="-D warnings" \
    CARGO_INCREMENTAL=0 \
    CARGO_TARGET_DIR="$documentation_target" \
    cargo doc \
      --all-features \
      --no-deps \
      --config "$release_config" \
      --manifest-path "$temporary_dir/${crate}-${version}/Cargo.toml"
done

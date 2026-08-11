#!/usr/bin/env bash
set -euo pipefail

repository=$(cd "$(dirname "$0")/.." && pwd -P)
toolchain=${NANOCODEX_VM_RUST_TOOLCHAIN:-1.97.1}
expected_rustc_commit=8bab26f4f68e0e26f0bb7960be334d5b520ea452
target=${1:-}
target_directory=${2:-$repository/target/reproducible-guest}

if [[ -z "$target" ]]; then
  case "$(uname -m)" in
    arm64|aarch64) target=aarch64-unknown-linux-musl ;;
    x86_64|amd64) target=x86_64-unknown-linux-musl ;;
    *) echo "unsupported VM guest architecture: $(uname -m)" >&2; exit 2 ;;
  esac
fi

rustc_commit=$(rustup run "$toolchain" rustc --version --verbose | sed -n 's/^commit-hash: //p')
if [[ "$rustc_commit" != "$expected_rustc_commit" ]]; then
  echo "Rust toolchain $toolchain resolved to unexpected compiler $rustc_commit" >&2
  exit 2
fi

if [[ "$target" == aarch64-unknown-linux-musl && "$(uname -s)" == Darwin ]]; then
  export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="$repository/scripts/aarch64-unknown-linux-musl-linker"
  export CC_aarch64_unknown_linux_musl="$repository/scripts/aarch64-unknown-linux-musl-linker"
  export AR_aarch64_unknown_linux_musl="$repository/scripts/aarch64-unknown-linux-musl-ar"
fi

export SOURCE_DATE_EPOCH=0
export CARGO_INCREMENTAL=0
export RUSTFLAGS="--remap-path-prefix=$repository=/usr/src/nanocodex -C link-arg=-Wl,--build-id=none"

cd "$repository"
rustup run "$toolchain" cargo build --locked \
  --profile reproducible-guest \
  --target "$target" \
  --target-dir "$target_directory" \
  -p nanocodex-vm \
  --bin nanocodex-vm-guest \
  --no-default-features \
  --features guest-runtime

printf '%s\n' "$target_directory/$target/reproducible-guest/nanocodex-vm-guest"

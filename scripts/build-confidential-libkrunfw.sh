#!/usr/bin/env bash
set -euo pipefail

source_repository=https://github.com/gakonst/libkrunfw
source_revision=5c2a7dd1d1bf62a8773e1e4dfac06b240cc5fe10
builder_image=docker.io/library/debian@sha256:ae614fe11cb373155bf26b938154c34bed87aa701f2f55a4ef03f872e4314ab0
builder_snapshot=20251201T000000Z
kernel_version=linux-6.12.91
kernel_sha256=0ff2ab9e169f9f1948557471fbb450d3018f8c5b77caf288e1a3982582597969

variant=${1:-}
output_directory=${2:-}

case "$variant" in
  sev) make_variant=SEV ;;
  tdx) make_variant=TDX ;;
  *) echo "usage: $0 <sev|tdx> <output-directory>" >&2; exit 2 ;;
esac

if [[ -z "$output_directory" ]]; then
  echo "usage: $0 <sev|tdx> <output-directory>" >&2
  exit 2
fi
if [[ "$(uname -s)" != Linux || "$(uname -m)" != x86_64 ]]; then
  echo "the pinned libkrunfw build environment must be Linux x86_64" >&2
  exit 2
fi

mkdir -p "$output_directory"
output_directory=$(cd "$output_directory" && pwd -P)
build_root=$(mktemp -d)
trap 'rm -rf -- "$build_root"' EXIT
source_directory=$build_root/libkrunfw

git init --quiet "$source_directory"
git -C "$source_directory" remote add origin "$source_repository"
git -C "$source_directory" fetch --quiet --depth=1 origin "$source_revision"
git -C "$source_directory" checkout --quiet --detach FETCH_HEAD

resolved_revision=$(git -C "$source_directory" rev-parse HEAD)
if [[ "$resolved_revision" != "$source_revision" ]]; then
  echo "libkrunfw resolved to unexpected revision $resolved_revision" >&2
  exit 2
fi
if ! git -C "$source_directory" diff --quiet --exit-code ||
   ! git -C "$source_directory" diff --cached --quiet --exit-code; then
  echo "libkrunfw source checkout is dirty before the build" >&2
  exit 2
fi

mkdir -p "$source_directory/tarballs"
kernel_tarball=$source_directory/tarballs/$kernel_version.tar.xz
curl --fail --location --retry 5 --silent --show-error \
  "https://cdn.kernel.org/pub/linux/kernel/v6.x/$kernel_version.tar.xz" \
  --output "$kernel_tarball"
printf '%s  %s\n' "$kernel_sha256" "$kernel_tarball" | sha256sum --check --status

jobs=${NANOCODEX_LIBKRUNFW_JOBS:-$(nproc)}
export KBUILD_BUILD_VERSION=1
export SOURCE_DATE_EPOCH=0
make -C "$source_directory" --jobs "$jobs" "$make_variant=1"

config=$source_directory/$kernel_version/.config
for required in CONFIG_BLK_DEV_DM CONFIG_DM_INIT CONFIG_DM_VERITY CONFIG_CRYPTO_SHA256; do
  if ! grep -qx "$required=y" "$config"; then
    echo "$variant firmware is missing required kernel option $required=y" >&2
    exit 2
  fi
done

firmware_name=libkrunfw-$variant.so.5.5.0
firmware=$source_directory/$firmware_name
kernel=$source_directory/$kernel_version/vmlinux
initrd=$source_directory/initrd/initrd.gz
qboot=$source_directory/qboot/$variant/bios.bin
source_config=$source_directory/config-libkrunfw-${variant}_x86_64

for artifact in "$firmware" "$kernel" "$initrd" "$qboot" "$source_config" "$config"; do
  if [[ ! -s "$artifact" ]]; then
    echo "expected libkrunfw build artifact is missing: $artifact" >&2
    exit 2
  fi
done

install -m 0755 "$firmware" "$output_directory/$firmware_name"
install -m 0644 "$kernel" "$output_directory/kernel-vmlinux"
install -m 0644 "$initrd" "$output_directory/initrd.gz"
install -m 0644 "$qboot" "$output_directory/qboot-bios.bin"
install -m 0644 "$source_config" "$output_directory/source-kernel.config"
install -m 0644 "$config" "$output_directory/resolved-kernel.config"
ln -sfn "$firmware_name" "$output_directory/libkrunfw-$variant.so.5"

sha256_file() {
  sha256sum "$1" | cut -d ' ' -f 1
}

size_file() {
  stat --format='%s' "$1"
}

firmware_sha256=$(sha256_file "$output_directory/$firmware_name")
kernel_binary_sha256=$(sha256_file "$output_directory/kernel-vmlinux")
initrd_sha256=$(sha256_file "$output_directory/initrd.gz")
qboot_sha256=$(sha256_file "$output_directory/qboot-bios.bin")
source_config_sha256=$(sha256_file "$output_directory/source-kernel.config")
resolved_config_sha256=$(sha256_file "$output_directory/resolved-kernel.config")

cat > "$output_directory/manifest.json" <<EOF
{"schema":"nanocodex-libkrunfw-build-v1","source_repository":"$source_repository","source_revision":"$source_revision","builder_image":"$builder_image","builder_snapshot":"$builder_snapshot","variant":"$variant","kernel_version":"$kernel_version","kernel_tarball_sha256":"$kernel_sha256","source_config_sha256":"$source_config_sha256","resolved_config_sha256":"$resolved_config_sha256","firmware":{"path":"$firmware_name","length":$(size_file "$output_directory/$firmware_name"),"sha256":"$firmware_sha256"},"kernel":{"path":"kernel-vmlinux","length":$(size_file "$output_directory/kernel-vmlinux"),"sha256":"$kernel_binary_sha256"},"initrd":{"path":"initrd.gz","length":$(size_file "$output_directory/initrd.gz"),"sha256":"$initrd_sha256"},"qboot":{"path":"qboot-bios.bin","length":$(size_file "$output_directory/qboot-bios.bin"),"sha256":"$qboot_sha256"}}
EOF

printf '%s\n' "$output_directory/manifest.json"

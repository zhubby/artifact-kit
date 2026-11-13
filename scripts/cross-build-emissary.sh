#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
mkdir -p .local/emissary-bin
# Cross targets require their Rust standard library and native linker/SDK.
# Release CI builds each of the five platforms on its corresponding native host.
targets=${EMISSARY_TARGETS:-$(rustc -vV | sed -n 's/^host: //p')}
for target in $targets; do
  case "$target" in
    x86_64-unknown-linux-gnu) os=linux; arch=amd64 ;;
    aarch64-unknown-linux-gnu) os=linux; arch=arm64 ;;
    x86_64-apple-darwin) os=darwin; arch=amd64 ;;
    aarch64-apple-darwin) os=darwin; arch=arm64 ;;
    x86_64-pc-windows-msvc) os=windows; arch=amd64 ;;
    *) echo "Unsupported Emissary release target: $target" >&2; exit 1 ;;
  esac
  extension=''
  if [ "$os" = windows ]; then extension='.exe'; fi
  cargo build --locked --release --target "$target" --bin artifactory-emissary
  cp "target/$target/release/artifactory-emissary$extension" ".local/emissary-bin/artifactory-emissary-$os-$arch$extension"
done

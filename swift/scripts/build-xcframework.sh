#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$repo_root"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "XCFramework packaging requires macOS and Xcode" >&2
  exit 1
fi

universal=0
for arg in "$@"; do
  case "$arg" in
    --universal) universal=1 ;;
    -h|--help)
      echo "Usage: $0 [--universal]"
      exit 0
      ;;
    *)
      echo "Unknown option: $arg" >&2
      exit 1
      ;;
  esac
done

host_arch="$(uname -m)"
if [[ "$host_arch" == "arm64" ]]; then
  default_target="aarch64-apple-darwin"
else
  default_target="x86_64-apple-darwin"
fi

# Match Package.swift `.macOS(.v15)` deployment target so C objects (e.g. QuickJS) link with 0 warnings.
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-15.0}"

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

build_target() {
  local target="$1"
  echo "==> Building pikra-ffi (release) for ${target}..."
  cargo build --release -p pikra-ffi --target "$target"
  local lib_path="$repo_root/target/${target}/release/libpikra_ffi.a"
  local out_path="$tmp_dir/libpikra_ffi_${target}.a"
  cp "$lib_path" "$out_path"
  strip -S "$out_path" || true
  echo "    Built ${target}: $(ls -lh "$out_path" | awk '{print $5}') ($(stat -f%z "$out_path") bytes)"
}

if [[ "$universal" -eq 1 ]]; then
  installed_targets="$(rustup target list --installed 2>/dev/null || true)"
  if echo "$installed_targets" | grep -q "aarch64-apple-darwin" && \
     echo "$installed_targets" | grep -q "x86_64-apple-darwin"; then
    build_target "aarch64-apple-darwin"
    build_target "x86_64-apple-darwin"
    xcrun lipo -create \
      "$tmp_dir/libpikra_ffi_aarch64-apple-darwin.a" \
      "$tmp_dir/libpikra_ffi_x86_64-apple-darwin.a" \
      -output "$tmp_dir/libpikra_ffi.a"
  else
    echo "Warning: both aarch64-apple-darwin and x86_64-apple-darwin targets not installed; falling back to ${default_target}" >&2
    build_target "$default_target"
    cp "$tmp_dir/libpikra_ffi_${default_target}.a" "$tmp_dir/libpikra_ffi.a"
  fi
else
  build_target "$default_target"
  cp "$tmp_dir/libpikra_ffi_${default_target}.a" "$tmp_dir/libpikra_ffi.a"
fi

mkdir -p "$tmp_dir/Headers/CPikra"
cp "$repo_root/crates/pikra-ffi/include/pikra.h" "$tmp_dir/Headers/CPikra/pikra.h"
cp "$repo_root/crates/pikra-ffi/include/module.modulemap" "$tmp_dir/Headers/CPikra/module.modulemap"

out_xcframework="$repo_root/swift/Artifacts/PikraCore.xcframework"
mkdir -p "$repo_root/swift/Artifacts"
rm -rf "$out_xcframework"

xcodebuild -create-xcframework \
  -library "$tmp_dir/libpikra_ffi.a" \
  -headers "$tmp_dir/Headers" \
  -output "$out_xcframework" >/dev/null

lib_bytes="$(stat -f%z "$tmp_dir/libpikra_ffi.a")"
lib_mb="$(awk -v b="$lib_bytes" 'BEGIN { printf "%.2f MiB", b / (1024 * 1024) }')"
echo "==> Packaged $out_xcframework"
echo "==> libpikra_ffi.a release size: ${lib_bytes} bytes (${lib_mb})"

# Report linked test binary code size if already built by SwiftPM.
test_bin="$(find "$repo_root/swift/.build" -type f \( -name "PikraKitPackageTests" -o -name "PikraKitTests-product" -o -name "PikraKitTests" \) ! -name "*.swiftmodule" ! -name "*.d" ! -name "*.o" 2>/dev/null | head -n 1 || true)"
if [[ -n "$test_bin" && -f "$test_bin" ]]; then
  bin_bytes="$(stat -f%z "$test_bin")"
  bin_mb="$(awk -v b="$bin_bytes" 'BEGIN { printf "%.2f MiB", b / (1024 * 1024) }')"
  echo "==> Linked test binary size ($test_bin): ${bin_bytes} bytes (${bin_mb})"
  size -m "$test_bin" | grep -E "Segment __TEXT|Segment __DATA|total" | head -n 4 || true
fi

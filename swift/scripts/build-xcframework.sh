#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$repo_root"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "XCFramework packaging requires macOS and Xcode" >&2
  exit 1
fi

universal=0
all_platforms=0
make_zip=0

for arg in "$@"; do
  case "$arg" in
    --universal) universal=1 ;;
    --all-platforms) all_platforms=1 ;;
    --zip) make_zip=1 ;;
    -h|--help)
      echo "Usage: $0 [--universal] [--all-platforms] [--zip]"
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

macos_deployment_target="${MACOSX_DEPLOYMENT_TARGET:-15.0}"
ios_deployment_target="${IPHONEOS_DEPLOYMENT_TARGET:-17.0}"

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

build_target() {
  local target="$1"
  echo "==> Building jimu-ffi (release) for ${target}..."
  case "$target" in
    *-apple-darwin)
      env -u IPHONEOS_DEPLOYMENT_TARGET -u SDKROOT \
        MACOSX_DEPLOYMENT_TARGET="$macos_deployment_target" \
        cargo build --release -p jimu-ffi --target "$target"
      ;;
    *-apple-ios*)
      env -u MACOSX_DEPLOYMENT_TARGET -u SDKROOT \
        IPHONEOS_DEPLOYMENT_TARGET="$ios_deployment_target" \
        cargo build --release -p jimu-ffi --target "$target"
      ;;
    *)
      cargo build --release -p jimu-ffi --target "$target"
      ;;
  esac
  local lib_path="$repo_root/target/${target}/release/libjimu_ffi.a"
  local out_path="$tmp_dir/libjimu_ffi_${target}.a"
  cp "$lib_path" "$out_path"
  strip -S "$out_path" || true
  echo "    Built ${target}: $(ls -lh "$out_path" | awk '{print $5}') ($(stat -f%z "$out_path") bytes)"
}

stage_headers() {
  local header_dir="$1"
  mkdir -p "$header_dir/CJimu"
  cp "$repo_root/crates/jimu-ffi/include/jimu.h" "$header_dir/CJimu/jimu.h"
  cp "$repo_root/crates/jimu-ffi/include/module.modulemap" "$header_dir/CJimu/module.modulemap"
}

report_slice_size() {
  local label="$1"
  local path="$2"
  local bytes
  bytes="$(stat -f%z "$path")"
  local mb
  mb="$(awk -v b="$bytes" 'BEGIN { printf "%.2f MiB", b / (1024 * 1024) }')"
  echo "==> ${label}: ${bytes} bytes (${mb})"
}

out_xcframework="$repo_root/swift/Artifacts/JimuCore.xcframework"
out_zip="$repo_root/swift/Artifacts/JimuCore.xcframework.zip"
mkdir -p "$repo_root/swift/Artifacts"
rm -rf "$out_xcframework"

if [[ "$all_platforms" -eq 1 ]]; then
  required_targets=(
    "aarch64-apple-darwin"
    "x86_64-apple-darwin"
    "aarch64-apple-ios"
    "aarch64-apple-ios-sim"
    "x86_64-apple-ios"
  )
  installed_targets="$(rustup target list --installed 2>/dev/null || true)"
  for t in "${required_targets[@]}"; do
    if ! echo "$installed_targets" | grep -qx "$t"; then
      echo "Missing required Rust target: ${t} (install with: rustup target add ${t})" >&2
      exit 1
    fi
  done

  build_target "aarch64-apple-darwin"
  build_target "x86_64-apple-darwin"
  mkdir -p "$tmp_dir/macos"
  xcrun lipo -create \
    "$tmp_dir/libjimu_ffi_aarch64-apple-darwin.a" \
    "$tmp_dir/libjimu_ffi_x86_64-apple-darwin.a" \
    -output "$tmp_dir/macos/libjimu_ffi.a"
  stage_headers "$tmp_dir/macos/Headers"

  build_target "aarch64-apple-ios"
  mkdir -p "$tmp_dir/ios"
  cp "$tmp_dir/libjimu_ffi_aarch64-apple-ios.a" "$tmp_dir/ios/libjimu_ffi.a"
  stage_headers "$tmp_dir/ios/Headers"

  build_target "aarch64-apple-ios-sim"
  build_target "x86_64-apple-ios"
  mkdir -p "$tmp_dir/ios-sim"
  xcrun lipo -create \
    "$tmp_dir/libjimu_ffi_aarch64-apple-ios-sim.a" \
    "$tmp_dir/libjimu_ffi_x86_64-apple-ios.a" \
    -output "$tmp_dir/ios-sim/libjimu_ffi.a"
  stage_headers "$tmp_dir/ios-sim/Headers"

  xcodebuild -create-xcframework \
    -library "$tmp_dir/macos/libjimu_ffi.a" -headers "$tmp_dir/macos/Headers" \
    -library "$tmp_dir/ios/libjimu_ffi.a" -headers "$tmp_dir/ios/Headers" \
    -library "$tmp_dir/ios-sim/libjimu_ffi.a" -headers "$tmp_dir/ios-sim/Headers" \
    -output "$out_xcframework" >/dev/null

  echo "==> Packaged $out_xcframework (3 slices: macOS universal, iOS arm64, iOS Simulator universal)"
  report_slice_size "macOS universal (arm64+x86_64)" "$tmp_dir/macos/libjimu_ffi.a"
  report_slice_size "iOS device (arm64)" "$tmp_dir/ios/libjimu_ffi.a"
  report_slice_size "iOS Simulator (arm64+x86_64)" "$tmp_dir/ios-sim/libjimu_ffi.a"
else
  if [[ "$universal" -eq 1 ]]; then
    installed_targets="$(rustup target list --installed 2>/dev/null || true)"
    if echo "$installed_targets" | grep -qx "aarch64-apple-darwin" && \
       echo "$installed_targets" | grep -qx "x86_64-apple-darwin"; then
      build_target "aarch64-apple-darwin"
      build_target "x86_64-apple-darwin"
      xcrun lipo -create \
        "$tmp_dir/libjimu_ffi_aarch64-apple-darwin.a" \
        "$tmp_dir/libjimu_ffi_x86_64-apple-darwin.a" \
        -output "$tmp_dir/libjimu_ffi.a"
    else
      echo "Warning: both aarch64-apple-darwin and x86_64-apple-darwin targets not installed; falling back to ${default_target}" >&2
      build_target "$default_target"
      cp "$tmp_dir/libjimu_ffi_${default_target}.a" "$tmp_dir/libjimu_ffi.a"
    fi
  else
    build_target "$default_target"
    cp "$tmp_dir/libjimu_ffi_${default_target}.a" "$tmp_dir/libjimu_ffi.a"
  fi

  stage_headers "$tmp_dir/Headers"

  xcodebuild -create-xcframework \
    -library "$tmp_dir/libjimu_ffi.a" \
    -headers "$tmp_dir/Headers" \
    -output "$out_xcframework" >/dev/null

  echo "==> Packaged $out_xcframework"
  report_slice_size "libjimu_ffi.a release size" "$tmp_dir/libjimu_ffi.a"
fi

if [[ "$make_zip" -eq 1 ]]; then
  rm -f "$out_zip"
  ditto -c -k --sequesterRsrc --keepParent "$out_xcframework" "$out_zip"
  checksum="$(swift package compute-checksum "$out_zip")"
  report_slice_size "JimuCore.xcframework.zip" "$out_zip"
  echo "==> Checksum: ${checksum}"
fi

# Report linked test binary code size if already built by SwiftPM.
test_bin="$(find "$repo_root/swift/.build" -type f \( -name "JimuKitPackageTests" -o -name "JimuKitTests-product" -o -name "JimuKitTests" \) ! -name "*.swiftmodule" ! -name "*.d" ! -name "*.o" 2>/dev/null | head -n 1 || true)"
if [[ -n "$test_bin" && -f "$test_bin" ]]; then
  bin_bytes="$(stat -f%z "$test_bin")"
  bin_mb="$(awk -v b="$bin_bytes" 'BEGIN { printf "%.2f MiB", b / (1024 * 1024) }')"
  echo "==> Linked test binary size ($test_bin): ${bin_bytes} bytes (${bin_mb})"
  size -m "$test_bin" | grep -E "Segment __TEXT|Segment __DATA|total" | head -n 4 || true
fi

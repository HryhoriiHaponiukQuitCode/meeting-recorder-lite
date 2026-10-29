#!/usr/bin/env bash
# Builds the sherpa-onnx C API as static libraries for recorder-transcriber.
#
# - The prebuilt sherpa-onnx release libraries contain espeak-ng (GPL-3.0)
#   for text-to-speech, so we build the ASR/VAD part ourselves with TTS and
#   speaker diarization switched off. CMake downloads the remaining
#   dependencies (ONNX Runtime 1.28.2 and friends) and checks their SHA256.
# - Static, not shared: the transcriber is then one self-contained binary.
#   Shared libraries in the macOS app bundle fail library validation with
#   the ad-hoc signature that local and CI builds use.
# - The sherpa-onnx-sys crate links the TTS libraries (piper_phonemize,
#   espeak-ng, ucd) by name in static mode. With TTS off nothing references
#   them, so empty placeholder archives satisfy the linker.
# - The build runs at low priority on half the CPU cores so the computer
#   stays responsive.
#
# Output: .deps/sherpa-onnx-<version>/lib (SHERPA_ONNX_LIB_DIR in
# .cargo/config.toml points there). Runs on macOS and on Windows (Git Bash
# with Visual Studio). Re-running with an existing output does nothing.
set -euo pipefail

VERSION=1.13.8
COMMIT=11afbd009a7f8c08f4bcf2fc1b265d0df4670fbf
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/.deps/sherpa-onnx-$VERSION"
# sherpa-onnx passes its exported-symbols file to the linker unquoted, so the
# source and build trees must not contain spaces (the repository folder
# "Audio Recorder" does). Only the finished libraries go to $OUT.
WORK="${SHERPA_BUILD_DIR:-${TMPDIR:-/tmp}}"
WORK="${WORK%/}/sherpa-onnx-build-$VERSION"
case "$WORK" in
  *" "*)
    echo "error: build folder '$WORK' contains a space; set SHERPA_BUILD_DIR to a folder without spaces" >&2
    exit 1 ;;
esac

if [ -f "$OUT/.complete" ]; then
  echo "sherpa-onnx $VERSION is already built in $OUT"
  exit 0
fi

cores="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo "${NUMBER_OF_PROCESSORS:-2}")"
JOBS="${SHERPA_BUILD_JOBS:-$(( cores > 2 ? cores / 2 : 1 ))}"
renice -n 10 $$ >/dev/null 2>&1 || true

rm -rf "$WORK" "$OUT"
mkdir -p "$WORK"
git clone --quiet --depth 1 --branch "v$VERSION" https://github.com/k2-fsa/sherpa-onnx.git "$WORK/src"
actual="$(git -C "$WORK/src" rev-parse HEAD)"
if [ "$actual" != "$COMMIT" ]; then
  echo "error: tag v$VERSION points to $actual, expected $COMMIT" >&2
  exit 1
fi

args=(
  -DCMAKE_BUILD_TYPE=Release
  -DBUILD_SHARED_LIBS=OFF
  -DSHERPA_ONNX_ENABLE_C_API=ON
  -DSHERPA_ONNX_ENABLE_TTS=OFF
  -DSHERPA_ONNX_ENABLE_SPEAKER_DIARIZATION=OFF
  -DSHERPA_ONNX_ENABLE_PORTAUDIO=OFF
  -DSHERPA_ONNX_ENABLE_BINARY=OFF
  -DSHERPA_ONNX_BUILD_C_API_EXAMPLES=OFF
  -DSHERPA_ONNX_ENABLE_WEBSOCKET=OFF
  -DSHERPA_ONNX_ENABLE_PYTHON=OFF
  -DSHERPA_ONNX_ENABLE_TESTS=OFF
  -DSHERPA_ONNX_USE_PRE_INSTALLED_ONNXRUNTIME_IF_AVAILABLE=OFF
  -DCMAKE_INSTALL_PREFIX="$WORK/install"
)
platform=()
case "$(uname -s)" in
  Darwin) platform=(-DCMAKE_OSX_DEPLOYMENT_TARGET=13.0 -DCMAKE_OSX_ARCHITECTURES=arm64) ;;
  MINGW* | MSYS* | CYGWIN*)
    # Static CRT (/MT) like our Rust binaries (+crt-static).
    platform=(-A x64 -DSHERPA_ONNX_USE_STATIC_CRT=ON -DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded) ;;
esac

cmake -S "$WORK/src" -B "$WORK/build" "${args[@]}" "${platform[@]}"
cmake --build "$WORK/build" --config Release --parallel "$JOBS"
cmake --install "$WORK/build" --config Release

# Placeholders for the TTS libraries, built by the same toolchain so the
# archive format matches (.a on macOS, .lib on Windows).
mkdir -p "$WORK/placeholder"
cat > "$WORK/placeholder/CMakeLists.txt" <<'EOF'
cmake_minimum_required(VERSION 3.16)
project(placeholder C)
file(WRITE ${CMAKE_BINARY_DIR}/empty.c "void meeting_recorder_placeholder_unused(void);\n")
foreach(name piper_phonemize espeak-ng ucd)
  add_library(${name} STATIC ${CMAKE_BINARY_DIR}/empty.c)
endforeach()
EOF
cmake -S "$WORK/placeholder" -B "$WORK/placeholder/build" -DCMAKE_BUILD_TYPE=Release "${platform[@]}"
cmake --build "$WORK/placeholder/build" --config Release --parallel "$JOBS"

mkdir -p "$OUT/lib"
cp -R "$WORK/install/include" "$OUT/"
for f in "$WORK"/install/lib/*.a "$WORK"/install/lib/*.lib; do
  [ -e "$f" ] && cp "$f" "$OUT/lib/"
done
find "$WORK/placeholder/build" \( -name '*.a' -o -name '*.lib' \) -exec cp {} "$OUT/lib/" \;
# ONNX Runtime ships its own license and third-party notices.
find "$WORK/build/_deps" -maxdepth 3 \( -iname 'LICENSE*' -o -iname 'ThirdPartyNotices*' \) \
  -path '*onnxruntime*' -exec cp {} "$OUT/" \; 2>/dev/null || true
ls -la "$OUT" "$OUT/lib"

if nm -g "$OUT"/lib/*sherpa-onnx-c-api* 2>/dev/null | grep -q espeak; then
  echo "error: the built library contains espeak-ng symbols" >&2
  exit 1
fi
rm -rf "$WORK"
touch "$OUT/.complete"
echo "sherpa-onnx $VERSION built in $OUT"

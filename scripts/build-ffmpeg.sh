#!/usr/bin/env bash
# Builds a small ffmpeg.exe with only what the app uses (see src-tauri/src/recorder.rs).
# Cross-compiles for Windows from Linux (Ubuntu: see the apt line in .github/workflows/release.yml).
#
#   scripts/build-ffmpeg.sh [output path]     default: src-tauri/binaries/ffmpeg-x86_64-pc-windows-msvc.exe
#
# Adding an ffmpeg feature to the app? Enable it below and in scripts/test-ffmpeg.ps1, or it'll
# work with your local ffmpeg and fail in the release build.
set -euo pipefail

FFMPEG_VERSION=n9.1
X264_COMMIT=b35605ace3ddf7c1a5d67a2eb553f034aef41d55 # stable branch
NV_HEADERS_VERSION=n12.2.72.0 # SDK 12.2: works with NVIDIA drivers 550 and newer
AMF_VERSION=v1.5.3
LIBVPL_VERSION=v2.17.0

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/src-tauri/binaries/ffmpeg-x86_64-pc-windows-msvc.exe}"
WORK="${WORK:-$ROOT/target-ffmpeg}"
PREFIX="$WORK/prefix"
HOST=x86_64-w64-mingw32
JOBS="$(nproc)"

mkdir -p "$WORK" "$PREFIX"
cd "$WORK"
export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig"
export PKG_CONFIG_LIBDIR="$PREFIX/lib/pkgconfig" # only our libraries, never the build machine's

fetch() { # fetch <url> <ref> <dir>
    [ -d "$3" ] || git -c advice.detachedHead=false clone --quiet --depth 1 --branch "$2" "$1" "$3"
}

# --- NVIDIA (h264_nvenc): headers only, the driver is loaded at runtime ---
fetch https://github.com/FFmpeg/nv-codec-headers.git "$NV_HEADERS_VERSION" nv-codec-headers
make -C nv-codec-headers PREFIX="$PREFIX" install >/dev/null

# --- AMD (h264_amf): headers only ---
if [ ! -d "$PREFIX/include/AMF" ]; then
    rm -rf amf
    git -c advice.detachedHead=false clone --quiet --depth 1 --branch "$AMF_VERSION" \
        --filter=blob:none --sparse https://github.com/GPUOpen-LibrariesAndSDKs/AMF.git amf
    git -C amf sparse-checkout set amf/public/include
    mkdir -p "$PREFIX/include"
    cp -r amf/amf/public/include "$PREFIX/include/AMF"
fi

# --- Intel (h264_qsv): the libvpl dispatcher, which finds the GPU runtime at runtime ---
if [ ! -f "$PREFIX/lib/libvpl.a" ]; then
    fetch https://github.com/intel/libvpl.git "$LIBVPL_VERSION" libvpl
    cmake -S libvpl -B libvpl/build -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_SYSTEM_NAME=Windows -DCMAKE_C_COMPILER=$HOST-gcc -DCMAKE_CXX_COMPILER=$HOST-g++ \
        -DCMAKE_RC_COMPILER=$HOST-windres -DCMAKE_INSTALL_PREFIX="$PREFIX" \
        -DBUILD_SHARED_LIBS=OFF -DBUILD_TOOLS=OFF -DBUILD_EXAMPLES=OFF -DBUILD_TESTS=OFF \
        -DINSTALL_EXAMPLES=OFF -DINSTALL_DEV=ON >/dev/null
    cmake --build libvpl/build -j "$JOBS" >/dev/null
    cmake --install libvpl/build >/dev/null
fi

# --- libx264: the CPU fallback (and the only reason this build is GPL) ---
if [ ! -f "$PREFIX/lib/libx264.a" ]; then
    [ -d x264 ] || { git init -q x264 && git -C x264 fetch -q --depth 1 \
        https://code.videolan.org/videolan/x264.git "$X264_COMMIT" && git -C x264 checkout -q FETCH_HEAD; }
    (cd x264 && ./configure --host=$HOST --cross-prefix=$HOST- --prefix="$PREFIX" \
        --enable-static --disable-cli --disable-opencl --bit-depth=8 --chroma-format=420 \
        --disable-lavf --disable-swscale --disable-ffms --disable-gpac --disable-lsmash >/dev/null &&
        make -j "$JOBS" >/dev/null && make install >/dev/null)
fi

# --- ffmpeg: everything off, then only the pieces recorder.rs uses ---
fetch https://github.com/FFmpeg/FFmpeg.git "$FFMPEG_VERSION" ffmpeg
cd ffmpeg
./configure \
    --arch=x86_64 --target-os=mingw32 --cross-prefix=$HOST- --pkg-config=pkg-config \
    --pkg-config-flags=--static --extra-cflags="-I$PREFIX/include" \
    --extra-ldflags="-L$PREFIX/lib -static" --extra-libs="-lstdc++" \
    --enable-gpl --enable-version3 --disable-debug --disable-doc --disable-network \
    --disable-everything --disable-autodetect --disable-programs --enable-ffmpeg \
    --enable-w32threads \
    `# hardware: GPU encoders + the D3D11 capture path` \
    --enable-ffnvcodec --enable-nvenc --enable-amf --enable-libvpl --enable-d3d11va --enable-libx264 \
    --enable-encoder=h264_nvenc,h264_amf,h264_qsv,libx264 \
    `# capture: ddagrab (default) and gdigrab ("Compatible"); color is the encoder probe's test input` \
    --enable-indev=gdigrab,lavfi \
    `# their frames arrive "encoded": lavfi as wrapped_avframe, gdigrab as bmp` \
    --enable-decoder=wrapped_avframe,bmp \
    --enable-filter=ddagrab,hwdownload,format,scale,color,null \
    `# recording: rolling .ts segments` \
    --enable-muxer=segment,mpegts,null \
    `# saving a clip: concat the segments, copy into .mp4` \
    --enable-protocol=file,pipe,concat --enable-demuxer=mpegts --enable-muxer=mp4 \
    --enable-decoder=h264 --enable-parser=h264 --enable-bsf=extract_extradata,h264_mp4toannexb \
    `# reading an .mp4 back: the app doesn't (the WebView plays clips), test-ffmpeg.ps1 does` \
    --enable-demuxer=mov
make -j "$JOBS" >/dev/null
$HOST-strip ffmpeg.exe

mkdir -p "$(dirname "$OUT")"
cp ffmpeg.exe "$OUT"
echo "Built $OUT ($(du -h "$OUT" | cut -f1))"

#!/usr/bin/env bash
# Builds Rewind's QEMU machine for the browser: QEMU compiled to
# WebAssembly with emscripten, from a checkout of the qemu-wasm fork,
# with qemu/wasm (the fork's WebAssembly backend speedups, from trynix)
# and qemu/patches (the rewind machine) applied.
#
#   tools/build-qemu-wasm.sh <qemu-wasm checkout> <output directory>
#
# The checkout is ktock/qemu-wasm at the commit nix/qemu.nix pins.
# Needs docker and the fork's toolchain image, built from the fork's
# Dockerfile, under the name in IMAGE. The output directory receives
# out.js, qemu-system-x86_64.wasm and qemu-system-x86_64.worker.js.
set -euo pipefail

if [ $# -ne 2 ]; then
  echo "usage: $0 <qemu-wasm checkout> <output directory>" >&2
  exit 2
fi
SRC_CHECKOUT=$1
OUT=$2
HERE=$(cd "$(dirname "$0")" && pwd)
QEMU_DIR=$HERE/../qemu

IMAGE=${REWIND_QEMU_WASM_IMAGE:-buildqemu}
CONTAINER=rewind-qemu-wasm-$$
JOBS=$(nproc)

# The fork's emscripten flags, less the terminal library and 9p, which
# the rewind machine has no use for: it has no console and no share,
# and its inputs and step log are files the page puts in and takes out
# of emscripten's file system. QEMU's own messages go to the browser's
# console through print and printErr.
EXTRA_CFLAGS="-O3 -g -Wno-error=unused-command-line-argument -matomics -mbulk-memory -DNDEBUG -DG_DISABLE_ASSERT -D_GNU_SOURCE -sASYNCIFY=1 -pthread -sPROXY_TO_PTHREAD=1 -sFORCE_FILESYSTEM -sALLOW_TABLE_GROWTH -sTOTAL_MEMORY=2300MB -sWASM_BIGINT -sMALLOC=mimalloc -sEXPORT_ES6=1 -sASYNCIFY_IMPORTS=ffi_call_js -sEXIT_RUNTIME=1"
EXTRA_LDFLAGS="-sEXPORTED_RUNTIME_METHODS=getTempRet0,setTempRet0,addFunction,removeFunction,FS,callMain -sINVOKE_RUN=0"

# A writable copy of the tree with the patches applied.
WORK=$(mktemp -d)
trap 'docker rm -f "$CONTAINER" >/dev/null 2>&1 || true; rm -rf "$WORK"' EXIT
echo "copying $SRC_CHECKOUT to $WORK/src"
rsync -a --exclude .git --exclude build "$SRC_CHECKOUT/" "$WORK/src/"
for patch in "$QEMU_DIR"/wasm/*.patch "$QEMU_DIR"/patches/*.patch; do
  echo "applying $(basename "$patch")"
  patch -d "$WORK/src" -p1 -s < "$patch"
done

docker run --rm -d --name "$CONTAINER" -v "$WORK/src:/qemu" "$IMAGE" >/dev/null
docker exec "$CONTAINER" emconfigure /qemu/configure --static --target-list=x86_64-softmmu --cpu=wasm32 --cross-prefix= \
  --without-default-features --enable-system --with-coroutine=fiber \
  --extra-cflags="$EXTRA_CFLAGS" --extra-cxxflags="$EXTRA_CFLAGS" --extra-ldflags="$EXTRA_LDFLAGS"
docker exec "$CONTAINER" emmake make -j "$JOBS" qemu-system-x86_64

# The DWARF -g emits is most of the wasm and none of it is read at run
# time; the name section stays, so a profile still names functions.
docker exec "$CONTAINER" /emsdk/upstream/bin/llvm-strip --strip-debug /build/qemu-system-x86_64.wasm

mkdir -p "$OUT"
docker cp "$CONTAINER:/build/qemu-system-x86_64" "$OUT/out.js"
docker cp "$CONTAINER:/build/qemu-system-x86_64.wasm" "$OUT/qemu-system-x86_64.wasm"
docker cp "$CONTAINER:/build/qemu-system-x86_64.worker.js" "$OUT/qemu-system-x86_64.worker.js"
ls -la "$OUT"

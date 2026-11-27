#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
LINUX_SRC="${1:-/work/linux-build/linux-7.2.15}"
OUT_DIR="${2:-/work/linux-build/arm64-out}"

mkdir -p "$OUT_DIR"

echo "=== 1. Patching Linux tree at $LINUX_SRC ==="
python3 "$REPO_ROOT/guest/linux/arm64/patch-kernel.py" "$REPO_ROOT" "$LINUX_SRC"

echo "=== 2. Configuring & building ARM64 Linux 7.2.15 Image ==="
cd "$LINUX_SRC"
export KBUILD_BUILD_USER=rewind
export KBUILD_BUILD_HOST=rewind
export KBUILD_BUILD_TIMESTAMP="1970-01-01"
if [ ! -f .config ] || ! grep -qxF "CONFIG_REWIND_GUEST=y" .config || ! grep -qxF "CONFIG_EVENT_TRACING=y" .config || grep -q "^CONFIG_WLAN=y" .config; then
  make allnoconfig
  scripts/kconfig/merge_config.sh -m .config "$REPO_ROOT/guest/linux/config-arm64"
  make olddefconfig
fi
grep -qxF "CONFIG_REWIND_GUEST=y" .config
grep -qxF "CONFIG_OF_PMEM=y" .config
grep -qxF "CONFIG_EVENT_TRACING=y" .config
make -j"$(nproc)" Image
cp arch/arm64/boot/Image "$OUT_DIR/Image"
cp System.map "$OUT_DIR/System.map"
ls -lh "$OUT_DIR/Image"

echo "=== 3. Building static aarch64 rewind-init & initrd ==="
cd "$REPO_ROOT"
cargo build -p rewind-init --release --target aarch64-unknown-linux-musl 2>/dev/null || {
  rustup target add aarch64-unknown-linux-musl
  cargo build -p rewind-init --release --target aarch64-unknown-linux-musl
}

INITRD_ROOT="$OUT_DIR/initrd-root"
rm -rf "$INITRD_ROOT"
mkdir -p "$INITRD_ROOT"/{dev,proc,sys,tmp,build,etc,bin,nix,rewind/tools/bin}
cp "$REPO_ROOT/target/aarch64-unknown-linux-musl/release/rewind-init" "$INITRD_ROOT/init"
cp /bin/busybox "$INITRD_ROOT/rewind/tools/busybox"
cp /bin/busybox "$INITRD_ROOT/bin/sh"
for applet in $(/bin/busybox --list); do
  ln -sf ../busybox "$INITRD_ROOT/rewind/tools/bin/$applet"
done
cat > "$INITRD_ROOT/etc/passwd" <<'EOF'
root:x:0:0:Nix build user:/build:/noshell
nixbld:x:1000:100:Nix build user:/build:/noshell
nobody:x:65534:65534:Nobody:/:/noshell
EOF
cat > "$INITRD_ROOT/etc/group" <<'EOF'
root:x:0:
nixbld:!:100:
nogroup:x:65534:
EOF
cat > "$INITRD_ROOT/etc/hosts" <<'EOF'
127.0.0.1 localhost
::1 localhost
EOF
chmod 1777 "$INITRD_ROOT/tmp"
find "$INITRD_ROOT" -exec touch -h -d @1 {} +
(cd "$INITRD_ROOT" && find . | LC_ALL=C sort | cpio -o -H newc --reproducible -R 0:0 --quiet) > "$OUT_DIR/initrd"
ls -lh "$OUT_DIR/Image" "$OUT_DIR/initrd"
echo "=== ARM64 GUEST BUILD COMPLETE ==="

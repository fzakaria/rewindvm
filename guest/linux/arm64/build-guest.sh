#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
LINUX_SRC="${1:-/work/linux-build/linux-7.2.15}"
OUT_DIR="${2:-/work/linux-build/arm64-out}"

mkdir -p "$OUT_DIR"

echo "=== 1. Patching Linux tree at $LINUX_SRC ==="
python3 - "$REPO_ROOT" "$LINUX_SRC" <<'PY'
import pathlib, subprocess, sys

repo = pathlib.Path(sys.argv[1])
src = pathlib.Path(sys.argv[2])

def already_patched(path: pathlib.Path, marker: str) -> bool:
    return path.exists() and marker in path.read_text()

# Apply generic arch-independent patches directly if not yet applied.
for patch_name, check_file, marker in [
    ("rewind-dirents.patch", "fs/readdir.c", "rewind_dirents"),
    ("rewind-trace.patch", "kernel/trace/trace.c", "CONFIG_REWIND_GUEST"),
]:
    if not already_patched(src / check_file, marker):
        subprocess.run(
            ["patch", "-p1", "-d", str(src), "-i", str(repo / "guest/linux" / patch_name)],
            check=True,
        )

# Apply only the non-arch/x86 hunks from the shared kernel patches.
def extract_non_x86_patch(patch_path: pathlib.Path) -> str:
    lines = patch_path.read_text().splitlines(keepends=True)
    out = []
    keep = False
    i = 0
    while i < len(lines):
        line = lines[i]
        if line.startswith("diff ") or (line.startswith("--- ") and not (i > 0 and lines[i-1].startswith("diff "))):
            # Look ahead for +++ b/...
            target = ""
            for j in range(i, min(i + 5, len(lines))):
                if lines[j].startswith("+++ b/"):
                    target = lines[j][6:].split("\t")[0].strip()
                    break
            keep = bool(target) and not target.startswith("arch/x86/")
        if keep:
            out.append(line)
        i += 1
    return "".join(out)

for patch_name, check_file, marker in [
    ("rewind-guest.patch", "drivers/char/random.c", "rewind_rng_reseed"),
    ("rewind-switch.patch", "kernel/sched/core.c", "rewind_switch"),
    ("rewind-time.patch", "kernel/sched/core.c", "rewind_switch(current, prev)"),
    ("rewind-cpus.patch", "drivers/base/cpu.c", "rewind_user_cpus"),
]:
    if not already_patched(src / check_file, marker):
        filtered = extract_non_x86_patch(repo / "guest/linux" / patch_name)
        if filtered.strip():
            subprocess.run(
                ["patch", "-p1", "-d", str(src)],
                input=filtered,
                text=True,
                check=True,
            )

# Copy ARM64 Rewind platform source & header without clobbering Linux's own hypervisor.h.
(src / "arch/arm64/include/asm/rewind.h").write_text(
    (repo / "guest/linux/arm64/hypervisor.h").read_text()
)
hyp_h = src / "arch/arm64/include/asm/hypervisor.h"
tarball = src.parent / f"{src.name}.tar.xz"
if tarball.exists() and "__ASM_ARM64_HYPERVISOR_H" in hyp_h.read_text():
    orig = subprocess.check_output(
        ["tar", "-xOf", str(tarball), f"{src.name}/arch/arm64/include/asm/hypervisor.h"],
        text=True,
    )
    hyp_h.write_text(orig)
hyp_text = hyp_h.read_text()
if "#include <asm/rewind.h>" not in hyp_text:
    idx = hyp_text.rfind("#endif")
    if idx == -1:
        hyp_h.write_text(hyp_text + "\n#include <asm/rewind.h>\n")
    else:
        hyp_h.write_text(hyp_text[:idx] + "#include <asm/rewind.h>\n\n" + hyp_text[idx:])
(src / "arch/arm64/kernel/rewind.c").write_text(
    (repo / "guest/linux/arm64/rewind.c").read_text()
)

def insert_once(rel: str, needle: str, replacement: str, marker: str):
    p = src / rel
    text = p.read_text()
    if marker in text:
        return
    if needle not in text:
        raise RuntimeError(f"needle not found in {rel}: {needle!r}")
    p.write_text(text.replace(needle, replacement, 1))

# 1. arch/arm64/Kconfig
insert_once(
    "arch/arm64/Kconfig",
    'source "arch/arm64/kvm/Kconfig"',
    '''config REWIND_GUEST
\tbool "Rewind VM guest support"
\tdepends on ARM64
\tselect TRACEPOINTS
\tselect EVENT_TRACING
\thelp
\t  Rewind VM deterministic ARM64 guest platform.

source "arch/arm64/kvm/Kconfig"''',
    "config REWIND_GUEST",
)

# 1b. arch/arm64/include/asm/fixmap.h — permanent fixmap slot for Rewind MMIO page.
insert_once(
    "arch/arm64/include/asm/fixmap.h",
    "\tFIX_EARLYCON_MEM_BASE,\n",
    "\tFIX_EARLYCON_MEM_BASE,\n\tFIX_REWIND_IO,\n",
    "FIX_REWIND_IO",
)

# 2. arch/arm64/kernel/Makefile
insert_once(
    "arch/arm64/kernel/Makefile",
    "obj-$(CONFIG_COMPAT)",
    "obj-$(CONFIG_REWIND_GUEST)\t\t+= rewind.o\n\nobj-$(CONFIG_COMPAT)",
    "CONFIG_REWIND_GUEST",
)

# 3. arch/arm64/include/asm/timex.h
insert_once(
    "arch/arm64/include/asm/timex.h",
    "#define get_cycles()\tarch_timer_read_counter()",
    """#ifdef CONFIG_REWIND_GUEST
unsigned long rewind_get_entropy(void);
#define get_cycles()\t\trewind_get_entropy()
#define random_get_entropy()\trewind_get_entropy()
#else
#define get_cycles()\tarch_timer_read_counter()
#endif""",
    "CONFIG_REWIND_GUEST",
)

# 4. arch/arm64/kernel/setup.c
insert_once(
    "arch/arm64/kernel/setup.c",
    "#include <asm/xen/hypervisor.h>",
    "#include <asm/xen/hypervisor.h>\n#include <asm/hypervisor.h>",
    "<asm/hypervisor.h>",
)
insert_once(
    "arch/arm64/kernel/setup.c",
    "\tif (acpi_disabled)\n\t\tunflatten_device_tree();",
    "\tif (acpi_disabled)\n\t\tunflatten_device_tree();\n\n#ifdef CONFIG_REWIND_GUEST\n\trewind_init_platform();\n#endif",
    "rewind_init_platform",
)

# 5. arch/arm64/kernel/irq.c
insert_once(
    "arch/arm64/kernel/irq.c",
    "#include <asm/exception.h>",
    "#include <asm/exception.h>\n#include <asm/hypervisor.h>",
    "<asm/hypervisor.h>",
)
insert_once(
    "arch/arm64/kernel/irq.c",
    "\tirqchip_init();",
    "#ifdef CONFIG_REWIND_GUEST\n\trewind_irq_init();\n#endif\n\tirqchip_init();",
    "rewind_irq_init",
)

# 6. arch/arm64/kernel/time.c
insert_once(
    "arch/arm64/kernel/time.c",
    "#include <asm/paravirt.h>",
    "#include <asm/paravirt.h>\n#include <asm/hypervisor.h>",
    "<asm/hypervisor.h>",
)
insert_once(
    "arch/arm64/kernel/time.c",
    "\tof_clk_init(NULL);",
    "#ifdef CONFIG_REWIND_GUEST\n\tif (rewind_running) {\n\t\trewind_timer_init();\n\t\treturn;\n\t}\n#endif\n\tof_clk_init(NULL);",
    "rewind_timer_init",
)

# 7. arch/arm64/kernel/idle.c
insert_once(
    "arch/arm64/kernel/idle.c",
    "#include <asm/sysreg.h>",
    "#include <asm/sysreg.h>\n#include <asm/hypervisor.h>",
    "<asm/hypervisor.h>",
)
insert_once(
    "arch/arm64/kernel/idle.c",
    "void __cpuidle arch_cpu_idle(void)\n{",
    "void __cpuidle arch_cpu_idle(void)\n{\n#ifdef CONFIG_REWIND_GUEST\n\tif (rewind_running) {\n\t\trewind_idle();\n\t\treturn;\n\t}\n#endif",
    "rewind_idle",
)

# 8. arch/arm64/kernel/traps.c
insert_once(
    "arch/arm64/kernel/traps.c",
    "#include <asm/system_misc.h>",
    "#include <asm/system_misc.h>\n#include <asm/hypervisor.h>",
    "<asm/hypervisor.h>",
)
insert_once(
    "arch/arm64/kernel/traps.c",
    "\t\tpt_regs_write_reg(regs, rt, arch_timer_read_counter());",
    """#ifdef CONFIG_REWIND_GUEST
\t\tif (rewind_running)
\t\t\tpt_regs_write_reg(regs, rt, rewind_clock_now());
\t\telse
#endif
\t\tpt_regs_write_reg(regs, rt, arch_timer_read_counter());""",
    "rewind_clock_now",
)
insert_once(
    "arch/arm64/kernel/traps.c",
    "\t\tpt_regs_write_reg(regs, rt, arch_timer_get_rate());",
    """#ifdef CONFIG_REWIND_GUEST
\t\tif (rewind_running)
\t\t\tpt_regs_write_reg(regs, rt, NSEC_PER_SEC);
\t\telse
#endif
\t\tpt_regs_write_reg(regs, rt, arch_timer_get_rate());""",
    "NSEC_PER_SEC",
)

# 9. arch/arm64/kernel/process.c
insert_once(
    "arch/arm64/kernel/process.c",
    "#include <asm/system_misc.h>",
    "#include <asm/system_misc.h>\n#include <asm/hypervisor.h>",
    "<asm/hypervisor.h>",
)
insert_once(
    "arch/arm64/kernel/process.c",
    "void machine_halt(void)\n{\n\tlocal_irq_disable();\n\tsmp_send_stop();\n\twhile (1);\n}",
    """void machine_halt(void)
{
\tlocal_irq_disable();
\tsmp_send_stop();
#ifdef CONFIG_REWIND_GUEST
\tif (rewind_running)
\t\trewind_halt();
#endif
\twhile (1);
}""",
    "rewind_halt",
)
insert_once(
    "arch/arm64/kernel/process.c",
    "void machine_power_off(void)\n{\n\tlocal_irq_disable();\n\tsmp_send_stop();\n\tdo_kernel_power_off();\n}",
    """void machine_power_off(void)
{
\tlocal_irq_disable();
\tsmp_send_stop();
#ifdef CONFIG_REWIND_GUEST
\tif (rewind_running)
\t\trewind_power_off();
#endif
\tdo_kernel_power_off();
}""",
    "rewind_power_off",
)
insert_once(
    "arch/arm64/kernel/process.c",
    "void machine_restart(char *cmd)\n{",
    """void machine_restart(char *cmd)
{
#ifdef CONFIG_REWIND_GUEST
\tif (rewind_running)
\t\trewind_restart();
#endif""",
    "rewind_restart",
)

# 10. arch/arm64/kernel/cpuinfo.c
insert_once(
    "arch/arm64/kernel/cpuinfo.c",
    "#include <asm/cpu.h>",
    "#include <asm/cpu.h>\n#include <asm/hypervisor.h>",
    "<asm/hypervisor.h>",
)
insert_once(
    "arch/arm64/kernel/cpuinfo.c",
    "static void *c_start(struct seq_file *m, loff_t *pos)\n{\n\t*pos = cpumask_next(*pos - 1, cpu_online_mask);",
    """static void *c_start(struct seq_file *m, loff_t *pos)
{
#ifdef CONFIG_REWIND_GUEST
\tif (rewind_user_cpus() > 1)
\t\treturn *pos < rewind_user_cpus() ? &per_cpu(cpu_data, 0) : NULL;
#endif
\t*pos = cpumask_next(*pos - 1, cpu_online_mask);""",
    "rewind_user_cpus",
)

# 11. arch/arm64/include/asm/irq_work.h & arch/arm64/kernel/smp.c:
# Rewind has 1 vCPU and no GIC/IPI controller, so irq_work runs via timer ticks
# (just like x86_64 CONFIG_SMP=n) instead of calling smp_cross_call().
insert_once(
    "arch/arm64/include/asm/irq_work.h",
    "static inline bool arch_irq_work_has_interrupt(void)\n{\n\treturn true;\n}",
    """static inline bool arch_irq_work_has_interrupt(void)
{
#ifdef CONFIG_REWIND_GUEST
\textern bool rewind_running;
\tif (rewind_running)
\t\treturn false;
#endif
\treturn true;
}""",
    "CONFIG_REWIND_GUEST",
)
insert_once(
    "arch/arm64/kernel/smp.c",
    "void arch_irq_work_raise(void)\n{\n\tsmp_cross_call(cpumask_of(smp_processor_id()), IPI_IRQ_WORK);\n}",
    """void arch_irq_work_raise(void)
{
#ifdef CONFIG_REWIND_GUEST
\textern bool rewind_running;
\tif (rewind_running)
\t\treturn;
#endif
\tsmp_cross_call(cpumask_of(smp_processor_id()), IPI_IRQ_WORK);
}""",
    "CONFIG_REWIND_GUEST",
)

print("All ARM64 Rewind kernel patches applied cleanly.")
PY

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

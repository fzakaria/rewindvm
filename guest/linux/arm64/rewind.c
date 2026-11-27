// SPDX-License-Identifier: GPL-2.0
/*
 * Rewind VM ARM64 guest platform (for Apple Silicon Hypervisor.framework).
 *
 * Mirrors arch/x86/kernel/cpu/rewind.c (interface version 7), replacing x86
 * port I/O with HVC #imm hypercalls, LAPIC MSI with HVF's raw virtual IRQ
 * (acknowledged via HVC_IRQ_ACK), CPUID detection with Device Tree
 * ("rewind,arm64-vm"), and CR4.TSD/RDTSC trapping with CNTKCTL_EL1/CNTVCT_EL0.
 */

#include <linux/binfmts.h>
#include <linux/clockchips.h>
#include <linux/clocksource.h>
#include <linux/sched_clock.h>
#include <linux/console.h>
#include <linux/cpumask.h>
#include <linux/delay.h>
#include <linux/fcntl.h>
#include <linux/file.h>
#include <linux/fs.h>
#include <linux/fs_struct.h>
#include <linux/hardirq.h>
#include <linux/init.h>
#include <linux/irq.h>
#include <linux/kfifo.h>
#include <linux/kstrtox.h>
#include <linux/miscdevice.h>
#include <linux/mm.h>
#include <linux/mutex.h>
#include <linux/namei.h>
#include <linux/of.h>
#include <linux/of_fdt.h>
#include <linux/pm.h>
#include <linux/poll.h>
#include <linux/random.h>
#include <linux/reboot.h>
#include <linux/sched.h>
#include <linux/sched/clock.h>
#include <linux/sched/mm.h>
#include <linux/sched/signal.h>
#include <linux/sched/task_stack.h>
#include <linux/serial_core.h>
#include <linux/slab.h>
#include <linux/string.h>
#include <linux/task_work.h>
#include <linux/timekeeping.h>
#include <linux/tty.h>
#include <linux/tty_driver.h>
#include <linux/uaccess.h>
#include <linux/umh.h>
#include <linux/wait.h>
#include <linux/workqueue.h>

#include <trace/events/sched.h>
#include <trace/events/signal.h>
#include <trace/events/syscalls.h>

#include <asm/barrier.h>
#include <asm/fixmap.h>
#include <asm/hypervisor.h>
#include <asm/io.h>
#include <asm/irq.h>
#include <asm/irq_regs.h>
#include <asm/memory.h>
#include <asm/processor.h>
#include <asm/syscall.h>
#include <asm/sysreg.h>
#include <asm/unistd.h>

/*
 * Paravirtual MMIO register page at 0x0900_0000 (shared by macOS HVF and
 * Linux ARM64 KVM). Mapped once into permanent fixmap slot FIX_REWIND_IO.
 */
#define REWIND_MMIO_PHYS	0x09000000ULL

static void __iomem *rewind_mmio;

static inline void __iomem *rewind_io_base(void)
{
	if (unlikely(!rewind_mmio)) {
		set_fixmap_io(FIX_REWIND_IO, REWIND_MMIO_PHYS);
		rewind_mmio = (void __iomem *)__fix_to_virt(FIX_REWIND_IO);
	}
	return rewind_mmio;
}

/* 8-byte aligned paravirtual MMIO register offsets matching arm64/layout.rs. */
#define REWIND_MMIO_EMIT	0x00
#define REWIND_MMIO_CLOCK	0x08
#define REWIND_MMIO_TIMER	0x10
#define REWIND_MMIO_IDLE	0x18
#define REWIND_MMIO_EXIT	0x20
#define REWIND_MMIO_SETUP	0x28
#define REWIND_MMIO_IRQ_ACK	0x30
#define REWIND_MMIO_SERIAL	0x38

static inline void rewind_hvc_emit(u64 pa)
{
	writeq_relaxed(pa, rewind_io_base() + REWIND_MMIO_EMIT);
	isb();
}

static inline u64 rewind_hvc_clock(void)
{
	u64 now = readq_relaxed(rewind_io_base() + REWIND_MMIO_CLOCK);

	isb();
	return now;
}

static inline void rewind_hvc_timer(u64 delta_ns)
{
	writeq_relaxed(delta_ns, rewind_io_base() + REWIND_MMIO_TIMER);
	isb();
}

static inline void rewind_hvc_idle(void)
{
	writeq_relaxed(0, rewind_io_base() + REWIND_MMIO_IDLE);
	isb();
}

static inline void rewind_hvc_exit(u64 code)
{
	writeq_relaxed(code, rewind_io_base() + REWIND_MMIO_EXIT);
}

static inline void rewind_hvc_setup(u64 pa)
{
	writeq_relaxed(pa, rewind_io_base() + REWIND_MMIO_SETUP);
}

static inline void rewind_hvc_irq_ack(void)
{
	writeq_relaxed(0, rewind_io_base() + REWIND_MMIO_IRQ_ACK);
}

static inline void rewind_hvc_serial(u8 ch)
{
	writeq_relaxed(ch, rewind_io_base() + REWIND_MMIO_SERIAL);
}

#define REWIND_INTERFACE	7

enum rewind_exit {
	REWIND_EXIT_POWER_OFF = 0,
	REWIND_EXIT_RESTART = 1,
	REWIND_EXIT_HALT = 2,
};

enum rewind_kind {
	REWIND_EV_CONSOLE = 1,
	REWIND_EV_OUTPUT = 2,
	REWIND_EV_EXEC = 3,
	REWIND_EV_FORK = 4,
	REWIND_EV_EXIT = 5,
	REWIND_EV_SIGNAL = 6,
	REWIND_EV_OPEN = 7,
	REWIND_EV_UNLINK = 8,
	REWIND_EV_RENAME = 9,
	REWIND_EV_MARK = 10,
	REWIND_EV_READ_OPEN = 11,
	REWIND_EV_CLOSE = 12,
	REWIND_EV_IO = 13,
	REWIND_EV_SWITCH = 14,
};

#define REWIND_FLAG_THREAD		BIT(0)
#define REWIND_FLAG_NO_MEMORY		BIT(1)
#define REWIND_FLAG_PREV_RUNNABLE	BIT(2)

struct rewind_record {
	u32 len;
	u16 kind;
	u16 flags;
	u32 pid;
	u32 tid;
	u32 aux;
	u8 data[];
} __packed;

#define REWIND_RECORD_MAX	8192
#define REWIND_PAYLOAD_MAX	(REWIND_RECORD_MAX - sizeof(struct rewind_record))

#define REWIND_REQUEST_MAX	2048
#define REWIND_INPUT_MAX	1024

struct rewind_tasks {
	u64 init_task;
	u64 current_task;
	u64 page_offset;
	u64 tasks;
	u64 thread_node;
	u64 signal;
	u64 pid;
	u64 tgid;
	u64 stack;
	u64 mm;
	u64 comm;
	u64 thread_head;
	u64 pgd;
	u64 pt_regs;
};

struct rewind_shared {
	u64 now;
	u64 epoch;
	u32 pending;
	u32 request_len;
	u8 request[REWIND_REQUEST_MAX];
	u32 input_len;
	u8 input[REWIND_INPUT_MAX];
	u32 stall_ns;
	struct rewind_tasks tasks;
	u32 interface;
	u64 fsbase;
	u64 gsbase;
	u8 reseed[32];
	u64 io_buf;
	u32 io_size;
	u32 io_head;
	u32 io_lost;
};

#define REWIND_PENDING_TIMER	BIT(0)
#define REWIND_PENDING_PREEMPT	BIT(1)
#define REWIND_PENDING_INSPECT	BIT(2)
#define REWIND_PENDING_INPUT	BIT(3)
#define REWIND_PENDING_STALL	BIT(4)
#define REWIND_PENDING_RESEED	BIT(5)

static DEFINE_KFIFO(rewind_input, u8, 4096);
static DECLARE_WAIT_QUEUE_HEAD(rewind_input_wait);

static struct rewind_shared rewind_shared __aligned(PAGE_SIZE);
static u8 rewind_buf[REWIND_RECORD_MAX] __aligned(PAGE_SIZE);

#define REWIND_IO_BUF_SIZE	(1024 * 1024)

enum rewind_io_kind {
	REWIND_IO_WRITE = 1,
	REWIND_IO_READ_OPEN = 2,
	REWIND_IO_WRITE_OPEN = 3,
	REWIND_IO_WAKE = 5,
};

enum rewind_wake_how {
	REWIND_WAKE_TASK = 0,
	REWIND_WAKE_HARDIRQ = 1,
	REWIND_WAKE_SOFTIRQ = 2,
	REWIND_WAKE_PLATFORM = 3,
};

struct rewind_io_record {
	u16 len;
	u8 kind;
	u8 pad;
	u32 tid;
	u64 ino;
	u32 dev;
	u8 data[];
} __packed;

static u8 rewind_io_buf[REWIND_IO_BUF_SIZE] __aligned(PAGE_SIZE);

static void rewind_io_append_raw(u8 kind, u32 tid, u64 ino, u32 dev,
				 const void *data, size_t len)
{
	struct rewind_io_record *rec;
	unsigned long irqflags;
	u32 head, size;

	len = min_t(size_t, len, PATH_MAX);
	size = sizeof(*rec) + len;
	local_irq_save(irqflags);
	head = READ_ONCE(rewind_shared.io_head);
	if (head + size > REWIND_IO_BUF_SIZE) {
		WRITE_ONCE(rewind_shared.io_lost,
			   READ_ONCE(rewind_shared.io_lost) + 1);
		local_irq_restore(irqflags);
		return;
	}
	rec = (struct rewind_io_record *)(rewind_io_buf + head);
	rec->len = size;
	rec->kind = kind;
	rec->pad = 0;
	rec->tid = tid;
	rec->ino = ino;
	rec->dev = dev;
	memcpy(rec->data, data, len);
	WRITE_ONCE(rewind_shared.io_head, head + size);
	local_irq_restore(irqflags);
}

static void rewind_io_append(u8 kind, struct file *file, const void *data,
			     size_t len)
{
	struct inode *inode = file_inode(file);

	rewind_io_append_raw(kind, current->pid, inode->i_ino,
			     inode->i_sb->s_dev, data, len);
}

static void rewind_emit(u16 kind, u16 flags, u32 pid, u32 tid, u32 aux,
			const void *a, size_t alen, const void *b, size_t blen)
{
	struct rewind_record *rec = (struct rewind_record *)rewind_buf;
	unsigned long irqflags;

	alen = min(alen, REWIND_PAYLOAD_MAX);
	blen = min(blen, REWIND_PAYLOAD_MAX - alen);

	local_irq_save(irqflags);
	rec->len = sizeof(*rec) + alen + blen;
	rec->kind = kind;
	rec->flags = flags;
	rec->pid = pid;
	rec->tid = tid;
	rec->aux = aux;
	if (alen)
		memcpy(rec->data, a, alen);
	if (blen)
		memcpy(rec->data + alen, b, blen);
	rewind_hvc_emit(__pa_symbol(rewind_buf));
	local_irq_restore(irqflags);
	/*
	 * On ARM64, MSR DAIF is not context-synchronizing; force any pending
	 * virtual IRQ asserted during this exit to be taken at this exact
	 * instruction so kernel stack frames stay 100% deterministic across runs.
	 */
	isb();
}

bool rewind_running __ro_after_init;

/* Deterministic cycle/entropy source (splitmix64), never reads CNTVCT_EL0. */
noinstr unsigned long rewind_get_entropy(void)
{
	static u64 state;
	u64 z;

	z = (state += 0x9e3779b97f4a7c15ULL);
	z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ULL;
	z = (z ^ (z >> 27)) * 0x94d049bb133111ebULL;
	return z ^ (z >> 31);
}

noinstr u64 rewind_sched_clock(void)
{
	return READ_ONCE(rewind_shared.now);
}

void rewind_yield(void)
{
	if (rewind_running)
		rewind_hvc_clock();
}

u64 rewind_clock_now(void)
{
	rewind_hvc_clock();
	barrier();
	return READ_ONCE(rewind_shared.now);
}

static u64 rewind_clock_read(struct clocksource *cs)
{
	return rewind_clock_now();
}

static struct clocksource rewind_clocksource = {
	.name	= "rewind",
	.rating	= 400,
	.read	= rewind_clock_read,
	.mask	= CLOCKSOURCE_MASK(64),
	.mult	= 1,
	.shift	= 0,
	.flags	= CLOCK_SOURCE_IS_CONTINUOUS,
};

static int rewind_set_next_event(unsigned long delta,
				 struct clock_event_device *evt)
{
	rewind_hvc_timer(delta);
	return 0;
}

static int rewind_timer_shutdown(struct clock_event_device *evt)
{
	rewind_hvc_timer(0);
	return 0;
}

static struct clock_event_device rewind_clockevent = {
	.name			= "rewind",
	.features		= CLOCK_EVT_FEAT_ONESHOT,
	.rating			= 400,
	.set_next_event		= rewind_set_next_event,
	.set_state_shutdown	= rewind_timer_shutdown,
	.set_state_oneshot_stopped = rewind_timer_shutdown,
};

void __init rewind_timer_init(void)
{
	__clocksource_register(&rewind_clocksource);
	sched_clock_register(rewind_sched_clock, 64, NSEC_PER_SEC);

	rewind_clockevent.cpumask = cpumask_of(0);
	clockevents_config_and_register(&rewind_clockevent, NSEC_PER_SEC,
					NSEC_PER_USEC, 0x7fffffff);
	lpj_fine = NSEC_PER_SEC / HZ;
}

void read_persistent_clock64(struct timespec64 *ts)
{
	if (rewind_running) {
		ts->tv_sec = READ_ONCE(rewind_shared.epoch);
		ts->tv_nsec = 0;
	} else {
		ts->tv_sec = 0;
		ts->tv_nsec = 0;
	}
}

/* Inspection support. */
#define REWIND_INSPECT_ARGS	8

static bool rewind_inspecting;
static DECLARE_WAIT_QUEUE_HEAD(rewind_inspect_done);
static pid_t rewind_inspect_running;

static void rewind_stop_user_tasks(void)
{
	struct task_struct *p, *t;

	rcu_read_lock();
	for_each_process_thread(p, t) {
		if ((t->flags & PF_KTHREAD) || is_global_init(p))
			continue;
		send_sig(SIGSTOP, t, 1);
	}
	rcu_read_unlock();
}

static void rewind_inspect(struct work_struct *work)
{
	static char request[REWIND_REQUEST_MAX + 1];
	static char *argv[REWIND_INSPECT_ARGS + 3];
	static char running[sizeof("REWIND_RUNNING=-2147483648")];
	static char *envp[] = { "PATH=/bin", running, NULL };
	u32 len = min_t(u32, READ_ONCE(rewind_shared.request_len),
			REWIND_REQUEST_MAX);
	int argc = 0;
	char *arg;

	memcpy(request, rewind_shared.request, len);
	request[len] = '\0';

	argv[argc++] = "/init";
	argv[argc++] = "--inspect";
	for (arg = request; arg < request + len && argc < ARRAY_SIZE(argv) - 1;
	     arg += strlen(arg) + 1)
		argv[argc++] = arg;
	argv[argc] = NULL;
	snprintf(running, sizeof(running), "REWIND_RUNNING=%d",
		 READ_ONCE(rewind_inspect_running));

	call_usermodehelper(argv[0], argv, envp, UMH_WAIT_PROC);

	WRITE_ONCE(rewind_inspecting, false);
	wake_up_all(&rewind_inspect_done);
}

static int rewind_reboot_notify(struct notifier_block *nb,
				unsigned long action, void *data)
{
	wait_event(rewind_inspect_done, !READ_ONCE(rewind_inspecting));
	return NOTIFY_DONE;
}

static struct notifier_block rewind_reboot_nb = {
	.notifier_call = rewind_reboot_notify,
};

static DECLARE_WORK(rewind_inspect_work, rewind_inspect);

struct rewind_stall {
	struct callback_head work;
	u32 ns;
};

static void rewind_stall_now(struct callback_head *head)
{
	struct rewind_stall *stall = container_of(head, struct rewind_stall, work);
	unsigned long us = max(stall->ns / NSEC_PER_USEC, 1U);

	kfree(stall);
	usleep_range(us, us);
}

static void rewind_stall_current(void)
{
	struct rewind_stall *stall;

	if (!current->mm || (current->flags & PF_KTHREAD))
		return;
	stall = kmalloc(sizeof(*stall), GFP_ATOMIC);
	if (!stall)
		return;
	stall->ns = READ_ONCE(rewind_shared.stall_ns);
	init_task_work(&stall->work, rewind_stall_now);
	if (task_work_add(current, &stall->work, TWA_RESUME))
		kfree(stall);
}

static void rewind_take_input(void)
{
	u32 len = min_t(u32, READ_ONCE(rewind_shared.input_len),
			REWIND_INPUT_MAX);

	kfifo_in(&rewind_input, rewind_shared.input, len);
	WRITE_ONCE(rewind_shared.input_len, 0);
	wake_up_interruptible(&rewind_input_wait);
}

static void rewind_handle_irq(struct pt_regs *regs)
{
	u32 pending;

	irq_enter();
	rewind_hvc_irq_ack();
	pending = xchg(&rewind_shared.pending, 0);

	if ((pending & REWIND_PENDING_TIMER) && rewind_clockevent.event_handler)
		rewind_clockevent.event_handler(&rewind_clockevent);

	if (pending & REWIND_PENDING_PREEMPT) {
		set_tsk_need_resched(current);
		set_preempt_need_resched();
	}

	if (pending & REWIND_PENDING_INPUT)
		rewind_take_input();

	if (pending & REWIND_PENDING_STALL)
		rewind_stall_current();

	if (pending & REWIND_PENDING_RESEED)
		rewind_rng_reseed(rewind_shared.reseed,
				  sizeof(rewind_shared.reseed));

	if (pending & REWIND_PENDING_INSPECT) {
		WRITE_ONCE(rewind_inspecting, true);
		WRITE_ONCE(rewind_inspect_running,
			   (current->flags & PF_KTHREAD) || !current->mm ?
			   0 : task_tgid_nr(current));
		rewind_stop_user_tasks();
		queue_work(system_long_wq, &rewind_inspect_work);
		set_tsk_need_resched(current);
		set_preempt_need_resched();
	}

	irq_exit();
}

void __init rewind_irq_init(void)
{
	if (rewind_running)
		set_handle_irq(rewind_handle_irq);
}

void __cpuidle rewind_idle(void)
{
	raw_local_irq_enable();
	isb();
	if (!need_resched())
		rewind_hvc_idle();
	raw_local_irq_disable();
}

void rewind_power_off(void)
{
	rewind_hvc_exit(REWIND_EXIT_POWER_OFF);
	for (;;)
		cpu_relax();
}

void rewind_restart(void)
{
	rewind_hvc_exit(REWIND_EXIT_RESTART);
	for (;;)
		cpu_relax();
}

void rewind_halt(void)
{
	rewind_hvc_exit(REWIND_EXIT_HALT);
	for (;;)
		cpu_relax();
}

static void rewind_console_write(struct console *con, const char *s,
				 unsigned int n)
{
	rewind_emit(REWIND_EV_CONSOLE, 0, 0, 0, 0, s, n, NULL, 0);
}

static struct console rewind_console = {
	.name	= "rewind",
	.write	= rewind_console_write,
	.flags	= CON_PRINTBUFFER | CON_ANYTIME,
	.index	= -1,
};

static void rewind_earlycon_write(struct console *con, const char *s,
				  unsigned int n)
{
	unsigned int i;

	for (i = 0; i < n; i++)
		rewind_hvc_serial(s[i]);
}

static int __init rewind_earlycon_setup(struct earlycon_device *dev,
					const char *opt)
{
	dev->con->write = rewind_earlycon_write;
	return 0;
}
EARLYCON_DECLARE(rewind, rewind_earlycon_setup);

/* Fake CPU count (rewind.cpus=N). */
static unsigned int rewind_cpus __ro_after_init = 1;

static int __init rewind_parse_cpus(char *arg)
{
	unsigned int cpus;

	if (kstrtouint(arg, 10, &cpus) || cpus < 1 || cpus > BITS_PER_LONG) {
		pr_warn("rewind: ignoring rewind.cpus=%s\n", arg);
		return 1;
	}
	rewind_cpus = cpus;
	return 1;
}
__setup("rewind.cpus=", rewind_parse_cpus);

unsigned int rewind_user_cpus(void)
{
	return rewind_cpus;
}

void rewind_remember_cpus(struct task_struct *p, const struct cpumask *mask)
{
	if (rewind_cpus == 1)
		return;
	WRITE_ONCE(p->rewind_pinned,
		   cpumask_bits(mask)[0] & GENMASK(rewind_cpus - 1, 0));
}

void rewind_report_cpus(const struct task_struct *p, struct cpumask *mask)
{
	unsigned long pinned;

	if (rewind_cpus == 1)
		return;
	pinned = READ_ONCE(p->rewind_pinned);
	cpumask_bits(mask)[0] = pinned ? pinned : GENMASK(rewind_cpus - 1, 0);
}

const struct cpumask *rewind_fold_cpus(const struct cpumask *mask)
{
	if (rewind_cpus == 1)
		return mask;
	if (!(cpumask_bits(mask)[0] & GENMASK(rewind_cpus - 1, 0)))
		return mask;
	return cpumask_of(0);
}

static void __init rewind_publish_tasks(void)
{
	struct rewind_tasks *t = &rewind_shared.tasks;

	t->init_task = (u64)&init_task;
	t->current_task = 0;
	t->page_offset = PAGE_OFFSET;
	t->tasks = offsetof(struct task_struct, tasks);
	t->thread_node = offsetof(struct task_struct, thread_node);
	t->signal = offsetof(struct task_struct, signal);
	t->pid = offsetof(struct task_struct, pid);
	t->tgid = offsetof(struct task_struct, tgid);
	t->stack = offsetof(struct task_struct, stack);
	t->mm = offsetof(struct task_struct, mm);
	t->comm = offsetof(struct task_struct, comm);
	t->thread_head = offsetof(struct signal_struct, thread_head);
	t->pgd = offsetof(struct mm_struct, pgd);
	t->pt_regs = THREAD_SIZE - sizeof(struct pt_regs);
	rewind_shared.fsbase = offsetof(struct task_struct, thread.uw.tp_value);
	rewind_shared.gsbase = 0;
}

void __init rewind_init_platform(void)
{
	if (!of_machine_is_compatible("rewind,arm64-vm"))
		return;

	rewind_running = true;
	rewind_publish_tasks();
	rewind_shared.interface = REWIND_INTERFACE;
	rewind_shared.io_buf = __pa_symbol(rewind_io_buf);
	rewind_shared.io_size = REWIND_IO_BUF_SIZE;
	rewind_hvc_setup(__pa_symbol(&rewind_shared));

	pm_power_off = rewind_power_off;
	register_console(&rewind_console);
}

/* Tracing and event hooks (identical wire format to x86). */

static void rewind_probe_exec(void *data, struct task_struct *p,
			      pid_t old_pid, struct linux_binprm *bprm)
{
	static char args[1024];
	struct mm_struct *mm = p->mm;
	size_t len = 0;

	if (mm && mm->arg_end > mm->arg_start) {
		len = min_t(size_t, mm->arg_end - mm->arg_start, sizeof(args));
		if (copy_from_user_nofault(args, (void __user *)mm->arg_start,
					   len))
			len = 0;
	}

	rewind_emit(REWIND_EV_EXEC, 0, p->tgid, p->pid, old_pid,
		    bprm->filename, strlen(bprm->filename) + 1, args, len);
}

static void rewind_probe_fork(void *data, struct task_struct *parent,
			      struct task_struct *child)
{
	u16 flags = child->tgid == parent->tgid ? REWIND_FLAG_THREAD : 0;

	rewind_emit(REWIND_EV_FORK, flags, parent->tgid, parent->pid,
		    child->pid, NULL, 0, NULL, 0);
}

void rewind_switch(struct task_struct *next, struct task_struct *prev)
{
	u16 flags = next->mm ? 0 : REWIND_FLAG_NO_MEMORY;
	s32 syscall = -1;

	if (!rewind_running)
		return;

	if (prev->on_rq && !prev->se.sched_delayed)
		flags |= REWIND_FLAG_PREV_RUNNABLE;
	else if (prev->mm)
		syscall = task_pt_regs(prev)->syscallno;
	rewind_emit(REWIND_EV_SWITCH, flags, next->tgid, next->pid, prev->pid,
		    &syscall, sizeof(syscall),
		    next->comm, strnlen(next->comm, TASK_COMM_LEN));
}

static bool rewind_releasing;

static void rewind_unlock(struct mutex *lock)
{
	WRITE_ONCE(rewind_releasing, true);
	mutex_unlock(lock);
	WRITE_ONCE(rewind_releasing, false);
}

static void rewind_probe_waking(void *data, struct task_struct *p)
{
	struct {
		u32 waker;
		u8 how;
	} __packed w;

	if (!rewind_running)
		return;
	if (READ_ONCE(rewind_releasing)) {
		w.waker = current->pid;
		w.how = REWIND_WAKE_PLATFORM;
	} else if (in_hardirq()) {
		w.waker = 0;
		w.how = REWIND_WAKE_HARDIRQ;
	} else if (in_serving_softirq()) {
		w.waker = 0;
		w.how = REWIND_WAKE_SOFTIRQ;
	} else {
		w.waker = current->pid;
		w.how = REWIND_WAKE_TASK;
	}
	rewind_io_append_raw(REWIND_IO_WAKE, p->pid, 0, 0, &w, sizeof(w));
}

static void rewind_probe_exit(void *data, struct task_struct *p,
			      bool group_dead)
{
	u16 flags = group_dead ? 0 : REWIND_FLAG_THREAD;
	const char *comm = group_dead ? p->group_leader->comm : p->comm;

	rewind_emit(REWIND_EV_EXIT, flags, p->tgid, p->pid, p->exit_code,
		    comm, strnlen(comm, TASK_COMM_LEN), NULL, 0);
}

static void rewind_probe_signal(void *data, int sig,
				struct kernel_siginfo *info,
				struct k_sigaction *ka)
{
	struct {
		s32 code;
		u64 addr;
	} __packed detail = {};

	if (sig == SIGKILL && (current->signal->flags & SIGNAL_GROUP_EXIT)) {
		int cause = current->signal->group_exit_code & 0x7f;

		if (!cause || cause == SIGKILL || sig_kernel_coredump(cause) ||
		    current->pid != current->tgid)
			return;
		rewind_emit(REWIND_EV_SIGNAL, 0, current->tgid, current->pid,
			    cause, &detail, sizeof(detail), NULL, 0);
		return;
	}

	if (info) {
		detail.code = info->si_code;
		if (sig == SIGSEGV || sig == SIGBUS || sig == SIGILL ||
		    sig == SIGFPE)
			detail.addr = (u64)info->si_addr;
	}

	rewind_emit(REWIND_EV_SIGNAL, 0, current->tgid, current->pid, sig,
		    &detail, sizeof(detail), NULL, 0);
}

static DEFINE_MUTEX(rewind_path_lock);
static char rewind_path_a[PATH_MAX], rewind_path_b[PATH_MAX];
static char rewind_path_arg[PATH_MAX], rewind_path_dir[PATH_MAX];

static void rewind_strip_deleted(char *path)
{
	static const char deleted[] = " (deleted)";
	size_t len = strlen(path), tail = sizeof(deleted) - 1;

	if (len > tail && !strcmp(path + len - tail, deleted))
		path[len - tail] = '\0';
}

static bool rewind_user_path(char *dst, int dfd, unsigned long src)
{
	char *rel = rewind_path_arg, *dirs = rewind_path_dir;
	char *last, *at;
	struct path base, dir;
	struct file *file;
	size_t len;
	long n;

	n = strncpy_from_user(rel, (const char __user *)src, PATH_MAX);
	if (n <= 0 || n >= PATH_MAX)
		return false;

	while (n > 1 && rel[n - 1] == '/')
		rel[--n] = '\0';
	last = strrchr(rel, '/');
	if (last)
		*last++ = '\0';
	else
		last = rel;
	if (!*last)
		return false;

	if (rel[0] == '/' || last == rel + 1) {
		strscpy(dirs, last == rel + 1 ? "/" : rel, PATH_MAX);
	} else {
		if (dfd == AT_FDCWD) {
			get_fs_pwd(current->fs, &base);
		} else {
			file = fget_raw(dfd);
			if (!file)
				return false;
			base = file->f_path;
			path_get(&base);
			fput(file);
		}
		at = d_path(&base, dirs, PATH_MAX);
		path_put(&base);
		if (IS_ERR(at))
			return false;
		len = strlen(at);
		memmove(dirs, at, len + 1);
		if (last != rel) {
			if (len + 1 + strlen(rel) >= PATH_MAX)
				return false;
			dirs[len++] = '/';
			strscpy(dirs + len, rel, PATH_MAX - len);
		}
	}

	if (kern_path(dirs, LOOKUP_FOLLOW | LOOKUP_DIRECTORY, &dir))
		return false;
	at = d_path(&dir, dst, PATH_MAX);
	path_put(&dir);
	if (IS_ERR(at))
		return false;
	len = strlen(at);
	memmove(dst, at, len + 1);
	if (len == 1)
		len = 0;
	if (len + 1 + strlen(last) >= PATH_MAX)
		return false;
	dst[len++] = '/';
	strscpy(dst + len, last, PATH_MAX - len);
	return true;
}

static void rewind_report_open(long fd, unsigned long flags)
{
	struct file *file;
	char *path;

	if ((flags & O_ACCMODE) == O_RDONLY && !(flags & (O_CREAT | O_TRUNC)))
		return;
	file = fget(fd);
	if (!file)
		return;
	path = d_path(&file->f_path, rewind_path_a, PATH_MAX);
	if (!IS_ERR(path)) {
		rewind_strip_deleted(path);
		rewind_emit(REWIND_EV_OPEN, 0, current->tgid, current->pid,
			    flags, path, strlen(path), NULL, 0);
	}
	fput(file);
}

static void rewind_report_unlink(int dfd, unsigned long path)
{
	if (!rewind_user_path(rewind_path_a, dfd, path))
		return;

	rewind_emit(REWIND_EV_UNLINK, 0, current->tgid, current->pid, 0,
		    rewind_path_a, strlen(rewind_path_a), NULL, 0);
}

static void rewind_report_rename(int from_dfd, unsigned long from,
				 int to_dfd, unsigned long to)
{
	if (!rewind_user_path(rewind_path_a, from_dfd, from) ||
	    !rewind_user_path(rewind_path_b, to_dfd, to))
		return;

	rewind_emit(REWIND_EV_RENAME, 0, current->tgid, current->pid, 0,
		    rewind_path_a, strlen(rewind_path_a) + 1,
		    rewind_path_b, strlen(rewind_path_b));
}

static void rewind_report_close(unsigned int fd)
{
	struct file *file = fget(fd);
	char *path;

	if (!file)
		return;
	if ((file->f_mode & FMODE_WRITE) &&
	    S_ISREG(file_inode(file)->i_mode) && file_count(file) == 2) {
		path = d_path(&file->f_path, rewind_path_a, PATH_MAX);
		if (!IS_ERR(path)) {
			rewind_strip_deleted(path);
			rewind_emit(REWIND_EV_CLOSE, 0, current->tgid,
				    current->pid, 0, path, strlen(path),
				    NULL, 0);
		}
	}
	fput(file);
}

static void rewind_probe_sys_enter(void *data, struct pt_regs *regs, long id)
{
	if (unlikely(READ_ONCE(rewind_inspecting)) && is_global_init(current))
		wait_event_idle(rewind_inspect_done, !READ_ONCE(rewind_inspecting));

	if (is_compat_task())
		return;

	if (id == __NR_close) {
		mutex_lock(&rewind_path_lock);
		rewind_report_close(regs->regs[0]);
		rewind_unlock(&rewind_path_lock);
	} else if (id == __NR_dup3 && regs->regs[0] != regs->regs[1]) {
		mutex_lock(&rewind_path_lock);
		rewind_report_close(regs->regs[1]);
		rewind_unlock(&rewind_path_lock);
	}
}

static void rewind_io_write(struct pt_regs *regs, long id, long ret)
{
	loff_t __user *off_out;
	struct file *file;
	unsigned int fd;
	loff_t end;
	struct {
		u64 pos;
		u32 count;
	} __packed w;

	switch (id) {
	case __NR_write:
	case __NR_writev:
	case __NR_pwrite64:
	case __NR_pwritev:
	case __NR_pwritev2:
	case __NR_sendfile:
		fd = regs->regs[0];
		break;
	case __NR_copy_file_range:
		fd = regs->regs[2];
		break;
	default:
		return;
	}
	file = fget(fd);
	if (!file)
		return;
	if (!S_ISREG(file_inode(file)->i_mode)) {
		fput(file);
		return;
	}

	off_out = (loff_t __user *)regs->regs[3];
	if (id == __NR_pwrite64 || id == __NR_pwritev ||
	    (id == __NR_pwritev2 && (long)regs->regs[3] != -1))
		w.pos = regs->regs[3];
	else if (id == __NR_copy_file_range && off_out &&
		 !get_user(end, off_out))
		w.pos = end - ret;
	else
		w.pos = file->f_pos - ret;
	w.count = ret;
	rewind_io_append(REWIND_IO_WRITE, file, &w, sizeof(w));
	fput(file);
}

static void rewind_io_open(long fd)
{
	struct file *file = fget(fd);
	char *path;
	u8 kind;

	if (!file)
		return;
	kind = (file->f_mode & FMODE_WRITE) ? REWIND_IO_WRITE_OPEN :
					      REWIND_IO_READ_OPEN;
	if (S_ISREG(file_inode(file)->i_mode)) {
		path = d_path(&file->f_path, rewind_path_a, PATH_MAX);
		if (!IS_ERR(path)) {
			rewind_strip_deleted(path);
			rewind_io_append(kind, file, path, strlen(path));
		}
	}
	fput(file);
}

static void rewind_report_changes(struct pt_regs *regs, long id, long ret)
{
	u64 how_flags;

	switch (id) {
	case __NR_openat:
	case __NR_openat2:
	case __NR_unlinkat:
	case __NR_renameat:
	case __NR_renameat2:
		break;
	default:
		return;
	}

	mutex_lock(&rewind_path_lock);
	switch (id) {
	case __NR_openat:
		rewind_report_open(ret, regs->regs[2]);
		break;
	case __NR_openat2:
		if (!get_user(how_flags, (u64 __user *)regs->regs[2]))
			rewind_report_open(ret, how_flags);
		break;
	case __NR_unlinkat:
		rewind_report_unlink(regs->regs[0], regs->regs[1]);
		break;
	case __NR_renameat:
	case __NR_renameat2:
		rewind_report_rename(regs->regs[0], regs->regs[1],
				     regs->regs[2], regs->regs[3]);
		break;
	}
	if (id == __NR_openat || id == __NR_openat2)
		rewind_io_open(ret);
	rewind_unlock(&rewind_path_lock);
}

static void rewind_probe_sys_exit(void *data, struct pt_regs *regs, long ret)
{
	long id = syscall_get_nr(current, regs);
	struct inode *inode;
	struct file *file;
	char *path;

	if (ret < 0 || is_compat_task())
		return;
	rewind_report_changes(regs, id, ret);
	if (ret > 0)
		rewind_io_write(regs, id, ret);

	if (id != __NR_openat && id != __NR_openat2)
		return;
	file = fget(ret);
	if (!file)
		return;
	inode = file_inode(file);
	if (!(file->f_mode & FMODE_WRITE) && S_ISREG(inode->i_mode) &&
	    atomic_read(&inode->i_writecount) > 0) {
		mutex_lock(&rewind_path_lock);
		path = d_path(&file->f_path, rewind_path_a, PATH_MAX);
		if (!IS_ERR(path)) {
			rewind_strip_deleted(path);
			rewind_emit(REWIND_EV_READ_OPEN, 0, current->tgid,
				    current->pid, file->f_flags, path,
				    strlen(path), NULL, 0);
		}
		rewind_unlock(&rewind_path_lock);
	}
	fput(file);
}

/* Rewind character devices and TTYs. */
static DEFINE_MUTEX(rewind_write_lock);
static u8 rewind_bounce[4096];

static ssize_t rewind_write(struct file *file, const char __user *ubuf,
			    size_t len, u16 kind, u32 aux)
{
	size_t done = 0;

	mutex_lock(&rewind_write_lock);
	while (done < len) {
		size_t n = min(len - done, sizeof(rewind_bounce));

		if (copy_from_user(rewind_bounce, ubuf + done, n)) {
			rewind_unlock(&rewind_write_lock);
			return done ? done : -EFAULT;
		}
		rewind_emit(kind, 0, current->tgid, current->pid, aux,
			    rewind_bounce, n, NULL, 0);
		done += n;
	}
	rewind_unlock(&rewind_write_lock);
	return done;
}

static ssize_t rewind_stdout_write(struct file *file, const char __user *buf,
				   size_t len, loff_t *off)
{
	return rewind_write(file, buf, len, REWIND_EV_OUTPUT, 1);
}

static ssize_t rewind_stderr_write(struct file *file, const char __user *buf,
				   size_t len, loff_t *off)
{
	return rewind_write(file, buf, len, REWIND_EV_OUTPUT, 2);
}

static ssize_t rewind_mark_write(struct file *file, const char __user *buf,
				 size_t len, loff_t *off)
{
	if (len == 0) {
		rewind_yield();
		return 0;
	}
	return rewind_write(file, buf, len, REWIND_EV_MARK, 0);
}

#define REWIND_TERM_FD	3

static ssize_t rewind_term_write(struct file *file, const char __user *buf,
				 size_t len, loff_t *off)
{
	return rewind_write(file, buf, len, REWIND_EV_OUTPUT, REWIND_TERM_FD);
}

static ssize_t rewind_term_read(struct file *file, char __user *buf,
				size_t len, loff_t *off)
{
	unsigned int copied;
	int err;

	if (kfifo_is_empty(&rewind_input)) {
		if (file->f_flags & O_NONBLOCK)
			return -EAGAIN;
		err = wait_event_interruptible(rewind_input_wait,
					       !kfifo_is_empty(&rewind_input));
		if (err)
			return err;
	}
	err = kfifo_to_user(&rewind_input, buf, len, &copied);
	return err ? err : copied;
}

static __poll_t rewind_term_poll(struct file *file, poll_table *wait)
{
	poll_wait(file, &rewind_input_wait, wait);
	return EPOLLOUT | EPOLLWRNORM |
	       (kfifo_is_empty(&rewind_input) ? 0 : EPOLLIN | EPOLLRDNORM);
}

static const struct file_operations rewind_term_fops = {
	.owner	= THIS_MODULE,
	.read	= rewind_term_read,
	.write	= rewind_term_write,
	.poll	= rewind_term_poll,
};

static const struct file_operations rewind_stdout_fops = {
	.owner	= THIS_MODULE,
	.write	= rewind_stdout_write,
};

static const struct file_operations rewind_stderr_fops = {
	.owner	= THIS_MODULE,
	.write	= rewind_stderr_write,
};

static const struct file_operations rewind_mark_fops = {
	.owner	= THIS_MODULE,
	.write	= rewind_mark_write,
};

static struct miscdevice rewind_devices[] = {
	{ MISC_DYNAMIC_MINOR, "rewind-stdout", &rewind_stdout_fops, .mode = 0666 },
	{ MISC_DYNAMIC_MINOR, "rewind-stderr", &rewind_stderr_fops, .mode = 0666 },
	{ MISC_DYNAMIC_MINOR, "rewind", &rewind_mark_fops, .mode = 0666 },
	{ MISC_DYNAMIC_MINOR, "rewind-console", &rewind_term_fops, .mode = 0600 },
};

#define REWIND_TTY_LINES	2
#define REWIND_TTY_ROOM		65536

static struct tty_driver *rewind_tty_driver;
static struct tty_port rewind_tty_ports[REWIND_TTY_LINES];
static const struct tty_port_operations rewind_tty_port_ops;

static int rewind_tty_open(struct tty_struct *tty, struct file *filp)
{
	set_bit(TTY_NO_WRITE_SPLIT, &tty->flags);
	return tty_port_open(&rewind_tty_ports[tty->index], tty, filp);
}

static void rewind_tty_close(struct tty_struct *tty, struct file *filp)
{
	tty_port_close(&rewind_tty_ports[tty->index], tty, filp);
}

static void rewind_tty_hangup(struct tty_struct *tty)
{
	tty_port_hangup(&rewind_tty_ports[tty->index]);
}

static ssize_t rewind_tty_write(struct tty_struct *tty, const u8 *buf,
				size_t count)
{
	u32 fd = tty->index + 1;
	size_t done = 0;

	while (done < count) {
		size_t n = min(count - done, REWIND_PAYLOAD_MAX);

		rewind_emit(REWIND_EV_OUTPUT, 0, current->tgid, current->pid,
			    fd, buf + done, n, NULL, 0);
		done += n;
	}
	return count;
}

static unsigned int rewind_tty_write_room(struct tty_struct *tty)
{
	return REWIND_TTY_ROOM;
}

static const struct tty_operations rewind_tty_ops = {
	.open		= rewind_tty_open,
	.close		= rewind_tty_close,
	.hangup		= rewind_tty_hangup,
	.write		= rewind_tty_write,
	.write_room	= rewind_tty_write_room,
};

static int __init rewind_tty_init(void)
{
	struct tty_driver *driver;
	struct ktermios *t;
	int i, err;

	driver = tty_alloc_driver(REWIND_TTY_LINES,
				  TTY_DRIVER_RESET_TERMIOS |
				  TTY_DRIVER_REAL_RAW);
	if (IS_ERR(driver))
		return PTR_ERR(driver);

	driver->driver_name = "rewind-tty";
	driver->name = "rewind-tty";
	driver->type = TTY_DRIVER_TYPE_SERIAL;
	driver->subtype = SERIAL_TYPE_NORMAL;
	driver->init_termios = tty_std_termios;
	t = &driver->init_termios;
	t->c_iflag &= ~(IGNBRK | BRKINT | PARMRK | ISTRIP | INLCR | IGNCR |
			ICRNL | IXON);
	t->c_oflag &= ~OPOST;
	t->c_lflag &= ~(ECHO | ECHONL | ICANON | ISIG | IEXTEN);
	t->c_cflag &= ~(CSIZE | PARENB);
	t->c_cflag |= CS8;
	tty_set_operations(driver, &rewind_tty_ops);

	for (i = 0; i < REWIND_TTY_LINES; i++) {
		tty_port_init(&rewind_tty_ports[i]);
		rewind_tty_ports[i].ops = &rewind_tty_port_ops;
		tty_port_link_device(&rewind_tty_ports[i], driver, i);
	}

	err = tty_register_driver(driver);
	if (err) {
		tty_driver_kref_put(driver);
		for (i = 0; i < REWIND_TTY_LINES; i++)
			tty_port_destroy(&rewind_tty_ports[i]);
		return err;
	}
	rewind_tty_driver = driver;
	return 0;
}

static int __init rewind_events_init(void)
{
	int i, err;

	if (!rewind_running)
		return 0;

	/*
	 * Trap user-space CNTVCT_EL0/CNTPCT_EL0/CNTFRQ_EL0 reads into EL1 so
	 * arch/arm64/kernel/traps.c emulates them with Rewind's virtual clock.
	 */
	write_sysreg(0, cntkctl_el1);
	isb();

	for (i = 0; i < ARRAY_SIZE(rewind_devices); i++) {
		err = misc_register(&rewind_devices[i]);
		if (err)
			return err;
	}

	err = rewind_tty_init();
	if (err)
		return err;

	WARN_ON(register_trace_sched_process_exec(rewind_probe_exec, NULL));
	WARN_ON(register_trace_sched_process_fork(rewind_probe_fork, NULL));
	WARN_ON(register_trace_sched_process_exit(rewind_probe_exit, NULL));
	WARN_ON(register_trace_sched_waking(rewind_probe_waking, NULL));
	WARN_ON(register_trace_signal_deliver(rewind_probe_signal, NULL));
	WARN_ON(register_trace_sys_enter(rewind_probe_sys_enter, NULL));
	WARN_ON(register_trace_sys_exit(rewind_probe_sys_exit, NULL));
	WARN_ON(register_reboot_notifier(&rewind_reboot_nb));
	return 0;
}
device_initcall(rewind_events_init);

/* SPDX-License-Identifier: GPL-2.0 */
#ifndef __ASM_ARM64_REWIND_H
#define __ASM_ARM64_REWIND_H

#ifdef CONFIG_REWIND_GUEST
#include <linux/types.h>

struct cpumask;
struct pt_regs;
struct task_struct;

extern bool rewind_running;

void rewind_init_platform(void);
void rewind_timer_init(void);
void rewind_irq_init(void);
void rewind_idle(void);
void rewind_power_off(void);
void rewind_restart(void);
void rewind_halt(void);
u64 rewind_clock_now(void);
u64 rewind_sched_clock(void);
unsigned long rewind_get_entropy(void);

void rewind_remember_cpus(struct task_struct *p, const struct cpumask *mask);
void rewind_report_cpus(const struct task_struct *p, struct cpumask *mask);
const struct cpumask *rewind_fold_cpus(const struct cpumask *mask);
unsigned int rewind_user_cpus(void);
void rewind_yield(void);
void rewind_switch(struct task_struct *next, struct task_struct *prev);
#endif

#endif /* __ASM_ARM64_REWIND_H */

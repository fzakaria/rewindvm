//! The threads of a process in a forked machine, read from the VM
//! kernel's task structs where the kernel's [`TaskLayout`] says they are.
//! gdb sees the thread on the CPU through the vCPU's registers. Every
//! other thread saved its user registers in its struct pt_regs when it
//! last entered the kernel, and gdb sees those for it.

use anyhow::{Result, bail};
use rewind_vmm::pv::{TaskLayout, pt_regs};

/// The most entries a kernel list is followed through. A list longer than
/// any the VM can have was being changed, or is not a list.
const MAX_ENTRIES: usize = 1 << 16;

/// The length of a task's name, NUL included when shorter.
const TASK_COMM_LEN: usize = 16;

/// Where a list_head's `next` pointer is, and how long a kernel pointer
/// and pid are.
const LIST_NEXT: u64 = 0;
const POINTER_LEN: usize = 8;
const PID_LEN: usize = 4;

/// The VM kernel's memory, by kernel virtual address.
pub trait Memory {
    fn read(&self, address: u64, buf: &mut [u8]) -> Result<()>;
}

/// One thread of a process: its id, its task struct, and its name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thread {
    pub tid: u32,
    pub task: u64,
    pub name: String,
}

/// The VM kernel's tasks, read through `mem`.
pub struct Tasks<'a, M: Memory + ?Sized> {
    mem: &'a M,
    layout: TaskLayout,
}

impl<'a, M: Memory + ?Sized> Tasks<'a, M> {
    pub fn new(mem: &'a M, layout: TaskLayout) -> Tasks<'a, M> {
        Tasks { mem, layout }
    }

    /// The task on the CPU.
    pub fn current(&self) -> Result<u64> {
        self.pointer(self.layout.current_task)
    }

    /// The thread on the CPU, with the id of the process it belongs to.
    pub fn current_thread(&self) -> Result<(u32, Thread)> {
        let task = self.current()?;
        let thread = Thread {
            tid: self.tid(task)?,
            task,
            name: self.name(task)?,
        };
        Ok((self.tgid(task)?, thread))
    }

    /// A task's thread id: its pid, which is 0 for the idle task.
    pub fn tid(&self, task: u64) -> Result<u32> {
        self.pid_at(task + self.layout.pid)
    }

    /// The id of the process a task belongs to.
    pub fn tgid(&self, task: u64) -> Result<u32> {
        self.pid_at(task + self.layout.tgid)
    }

    /// A task's name, as the kernel keeps it.
    pub fn name(&self, task: u64) -> Result<String> {
        let mut comm = [0u8; TASK_COMM_LEN];
        self.mem.read(task + self.layout.comm, &mut comm)?;
        let len = comm.iter().position(|&b| b == 0).unwrap_or(TASK_COMM_LEN);
        Ok(String::from_utf8_lossy(&comm[..len]).into_owned())
    }

    /// The task of process `tgid`'s first thread, found in the list of
    /// processes that starts at init_task; None when it has none.
    pub fn process(&self, tgid: u32) -> Result<Option<u64>> {
        let head = self.layout.init_task + self.layout.tasks;
        for task in self.entries(head, self.layout.tasks)? {
            if self.tgid(task)? == tgid {
                return Ok(Some(task));
            }
        }
        Ok(None)
    }

    /// Every thread of the process `task` belongs to, in the kernel's
    /// order: the list in its signal_struct, linked through each task's
    /// `thread_node`.
    pub fn threads(&self, task: u64) -> Result<Vec<Thread>> {
        let signal = self.pointer(task + self.layout.signal)?;
        let head = signal + self.layout.thread_head;
        self.entries(head, self.layout.thread_node)?
            .into_iter()
            .map(|task| {
                Ok(Thread {
                    tid: self.tid(task)?,
                    task,
                    name: self.name(task)?,
                })
            })
            .collect()
    }

    /// The physical address of a task's page table, or None for a kernel
    /// thread, which has no memory of its own.
    pub fn page_table(&self, task: u64) -> Result<Option<u64>> {
        let mm = self.pointer(task + self.layout.mm)?;
        if mm == 0 {
            return Ok(None);
        }
        let pgd = self.pointer(mm + self.layout.pgd)?;
        Ok(Some(pgd - self.layout.page_offset))
    }

    /// The user registers a task saved when it last entered the kernel, in
    /// struct pt_regs at the top of its stack.
    pub fn user_registers(&self, task: u64) -> Result<[u64; pt_regs::WORDS]> {
        let stack = self.pointer(task + self.layout.stack)?;
        let mut bytes = [0u8; pt_regs::WORDS * POINTER_LEN];
        self.mem.read(stack + self.layout.pt_regs, &mut bytes)?;
        let mut words = [0u64; pt_regs::WORDS];
        for (word, chunk) in words.iter_mut().zip(bytes.as_chunks::<POINTER_LEN>().0) {
            *word = u64::from_le_bytes(*chunk);
        }
        Ok(words)
    }

    /// The structs on the list whose head is at `head`, each linked through
    /// the list_head `link` bytes into it.
    fn entries(&self, head: u64, link: u64) -> Result<Vec<u64>> {
        let mut entries = Vec::new();
        let mut node = self.pointer(head + LIST_NEXT)?;
        while node != head {
            if entries.len() == MAX_ENTRIES {
                bail!("a kernel list at {head:#x} runs past {MAX_ENTRIES} entries");
            }
            entries.push(node - link);
            node = self.pointer(node + LIST_NEXT)?;
        }
        Ok(entries)
    }

    fn pointer(&self, address: u64) -> Result<u64> {
        let mut bytes = [0u8; POINTER_LEN];
        self.mem.read(address, &mut bytes)?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn pid_at(&self, address: u64) -> Result<u32> {
        let mut bytes = [0u8; PID_LEN];
        self.mem.read(address, &mut bytes)?;
        Ok(u32::from_le_bytes(bytes))
    }
}

#[cfg(test)]
mod tests {
    // The task walk over a made-up kernel: task structs, a signal_struct
    // and an mm_struct laid out in one block of memory at the offsets a
    // made-up TaskLayout gives, linked the way the kernel links them.
    use super::*;
    use std::cell::RefCell;

    /// Where the made-up kernel's memory starts, as the direct map does.
    const BASE: u64 = 0xffff_8880_0000_0000;
    const SIZE: usize = 0x20000;
    /// Each struct gets a page of its own.
    const SLOT: u64 = 0x1000;

    /// The made-up kernel's memory.
    struct Fake(RefCell<Vec<u8>>);

    impl Fake {
        fn new() -> Fake {
            Fake(RefCell::new(vec![0; SIZE]))
        }

        fn put(&self, address: u64, bytes: &[u8]) {
            let at = (address - BASE) as usize;
            self.0.borrow_mut()[at..at + bytes.len()].copy_from_slice(bytes);
        }

        fn put_u64(&self, address: u64, value: u64) {
            self.put(address, &value.to_le_bytes());
        }

        fn put_u32(&self, address: u64, value: u32) {
            self.put(address, &value.to_le_bytes());
        }
    }

    impl Memory for Fake {
        fn read(&self, address: u64, buf: &mut [u8]) -> Result<()> {
            let Some(at) = address
                .checked_sub(BASE)
                .map(|a| a as usize)
                .filter(|a| a + buf.len() <= SIZE)
            else {
                bail!("{address:#x} is not mapped");
            };
            buf.copy_from_slice(&self.0.borrow()[at..at + buf.len()]);
            Ok(())
        }
    }

    fn layout() -> TaskLayout {
        TaskLayout {
            init_task: BASE,
            current_task: BASE + SIZE as u64 - SLOT,
            page_offset: BASE,
            tasks: 0x10,
            thread_node: 0x20,
            signal: 0x30,
            pid: 0x40,
            tgid: 0x44,
            stack: 0x48,
            mm: 0x50,
            comm: 0x60,
            thread_head: 0x8,
            pgd: 0x18,
            pt_regs: 0x800,
        }
    }

    /// Links the structs at `entries` into a circular list at `head`,
    /// through the list_head `link` bytes into each.
    fn link(fake: &Fake, head: u64, entries: &[u64], link: u64) {
        let mut prev = head;
        for &entry in entries {
            fake.put_u64(prev, entry + link);
            prev = entry + link;
        }
        fake.put_u64(prev, head);
    }

    /// A task at slot `slot` with these ids, name, signal_struct and mm.
    fn task(
        fake: &Fake,
        slot: u64,
        (pid, tgid): (u32, u32),
        name: &str,
        signal: u64,
        mm: u64,
    ) -> u64 {
        let l = layout();
        let task = BASE + slot * SLOT;
        fake.put_u32(task + l.pid, pid);
        fake.put_u32(task + l.tgid, tgid);
        fake.put(task + l.comm, name.as_bytes());
        fake.put_u64(task + l.signal, signal);
        fake.put_u64(task + l.mm, mm);
        task
    }

    /// init_task, then process 40 with threads 40, 41 and 42, then
    /// process 50 with one thread, a kernel thread with no mm. Returns the
    /// memory and process 40's three tasks.
    fn kernel() -> (Fake, [u64; 3]) {
        let fake = Fake::new();
        let l = layout();
        let signal40 = BASE + 10 * SLOT;
        let signal50 = BASE + 11 * SLOT;
        let mm40 = BASE + 12 * SLOT;
        fake.put_u64(mm40 + l.pgd, BASE + 0x9000);

        let init = task(&fake, 0, (0, 0), "swapper", 0, 0);
        let t40 = task(&fake, 1, (40, 40), "phil", signal40, mm40);
        let t41 = task(&fake, 2, (41, 40), "phil", signal40, mm40);
        let t42 = task(&fake, 3, (42, 40), "phil", signal40, mm40);
        let t50 = task(&fake, 4, (50, 50), "kworker/0:1", signal50, 0);

        link(&fake, init + l.tasks, &[t40, t50], l.tasks);
        link(
            &fake,
            signal40 + l.thread_head,
            &[t40, t41, t42],
            l.thread_node,
        );
        link(&fake, signal50 + l.thread_head, &[t50], l.thread_node);
        fake.put_u64(l.current_task, t41);
        (fake, [t40, t41, t42])
    }

    /// A process is found by its id in the list of processes; one with
    /// no task there is not.
    #[test]
    fn a_process_is_found_by_its_id() {
        let (fake, [t40, ..]) = kernel();
        let tasks = Tasks::new(&fake, layout());
        assert_eq!(tasks.process(40).unwrap(), Some(t40));
        assert_eq!(tasks.process(60).unwrap(), None);
    }

    /// A process's threads come from its signal_struct's list, each with
    /// its id and name, and the running one from current_task.
    #[test]
    fn a_process_lists_every_thread() {
        let (fake, [t40, t41, t42]) = kernel();
        let tasks = Tasks::new(&fake, layout());
        let threads = tasks.threads(t42).unwrap();
        let ids: Vec<(u32, u64)> = threads.iter().map(|t| (t.tid, t.task)).collect();
        assert_eq!(ids, vec![(40, t40), (41, t41), (42, t42)]);
        assert!(threads.iter().all(|t| t.name == "phil"));
        assert_eq!(tasks.current().unwrap(), t41);
        assert_eq!(tasks.tgid(t41).unwrap(), 40);
    }

    /// The thread on the CPU, with the process it belongs to and its
    /// name, read from current_task.
    #[test]
    fn the_thread_on_the_cpu_names_its_process() {
        let (fake, [_, t41, _]) = kernel();
        let tasks = Tasks::new(&fake, layout());
        assert_eq!(
            tasks.current_thread().unwrap(),
            (
                40,
                Thread {
                    tid: 41,
                    task: t41,
                    name: "phil".into(),
                }
            )
        );
    }

    /// The page table is mm->pgd's physical address; a kernel thread has
    /// no mm and so none.
    #[test]
    fn a_page_table_is_the_pgd_less_the_direct_map() {
        let (fake, [t40, ..]) = kernel();
        let tasks = Tasks::new(&fake, layout());
        assert_eq!(tasks.page_table(t40).unwrap(), Some(0x9000));
        let t50 = tasks.process(50).unwrap().unwrap();
        assert_eq!(tasks.page_table(t50).unwrap(), None);
    }

    /// A thread's user registers are the words of its pt_regs, at the
    /// layout's offset into its stack.
    #[test]
    fn user_registers_come_from_the_top_of_the_stack() {
        let (fake, [_, t41, _]) = kernel();
        let l = layout();
        let stack = BASE + 20 * SLOT;
        fake.put_u64(t41 + l.stack, stack);
        for word in 0..pt_regs::WORDS {
            fake.put_u64(stack + l.pt_regs + 8 * word as u64, 100 + word as u64);
        }
        let regs = Tasks::new(&fake, l).user_registers(t41).unwrap();
        assert_eq!(regs[pt_regs::R15], 100);
        assert_eq!(regs[pt_regs::RIP], 100 + pt_regs::RIP as u64);
        assert_eq!(regs[pt_regs::SS], 100 + pt_regs::SS as u64);
    }

    /// A list that never comes back to its head is refused, not followed
    /// forever.
    #[test]
    fn a_list_that_never_ends_is_refused() {
        let (fake, [t40, ..]) = kernel();
        let l = layout();
        let signal40 = BASE + 10 * SLOT;
        // The first thread's link points at itself.
        fake.put_u64(t40 + l.thread_node, t40 + l.thread_node);
        let err = Tasks::new(&fake, l).threads(t40).unwrap_err();
        assert!(
            err.to_string()
                .contains(&format!("{:#x}", signal40 + l.thread_head))
        );
    }
}

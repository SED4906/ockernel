use super::{queue::TaskQueue, ProcessID};
use crate::arch::ThreadInfo;
use alloc::{collections::VecDeque, vec::Vec};
use common::types::{Errno, Result};
use core::{
    fmt,
    sync::atomic::{AtomicBool, Ordering},
};
use log::{trace, warn};
use spin::Mutex;

/// describes a CPU and its layout of cores and threads
///
/// this kind of knowledge of the CPU's topology is required for more intelligent load balancing
#[derive(Debug)]
pub struct CPU {
    pub cores: Vec<CPUCore>,
}

impl CPU {
    pub fn new() -> Self {
        Self { cores: Vec::new() }
    }

    /// adds a core with the given number of threads to this CPU representation
    pub fn add_core(&mut self) {
        self.cores.push(CPUCore { threads: Vec::new(), num_tasks: 0 });
    }

    /// gets a reference to a CPU thread given its ID
    pub fn get_thread(&self, id: ThreadID) -> Option<&CPUThread> {
        self.cores.get(id.core)?.threads.get(id.thread)
    }

    /// gets a mutable reference to a CPU thread given its ID
    pub fn get_thread_mut(&mut self, id: ThreadID) -> Option<&mut CPUThread> {
        self.cores.get_mut(id.core)?.threads.get_mut(id.thread)
    }

    /// searches through threads in this CPU in hierarchical order to find a thread with at least one extra task
    pub fn find_thread_to_steal_from(&self, id: ThreadID) -> Option<ThreadID> {
        // search threads in the same core as the provided ID
        if let Some(thread_id) = self.cores.get(id.core)?.find_busiest_thread() {
            return Some(ThreadID { core: id.core, thread: thread_id });
        }

        // search all other cores
        for (core_id, core) in self.cores.iter().enumerate() {
            // skip the original core, no need to search it twice
            // it's possible that since the CPU struct itself won't be locked that maybe another task will pop up by now,
            // but it probably doesn't matter and is likely not worth the overhead
            if core_id == id.core {
                continue;
            }

            if let Some(thread_id) = core.find_busiest_thread() {
                return Some(ThreadID { core: core_id, thread: thread_id });
            }
        }

        // no tasks?
        None
    }

    /// searches through cores and threads in this CPU to find the one with the least amount of tasks
    pub fn find_thread_to_add_to(&self) -> Option<ThreadID> {
        let mut possible_threads = Vec::new();

        for (core_id, core) in self.cores.iter().enumerate() {
            if let Some((thread_num, num_tasks)) = core.find_emptiest_thread() {
                let id = ThreadID { core: core_id, thread: thread_num };

                if num_tasks == 0 {
                    return Some(id);
                } else {
                    possible_threads.push((id, num_tasks));
                }
            }
        }

        let mut thread_id = None;
        let mut num_tasks = usize::MAX;

        for (id, cur_num_tasks) in possible_threads.iter() {
            if *cur_num_tasks < num_tasks {
                thread_id = Some(*id);
                num_tasks = *cur_num_tasks;
            }
        }

        thread_id
    }
}

impl Default for CPU {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub struct CPUCore {
    pub threads: Vec<CPUThread>,
    pub num_tasks: usize,
}

impl CPUCore {
    /// adds a new thread to this core
    pub fn add_thread(&mut self, info: ThreadInfo, timer: usize) {
        self.threads.push(CPUThread::new(info, timer));
    }

    /// finds the thread in this core with the most tasks waiting in its queue
    pub fn find_busiest_thread(&self) -> Option<usize> {
        let mut thread_id = None;
        let mut num_tasks = 0;

        for (id, thread) in self.threads.iter().enumerate() {
            let cur_num_tasks = thread.task_queue.lock().len();
            if cur_num_tasks > num_tasks {
                thread_id = Some(id);
                num_tasks = cur_num_tasks;
            }
        }

        thread_id
    }

    /// finds the thread in this core with the least tasks waiting in its queue
    ///
    /// when successful, returns the ID of the thread and how many tasks it has
    pub fn find_emptiest_thread(&self) -> Option<(usize, usize)> {
        let mut thread_id = None;
        let mut num_tasks = usize::MAX;

        for (id, thread) in self.threads.iter().enumerate() {
            let cur_num_tasks = {
                let queue = thread.task_queue.lock();
                queue.len() + usize::from(queue.current().is_some())
            };
            if cur_num_tasks < num_tasks {
                thread_id = Some(id);
                num_tasks = cur_num_tasks;
            }
        }

        thread_id.map(|i| (i, num_tasks))
    }
}

#[derive(Debug, Copy, Clone)]
pub enum UrgentMessage {
    /// update a page in the current task's address space
    TaskPageUpdate { process_id: u32, addr: usize },

    /// update a page in the kernel's address space
    KernelPageUpdate { addr: usize },
}

#[derive(Debug, Copy, Clone)]
pub enum Message {
    /// kill the specified thread
    KillThread(ProcessID),

    /// kill all threads of the specified process
    KillProcess(u32),

    SendMessage {
        process: u32,
        message: u32,
        data: Option<(u64, usize)>,
    },
}

#[derive(Debug)]
pub struct CPUThread {
    pub task_queue: Mutex<TaskQueue>,
    pub urgent_message_queue: Mutex<VecDeque<UrgentMessage>>,
    pub message_queue: Mutex<VecDeque<Message>>,
    pub timer: usize,
    pub info: ThreadInfo,
    in_kernel: AtomicBool,
    has_started: AtomicBool,
}

impl CPUThread {
    pub fn new(info: ThreadInfo, timer: usize) -> Self {
        Self {
            task_queue: Mutex::new(TaskQueue::new()),
            urgent_message_queue: Mutex::new(VecDeque::new()),
            message_queue: Mutex::new(VecDeque::new()),
            timer,
            info,
            in_kernel: AtomicBool::new(true),
            has_started: AtomicBool::new(false),
        }
    }

    pub fn send_urgent_message(&self, message: UrgentMessage) -> Result<()> {
        let mut queue = self.urgent_message_queue.lock();
        queue.try_reserve(1).map_err(|_| Errno::OutOfMemory)?;
        queue.push_back(message);
        Ok(())
    }

    pub fn process_urgent_messages(&self) {
        while let Some(entry) = self.urgent_message_queue.lock().pop_front() {
            trace!("processing {entry:?}");
            match entry {
                UrgentMessage::TaskPageUpdate { process_id: id, addr } => {
                    let queue_lock = self.task_queue.lock(); // hold queue lock for as long as this TLB flush takes, not sure whether a race condition here would be bad or not

                    let process_id = queue_lock.current().map(|c| c.id());
                    if let Some(pid) = process_id && id == pid.process {
                        crate::arch::refresh_page(addr);
                    }
                }
                UrgentMessage::KernelPageUpdate { addr } => crate::arch::refresh_page(addr),
            }
        }
    }

    pub fn send_message(&self, message: Message) -> Result<()> {
        let mut queue = self.message_queue.lock();
        queue.try_reserve(1).map_err(|_| Errno::OutOfMemory)?;
        queue.push_back(message);
        Ok(())
    }

    pub fn process_messages(&self, cpu: ThreadID, regs: &mut crate::arch::Registers) {
        while let Some(entry) = self.message_queue.lock().pop_front() {
            trace!("processing {entry:?}");
            match entry {
                Message::KillThread(id) => {
                    self.task_queue.lock().remove_thread(id);
                    if let Some(current_id) = self.task_queue.lock().current().map(|c| c.id()) && current_id == id {
                        super::switch::manual_context_switch(self.timer, Some(cpu), regs, super::switch::ContextSwitchMode::Remove);
                    }
                }
                Message::KillProcess(id) => {
                    self.task_queue.lock().remove_process(id);
                    if let Some(current_id) = self.task_queue.lock().current().map(|c| c.id()) && current_id.process == id {
                        super::switch::manual_context_switch(self.timer, Some(cpu), regs, super::switch::ContextSwitchMode::Remove);
                    }
                }
                Message::SendMessage { process, message, data } => {
                    match super::ipc::send_message(cpu, self, regs, process, message, data) {
                        Ok(_) => (),
                        Err(Errno::NoSuchProcess) => (), // process doesn't exist, do nothing here since it probably exited
                        Err(err) => warn!("(CPU {cpu}) couldn't send_message: {err:?}"),
                    }
                }
            }
        }
    }

    pub fn check_enter_kernel(&self) {
        if self.enter_kernel() {
            panic!("already in kernel");
        }
    }

    pub fn enter_kernel(&self) -> bool {
        //trace!("entering kernel");
        self.in_kernel.swap(true, Ordering::Acquire)
    }

    pub fn leave_kernel(&self) {
        //trace!("leaving kernel");
        self.in_kernel.store(false, Ordering::Release);
    }

    pub fn start(&self) {
        if self.has_started.swap(true, Ordering::Acquire) {
            panic!("CPU already started");
        }
    }

    pub fn has_started(&self) -> bool {
        self.has_started.load(Ordering::Relaxed)
    }
}

/// an ID of a CPU thread
#[derive(Default, Copy, Clone, Debug, PartialEq, Eq)]
pub struct ThreadID {
    pub core: usize,
    pub thread: usize,
}

impl fmt::Display for ThreadID {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.core, self.thread)
    }
}

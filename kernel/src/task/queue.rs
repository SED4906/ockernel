use alloc::{collections::VecDeque, vec::Vec};
use common::types::{Errno, Result};

/// a per-CPU task queue
#[derive(Debug)]
pub struct TaskQueue {
    /// current
    current: Option<TaskQueueEntry>,

    /// tasks waiting for CPU time
    queue: VecDeque<TaskQueueEntry>,

    pub timer: Option<u64>,
}

impl TaskQueue {
    pub fn new() -> Self {
        Self {
            current: None,
            queue: VecDeque::new(),
            timer: None,
        }
    }

    /// gets the first task in the queue
    pub fn consume(&mut self) -> Option<&TaskQueueEntry> {
        self.current = self.queue.pop_front();

        self.current.as_ref()
    }

    /// wrapper around try_reserve for the internal queue structure
    pub fn try_reserve(&mut self, amt: usize) -> Result<()> {
        self.queue.try_reserve(amt).map_err(|_| Errno::OutOfMemory)
    }

    /// inserts a task into the queue
    pub fn insert(&mut self, entry: TaskQueueEntry) -> Result<()> {
        self.try_reserve(1)?;

        let mut insert_position = None;

        for (idx, item) in self.queue.iter().enumerate() {
            if item.id == entry.id {
                return Err(Errno::Exists);
            } else if insert_position.is_none() && entry.full_priority() > item.full_priority() {
                insert_position = Some(idx);
            }
        }

        match insert_position {
            Some(index) => self.queue.insert(index, entry),
            None => self.queue.push_back(entry),
        }

        Ok(())
    }

    /// checks whether this taskqueue is empty
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// gets how many tasks are in this queue
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// gets the current task being processed in the queue
    pub fn current(&self) -> Option<TaskQueueEntry> {
        self.current
    }

    /// given a fully qualified process id, remove the thread corresponding to it from the queue
    pub fn remove_thread(&mut self, id: super::ProcessID) {
        if let Some(index) = self.queue.iter().position(|e| e.id() == id) {
            self.queue.remove(index);
        }
    }

    /// given a process id, remove all threads corresponding to it from the queue
    pub fn remove_process(&mut self, id: u32) {
        let to_remove = self.queue.iter().enumerate().filter(|(_, e)| e.id().process == id).map(|(i, _)| i).collect::<Vec<usize>>();

        for index in to_remove.iter() {
            self.queue.remove(*index);
        }
    }
}

impl Default for TaskQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// an entry in a task queue
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct TaskQueueEntry {
    /// the PID associated with this task
    id: super::ProcessID,

    /// the priority of this task
    priority: u8,
}

impl TaskQueueEntry {
    /// creates a new task queue entry for the given process with the given priority
    pub fn new(id: super::ProcessID, priority: i8) -> Self {
        Self {
            priority: (((priority + 7) as u8) << 4) | 7,
            id,
        }
    }

    /// gets the priority of this task queue entry
    pub fn priority(&self) -> i8 {
        (self.priority >> 4) as i8 - 7
    }

    /// sets the priority of this task queue entry
    pub fn set_priority(&mut self, priority: i8) {
        self.priority = (self.priority & 0x0f) | (((priority + 7) as u8) << 4);
    }

    /// gets the sub-priority of this task queue entry
    pub fn sub_priority(&self) -> i8 {
        (self.priority & 0xf) as i8 - 7
    }

    /// sets the sub-priority of this task queue entry
    pub fn set_sub_priority(&mut self, sub_priority: i8) {
        self.priority = (self.priority & 0xf0) | (sub_priority + 7) as u8;
    }

    /// gets the full priority index of this task queue entry
    pub fn full_priority(&self) -> u8 {
        self.priority
    }

    /// gets the task id that this task queue entry represents
    pub fn id(&self) -> super::ProcessID {
        self.id
    }
}

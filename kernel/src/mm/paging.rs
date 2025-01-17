//! paging abstraction layer

use super::sync::PageDirTracker;
use crate::{
    mm::sync::MutexedPageDir,
    util::{array::BitSet, debug::FormatHex},
};
use alloc::{
    alloc::{alloc, dealloc, Layout},
    collections::BTreeMap,
    vec::Vec,
};
use common::types::Errno;
use core::fmt;
use lazy_static::lazy_static;
use log::{debug, error, trace};
use spin::{Mutex, MutexGuard};

/// an error that can be returned from paging operations
pub enum PagingError {
    NoAvailableFrames,
    FrameUnused,
    FrameInUse,
    AllocError,
    BadFrame,
    BadAddress,
}

impl fmt::Debug for PagingError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", match self {
            Self::NoAvailableFrames => "no available frames (out of memory)",
            Self::FrameUnused => "frame is unused",
            Self::FrameInUse => "frame already in use",
            Self::AllocError => "error allocating memory",
            Self::BadFrame => "bad frame",
            Self::BadAddress => "address not mapped",
        })
    }
}

impl From<PagingError> for Errno {
    fn from(err: PagingError) -> Errno {
        match err {
            PagingError::BadAddress => Errno::BadAddress,
            _ => Errno::OutOfMemory,
        }
    }
}

/// hardware agnostic form of a page frame
#[derive(Default, Copy, Clone)]
pub struct PageFrame {
    /// physical address of this page frame
    ///
    /// this determines where in physical memory this page will map to
    pub addr: u64,

    /// whether this page is present in memory and can be accessed
    ///
    /// can be used to swap pages out of memory and reload them when accessed
    pub present: bool,

    /// whether this frame can be accessed in user mode (ring 3)
    pub user_mode: bool,

    /// whether this frame can be written to
    pub writable: bool,

    /// whether this page should be copied upon attempting to write to it (requires writable flag to be disabled)
    pub copy_on_write: bool,

    /// whether code can be executed from this page. not supported on all platforms
    pub executable: bool,

    /// whether this page has more than one reference and its freeing should be handled by the reference counter
    pub referenced: bool,

    /// whether this page has been shared from another process
    pub shared: bool,
}

impl fmt::Debug for PageFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PageFrame")
            .field("addr", &FormatHex(self.addr))
            .field("present", &self.present)
            .field("user_mode", &self.user_mode)
            .field("writable", &self.writable)
            .field("copy_on_write", &self.copy_on_write)
            .field("executable", &self.executable)
            .field("referenced", &self.referenced)
            .finish()
    }
}

/// safe abstraction layer for page directories. allows a consistent interface to page directories of multiple architectures
pub trait PageDirectory {
    const PAGE_SIZE: usize;

    /* -= Required functions -= */

    /// given a virtual address, get the page that contains it from this directory in a hardware agnostic form
    fn get_page(&self, addr: usize) -> Option<PageFrame>;

    /// insert a page frame into the directory
    ///
    /// # Arguments
    ///
    /// * `addr` - the virtual address to insert the page frame at
    /// * `page` - the page frame to insert
    fn set_page(&mut self, addr: usize, page: Option<PageFrame>) -> Result<(), PagingError>;

    /// switch the mmu to this page directory
    ///
    /// # Safety
    ///
    /// this function is unsafe since whatever code is being run currently could be different or nonexistent when switching pages, thus causing undefined behavior
    unsafe fn switch_to(&self);

    /* -= Non required functions =- */

    /// given an address, checks whether the page that contains it is unused and can be freely remapped
    fn is_unused(&self, addr: usize) -> bool {
        self.get_page(addr).is_none()
    }

    /// transforms the provided virtual address in this page directory into a physical address, if possible
    fn virt_to_phys(&self, virt: usize) -> Option<u64> {
        let page_size = Self::PAGE_SIZE - 1;
        let page_addr = virt & !page_size;
        let offset = virt & page_size;

        self.get_page(page_addr).map(|page| page.addr | offset as u64)
    }
}

/// when run on the current page directory, this function maps the range `addr..addr + len` from the page table given in `from`
/// to a region on the heap, then calls `op` with a reference to a slice of the mapped region. the region on the heap is then deallocated.
/// this function does not allocate new pages in the given page directory, and attempting to run it on a region which is not fully allocated
/// will return an error
///
/// # Arguments
///
/// * `from` - the page directory to map memory from. must be the same type as the one that this function is being called on
/// * `addr` - the starting address to map memory from
/// * `len` - how much memory to map, in bytes
/// * `op` - function to be called while memory is mapped
///
/// # Safety
///
/// this function is unsafe because it (at least in its default implementation) cannot guarantee that it's being called on the current
/// page directory, and things can and will break if it's called on any other page directory
pub unsafe fn map_memory_from<D: PageDirectory, O, R>(map_into: &mut D, from: &mut impl PageDirectory, addr: usize, len: usize, op: O) -> Result<R, PagingError>
where O: FnOnce(&mut [u8]) -> R {
    let page_size = D::PAGE_SIZE;

    // get starting and ending addresses
    let mut start = addr;
    let mut end = addr + len;

    assert!(end > start);

    debug!("mapping memory in ({start:#x} - {end:#x})");

    // offset into memory we've paged in
    let mut offset = 0;

    // align start and end addresses to page boundaries
    if start % page_size != 0 {
        start &= !(page_size - 1);
        offset = addr - start;
    }

    if end % page_size != 0 {
        end = (end & !(page_size - 1)) + (page_size - 1);
    }

    trace!("aligned to {start:#x} + {offset:#x} - {end:#x}, slice len is {len:#x}");

    let mut addresses: Vec<u64> = Vec::new();

    // attempt to safely reserve memory
    if let Err(err) = addresses.try_reserve_exact((end - start) / page_size) {
        error!("error reserving memory in map_memory_from(): {err:?}");

        return Err(PagingError::AllocError);
    }

    // get physical addresses of this region
    for i in (start..=end).step_by(page_size) {
        let phys_addr = match from.get_page(i) {
            Some(page) => {
                if !page.writable && page.copy_on_write && page.referenced {
                    copy_on_write(from, addr, page)?.addr
                } else {
                    page.addr
                }
            }
            None => {
                debug!("couldn't get phys addr for virt {i:#x}");

                return Err(PagingError::BadAddress);
            }
        };

        addresses.push(phys_addr);
    }

    trace!("addresses: {addresses:x?}");

    // map the memory
    map_memory(map_into, &addresses, |s| op(&mut s[offset..offset + len]))
}

/// maps the given physical addresses in order into a region of memory allocated on the heap, then calls `op` with a slice over all the mapped memory
///
/// # Arguments
///
/// * `addresses` - a list of physical addresses to map into memory in order
/// * `op` - function to be called while memory is mapped
///
/// # Safety
///
/// this function is unsafe because it (at least in its default implementation) cannot guarantee that it's being called on the current
/// page directory, and things can and will break if it's called on any other page directory
pub unsafe fn map_memory<D: PageDirectory, O, R>(map_into: &mut D, addresses: &[u64], op: O) -> Result<R, PagingError>
where O: FnOnce(&mut [u8]) -> R {
    let page_size = D::PAGE_SIZE;

    let buf_len = addresses.len() * page_size;

    // allocate memory for us to remap
    let layout = Layout::from_size_align(buf_len, page_size).unwrap();
    let ptr = alloc(layout);

    if ptr.is_null() {
        error!("error allocating buffer in map_memory()");
        return Err(PagingError::AllocError);
    }

    assert!(ptr as usize % page_size == 0); // make absolutely sure pointer is page aligned

    debug!("mapping {} pages to {:#x} (kernel mem)", addresses.len(), ptr as usize);

    // get addresses of pages we're gonna remap so we can map them back later
    let mut existing_phys: Vec<u64> = Vec::new();

    // attempt to safely reserve memory for our mapping
    if let Err(err) = existing_phys.try_reserve_exact(addresses.len()) {
        error!("error reserving memory in map_memory(): {err:?}");
        dealloc(ptr, layout);

        return Err(PagingError::AllocError);
    }

    for i in (ptr as usize..ptr as usize + buf_len).step_by(page_size) {
        // virt to phys calculation from current page directory
        let addr = match map_into.virt_to_phys(i) {
            Some(a) => a,
            None => {
                // something bad happened, revert back to original state and return an error
                debug!("aborting map (before remap), dealloc()ing");
                dealloc(ptr, layout);

                return Err(PagingError::BadAddress);
            }
        };
        trace!("existing: {i:#x} -> {addr:#x}");
        existing_phys.push(addr);
    }

    trace!("existing_phys: {existing_phys:x?}");

    // remap all pages in region
    for (i, phys_addr) in addresses.iter().enumerate() {
        let virt = ptr as usize + i * page_size;

        trace!("{virt:x} now @ phys addr: {phys_addr:x}");

        // todo: maybe change this to debug_assert at some point? its prolly hella slow
        assert!(!existing_phys.contains(phys_addr), "trampling on other page directory's memory");

        // remap memory
        map_into
            .set_page(
                virt,
                Some(PageFrame {
                    addr: *phys_addr,
                    present: true,
                    writable: true,
                    ..Default::default()
                }),
            )
            .expect("couldn't remap page");
    }

    trace!("slice @ {ptr:?}, len {buf_len:#x}");

    // call function
    let res = op(core::slice::from_raw_parts_mut(ptr as *mut u8, buf_len));

    // map pages back to their original addresses
    trace!("cleaning up mapping");
    for (idx, addr) in (ptr as usize..ptr as usize + buf_len).step_by(page_size).enumerate() {
        let phys_addr = existing_phys[idx];
        trace!("virt @ {addr:x}, phys @ {phys_addr:x}");
        map_into
            .set_page(
                addr,
                Some(PageFrame {
                    addr: phys_addr,
                    present: true,
                    writable: true,
                    ..Default::default()
                }),
            )
            .expect("couldn't remap page");
    }

    // deallocate the buffer
    dealloc(ptr, layout);

    Ok(res)
}

/// finds available area in this page directory's memory of given size. this area is guaranteed to be unused, unallocated, and aligned to a page boundary
///
/// # Arguments
///
/// * `start` - the lowest address this hole can be located at. useful to keep null pointers null. must be page aligned
/// * `end` - the highest address this hole can be located at. must be page aligned
/// * `size` - the size of the hole (automatically rounded up to the nearest multiple of the page size of this page directory)
pub fn find_hole<D: PageDirectory>(page_dir: &D, start: usize, end: usize, size: usize) -> Option<usize> {
    let page_size = D::PAGE_SIZE;

    assert!(start % page_size == 0, "start address is not page aligned");
    assert!(end % page_size == 0, "end address is not page aligned");

    let size = (size / page_size) * page_size + page_size;

    let mut hole_start: Option<usize> = None;

    for addr in (start..end).step_by(page_size) {
        if page_dir.is_unused(addr) {
            if let Some(start) = hole_start {
                if addr - start >= size {
                    return hole_start;
                }
            /*} else if size <= page_size && addr >= start {
            return Some(addr);*/
            } else if hole_start.is_none() && addr >= start {
                hole_start = Some(addr);
            }
        } else {
            hole_start = None;
        }
    }

    None
}

/// struct to make allocating physical memory for page directories easier
#[repr(C)]
pub struct PageManager {
    /// bitset to speed up allocation of page frames
    ///
    /// every bit in this set represents an individual page in the directory
    ///
    /// the size of this bitset can be calculated by dividing the address of the top of available memory by the system's page size
    pub frame_set: BitSet,

    /// the page size of this page manager
    pub page_size: usize,
}

impl PageManager {
    /// creates a new page manager with the provided bitset for available frames
    ///
    /// # Arguments
    ///
    /// * `frame_set` - a BitSet that stores which pages are available and which arent. should be created based on the system's memory map
    pub fn new(frame_set: BitSet, page_size: usize) -> Self {
        Self { frame_set, page_size }
    }

    /// allocates a frame in memory, returning its physical address without assigning it to any page directories
    pub fn alloc_frame(&mut self) -> Result<u64, PagingError> {
        if let Some(idx) = self.frame_set.first_unset() {
            self.frame_set.set(idx);

            Ok(idx as u64 * self.page_size as u64)
        } else {
            Err(PagingError::NoAvailableFrames)
        }
    }

    pub fn first_available_frame(&self) -> Option<u64> {
        self.frame_set.first_unset().map(|i| (i as u64) * (self.page_size as u64))
    }

    /// allocates a frame in the provided page directory at the given physical address, if available
    ///
    /// # Arguments
    ///
    /// * `dir` - the page directory to allocate the frame in
    /// * `addr` - the virtual address to allocate the frame at. must be page aligned
    /// * `phys` - the physical address to map the frame to. must also be page aligned
    /// * `user_mode` - whether the allocated page will be accessible in user mode
    /// * `writable` - whether the allocated page will be able to be written to
    pub fn alloc_frame_at<T: PageDirectory>(&mut self, dir: &mut T, addr: usize, phys: u64, user_mode: bool, writable: bool, executable: bool) -> Result<(), PagingError> {
        assert!(T::PAGE_SIZE == self.page_size);

        assert!(addr % self.page_size == 0, "frame address is not page aligned");
        assert!(phys % self.page_size as u64 == 0, "physical address is not page aligned");

        if dir.is_unused(addr) {
            let idx = phys / self.page_size as u64;

            let frame = PageFrame {
                addr: phys,
                present: true,
                user_mode,
                writable,
                executable,
                ..Default::default()
            };

            trace!("allocating frame {:?} @ {:#x}", frame, addr);

            self.frame_set.set(idx as usize);
            dir.set_page(addr, Some(frame))?;

            Ok(())
        } else {
            Err(PagingError::FrameInUse)
        }
    }

    /// sets a frame in our list of frames as used, preventing it from being allocated elsewhere
    ///
    /// # Arguments
    ///
    /// * `dir` - a page table, used to get page size
    /// * `addr` - the address of the frame
    pub fn set_frame_used(&mut self, addr: u64) {
        assert!(addr % self.page_size as u64 == 0, "frame address is not page aligned");

        let idx = (addr / self.page_size as u64).try_into().unwrap();
        debug!("setting {idx:#x} as used");
        self.frame_set.set(idx);

        debug!("first_unset is now {:?}", self.frame_set.first_unset());
    }

    /// sets a frame in our list of frames as free, allowing it to be allocated elsewhere
    ///
    /// # Arguments
    ///
    /// * `dir` - a page table, used to get page size
    /// * `addr` - the address of the frame
    pub fn set_frame_free(&mut self, addr: u64) {
        assert!(addr % self.page_size as u64 == 0, "frame address is not page aligned");

        self.frame_set.clear((addr / self.page_size as u64).try_into().unwrap());
    }

    /// frees a frame in the provided page directory, allowing that region of memory to be used by other things
    ///
    /// returns the frame's physical address if successful
    ///
    /// # Arguments
    ///
    /// * `dir` - the page directory to free the frame in
    /// * `addr` - the virtual address to free the frame at. must be page aligned
    pub fn free_frame<T: PageDirectory>(&mut self, dir: &mut T, addr: usize) -> Result<u64, PagingError> {
        assert!(T::PAGE_SIZE == self.page_size);

        assert!(addr % self.page_size == 0, "frame address is not page aligned");

        if let Some(page) = dir.get_page(addr) {
            trace!("freeing phys {:#x}", page.addr);

            self.frame_set.clear((page.addr / self.page_size as u64) as usize);
            dir.set_page(addr, None)?;

            Ok(page.addr)
        } else {
            Err(PagingError::FrameUnused)
        }
    }

    /// prints out information about this page directory
    pub fn print_free(&self) {
        let bits_used = self.frame_set.bits_used;
        let size = self.frame_set.size;
        debug!("{}/{} mapped ({}% usage)", bits_used, size, (bits_used * 100) / size);
    }

    /// sets all the pages mapped in the given page directory to used in this PageManager, so that no future allocations use the same memory
    ///
    /// note: this is slow! very slow! this should be done as infrequently as possible
    pub fn sync_from_dir<T: PageDirectory>(&mut self, dir: &T) {
        assert!(T::PAGE_SIZE == self.page_size);

        // iterate over all virtual addresses
        for i in (0..=usize::MAX).step_by(self.page_size) {
            if dir.get_page(i).is_some() {
                //info!("got page @ {:#x}", i);
                self.frame_set.set(i / self.page_size);
            }
        }
    }
}

/// our kernel-wide page manager instance
static mut PAGE_MANAGER: Option<Mutex<PageManager>> = None;

/// gets the global page manager, locked with a spinlock
pub fn get_page_manager() -> MutexGuard<'static, PageManager> {
    unsafe {
        let manager = PAGE_MANAGER.as_ref().expect("page manager not initialized");

        let thread_id = crate::arch::get_thread_id();
        let mut has_warned = false;

        loop {
            match manager.try_lock() {
                Some(guard) => return guard,
                None => {
                    if !has_warned {
                        debug!("warning (cpu {thread_id}): page manager is locked");
                        has_warned = true;
                    }

                    // if the page manager is locked, we're likely waiting for a page update
                    // we can try to process urgent messages here in case we're waiting for this cpu to handle a page update so we don't deadlock
                    crate::task::get_cpus().expect("CPUs not initialized").get_thread(thread_id).unwrap().process_urgent_messages();
                }
            }
        }
    }
}

/// sets the global page manager. can only be called once
pub fn set_page_manager(manager: PageManager) {
    unsafe {
        if PAGE_MANAGER.is_some() {
            panic!("can't initialize pagemanager twice");
        } else {
            PAGE_MANAGER = Some(Mutex::new(manager));
        }
    }
}

static mut KERNEL_PAGE_DIR: Option<Mutex<PageDirTracker<crate::arch::PageDirectory<'static>>>> = None;

pub fn get_kernel_page_dir() -> MutexedPageDir<'static, PageDirTracker<crate::arch::PageDirectory<'static>>> {
    unsafe { MutexedPageDir(KERNEL_PAGE_DIR.as_ref().expect("kernel page directory not set")) }
}

pub fn set_kernel_page_dir(dir: crate::arch::PageDirectory<'static>) {
    unsafe {
        if KERNEL_PAGE_DIR.is_some() {
            panic!("can't set kernel page directory twice");
        } else {
            KERNEL_PAGE_DIR = Some(Mutex::new(PageDirTracker::new(dir, true)));
        }
    }
}

#[derive(Debug)]
pub enum ProcessOrKernelPageDir {
    Process(u32),
    Kernel,
}

impl PageDirectory for ProcessOrKernelPageDir {
    const PAGE_SIZE: usize = crate::arch::PageDirectory::PAGE_SIZE;

    fn get_page(&self, addr: usize) -> Option<PageFrame> {
        match self {
            Self::Process(id) => crate::task::get_process(*id).unwrap().page_directory.get_page(addr),
            Self::Kernel => get_kernel_page_dir().get_page(addr),
        }
    }

    fn set_page(&mut self, addr: usize, page: Option<PageFrame>) -> Result<(), PagingError> {
        match self {
            Self::Process(id) => crate::task::get_process(*id).unwrap().page_directory.set_page(addr, page),
            Self::Kernel => get_kernel_page_dir().set_page(addr, page),
        }
    }

    unsafe fn switch_to(&self) {
        match self {
            Self::Process(id) => crate::task::get_process(*id).unwrap().page_directory.switch_to(),
            Self::Kernel => get_kernel_page_dir().switch_to(),
        }
    }

    fn is_unused(&self, addr: usize) -> bool {
        match self {
            Self::Process(id) => crate::task::get_process(*id).unwrap().page_directory.is_unused(addr),
            Self::Kernel => get_kernel_page_dir().is_unused(addr),
        }
    }

    fn virt_to_phys(&self, virt: usize) -> Option<u64> {
        match self {
            Self::Process(id) => crate::task::get_process(*id).unwrap().page_directory.virt_to_phys(virt),
            Self::Kernel => get_kernel_page_dir().virt_to_phys(virt),
        }
    }
}

pub fn get_page_dir(thread_id: Option<crate::task::cpu::ThreadID>) -> ProcessOrKernelPageDir {
    if let Some(cpus) = crate::task::get_cpus() {
        let thread = cpus.get_thread(thread_id.unwrap_or_else(crate::arch::get_thread_id)).expect("couldn't get CPU thread");

        if let Some(current) = thread.task_queue.lock().current() {
            ProcessOrKernelPageDir::Process(current.id().process)
        } else {
            ProcessOrKernelPageDir::Kernel
        }
    } else {
        ProcessOrKernelPageDir::Kernel
    }
}

/// allows for easy reference counting of copy-on-write pages and memory mappings
pub struct PageRefCounter {
    references: BTreeMap<u64, PageReference>,
}

impl PageRefCounter {
    pub fn new() -> Self {
        Self { references: BTreeMap::default() }
    }

    pub fn add_reference(&mut self, phys: u64) {
        self.add_references(phys, 1);
    }

    pub fn add_references(&mut self, phys: u64, num: usize) {
        if let Some(reference) = self.references.get_mut(&phys) {
            reference.references += num;
        } else {
            self.references.insert(phys, PageReference { references: num, phys });
        }
    }

    pub fn remove_reference_no_free(&mut self, phys: u64) {
        if let Some(reference) = self.references.get_mut(&phys) {
            if reference.references > 1 {
                reference.references -= 1;
            }
        }
    }

    pub fn remove_reference(&mut self, phys: u64) {
        if let Some(reference) = self.references.get_mut(&phys) {
            if reference.references > 1 {
                reference.references -= 1;
            } else {
                debug!("no more references, freeing {phys:#x}");
                self.references.remove(&phys);
                get_page_manager().set_frame_free(phys);
            }
        } else {
            debug!("no references, freeing {phys:#x}");
            get_page_manager().set_frame_free(phys);
        }
    }

    pub fn remove_all_references(&mut self, phys: u64) {
        if self.references.contains_key(&phys) {
            debug!("removing all references to {phys:#x}");
            self.references.remove(&phys);
        }
    }

    pub fn get_references_for(&self, phys: u64) -> usize {
        if let Some(reference) = self.references.get(&phys) {
            reference.references
        } else {
            0
        }
    }
}

impl Default for PageRefCounter {
    fn default() -> Self {
        Self::new()
    }
}

/// used to keep track of references to a copied page
#[derive(Debug)]
pub struct PageReference {
    /// how many references to this page exist
    pub references: usize,

    /// physical address of the page this references
    pub phys: u64,
}

lazy_static! {
    pub static ref PAGE_REF_COUNTER: Mutex<PageRefCounter> = Mutex::new(PageRefCounter::new());
}

/// manages freeing pages allocated for process page directories
#[repr(transparent)]
pub struct FreeablePageDir<D: PageDirectory>(D);

impl<D: PageDirectory> PageDirectory for FreeablePageDir<D> {
    const PAGE_SIZE: usize = D::PAGE_SIZE;

    fn get_page(&self, addr: usize) -> Option<PageFrame> {
        self.0.get_page(addr)
    }

    fn set_page(&mut self, addr: usize, page: Option<PageFrame>) -> Result<(), PagingError> {
        self.0.set_page(addr, page)
    }

    unsafe fn switch_to(&self) {
        self.0.switch_to()
    }

    fn is_unused(&self, addr: usize) -> bool {
        self.0.is_unused(addr)
    }

    fn virt_to_phys(&self, virt: usize) -> Option<u64> {
        self.0.virt_to_phys(virt)
    }
}

impl<D: PageDirectory> Drop for FreeablePageDir<D> {
    fn drop(&mut self) {
        free_page_dir(&self.0);
    }
}

impl<D: PageDirectory> FreeablePageDir<D> {
    pub fn new(dir: D) -> Self {
        Self(dir)
    }

    pub fn into_inner(self) -> D {
        unsafe {
            // this effectively duplicates the value, however we forget self right after, so it should be fine?
            let res = core::ptr::read(&self.0);
            core::mem::forget(self);
            res
        }
    }
}

/// given a page frame, free its contents or otherwise clean them up with the proper method
pub fn free_page(page: PageFrame) {
    if page.shared {
        if !super::shared::free_shared_reference(page.addr) {
            PAGE_REF_COUNTER.lock().remove_reference(page.addr);
        }
    } else if page.referenced {
        PAGE_REF_COUNTER.lock().remove_reference(page.addr);
    } else {
        get_page_manager().set_frame_free(page.addr);
    }
}

/// frees all pages in the provided page directory
pub fn free_page_dir<D: PageDirectory>(dir: &D) {
    for addr in (0..crate::arch::KERNEL_PAGE_DIR_SPLIT).step_by(D::PAGE_SIZE) {
        if let Some(page) = dir.get_page(addr) {
            free_page(page);
        }
    }
}

/// given a page directory, address, and the page frame at that address, copy its contents to a new page and replace the existing page with the new one, freeing the old page in the process
pub fn copy_on_write(page_dir: &mut impl PageDirectory, addr: usize, mut page: PageFrame) -> Result<PageFrame, PagingError> {
    let page_size = crate::arch::PageDirectory::PAGE_SIZE;

    if PAGE_REF_COUNTER.lock().get_references_for(page.addr) > 1 {
        debug!("copying page {addr:#x} (phys {:#x})", page.addr);

        unsafe {
            let copied_layout = Layout::from_size_align(page_size, page_size).unwrap();
            let copied_area = alloc(copied_layout);

            let copied_slice = core::slice::from_raw_parts_mut(copied_area, page_size);
            let copybara = core::slice::from_raw_parts_mut(addr as *mut u8, page_size);

            trace!("copying");
            copied_slice.copy_from_slice(copybara);

            let original_page = page;

            {
                match page_dir.virt_to_phys(copied_area as usize) {
                    Some(new) => page.addr = new,
                    None => {
                        dealloc(copied_area, copied_layout);

                        return Err(PagingError::BadAddress);
                    }
                }
                page.writable = true;
                page.copy_on_write = false;
                page.referenced = false;

                trace!("updating page");
                if let Err(err) = page_dir.set_page(addr, Some(page)) {
                    dealloc(copied_area, copied_layout);

                    return Err(err);
                }
            }

            trace!("cleaning up");

            // allocate a new page for the heap
            trace!("allocating new page");
            let phys_addr = match get_page_manager().alloc_frame() {
                Ok(addr) => addr,
                Err(err) => {
                    page_dir.set_page(addr, Some(original_page)).expect("copy on write cleanup failed");
                    dealloc(copied_area, copied_layout);

                    return Err(err);
                }
            };

            let new_page = PageFrame {
                addr: phys_addr,
                present: true,
                writable: true,
                ..Default::default()
            };

            trace!("replacing new page");
            page_dir.set_page(copied_area as usize, Some(new_page)).expect("couldn't set page in copy on write cleanup"); // if we can't set this page we're fucked tbqh

            trace!("freeing area");
            dealloc(copied_area, copied_layout);

            free_page(original_page);

            trace!("copied");

            Ok(new_page)
        }
    } else {
        debug!("page {addr:#x} (phys {:#x}) isn't referenced by anything else, not copying", page.addr);

        // we can just update writable here, keeping the copy on write flag set means it'll be deallocated thru the page reference counter
        page.writable = true;

        page_dir.set_page(addr, Some(page))?;

        Ok(page)
    }
}

/// used in page fault exception handlers to check whether to copy on write and do so if required
///
/// returns true if a copy was successful and false if it's not marked for copy on write
pub fn try_copy_on_write(thread: &crate::task::cpu::CPUThread, addr: usize) -> Result<bool, Errno> {
    let current_id = thread.task_queue.lock().current().ok_or(Errno::NoSuchProcess)?.id();

    let page = crate::task::get_process(current_id.process)
        .ok_or(Errno::NoSuchProcess)?
        .page_directory
        .get_page(addr)
        .ok_or(Errno::BadAddress)?;

    let page_size = crate::arch::PageDirectory::PAGE_SIZE;

    // round down to nearest multiple of page size
    let addr = (addr / page_size) * page_size;

    if !page.writable && page.copy_on_write && page.referenced {
        copy_on_write(&mut ProcessOrKernelPageDir::Process(current_id.process), addr, page)?;

        Ok(true)
    } else {
        Ok(false)
    }
}

pub fn validate_region(page_dir: &impl PageDirectory, start: usize, len: usize) -> bool {
    let page_size = crate::arch::PageDirectory::PAGE_SIZE;
    let start = (start / page_size) * page_size;
    let end = ((start + len) / page_size) * page_size + page_size;

    for addr in (start..end).step_by(page_size) {
        if page_dir.get_page(addr).is_none() {
            return false;
        }
    }

    true
}

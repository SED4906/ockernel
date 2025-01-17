pub mod bootloader;
pub mod logger;

use crate::{
    arch::{
        paging::{PageDir, PageTable},
        PAGE_SIZE,
    },
    mm::{
        bump_alloc::{bump_alloc, init_bump_alloc},
        heap::ALLOCATOR,
        paging::{get_page_manager, set_page_manager, PageDirectory, PageFrame, PageManager},
    },
    util::{
        abi::ABI,
        array::BitSet,
        debug::DebugArray,
        tar::{EntryKind, TarIterator},
    },
};
use alloc::{
    alloc::Layout,
    boxed::Box,
    collections::BTreeMap,
    format,
    string::{String, ToString},
    vec::Vec,
};
use compression::prelude::*;
use core::{arch::asm, mem::size_of};
use log::{debug, error, info};

pub const LINKED_BASE: usize = 0xe0000000;
pub const HEAP_START: usize = LINKED_BASE + 0x01000000;
pub const KHEAP_INITIAL_SIZE: usize = 0x100000;
pub const KHEAP_MAX_SIZE: usize = 0xffff000;
pub const HEAP_MIN_SIZE: usize = 0x70000;

pub const PLATFORM_ABI: ABI = ABI::Fastcall;

//static mut PAGE_MANAGER: Option<PageManager<PageDir>> = None;
static mut PAGE_DIR: Option<PageDir> = None;

static mut BOOTSTRAP_ADDR: u64 = 0;

extern "C" {
    /// located at end of loader, used for more efficient memory mappings
    static kernel_end: u8;

    /// base of the stack, used to map out the page below to catch stack overflow
    static stack_base: u8;

    /// top of the stack
    static stack_end: u8;

    /// base of the interrupt handler stack
    static int_stack_base: u8;

    /// top of the interrupt handler stack
    static int_stack_end: u8;
}

/// initialize paging, just cleanly map our kernel to 3.5gb
#[no_mangle]
pub extern "C" fn x86_prep_page_table(buf: &mut [u32; 1024]) {
    for i in 0u32..1024 {
        buf[i as usize] = i * PAGE_SIZE as u32 + 3;
    }

    buf[((unsafe { (&int_stack_base as *const _) as usize } - LINKED_BASE) / PAGE_SIZE) - 1] = 0;
    buf[((unsafe { (&stack_base as *const _) as usize } - LINKED_BASE) / PAGE_SIZE) - 1] = 0;
}

/// gets the physical address for bootstrap code for other cpus
pub fn get_cpu_bootstrap_addr() -> u64 {
    unsafe { BOOTSTRAP_ADDR }
}

#[no_mangle]
pub fn kmain() {
    logger::init().unwrap();

    unsafe {
        bootloader::pre_init();
    }

    unsafe {
        crate::arch::ints::init();
        crate::arch::gdt::init((&int_stack_end as *const _) as u32);
    }

    let kernel_end_pos = unsafe { (&kernel_end as *const _) as usize };
    let stack_base_pos = unsafe { (&stack_base as *const _) as usize };
    let stack_end_pos = unsafe { (&stack_end as *const _) as usize };
    let int_stack_base_pos = unsafe { (&int_stack_base as *const _) as usize };
    let int_stack_end_pos = unsafe { (&int_stack_end as *const _) as usize };

    // === multiboot pre-init ===

    let mem_size = bootloader::init();
    let mem_size_pages: usize = (mem_size / PAGE_SIZE as u64).try_into().unwrap();

    // === paging init ===

    // initialize the bump allocator so we can allocate initial memory for paging
    unsafe {
        init_bump_alloc(LINKED_BASE);
    }

    // initialize the pagemanager to manage our page allocations
    set_page_manager(PageManager::new(
        {
            let layout = Layout::new::<u32>();
            let ptr = unsafe {
                bump_alloc::<u32>(Layout::from_size_align(mem_size_pages / 32 * layout.size(), layout.align()).unwrap())
                    .unwrap()
                    .pointer
            };
            let mut bitset = BitSet::place_at(ptr, mem_size_pages);
            bitset.clear_all();
            bootloader::reserve_pages(&mut bitset);
            bitset
        },
        PAGE_SIZE,
    ));

    // page directory for kernel
    let mut page_dir = PageDir::bump_allocate();

    {
        // grab a reference to the page manager so we don't have to continuously lock and unlock it while we're doing initial memory allocations
        let mut manager = get_page_manager();

        let kernel_start = LINKED_BASE + 0x100000;

        // allocate pages
        debug!("mapping kernel ({kernel_start:#x} - {kernel_end_pos:#x})");

        for addr in (kernel_start..kernel_end_pos).step_by(PAGE_SIZE) {
            if !page_dir.has_page_table(addr.try_into().unwrap()) {
                debug!("allocating new page table");
                let ptr = unsafe { bump_alloc::<PageTable>(Layout::from_size_align(size_of::<PageTable>(), PAGE_SIZE).unwrap()).unwrap() };
                page_dir.add_page_table(addr.try_into().unwrap(), unsafe { &mut *ptr.pointer }, ptr.phys_addr.try_into().unwrap(), false);
            }

            manager.alloc_frame_at(&mut page_dir, addr, (addr - LINKED_BASE) as u64, false, true, true).unwrap();
        }

        // free the page below the stack, to catch stack overflow
        debug!("stack @ {stack_base_pos:#x} - {stack_end_pos:#x}");
        manager.free_frame(&mut page_dir, stack_base_pos - PAGE_SIZE).unwrap();

        debug!("interrupt stack @ {int_stack_base_pos:#x} - {int_stack_end_pos:#x}");
        manager.free_frame(&mut page_dir, int_stack_base_pos - PAGE_SIZE).unwrap();

        // set aside some memory for bootstrapping other CPUs
        //let bootstrap_addr = manager.first_available_frame().unwrap();
        let bootstrap_addr = 0x1000;
        manager.set_frame_used(bootstrap_addr);

        debug!("bootstrap code @ {bootstrap_addr:#x}");

        unsafe {
            BOOTSTRAP_ADDR = bootstrap_addr;
        }

        let heap_init_end = HEAP_START + HEAP_MIN_SIZE;
        debug!("mapping heap ({HEAP_START:#x} - {heap_init_end:#x})");

        for addr in (HEAP_START..heap_init_end).step_by(PAGE_SIZE) {
            if !page_dir.has_page_table(addr.try_into().unwrap()) {
                debug!("allocating new page table");
                let ptr = unsafe { bump_alloc::<PageTable>(Layout::from_size_align(size_of::<PageTable>(), PAGE_SIZE).unwrap()).unwrap() };
                page_dir.add_page_table(addr.try_into().unwrap(), unsafe { &mut *ptr.pointer }, ptr.phys_addr.try_into().unwrap(), false);
            }

            let phys_addr = manager.alloc_frame().unwrap();

            page_dir
                .set_page(
                    addr,
                    Some(PageFrame {
                        addr: phys_addr,
                        present: true,
                        writable: true,
                        ..Default::default()
                    }),
                )
                .unwrap();
        }
    }

    // switch to our new page directory so all the pages we've just mapped will be accessible
    unsafe {
        // if we don't set this as global state something breaks, haven't bothered figuring out what yet
        PAGE_DIR = Some(page_dir);

        PAGE_DIR.as_ref().unwrap().switch_to();
    }

    // === heap init ===

    // set up allocator with minimum size
    ALLOCATOR.init(HEAP_START, HEAP_MIN_SIZE);

    crate::arch::init_alloc();

    unsafe {
        crate::mm::bump_alloc::free_unused_bump_alloc(&mut get_page_manager(), PAGE_DIR.as_mut().unwrap());
    }

    get_page_manager().print_free();

    // === enable interrupts ===

    unsafe {
        asm!("sti");
    }

    // === multiboot init after heap init ===

    unsafe {
        bootloader::init_after_heap(&mut get_page_manager(), PAGE_DIR.as_mut().unwrap());
    }

    let info = bootloader::get_multiboot_info();

    debug!("{info:?}");

    // === discover modules ===

    if info.mods.is_none() || info.mods.as_ref().unwrap().is_empty() {
        panic!("no modules found, cannot continue booting");
    }

    let bootloader_modules = info.mods.as_ref().unwrap();

    let mut modules: BTreeMap<String, &'static [u8]> = BTreeMap::new();

    fn discover_module(modules: &mut BTreeMap<String, &'static [u8]>, name: String, data: &'static [u8]) {
        debug!("found module {name:?}: {:?}", DebugArray(data));

        match name.split('.').last() {
            Some("tar") => {
                info!("discovering all files in {name:?} as modules");

                for entry in TarIterator::new(data) {
                    if entry.header.kind() == EntryKind::NormalFile {
                        discover_module(modules, entry.header.name().to_string(), entry.contents);
                    }
                }
            }
            Some("bz2") => {
                // remove the extension from the name of the compressed file
                let new_name = {
                    let mut split: Vec<&str> = name.split('.').collect();
                    split.pop();
                    split.join(".")
                };

                info!("decompressing {name:?} as {new_name:?}");

                match data.iter().cloned().decode(&mut BZip2Decoder::new()).collect::<Result<Vec<_>, _>>() {
                    // Box::leak() prevents the decompressed data from being dropped, giving it the 'static lifetime since it doesn't
                    // contain any references to anything else
                    Ok(decompressed) => discover_module(modules, new_name, Box::leak(decompressed.into_boxed_slice())),
                    Err(err) => error!("error decompressing {name}: {err:?}"),
                }
            }
            Some("gz") => {
                let new_name = {
                    let mut split: Vec<&str> = name.split('.').collect();
                    split.pop();
                    split.join(".")
                };

                info!("decompressing {name:?} as {new_name:?}");

                match data.iter().cloned().decode(&mut GZipDecoder::new()).collect::<Result<Vec<_>, _>>() {
                    Ok(decompressed) => discover_module(modules, new_name, Box::leak(decompressed.into_boxed_slice())),
                    Err(err) => error!("error decompressing {name}: {err:?}"),
                }
            }
            // no special handling for this file, assume it's a module
            _ => {
                modules.insert(name, data);
            }
        }
    }

    for module in bootloader_modules.iter() {
        discover_module(&mut modules, module.string().to_string(), module.data());
    }

    // === print module info ===

    let mut num_modules = 0;
    let mut max_len = 0;
    for (name, _) in modules.iter() {
        num_modules += 1;
        if name.len() > max_len {
            max_len = name.len();
        }
    }

    if num_modules == 1 {
        info!("1 module:");
    } else {
        info!("{num_modules} modules:");
    }

    for (name, data) in modules.iter() {
        let size = if data.len() > 1024 * 1024 * 10 {
            format!("{} MB", data.len() / 1024 / 1024)
        } else if data.len() > 1024 * 10 {
            format!("{} KB", data.len() / 1024)
        } else {
            format!("{} B", data.len())
        };
        info!("\t{name:max_len$} : {size}");
    }

    get_page_manager().print_free();

    // === parse command line ===
    let cmdline = bootloader::get_multiboot_info().cmdline.filter(|s| !s.is_empty()).map(|cmdline| {
        let mut map = BTreeMap::new();

        for arg in cmdline.split(' ') {
            if !arg.is_empty() {
                let arg = arg.split('=').collect::<Vec<_>>();
                map.insert(arg[0], arg.get(1).copied().unwrap_or(""));
            }
        }

        map
    });

    debug!("{:?}", cmdline);

    // set the global kernel page directory
    crate::mm::paging::set_kernel_page_dir(unsafe { PAGE_DIR.take().unwrap() });

    // add shared memory area for video memory
    let start = 0xb8000;
    let end = start + 32 * 1024;
    let mut temp = crate::mm::shared::TempMemoryShare::new(Default::default(), start, end - 1).unwrap();
    for i in (start..end).step_by(PAGE_SIZE) {
        temp.add_reserved(i as u64);
    }
    temp.share(Default::default()).unwrap();

    // arch code takes over here
    crate::arch::init(cmdline, modules);

    loop {
        crate::arch::halt_until_interrupt();
    }
}

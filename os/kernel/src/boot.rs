use crate::interrupt::interrupt_dispatcher;
use crate::syscall::syscall_dispatcher;
use crate::thread::thread::Thread;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::ToString;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::fmt::Arguments;
use core::mem::size_of;
use core::ops::Deref;
use core::panic::PanicInfo;
use core::ptr;
use chrono::DateTime;
use log::{debug, error, info, Level, Log, Record};
use multiboot2::{BootInformation, BootInformationHeader, EFIMemoryMapTag, MemoryAreaType, MemoryMapTag, Tag};
use uefi::prelude::*;
use uefi::table::boot::{MemoryMap, PAGE_SIZE};
use uefi::table::Runtime;
use uefi_raw::table::boot::MemoryType;
use x86_64::instructions::interrupts;
use x86_64::instructions::segmentation::{Segment, CS, DS, ES, FS, GS, SS};
use x86_64::instructions::tables::load_tss;
use x86_64::{PhysAddr, VirtAddr};
use x86_64::registers::segmentation::SegmentSelector;
use x86_64::structures::gdt::Descriptor;
use x86_64::structures::paging::{Page, PageTableFlags, PhysFrame};
use x86_64::PrivilegeLevel::Ring0;
use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::structures::paging::frame::PhysFrameRange;
use x86_64::structures::paging::page::PageRange;
use crate::{allocator, efi_system_table, gdt, init_acpi_tables, init_apic, init_efi_system_table, init_keyboard, init_serial_port, init_terminal, logger, memory, ps2_devices, scheduler, serial_port, terminal, terminal_initialized, timer, tss};
use crate::memory::MemorySpace;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    if terminal_initialized() {
        println!("Panic: {}", info);
    } else {
        let record = Record::builder()
            .level(Level::Error)
            .file(Some("panic"))
            .args(*info.message().unwrap_or(&Arguments::new_const(&["A panic occurred!"])))
            .build();

        unsafe { logger().force_unlock() };
        let log = logger().lock();
        unsafe { logger().force_unlock() }; // log() also calls logger().lock()
        log.log(&record);
    }

    loop {}
}

pub mod built_info {
    // The file has been placed there by the build script.
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

extern "C" {
    static ___KERNEL_DATA_START__: u64;
    static ___KERNEL_DATA_END__: u64;
}

const INIT_HEAP_PAGES: usize = 0x400;

#[no_mangle]
pub extern "C" fn start(multiboot2_magic: u32, multiboot2_addr: *const BootInformationHeader) {
    // Initialize logger
    if logger().lock().init().is_err() {
        panic!("Failed to initialize logger!")
    }

    // Log messages and panics are now working, but cannot use format string until the heap is initialized later on
    info!("Welcome to hhuTOSr early boot environment!");

    // Get multiboot information
    if multiboot2_magic != multiboot2::MAGIC {
        panic!("Invalid Multiboot2 magic number!");
    }

    let multiboot = unsafe { BootInformation::load(multiboot2_addr).expect("Failed to get Multiboot2 information!") };

    let mut heap_region = PhysFrameRange { start: PhysFrame::from_start_address(PhysAddr::zero()).unwrap(), end: PhysFrame::from_start_address(PhysAddr::zero()).unwrap() };
    let bootloader_memory_regions: Vec<PhysFrameRange>;

    // Search memory map, provided by bootloader of EFI, for usable memory
    // and initialize kernel heap, after which format strings may be used in logs and panics.
    if let Some(_) = multiboot.efi_bs_not_exited_tag() {
        // EFI boot services have not been exited and we obtain access to the memory map and EFI runtime services by exiting them manually
        info!("EFI boot services have not been exited");
        let image_tag = multiboot.efi_ih64_tag().expect("EFI image handle not available!");
        let sdt_tag = multiboot.efi_sdt64_tag().expect("EFI system table not available!");
        let image_handle;
        let system_table;

        unsafe {
            image_handle = Handle::from_ptr(image_tag.image_handle() as *mut c_void).expect("Failed to create EFI image handle struct from pointer!");
            system_table = SystemTable::<Boot>::from_ptr(sdt_tag.sdt_address() as *mut c_void).expect("Failed to create EFI system table struct from pointer!");
            system_table.boot_services().set_image_handle(image_handle);
        }

        info!("Exiting EFI boot services to obtain runtime system table and memory map");
        let (runtime_table, memory_map) = system_table.exit_boot_services(MemoryType::LOADER_DATA);

        bootloader_memory_regions = scan_efi_memory_map(&memory_map, &mut heap_region);
        init_efi_system_table(runtime_table);
    } else {
        info!("EFI boot services have been exited");
        if let Some(memory_map) = multiboot.efi_memory_map_tag() {
            // EFI services have been exited, but the bootloader has provided us with the EFI memory map
            info!("Bootloader provides EFI memory map");
            bootloader_memory_regions = scan_efi_multiboot2_memory_map(memory_map, &mut heap_region);
        } else if let Some(memory_map) = multiboot.memory_map_tag() {
            // EFI services have been exited, but the bootloader has provided us with a Multiboot2 memory map
            info!("Bootloader provides Multiboot2 memory map");
            bootloader_memory_regions = scan_multiboot2_memory_map(memory_map, &mut heap_region);
        } else {
            panic!("No memory information available!");
        }
    }

    // Setup global descriptor table
    // Has to be done after EFI boot services have been exited, since they rely on their own GDT
    info!("Initializing GDT");
    init_gdt();

    // The bootloader marks the kernel image region as available, so we need to check for regions overlapping
    // with the kernel image and temporary heap and build a new memory map with the kernel image and heap cut out.
    // Furthermore, we need to make sure, that no region starts at address 0, to avoid null pointer panics.
    let null_region = PhysFrameRange { start: PhysFrame::from_start_address(PhysAddr::zero()).unwrap(), end: PhysFrame::from_start_address(PhysAddr::new(PAGE_SIZE as u64)).unwrap() };
    let mut available_memory_regions = cut_region(bootloader_memory_regions, null_region);
    available_memory_regions = cut_region(available_memory_regions, kernel_image_region());
    available_memory_regions = cut_region(available_memory_regions, heap_region);

    // Initialize physical memory management
    info!("Initializing page frame allocator");
    unsafe { memory::physical::init(available_memory_regions, heap_region.end); }

    // Initialize virtual memory management
    info!("Initializing paging");
    let address_space = memory::r#virtual::create_address_space();
    unsafe { Cr3::write(address_space.read().page_table_address(), Cr3Flags::empty()) };

    // Initialize serial port and enable serial logging
    init_serial_port();
    if let Some(serial) = serial_port() {
        logger().lock().register(serial);
    }

    // Initialize terminal and enable terminal logging
    let fb_info = multiboot.framebuffer_tag()
        .expect("No framebuffer information provided by bootloader!")
        .expect("Unknown framebuffer type!");

    let fb_start_page = Page::from_start_address(VirtAddr::new(fb_info.address())).expect("Framebuffer address is not page aligned!");
    let fb_end_page = Page::from_start_address(VirtAddr::new(fb_info.address() + (fb_info.height() * fb_info.pitch()) as u64).align_up(PAGE_SIZE as u64)).unwrap();
    address_space.write().map(PageRange { start: fb_start_page, end: fb_end_page }, MemorySpace::Kernel, PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE | PageTableFlags::NO_CACHE);

    init_terminal(fb_info.address() as *mut u8, fb_info.pitch(), fb_info.width(), fb_info.height(), fb_info.bpp());
    logger().lock().register(terminal());

    info!("Welcome to hhuTOSr!");
    let version = format!("v{} ({} - O{})", built_info::PKG_VERSION, built_info::PROFILE, built_info::OPT_LEVEL);
    let git_ref = built_info::GIT_HEAD_REF.unwrap_or("Unknown");
    let git_commit = built_info::GIT_COMMIT_HASH_SHORT.unwrap_or("Unknown");
    let build_date = match DateTime::parse_from_rfc2822(built_info::BUILT_TIME_UTC) {
        Ok(date_time) => date_time.format("%Y-%m-%d %H:%M:%S").to_string(),
        Err(_) => "Unknown".to_string(),
    };
    let bootloader_name = match multiboot.boot_loader_name_tag() {
        Some(tag) => if tag.name().is_ok() { tag.name().unwrap_or("Unknown") } else { "Unknown" },
        None => "Unknown",
    };

    info!("OS Version: [{}]", version);
    info!("Git Version: [{} - {}]", built_info::GIT_HEAD_REF.unwrap_or_else(|| "Unknown"), git_commit);
    info!("Build Date: [{}]", build_date);
    info!("Compiler: [{}]", built_info::RUSTC_VERSION);
    info!("Bootloader: [{}]", bootloader_name);

    // Initialize ACPI tables
    let rsdp_addr: usize = if let Some(rsdp_tag) = multiboot.rsdp_v2_tag() {
        ptr::from_ref(rsdp_tag) as usize + size_of::<Tag>()
    } else if let Some(rsdp_tag) = multiboot.rsdp_v1_tag() {
        ptr::from_ref(rsdp_tag) as usize + size_of::<Tag>()
    } else {
        panic!("ACPI not available!");
    };

    init_acpi_tables(rsdp_addr);

    // Initialize interrupts
    info!("Initializing IDT");
    interrupt_dispatcher::setup_idt();
    info!("Initializing system calls");
    syscall_dispatcher::init();
    init_apic();

    // Initialize timer
    {
        info!("Initializing timer");
        let mut timer = timer().write();
        timer.interrupt_rate(1);
        timer.plugin();
    }

    // Enable interrupts
    info!("Enabling interrupts");
    interrupts::enable();

    // Initialize EFI runtime service (if available and not done already during memory initialization)
    if efi_system_table().is_none() {
        if let Some(sdt_tag) = multiboot.efi_sdt64_tag() {
            info!("Initializing EFI runtime services");
            let system_table = unsafe { SystemTable::<Runtime>::from_ptr(sdt_tag.sdt_address() as *mut c_void) };
            if system_table.is_some() {
                init_efi_system_table(system_table.unwrap());
            } else {
                error!("Failed to create EFI system table struct from pointer!");
            }
        }
    }

    if let Some(system_table) = efi_system_table() {
        info!("EFI runtime services available (Vendor: [{}], UEFI version: [{}])", system_table.firmware_vendor(), system_table.uefi_revision());
    }

    // Initialize keyboard
    info!("Initializing PS/2 devices");
    init_keyboard();
    ps2_devices().keyboard().plugin();

    // Enable serial port interrupts
    if let Some(serial) = serial_port() {
        serial.plugin();
    }

    let scheduler = scheduler();
    scheduler.ready(Thread::new_kernel_thread(Box::new(|| {
        let terminal = terminal();
        terminal.write_str("> ");

        loop {
            match terminal.read_byte() {
                -1 => panic!("Terminal input stream closed!"),
                0x0a => terminal.write_str("> "),
                _ => {}
            }
        }
    })));

    // Disable terminal logging
    logger().lock().remove(terminal());
    terminal().clear();

    println!(include_str!("banner.txt"), version, git_ref.rsplit("/").next().unwrap_or(git_ref), git_commit, build_date,
             built_info::RUSTC_VERSION.split_once("(").unwrap_or((built_info::RUSTC_VERSION, "")).0.trim(), bootloader_name);

    info!("Starting scheduler");
    scheduler.start();
}

fn init_kernel_heap(heap_region: &PhysFrameRange) {
    info!("Initializing kernel heap");
    unsafe { allocator().init(heap_region); }
    debug!("Kernel heap is initialized (Start: [{} KiB], End: [{} KiB]])", heap_region.start.start_address().as_u64() / 1024, heap_region.end.start_address().as_u64() / 1024);
}

fn init_gdt() {
    let mut gdt = gdt().lock();
    let tss = tss().lock();

    gdt.add_entry(Descriptor::kernel_code_segment());
    gdt.add_entry(Descriptor::kernel_data_segment());
    gdt.add_entry(Descriptor::user_data_segment());
    gdt.add_entry(Descriptor::user_code_segment());

    unsafe {
        // We need to obtain a static reference to the TSS and GDT for the following operations.
        // We know, that they have a static lifetime, since they are declared as static variables in 'kernel/mod.rs'.
        // However, since they are hidden behind a Mutex, the borrow checker does not see them with a static lifetime.
        let gdt_ref = ptr::from_ref(gdt.deref()).as_ref().unwrap();
        let tss_ref = ptr::from_ref(tss.deref()).as_ref().unwrap();
        gdt.add_entry(Descriptor::tss_segment(tss_ref));
        gdt_ref.load();
    }

    unsafe {
        // Load task state segment
        load_tss(SegmentSelector::new(5, Ring0));

        // Set code and stack segment register
        CS::set_reg(SegmentSelector::new(1, Ring0));
        SS::set_reg(SegmentSelector::new(2, Ring0));

        // Other segment registers are not used in long mode (set to 0)
        DS::set_reg(SegmentSelector::new(0, Ring0));
        ES::set_reg(SegmentSelector::new(0, Ring0));
        FS::set_reg(SegmentSelector::new(0, Ring0));
        GS::set_reg(SegmentSelector::new(0, Ring0));
    }
}

fn kernel_image_region() -> PhysFrameRange {
    let start: PhysFrame;
    let end: PhysFrame;

    unsafe {
        start = PhysFrame::from_start_address(PhysAddr::new(ptr::from_ref(&___KERNEL_DATA_START__) as u64)).expect("Kernel code is not page aligned!");
        end = PhysFrame::from_start_address(PhysAddr::new(ptr::from_ref(&___KERNEL_DATA_END__) as u64).align_up(PAGE_SIZE as u64)).unwrap();
    }

    return PhysFrameRange { start, end };
}

fn scan_efi_memory_map(memory_map: &MemoryMap, heap_region: &mut PhysFrameRange) -> Vec<PhysFrameRange> {
    info!("Searching memory map for region usable for kernel heap");
    let kernel_region = kernel_image_region();
    let heap_area = memory_map.entries()
        .filter(|area| (area.ty == MemoryType::CONVENTIONAL || area.ty == MemoryType::LOADER_CODE || area.ty == MemoryType::LOADER_DATA
            || area.ty == MemoryType::BOOT_SERVICES_CODE || area.ty == MemoryType::BOOT_SERVICES_DATA)
            && area.page_count >= INIT_HEAP_PAGES as u64 && area.phys_start >= kernel_region.end.start_address().as_u64())
        .min_by(|area1, area2| area1.phys_start.cmp(&area2.phys_start))
        .expect("Failed to find memory region usable for kernel heap!");

    heap_region.start = PhysFrame::from_start_address(PhysAddr::new(heap_area.phys_start)).unwrap();
    heap_region.end = heap_region.start + INIT_HEAP_PAGES as u64;
    init_kernel_heap(heap_region);

    info!("Searching memory map for available regions");
    let mut regions: Vec<PhysFrameRange> = Vec::new();
    memory_map.entries()
        .filter(|area| area.ty == MemoryType::CONVENTIONAL || area.ty == MemoryType::LOADER_CODE || area.ty == MemoryType::LOADER_DATA
            || area.ty == MemoryType::BOOT_SERVICES_CODE || area.ty == MemoryType::BOOT_SERVICES_DATA)
        .for_each(|area| {
            let start = PhysFrame::from_start_address(PhysAddr::new(area.phys_start).align_up(PAGE_SIZE as u64)).unwrap();
            regions.push(PhysFrameRange { start, end: start + area.page_count });
        });

    return regions;
}

fn scan_efi_multiboot2_memory_map(memory_map: &EFIMemoryMapTag, heap_region: &mut PhysFrameRange) -> Vec<PhysFrameRange> {
    info!("Searching memory map for region usable for kernel heap");
    let kernel_region = kernel_image_region();
    let heap_area = memory_map.memory_areas().filter(|area|
        (area.ty.0 == MemoryType::CONVENTIONAL.0 || area.ty.0 == MemoryType::LOADER_CODE.0 || area.ty.0 == MemoryType::LOADER_DATA.0
            || area.ty.0 == MemoryType::BOOT_SERVICES_CODE.0 || area.ty.0 == MemoryType::BOOT_SERVICES_DATA.0) // .0 necessary because of different version dependencies to uefi-crate
            && area.page_count >= INIT_HEAP_PAGES as u64 && area.phys_start >= kernel_region.end.start_address().as_u64())
        .min_by(|area1, area2| area1.phys_start.cmp(&area2.phys_start))
        .expect("Failed to find memory region usable for kernel heap!");

    heap_region.start = PhysFrame::from_start_address(PhysAddr::new(heap_area.phys_start)).unwrap();
    heap_region.end = heap_region.start + INIT_HEAP_PAGES as u64;
    init_kernel_heap(heap_region);

    info!("Searching memory map for available regions");
    let mut regions: Vec<PhysFrameRange> = Vec::new();
    memory_map.memory_areas()
        .filter(|area| area.ty.0 == MemoryType::CONVENTIONAL.0 || area.ty.0 == MemoryType::LOADER_CODE.0 || area.ty.0 == MemoryType::LOADER_DATA.0
            || area.ty.0 == MemoryType::BOOT_SERVICES_CODE.0 || area.ty.0 == MemoryType::BOOT_SERVICES_DATA.0) // .0 necessary because of different version dependencies to uefi-crate
        .for_each(|area| {
            let start = PhysFrame::from_start_address(PhysAddr::new(area.phys_start).align_up(PAGE_SIZE as u64)).unwrap();
            regions.push(PhysFrameRange { start, end: start + area.page_count });
        });

    return regions;
}

fn scan_multiboot2_memory_map(memory_map: &MemoryMapTag, heap_region: &mut PhysFrameRange) -> Vec<PhysFrameRange> {
    info!("Searching memory map for region usable for kernel heap");
    let kernel_region = kernel_image_region();
    let heap_area = memory_map.memory_areas().iter().filter(|area|
        area.typ() == MemoryAreaType::Available && area.size() / PAGE_SIZE as u64 >= INIT_HEAP_PAGES as u64 && area.start_address() >= kernel_region.end.start_address().as_u64())
        .min_by(|area1, area2| area1.start_address().cmp(&area2.start_address()))
        .expect("Failed to find memory region usable for kernel heap!");

    heap_region.start = PhysFrame::from_start_address(PhysAddr::new(heap_area.start_address()).align_up(PAGE_SIZE as u64)).unwrap();
    heap_region.end = heap_region.start + INIT_HEAP_PAGES as u64;
    init_kernel_heap(heap_region);

    info!("Searching memory map for available regions");
    let mut regions: Vec<PhysFrameRange> = Vec::new();
    memory_map.memory_areas().iter()
        .filter(|area| area.typ() == MemoryAreaType::Available)
        .for_each(|area| {
            regions.push(PhysFrameRange {
                start: PhysFrame::from_start_address(PhysAddr::new(area.start_address()).align_up(PAGE_SIZE as u64)).unwrap(),
                end: PhysFrame::from_start_address(PhysAddr::new(area.end_address()).align_down(PAGE_SIZE as u64)).unwrap()
            });
        });

    return regions;
}

fn cut_region(regions: Vec<PhysFrameRange>, reserved_region: PhysFrameRange) -> Vec<PhysFrameRange>{
    let mut new_regions: Vec<PhysFrameRange> = Vec::new();

    for region in regions {
        if region.start < reserved_region.start && region.end >= reserved_region.start { // Region starts below the reserved region
            if region.end <= reserved_region.end { // Region starts below and ends inside the reserved region
                new_regions.push(PhysFrameRange { start: region.start, end: reserved_region.start });
            } else { // Regions starts below and ends above the kernel image
                new_regions.push(PhysFrameRange { start: region.start, end: reserved_region.start }); // Region below reserved region
                new_regions.push(PhysFrameRange { start: reserved_region.end, end: region.end }); // Region above reserved region
            }
        } else if region.start <= reserved_region.end && region.end >= reserved_region.start { // Region starts within the reserved region
            if region.end <= reserved_region.end { // Regions start within and ends within the reserved region
                continue
            } else { // Region starts within and ends above the reserved region
                new_regions.push(PhysFrameRange { start: reserved_region.end, end: region.end });
            }
        } else { // Region does not interfere with the reserved region
            new_regions.push(region);
        }
    }

    return new_regions;
}
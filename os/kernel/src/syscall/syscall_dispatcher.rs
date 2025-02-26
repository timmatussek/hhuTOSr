use core::arch::asm;
use x86_64::registers::control::{Efer, EferFlags};
use x86_64::registers::model_specific::{LStar, Star};
use x86_64::structures::gdt::SegmentSelector;
use x86_64::{PrivilegeLevel, VirtAddr};
use library_syscall::NUM_SYSCALLS;
use crate::syscall::{sys_thread_exit, sys_thread_sleep, sys_thread_switch};


pub fn init() {
    // Enable system call extensions
    unsafe { Efer::update(|flags| flags.set(EferFlags::SYSTEM_CALL_EXTENSIONS, true)) }

    // Set code and stack segments for syscall
    let cs_syscall = SegmentSelector::new(1, PrivilegeLevel::Ring0);
    let ss_syscall = SegmentSelector::new(2, PrivilegeLevel::Ring0);
    let cs_sysret = SegmentSelector::new(4, PrivilegeLevel::Ring3);
    let ss_sysret = SegmentSelector::new(3, PrivilegeLevel::Ring3);

    if let Err(err) = Star::write(cs_sysret, ss_sysret, cs_syscall, ss_syscall) {
        panic!(
            "System Call: Failed to initialize STAR register (Error: {})",
            err
        )
    }

    // Set rip for syscall
    LStar::write(VirtAddr::new(syscall_handler as u64));
}

#[no_mangle]
pub static SYSCALL_TABLE: SyscallTable = SyscallTable::new();

#[repr(align(64))]
#[repr(C)]
pub struct SyscallTable {
    handle: [*const usize; NUM_SYSCALLS],
}

impl SyscallTable {
    pub const fn new() -> Self {
        SyscallTable {
            handle: [
                sys_thread_switch as *const _,
                sys_thread_sleep as *const _,
                sys_thread_exit as *const _,
            ],
        }
    }
}

unsafe impl Send for SyscallTable {}
unsafe impl Sync for SyscallTable {}

#[naked]
#[no_mangle]
// This functions does not take any parameters per its declaration,
// but in reality, it takes at least the system call ID in rax
// and may take additional parameters for the system call in rdi, rsi and rdx.
unsafe extern "C" fn syscall_handler() {
    asm!(
    // We are now in ring 0, but still on the user stack
    // Disable interrupts until we have switched to kernel stack
    "cli",

    // Save registers (except rax, which is used for system call ID and return value)
    "push rbx",
    "push rcx", // Contains rip for returning to ring 3
    "push rdx",
    "push rdi",
    "push rsi",
    "push r8",
    "push r9",
    "push r10",
    "push r11", // Contains eflags for returning to ring 3
    "push r12",
    "push r13",
    "push r14",
    "push r15",

    // Switch to kernel stack and enable interrupts
    "mov r15, rax", // Save system call ID in r15
    "mov r14, rdi", // Save first parameter in r14
    "mov r13, rsi", // Save second parameter in r13
    "mov r12, rdx", // Save third parameter in r12
    "call tss_get_rsp0", // Get kernel rsp (returned in rax)
    "mov rbx, rax", // Save kernel rsp in rbx
    "mov rcx, rsp", // Save user rsp in rcx
    "mov rdx, r12", // Restore third parameter
    "mov rsi, r13", // Restore second parameter
    "mov rdi, r14", // Restore first parameter
    "mov rax, r15", // Restore system call ID
    "mov rsp, rbx", // Switch to kernel stack
    "push rcx", // Save user rsp on stack
    "sti",

    // Check if system call ID is in bounds
    "cmp rax, {}",
    "jge syscall_abort", // Panics and does not return

    // Call system call handler, corresponding to ID (in rax)
    "call syscall_disp",

    // Switch to user stack (user rsp is last value on stack)
    // Disable interrupts, since we are still in Ring 0 and no interrupt handler should be called with the user stack
    "cli",
    "pop rsp",

    // Restore registers
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop r11", // Contains eflags for returning to ring 3
    "pop r10",
    "pop r9",
    "pop r8",
    "pop rsi",
    "pop rdi",
    "pop rdx",
    "pop rcx", // Contains rip for returning to ring 3
    "pop rbx",

    // Return to Ring 3
    // Interrupts will be enabled automatically, because eflags gets restored from r11
    "sysretq",
    const NUM_SYSCALLS,
    options(noreturn)
    );
}

#[no_mangle]
#[naked]
unsafe extern "C" fn syscall_disp() {
    asm!(
    "call [{SYSCALL_TABLE} + 8 * rax]",
    "ret",
    SYSCALL_TABLE = sym SYSCALL_TABLE,
    options(noreturn)
    );
}
#[no_mangle]
unsafe extern "C" fn syscall_abort() {
    let syscall_number: u64;

    asm!(
    "mov {}, rax", out(reg) syscall_number
    );

    panic!("System call with id [{}] does not exist!", syscall_number);
}

#![no_std]
#![no_main]

#[repr(usize)]
enum SyscallId {
    Sleep = 10,
    Write = 64,
}

#[inline(always)]
fn syscall(id: SyscallId, arg0: usize) -> usize {
    let ret: usize;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("a7") id as usize => ret,
            in("a0") arg0,
        );
    }
    ret
}

fn sleep_sec(sec: usize) -> usize {
    syscall(SyscallId::Sleep, sec)
}

fn puts(s: &[u8]) {
    syscall(SyscallId::Write, s.as_ptr() as usize);
}

#[no_mangle]
fn main() -> ! {
    puts(b"[User C] going to sleep 5 seconds...\n");
    sleep_sec(5);
    puts(b"[User C] wake up!\n");
    loop {}
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

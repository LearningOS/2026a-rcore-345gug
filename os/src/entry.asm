    .section .text.entry
    .globl _start
_start:
    # a0 holds the hart id passed by RustSBI; keep it in tp
    mv tp, a0
    la sp, boot_stack_top
    call rust_main

    .globl secondary_entry
secondary_entry:
    # a0 holds the hart id passed by SBI HSM
    mv tp, a0
    li t0, 16
    sll t0, a0, t0
    la sp, secondary_stack_top
    sub sp, sp, t0
    call secondary_main

    .section .bss.stack
    .globl boot_stack_lower_bound
boot_stack_lower_bound:
    .space 4096 * 16
    .globl boot_stack_top
boot_stack_top:

    .globl secondary_stack_lower_bound
secondary_stack_lower_bound:
    .space 4096 * 16 * 8
    .globl secondary_stack_top
secondary_stack_top:

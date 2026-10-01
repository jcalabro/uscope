// Indirect jumps, calls, and returns in each operand form whose target the
// debugger reads at a stop. A label names each indirect branch so tests can
// stop on it, and each target returns its own bit so the result shows that
// every branch reached it: disasm_indirect(1) returns 0x3f.

// Pointers stored in memory, which need relocations in a position-independent
// executable.
    .section .data.rel.ro, "aw"
    .p2align 3

// A function pointer, like a global offset table entry.
    .globl disasm_slot
    .type disasm_slot, @object
disasm_slot:
    .quad disasm_target_one
    .size disasm_slot, . - disasm_slot

// A table of functions called through its address in a register, like a
// C++ virtual table.
    .globl disasm_methods
    .type disasm_methods, @object
disasm_methods:
    .quad disasm_target_one
    .quad disasm_target_two
    .size disasm_methods, . - disasm_methods

// A jump table.
    .globl disasm_cases
    .type disasm_cases, @object
disasm_cases:
    .quad disasm_case_zero
    .quad disasm_case_one
    .size disasm_cases, . - disasm_cases

// A thread-local function pointer.
    .section .tbss, "awT", @nobits
    .p2align 3
    .type disasm_thread_slot, @tls_object
disasm_thread_slot:
    .zero 8
    .size disasm_thread_slot, . - disasm_thread_slot

    .text

    .p2align 4
    .globl disasm_target_one
    .type disasm_target_one, @function
disasm_target_one:
    .cfi_startproc
    movl $0x1, %eax
    ret
    .cfi_endproc
    .size disasm_target_one, . - disasm_target_one

    .p2align 4
    .globl disasm_target_two
    .type disasm_target_two, @function
disasm_target_two:
    .cfi_startproc
    movl $0x2, %eax
    ret
    .cfi_endproc
    .size disasm_target_two, . - disasm_target_two

    .p2align 4
    .globl disasm_target_four
    .type disasm_target_four, @function
disasm_target_four:
    .cfi_startproc
    movl $0x4, %eax
    ret
    .cfi_endproc
    .size disasm_target_four, . - disasm_target_four

// int disasm_indirect(int selector), where selector indexes the jump table.
    .p2align 4
    .globl disasm_indirect
    .type disasm_indirect, @function
disasm_indirect:
    .cfi_startproc
    pushq %rbx
    .cfi_def_cfa_offset 16
    .cfi_offset %rbx, -16
    pushq %r12
    .cfi_def_cfa_offset 24
    .cfi_offset %r12, -24
    // Aligns the stack for calls.
    subq $8, %rsp
    .cfi_def_cfa_offset 32
    movl %edi, %r12d
    xorl %ebx, %ebx

// Memory that the instruction alone addresses.
disasm_call_slot:
    call *disasm_slot(%rip)
    orl %eax, %ebx

// Memory addressed by a register.
    leaq disasm_methods(%rip), %rax
disasm_call_method:
    call *8(%rax)
    orl %eax, %ebx

// A register.
    leaq disasm_target_four(%rip), %rcx
disasm_call_register:
    call *%rcx
    orl %eax, %ebx

// Thread-local storage, addressed relative to the FS segment base.
    leaq disasm_target_two(%rip), %rax
    movq %rax, %fs:disasm_thread_slot@tpoff
disasm_call_thread:
    call *%fs:disasm_thread_slot@tpoff
    shll $3, %eax
    orl %eax, %ebx

// A global offset table entry the dynamic loader fills at startup, called
// without a procedure linkage table stub. labs(-8) is 8.
    movq $-8, %rdi
disasm_call_got:
    call *labs@GOTPCREL(%rip)
    orl %eax, %ebx

// A jump table indexed by a register.
    leaq disasm_cases(%rip), %rdx
    movslq %r12d, %rax
disasm_jump_table:
    jmp *(%rdx,%rax,8)
disasm_case_zero:
    orl $0x40, %ebx
    jmp 1f
disasm_case_one:
    orl $0x20, %ebx
1:
    movl %ebx, %eax
    addq $8, %rsp
    .cfi_def_cfa_offset 24
    popq %r12
    .cfi_def_cfa_offset 16
    popq %rbx
    .cfi_def_cfa_offset 8
disasm_return:
    ret
    .cfi_endproc
    .size disasm_indirect, . - disasm_indirect

// Never executed: branches whose targets the debugger does not compute, and
// one through memory that is never mapped.
    .p2align 4
    .globl disasm_indirect_forms
    .type disasm_indirect_forms, @function
disasm_indirect_forms:
    .cfi_startproc
    jmp *0
    ljmp *(%rax)
    // jmp *%rax on Intel processors, but jmp *%ax on AMD ones.
    .byte 0x66, 0xff, 0xe0
    iretq
    .cfi_endproc
    .size disasm_indirect_forms, . - disasm_indirect_forms

    .section .note.GNU-stack, "", @progbits

// Code that jumps over two bytes of data. The bytes begin a ten-byte movabs,
// so decoding straight through from a function's start swallows the code the
// jump reaches. The tests depend on the exact byte layout: a two-byte jump,
// the two data bytes, then `mov $1, %eax` at offset 4.

    .text

// A code symbol marks where execution resumes.
    .p2align 4
    .globl disasm_marked_data
    .type disasm_marked_data, @function
disasm_marked_data:
    .cfi_startproc
    jmp 1f
    .byte 0x48, 0xb8
    .globl disasm_marked_resume
    .type disasm_marked_resume, @function
disasm_marked_resume:
1:
    movl $1, %eax
    ret
    .cfi_endproc
    .size disasm_marked_resume, . - disasm_marked_resume
    .size disasm_marked_data, . - disasm_marked_data

// Nothing marks where execution resumes.
    .p2align 4
    .globl disasm_hidden_data
    .type disasm_hidden_data, @function
disasm_hidden_data:
    .cfi_startproc
    jmp 1f
    .byte 0x48, 0xb8
1:
    movl $1, %eax
    ret
    .cfi_endproc
    .size disasm_hidden_data, . - disasm_hidden_data

    .section .note.GNU-stack, "", @progbits

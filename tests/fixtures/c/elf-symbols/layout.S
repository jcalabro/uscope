// Hand-written code whose exact ELF symbol layout the symbolization tests
// depend on. Every function has call-frame information but no debug info.
// Each entry point takes a callback in %rdi and calls it with an aligned stack.

    .text

// A sized global function.
    .p2align 4
    .globl asm_sized
    .type asm_sized, @function
asm_sized:
    .cfi_startproc
    subq $8, %rsp
    .cfi_adjust_cfa_offset 8
    call *%rdi
    addq $8, %rsp
    .cfi_adjust_cfa_offset -8
    ret
    .cfi_endproc
    .size asm_sized, . - asm_sized

// A global function without a declared size.
    .p2align 4
    .globl asm_unsized
    .type asm_unsized, @function
asm_unsized:
    .cfi_startproc
    subq $8, %rsp
    .cfi_adjust_cfa_offset 8
    call *%rdi
    addq $8, %rsp
    .cfi_adjust_cfa_offset -8
    ret
    .cfi_endproc

// An unsized global leaf without call-frame information, so only the next
// function can bound it.
    .p2align 4
    .globl asm_unsized_leaf
    .type asm_unsized_leaf, @function
asm_unsized_leaf:
    ret

// A local function that bounds asm_unsized_leaf. Stripping removes its symbol
// but not its call-frame entry, which must still bound asm_unsized_leaf.
    .p2align 4
    .type asm_local_after_unsized, @function
asm_local_after_unsized:
    .cfi_startproc
    ret
    .cfi_endproc
    .size asm_local_after_unsized, . - asm_local_after_unsized

// A sized function containing a smaller sized function. The callback is
// called once before, once within, and once after the inner symbol.
    .p2align 4
    .globl asm_nested_outer
    .type asm_nested_outer, @function
asm_nested_outer:
    .cfi_startproc
    pushq %rbx
    .cfi_adjust_cfa_offset 8
    .cfi_offset %rbx, -16
    movq %rdi, %rbx
    call *%rbx
    .globl asm_nested_inner
    .type asm_nested_inner, @function
asm_nested_inner:
    call *%rbx
    nop
    .size asm_nested_inner, . - asm_nested_inner
    call *%rbx
    popq %rbx
    .cfi_adjust_cfa_offset -8
    .cfi_restore %rbx
    ret
    .cfi_endproc
    .size asm_nested_outer, . - asm_nested_outer

// Four names for one function. Binding and leading underscores must select
// asm_alias_global even though every other name sorts before it.
    .p2align 4
    .globl asm_alias_global
    .type asm_alias_global, @function
    .globl __asm_alias_global
    .type __asm_alias_global, @function
    .weak asm_alias_a_weak
    .type asm_alias_a_weak, @function
    .type asm_alias_0_local, @function
asm_alias_global:
__asm_alias_global:
asm_alias_a_weak:
asm_alias_0_local:
    .cfi_startproc
    subq $8, %rsp
    .cfi_adjust_cfa_offset 8
    call *%rdi
    addq $8, %rsp
    .cfi_adjust_cfa_offset -8
    ret
    .cfi_endproc
    .size asm_alias_global, . - asm_alias_global
    .size __asm_alias_global, . - __asm_alias_global
    .size asm_alias_a_weak, . - asm_alias_a_weak
    .size asm_alias_0_local, . - asm_alias_0_local

// An indirect function's resolver, also named by an ordinary function symbol.
// The ordinary symbol names the code even though asm_indirect sorts first.
// Nothing references asm_indirect, so the loader never runs the resolver.
    .p2align 4
    .globl asm_resolver_impl
    .type asm_resolver_impl, @function
    .globl asm_indirect
    .type asm_indirect, @gnu_indirect_function
asm_resolver_impl:
asm_indirect:
    .cfi_startproc
    subq $8, %rsp
    .cfi_adjust_cfa_offset 8
    call *%rdi
    addq $8, %rsp
    .cfi_adjust_cfa_offset -8
    xorl %eax, %eax
    ret
    .cfi_endproc
    .size asm_resolver_impl, . - asm_resolver_impl
    .size asm_indirect, . - asm_indirect

// A sized function that calls code no symbol names.
    .p2align 4
    .globl asm_gap_entry
    .type asm_gap_entry, @function
asm_gap_entry:
    .cfi_startproc
    subq $8, %rsp
    .cfi_adjust_cfa_offset 8
    call .Lgap_body
    addq $8, %rsp
    .cfi_adjust_cfa_offset -8
    ret
    .cfi_endproc
    .size asm_gap_entry, . - asm_gap_entry

// Code described only by its call-frame entry.
    .p2align 4
.Lgap_body:
    .cfi_startproc
    subq $8, %rsp
    .cfi_adjust_cfa_offset 8
    call *%rdi
    addq $8, %rsp
    .cfi_adjust_cfa_offset -8
    ret
    .cfi_endproc

// A function whose last instruction is a call, so its return address is the
// first byte of the next symbol. asm_after_noreturn completes the frame when
// the callback returns and must never be called directly.
    .p2align 4
    .globl asm_noreturn_caller
    .type asm_noreturn_caller, @function
asm_noreturn_caller:
    .cfi_startproc
    subq $8, %rsp
    .cfi_adjust_cfa_offset 8
    call *%rdi
    .cfi_endproc
    .size asm_noreturn_caller, . - asm_noreturn_caller
    .globl asm_after_noreturn
    .type asm_after_noreturn, @function
asm_after_noreturn:
    .cfi_startproc
    .cfi_def_cfa_offset 16
    addq $8, %rsp
    .cfi_def_cfa_offset 8
    ret
    .cfi_endproc
    .size asm_after_noreturn, . - asm_after_noreturn

// Data objects that address descriptions name only within declared sizes: a
// sized object containing a smaller one, an object without a size, and
// zero-initialized storage.
    .data
    .p2align 4
    .globl asm_data_outer
    .type asm_data_outer, @object
asm_data_outer:
    .quad 1
    .globl asm_data_inner
    .type asm_data_inner, @object
asm_data_inner:
    .quad 2
    .size asm_data_inner, . - asm_data_inner
    .quad 3
    .size asm_data_outer, . - asm_data_outer
    .globl asm_data_unsized
    .type asm_data_unsized, @object
asm_data_unsized:
    .quad 4

    .bss
    .p2align 4
    .globl asm_bss_object
    .type asm_bss_object, @object
asm_bss_object:
    .zero 32
    .size asm_bss_object, . - asm_bss_object

    .section .note.GNU-stack, "", @progbits

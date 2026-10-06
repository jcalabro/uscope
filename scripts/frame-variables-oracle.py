# Records gdb's view of every frame's variables in a core dump, for
# differential tests of frame selection. Run inside gdb:
#
#   USCOPE_FRAME_ORACLE=ORACLE gdb -batch -x scripts/frame-variables-oracle.py PROGRAM CORE
#
# The oracle is line-oriented and tab-separated:
#
#   thread  <lwp>
#   frame   <level>  <function or empty>  <scope|noscope>
#   var     <name>   <argument|local>  <state>  [value]
#   member  <path>   <state>  [value]
#
# A frame's variables are those of its innermost block outward to its
# function, innermost first, so the first of several equal names is the one
# a lookup finds. A state is `int` (a decimal integer, pointer, enumerator,
# boolean, or character), `float` (Python's repr of the value), `optimized-out`,
# `error`, `struct` for a C or C++ structure, whose members follow, or `other`
# for values the tests do not compare. gdb calls a structure optimized out
# when any of its bits are, so each member is judged by its own: a member's
# path is an expression naming it from the variable, such as `pair.first`.

import os

import gdb

SCALAR_CODES = {
    gdb.TYPE_CODE_INT,
    gdb.TYPE_CODE_PTR,
    gdb.TYPE_CODE_ENUM,
    gdb.TYPE_CODE_BOOL,
    gdb.TYPE_CODE_CHAR,
}


# Zig describes itself as C, so a frame's language is its source's.
C_SOURCES = (".c", ".cc", ".cpp", ".cxx", ".h", ".hpp")


def describe(value, c_source):
    """A value's state and text, and its members' lines, if it has any."""
    try:
        code = value.type.strip_typedefs().code
        if code == gdb.TYPE_CODE_STRUCT and c_source:
            return ("struct", "", list(members(value)))
        if value.is_optimized_out:
            return ("optimized-out", "", [])
        if code in SCALAR_CODES:
            return ("int", str(int(value)), [])
        if code == gdb.TYPE_CODE_FLT:
            return ("float", repr(float(value)), [])
        # Reading the value proves it is readable.
        value.fetch_lazy()
        return ("other", "", [])
    except gdb.error:
        return ("error", "", [])


def members(value):
    """(path, state, text) for each named member of a structure, depth first."""
    for field in value.type.strip_typedefs().fields():
        if not field.name or field.is_base_class or not hasattr(field, "bitpos"):
            continue
        state, text, nested = describe(value[field], True)
        yield (field.name, state, text)
        for path, state, text in nested:
            yield (f"{field.name}.{path}", state, text)


def frame_lines(frame, level):
    name = frame.name() or ""
    try:
        block = frame.block()
    except RuntimeError:
        return [f"frame\t{level}\t{name}\tnoscope"]
    lines = [f"frame\t{level}\t{name}\tscope"]
    symtab = frame.find_sal().symtab
    c_source = symtab is not None and symtab.filename.endswith(C_SOURCES)
    while block is not None:
        for symbol in block:
            if not (symbol.is_argument or symbol.is_variable):
                continue
            kind = "argument" if symbol.is_argument else "local"
            try:
                value = symbol.value(frame)
            except gdb.error:
                lines.append(f"var\t{symbol.name}\t{kind}\terror\t")
                continue
            state, text, nested = describe(value, c_source)
            lines.append(f"var\t{symbol.name}\t{kind}\t{state}\t{text}")
            for path, state, text in nested:
                lines.append(f"member\t{symbol.name}.{path}\t{state}\t{text}")
        if block.function is not None:
            break
        block = block.superblock
    return lines


def main():
    gdb.execute("set pagination off")
    lines = ["uscope-frame-variables-oracle-v2"]
    threads = sorted(gdb.selected_inferior().threads(), key=lambda thread: thread.ptid[1])
    for thread in threads:
        thread.switch()
        lines.append(f"thread\t{thread.ptid[1]}")
        frame = gdb.newest_frame()
        level = 0
        while frame is not None:
            lines.extend(frame_lines(frame, level))
            try:
                frame = frame.older()
            except gdb.error:
                frame = None
            level += 1
    with open(os.environ["USCOPE_FRAME_ORACLE"], "w", encoding="utf-8") as oracle:
        oracle.write("\n".join(lines) + "\n")


main()

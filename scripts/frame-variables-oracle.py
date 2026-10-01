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
#
# A frame's variables are those of its innermost block outward to its
# function, innermost first, so the first of several equal names is the one
# a lookup finds. A state is `int` (a decimal integer, pointer, enumerator,
# boolean, or character), `float` (Python's repr of the value), `optimized-out`,
# `error`, or `other` for values the tests do not compare.

import os

import gdb

SCALAR_CODES = {
    gdb.TYPE_CODE_INT,
    gdb.TYPE_CODE_PTR,
    gdb.TYPE_CODE_ENUM,
    gdb.TYPE_CODE_BOOL,
    gdb.TYPE_CODE_CHAR,
}


def describe(symbol, frame):
    try:
        value = symbol.value(frame)
        if value.is_optimized_out:
            return ("optimized-out", "")
        code = value.type.strip_typedefs().code
        if code in SCALAR_CODES:
            return ("int", str(int(value)))
        if code == gdb.TYPE_CODE_FLT:
            return ("float", repr(float(value)))
        # Reading the value proves it is readable.
        value.fetch_lazy()
        return ("other", "")
    except gdb.error:
        return ("error", "")


def frame_lines(frame, level):
    name = frame.name() or ""
    try:
        block = frame.block()
    except RuntimeError:
        return [f"frame\t{level}\t{name}\tnoscope"]
    lines = [f"frame\t{level}\t{name}\tscope"]
    while block is not None:
        for symbol in block:
            if not (symbol.is_argument or symbol.is_variable):
                continue
            kind = "argument" if symbol.is_argument else "local"
            state, value = describe(symbol, frame)
            lines.append(f"var\t{symbol.name}\t{kind}\t{state}\t{value}")
        if block.function is not None:
            break
        block = block.superblock
    return lines


def main():
    gdb.execute("set pagination off")
    lines = ["uscope-frame-variables-oracle-v1"]
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

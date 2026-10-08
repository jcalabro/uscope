# Reduces `readelf --debug-dump=info` output to the coroutines rustc
# describes, an oracle uscope's own reading of them is compared with. Run
# it with the dump as both of its inputs: the first pass names each entry
# by offset, the second writes one tab-separated line per state:
#
#   <path::name>  <state member offset>  <state number>  <state record>
#   <declared line>  <awaited future's type, or ->
#
# A coroutine is a structure named `{async_fn_env#N}`, `{async_block_env#N}`,
# or `{async_closure_env#N}`; its path is the namespaces enclosing it.

function header(line, parts) {
    if (substr(line, 1, 2) != " <" \
        || !match(line, /^ <[0-9]+><[0-9a-f]+>: Abbrev Number: [0-9]+/)) {
        return 0
    }
    split(substr(line, 3), parts, /[<>]/)
    next_depth = parts[1] + 0
    next_offset = parts[3]
    next_tag = "null"
    if (match(line, /\(DW_TAG_[a-z_]+\)$/)) {
        next_tag = substr(line, RSTART + 1, RLENGTH - 2)
    }
    return 1
}

# An attribute's value, without the form or string offset readelf may
# write before it.
function value(line) {
    sub(/^[^:]*: /, "", line)
    sub(/^\([a-z0-9_]+\) /, "", line)
    sub(/^\(((indirect|indexed) (line )?string, )?offset: 0x[0-9a-f]+\): /, "", line)
    sub(/^\(indexed string: 0x[0-9a-f]+\): /, "", line)
    return line
}

function reference(line) {
    if (match(line, /<0x[0-9a-f]+>/)) {
        return substr(line, RSTART + 3, RLENGTH - 4)
    }
    return ""
}

# Takes in the entry whose attributes were just read.
function finish(    path, d) {
    if (depth == "") {
        return
    }
    tags[depth] = tag
    names[depth] = name
    if (FNR == NR) {
        if (name != "") {
            named[offset] = name
        }
        if (tag == "DW_TAG_member" && name == "__awaitee" && depth > 0) {
            awaitee[parents[depth - 1]] = type
        }
        parents[depth] = offset
        return
    }
    if (coroutine != "" && depth <= coroutine_depth) {
        coroutine = ""
    }
    if (tag == "DW_TAG_structure_type" && name ~ /^\{async_(fn|block|closure)_env#[0-9]+\}/) {
        path = ""
        for (d = 0; d < depth; d++) {
            if (tags[d] == "DW_TAG_namespace") {
                path = path names[d] "::"
            }
        }
        coroutine = path name
        coroutine_depth = depth
        state_offset = "?"
    } else if (coroutine != "") {
        if (tag == "DW_TAG_member" && depth == coroutine_depth + 2 && name == "__state") {
            state_offset = location
        } else if (tag == "DW_TAG_variant" && depth == coroutine_depth + 2) {
            discriminant = discr
        } else if (tag == "DW_TAG_member" && depth == coroutine_depth + 3) {
            awaited = "-"
            if (type in awaitee && awaitee[type] in named) {
                awaited = named[awaitee[type]]
            }
            printf "%s\t%s\t%s\t%s\t%s\t%s\n", coroutine, state_offset, discriminant, \
                named[type], line_number, awaited
        }
    }
}

FNR == 1 {
    depth = ""
    coroutine = ""
}

# Most lines describe attributes nothing here reads. Each kind of line is
# told by its indentation and second field before any pattern runs.
substr($0, 1, 4) == "    " && $2 !~ /^DW_AT_(name|type|data_member_location|decl_line|discr_value)/ {
    next
}

{
    if (header($0)) {
        finish()
        depth = next_depth
        offset = next_offset
        tag = next_tag
        name = ""
        type = ""
        location = ""
        line_number = ""
        discr = ""
        next
    }
    if ($0 ~ /^ +<[0-9a-f]+> +DW_AT_name /) {
        name = value($0)
    } else if ($0 ~ /^ +<[0-9a-f]+> +DW_AT_type /) {
        type = reference($0)
    } else if ($0 ~ /^ +<[0-9a-f]+> +DW_AT_data_member_location/) {
        location = value($0)
    } else if ($0 ~ /^ +<[0-9a-f]+> +DW_AT_decl_line/) {
        line_number = value($0)
    } else if ($0 ~ /^ +<[0-9a-f]+> +DW_AT_discr_value/) {
        discr = value($0)
    }
}

ENDFILE {
    finish()
    depth = ""
}

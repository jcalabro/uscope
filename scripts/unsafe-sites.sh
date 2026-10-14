#!/usr/bin/env bash
# Fails when a file outside the list below allows unsafe code, or when a
# crate stops denying it. dev/index.md says why each file may; `just miri`
# tests what Miri can reach. The SDKs are crates of their own.
#
# DO NOT CHANGE THIS FILE WITHOUT ASKING THE HUMAN IF IT IS OKAY!
set -euo pipefail

allowed=(
  src/backend/linux/sys.rs                # pre_exec, raw ptrace requests, tgkill
  src/backend/linux/perf/sys.rs           # perf_event_open, its ioctls and mapping
  src/backend/linux/tls/thread_db.rs      # libthread_db and its callbacks
  src/backend/linux/syscalls/ptrace.rs    # the SIGURG handler
  src/timings/alloc.rs                    # the counting allocator
  src/cli/syscalls.rs                     # localtime_r
  src/cli/config.rs                       # remove_var before any thread starts
  src/main.rs                             # the same, and the global allocator
  tests/miri.rs                           # the tests of the files above
  tests/support/memory_cap.rs             # the tests' capped allocator
  tests/support/mod.rs                    # tgkill
)

failed=0
for manifest in Cargo.toml fuzz/Cargo.toml; do
  if ! grep -qx 'unsafe_code = "deny"' "$manifest"; then
    echo "error: $manifest no longer denies unsafe_code" >&2
    failed=1
  fi
done

while IFS= read -r -d '' file; do
  [[ -f "$file" ]] || continue
  grep -q 'unsafe_code' "$file" || continue
  if [[ " ${allowed[*]} " != *" $file "* ]]; then
    echo "error: $file allows unsafe code; keep it in the files scripts/unsafe-sites.sh lists" >&2
    failed=1
  fi
done < <(git ls-files -z --cached --others --exclude-standard -- '*.rs' ':!:sdk/')

exit "$failed"

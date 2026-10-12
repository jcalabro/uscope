// What the programs and uscope exchange: ring records, map keys and
// values. src/backend/linux/syscalls/bpf.rs decodes each with a struct of
// the same layout, and a test compares the two through this object's BTF.
// Every field is little-endian and naturally aligned; records are padded
// to eight bytes.
#pragma once

typedef unsigned char u8;
typedef unsigned short u16;
typedef unsigned int u32;
typedef int s32;
typedef unsigned long long u64;
typedef long long s64;

enum record_kind {
	RECORD_CALL = 1,
	// A call's arguments read at its entry, as execve's must be; its exit
	// record follows.
	RECORD_ENTRY = 2,
	RECORD_SPAWN = 3,
	RECORD_EXEC = 4,
	RECORD_EXIT = 5,
	RECORD_SIGNAL = 6,
	// What a descriptor the call that follows made refers to.
	RECORD_FILE = 7,
	// The stack of the thread whose call or signal follows.
	RECORD_STACK = 8,
};

// Which descriptors a call makes, from call_spec.makes.
enum makes {
	MAKES_NONE = 0,
	// Its result.
	MAKES_RESULT = 1,
	// The two ints argument makes_arg points at, as pipe2's.
	MAKES_PAIR = 2,
};

// Bits of config.flags.
enum config_flags {
	CONFIG_STREAM = 1,
	CONFIG_COUNT = 2,
	CONFIG_FOLLOW = 4,
	// Leave out calls whose entry was not seen: a launched program's are
	// uscope's own, made before it executes.
	CONFIG_SKIP_UNSEEN = 8,
	// Send each call's and signal's stack: the kernel's walk of its
	// frame pointers.
	CONFIG_STACKS = 16,
	// With each stack, a copy of its top, for uscope to unwind.
	CONFIG_STACK_COPY = 32,
};

// Bits of call_record.flags, as EventFlags.
enum call_flags {
	CALL_ENTRY_UNSEEN = 2,
	CALL_ABI_UNCERTAIN = 4,
	// The thread ended inside the call, which never returned.
	CALL_PROCESS_EXITED = 8,
	// Left out of the stream by sampling, so counted by the kernel.
	CALL_SAMPLED_OUT = 16,
};

enum abi {
	ABI_64 = 0,
	ABI_32 = 1,
	ABI_X32 = 2,
};

// How the copier reads one argument.
enum copy_kind {
	COPY_NONE = 0,
	COPY_PATH = 1,
	COPY_STRING = 2,
	COPY_STRINGS = 3,
	// An array of strings whose elements are only counted.
	COPY_STRINGS_COUNTED = 4,
	COPY_BYTES = 5,
	COPY_IOVEC = 6,
	COPY_SOCKADDR = 7,
	COPY_STRUCT = 8,
	COPY_FD_SET = 9,
};

enum copy_count {
	COUNT_ONE = 0,
	COUNT_ARG = 1,
	COUNT_RETURN = 2,
	// The 32-bit integer the argument count_arg points at.
	COUNT_POINTED = 3,
};

enum read_at {
	READ_ENTRY = 0,
	READ_EXIT = 1,
};

enum capture_state {
	CAPTURE_COMPLETE = 0,
	CAPTURE_TRUNCATED = 1,
	CAPTURE_UNREADABLE = 2,
};

struct arg_copy {
	u8 kind;
	u8 when;
	// Whether the call writes it, so a failed call leaves nothing to read.
	u8 writes;
	u8 count;
	u8 count_arg;
	u8 pad[3];
	// A structure's size, the bytes kept of a buffer, or a string's limit.
	u32 size;
	u32 limit;
};

// The value of each call's slot in the CALLS map: (abi << 10) | number.
struct call_spec {
	u8 selected;
	// Whether its result counts bytes the call moved.
	u8 moves;
	u8 makes;
	u8 makes_arg;
	// Whether it creates a thread or process, whose own return from it
	// is not a call of its own.
	u8 forks;
	u8 pad;
	// Its row in STATS, which holds only the chosen calls.
	u16 stat;
	struct arg_copy args[6];
};

struct config {
	u32 flags;
	u32 elements;
	// A cgroup's processes, by its id and depth, rather than TARGETS'.
	u64 cgroup_id;
	u32 cgroup_level;
	// Ids shown in this pid namespace, by its inode and depth, when set.
	u32 ns_inum;
	u32 ns_level;
	// While streaming, send one call in this many and count the rest;
	// 0 or 1 sends them all.
	u32 sample;
};

// A thread's call in progress.
struct entry {
	u64 entered;
	u64 args[6];
	u32 nr;
	u32 tgid;
	u8 abi;
	u8 flags;
	u8 pad[6];
};

// What the in_flight iterator writes for each call in progress.
struct in_flight_record {
	u32 tid;
	u32 pad;
	struct entry entry;
};

struct record {
	u32 kind;
	// The whole record's bytes, captures included.
	u32 size;
	u32 tid;
	u32 tgid;
};

// Followed by `captures` capture headers, each followed by its bytes
// padded to eight.
struct call_record {
	struct record header;
	u64 entered;
	u64 exited;
	s64 ret;
	u64 args[6];
	u32 nr;
	u8 abi;
	u8 flags;
	u16 captures;
};

struct capture_header {
	u8 arg;
	u8 when;
	u8 state;
	u8 pad;
	u32 len;
	u32 wanted;
	u32 pad2;
};

struct spawn_record {
	struct record header;
	u64 time;
	u32 child;
	u32 child_tgid;
};

// Followed by the new image's path, `len` bytes.
struct exec_record {
	struct record header;
	u64 time;
	u32 former;
	u32 len;
};

// A thread's end, and when it was the last, its process's.
struct exit_record {
	struct record header;
	u64 time;
	// The exit code as wait(2) encodes it.
	s32 code;
	// Whether the process ended with the thread.
	u32 last;
};

struct signal_record {
	struct record header;
	u64 time;
	s32 signal;
	u32 pad;
	// The kernel's siginfo, as much as every kind of signal fills.
	u8 info[48];
};

// Followed by `frames` addresses, the kernel's walk of the frame pointers
// from the instruction, then `copied` bytes of the stack from its pointer.
struct stack_record {
	struct record header;
	u32 frames;
	u32 copied;
	// The user registers by DWARF number: ax, dx, cx, bx, si, di, bp, sp,
	// r8 to r15, and the instruction pointer.
	u64 regs[17];
};

// Followed by the dentry's name, `len` bytes.
struct file_record {
	struct record header;
	s32 fd;
	u32 mode;
	u64 ino;
	u64 magic;
	// The kernel's dev_t: twelve bits of major above twenty of minor.
	u32 rdev;
	u32 len;
	u32 cloexec;
	u32 pad;
};

struct stat_value {
	u64 calls;
	u64 errors;
	u64 timed;
	u64 total_ns;
	u64 min_ns;
	u64 max_ns;
	u64 bytes;
	u64 log2_ns[40];
};

struct errno_key {
	u32 slot;
	u32 err;
};

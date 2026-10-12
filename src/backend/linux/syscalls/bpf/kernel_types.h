// The kernel fields the programs read, and only those. Each structure is
// relocated against the running kernel's BTF (CO-RE), so neither the
// order nor the padding here need match the kernel's; a field the running
// kernel lacks is checked with bpf_core_field_exists before it is read.
#pragma once

#include "records.h"

#define __kernel_preserve __attribute__((preserve_access_index))

struct pt_regs {
	unsigned long bx;
	unsigned long cx;
	unsigned long dx;
	unsigned long si;
	unsigned long di;
	unsigned long bp;
	unsigned long r8;
	unsigned long r9;
	unsigned long r10;
	unsigned long orig_ax;
} __kernel_preserve;

struct thread_info {
	u32 status;
} __kernel_preserve;

typedef struct {
	int counter;
} atomic_t;

struct signal_struct {
	atomic_t live;
} __kernel_preserve;

struct super_block {
	unsigned long s_magic;
} __kernel_preserve;

struct inode {
	unsigned short i_mode;
	unsigned int i_rdev;
	unsigned long i_ino;
	struct super_block *i_sb;
} __kernel_preserve;

struct qstr {
	const unsigned char *name;
} __kernel_preserve;

struct dentry {
	struct qstr d_name;
} __kernel_preserve;

struct path {
	struct dentry *dentry;
} __kernel_preserve;

struct file {
	struct path f_path;
	struct inode *f_inode;
} __kernel_preserve;

struct fdtable {
	unsigned int max_fds;
	struct file **fd;
	unsigned long *close_on_exec;
} __kernel_preserve;

struct files_struct {
	struct fdtable *fdt;
} __kernel_preserve;

struct task_struct {
	struct thread_info thread_info;
	int pid;
	int tgid;
	int exit_code;
	struct signal_struct *signal;
	struct files_struct *files;
} __kernel_preserve;

struct linux_binprm {
	const char *filename;
} __kernel_preserve;

struct kernel_siginfo {
	int si_signo;
} __kernel_preserve;

struct k_sigaction;

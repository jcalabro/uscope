// The freestanding runtime golden programs link with: process entry, output,
// and exit through raw system calls, so that a program's behavior depends on
// nothing but its own code and its arguments.

#ifndef USCOPE_GOLDEN_RT_H
#define USCOPE_GOLDEN_RT_H

typedef unsigned long u64;
typedef long i64;

void rt_write(int fd, const char *bytes, u64 count);
void rt_print(const char *text);
void rt_print_u64(u64 value);
u64 rt_parse_u64(const char *text);
_Noreturn void rt_exit_group(int code);

int main(int argc, char **argv);

#endif

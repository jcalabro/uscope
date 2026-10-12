// Makes each system call its arguments number, with every argument -1, in
// a child of its own, so that a tracer's classes can be read from which
// calls it reports. Arguments of -1 are invalid for almost every call,
// which then fails before doing anything; calls that ignore their
// arguments are named by the test so that it leaves them out.
#define _GNU_SOURCE
#include <stdlib.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

int main(int argc, char **argv) {
    for (int index = 1; index < argc; index++) {
        long number = strtol(argv[index], NULL, 10);
        pid_t child = fork();
        if (child == 0) {
            syscall(number, -1L, -1L, -1L, -1L, -1L, -1L);
            _exit(0);
        }
        int status;
        waitpid(child, &status, 0);
    }
    return 0;
}

// System calls for the syscall tracer's tests, one workload per mode. Paths
// are relative to the working directory the test chooses.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

static void files(void) {
    int fd = openat(AT_FDCWD, "present", O_CREAT | O_WRONLY | O_TRUNC | O_CLOEXEC, 0644);
    if (write(fd, "hello\n", 6) != 6) {
        _exit(10);
    }
    close(fd);
    if (openat(AT_FDCWD, "missing", O_RDONLY) != -1 || errno != ENOENT) {
        _exit(11);
    }
    fd = open("present", O_RDONLY);
    char buffer[16];
    if (read(fd, buffer, sizeof buffer) != 6) {
        _exit(12);
    }
    struct stat status;
    fstat(fd, &status);
    int pipes[2];
    pipe2(pipes, O_CLOEXEC);
    dup2(pipes[1], 9);
    close(pipes[0]);
    close(pipes[1]);
    close(9);
    close(fd);
    unlink("present");
}

static void *thread_main(void *argument) {
    (void)argument;
    syscall(SYS_getppid);
    return NULL;
}

static void family(void) {
    // SIGCHLD stays pending: delivered, it lands before or after wait4
    // returns as the child's exit races it, and two traces would differ.
    sigset_t blocked;
    sigemptyset(&blocked);
    sigaddset(&blocked, SIGCHLD);
    sigprocmask(SIG_BLOCK, &blocked, NULL);
    pid_t child = fork();
    if (child == 0) {
        syscall(SYS_write, 1, "child\n", 6);
        _exit(4);
    }
    int status;
    waitpid(child, &status, 0);
    pthread_t thread;
    pthread_create(&thread, NULL, thread_main, NULL);
    pthread_join(thread, NULL);
}

static void block(void) {
    // The test sees this line, then the read blocked in flight, then
    // closes standard input.
    syscall(SYS_write, 1, "ready\n", 6);
    char byte;
    while (read(0, &byte, 1) > 0) {
    }
}

static volatile sig_atomic_t delivered;

static void handler(int signal) {
    (void)signal;
    delivered = 1;
}

static void signals(void) {
    struct sigaction action = {.sa_handler = handler};
    sigaction(SIGUSR1, &action, NULL);
    kill(getpid(), SIGUSR1);
    if (!delivered) {
        _exit(13);
    }
}

static void compat(void) {
    // getpid through the i386 ABI, then through x32's numbering, which
    // kernels without x32 support fail with ENOSYS.
    long result;
    __asm__ volatile("int $0x80" : "=a"(result) : "a"(20) : "memory");
    if (result != getpid()) {
        _exit(14);
    }
    __asm__ volatile("syscall" : "=a"(result) : "a"(0x40000000L | 39) : "rcx", "r11", "memory");
}

static void dumps(void) {
    // Data through a pipe, written whole and in parts, read whole and in
    // parts, for -e read= and -e write= to dump.
    int pipes[2];
    if (pipe(pipes) != 0) {
        _exit(15);
    }
    static const char text[] = "a dump of\tforty-odd bytes\x01\x02 over three lines\n";
    if (write(pipes[1], text, sizeof text - 1) != sizeof text - 1) {
        _exit(16);
    }
    char buffer[64];
    if (read(pipes[0], buffer, sizeof buffer) != sizeof text - 1) {
        _exit(17);
    }
    struct iovec parts[2] = {{(void *)"two ", 4}, {(void *)"parts\n", 6}};
    if (writev(pipes[1], parts, 2) != 10) {
        _exit(18);
    }
    struct iovec into[2] = {{buffer, 3}, {buffer + 3, 61}};
    if (readv(pipes[0], into, 2) != 10) {
        _exit(19);
    }
}

int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "files";
    if (strcmp(mode, "files") == 0) {
        files();
        return 3;
    }
    if (strcmp(mode, "family") == 0) {
        family();
        return 0;
    }
    if (strcmp(mode, "block") == 0) {
        block();
        return 0;
    }
    if (strcmp(mode, "signal") == 0) {
        signals();
        return 0;
    }
    if (strcmp(mode, "compat") == 0) {
        compat();
        return 0;
    }
    if (strcmp(mode, "dumps") == 0) {
        dumps();
        return 0;
    }
    return 2;
}

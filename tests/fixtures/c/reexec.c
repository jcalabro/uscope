/* Runs itself again with one argument; a run with arguments prints them
 * and exits with its argument count. */
#include <stdio.h>
#include <unistd.h>

__attribute__((noinline)) int reexecuted(int argc, char **argv) {
    for (int index = 1; index < argc; index++) {
        puts(argv[index]);
    }
    return argc;
}

int main(int argc, char **argv) {
    if (argc == 1) {
        char *const arguments[] = {argv[0], "again", NULL};
        execv("/proc/self/exe", arguments);
        return 90;
    }
    return reexecuted(argc, argv);
}

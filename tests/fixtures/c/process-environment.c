// Reports how it was started: its arguments, two environment variables, its
// working directory, and one line of standard input, then exits with argc.
#define _POSIX_C_SOURCE 200809L

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv) {
    char directory[4096];
    char line[256];

    for (int index = 1; index < argc; ++index) {
        printf("argument %d: %s\n", index, argv[index]);
    }
    const char *value = getenv("USCOPE_FIXTURE_VALUE");
    printf("value: %s\n", value != NULL ? value : "(unset)");
    printf("removed: %s\n", getenv("USCOPE_FIXTURE_REMOVED") != NULL ? "present" : "absent");
    if (getcwd(directory, sizeof directory) == NULL) {
        return 100;
    }
    printf("directory: %s\n", directory);
    fflush(stdout);
    if (fgets(line, sizeof line, stdin) == NULL) {
        strcpy(line, "(eof)\n");
    }
    fprintf(stderr, "input: %s", line);
    return argc;
}

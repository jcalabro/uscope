#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

/* Whether the leader sleeps in pause, as its /proc files say. */
static int leader_paused(pid_t leader) {
    char path[64];
    char text[512];
    FILE *file;
    char *state;
    long number = -1;

    snprintf(path, sizeof path, "/proc/self/task/%d/stat", leader);
    file = fopen(path, "r");
    if (file == NULL) {
        return 0;
    }
    if (fgets(text, sizeof text, file) == NULL) {
        text[0] = '\0';
    }
    fclose(file);
    state = strrchr(text, ')');
    if (state == NULL || state[1] != ' ' || state[2] != 'S') {
        return 0;
    }
    snprintf(path, sizeof path, "/proc/self/task/%d/syscall", leader);
    file = fopen(path, "r");
    if (file == NULL) {
        return 0;
    }
    if (fscanf(file, "%ld", &number) != 1) {
        number = -1;
    }
    fclose(file);
    return number == SYS_pause;
}

/* Executes a program once the leader sleeps in pause, or after a deadline
 * without that. */
static void *replace_image(void *argument) {
    char *const arguments[] = {"true", NULL};
    pid_t leader = getpid();
    struct timespec wait = {0, 1000000};

    (void)argument;
    for (int tries = 0; tries < 30000 && !leader_paused(leader); tries++) {
        nanosleep(&wait, NULL);
    }
    execv("/bin/true", arguments);
    _exit(90);
}

int main(void) {
    pthread_t thread;

    if (pthread_create(&thread, NULL, replace_image, NULL) != 0) {
        return 2;
    }
    for (;;) {
        pause();
    }
}

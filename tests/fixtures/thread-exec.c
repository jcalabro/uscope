#include <pthread.h>
#include <unistd.h>

static void *replace_image(void *argument) {
    char *const arguments[] = {"true", NULL};

    (void)argument;
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

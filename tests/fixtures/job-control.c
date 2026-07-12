#include <signal.h>

int main(void) {
    if (raise(SIGSTOP) != 0) {
        return 2;
    }
    return 0;
}

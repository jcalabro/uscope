extern _Thread_local int data_tls;

__attribute__((noinline)) void data_stop(void) {
    __asm__ volatile("" ::: "memory");
}

int main(void) {
    data_tls += 1;
    data_stop();
    return 0;
}

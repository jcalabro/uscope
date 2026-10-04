// Hand-written assembly placed after a compiled function, with no line
// information of its own: the function's last line table row runs on, to
// the next row, through it.

__attribute__((noinline)) int before(int value) {
    return value + 1;
}

__asm__(".text\n"
        ".global bare\n"
        ".type bare, @function\n"
        "bare:\n"
        "    lea 2(%rdi), %eax\n"
        "    ret\n"
        ".size bare, . - bare\n");

int bare(int value);

int main(void) {
    return before(1) + bare(1) == 5 ? 0 : 1;
}

// Calls a source step must not lose its way through. A line that calls one
// function twice: stepping over the callee's last line returns into the
// middle of the caller's line, whose second call then enters the function
// again at the same stack depth, a new frame, not the one the step began
// in. And a function that returns right after its own call, so that a step
// from the inner callee's last line returns through it to its caller, which
// calls it again on the same line.
volatile int values[4] = {1, 2, 3, 4};
volatile int sink;

__attribute__((noinline)) int load(int index) {
    return values[index]; // LOAD_RETURN
}

__attribute__((noinline)) int relay(int index) {
    return load(index);
}

int main(void) {
    int total = 0;
    for (int index = 0; index < 3; index++) {
        total += load(index) * load(index + 1); // TWO_CALLS
        sink = total;
    }
    total += relay(3) + relay(0); // RELAY_CALL
    sink = total; // AFTER_RELAY
    return total == 2 + 6 + 12 + 4 + 1 ? 0 : 1;
}

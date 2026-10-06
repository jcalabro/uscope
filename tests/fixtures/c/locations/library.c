// A library function the program calls through its procedure linkage
// table, so the call site names its target only by symbol.

volatile int locations_library_sink;

// Keeps nothing of what it was given once it calls back.
__attribute__((visibility("default"), noinline)) int locations_in_library(int value,
                                                                          void (*stop)(void)) {
    locations_library_sink = value;
    stop();
    return locations_library_sink;
}

// Stops where only the call that entered it says what it was given.
__attribute__((visibility("default"), noinline)) int
locations_interposed_target(int value, void (*stop)(void)) {
    locations_library_sink = value;
    stop();
    return locations_library_sink;
}

// The program defines its own, which the library's calls reach instead.
__attribute__((visibility("default"), noinline)) int locations_interposed(int value,
                                                                          void (*stop)(void)) {
    return locations_interposed_target(value + 1, stop);
}

// Jumps to `locations_interposed` through the procedure linkage table, so
// whichever module's the dynamic linker chose.
__attribute__((visibility("default"), noinline)) int locations_interposing(int value,
                                                                           void (*stop)(void)) {
    return locations_interposed(value * 2, stop);
}

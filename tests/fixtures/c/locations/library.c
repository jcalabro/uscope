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

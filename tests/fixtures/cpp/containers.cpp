// Standard library containers, and deliberately corrupted ones, which the
// built-in views present. Each `VIEW:` marker says what its expression must
// show, evaluated in main() where barrier() is called: `{c*N}` stands for N
// of the character c, and `problem:` says the view must refuse the value,
// and why.

#include <array>
#include <cstdint>
#include <cstring>
#include <span>
#include <string>
#include <string_view>
#include <vector>

extern "C" __attribute__((noinline)) void barrier(void *fixture) {
    __asm__ volatile("" : : "r"(fixture) : "memory");
}

// Storage the program never constructs or destroys, so its bytes can be
// whatever a corrupted container holds.
template <typename T> union Corrupt {
    T value;
    Corrupt() {}
    ~Corrupt() {}
};

template <typename T> static void keep(T &value) {
    __asm__ volatile("" : : "r"(&value) : "memory");
}

int main() {
    std::string text = "hello, world";            // VIEW: text => "hello, world"
    std::string empty_text;                       // VIEW: empty_text => ""
    std::string long_text(300, 'y');              // VIEW: long_text => "{y*256}"... (300 bytes)
    std::string with_nul("a\0b", 3);             // VIEW: with_nul => "a\u{0}b"
    std::string_view view = "a view";             // VIEW: view => "a view"
    std::vector<int> ints = {1, 2, 3};            // VIEW: ints => len=3 [1, 2, 3]
    std::vector<int> no_ints;                     // VIEW: no_ints => len=0 []
    std::vector<std::string> words = {"one", "two"}; // VIEW: words => len=2 ["one", "two"]
    std::vector<int> many(300);                   // VIEW: many => len=300 [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, …]
    for (int index = 0; index < 300; ++index) {
        many[index] = index;
    }
    std::array<int, 4> four = {4, 5, 6, 7};       // VIEW: four => len=4 [4, 5, 6, 7]
    std::array<int, 0> none{};                    // VIEW: none => len=0 []
    std::span<int> dynamic_span(ints);            // VIEW: dynamic_span => len=3 [1, 2, 3]
    std::span<int, 4> fixed_span(four);           // VIEW: fixed_span => len=4 [4, 5, 6, 7]

    // A vector whose end passes its storage's end.
    int storage[16] = {10, 11, 12, 13};
    Corrupt<std::vector<int>> past_capacity;      // VIEW: past_capacity.value => problem: check
    int *past[3] = {storage, storage + 9, storage + 4};
    std::memcpy(static_cast<void *>(&past_capacity.value), past, sizeof past);
    // A vector whose elements are in no mapped memory.
    Corrupt<std::vector<int>> dangling;           // VIEW: dangling.value => len=2 [<unavailable>, …]
    std::uintptr_t garbage[3] = {0x10, 0x18, 0x18};
    std::memcpy(static_cast<void *>(&dangling.value), garbage, sizeof garbage);
    // A vector whose end is not a whole element past its start.
    Corrupt<std::vector<int>> ragged;             // VIEW: ragged.value => problem: whole number of elements
    char *bytes = reinterpret_cast<char *>(storage);
    char *ragged_ends[3] = {bytes, bytes + 6, bytes + 16};
    std::memcpy(static_cast<void *>(&ragged.value), ragged_ends, sizeof ragged_ends);

    keep(text), keep(empty_text), keep(long_text), keep(with_nul), keep(view);
    keep(ints), keep(no_ints), keep(words), keep(many), keep(four), keep(none);
    keep(dynamic_span), keep(fixed_span), keep(past_capacity), keep(dangling), keep(ragged);
    barrier(&text);
    return static_cast<int>(ints.size() + many.size()) == 303 ? 0 : 1;
}

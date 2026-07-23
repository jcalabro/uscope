#include <cstdint>

enum class State : std::int8_t {
    Negative = -3,
    Zero = 0,
    Alias = 0,
    Ready = 7,
};

union Raw {
    std::int32_t integer;
    float floating;
};

struct ManualTagged {
    enum class Kind : std::uint8_t {
        Integer,
        Floating,
    } kind;
    union {
        std::int32_t integer;
        float floating;
    } payload;
};

__attribute__((noinline)) static bool inspect_enums(const State* state,
                                                    const State* alias,
                                                    const Raw* raw,
                                                    const ManualTagged* tagged) {
    asm volatile("" : : "r"(state), "r"(alias), "r"(raw), "r"(tagged) : "memory");
    return *state == State::Negative && *alias == State::Alias && raw->integer == 42 &&
           tagged->kind == ManualTagged::Kind::Integer && tagged->payload.integer == 42;
}

int main() {
    State state = State::Negative;
    State alias = State::Alias;
    Raw raw{.integer = 42};
    ManualTagged tagged{
        .kind = ManualTagged::Kind::Integer,
        .payload = {.integer = 42},
    };
    return inspect_enums(&state, &alias, &raw, &tagged) ? 0 : 1;
}

// Prints `EXPECT\t<expression>\t<kind>\t<value>` for each expression the
// debugger must agree with, then calls barrier().

#include <cstdint>
#include <cstdio>
#include <cstring>

extern "C" void barrier(void *fixture);

namespace shapes {
struct Inner {
    short s;
    long long ll;
};
}  // namespace shapes

struct Fixture {
    int count;
    unsigned char byte;
    shapes::Inner inner;
    int arr[4];
    int *ptr;
    const char *text;
    bool flag;
    double real;
};

#define EXPECT_INT(expression, native) \
    std::printf("EXPECT\t%s\tint\t%lld\n", expression, static_cast<long long>(native))
#define EXPECT_BOOL(expression, native) \
    std::printf("EXPECT\t%s\tbool\t%s\n", expression, (native) ? "true" : "false")
#define EXPECT_F64(expression, native)                                               \
    do {                                                                             \
        double value_ = (native);                                                    \
        std::uint64_t bits_;                                                         \
        std::memcpy(&bits_, &value_, sizeof bits_);                                  \
        std::printf("EXPECT\t%s\tf64\t%#llx\n", expression,                         \
                    static_cast<unsigned long long>(bits_));                         \
    } while (0)

int main() {
    Fixture f{-70000, 250, {-12, 123456789012LL}, {10, 20, 30, 40}, nullptr, "hello", true, 2.75};
    f.ptr = &f.arr[2];
    int &ref = f.arr[1];

    EXPECT_INT("f.count", f.count);
    EXPECT_INT("f.byte + 10", f.byte + 10);
    EXPECT_INT("f.inner.ll", f.inner.ll);
    EXPECT_INT("f.arr[3]", f.arr[3]);
    EXPECT_INT("*f.ptr", *f.ptr);
    EXPECT_INT("f.ptr - f.arr", f.ptr - f.arr);
    EXPECT_INT("ref", ref);
    EXPECT_INT("ref * 2", ref * 2);
    EXPECT_BOOL("&ref == &f.arr[1]", &ref == &f.arr[1]);
    EXPECT_BOOL("f.text == \"hello\"", std::strcmp(f.text, "hello") == 0);
    EXPECT_BOOL("f.flag", f.flag);
    EXPECT_F64("f.real * 2", f.real * 2);
    EXPECT_INT("sizeof(f.inner)", sizeof f.inner);
    EXPECT_INT("(short)f.count", static_cast<short>(f.count));
    std::fflush(stdout);
    barrier(&f);
    return ref == 0;
}

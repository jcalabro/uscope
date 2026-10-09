// A program and its library that share a header and a helper, as a
// distribution's packages do, for debug files that share what they have in
// common through a dwz supplementary file.
#include <cstdio>

#include "shapes.h"

int main() {
    shapes::Shape square("square", {0, 0}, {4, 4});
    std::int64_t measured = shapes::measure(square);
    std::int64_t again = shapes::area(square);
    std::printf("%lld %lld %d\n", static_cast<long long>(measured), static_cast<long long>(again),
                shapes::shapes_measured); // shapes: printed
    return 0;
}

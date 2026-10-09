// Compiled into both the program and the library, as distributions link
// one helper library's objects into several of a package's files.
#include "shapes.h"

namespace shapes {

std::int64_t area(const Shape &shape) {
    std::int64_t extent = 0;
    for (std::int32_t row = 0; row < shape.down().length(); row++) {
        std::int64_t added = width(shape);
        extent += added; // shapes: added
    }
    return extent;
}

} // namespace shapes

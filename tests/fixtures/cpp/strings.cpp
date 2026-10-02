// libstdc++ strings: one short enough to live inside the object, one long
// enough to be truncated, an empty one, and one holding a NUL.
#include <string>

__attribute__((noinline)) int strings_target(const std::string &short_text,
                                             const std::string &long_text) {
    std::string empty;
    std::string with_nul("a\0b", 3);
    asm volatile("" : : "g"(&short_text), "g"(&long_text), "g"(&empty), "g"(&with_nul)
                 : "memory");
    return static_cast<int>(short_text.size() + with_nul.size()); // strings stop here
}

int main() {
    std::string short_text = "short";
    std::string long_text(300, 'y');
    return strings_target(short_text, long_text) == 8 ? 0 : 1;
}

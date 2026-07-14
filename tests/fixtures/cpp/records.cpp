#include <cstdint>

struct LeftBase {
    std::int32_t left;
};

struct RightBase {
    std::int32_t right;
};

struct Derived : LeftBase, RightBase {
    std::int32_t own;
    static inline std::int32_t static_value = 99;
};

struct VirtualBase {
    std::int32_t virtual_value;
};

struct VirtualDerived : virtual VirtualBase {
    std::int32_t own;
    virtual ~VirtualDerived() = default;
};

struct DiamondRoot {
    std::int32_t root;
};

struct DiamondLeft : virtual DiamondRoot {
    std::int32_t left;
};

struct DiamondRight : virtual DiamondRoot {
    std::int32_t right;
};

struct Diamond : DiamondLeft, DiamondRight {
    std::int32_t own;
};

__attribute__((noinline)) static bool inspect_records(Derived* derived,
                                                       VirtualDerived* virtual_derived,
                                                       Diamond* diamond) {
    __asm__ volatile("" : : "g"(derived), "g"(virtual_derived), "g"(diamond) : "memory");
    volatile std::int32_t marker = derived->own;
    return marker == 22 && derived->left == 9 && derived->right == 11 &&
           virtual_derived->own == 20 && virtual_derived->virtual_value == 22 &&
           diamond->root == 7 && diamond->left == 8 && diamond->right == 9 && diamond->own == 18;
}

int main() {
    Derived derived{{9}, {11}, 22};
    VirtualDerived virtual_derived;
    virtual_derived.own = 20;
    virtual_derived.virtual_value = 22;
    Diamond diamond;
    diamond.root = 7;
    diamond.left = 8;
    diamond.right = 9;
    diamond.own = 18;
    return inspect_records(&derived, &virtual_derived, &diamond) ? 0 : 1;
}

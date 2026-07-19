#include <stdint.h>

struct inner_record {
    int32_t signed_value;
    uint32_t unsigned_value;
};

struct outer_record {
    struct inner_record inner;
    int32_t values[2];
};

struct bit_fields {
    signed int negative : 5;
    unsigned int first : 3;
    unsigned int second : 6;
};

struct flexible_record {
    int32_t count;
    int32_t values[];
};

struct large_record {
    unsigned char padding[2048];
    int32_t small;
};

struct incomplete_record;

struct outer_record global_record = {{-7, 9}, {20, 22}};

__attribute__((noinline)) static int inspect_records(
    struct outer_record *record,
    struct bit_fields *bits,
    struct outer_record (*records)[2],
    struct flexible_record *flexible,
    struct large_record *large,
    struct incomplete_record *incomplete) {
    __asm__ volatile("" : : "g"(record), "g"(bits), "g"(records), "g"(flexible),
                     "g"(large), "g"(incomplete) : "memory");
    volatile int marker = record->inner.signed_value;
    return marker == -7 && bits->negative == -3 && bits->first == 5 && bits->second == 42 &&
           (*records)[1].values[1] == 44 && flexible->count == 2 && large->small == 73 &&
           incomplete != 0;
}

int main(void) {
    struct bit_fields bits = {-3, 5, 42};
    struct outer_record records[2] = {{{1, 2}, {3, 4}}, {{5, 6}, {43, 44}}};
    struct {
        int32_t count;
        int32_t values[2];
    } flexible = {2, {20, 22}};
    struct large_record large = {{0}, 73};
    return inspect_records(&global_record, &bits, &records, (struct flexible_record *)&flexible,
                           &large, (struct incomplete_record *)&global_record)
               ? 0
               : 1;
}

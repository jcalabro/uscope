#include <stdio.h>
#include <stdlib.h>

typedef struct TestStruct {
    char A;
    int B;
} TestStruct;

typedef enum TestEnum {
    ONE,
    TWO,
    THREE,
} TestEnum ;

typedef struct CircularPointer CircularPointer;
struct CircularPointer {
    int value;
    CircularPointer* ptr;
};

int my_func(int param, TestStruct* ts2) {
    printf("IN my_func\n");
    return param + ts2->B;
}

int main() {
    char      a = 1;
    short     b = 2;
    int       c = 3;
    long      d = 4;
    long long e = 5;

    unsigned char      f = 6;
    unsigned short     g = 7;
    unsigned int       h = 8;
    unsigned long      i = 9;
    unsigned long long j = 10;
    unsigned long long *j_ptr = &j;

    float k = 11.5;
    double l = 12.75;

    TestStruct ts = {13, 14};

    TestStruct *ts2 = (TestStruct*)malloc(sizeof(TestStruct));
    ts2->A = 15;
    ts2->B = 16;

    float arr[14] = {0};
    arr[0] = 1.23;
    arr[1] = 4.56;
    arr[13] = 7.89;

    char *basic_str = "Hello, world!";

    char *heap_str = (char*)malloc(4 * sizeof(char));
    heap_str[0] = 'y';
    heap_str[1] = 'e';
    heap_str[2] = 's';
    heap_str[3] = '\0';

    char* strings[4];
    strings[0] = "this";
    strings[1] = "is";
    strings[2] = "a";
    strings[3] = "test";

    // test with and without the `enum` prefix
    TestEnum enum_one = ONE;
    enum TestEnum enum_two = TWO;
    TestEnum enum_three = THREE;

    // Create a circular pointer chain
    CircularPointer* circular_a = (CircularPointer*)malloc(sizeof(CircularPointer));
    circular_a->value = 17;
    CircularPointer* circular_b = (CircularPointer*)malloc(sizeof(CircularPointer));
    circular_b->value = 18;
    circular_b->ptr = circular_a;
    circular_a->ptr = circular_b;

    int func_result = my_func(19, ts2);

    printf("A: %d\n", a);
    printf("B: %d\n", b); // sim:cprint stops here
    printf("C: %d\n", c);
    printf("D: %ld\n", d);
    printf("E: %lld\n", e);

    printf("F: %d\n", f);
    printf("G: %d\n", g);
    printf("H: %d\n", h);
    printf("I: %ld\n", i);
    printf("J: %lld\n", j);
    printf("&J: %lln\n", j_ptr);

    printf("K: %f\n", k);
    printf("L: %f\n", l);

    printf("TestStruct.A: %d\n", ts.A);
    printf("TestStruct.B: %d\n", ts.B);

    printf("TestStruct2->A: %d\n", ts2->A);
    printf("TestStruct2->B: %d\n", ts2->B);

    printf("ARR: %p\n", arr);
    printf("STR: %s\n", basic_str);
    printf("HEAP STR: %s\n", heap_str);
    printf("STRINGS[0]: %s\n", strings[0]);
    printf("STRINGS[1]: %s\n", strings[1]);
    printf("STRINGS[2]: %s\n", strings[2]);
    printf("STRINGS[3]: %s\n", strings[3]);

    printf("ENUM ONE: %d\n", enum_one);
    printf("ENUM TWO: %d\n", enum_two);
    printf("ENUM THREE: %d\n", enum_three);

    printf("CIRCULAR_A: %d\n", circular_a->value);
    printf("CIRCULAR_B: %d\n", circular_b->value);

    printf("FUNC_RESULT: %d\n", func_result);
}

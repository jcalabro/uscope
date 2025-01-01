#include "second.h"

int main() {
    MyFunc("hello world!");
    printf("back in main.c\n");

    // declare a variable whose definition lives
    // in another compile unit
    struct MyStruct my_struct;
    my_struct.Field = 123;
    printf("MyStruct.Field: %d\n", my_struct.Field);

    return 0;
}

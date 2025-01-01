#include <stdio.h>

void FuncE() {
    printf("FuncE\n");
}

void FuncD() {
    FuncE();
    printf("FuncD\n");
}

void FuncC() {
    FuncD();
    printf("FuncC\n");
}

void FuncB() {
    FuncC();
    printf("FuncB\n");
}

void FuncA() {
    FuncB();
    printf("FuncA\n");
}

void FuncF() {
    // reverse the order of the print/call
    printf("FuncF\n");
    FuncE();
}

int main() {
    FuncA();
    FuncB();
    FuncC();
    FuncD();
    FuncE();
    FuncF();
}

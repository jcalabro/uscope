#include <iostream>

#include "main.h"

namespace MyNamespace {
    int MyClass::call_lambda() {
        std::cout << "calling lambda with protected_field " << protected_field << std::endl;
        return this->private_lambda(this->public_field, public_field);
    }

    MyClass::~MyClass() {
        std::cout << "in destructor" << std::endl;
    }
};

void myFunc(MyNamespace::MyClass& ref) {
    std::cout << "ref" << std::endl;
    std::cout << ref.public_field << std::endl;
    std::cout << ref.call_lambda() << std::endl;
}

int main() {
    using namespace MyNamespace;

    MyNamespace::MyClass stack(1);
    auto heap = new MyClass(2);

    std::cout << "stack" << std::endl;
    std::cout << stack.public_field << std::endl;
    std::cout << stack.call_lambda() << std::endl;

    myFunc(stack);

    std::cout << "heap" << std::endl;
    std::cout << heap->public_field << std::endl;
    std::cout << heap->call_lambda() << std::endl;

    FriendClass fr(heap);
    fr.print();

    delete heap;

    return 0;
}

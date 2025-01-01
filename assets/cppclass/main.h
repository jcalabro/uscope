#include <functional>
#include <iostream>
#include <string>

using namespace std;

namespace MyNamespace {
    class MyClass {
        friend class FriendClass;

        public:
            MyClass(int a) {
                public_field = a;
                private_field = a + 1000;
                private_lambda = [](int a, int b) { return a + b; };
            }
            ~MyClass();

            int public_field;
            int call_lambda();

        protected:
            string protected_field;

        private:
            int private_field;
            function<int(int, int)> private_lambda;
    };

    class FriendClass {
        public:
            FriendClass(MyClass *c) {
                this->c = c;
            }

            void print() {
                cout << "friend class: " << this->c->private_lambda(c->private_field, 123) << endl;
            }

        private:
            MyClass *c = nullptr;
    };
};

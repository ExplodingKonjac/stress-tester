#include "testlib.h"

int main(int argc, char* argv[]) {
    registerGen(argc, argv, 1);
    int a = rnd.next(1, 2'000'000'000), b = rnd.next(1, 2'000'000'000);
    println(a, b);
    return 0;
}

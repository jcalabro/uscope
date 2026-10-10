// D's Phobos containers, which the built-in views present. Each `VIEW:`
// marker says what its expression must show, evaluated in main() where
// barrier() is called; `problem:` says the view must refuse the value, and
// why.
module containers;

import std.array : Appender, appender;
import std.typecons : Nullable, nullable;

__gshared const(void)* sink;

// Takes every value at once, so that each is complete where it stops.
pragma(inline, false) void barrier(const(void)*[] fixtures...)
{
    foreach (fixture; fixtures)
        sink = fixture;
}

void main()
{
    auto text = appender!string(); // VIEW: text => "hello, world"
    text.put("hello, ");
    text.put("world");
    auto numbers = appender!(int[])(); // VIEW: numbers => len=3 [1, 2, 3]
    numbers.put(1);
    numbers.put(2);
    numbers.put(3);
    Appender!(int[]) unused; // VIEW: unused => len=0 []
    auto present = nullable(42); // VIEW: present => 42
    Nullable!int absent; // VIEW: absent => null
    barrier(&text, &numbers, &unused, &present, &absent);
}

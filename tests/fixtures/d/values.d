// D's values, where this program says they are. Before each checkpoint the
// program prints its own truth, one tab-separated line per value,
//
//	TRUTH	<checkpoint>	<path>	<kind>	<value>
//
// and then calls reached(checkpoint); the tests read the values in
// reached's caller. A path names a variable and then its fields and
// elements. Floats are their bits in hexadecimal; kind `summary` is how
// uscope writes the value.
module values;

import std.stdio : stdout, writefln;

struct Point
{
    int x;
    int y;
}

struct Segment
{
    Point from;
    Point to;
    ubyte tag;
}

enum Color : ubyte
{
    red,
    green,
    blue,
}

class Animal
{
    int legs;
    string name;

    this(int legs, string name)
    {
        this.legs = legs;
        this.name = name;
    }
}

__gshared const(void)* sink;

// Reaches a checkpoint once every truth before it is written.
pragma(inline, false) void reached(string checkpoint)
{
    sink = checkpoint.ptr;
}

// Keeps a value alive, and where the program put it, past the checkpoint.
pragma(inline, false) void keep(const(void)* value)
{
    sink = value;
}

void truth(T)(string checkpoint, string path, string kind, T value)
{
    writefln("TRUTH\t%s\t%s\t%s\t%s", checkpoint, path, kind, value);
    stdout.flush();
}

void textTruth(string checkpoint, string path, string value)
{
    truth(checkpoint, path, "string", '"' ~ value ~ '"');
}

void f32Truth(string checkpoint, string path, float value)
{
    writefln("TRUTH\t%s\t%s\tf32\t%#x", checkpoint, path, *cast(uint*)&value);
    stdout.flush();
}

void f64Truth(string checkpoint, string path, double value)
{
    writefln("TRUTH\t%s\t%s\tf64\t%#x", checkpoint, path, *cast(ulong*)&value);
    stdout.flush();
}

pragma(inline, false) int add(int a, int b)
{
    int sum = a + b;
    keep(&sum);
    return sum;
}

pragma(inline, false) void scalars()
{
    byte small = -5;
    ushort wide = 65_000;
    long big = -(1L << 40);
    float single = 1.5;
    double precise = -0.1;
    bool flag = true;
    dchar letter = 'λ';
    truth("scalars", "small", "int", small);
    truth("scalars", "wide", "int", wide);
    truth("scalars", "big", "int", big);
    f32Truth("scalars", "single", single);
    f64Truth("scalars", "precise", precise);
    truth("scalars", "flag", "summary", flag);
    truth("scalars", "letter", "int", cast(uint) letter);
    reached("scalars");
    keep(&small); keep(&wide); keep(&big); keep(&single);
    keep(&precise); keep(&flag); keep(&letter);
}

pragma(inline, false) void records()
{
    auto point = Point(3, -4);
    auto segment = Segment(Point(1, 2), Point(5, 6), 9);
    int[3] numbers = [10, 20, 30];
    ubyte[2][2] grid = [[1, 2], [3, 4]];
    auto color = Color.green;
    auto animal = new Animal(4, "cat");
    truth("records", "point.x", "int", point.x);
    truth("records", "point.y", "int", point.y);
    truth("records", "segment.to.y", "int", segment.to.y);
    truth("records", "segment.tag", "int", segment.tag);
    foreach (index, number; numbers)
        truth("records", "numbers." ~ format(index), "int", number);
    truth("records", "grid.(1,0)", "int", grid[1][0]);
    truth("records", "color", "symbol", color);
    reached("records");
    keep(&point); keep(&segment); keep(&numbers); keep(&grid); keep(&color);
    keep(cast(void*) animal);
}

pragma(inline, false) void slices()
{
    long[] backing = [7, 8, 9, 10];
    long[] ints = backing[1 .. $];
    long[] nothing;
    string text = "héllo";
    string empty = "";
    wstring wide = "wïde"w;
    truth("slices", "ints", "len", ints.length);
    foreach (index, value; ints)
        truth("slices", "ints." ~ format(index), "int", value);
    truth("slices", "nothing", "len", nothing.length);
    textTruth("slices", "text", text);
    textTruth("slices", "empty", empty);
    textTruth("slices", "wide", "wïde");
    reached("slices");
    keep(&ints); keep(&nothing); keep(&text); keep(&empty); keep(&wide);
}

// Stops within a loop, among the variables D makes to run it.
pragma(inline, false) void loop()
{
    long total = 0;
    foreach (index, value; [4L, 5L, 6L])
    {
        total += value;
        if (index == 1)
        {
            truth("loop", "value", "int", value);
            truth("loop", "total", "int", total);
            reached("loop");
        }
    }
    keep(&total);
}

string format(size_t value)
{
    import std.conv : to;

    return value.to!string;
}

void main()
{
    scalars();
    records();
    slices();
    loop();
    if (add(2, 3) != 5)
        assert(0, "add");
}

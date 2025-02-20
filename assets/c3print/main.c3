import std::io;

struct MyStruct
{
    char field_a;
    String field_b;
}

enum MyEnum : int
{
	FIRST,
	SECOND,
	THIRD
}

fn void main()
{
	bool a = true;
	bool b = false;

	ichar   c = 1;
	short   d = 2;
	int     e = 3;
	long    f = 4;
	int128  g = 5;
	iptr    h = 6;
	isz     i = 7;
	char    j = 8;
	ushort  k = 9;
	uint    l = 11;
	ulong   m = 12;
	uint128 n = 13;
	uptr    o = 14;
	usz     p = 15;

	float q = 16.1;
	double r = 17.2;

	String s = "Hello, world!";
	String* t = &s;
	void* u = t;

	int[*] v = { 11, 12, 13 }; // array
	int[]  w = &v;             // slice

	MyStruct x;
    x.field_a = 18;
    x.field_b = "this is the second field";

	MyEnum y = MyEnum.FIRST;
	MyEnum z = MyEnum.SECOND;
	MyEnum aa = MyEnum.THIRD; // sim:c3print stops here because all the prints are inlined

    io::printn(a);
    io::printn(b);

	io::printn(c);
	io::printn(d);
	io::printn(e);
	io::printn(f);
	io::printn(g);
	io::printn(h);
	io::printn(i);
	io::printn(j);
	io::printn(k);
	io::printn(l);
	io::printn(m);
	io::printn(n);
	io::printn(o);
	io::printn(p);

	io::printn(q);
	io::printn(r);

	io::printn(s);
	io::printn(*t);

	io::printn(v);
	io::printn(w);

	io::printn(x);

	io::printn(y);
	io::printn(z);
	io::printn(aa);
}

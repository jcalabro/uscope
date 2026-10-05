# Expressions

uscope reads expressions in one language for every program it debugs,
whether it is written in C, C++, Rust, Go, or Zig. The language is small and
exact, and the same everywhere: it has no per-language parsers, type checkers,
or compilers behind it.

Every example on this page runs as one of uscope's tests. Each row reads
`expression => outcome`: `reads as` gives the expression's normal form, the
spelling uscope prints it in, and `error` names the kind of error the
expression produces and the text it points at.

## Names

A name is `[A-Za-z_][A-Za-z0-9_]*`. Names may be qualified with `::`, and
`::name` starts from the outermost scope. Backticks quote any other name,
including one with `/`, `-`, spaces, or angle brackets, and one that is
otherwise a keyword.

A dotted name such as Go's `main.counter` names a global when one has that
whole name; otherwise the dots select members. Parentheses make the choice
explicit: `(a).b` always selects the member `b` of `a`.

`$name` reads a register of the selected frame, such as `$rax` or `$rip`, and
`$pc`, `$sp`, and `$fp` name the program counter, stack pointer, and frame
pointer.

```uscope-example
ns::counter               => reads as `ns::counter`
::counter                 => reads as `::counter`
`github.com/acme/pkg.x`   => reads as `` `github.com/acme/pkg.x` ``
`Vec<i32>`::len           => reads as `` `Vec<i32>`::len ``
`as` + 1                  => reads as `` `as` + 1 ``
main.counter              => reads as `main.counter`
(main).counter            => reads as `(main).counter`
$rip                      => reads as `$rip`
naïve                     => error syntax at `ï`
$                         => error syntax at `$`
```

## Literals

Integers are decimal, or `0x`, `0o`, or `0b` prefixed, and may contain `_`.
A suffix naming a built-in type gives a literal that type: `255u8`, `1_i64`,
`3usize`. A literal without a suffix is an exact integer.

A leading zero is refused, because C reads `017` as octal and other languages
read it as decimal. C suffixes such as `UL` are refused too; cast instead.

Floats have a fraction or an exponent: `1.5`, `1e9`, `2.5e-3`. They are
binary64 unless suffixed `f32`, and a float never ends in `.`, so `a[1..4]`
is a range. `'a'` is a character's code point, and `"text"` is a string,
which compares with the program's text. Both take the escapes `\n`, `\r`,
`\t`, `\0`, `\\`, `\'`, `\"`, `\xHH`, and `\u{H…}`.

`true`, `false`, and `null` mean what they say. `nil`, `nullptr`, and `NULL`
are refused with a hint to write `null`, so that one spelling reads the same
in every language.

```uscope-example
0x2a + 0b1010_1010 + 0o17 => reads as `42 + 170 + 15`
0xffu8                    => reads as `255u8`
1_000_i64                 => reads as `1000i64`
2.5f32 + 1e3              => reads as `2.5f32 + 1000.0`
'\x41' == 'A'             => reads as `'A' == 'A'`
"tab\tquote\""            => reads as `"tab\tquote\""`
017                       => error syntax at `017`
17UL                      => error syntax at `17UL`
1.5u8                     => error syntax at `1.5u8`
1e400                     => error syntax at `1e400`
340282366920938463463374607431768211456 => error syntax at `340282366920938463463374607431768211456`
'ab'                      => error syntax at `'a`
nullptr                   => error syntax at `nullptr`
```

## Operators

From loosest to tightest:

| Operators | Grouping |
|---|---|
| `=` `+=` `-=` `*=` `/=` `%=` `&=` `\|=` `^=` `<<=` `>>=` | right |
| `?:` | right |
| `\|\|` | left |
| `&&` | left |
| `==` `!=` `<` `<=` `>` `>=` | none |
| `\|` | left |
| `^` | left |
| `&` | left |
| `<<` `>>` | left |
| `+` `-` | left |
| `*` `/` `%` | left |
| `as` | left |
| prefix `-` `!` `~` `*` `&`, casts `(T)x`, `sizeof` | right |
| postfix `.` `->` `[]` | left |

Bit operations bind tighter than comparisons, so `x & 1 == 0` means
`(x & 1) == 0`, and comparisons do not chain. `.` selects a member, and also
selects through one pointer; `->` selects through a pointer as in C;
`t.0` selects a tuple's field. `a[start..end]` is a half-open range of an
array or slice, and must be the whole expression. `len(x)` is a length and
`sizeof(x)` a size. The language does not call functions.

```uscope-example
a+b*c                     => reads as `a + b * c`
x & 1 == 0                => reads as `x & 1 == 0`
(x & 1) == 0              => reads as `x & 1 == 0`
x & (1 == 0)              => reads as `x & (1 == 0)`
1 << 70 >> 68             => reads as `1 << 70 >> 68`
a - (b - c)               => reads as `a - (b - c)`
a ? b : c ? d : e         => reads as `a ? b : c ? d : e`
-x as u8                  => reads as `-x as u8`
-(x as u8)                => reads as `-(x as u8)`
p->items[i + 1].0         => reads as `p->items[i + 1].0`
&(&x)                     => reads as `& &x`
a[1..4]                   => reads as `a[1..4]`
1 < 2 < 3                 => error syntax at `<`
a[1..4] + 1               => error syntax at `a[1..4]`
f(x)                      => error syntax at `f(`
a +                       => error syntax at ``
```

## Casts

`(T)x` and `x as T` convert a value. `T` may be a built-in type (`iN` and
`uN` for N from 1 to 128, `isize`, `usize`, `f32`, `f64`, `bool`), a type the
program defines, a C base type written with its words in any order, or a
`struct`, `union`, `enum`, or `class` tag. `const`, `volatile`, and `mut` are
accepted and ignored. A pointer type is `T*` inside a cast's parentheses,
where `(*p)` dereferences, and `*T` after `as`, where a trailing `*` would
multiply.

`(name) - 1` subtracts when `name` is a value and casts `-1` when it is a
type, and the two readings group the rest of the expression differently:
`(n) - a * b` is `n - (a * b)`, while `(T) - a * b` is `((T)(-a)) * b`. Such a
parenthesized name before `-`, `*`, or `&` is read whichever way the name
is, as C reads it, with a value in scope winning. Anything else after the
parentheses is unambiguous: `(n) + 1` always adds, and `(T)x` always casts.
An expression may hold at most four such names.

```uscope-example
(unsigned long)-1         => reads as `(unsigned long)-1`
(long unsigned)-1         => reads as `(unsigned long)-1`
(const char*)p            => reads as `(char*)p`
(struct node*)p           => reads as `(struct node*)p`
(T)(-1)                   => reads as `(T)(-1)`
(T)x                      => reads as `(T)x`
x as *const u8            => reads as `x as *u8`
x as unsigned long + 1    => reads as `x as unsigned long + 1`
x as T * 2                => reads as `x as T * 2`
(n) + 1                   => reads as `n + 1`
(n) - 1                   => reads as `(n) - 1`
sizeof(int*)              => reads as `sizeof(int*)`
(int)                     => error syntax at ``
(a)-(b)-(c)-(d)-(e)-f     => error limit at `(`
```

## Limits

An expression may be at most 4096 bytes long, nest at most 64 deep, and have
at most 1024 parts.

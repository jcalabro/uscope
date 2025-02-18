fn main() {
    // primitive types
    let boolean: bool = true;
    let character: char = 'A';
    let integer: i32 = 42;
    let unsigned_integer: u32 = 42;
    let floating_point: f64 = 3.14;
    let byte: u8 = 255;
    let short: i16 = -32000;
    let long: i64 = 1234567890;
    let long_long: i128 = 1234567890123456789;
    let unsigned_long_long: u128 = 12345678901234567890;
    let float: f32 = 2.71;
    let tuple: (i32, f64, char) = (10, 6.28, 'R');

    // string types
    let string_literal: &str = "Hello, world!";
    let string_object: String = String::from("Rust is fun!");

    // print all
    println!("Boolean: {}", boolean);
    println!("Character: {}", character);
    println!("Integer: {}", integer);
    println!("Unsigned Integer: {}", unsigned_integer);
    println!("Floating Point: {}", floating_point);
    println!("Byte: {}", byte); // sim:rustprint stops here
    println!("Short: {}", short);
    println!("Long: {}", long);
    println!("Long Long: {}", long_long);
    println!("Unsigned Long Long: {}", unsigned_long_long);
    println!("Float: {}", float);
    println!("Tuple: ({}, {}, {})", tuple.0, tuple.1, tuple.2);
    println!("String Literal: {}", string_literal);
    println!("String Object: {}", string_object);
}

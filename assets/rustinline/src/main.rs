#[inline(always)]
fn inlined_func(n: i32) {
    let res = n * 2;
    println!("{}!", res);
}

fn not_inlined_func(n: i32) {
    let res = n * 2;
    println!("{}!", res);
}

fn main() {
    not_inlined_func(123);
    inlined_func(456);
    not_inlined_func(789);
}

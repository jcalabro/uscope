fn func_e() {
    println!("func_e");
}

fn func_d() {
    func_e();
    println!("func_d");
}

fn func_c() {
    func_d();
    println!("func_c");
}

fn func_b() {
    func_c();
    println!("func_b");
}

fn func_a() {
    func_b();
    println!("func_a");
}

fn func_f() {
    // reverse the order
    println!("func_f");
    func_e();
}

fn main() {
    func_a();
    func_b();
    func_c();
    func_d();
    func_e();
    func_f();
}

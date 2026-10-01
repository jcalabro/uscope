mod support;

use std::fmt::Write as _;
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use object::{Object, ObjectSection, ObjectSymbol};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(name)
}

fn assert_success(output: std::process::Output) -> String {
    assert!(
        output.status.success(),
        "uscope failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("UTF-8 output")
}

fn assert_no_sgr(output: &str) {
    assert!(
        !output.contains("\x1b["),
        "unexpected terminal styling in {output:?}"
    );
}

#[test]
fn pid_only_attach_discovers_the_executable_and_quit_detaches() {
    let executable = fixture("build/test-programs/attach");
    let mut target = Command::new(&executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn attach target");
    let mut ready = [0_u8; 6];
    target
        .stdout
        .as_mut()
        .expect("target stdout")
        .read_exact(&mut ready)
        .expect("wait for target readiness");
    assert_eq!(&ready, b"READY\n");

    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args([
            "--batch",
            "--eval",
            "quit",
            "--attach",
            &target.id().to_string(),
        ])
        .output()
        .expect("attach uscope by PID");
    assert_success(output);

    target
        .stdin
        .as_mut()
        .expect("target stdin")
        .write_all(b"x")
        .expect("release detached target");
    assert_eq!(target.wait().expect("reap target").code(), Some(23));
}

#[test]
fn failed_pid_discovery_suggests_the_executable_fallback() {
    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args(["--attach", &i32::MAX.to_string(), "--batch"])
        .output()
        .expect("run uscope with a missing process");
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains("pass EXECUTABLE explicitly"), "{stderr}");
}

fn symbol_address(executable: &std::path::Path, name: &str) -> u64 {
    let data = fs::read(executable).expect("read fixture");
    let object = object::File::parse(data.as_slice()).expect("parse fixture");
    object
        .symbols()
        .find_map(|symbol| {
            (symbol.name().ok() == Some(name) && symbol.is_definition()).then(|| symbol.address())
        })
        .unwrap_or_else(|| panic!("fixture has no defined symbol named {name:?}"))
}

#[test]
fn redirected_and_batch_output_is_plain_unless_color_is_forced() {
    let executable = fixture("build/test-programs/basic");

    let automatic = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args(["--batch", "--eval", "help"])
        .arg(&executable)
        .output()
        .expect("run uscope with automatic color");
    assert_no_sgr(&assert_success(automatic));

    let forced = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args(["--batch", "--color", "always", "--eval", "help"])
        .arg(&executable)
        .output()
        .expect("run uscope with forced color");
    let stdout = assert_success(forced);
    assert!(stdout.contains("\x1b["), "missing forced color: {stdout:?}");

    let disabled = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .env("CLICOLOR_FORCE", "1")
        .args(["--batch", "--color", "never", "--eval", "help"])
        .arg(executable)
        .output()
        .expect("run uscope with color disabled");
    assert_no_sgr(&assert_success(disabled));
}

#[test]
fn forced_color_styles_both_stdout_and_stderr() {
    let executable = fixture("build/test-programs/basic");
    let mut child = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args(["--color", "always"])
        .arg(executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run uscope");
    child
        .stdin
        .take()
        .expect("stdin pipe")
        .write_all(b"invalid\nquit\n")
        .expect("write commands");
    let output = child.wait_with_output().expect("wait for uscope");
    let stdout = String::from_utf8(output.stdout.clone()).expect("UTF-8 output");
    let stderr = String::from_utf8(output.stderr.clone()).expect("UTF-8 error output");

    assert_success(output);
    assert!(
        stdout.contains("\x1b["),
        "stdout was not colored: {stdout:?}"
    );
    assert!(
        stderr.contains("\x1b["),
        "stderr was not colored: {stderr:?}"
    );
    assert!(stderr.contains("error"), "{stderr:?}");
}

#[test]
fn color_styles_source_metadata_but_never_source_text() {
    let executable = fixture("build/test-programs/basic");
    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args([
            "--batch",
            "--color",
            "always",
            "--eval",
            "break breakpoint_target",
            "--eval",
            "run",
        ])
        .arg(executable)
        .output()
        .expect("run uscope");
    let stdout = assert_success(output);
    let current = stdout
        .lines()
        .find(|line| line.contains("uint64_t breakpoint_target(void)"))
        .expect("current source line");
    let (_, source) = current.split_once("| ").expect("source separator");

    assert!(
        current.contains("\x1b["),
        "metadata was not styled: {current:?}"
    );
    assert!(
        !source.contains("\x1b["),
        "source text was styled: {current:?}"
    );
}

#[test]
fn batch_mode_executes_a_command_file() {
    let executable = fixture("build/test-programs/basic");
    assert!(
        executable.exists(),
        "missing test fixture; run `just build-test-programs`"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args(["--batch", "-c"])
        .arg(fixture("tests/fixtures/basic.uscope"))
        .arg(executable)
        .output()
        .expect("run uscope");
    let stdout = assert_success(output);

    assert_eq!(stdout.matches("stopped at breakpoint").count(), 2);
    assert!(stdout.contains("breakpoint_target at"));
    assert!(stdout.contains("basic.c:"));
    assert!(stdout.contains("inferior exited with status 0"));
}

#[test]
fn batch_mode_streams_commands_from_stdin() {
    let executable = fixture("build/test-programs/basic");
    assert!(
        executable.exists(),
        "missing test fixture; run `just build-test-programs`"
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .arg("--batch")
        .arg(executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run uscope");
    child
        .stdin
        .take()
        .expect("stdin pipe")
        .write_all(b"break breakpoint_target\nrun\nquit\n")
        .expect("write commands");
    let stdout = assert_success(child.wait_with_output().expect("wait for uscope"));

    assert!(stdout.contains("breakpoint 1 set"));
    assert!(stdout.contains("stopped at breakpoint"));
}

#[test]
fn batch_mode_prints_every_location_of_an_inline_breakpoint() {
    let executable = fixture("build/test-programs/inline-gcc-o2");
    assert!(
        executable.exists(),
        "missing test fixture; run `just build-test-programs`"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args(["--batch", "--eval", "break leaf"])
        .arg(executable)
        .output()
        .expect("run uscope");
    let stdout = assert_success(output);

    assert!(
        stdout.contains("breakpoint 1 set at 6 locations"),
        "{stdout}"
    );
    assert_eq!(stdout.matches("  image address ").count(), 6, "{stdout}");
}

#[test]
fn batch_mode_sets_lists_and_deletes_source_and_file_function_breakpoints() {
    let executable = fixture("build/test-programs/basic");
    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args([
            "--batch",
            "--eval",
            "break basic.c:11",
            "--eval",
            "break basic.c:breakpoint_target",
            "--eval",
            "breakpoints",
            "--eval",
            "del 1",
            "--eval",
            "d all",
            "--eval",
            "info breakpoints",
        ])
        .arg(executable)
        .output()
        .expect("run uscope");
    let stdout = assert_success(output);

    assert!(stdout.contains("1  basic.c:11  1 location"), "{stdout}");
    assert!(
        stdout.contains("2  basic.c:breakpoint_target  1 location"),
        "{stdout}"
    );
    assert!(stdout.contains("deleted breakpoint 1"), "{stdout}");
    assert!(stdout.contains("deleted 1 breakpoint"), "{stdout}");
    assert!(stdout.ends_with("no breakpoints\n"), "{stdout}");
}

#[test]
fn help_and_clear_are_generated_from_the_command_registry() {
    let executable = fixture("build/test-programs/basic");
    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args([
            "--batch",
            "--eval",
            "help",
            "--eval",
            "help clear",
            "--eval",
            "help del",
            "--eval",
            "help fin",
            "--eval",
            "help break",
            "--eval",
            "help where",
            "--eval",
            "help continue",
        ])
        .arg(executable)
        .output()
        .expect("run uscope");
    let stdout = assert_success(output);

    assert!(
        stdout.contains("  break        b       Set a breakpoint"),
        "{stdout}"
    );
    assert!(
        stdout.contains("  finish       fin, f  Run until the selected frame returns"),
        "{stdout}"
    );
    assert!(
        stdout.contains("  continue     c       Continue execution"),
        "{stdout}"
    );
    assert!(
        stdout.contains("  delete       del, d  Delete logical breakpoints"),
        "{stdout}"
    );
    assert!(
        stdout.contains("  clear        cls     Clear and redraw the terminal"),
        "{stdout}"
    );
    assert!(
        stdout.contains("  help         h, ?    Show command help"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("break <function|0xaddress|file:line|file:function> (b)"),
        "overview should omit detailed usage: {stdout}"
    );
    assert!(stdout.contains("delete <id|all>"), "{stdout}");
    assert!(stdout.contains("aliases: del, d"), "{stdout}");
    assert!(
        stdout.contains("  Clear and redraw the terminal\n  aliases: cls"),
        "{stdout}"
    );
    assert!(
        stdout.contains("  Run until the selected frame returns to its caller\n  aliases: fin, f"),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "  Set a breakpoint\n  aliases: b\n  usage: break <function|0xaddress|file:line|file:function>"
        ),
        "{stdout}"
    );
    assert!(!stdout.contains("break\n  Set a breakpoint"), "{stdout}");
    assert!(
        !stdout.contains("finish\n  Run until the selected frame returns"),
        "{stdout}"
    );
    assert!(
        stdout.contains("  Show the selected frame's execution location"),
        "{stdout}"
    );
    assert!(!stdout.contains("usage: where"), "{stdout}");
    assert!(
        stdout.contains("  Continue execution\n  aliases: c"),
        "{stdout}"
    );
    assert!(!stdout.contains("usage: continue"), "{stdout}");
    assert!(!stdout.contains("\x1b[2J\x1b[H"), "{stdout:?}");
}

#[test]
fn clear_refuses_redirected_output_without_emitting_terminal_controls() {
    for command in ["clear", "cls"] {
        let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
            .args(["--batch", "--eval", command])
            .arg(fixture("build/test-programs/basic"))
            .output()
            .expect("run uscope");
        let stdout = String::from_utf8(output.stdout).expect("UTF-8 output");
        let stderr = String::from_utf8(output.stderr).expect("UTF-8 error output");

        assert!(!output.status.success(), "{command} unexpectedly succeeded");
        assert!(!stdout.contains("\x1b[2J\x1b[H"), "{stdout:?}");
        assert!(
            stderr.contains("cannot clear screen: stdout is not an ANSI terminal"),
            "{stderr:?}"
        );
    }
}

#[test]
fn colored_help_distinguishes_commands_aliases_and_descriptions() {
    let executable = fixture("build/test-programs/basic");
    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args(["--batch", "--color", "always", "--eval", "help"])
        .arg(executable)
        .output()
        .expect("run uscope");
    let stdout = assert_success(output);

    assert!(stdout.contains("\x1b[1m\x1b[94mbreak\x1b[0m"), "{stdout:?}");
    assert!(stdout.contains("\x1b[94mb\x1b[0m"), "{stdout:?}");
    assert!(
        !stdout.contains("\x1b[94m\x1b[0m"),
        "empty aliases emitted styling: {stdout:?}"
    );
    assert!(stdout.contains("commands:"), "{stdout:?}");
    assert!(stdout.contains("Set a breakpoint"), "{stdout:?}");
    assert!(
        stdout.contains("Use `help <command>` for aliases and usage."),
        "{stdout:?}"
    );
    assert!(!stdout.contains("\x1b[2mcommands"), "{stdout:?}");
    assert!(!stdout.contains("\x1b[2mSet a breakpoint"), "{stdout:?}");
    assert!(
        !stdout.contains("\x1b[2mUse `help <command>`"),
        "{stdout:?}"
    );
}

#[test]
fn print_and_p_render_stack_scalars_and_generated_alias_help() {
    let executable = fixture("build/test-programs/variables-gcc-o0");
    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args([
            "--batch",
            "--eval",
            "help p",
            "--eval",
            "help where",
            "--eval",
            "break variables.c:108",
            "--eval",
            "run",
            "--eval",
            "p signed_int",
            "--eval",
            "print",
        ])
        .arg(executable)
        .output()
        .expect("run uscope");
    let stdout = assert_success(output);

    assert!(stdout.contains("print [value-path]\n"), "{stdout}");
    assert!(stdout.contains("aliases: p"), "{stdout}");
    assert!(
        stdout.contains("  Show the selected frame's execution location"),
        "{stdout}"
    );
    assert!(!stdout.contains("usage: where"), "{stdout}");
    assert!(
        !stdout.contains("  Show the selected frame's execution location\n  aliases:"),
        "{stdout}"
    );
    assert_eq!(stdout.matches("(int) signed_int = -1234567").count(), 2);
    assert!(stdout.contains("(char) character = 65 'A'"), "{stdout}");
    assert!(stdout.contains("(float) single = 1.25"), "{stdout}");
    assert!(
        stdout.contains("(double) double_precision = -2.5"),
        "{stdout}"
    );
    assert!(
        stdout.contains("(long double) extended = 3.125"),
        "{stdout}"
    );
}

#[test]
fn info_symbol_names_code_data_and_unnamed_addresses_by_section_and_module() {
    let executable = fixture("build/test-programs/variables-gcc-nopie");
    let main = symbol_address(&executable, "main");
    let data = symbol_address(&executable, "pointer_parameter_value");
    // _dl_relocate_static_pie is five bytes long and followed by padding.
    let padding = symbol_address(&executable, "_dl_relocate_static_pie") + 8;
    let commands = [
        format!("info symbol {:#x}", main + 4),
        format!("info symbol {:#x}", data + 2),
        format!("info symbol {padding:#x}"),
        "info symbol 0x8".to_owned(),
    ];
    let mut arguments = vec!["--batch", "--eval", "break main", "--eval", "run"];
    for command in &commands {
        arguments.extend(["--eval", command]);
    }
    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args(arguments)
        .arg(&executable)
        .output()
        .expect("run info symbol");
    let stdout = assert_success(output);
    let path = executable.canonicalize().expect("canonical fixture path");
    let path = path.display();

    for expected in [
        format!("main+0x4 in section .text of {path}"),
        format!("pointer_parameter_value+0x2 in section .data of {path}"),
        format!("no symbol contains {padding:#x} in section .text of {path}"),
        "no loaded module contains 0x8".to_owned(),
    ] {
        assert!(
            stdout.lines().any(|line| line == expected),
            "missing {expected:?} in:\n{stdout}"
        );
    }
}

#[test]
fn x_renders_a_bounded_hex_and_ascii_memory_view() {
    let executable = fixture("build/test-programs/variables-gcc-nopie");
    let address = symbol_address(&executable, "pointer_parameter_value");
    let command = format!("x {address:#x} 4");
    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args([
            "--batch",
            "--eval",
            "break main",
            "--eval",
            "run",
            "--eval",
            &command,
        ])
        .arg(executable)
        .output()
        .expect("run memory view");
    let stdout = assert_success(output);

    assert!(
        stdout.contains(&format!("{address:#018x}: 2a 00 00 00")),
        "{stdout}"
    );
    assert!(stdout.contains("|*...            |"), "{stdout}");
}

#[test]
fn print_and_p_render_parameters_and_locals() {
    let executable = fixture("build/test-programs/variables-parameters-gcc-o0");
    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args([
            "--batch",
            "--eval",
            "help print",
            "--eval",
            "break variables-parameters.c:22",
            "--eval",
            "run",
            "--eval",
            "p signed_int",
            "--eval",
            "print",
        ])
        .arg(executable)
        .output()
        .expect("run uscope");
    let stdout = assert_success(output);

    assert!(
        stdout.contains("Print variables, indexed values, members, or one bounded range"),
        "{stdout}"
    );
    assert_eq!(stdout.matches("(int) signed_int = -1234567").count(), 2);
    assert!(
        stdout.contains("(long double) extended = 3.125"),
        "{stdout}"
    );
    assert!(stdout.contains("(int) local = 99"), "{stdout}");
}

#[test]
fn print_explicitly_dereferences_pointer_chains_and_reports_typed_failures() {
    let executable = fixture("build/test-programs/variables-gcc-o0");
    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args([
            "--batch",
            "--eval",
            "break variables.c:68",
            "--eval",
            "run",
            "--eval",
            "p pointer",
            "--eval",
            "p *pointer",
            "--eval",
            "p **pointer_pointer",
            "--eval",
            "p *null_pointer",
            "--eval",
            "p *void_pointer",
            "--eval",
            "p *pointee",
            "--eval",
            "p **pointer",
            "--eval",
            "p *invalid_pointer",
        ])
        .arg(executable)
        .output()
        .expect("run pointer print commands");
    let stdout = assert_success(output);
    assert!(stdout.contains("(int *) pointer = 0x"), "{stdout}");
    assert!(stdout.contains("(int) *pointer = 42"), "{stdout}");
    assert!(stdout.contains("(int) **pointer_pointer = 42"), "{stdout}");
    // A failed dereference is labeled with the pointee type (int), not the
    // operand's pointer type (int *).
    assert!(
        stdout.contains("(int) *null_pointer = <unavailable: cannot dereference a null pointer>"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("(int *) *null_pointer"),
        "failed dereference must not render the pointer's own type: {stdout}"
    );
    assert!(stdout.contains("no concrete pointee type"), "{stdout}");
    assert!(
        stdout
            .matches("the value is not a pointer or reference")
            .count()
            >= 2,
        "{stdout}"
    );
    assert!(
        stdout.contains("*invalid_pointer = <unavailable:"),
        "{stdout}"
    );
}

#[test]
fn print_selects_members_and_implicitly_dereferences_only_intermediate_pointers() {
    let executable = fixture("build/test-programs/variables-gcc-o0");
    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args([
            "--batch",
            "--eval",
            "break variables.c:68",
            "--eval",
            "run",
            "--eval",
            "p pair.first",
            "--eval",
            "p pair.second",
            "--eval",
            "p structure_pointer.first",
            "--eval",
            "p const_pointee",
            "--eval",
            "p const_pointer",
            "--eval",
            "p array_pointer",
            "--eval",
            "p recursive_pointer.next",
            "--eval",
            "p recursive_pointer.next.next.value",
            "--eval",
            "p recursive_pointer.next.next.next.value",
        ])
        .arg(executable)
        .output()
        .expect("run structural print commands");
    let stdout = assert_success(output);

    assert!(stdout.contains("(int) pair.first = 20"), "{stdout}");
    assert!(stdout.contains("(int) pair.second = 22"), "{stdout}");
    assert!(
        stdout.contains("(int) structure_pointer.first = 20"),
        "{stdout}"
    );
    assert!(
        stdout.contains("(const int *) const_pointee = 0x"),
        "{stdout}"
    );
    assert!(
        stdout.contains("(int * const) const_pointer = 0x"),
        "{stdout}"
    );
    assert!(
        stdout.contains("(int (*)[2]) array_pointer = 0x"),
        "{stdout}"
    );
    assert!(
        stdout.contains("(pointer_node *) recursive_pointer.next = 0x"),
        "terminal pointer must render its address: {stdout}"
    );
    assert!(
        !stdout.contains("<recursive type>"),
        "construction placeholders must not escape into finalized names: {stdout}"
    );
    assert!(
        stdout.contains("(int) recursive_pointer.next.next.value = 42"),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "(int) recursive_pointer.next.next.next.value = <unavailable: cannot dereference a null pointer>"
        ),
        "{stdout}"
    );
}

#[test]
fn print_resolves_recursive_types_from_dwarf_four_and_five_type_units() {
    for fixture_name in [
        "build/test-programs/types-cpp-gcc-dwarf4",
        "build/test-programs/types-cpp-gcc-dwarf5",
    ] {
        let executable = fixture(fixture_name);
        let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
            .args([
                "--batch",
                "--eval",
                "break types.cpp:31",
                "--eval",
                "run",
                "--eval",
                "p recursive.value",
                "--eval",
                "p mutual.peer",
            ])
            .arg(executable)
            .output()
            .expect("run type-unit print commands");
        let stdout = assert_success(output);

        assert!(
            stdout.contains("(volatile alias_chain) recursive.value = 9"),
            "{fixture_name}: {stdout}"
        );
        assert!(
            stdout.contains("(right *) mutual.peer = 0x"),
            "{fixture_name}: {stdout}"
        );
    }
}

#[test]
fn print_renders_rust_slice_elements() {
    let executable = fixture("build/test-programs/variables-rust-o0");
    assert!(
        executable.exists(),
        "missing test fixture; run `just build-test-programs`"
    );
    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args([
            "--batch",
            "--eval",
            "break variables.rs:67",
            "--eval",
            "run",
            "--eval",
            "print slice",
        ])
        .arg(executable)
        .output()
        .expect("run uscope");
    let stdout = assert_success(output);
    assert!(stdout.contains("(&[i32]) slice = [20, 22]"), "{stdout}");
}

#[test]
fn print_renders_nested_records_arrays_and_bit_fields() {
    let executable = fixture("build/test-programs/records-c-gcc-o0");
    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args([
            "--batch",
            "--eval",
            "break inspect_records",
            "--eval",
            "run",
            "--eval",
            "print global_record",
            "--eval",
            "print *bits",
            "--eval",
            "print *records",
            "--eval",
            "print huge_array[1048576]",
            "--eval",
            "print huge_array[3..7]",
            "--eval",
            "print (*records)[1].values[1]",
        ])
        .arg(executable)
        .output()
        .expect("run record print commands");
    let stdout = assert_success(output);
    assert!(
        stdout.contains(
            "global_record = {inner = {signed_value = -7, unsigned_value = 9}, values = [20, 22]}"
        ),
        "{stdout}"
    );
    assert!(
        stdout.contains("*bits = {negative = -3, first = 5, second = 42}"),
        "{stdout}"
    );
    assert!(stdout.contains("values = [43, 44]"), "{stdout}");
    assert!(
        stdout.contains("(unsigned char) huge_array[1048576] = 0"),
        "{stdout}"
    );
    assert!(
        stdout.contains("huge_array[3..7] = [0, 0, 0, 0]"),
        "{stdout}"
    );
    assert!(
        stdout.contains("(int32_t) (*records)[1].values[1] = 44"),
        "{stdout}"
    );
}

#[test]
fn print_renders_symbolic_enums_variants_and_raw_unions() {
    let rust = fixture("build/test-programs/enums-rust-o0");
    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args([
            "--batch",
            "--eval",
            "break inspect_enum",
            "--eval",
            "run",
            "--eval",
            "print *value",
            "--eval",
            "print *fieldless",
            "--eval",
            "print *wide",
        ])
        .arg(rust)
        .output()
        .expect("render Rust enum values");
    let stdout = assert_success(output);
    assert!(
        stdout.contains("*value = {Integer = {__0 = 42}}"),
        "{stdout}"
    );
    assert!(stdout.contains("*fieldless = Negative (-3)"), "{stdout}");
    assert!(
        stdout.contains("*wide = Huge (1267650600228229401496703205385)"),
        "{stdout}"
    );

    let c = fixture("build/test-programs/enums-c-gcc-o0");
    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args([
            "--batch",
            "--eval",
            "break inspect_enums",
            "--eval",
            "run",
            "--eval",
            "print *raw",
        ])
        .arg(c)
        .output()
        .expect("render raw C union");
    let stdout = assert_success(output);
    assert!(
        stdout.contains("*raw = {integer = 42, floating = ")
            && stdout.contains("} <active member unknown>"),
        "{stdout}"
    );
}

#[test]
fn globals_lists_metadata_and_print_accepts_exact_qualification() {
    let executable = fixture("build/test-programs/globals-c-gcc-o0");
    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args([
            "--batch",
            "--eval",
            "help globals",
            "--eval",
            "globals duplicate",
            "--eval",
            "break main.c:9",
            "--eval",
            "run",
            "--eval",
            "print one.c::duplicate",
            "--eval",
            "continue",
        ])
        .arg(&executable)
        .output()
        .expect("run uscope global commands");
    let stdout = assert_success(output);
    assert!(stdout.contains("globals [filter]"), "{stdout}");
    assert!(stdout.contains("duplicate ("), "{stdout}");
    assert!(stdout.contains("duplicate = 201"), "{stdout}");

    let ambiguous = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args([
            "--batch",
            "--eval",
            "break main.c:9",
            "--eval",
            "run",
            "--eval",
            "print duplicate",
        ])
        .arg(executable)
        .output()
        .expect("run ambiguous global command");
    assert!(!ambiguous.status.success());
    let stderr = String::from_utf8(ambiguous.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains("global variable selector 'duplicate' is ambiguous"));
    assert!(stderr.contains("one.c"), "{stderr}");
    assert!(stderr.contains("two.c"), "{stderr}");
}

#[test]
fn batch_mode_reports_command_context() {
    let executable = fixture("build/test-programs/basic");
    assert!(
        executable.exists(),
        "missing test fixture; run `just build-test-programs`"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args(["--batch", "--color", "always", "--eval", "invalid"])
        .arg(executable)
        .output()
        .expect("run uscope");
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 error output");

    assert!(!output.status.success());
    assert!(stderr.contains("\x1b[1m\x1b[91merror\x1b[0m"), "{stderr:?}");
    assert!(stderr.contains("--eval #1"));
    assert!(stderr.contains("unknown command 'invalid'"), "{stderr}");
}

#[test]
fn command_argument_errors_use_the_registered_canonical_usage() {
    let executable = fixture("build/test-programs/basic");

    let extra = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args(["--batch", "--eval", "run ignored"])
        .arg(&executable)
        .output()
        .expect("run uscope");
    let stderr = String::from_utf8(extra.stderr).expect("UTF-8 error output");
    assert!(!extra.status.success());
    assert!(stderr.contains("usage: run"), "{stderr}");

    let missing = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args(["--batch", "--eval", "break"])
        .arg(&executable)
        .output()
        .expect("run uscope");
    let stderr = String::from_utf8(missing.stderr).expect("UTF-8 error output");
    assert!(!missing.status.success());
    assert!(
        stderr.contains("usage: break <function|0xaddress|file:line|file:function>"),
        "{stderr}"
    );

    // Only `info symbol` takes an address, and it requires one.
    for command in ["info symbol", "info breakpoints 0x10"] {
        let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
            .args(["--batch", "--eval", command])
            .arg(&executable)
            .output()
            .expect("run uscope");
        let stderr = String::from_utf8(output.stderr).expect("UTF-8 error output");
        assert!(!output.status.success(), "{command}");
        assert!(
            stderr.contains("usage: info breakpoints|watchpoints|core|symbol [0xaddress]"),
            "{command}: {stderr}"
        );
    }
}

#[test]
fn batch_mode_prints_a_nested_backtrace() {
    let executable = fixture("build/test-programs/unwind-o2");
    assert!(
        executable.exists(),
        "missing test fixture; run `just build-test-programs`"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args([
            "--batch",
            "--eval",
            "break deepest",
            "--eval",
            "run",
            "--eval",
            "backtrace",
        ])
        .arg(executable)
        .output()
        .expect("run uscope");
    let stdout = assert_success(output);
    let deepest = stdout.find("in deepest").expect("deepest frame");
    let middle = stdout.find("in middle").expect("middle frame");
    let outer = stdout.find("in outer").expect("outer frame");
    let main = stdout.find("in main").expect("main frame");

    assert!(
        deepest < middle && middle < outer && outer < main,
        "{stdout}"
    );
    assert!(stdout.contains("unwind stopped:"));
}

#[test]
fn batch_mode_prints_registers_one_per_line() {
    let executable = fixture("build/test-programs/basic");
    assert!(
        executable.exists(),
        "missing test fixture; run `just build-test-programs`"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args([
            "--batch",
            "--eval",
            "break breakpoint_target",
            "--eval",
            "run",
            "--eval",
            "registers",
        ])
        .arg(executable)
        .output()
        .expect("run uscope");
    let stdout = assert_success(output);

    assert!(stdout.lines().any(|line| line.starts_with("rax ")));
    assert!(stdout.lines().any(|line| line.starts_with("rsp ")));
    assert!(stdout.lines().any(|line| line.starts_with("rip ")));
}

#[test]
fn batch_mode_lists_threads_and_steps_one_instruction() {
    let executable = fixture("build/test-programs/basic");
    assert!(
        executable.exists(),
        "missing test fixture; run `just build-test-programs`"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args([
            "--batch",
            "--eval",
            "break breakpoint_target",
            "--eval",
            "run",
            "--eval",
            "threads",
            "--eval",
            "stepi",
        ])
        .arg(executable)
        .output()
        .expect("run uscope");
    let stdout = assert_success(output);

    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("* ") && line.contains(" stopped")),
        "{stdout}"
    );
    assert!(stdout.contains("stopped after instruction step"));
}

#[test]
fn breakpoint_stops_print_source_context_from_any_working_directory() {
    let executable = fixture("build/test-programs/basic");
    assert!(
        executable.exists(),
        "missing test fixture; run `just build-test-programs`"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .current_dir("/")
        .args([
            "--batch",
            "--eval",
            "break breakpoint_target",
            "--eval",
            "run",
        ])
        .arg(executable)
        .output()
        .expect("run uscope");
    let stdout = assert_success(output);

    assert!(stdout.contains("tests/fixtures/c/basic.c:6"));
    assert!(stdout.contains("=> 6 |     return uscope_value;"));
    assert!(stdout.contains("   8 |"));
}

#[test]
fn source_maps_read_sources_recorded_under_another_directory() {
    let executable = fixture("build/test-programs/basic-relocated");
    let arguments = [
        "--batch",
        "--eval",
        "break breakpoint_target",
        "--eval",
        "run",
    ];
    let unmapped = assert_success(uscope_batch(&arguments[1..], &executable));
    assert!(
        unmapped.contains(
            "source unavailable: source file /nonexistent/uscope/tests/fixtures/c/basic.c does not exist"
        ),
        "{unmapped}"
    );

    let repository = env!("CARGO_MANIFEST_DIR");
    let mut mapped = arguments[1..].to_vec();
    mapped.extend(["--source-map", "/nonexistent/uscope", repository]);
    let stdout = assert_success(uscope_batch(&mapped, &executable));
    assert!(
        stdout.contains(&format!("{repository}/tests/fixtures/c/basic.c:6")),
        "{stdout}"
    );
    assert!(
        stdout.contains("=> 6 |     return uscope_value;"),
        "{stdout}"
    );

    assert_failure(
        &uscope(&["build/test-programs/basic", "--source-map", "/nonexistent"]),
        "2 values required for '--source-map <FROM> <TO>'",
    );
}

#[test]
fn list_command_prints_the_current_source_context() {
    let executable = fixture("build/test-programs/basic");
    assert!(
        executable.exists(),
        "missing test fixture; run `just build-test-programs`"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args([
            "--batch",
            "--eval",
            "break breakpoint_target",
            "--eval",
            "run",
            "--eval",
            "l",
        ])
        .arg(executable)
        .output()
        .expect("run uscope");
    let stdout = assert_success(output);

    assert_eq!(stdout.matches("tests/fixtures/c/basic.c:6").count(), 2);
    assert_eq!(stdout.matches("=> 6 |     return uscope_value;").count(), 2);
}

#[test]
fn plain_repl_preserves_output_and_exits_at_eof() {
    let executable = fixture("build/test-programs/basic");
    assert!(
        executable.exists(),
        "missing test fixture; run `just build-test-programs`"
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .arg(executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run uscope");
    child
        .stdin
        .take()
        .expect("stdin pipe")
        .write_all(b"break breakpoint_target\nrun\nregisters\n")
        .expect("write commands");
    let stdout = assert_success(child.wait_with_output().expect("wait for uscope"));

    assert!(stdout.starts_with("debugging "));
    assert!(stdout.lines().any(|line| line.starts_with("rax ")));
    assert!(stdout.lines().any(|line| line.starts_with("orig_rax ")));
}

#[test]
fn plain_repl_reports_errors_and_continues() {
    let executable = fixture("build/test-programs/basic");
    assert!(
        executable.exists(),
        "missing test fixture; run `just build-test-programs`"
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .arg(executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run uscope");
    child
        .stdin
        .take()
        .expect("stdin pipe")
        .write_all(b"invalid\nquit\n")
        .expect("write commands");
    let output = child.wait_with_output().expect("wait for uscope");
    let stderr = String::from_utf8(output.stderr.clone()).expect("UTF-8 error output");

    assert_success(output);
    assert!(stderr.contains("unknown command 'invalid'"), "{stderr}");
}

#[test]
fn ctrl_c_shuts_down_and_reaps_a_running_inferior() {
    let executable = fixture("build/test-programs/spin");
    assert!(
        executable.exists(),
        "missing test fixture; run `just build-test-programs`"
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .arg(executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run uscope");
    child
        .stdin
        .take()
        .expect("stdin pipe")
        .write_all(b"run\n")
        .expect("write command");

    let debugger_pid = child.id();
    let inferior_pid = wait_for_child_process(debugger_pid).unwrap_or_else(|| {
        child.kill().expect("kill debugger after test timeout");
        child.wait().expect("reap debugger after test timeout");
        panic!("debugger did not launch an inferior");
    });

    kill(
        Pid::from_raw(i32::try_from(debugger_pid).expect("debugger PID fits i32")),
        Signal::SIGINT,
    )
    .expect("interrupt uscope");

    let output = child.wait_with_output().expect("wait for uscope");

    assert_success(output);
    assert!(
        !PathBuf::from(format!("/proc/{inferior_pid}")).exists(),
        "inferior {inferior_pid} survived debugger shutdown"
    );
}

#[test]
fn ctrl_c_pauses_a_running_inferior_before_accepting_more_commands() {
    let executable = fixture("build/test-programs/spin");
    assert!(
        executable.exists(),
        "missing test fixture; run `just build-test-programs`"
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .arg(&executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run uscope");
    let mut stdin = child.stdin.take().expect("stdin pipe");
    stdin.write_all(b"run\n").expect("write run command");

    let debugger_pid = child.id();
    let inferior_pid = wait_for_child_process(debugger_pid).expect("debugger launched inferior");
    wait_for_running_executable(inferior_pid, &executable);
    kill(
        Pid::from_raw(i32::try_from(debugger_pid).expect("debugger PID fits i32")),
        Signal::SIGINT,
    )
    .expect("pause uscope");
    stdin
        .write_all(b"registers\nquit\n")
        .expect("write inspection commands");
    drop(stdin);

    let stdout = assert_success(child.wait_with_output().expect("wait for uscope"));
    assert!(stdout.contains("inferior paused"), "{stdout}");
    assert!(stdout.lines().any(|line| line.starts_with("rip ")));
    assert!(
        !PathBuf::from(format!("/proc/{inferior_pid}")).exists(),
        "inferior {inferior_pid} survived debugger shutdown"
    );
}

#[test]
fn continuing_past_a_sigint_stop_discards_the_interrupt() {
    // A terminal Ctrl-C signals the inferior as well as uscope. Like gdb, the
    // CLI must not deliver that SIGINT, which would terminate the inferior.
    let stdout = assert_success(uscope_batch(
        &["-e", "run", "-e", "continue"],
        &fixture("build/test-programs/interrupt"),
    ));
    assert!(stdout.contains("stopped by SIGINT"), "{stdout}");
    assert!(stdout.contains("inferior exited with status 0"), "{stdout}");
}

fn wait_for_child_process(parent: u32) -> Option<u32> {
    let tasks = PathBuf::from(format!("/proc/{parent}/task"));
    let deadline = Instant::now() + Duration::from_secs(2);

    loop {
        for task in fs::read_dir(&tasks).expect("read debugger tasks") {
            let children = task.expect("read debugger task").path().join("children");
            let contents = fs::read_to_string(children).expect("read debugger children");

            if let Some(pid) = contents.split_whitespace().next() {
                return Some(pid.parse().expect("numeric inferior PID"));
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Waits until `pid` has exec'd `executable` and is no longer stopped.
///
/// A freshly forked child is also running before it execs, and a pause there
/// completes at the launch's exec stop instead of interrupting running code.
fn wait_for_running_executable(pid: u32, executable: &Path) {
    let status = PathBuf::from(format!("/proc/{pid}/status"));
    let exe = PathBuf::from(format!("/proc/{pid}/exe"));
    let executable = executable.canonicalize().expect("canonical fixture path");
    let deadline = Instant::now() + Duration::from_secs(2);

    loop {
        let execed = fs::read_link(&exe).is_ok_and(|path| path == executable);
        let contents = fs::read_to_string(&status).expect("read inferior status");
        let stopped = contents
            .lines()
            .find_map(|line| line.strip_prefix("State:"))
            .is_some_and(|state| state.trim_start().starts_with(['T', 't']));
        if execed && !stopped {
            return;
        }
        assert!(Instant::now() < deadline, "inferior did not start running");
        thread::sleep(Duration::from_millis(10));
    }
}

fn uscope(arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args(arguments)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::null())
        .output()
        .expect("run uscope")
}

fn assert_failure(output: &std::process::Output, expected: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success() && stderr.contains(expected),
        "expected failure containing {expected:?}:\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
}

const SEGV_CORE: &str = "build/test-programs/crash-gcc-o0-segv.core";

#[test]
fn core_dumps_are_inspected_in_batch_mode_without_executing() {
    let output = uscope(&[
        "--core",
        SEGV_CORE,
        "--batch",
        "--eval",
        "where",
        "--eval",
        "bt",
        "--eval",
        "threads",
        "--eval",
        "print depth",
        "--eval",
        "print crash_library_tls",
        "--eval",
        "info core",
    ]);
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = assert_success(output);
    assert!(stdout.starts_with("crash_segv at "), "{stdout}");
    assert!(stdout.contains("#1 "), "{stdout}");
    assert!(stdout.contains("in main at "), "{stdout}");
    assert!(
        stdout.contains("unwind stopped: the outermost frame has no caller"),
        "{stdout}"
    );
    assert_eq!(
        stdout
            .matches("process terminated by SIGSEGV (SEGV_MAPERR) at 0x0 (0xb)")
            .count(),
        1,
        "only the faulting thread carries the reason: {stdout}"
    );
    assert!(stdout.contains("(int32_t) depth = 3"), "{stdout}");
    assert!(stdout.contains("crash_library_tls = 654"), "{stdout}");
    assert!(
        stdout.contains("signal: SIGSEGV (SEGV_MAPERR) at 0x0"),
        "{stdout}"
    );
    assert!(
        stdout.contains("crash-gcc-o0 module 0 verified by build-id"),
        "{stdout}"
    );
    assert_no_sgr(&stdout);

    for command in [
        "run",
        "continue",
        "step",
        "next",
        "finish",
        "stepi",
        "break main",
    ] {
        assert_failure(
            &uscope(&["--core", SEGV_CORE, "--batch", "--eval", command]),
            "a post-mortem core dump cannot execute, be modified, or hold breakpoints",
        );
    }
}

#[test]
fn core_banner_reports_the_signal_and_every_unproven_module() {
    let stdout = assert_success(uscope(&["--core", SEGV_CORE]));
    let mut lines = stdout.lines();
    let banner = lines.next().unwrap_or_default();
    assert!(
        banner.starts_with("opened core dump ") && banner.contains("of crash-gcc-o0 (process "),
        "{stdout}"
    );
    assert_eq!(
        lines.next(),
        Some("process terminated by SIGSEGV (SEGV_MAPERR) at 0x0 (0xb)")
    );
    assert!(!stdout.contains("warning"), "{stdout}");

    // Warnings go to stderr, keeping stdout clean.
    let missing = uscope(&[
        "--core",
        "build/test-programs/core-missing-library/crash.core",
    ]);
    let stderr = String::from_utf8_lossy(&missing.stderr).into_owned();
    assert!(
        stderr.contains("core-missing-library/libcrash.so is missing; its frames and unsaved memory are unavailable"),
        "{stderr}"
    );
    assert!(!assert_success(missing).contains("warning"));
    let batch = uscope(&[
        "--core",
        "build/test-programs/core-missing-library/crash.core",
        "--batch",
    ]);
    assert!(String::from_utf8_lossy(&batch.stderr).contains("libcrash.so is missing"));
    assert_eq!(assert_success(batch), "");
}

#[test]
fn core_identity_mismatches_require_the_explicit_override() {
    assert_failure(
        &uscope(&[
            "--core",
            SEGV_CORE,
            "build/test-programs/crash-gcc-o0-rebuilt",
            "--batch",
        ]),
        "does not match the image recorded in the core dump: the build-id note differs; allow module mismatches",
    );
    let allowed = uscope(&[
        "--core",
        SEGV_CORE,
        "build/test-programs/crash-gcc-o0-rebuilt",
        "--allow-module-mismatch",
        "--batch",
        "--eval",
        "info core",
    ]);
    let stderr = String::from_utf8_lossy(&allowed.stderr).into_owned();
    assert!(
        stderr.contains("crash-gcc-o0-rebuilt does not match the dump (the build-id note differs); using its metadata anyway"),
        "{stderr}"
    );
    // The line names the file used when it is not at the recorded path.
    let stdout = assert_success(allowed);
    let rebuilt = fs::canonicalize(fixture("build/test-programs/crash-gcc-o0-rebuilt"))
        .expect("canonical fixture");
    assert!(
        stdout.contains(&format!(
            "crash-gcc-o0 module 0 from {} mismatched: the build-id note differs",
            rebuilt.display()
        )),
        "{stdout}"
    );

    assert_failure(
        &uscope(&[
            "--core",
            "build/test-programs/core-missing-executable/crash.core",
            "--batch",
        ]),
        "no longer exists; supply the executable explicitly",
    );
    assert_failure(
        &uscope(&["--core", "build/test-programs/crash-gcc-o0", "--batch"]),
        "invalid core dump: the ELF file is not a core dump",
    );
}

#[test]
fn core_module_searches_name_the_files_they_use_and_those_still_missing() {
    const FOREIGN_CORE: &str = "build/test-programs/core-foreign/crash.core";
    for option in ["--sysroot", "--module-path"] {
        assert_failure(
            &uscope(&[option, "/", "build/test-programs/basic"]),
            "--core <CORE>",
        );
    }
    assert_failure(
        &uscope(&["--core", FOREIGN_CORE, "--module-path", "absent", "--batch"]),
        "cannot search absent for core dump modules: No such file or directory",
    );

    // Without the C library, its recorded build-id says what to supply.
    let directory = support::ScratchDir::new("cli-module-path");
    for name in ["crash-gcc-o0", "libcrash.so"] {
        fs::copy(
            fixture(&format!("build/test-programs/{name}")),
            directory.path().join(name),
        )
        .expect("copy module");
    }
    let module_path = directory.path().to_str().expect("UTF-8 scratch path");
    let partial = uscope(&[
        "--core",
        FOREIGN_CORE,
        "--module-path",
        module_path,
        "--batch",
        "--eval",
        "info core",
    ]);
    let stderr = String::from_utf8_lossy(&partial.stderr).into_owned();
    let stdout = assert_success(partial);
    let libc = object::File::parse(
        fs::read(fixture("build/test-programs/libc-foreign.so.6"))
            .expect("read foreign C library")
            .as_slice(),
    )
    .expect("parse foreign C library")
    .build_id()
    .expect("read build-id")
    .expect("build-id note")
    .iter()
    .fold(String::new(), |mut text, byte| {
        write!(text, "{byte:02x}").expect("writing to a String cannot fail");
        text
    });
    assert!(
        stderr.contains(&format!(
            "core-foreign/libc.so.6 is missing; its frames and unsaved memory are unavailable (build-id {libc})"
        )),
        "{stderr}"
    );
    assert!(
        stdout.contains(&format!("core-foreign/libc.so.6 missing (build-id {libc})")),
        "{stdout}"
    );
    let canonical = directory
        .path()
        .canonicalize()
        .expect("canonical scratch path");
    for name in ["crash-gcc-o0", "libcrash.so"] {
        let line = stdout
            .lines()
            .find(|line| line.contains(&format!("core-foreign/{name} module")))
            .unwrap_or_else(|| panic!("no {name} line:\n{stdout}"));
        assert!(
            line.ends_with(&format!(
                " from {} verified by build-id",
                canonical.join(name).display()
            )),
            "{line}"
        );
    }
}

#[test]
fn core_arguments_conflict_with_live_targets() {
    assert_failure(
        &uscope(&["--core", SEGV_CORE, "--attach", "1"]),
        "cannot be used with",
    );
    assert_failure(
        &uscope(&["--allow-module-mismatch", "build/test-programs/basic"]),
        "--core <CORE>",
    );
    assert_failure(
        &uscope(&[
            "build/test-programs/basic",
            "--batch",
            "--eval",
            "info core",
        ]),
        "no core dump is open",
    );
}

#[test]
fn backtraces_name_source_files_from_each_frames_own_module() {
    let stdout = assert_success(uscope(&[
        "build/test-programs/module-frames-gcc-o0",
        "--batch",
        "--eval",
        "break module_callback",
        "--eval",
        "run",
        "--eval",
        "bt",
    ]));
    let library_frame = stdout
        .lines()
        .find(|line| line.contains(" in dso_apply"))
        .unwrap_or_else(|| panic!("no shared-library frame: {stdout}"));
    assert!(
        library_frame.ends_with("tests/fixtures/c/module-frames/library.c:6"),
        "{library_frame}"
    );
}

#[test]
fn backtraces_and_where_name_code_without_debug_info_by_symbol_and_module() {
    let stdout = assert_success(uscope(&[
        "--core",
        "build/test-programs/elf-symbols-stripped.core",
        "--batch",
        "--eval",
        "bt",
        "--eval",
        "where",
    ]));
    let lines = stdout.lines().collect::<Vec<_>>();
    let frame = |needle: &str| {
        lines
            .iter()
            .find(|line| line.starts_with('#') && line.contains(needle))
            .unwrap_or_else(|| panic!("no frame containing {needle:?}:\n{stdout}"))
    };
    // An unsized symbol says so; code no symbol names stays unknown, even
    // where stripping removed a static function between named ones.
    assert!(
        frame(" in asm_unsized+0x6 ")
            .ends_with(" (unsized symbol) from libelf-symbols-stripped.so"),
        "{stdout}"
    );
    assert!(frame(" in asm_sized+0x6 ").ends_with(" from libelf-symbols-stripped.so"));
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.ends_with(" in <unknown> from libelf-symbols-stripped.so"))
            .count(),
        2,
        "{stdout}"
    );
    assert!(!stdout.contains("lib_static_helper"), "{stdout}");
    // Frames with source keep showing it instead of a module.
    assert!(frame(" in chain_gap at ").ends_with("tests/fixtures/c/elf-symbols/main.c:69"));
    assert!(frame(" in _start+0x").ends_with(" from elf-symbols-stripped"));
    let location = lines.last().expect("where output");
    assert!(
        location.starts_with("lib_fault at 0x")
            && location.ends_with(" from libelf-symbols-stripped.so"),
        "{stdout}"
    );
}

fn frames_line(needle: &str) -> usize {
    fs::read_to_string(fixture("tests/fixtures/c/frames.c"))
        .expect("read frames fixture")
        .lines()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("frames.c has no line containing {needle:?}"))
        + 1
}

/// Returns the index of the first output line at or after `from` that
/// satisfies `predicate`.
fn line_index(lines: &[&str], from: usize, predicate: impl Fn(&str) -> bool) -> usize {
    lines[from..]
        .iter()
        .position(|line| predicate(line))
        .map_or_else(
            || panic!("no matching line after {from} in:\n{}", lines.join("\n")),
            |index| from + index,
        )
}

#[test]
fn frame_commands_move_through_caller_frames_and_show_their_source() {
    let stdout = assert_success(uscope(&[
        "--core",
        "build/test-programs/frames-gcc-o0.core",
        "--batch",
        "--eval",
        "frame 5",
        "--eval",
        "print depth",
        "--eval",
        "up",
        "--eval",
        "print depth",
        "--eval",
        "where",
        "--eval",
        "down 2",
        "--eval",
        "list",
        "--eval",
        "frame",
        "--eval",
        "up 100",
        "--eval",
        "down",
    ]));
    let lines = stdout.lines().collect::<Vec<_>>();
    let recursive_call = format!(
        "frames.c:{}",
        frames_line("int64_t below = frames_recurse(depth - 1, seed);")
    );
    let innermost_call = frames_line("return frames_keep(seed) + level;");

    let frame = line_index(&lines, 0, |line| {
        line.starts_with("#5 ")
            && line.contains(" in frames_recurse at ")
            && line.ends_with(&recursive_call)
    });
    // The frame's source follows it, marking its call.
    let marked = line_index(&lines, frame, |line| line.starts_with("=> "));
    assert!(lines[marked].contains("frames_recurse(depth - 1, seed)"));
    let depth = line_index(&lines, marked, |line| line == "(int64_t) depth = 1");
    let up = line_index(&lines, depth, |line| line.starts_with("#6 "));
    let depth = line_index(&lines, up, |line| line == "(int64_t) depth = 2");
    let location = line_index(&lines, depth, |line| {
        line.starts_with("frames_recurse at ") && line.contains(&format!("{recursive_call} (0x"))
    });
    let down = line_index(&lines, location, |line| {
        line.starts_with("#4 ") && line.ends_with(&format!("frames.c:{innermost_call}"))
    });
    let listed = line_index(&lines, down + 1, |line| {
        line.starts_with(&format!("=> {innermost_call} |"))
    });
    let shown = line_index(&lines, listed, |line| line.starts_with("#4 "));
    let outermost = line_index(&lines, shown, |line| {
        line.starts_with("#11 ") && line.contains(" in _start+0x")
    });
    line_index(&lines, outermost, |line| line.starts_with("#10 "));
}

#[test]
fn frame_commands_report_the_ends_of_the_stack() {
    for (commands, expected) in [
        (&["down"][..], "the innermost frame is selected"),
        (&["up 100", "up"], "the outermost frame is selected"),
        (
            &["frame 99"],
            "frame 99 does not exist; the backtrace has 12 frames",
        ),
        (&["up x"], "usage: up [count]"),
    ] {
        let mut arguments = vec![
            "--core",
            "build/test-programs/frames-gcc-o0.core",
            "--batch",
        ];
        for command in commands {
            arguments.extend(["--eval", command]);
        }
        assert_failure(&uscope(&arguments), expected);
    }
}

#[test]
fn disassembly_and_finish_follow_the_selected_frame() {
    // An outer frame's function is shown around its call, marking where
    // execution returns.
    let stdout = assert_success(uscope(&[
        "--core",
        "build/test-programs/frames-gcc-o0.core",
        "--batch",
        "--eval",
        "frame 3",
        "--eval",
        "disassemble",
    ]));
    let lines = stdout.lines().collect::<Vec<_>>();
    let function = line_index(&lines, 0, |line| {
        line == "function frames_keep in frames-gcc-o0:"
    });
    let marked = line_index(&lines, function, |line| line.starts_with("=> "));
    assert!(
        lines[marked - 1].contains("call") && lines[marked - 1].ends_with("<frames_relay>"),
        "{stdout}"
    );

    let stdout = assert_success(uscope(&[
        "build/test-programs/frames-gcc-o0",
        "--batch",
        "--eval",
        &format!(
            "break frames.c:{}",
            frames_line("frames_sink = leaf_local;")
        ),
        "--eval",
        "run",
        "--eval",
        "frame 5",
        "--eval",
        "finish",
        "--eval",
        "print depth",
        "--eval",
        "backtrace",
    ]));
    let lines = stdout.lines().collect::<Vec<_>>();
    let finished = line_index(&lines, 0, |line| line == "(int64_t) depth = 2");
    line_index(&lines, finished, |line| {
        line.starts_with("#0 ") && line.contains(" in frames_recurse at ")
    });
    // Depths 2 and 3 remain below main.
    line_index(&lines, finished, |line| {
        line.starts_with("#2 ") && line.contains(" in main at ")
    });
}

#[test]
fn the_innermost_frame_disassembles_where_no_single_inline_frame_is_active() {
    // A source breakpoint inside the inline body belongs to both the caller
    // and the inline instance, so no backtrace exists, but the program
    // counter does.
    let stdout = assert_success(uscope(&[
        "build/test-programs/variables-inline-gcc-o0",
        "--batch",
        "--eval",
        "break variables-inline.c:6",
        "--eval",
        "run",
        "--eval",
        "disassemble",
    ]));
    assert!(
        stdout.contains("function inline_caller in variables-inline-gcc-o0:")
            && stdout.lines().any(|line| line.starts_with("=> 0x")),
        "{stdout}"
    );
    assert_failure(
        &uscope(&[
            "build/test-programs/variables-inline-gcc-o0",
            "--batch",
            "--eval",
            "break variables-inline.c:6",
            "--eval",
            "run",
            "--eval",
            "up",
        ]),
        "the active inline frame is ambiguous",
    );
}

#[test]
fn backtraces_demangle_rust_symbols() {
    let stdout = assert_success(uscope(&[
        "--core",
        "build/test-programs/crash-rust-nodebug.core",
        "--batch",
        "--eval",
        "bt",
    ]));
    assert!(
        stdout
            .lines()
            .any(|line| line.contains(" in crash::crash_now+0x")
                && line.ends_with(" from crash-rust-nodebug")),
        "{stdout}"
    );
}

#[test]
fn where_reports_instructions_outside_every_module_by_address() {
    let stdout = assert_success(uscope(&[
        "--core",
        "build/test-programs/null-call.core",
        "--batch",
        "--eval",
        "where",
    ]));
    assert_eq!(
        stdout.trim_end(),
        "<unknown> at 0x0000000000000000 outside every loaded module"
    );
}

fn uscope_batch(arguments: &[&str], executable: &Path) -> std::process::Output {
    assert!(
        executable.exists(),
        "missing test fixture; run `just build-test-programs`"
    );
    Command::new(env!("CARGO_BIN_EXE_uscope"))
        .arg("--batch")
        .args(arguments)
        .arg(executable)
        .output()
        .expect("run uscope")
}

#[test]
fn watch_script_reports_values_lists_and_deletes_watchpoints() {
    let script = fixture("tests/fixtures/watch.uscope");
    let output = uscope_batch(
        &["-c", script.to_str().expect("UTF-8 path")],
        &fixture("build/test-programs/watch-gcc-o0"),
    );
    let stdout = assert_success(output);
    assert_no_sgr(&stdout);

    let set = stdout
        .lines()
        .find(|line| line.starts_with("watchpoint 1 set on watch_i32: 4 bytes at 0x"))
        .unwrap_or_else(|| panic!("missing set confirmation:\n{stdout}"));
    assert!(set.ends_with("using 1 hardware slot"), "{set}");
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("1  write  watch_i32  4 bytes at 0x")),
        "{stdout}"
    );
    for (old, new) in [(0, 1), (1, 2), (2, 42)] {
        assert!(
            stdout.contains(&format!("\n  old: {old}\n  new: {new}\n")),
            "missing {old} -> {new}:\n{stdout}"
        );
    }
    assert!(stdout.contains("\n  value: 2 (unchanged)\n"), "{stdout}");
    assert_eq!(
        stdout
            .matches("stopped by watchpoint 1 (write) on watch_i32 in thread ")
            .count(),
        4
    );
    assert!(stdout.contains("deleted watchpoint 1"));
    assert!(stdout.contains("no watchpoints"));
    assert!(
        stdout.contains("watch.c:"),
        "watch stops show source:\n{stdout}"
    );
    assert!(stdout.contains("inferior exited with status 0"), "{stdout}");
}

#[test]
fn access_and_location_watchpoints_render_their_kind_and_slots() {
    let output = uscope_batch(
        &[
            "-e",
            "break read_access",
            "-e",
            "run",
            "-e",
            "awatch watch_i32",
            "-e",
            "watch watch_packed.field",
            "-e",
            "info watchpoints",
            "-e",
            "continue",
            "-e",
            "unwatch all",
        ],
        &fixture("build/test-programs/watch-gcc-o0"),
    );
    let stdout = assert_success(output);
    assert!(stdout.contains("1  read/write  watch_i32"), "{stdout}");
    assert!(
        stdout.contains("watchpoint 2 set on watch_packed.field: 4 bytes at 0x")
            && stdout.contains("using 3 hardware slots"),
        "{stdout}"
    );
    assert!(
        stdout.contains("stopped by watchpoint 1 (read/write) on watch_i32"),
        "{stdout}"
    );
    assert!(stdout.contains("\n  value: 42 (unchanged)\n"), "{stdout}");
    assert!(stdout.contains("deleted 2 watchpoints"), "{stdout}");
}

#[test]
fn signed_watched_values_render_with_their_sign() {
    let output = uscope_batch(
        &[
            "-e",
            "break read_access",
            "-e",
            "run",
            "-e",
            "watch watch_sink",
            "-e",
            "continue",
            "-e",
            "continue",
        ],
        &fixture("build/test-programs/watch-gcc-o0"),
    );
    let stdout = assert_success(output);
    assert!(stdout.contains("\n  new: 42\n"), "{stdout}");
    assert!(stdout.contains("\n  old: 42\n  new: -42\n"), "{stdout}");
}

#[test]
fn watch_command_failures_explain_themselves() {
    let executable = fixture("build/test-programs/watch-gcc-o0");
    for (commands, expected) in [
        (
            &["watch watch_i32"][..],
            "the inferior has not been launched",
        ),
        (
            &["break scalar_stores", "run", "rwatch watch_i32"][..],
            "read watchpoints are unsupported by this target's debug hardware",
        ),
        (
            &["break scalar_stores", "run", "watch 0x1000:0"][..],
            "cannot watch 0 bytes at 0x1000: a watchpoint must cover at least one byte",
        ),
        (
            &["break scalar_stores", "run", "watch watch_oversized"][..],
            "only 4 remain",
        ),
        (
            &["break scalar_stores", "run", "watch watch_array[0..2]"][..],
            "cannot watch a range",
        ),
        (
            &["break scalar_stores", "run", "watch 0x1000:many"][..],
            "watch <value-path|0xaddress:byte-count>",
        ),
        (
            &["break scalar_stores", "run", "unwatch 9"][..],
            "watchpoint 9 was not found",
        ),
        (&["watch"][..], "watch <value-path|0xaddress:byte-count>"),
    ] {
        let arguments = commands
            .iter()
            .flat_map(|command| ["-e", command])
            .collect::<Vec<_>>();
        let output = uscope_batch(&arguments, &executable);
        assert!(!output.status.success(), "{commands:?} should fail");
        let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
        assert!(
            stderr.contains(expected),
            "{commands:?}: expected {expected:?} in:\n{stderr}"
        );
    }
}

/// Runs batch commands against a fixture and returns its output.
fn batch(fixture_name: &str, options: &[&str], commands: &[&str]) -> String {
    let mut arguments = vec!["--batch"];
    arguments.extend_from_slice(options);
    for command in commands {
        arguments.extend(["--eval", command]);
    }
    let executable = format!("build/test-programs/{fixture_name}");
    arguments.push(&executable);
    assert_success(uscope(&arguments))
}

#[test]
fn disassemble_renders_the_stopped_function_with_named_targets_and_source_lines() {
    let stdout = batch(
        "disassembly-gcc-o0",
        &[],
        &["break main", "run", "disassemble"],
    );
    let listing = stdout
        .split_once("function main in disassembly-gcc-o0:\n")
        .unwrap_or_else(|| panic!("no function header in:\n{stdout}"))
        .1;
    let lines = listing.lines().collect::<Vec<_>>();
    let stopped = lines
        .iter()
        .position(|line| line.starts_with("=> "))
        .unwrap_or_else(|| panic!("no stopped instruction in:\n{listing}"));
    // The stop follows the frame setup, under its source line.
    assert!(lines[stopped].contains(" <main+0x4>: "), "{listing}");
    assert!(
        lines[stopped].ends_with(" <disasm_marked_data>") && lines[stopped].contains("call 0x"),
        "{listing}"
    );
    assert!(
        lines[stopped - 1].ends_with("tests/fixtures/c/disassembly/main.c:17"),
        "{listing}"
    );
    for expected in [
        "call 0x",
        " <disasm_helper>",
        "# 0x",
        " <disasm_counter>",
        " <.plt+0x",
        "push rbp",
    ] {
        assert!(
            listing.contains(expected),
            "missing {expected:?} in:\n{listing}"
        );
    }
    assert_no_sgr(&stdout);
}

#[test]
fn disassemble_reports_data_inside_code_and_unproven_addresses() {
    let executable = fixture("build/test-programs/disassembly-clang-o2-nopie");
    let hidden = symbol_address(&executable, "disasm_hidden_data");
    let inside = format!("disassemble {:#x} 2", hidden + 4);
    let stdout = batch(
        "disassembly-clang-o2-nopie",
        &["--disassembly-syntax", "att"],
        &[
            "break main",
            "run",
            "disassemble disasm_marked_data",
            "disassemble disasm_hidden_data",
            &inside,
        ],
    );
    for expected in [
        format!(
            "this instruction overlaps {:#x}, where ",
            symbol_address(&executable, "disasm_marked_resume")
        ),
        "proves an instruction begins; decoding resumes there".to_owned(),
        format!(
            "this instruction extends past the end of the range at {:#x}",
            hidden + 10
        ),
        format!(
            "{:#x} lies inside the instruction at {:#x} when decoding from {hidden:#x}; it is probably not an instruction start",
            hidden + 4,
            hidden + 2
        ),
        "mov $1, %eax".to_owned(),
        "movabs".to_owned(),
    ] {
        assert!(
            stdout.contains(&expected),
            "missing {expected:?} in:\n{stdout}"
        );
    }
}

#[test]
fn disassemble_names_the_targets_indirect_branches_read_at_the_stop() {
    let executable = fixture("build/test-programs/disassembly-clang-o2-nopie");
    let address = |name| symbol_address(&executable, name);
    let data = fs::read(&executable).expect("read fixture");
    let object = object::File::parse(data.as_slice()).expect("parse fixture");
    let section = |name| {
        object
            .section_by_name(name)
            .unwrap_or_else(|| panic!("no section {name}"))
            .address()
    };
    let (plt, got_plt) = (section(".plt"), section(".got.plt"));
    let commands = [
        format!("break {:#x}", address("disasm_call_method")),
        format!("break {:#x}", address("disasm_call_register")),
        format!("break {:#x}", address("disasm_return")),
        "run\ndisassemble".to_owned(),
        format!("disassemble {:#x} 1", plt + 0x10),
        format!(
            "continue\ndisassemble {:#x} 1",
            address("disasm_call_register")
        ),
        format!("continue\ndisassemble {:#x} 1", address("disasm_return")),
        "disassemble disasm_indirect_forms".to_owned(),
    ];
    let commands = commands
        .iter()
        .flat_map(|command| command.lines())
        .collect::<Vec<_>>();
    let stdout = batch("disassembly-clang-o2-nopie", &[], &commands);
    let lines = stdout.lines().collect::<Vec<_>>();
    let find = |needle: &str, stopped: bool| {
        lines
            .iter()
            .find(|line| line.contains(needle) && line.starts_with("=> ") == stopped)
            .unwrap_or_else(|| panic!("no line with {needle:?} in:\n{stdout}"))
            .to_owned()
    };
    // At the stop, registers hold the target or address its slot.
    assert!(
        find("call qword ptr [rax+8]", true).ends_with(&format!(
            "  # {:#x} <disasm_methods+0x8> -> {:#x} <disasm_target_two>",
            address("disasm_methods") + 8,
            address("disasm_target_two")
        )),
        "{stdout}"
    );
    assert!(
        find("call rcx", true).ends_with(&format!(
            "call rcx  # -> {:#x} <disasm_target_four>",
            address("disasm_target_four")
        )),
        "{stdout}"
    );
    let ret = find("ret  # ", true);
    assert!(ret.contains(" <main+0x"), "{ret}");
    // Away from the stop, only slots the instruction addresses are read.
    assert!(find("call rcx", false).ends_with("call rcx"), "{stdout}");
    assert!(
        find(
            &format!("# {:#x} <disasm_slot>", address("disasm_slot")),
            false
        )
        .ends_with(&format!(
            "-> {:#x} <disasm_target_one>",
            address("disasm_target_one")
        )),
        "{stdout}"
    );
    let got = find(" <.got+0x", false);
    assert!(
        got.contains(" -> 0x7") && got.ends_with(" in libc.so.6>"),
        "{got}"
    );
    // Lazy binding has not yet filled the stub's slot, which is named once.
    let stub = find("jmp qword ptr [rip+", false);
    assert!(
        stub.ends_with(&format!(
            "jmp qword ptr [rip+{:#x}]  # {:#x} <.got.plt+0x18> -> {:#x} <.plt+0x16>",
            got_plt + 0x18 - (plt + 0x16),
            got_plt + 0x18,
            plt + 0x16
        )),
        "{stub}"
    );
    assert!(
        find("jmp qword ptr [0]", false).ends_with("  # 0x0 -> memory inaccessible at 0x0"),
        "{stdout}"
    );
    // Far transfers, interrupt returns, and branches whose operand size
    // differs between Intel and AMD processors say they are not computed.
    for form in ["jmp far fword ptr [rax]", "jmp rax", "iretq"] {
        assert!(
            find(form, false).ends_with(&format!(
                "{form}  # -> not computed for this form of branch"
            )),
            "{stdout}"
        );
    }
    assert_no_sgr(&stdout);
}

#[test]
fn disassemble_says_when_a_restarted_system_call_hides_a_stopped_target() {
    let mut target = Command::new(fixture("build/test-programs/attach-restart"))
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn attach target");
    let mut ready = [0_u8; 6];
    let ready = target
        .stdout
        .as_mut()
        .expect("target stdout")
        .read_exact(&mut ready)
        .map(|()| ready);
    // Attaching must interrupt pause(2).
    let syscall = format!("/proc/{}/syscall", target.id());
    let deadline = Instant::now() + Duration::from_secs(5);
    while fs::read_to_string(&syscall).is_ok_and(|text| !text.starts_with("34 "))
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(1));
    }
    let output = Command::new(env!("CARGO_BIN_EXE_uscope"))
        .args([
            "--batch",
            "--eval",
            "disassemble",
            "--attach",
            &target.id().to_string(),
        ])
        .output()
        .expect("attach uscope by PID");
    target.kill().expect("kill attach target");
    target.wait().expect("reap attach target");
    assert_eq!(ready.expect("wait for target readiness"), *b"READY\n");
    let stdout = assert_success(output);
    let stopped = stdout
        .lines()
        .find(|line| line.starts_with("=> "))
        .unwrap_or_else(|| panic!("no stopped instruction in:\n{stdout}"));
    assert!(
        stopped.ends_with(
            "jmp rax  # -> unknown: a restarted system call replaces a register it uses"
        ),
        "{stdout}"
    );
}

#[test]
fn disassemble_shows_a_stop_outside_every_function_and_rejects_bad_requests() {
    // A call through a null pointer stops where no module or function is.
    let stdout = batch("null-call", &[], &["run", "disassemble"]);
    assert!(
        stdout.contains("memory inaccessible at 0x0"),
        "missing unreadable stop in:\n{stdout}"
    );

    for (command, expected) in [
        (
            "disassemble 0x8",
            "no function or code symbol contains 0x8; give an instruction count to disassemble from it",
        ),
        (
            "disassemble main 0",
            "instruction count must be between 1 and 4096",
        ),
        (
            "disassemble main 4097",
            "instruction count must be between 1 and 4096",
        ),
        (
            "disassemble missing_function",
            "no symbol named 'missing_function'",
        ),
    ] {
        let output = uscope(&[
            "--batch",
            "--eval",
            "break main",
            "--eval",
            "run",
            "--eval",
            command,
            "build/test-programs/disassembly-gcc-o0",
        ]);
        assert_failure(&output, expected);
    }
}

#[test]
fn disassemble_reads_code_from_core_dumps() {
    let stdout = assert_success(uscope(&[
        "--batch",
        "--core",
        "build/test-programs/crash-gcc-o2-nopie-segv.core",
        "--eval",
        "disassemble",
        "--eval",
        "disassemble main",
    ]));
    assert!(
        stdout.contains("function crash_segv in crash-gcc-o2-nopie:"),
        "{stdout}"
    );
    assert!(
        stdout.lines().any(|line| line.starts_with("=> 0x")),
        "{stdout}"
    );
    // The optimized main has a cold range of its own.
    assert!(
        stdout.contains("range 1 of 2: 0x4010a0..0x4010a5"),
        "{stdout}"
    );
    assert!(stdout.contains(" <main.cold>: "), "{stdout}");
}

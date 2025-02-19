const std = @import("std");
const builtin = @import("builtin");
const Build = std.Build;

const Flags = struct {
    /// Whether or not to enable the Spall profiler
    spall: bool = false,

    /// Whether or not to enable the Tracy profiler
    tracy: bool = false,

    /// Whether or not to enable TSan for race detection,
    /// lock inversion reports, thread leaks, etc.
    race: bool = false,

    /// Indicates that the binary being build will ship to customers
    /// and should be hardened appropriately
    release: bool = false,

    /// Whether or not to build the binary with LLVM enabled. We need
    /// LLVM for TSan.
    llvm: bool = false,

    /// Whether or not the built test binary is intended to be run under valgrind.
    /// This changes the timings of certain conditions for our old, slow CI runners
    /// that are running valgrind with full leak checking.
    valgrind: bool = false,
};

pub fn build(b: *Build) !void {
    const target = b.standardTargetOptions(.{});
    const optimize = b.standardOptimizeOption(.{});

    //
    // Define command line flags
    //

    const opts = b.addOptions();
    var flags = Flags{};

    const ci = b.option(bool, "ci", "True if running tests in CI (default: false)") orelse false;
    opts.addOption(bool, "ci", ci);

    const test_filter: ?[]const u8 = b.option([]const u8, "filter", "Filter running tests down to just the given name (default: null)") orelse null;

    const imgui_demo = b.option(bool, "imgui-demo", "Show the IMGUI Demo, available in debug builds only (default: false)") orelse false;
    opts.addOption(bool, "imgui_demo", imgui_demo);

    flags.release = b.option(bool, "release", "Hardens the binary for production release to users (default: false)") orelse false;
    opts.addOption(bool, "release", flags.release);

    const reset_gui = b.option(bool, "reset-gui", "Delete the IMGUI config file on startup (default: false)") orelse false;
    opts.addOption(bool, "reset_gui", reset_gui);

    flags.spall = b.option(bool, "spall", "Enable spall (default: false)") orelse false;
    opts.addOption(bool, "spall_enabled", flags.spall);

    flags.tracy = b.option(bool, "tracy", "Enable tracy (default: false)") orelse false;
    opts.addOption(bool, "tracy_enabled", flags.tracy);

    flags.race = b.option(bool, "race", "Enable TSan (default: false)") orelse false;
    opts.addOption(bool, "race", flags.race);

    // @NOTE (jrc): swap to the self-hosted backend when it's more reliable
    const llvm_default = true or flags.release or flags.race or flags.spall or flags.tracy or optimize != .Debug;
    const llvm_help = try std.fmt.allocPrint(b.allocator, "Enable LLVM (default: {any})", .{llvm_default});
    flags.llvm = b.option(bool, "llvm", llvm_help) orelse llvm_default;
    opts.addOption(bool, "llvm", flags.llvm);

    flags.valgrind = b.option(bool, "valgrind", "Indicates the binary is intended to be run under valgrind (default: false)") orelse false;
    opts.addOption(bool, "valgrind", flags.valgrind);

    //
    // Define all the possible executables and objects we'd ever want to build
    //

    // build and run the application
    const run_exe = b.addExecutable(.{
        .name = "uscope",
        .root_source_file = b.path("src/main.zig"),
        .target = target,
        .optimize = optimize,
    });

    // build and run the tests
    const test_exe = b.addTest(.{
        .name = "uscope-tests",
        .root_source_file = b.path("src/main.zig"),
        .target = target,
        .optimize = optimize,
        .filters = if (test_filter) |f| &.{f} else &.{},
    });

    //
    // Define the build steps and dependency graph using the
    // set of exe's and obj's above
    //

    defineStep(b, stepDef{
        .name = "run",
        .description = "Build and run uscope",
        .exes = &.{run_exe},
        .opts = opts,
        .flags = flags,
        .target = target,
        .optimize = optimize,
    });

    defineStep(b, stepDef{
        .name = "test",
        .description = "Build and run tests",
        .exes = &.{test_exe},
        .opts = opts,
        .flags = flags,
        .no_caching = true,
        .target = target,
        .optimize = optimize,
    });
}

const stepDef = struct {
    name: []const u8,
    description: []const u8,
    exes: []const *Build.Step.Compile,
    depend_on: ?[]const *Build.Step = null,
    opts: *Build.Step.Options,
    flags: Flags,
    no_caching: bool = false,
    target: Build.ResolvedTarget,
    optimize: std.builtin.OptimizeMode,
};

fn defineStep(b: *Build, def: stepDef) void {
    for (def.exes) |exe| {
        const is_debug = exe.root_module.optimize == .Debug;

        b.installArtifact(exe);
        exe.linkLibC();
        exe.root_module.addOptions("build_options", def.opts);

        exe.use_llvm = def.flags.llvm;
        exe.use_lld = def.flags.llvm;

        exe.root_module.omit_frame_pointer = false;
        exe.root_module.stack_check = !is_debug;
        exe.root_module.stack_protector = !is_debug;
        exe.root_module.sanitize_c = is_debug;
        exe.root_module.sanitize_thread = def.flags.race;
        exe.root_module.strip = def.flags.release;

        const step = b.step(def.name, def.description);
        if (def.depend_on != null) {
            for (def.depend_on.?) |dep| step.dependOn(dep);
        }

        const cmd = b.addRunArtifact(exe);
        if (def.no_caching) cmd.has_side_effects = true;
        if (b.args) |args| cmd.addArgs(args);
        step.dependOn(&cmd.step);

        //
        // Set up module dependencies
        //

        exe.root_module.addIncludePath(b.path("libs/stb_image"));
        exe.addCSourceFile(.{ .file = b.path("libs/stb_image/stb_image.c") });

        exe.root_module.addIncludePath(b.path("libs/spall"));

        const time_dep = b.dependency("time", .{});
        const time_mod = time_dep.module("time");
        exe.root_module.addImport("time", time_mod);

        const cimgui_dep = b.dependency("cimgui", .{
            .target = def.target,
            .optimize = def.optimize,
        });
        exe.root_module.addImport("cimgui", cimgui_dep.module("cimgui"));
        exe.linkLibrary(cimgui_dep.artifact("cimgui"));

        if (b.lazyDependency("system_sdk", .{})) |system_sdk| {
            exe.addLibraryPath(system_sdk.path("linux/lib/x86_64-linux-gnu"));
        }

        const zglfw = b.dependency("zglfw", .{
            .target = def.target,
            .optimize = def.optimize,
        });
        exe.root_module.addImport("zglfw", zglfw.module("root"));
        exe.linkLibrary(zglfw.artifact("glfw"));

        const zopengl = b.dependency("zopengl", .{});
        exe.root_module.addImport("zopengl", zopengl.module("root"));

        const ztracy = b.dependency("ztracy", .{
            .enable_ztracy = def.flags.tracy,
            .target = def.target,
            .optimize = def.optimize,
        });
        exe.root_module.addImport("ztracy", ztracy.module("root"));
        exe.linkLibrary(ztracy.artifact("tracy"));
    }
}

comptime {
    ensureCorrectZigVersion();
}

fn ensureCorrectZigVersion() void {
    const version_str = std.mem.trimRight(u8, @embedFile("zig_version.txt"), "\n");
    const expected = std.SemanticVersion.parse(version_str) catch unreachable;
    const actual = builtin.zig_version;

    if (actual.major != expected.major or actual.minor != expected.minor) {
        @compileError(std.fmt.comptimePrint(
            "Your Zig version v{} does not meet the required build version of v{}. \n" ++
                "An example of how to install the exactly correct version can be " ++
                "found in the Dockerfile in this repo.",
            .{
                actual,
                expected,
            },
        ));
    }
}

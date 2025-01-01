const std = @import("std");

pub fn build(_: *std.Build) void {}

fn absPath(b: *std.Build, pathname: []const u8) []const u8 {
    const cwd = std.fs.cwd();
    const full_path = std.fmt.allocPrint(b.allocator, "libs/system-sdk/{s}", .{pathname}) catch unreachable;
    return cwd.realpathAlloc(b.allocator, full_path) catch unreachable;
}

pub fn addLibraryPathsTo(compile_step: *std.Build.Step.Compile) void {
    const b = compile_step.step.owner;
    const target = compile_step.rootModuleTarget();

    const system_sdk = b.dependency("system_sdk", .{});

    switch (target.os.tag) {
        .windows => {
            if (target.cpu.arch.isX86()) {
                compile_step.addLibraryPath(
                    system_sdk.path(absPath(b, "windows/lib/x86_64-windows-gnu")),
                );
            }
        },
        .macos => {
            compile_step.addLibraryPath(system_sdk.path(absPath(b, "macos12/usr/lib")));
            compile_step.addFrameworkPath(system_sdk.path(absPath(b, "macos12/System/Library/Frameworks")));
        },
        .linux => {
            if (target.cpu.arch.isX86()) {
                compile_step.addLibraryPath(system_sdk.path(absPath(b, "linux/lib/x86_64-linux-gnu")));
            } else if (target.cpu.arch == .aarch64) {
                compile_step.addLibraryPath(system_sdk.path(absPath(b, "linux/lib/aarch64-linux-gnu")));
            }
        },
        else => {},
    }
}

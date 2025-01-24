const std = @import("std");
const NativeTargetInfo = std.zig.system.NativeTargetInfo;

pub fn build(b: *std.Build) !void {
    const target = b.standardTargetOptions(.{});
    const optimize = b.standardOptimizeOption(.{});

    const opts = b.addOptions();
    const webgpu = b.option(bool, "webgpu", "Enable WebGPU rather than OpenGL") orelse false;
    opts.addOption(bool, "webgpu", webgpu);

    const cimgui_mod = b.addModule("cimgui", .{
        .root_source_file = b.path("main.zig"),
        .target = target,
        .optimize = optimize,
    });

    cimgui_mod.addOptions("cimgui_options", opts);

    const imgui = b.dependency("imgui", .{});
    const freetype = b.dependency("freetype", .{
        .target = target,
        .optimize = optimize,
        .@"enable-libpng" = true,
    });

    const lib = b.addStaticLibrary(.{
        .name = "cimgui",
        .target = target,
        .optimize = optimize,
    });
    lib.linkLibC();
    lib.linkLibCpp();
    lib.linkLibrary(freetype.artifact("freetype"));
    if (target.result.os.tag == .windows) {
        lib.linkSystemLibrary("imm32");
    }

    cimgui_mod.addIncludePath(b.path("vendor"));
    lib.addIncludePath(b.path("vendor"));
    lib.addIncludePath(imgui.path(""));
    if (webgpu) {
        lib.addIncludePath(b.path("../zgpu/libs/dawn/include"));
    } else {
        lib.addIncludePath(b.path("../zglfw/libs/glfw/include"));
        lib.addIncludePath(b.path("../system-sdk/linux/include"));
    }

    var flags = std.ArrayList([]const u8).init(b.allocator);
    defer flags.deinit();
    try flags.appendSlice(&.{
        "-DCIMGUI_FREETYPE=1",
        "-DIMGUI_USE_WCHAR32=1",
        "-DIMGUI_DISABLE_OBSOLETE_FUNCTIONS=1",
    });
    if (target.result.os.tag == .windows) {
        try flags.appendSlice(&.{
            "-DIMGUI_IMPL_API=extern\t\"C\"\t__declspec(dllexport)",
        });
    } else {
        try flags.appendSlice(&.{
            "-DIMGUI_IMPL_API=extern\t\"C\"",
        });
    }

    lib.addCSourceFile(.{ .file = b.path("vendor/cimgui.cpp"), .flags = flags.items });
    lib.addCSourceFile(.{ .file = imgui.path("imgui.cpp"), .flags = flags.items });
    lib.addCSourceFile(.{ .file = imgui.path("imgui_draw.cpp"), .flags = flags.items });
    lib.addCSourceFile(.{ .file = imgui.path("imgui_demo.cpp"), .flags = flags.items });
    lib.addCSourceFile(.{ .file = imgui.path("imgui_widgets.cpp"), .flags = flags.items });
    lib.addCSourceFile(.{ .file = imgui.path("imgui_tables.cpp"), .flags = flags.items });
    lib.addCSourceFile(.{ .file = imgui.path("misc/freetype/imgui_freetype.cpp"), .flags = flags.items });

    lib.addCSourceFile(.{ .file = imgui.path("backends/imgui_impl_glfw.cpp"), .flags = flags.items });
    if (webgpu) {
        lib.addCSourceFile(.{ .file = imgui.path("backends/imgui_impl_wgpu.cpp"), .flags = flags.items });
    } else {
        lib.addCSourceFile(.{ .file = imgui.path("backends/imgui_impl_opengl3.cpp"), .flags = flags.items });
    }

    lib.installHeadersDirectory(b.path("vendor"), "", .{
        .include_extensions = &.{".h"},
    });

    b.installArtifact(lib);
}

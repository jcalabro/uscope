const std = @import("std");
const builtin = @import("builtin");
const options = @import("build_options");
const eql = mem.eql;
const mem = std.mem;
const time = std.time;

const logging = @import("logging.zig");

const log = logging.Logger.init(logging.Region.Main);

pub const CI = flag(bool, "ci");
pub const ImGuiDemo = flag(bool, "imgui_demo");
pub const LLVM = flag(bool, "llvm");
pub const RaceEnabled = flag(bool, "race");
pub const Release = flag(bool, "release");
pub const ResetGUI = flag(bool, "reset_gui");
pub const SpallEnabled = flag(bool, "spall_enabled");
pub const TracyEnabled = flag(bool, "tracy_enabled");
pub const Valgrind = flag(bool, "valgrind");

fn flag(comptime T: type, comptime name: []const u8) T {
    if (@hasDecl(options, name)) {
        if (eql(u8, name, "ci")) {
            return options.ci;
        } else if (eql(u8, name, "imgui_demo")) {
            return options.imgui_demo;
        } else if (eql(u8, name, "llvm")) {
            return options.llvm;
        } else if (eql(u8, name, "race")) {
            return options.race;
        } else if (eql(u8, name, "release")) {
            return options.release;
        } else if (eql(u8, name, "reset_gui")) {
            return options.reset_gui;
        } else if (eql(u8, name, "spall_enabled")) {
            return options.spall_enabled;
        } else if (eql(u8, name, "tracy_enabled")) {
            return options.tracy_enabled;
        } else if (eql(u8, name, "valgrind")) {
            return options.valgrind;
        } else {
            @compileError("unknown compile-time flag: " ++ name);
        }
    }

    return false;
}

pub fn logAll() void {
    log.info("flags:");
    if (CI) log.infof("ci: {any}", .{CI});
    if (ImGuiDemo) log.infof("imgui_demo: {any}", .{ImGuiDemo});
    if (RaceEnabled) log.infof("race: {any}", .{RaceEnabled});
    if (Release) log.infof("release: {any}", .{Release});
    if (ResetGUI) log.infof("reset_gui: {any}", .{ResetGUI});
    if (SpallEnabled) log.infof("spall: {any}", .{SpallEnabled});
    if (TracyEnabled) log.infof("tracy: {any}", .{TracyEnabled});
    if (Valgrind) log.infof("valgrind: {any}", .{Valgrind});
}

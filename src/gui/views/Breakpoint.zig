const std = @import("std");
const fmt = std.fmt;
const mem = std.mem;

const debugger = @import("../../debugger.zig");
const file = @import("../../file.zig");
const GUI = @import("../Gui.zig");
const Input = @import("../Input.zig");
const logging = @import("../../logging.zig");
const proto = debugger.proto;
const State = @import("../State.zig");
const trace = @import("../../trace.zig");
const windows = @import("../windows.zig");
const zui = @import("../zui.zig");

const log = logging.Logger.init(logging.Region.GUI);

const Breakpoint = @This();

gui: *State.GuiType,
state: *State,

pub fn init(state: *State, gui: *State.GuiType) !*Breakpoint {
    const self = try state.perm_alloc.create(Breakpoint);
    errdefer state.perm_alloc.free(self);

    self.* = .{ .state = state, .gui = gui };
    return self;
}

pub fn deinit(self: *Breakpoint) void {
    self.alloc.destroy(self);
}

pub fn view(self: *Breakpoint) State.View {
    const z = trace.zone(@src());
    defer z.end();

    return State.View{ .breakpoint = self };
}

fn handleInput(self: *Breakpoint) ?State.View {
    const z = trace.zone(@src());
    defer z.end();

    if (Input.cancelPressed()) {
        return self.state.primary.view();
    }

    return null;
}

pub fn update(self: *Breakpoint) State.View {
    const z = trace.zoneN(@src(), "Breakpoint.update");
    defer z.end();

    if (self.handleInput()) |next| return next;

    const window_size = self.gui.getSingleFocusWindowSizeWithScale(0.75);
    zui.setNextWindowPos(window_size.x, window_size.y, 0, 0);
    zui.setNextWindowSize(window_size.w, window_size.h);
    zui.setNextWindowFocus();

    if (zui.begin(windows.Breakpoint_List, .{ .flags = .{
        .no_move = true,
        .no_resize = true,
        .no_title_bar = true,
        .no_scrollbar = true,
        .no_scroll_with_mouse = true,
    } })) {
        defer zui.end();

        if (zui.beginChild("Breakpoint List Child", .{
            .w = window_size.w,
            .h = window_size.h,
            .flags = .{
                .no_title_bar = true,
            },
        })) {
            defer zui.endChild();

            if (self.state.dbg_state.breakpoints.len == 0) {
                const txt = "No breakpoints have been set";
                zui.centerText(txt);
                zui.textUnformatted(txt);
                return self.view();
            }

            const num_cols = 6;
            if (zui.beginTable("Breakpoints List", .{
                .flags = .{
                    .resizable = false,
                    .reorderable = false,
                    .scroll_y = true,
                    .row_bg = true,
                },
                .column = num_cols,
            })) {
                defer zui.endTable();

                zui.tableSetupScrollFreeze(num_cols, 1); // sticky header row
                zui.tableSetupColumn("Delete", .{
                    .flags = .{ .width_fixed = true },
                    .init_width_or_height = 100,
                });
                zui.tableSetupColumn("Enabled", .{
                    .flags = .{ .width_fixed = true },
                    .init_width_or_height = 100,
                });
                zui.tableSetupColumn("ID", .{
                    .flags = .{ .width_fixed = true },
                    .init_width_or_height = 100,
                });
                zui.tableSetupColumn("Hit Count", .{
                    .flags = .{ .width_fixed = true },
                    .init_width_or_height = 100,
                });
                zui.tableSetupColumn("Location", .{
                    .flags = .{ .width_stretch = true },
                });
                zui.tableSetupColumn("Condition", .{
                    .flags = .{ .width_stretch = true },
                });
                zui.tableHeadersRow();

                for (self.state.dbg_state.breakpoints, 0..) |bp, ndx| {
                    var local_bp = bp;
                    zui.tableNextRow(.{});

                    if (zui.tableNextColumn()) {
                        var label = [_]u8{0} ** 16;
                        _ = fmt.bufPrint(&label, "X##Enabled{d}\x00", .{ndx}) catch |err| {
                            log.errf("unable to format enabled column header: {!}", .{err});
                            continue;
                        };
                        if (zui.button(@ptrCast(&label), .{ .w = 50 })) {
                            self.state.dbg.enqueue(proto.UpdateBreakpointRequest{ .loc = .{
                                .bid = bp.bid,
                            } });
                        }
                    }

                    if (zui.tableNextColumn()) {
                        var label = [_]u8{0} ** 16;
                        _ = fmt.bufPrint(&label, "##Enabled{d}\x00", .{ndx}) catch |err| {
                            log.errf("unable to format enabled column header: {!}", .{err});
                            continue;
                        };

                        var active = local_bp.flags.active;
                        if (zui.checkbox(@ptrCast(&label), .{ .v = &active })) {
                            local_bp.flags.active = active;
                            self.state.dbg.enqueue(proto.ToggleBreakpointRequest{
                                .id = local_bp.bid,
                            });
                        }
                    }

                    if (zui.tableNextColumn()) {
                        zui.textWrapped("{d}", .{bp.bid.int()});
                    }

                    if (zui.tableNextColumn()) {
                        zui.textWrapped("{d}", .{bp.hit_count});
                    }

                    if (zui.tableNextColumn()) {
                        var rendered = false;
                        if (bp.source_location) |loc| {
                            if (file.getCachedFile(loc.file_hash)) |src| {
                                zui.textWrapped("{s}:{d}", .{ src.name, loc.line });
                                rendered = true;
                            }
                        }

                        if (!rendered) {
                            zui.textWrapped("0x{x}", .{bp.addr});
                        }
                    }

                    // @TODO (jrc): display a preview of the line of source code

                    if (zui.tableNextColumn()) {
                        // not yet implemented
                        zui.textUnformatted("");
                    }
                }
            }
        }
    }

    return self.view();
}

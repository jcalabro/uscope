const std = @import("std");
const builtin = @import("builtin");
const Allocator = mem.Allocator;
const ArrayList = std.ArrayList;
const assert = std.debug.assert;
const fmt = std.fmt;
const mem = std.mem;
const path = std.fs.path;
const t = std.testing;

const colors = @import("../colors.zig");
const debugger = @import("../../debugger.zig");
const GUI = @import("../GUI.zig");
const Input = @import("../Input.zig");
const file_util = @import("../../file.zig");
const logging = @import("../../logging.zig");
const proto = debugger.proto;
const Reader = @import("../../Reader.zig");
const safe = @import("../../safe.zig");
const settings = @import("../../settings.zig");
const State = @import("../State.zig");
const strings = @import("../../strings.zig");
const String = strings.String;
const trace = @import("../../trace.zig");
const types = @import("../../types.zig");
const windows = @import("../windows.zig");
const zui = @import("../zui.zig");

const time = @import("time");

const log = logging.Logger.init(logging.Region.GUI);

const Self = @This();

gui: *State.GUIType,
state: *State,

typing_in_textbox: bool = false,

show_diagnostics: bool = false,

open_space_menu: bool = false,
show_space_menu: bool = false,

watch_vars: ArrayList([]const u8),
focus_watch_var_input_on_next_frame: bool = false,

open_windows: OpenWindows = .{},

pub fn init(state: *State, gui: *State.GUIType) !*Self {
    const z = trace.zone(@src());
    defer z.end();

    const self = try state.perm_alloc.create(Self);
    errdefer state.perm_alloc.destroy(self);

    self.* = .{
        .gui = gui,
        .state = state,
        .watch_vars = ArrayList([]const u8).init(state.perm_alloc),
    };

    for (settings.settings.project.sources.open_files) |file_path| {
        self.state.openSourceFile(file_path) catch |err| {
            log.errf("unable to open source file from settings: {!}", .{err});
            continue;
        };
    }

    for (settings.settings.project.target.watch_expressions) |e| {
        self.addWatchValue(e) catch |err| {
            log.errf("unable to add watch expression {s}: {!}", .{ e, err });
            continue;
        };
    }

    return self;
}

pub fn toggleDiagnosticsView(self: *Self) void {
    self.show_diagnostics = !self.show_diagnostics;
}

pub fn view(self: *Self) State.View {
    const z = trace.zone(@src());
    defer z.end();

    return State.View{ .primary = self };
}

pub const OpenWindows = packed struct {
    sources: bool = true,
    output: bool = true,
    registers: bool = true,
    call_stack: bool = true,
    hex: bool = true,
    watch: bool = true,
    locals: bool = true,
    diagnostics: bool = true,
};

fn handleInput(self: *Self) ?State.View {
    const z = trace.zone(@src());
    defer z.end();

    if (self.show_space_menu) {
        if (Input.keyPressed(.f)) {
            self.show_space_menu = false;
            return self.state.file_picker.view();
        }

        if (Input.keyPressed(.b)) {
            self.show_space_menu = false;
            return self.state.breakpoint.view();
        }

        if (Input.cancelPressed() or Input.keyPressed(.q)) {
            self.show_space_menu = false;
        }

        return self.view();
    }

    // @NOTE (jrc): all these key commands are temporary placeholders. We'll
    // be doing a full redesign to use modal debugging semantics

    if (Input.keyPressedWithCtrl(.q)) {
        self.state.quit();
    }

    if (self.typing_in_textbox) {
        // don't allow keyboard shortcuts while the user it typing
        return null;
    }

    if (Input.keyPressed(.F1)) {
        self.toggleDiagnosticsView();
    }

    if (Input.keyPressed(.r)) {
        self.state.launchSubordinate();
    }

    if (Input.keyPressed(.k)) {
        self.state.killSubordinate();
    }

    if (Input.keyPressed(.c)) {
        self.state.continueExecution();
    }

    if (Input.keyPressed(.w)) {
        self.state.sendStepRequest(.out_of);
    }
    if (Input.keyPressed(.a)) {
        self.state.sendStepRequest(.single);
    }
    if (Input.keyPressed(.s)) {
        self.state.sendStepRequest(.into);
    }
    if (Input.keyPressed(.d)) {
        self.state.sendStepRequest(.over);
    }

    if (Input.keyPressed(.space)) {
        self.open_space_menu = true;
        self.show_space_menu = true;
    }

    if (Input.keyPressedWithCtrl(.d)) {
        self.state.closeSourceFile(null);
    }
    if (Input.keyPressedWithCtrl(.j)) {
        if (self.state.open_source_file_ndx == 0) {
            self.state.open_source_file_ndx = self.state.open_files.items.len - 1;
        } else {
            self.state.open_source_file_ndx -= 1;
        }
        self.state.newly_opened_file = self.state.open_source_file_ndx;
    }
    if (Input.keyPressedWithCtrl(.semicolon)) {
        if (self.state.open_source_file_ndx == self.state.open_files.items.len - 1) {
            self.state.open_source_file_ndx = 0;
        } else {
            self.state.open_source_file_ndx += 1;
        }
        self.state.newly_opened_file = self.state.open_source_file_ndx;
    }

    return null;
}

pub fn update(self: *Self) State.View {
    const z = trace.zone(@src());
    defer z.end();

    if (self.show_space_menu) self.drawSpaceMenuHelp();

    if (self.handleInput()) |next| return next;
    self.typing_in_textbox = false;

    if (self.show_diagnostics) {
        defer zui.end();

        if (zui.begin(windows.Diagnostics, .{})) {
            // zui.bullet();
            // zui.textUnformattedColored(.{ 0, 0.8, 0, 1 }, "Average :");
            // zui.sameLine(.{});
            // zui.text(
            //     "{d:.3} ms/frame ({d:.1} fps)",
            //     .{ self.state.gctx.stats.average_cpu_time, self.state.gctx.stats.fps },
            // );
        }
    }

    if (self.open_windows.output) blk: {
        defer zui.end();
        if (zui.begin(windows.Primary_Output, .{ .flags = .{ .no_title_bar = false } })) {
            self.state.subordinate_output_mu.lock();
            defer self.state.subordinate_output_mu.unlock();

            // @PERFORMANCE (jrc): can we just take a view of the existing slice?
            const text = self.state.scratch_alloc.alloc(u8, self.state.subordinate_output.len) catch |err| {
                log.errf("unable to allocate subordinate output buffer: {!}", .{err});
                break :blk;
            };
            for (0..self.state.subordinate_output.len) |ndx| {
                text[ndx] = self.state.subordinate_output.get(ndx);
            }

            // @TODO (jrc): scroll to follow the latest text (can use zui.setScrollYFloat)
            zui.textWrapped("{s}", .{text});
        }
    }

    if (self.open_windows.watch) blk: {
        defer zui.end();
        if (zui.begin(windows.Primary_Watch, .{ .flags = .{ .no_title_bar = true } })) {
            {
                //
                // Input box
                // @TODO (jrc): make this a space item, or just w to start watching
                //

                if (self.focus_watch_var_input_on_next_frame) {
                    self.focus_watch_var_input_on_next_frame = false;
                    zui.setKeyboardFocusHere(0);
                }

                var buf = [_]u8{0} ** 256;
                if (zui.inputTextWithHint("Watch", .{
                    .flags = .{
                        .enter_returns_true = true,
                        .chars_no_blank = true,
                        .no_horizontal_scroll = true,
                        .escape_clears_all = true,
                    },
                    .hint = "enter a value to watch",
                    .buf = &buf,
                })) {
                    self.focus_watch_var_input_on_next_frame = true;

                    const len = mem.indexOfSentinel(u8, 0, @ptrCast(&buf));
                    const watch = self.state.scratch_alloc.alloc(u8, len) catch |err| {
                        log.errf("unable to allocate scratch buffer for watch expression: {!}", .{err});
                        break :blk;
                    };
                    @memcpy(watch, buf[0..len]);

                    self.addWatchValue(watch) catch |err| {
                        log.errf("unable to add watch value: {!}", .{err});
                    };
                }

                if (zui.isItemActive()) self.typing_in_textbox = true;
            }
        }

        {
            //
            // Watch window
            //

            var to_delete = ArrayList(usize).init(self.state.scratch_alloc);
            defer to_delete.deinit();

            if (zui.beginTable("Watch Values", .{
                .flags = .{ .scroll_y = true, .row_bg = true },
                .column = 4,
            })) {
                defer zui.endTable();

                for (self.watch_vars.items, 0..) |watch, ndx| {
                    zui.tableNextRow(.{});

                    if (zui.tableNextColumn()) {
                        const label = fmt.allocPrint(self.state.scratch_alloc, "X###{d}\x00", .{ndx}) catch |err| e: {
                            log.errf("unable to create delete watch item label: {!}", .{err});
                            break :e "";
                        };
                        if (zui.button(@ptrCast(label), .{})) {
                            to_delete.append(ndx) catch |err| {
                                log.errf("unable to mark watch value for deletion: {!}", .{err});
                            };
                        }
                    }
                    if (zui.tableNextColumn()) {
                        zui.textWrapped("{s}", .{watch});
                    }
                    if (self.state.dbg_state.paused) |paused| {
                        if (ndx >= paused.watches.len) {
                            // it might be the case that the debugger thread hasn't finished calculating watch values yet,
                            // so we skip here and attempt to display again next frame
                            continue;
                        }

                        self.renderExpressionResult(
                            self.state.scratch_alloc,
                            &paused,
                            paused.watches[ndx],
                        );
                    }
                }

                var ndx: i64 = @as(i64, @intCast(to_delete.items.len)) - 1;
                while (ndx >= 0) : (ndx -= 1) {
                    const str = self.watch_vars.orderedRemove(to_delete.items[@intCast(ndx)]);
                    self.state.perm_alloc.free(str);
                }
            }
        }
    }

    if (self.open_windows.locals) {
        //
        // Locals window
        //

        defer zui.end();
        if (zui.begin(windows.Primary_Locals, .{ .flags = .{ .no_title_bar = true } })) {
            if (zui.beginTable("Locals", .{
                .flags = .{ .scroll_y = true, .row_bg = true },
                .column = 3,
            })) {
                defer zui.endTable();

                if (self.state.dbg_state.paused) |paused| {
                    for (paused.locals) |local| {
                        if (zui.tableNextColumn()) {
                            const expr = paused.getString(local.expression);
                            zui.textWrapped("{s}", .{expr});
                        }

                        self.renderExpressionResult(self.state.scratch_alloc, &paused, local);
                    }
                }
            }
        }
    }

    if (self.open_windows.call_stack) {
        defer zui.end();
        if (zui.begin(windows.Primary_CallStack, .{})) {
            if (self.state.dbg_state.paused) |paused| {
                for (paused.stack_frames) |frame| {
                    const name: String = n: {
                        if (frame.name) |hash| {
                            break :n paused.getString(hash);
                        }
                        break :n types.Unknown;
                    };

                    zui.textWrapped("0x{x}: {s}", .{
                        frame.address.int(),
                        name,
                    });
                }
            }
        }
    }

    if (self.open_windows.registers) {
        defer zui.end();
        if (zui.begin(windows.Primary_Registers, .{})) {
            if (self.state.dbg_state.paused) |paused| {
                inline for (@typeInfo(@TypeOf(paused.registers)).Struct.fields) |field| {
                    const addr = @field(paused.registers, field.name);
                    const line = fmt.allocPrint(self.state.scratch_alloc, "{s}: 0x{x}\x00", .{
                        field.name,
                        addr,
                    }) catch |err| blk: {
                        log.errf("unable to render register value: {!}", .{err});
                        break :blk "";
                    };

                    // send to the memory hex window
                    if (zui.selectable(@ptrCast(line), .{})) {
                        self.state.dbg.enqueue(proto.SetHexWindowAddressRequest{
                            .address = types.Address.from(addr),
                        });
                    }
                }
            }
        }
    }

    if (self.open_windows.hex) {
        defer zui.end();
        if (zui.begin(windows.Primary_Hex, .{})) {
            {
                //
                // Input box
                //

                var buf = [_]u8{0} ** 17; // +1 for null-terminator
                if (zui.inputTextWithHint("Address", .{
                    .flags = .{
                        .enter_returns_true = true,
                        .chars_no_blank = true,
                        .no_horizontal_scroll = true,
                        .escape_clears_all = true,
                        .chars_hexadecimal = true,
                    },
                    .hint = "deadbeef",
                    .buf = &buf,
                })) {
                    // trim null terminator
                    const trimmed = mem.sliceTo(&buf, 0);
                    if (trimmed.len > 0) {
                        const addr_null: ?u64 = fmt.parseInt(u64, trimmed, 16) catch |err| blk: {
                            log.errf("unable to parse hex string \"{s}\" to int: {!}", .{ buf, err });
                            break :blk null;
                        };
                        if (addr_null) |addr| {
                            self.state.dbg.enqueue(proto.SetHexWindowAddressRequest{
                                .address = types.Address.from(addr),
                            });
                        }
                    }
                }

                if (zui.isItemActive()) self.typing_in_textbox = true;
            }

            // @TODO (jrc): display this nicely
            if (self.state.dbg_state.paused) |paused| {
                // @TODO (jrc): support multiple hex values
                if (paused.hex_displays.len > 0) {
                    const hex = paused.hex_displays[0];
                    var ndx: usize = 0;
                    while (ndx < hex.contents.len) : (ndx += 8) {
                        const addr = hex.address.int() + ndx;
                        var buf = [_]u8{0} ** 8;
                        @memcpy(&buf, hex.contents[ndx .. ndx + 8]);

                        // print the raw bytes
                        zui.text("0x{x:0>16}: {x:0>2} {x:0>2} {x:0>2} {x:0>2} {x:0>2} {x:0>2} {x:0>2} {x:0>2}", .{
                            addr,
                            buf[0],
                            buf[1],
                            buf[2],
                            buf[3],
                            buf[4],
                            buf[5],
                            buf[6],
                            buf[7],
                        });

                        // print it as ascii, turning null-terminators in to empty space first
                        for (buf, 0..) |c, i| {
                            if (c == 0) buf[i] = ' ';
                        }
                        zui.text("                    {c:2} {c:2} {c:2} {c:2} {c:2} {c:2} {c:2} {c:2}", .{
                            buf[0],
                            buf[1],
                            buf[2],
                            buf[3],
                            buf[4],
                            buf[5],
                            buf[6],
                            buf[7],
                        });
                    }
                }
            }
        }
    }

    self.displaySourceFiles();
    self.drawMainDockspace();

    return self.view();
}

fn drawSpaceMenuHelp(self: *Self) void {
    const z = trace.zone(@src());
    defer z.end();

    const name = "Open View:";

    if (self.open_space_menu) {
        // should only be called once to open the popup, not every frame
        self.open_space_menu = false;
        zui.openPopup(name, .{});
    }

    // center the window
    const center = zui.getViewportCenter(zui.getMainViewport());
    zui.setNextWindowPos(center.x, center.y, 0.5, 0.5);

    if (zui.beginPopupModal(name, .{ .flags = .{ .always_auto_resize = true } })) {
        defer zui.endPopup();

        zui.textUnformatted("Open:");

        zui.bullet();
        zui.textUnformattedColored(.{ 0, 0.8, 0, 1 }, "f:");
        zui.sameLine(.{});
        zui.textUnformatted("File Picker");

        zui.bullet();
        zui.textUnformattedColored(.{ 0, 0.8, 0, 1 }, "b:");
        zui.sameLine(.{});
        zui.textUnformatted("Breakpoints");

        zui.bullet();
        zui.textUnformattedColored(.{ 0, 0.8, 0, 1 }, "q:");
        zui.sameLine(.{});
        zui.textUnformatted("Cancel");
    }
}

fn displaySourceFiles(self: *Self) void {
    const z = trace.zone(@src());
    defer z.end();

    defer self.state.newly_opened_file = null;

    if (!self.open_windows.sources) return;

    defer zui.end();
    if (zui.begin(windows.Primary_Sources, .{ .flags = .{
        .no_title_bar = true,
    } })) {
        defer zui.endTabBar();
        if (zui.beginTabBar("Sources", .{
            .reorderable = true,
            .tab_list_popup_button = self.state.open_files.items.len > 0,
            .auto_select_new_tabs = true,
        })) {
            var to_remove = ArrayList(usize).init(self.state.scratch_alloc);
            for (self.state.open_files.items, 0..) |*src, file_ndx| {
                if (!src.open) {
                    to_remove.append(file_ndx) catch |err| {
                        log.errf("unable to mark file for removal: {!}", .{err});
                    };
                    continue;
                }

                var name_zero = [_]u8{0} ** 256;
                mem.copyForwards(u8, &name_zero, src.name);
                const name: [:0]u8 = @ptrCast(&name_zero);

                if (zui.beginTabItem(name, .{
                    .p_open = &src.open,
                    .flags = .{
                        .set_selected = self.state.newly_opened_file != null and
                            self.state.newly_opened_file.? == file_ndx,
                    },
                })) {
                    defer zui.endTabItem();

                    if (!src.open) {
                        to_remove.append(file_ndx) catch |err| {
                            log.errf("unable to mark file for removal: {!}", .{err});
                        };
                        continue;
                    }

                    self.state.open_source_file_ndx = file_ndx;

                    zui.pushStyleColor4f(.{
                        .idx = .child_bg,
                        .c = .{ 0.1, 0.1, 0.1, 1 },
                    });
                    defer zui.popStyleColor(.{});

                    const line_vertical_padding = zui.getStyle().item_spacing.y;
                    var total_text_y_offset: f32 = 0;

                    if (zui.beginChild(name, .{})) {
                        defer zui.endChild();

                        const max_line_number_width = countNumberOfDigits(src.lines.len);

                        for (src.lines, 0..) |line, line_ndx| {
                            const line_num = line_ndx + 1;
                            const line_num_padding_len = max_line_number_width - countNumberOfDigits(line_num);
                            const line_num_padding = self.state.scratch_alloc.alloc(u8, line_num_padding_len) catch |err| {
                                log.errf("unable to allocate line number padding: {!}", .{err});
                                continue;
                            };
                            @memset(line_num_padding, ' ');

                            {
                                var line_color: ?[4]f32 = null;
                                defer if (line_color != null) zui.popStyleColor(.{});

                                for (self.state.dbg_state.breakpoints) |bp| {
                                    assert(!bp.flags.internal); // these should never be passed from the debugger to the GUI

                                    if (bp.source_location) |loc| {
                                        if (loc.file_hash == src.abs_path_hash and loc.line.int() == line_ndx + 1) {
                                            line_color = if (bp.flags.active) colors.BreakpointActive else colors.BreakpointInctive;
                                            break;
                                        }
                                    }
                                }
                                if (line_color) |c| zui.pushStyleColor4f(.{ .idx = .text, .c = c });

                                // null-terminate the line because imgui expects it
                                const line_num_str = fmt.allocPrint(self.state.scratch_alloc, "{d}{s}\x00", .{
                                    line_num,
                                    line_num_padding,
                                }) catch |err| {
                                    log.errf("unable to line number text at line num: {d}: {!}", .{
                                        line_num,
                                        err,
                                    });
                                    continue;
                                };
                                zui.text("{s}", .{line_num_str});
                                zui.sameLine(.{});
                            }

                            var paused_color: ?[4]f32 = null;
                            defer if (paused_color != null) zui.popStyleColor(.{});

                            if (self.state.dbg_state.paused) |paused| {
                                if (paused.source_location) |src_loc| {
                                    if (src.abs_path_hash == src_loc.file_hash and src_loc.line.int() == line_num) {
                                        paused_color = colors.StoppedAtLine;
                                    }
                                }
                            }
                            if (paused_color) |c| zui.pushStyleColor4f(.{ .idx = .text, .c = c });

                            const full_line = fmt.allocPrint(self.state.scratch_alloc, "{s}\x00", .{line}) catch |err| {
                                log.errf("unable to format full line text at line num: {d}: {!}", .{
                                    line_num,
                                    err,
                                });
                                continue;
                            };

                            if (src.language == .Unsupported or line.len == 0) {
                                // non-clickable line
                                zui.text("{s}", .{full_line});
                            } else {
                                // clickable line

                                // @TODO (jrc): only allow clicking on lines that have known locations
                                // in the target's debug symbols
                                if (zui.selectable(@ptrCast(full_line), .{})) {
                                    self.state.updateBreakpoint(src.abs_path, types.SourceLine.from(line_num));
                                }
                            }

                            // if the user stepped to a new line in this file, scroll to it
                            if (self.state.scroll_to_line_of_text) |scroll_to_loc| {
                                if (self.state.has_waited_one_frame_to_scroll_to_line_of_text) {
                                    if (scroll_to_loc.file_hash == src.abs_path_hash and
                                        scroll_to_loc.line.int() == line_ndx)
                                    {
                                        // @NOTE (jrc): this extra offset of 250 is pretty bogus, but it's a
                                        // stop-gap to somewhat center the text while also not requiring me
                                        // to spend a million hours fucking around with DearIMGUI when I
                                        // want to replace it anyways.
                                        zui.setScrollYFloat(total_text_y_offset - 250);
                                        self.state.scroll_to_line_of_text = null;
                                    }
                                } else {
                                    self.state.has_waited_one_frame_to_scroll_to_line_of_text = true;
                                }
                            }

                            total_text_y_offset += zui.calcTextSize(full_line, .{}).y + line_vertical_padding;
                        }
                    }
                }
            }

            for (to_remove.items) |ndx| self.state.closeSourceFile(ndx);
        }
    }
}

fn countNumberOfDigits(num: usize) usize {
    var count: usize = 1;
    var remaining = num;
    const maxIterations = 1024;
    for (0..maxIterations) |_| {
        remaining /= 10;
        if (remaining == 0) break;

        count += 1;
    }

    return count;
}

test "countNumberOfDigits" {
    try t.expectEqual(@as(usize, 1), countNumberOfDigits(0));
    try t.expectEqual(@as(usize, 1), countNumberOfDigits(5));
    try t.expectEqual(@as(usize, 1), countNumberOfDigits(9));

    try t.expectEqual(@as(usize, 2), countNumberOfDigits(10));
    try t.expectEqual(@as(usize, 2), countNumberOfDigits(11));
    try t.expectEqual(@as(usize, 2), countNumberOfDigits(15));

    try t.expectEqual(@as(usize, 3), countNumberOfDigits(100));
    try t.expectEqual(@as(usize, 3), countNumberOfDigits(101));
    try t.expectEqual(@as(usize, 6), countNumberOfDigits(999_999));
}

// @TODO (jrc): this is all temporary and we will re-think the entire UX
fn drawMainDockspace(self: *Self) void {
    const z = trace.zone(@src());
    defer z.end();

    if (!self.state.first_frame) return;

    const main_dockspace = zui.dockBuilderGetNode(self.gui.getMainDockspaceID());
    if (zui.dockNodeIsSplitNode(main_dockspace)) {
        // we've already set up the default workspace from a different
        // run of the program
        return;
    }

    log.debug("initializing window layout");

    // our initial layout
    const dock_id: struct {
        top_left: zui.ID,
        bottom_left: zui.ID,
        top_right: zui.ID,
        mid_right: zui.ID,
        bottom_right: zui.ID,
        bottom_bottom_right: zui.ID,
    } = dock_id: {
        var dock_id_top_left: zui.ID = undefined;
        var dock_id_bottom_left: zui.ID = undefined;
        var dock_id_top_right: zui.ID = undefined;
        var dock_id_mid_right: zui.ID = undefined;
        var dock_id_bottom_right: zui.ID = undefined;
        var dock_id_bottom_bottom_right: zui.ID = undefined;

        const width = 0.5;
        const height = 0.3;

        _ = zui.dockBuilderSplitNode(
            self.gui.getMainDockspaceID(),
            zui.Direction.left,
            width,
            &dock_id_top_left,
            &dock_id_top_right,
        );

        _ = zui.dockBuilderSplitNode(
            dock_id_top_left,
            zui.Direction.down,
            height,
            &dock_id_bottom_left,
            &dock_id_top_left,
        );

        _ = zui.dockBuilderSplitNode(
            dock_id_top_right,
            zui.Direction.down,
            height + 0.3,
            &dock_id_mid_right,
            &dock_id_top_right,
        );

        _ = zui.dockBuilderSplitNode(
            dock_id_mid_right,
            zui.Direction.down,
            height + 0.3,
            &dock_id_bottom_right,
            &dock_id_mid_right,
        );

        _ = zui.dockBuilderSplitNode(
            dock_id_bottom_right,
            zui.Direction.down,
            height + 0.3,
            &dock_id_bottom_bottom_right,
            &dock_id_bottom_right,
        );

        break :dock_id .{
            .top_left = dock_id_top_left,
            .bottom_left = dock_id_bottom_left,
            .top_right = dock_id_top_right,
            .mid_right = dock_id_mid_right,
            .bottom_right = dock_id_bottom_right,
            .bottom_bottom_right = dock_id_bottom_bottom_right,
        };
    };

    zui.dockBuilderDockWindow(windows.Primary_Sources, dock_id.top_left);
    zui.dockBuilderDockWindow(windows.Primary_Output, dock_id.bottom_left);
    zui.dockBuilderDockWindow(windows.Primary_Watch, dock_id.top_right);
    zui.dockBuilderDockWindow(windows.Primary_Locals, dock_id.mid_right);
    zui.dockBuilderDockWindow(windows.Primary_CallStack, dock_id.bottom_right);
    zui.dockBuilderDockWindow(windows.Primary_Hex, dock_id.bottom_bottom_right);

    zui.dockBuilderFinish(self.gui.getMainDockspaceID());
}

pub fn addWatchValue(self: *Self, val: []const u8) Allocator.Error!void {
    if (val.len == 0) return;

    // copy to the heap
    const str = try safe.copySlice(u8, self.state.perm_alloc, val);

    // ensure there are no duplicates
    for (self.watch_vars.items) |w| {
        if (mem.eql(u8, w, str)) {
            log.warnf("skipping dupliate watch value: {s}", .{str});
            self.state.perm_alloc.free(str);
            return;
        }
    }

    try self.watch_vars.append(str);

    const alloc = self.state.dbg.requests.alloc;
    var num_allocated: usize = 0;
    const items = try alloc.alloc([]const u8, self.watch_vars.items.len);
    errdefer {
        for (0..num_allocated) |ndx| alloc.free(items[ndx]);
        alloc.free(items);
    }

    for (self.watch_vars.items, 0..) |it, ndx| {
        items[ndx] = try safe.copySlice(u8, alloc, it);
        num_allocated += 1;
    }

    self.state.dbg.enqueue(proto.SetWatchExpressionsRequest{ .expressions = items });
}

fn renderExpressionResult(
    self: *Self,
    scratch: Allocator,
    paused: *const types.PauseData,
    expr_res: types.ExpressionResult,
) void {
    const z = trace.zone(@src());
    defer z.end();

    assert(expr_res.fields.len > 0);

    // display the name and data type of the field
    if (zui.tableNextColumn()) {
        // the field data type is always encapsulated in the first field in the list
        const field = expr_res.fields[0];

        zui.pushStyleColor4f(.{ .idx = .text, .c = colors.EncodingMetaText });
        defer zui.popStyleColor(.{});

        const type_name = paused.getString(field.data_type_name);
        zui.text("{s}", .{type_name});

        // if rendering an array/string, first render the length
        if (!strings.eql(type_name, types.Unknown)) {
            switch (field.encoding) {
                .array => |arr| {
                    zui.textWrapped("(len: {d})", .{arr.items.len});
                },
                .primitive => |p| {
                    switch (p.encoding) {
                        .string => {
                            if (paused.strings.get(field.data.?)) |data| {
                                zui.textWrapped("(len: {d})", .{data.len});
                            }
                        },
                        else => {},
                    }
                },
                else => {},
            }
        }
    }

    if (zui.tableNextColumn()) {
        const first_field = expr_res.fields[0];
        for (expr_res.fields, 0..) |field, field_ndx| {
            // if we're rendering a pointer value, display the address as well
            if (field.address) |addr| {
                // @TODO (jrc): clean up the display of this address, it looks terrible
                const line = fmt.allocPrint(scratch, "0x{x}\x00", .{addr}) catch |err| {
                    log.errf("unable to render variable address 0x{x}: {!}", .{ addr, err });
                    return;
                };

                // send to the memory hex window
                if (zui.selectable(@ptrCast(line), .{})) {
                    self.state.dbg.enqueue(proto.SetHexWindowAddressRequest{ .address = addr });
                }
            }

            // if rendering a list of array values, label each of their indicies
            if (first_field.encoding == .array and field_ndx > 0) {
                zui.pushStyleColor4f(.{ .idx = .text, .c = colors.EncodingMetaText });
                defer zui.popStyleColor(.{});

                zui.text("{d}: ", .{field_ndx - 1});
                zui.sameLine(.{});
            }

            const data = blk: {
                if (field.data == null) break :blk "";

                break :blk paused.strings.get(field.data.?) orelse {
                    log.err("unable to find raw symbol render data");
                    continue;
                };
            };

            const buf = switch (field.encoding) {
                // noop, we are rendering the preview via other fields in the list
                .array => continue,

                .primitive => |primitive| switch (primitive.encoding) {
                    .boolean => renderWatchBoolean(scratch, data) catch |err| e: {
                        log.errf("unable to render boolean watch value: {!}", .{err});
                        break :e types.Unknown;
                    },
                    .signed => renderWatchInteger(scratch, data, .signed) catch |err| e: {
                        log.errf("unable to render signed integer watch value: {!}", .{err});
                        break :e types.Unknown;
                    },
                    .unsigned => renderWatchInteger(scratch, data, .unsigned) catch |err| e: {
                        log.errf("unable to render unsigned integer watch value: {!}", .{err});
                        break :e types.Unknown;
                    },
                    .float => renderWatchFloat(scratch, data) catch |err| e: {
                        log.errf("unable to render float watch value: {!}", .{err});
                        break :e types.Unknown;
                    },
                    .string => e: {
                        break :e data;
                    },

                    else => e: {
                        log.warnf(
                            "unsupported primitive encoding: {s}",
                            .{@tagName(primitive.encoding)},
                        );
                        break :e types.Unknown;
                    },
                },

                else => types.Unknown, // @DELETEME (jrc): once all encodings are implemeted
            };

            zui.textWrapped("{s}", .{buf});
        }
    }
}

fn renderWatchBoolean(scratch: Allocator, buf: []const u8) ![]const u8 {
    assert(buf.len == 1);

    if (buf.len > 0 and buf[0] != 0) {
        return safe.copySlice(u8, scratch, "true");
    }

    return safe.copySlice(u8, scratch, "false");
}

test "renderWatchBoolean" {
    var arena = std.heap.ArenaAllocator.init(t.allocator);
    defer arena.deinit();
    const alloc = arena.allocator();

    try t.expectEqualStrings("true", try renderWatchBoolean(alloc, &[_]u8{1}));
    try t.expectEqualStrings("true", try renderWatchBoolean(alloc, &[_]u8{2}));
    try t.expectEqualStrings("false", try renderWatchBoolean(alloc, &[_]u8{0}));
}

fn renderWatchInteger(scratch: Allocator, buf: []const u8, signedness: types.Signedness) ![]const u8 {
    var r: Reader = undefined;
    r.init(buf);

    const n: i128 = switch (buf.len) {
        1 => if (signedness == .signed) try r.read(i8) else try r.read(u8),
        2 => if (signedness == .signed) try r.read(i16) else try r.read(u16),
        4 => if (signedness == .signed) try r.read(i32) else try r.read(u32),
        8 => if (signedness == .signed) try r.read(i64) else try r.read(u64),
        16 => try r.read(i128),
        else => {
            return error.InvalidIntegerSize;
        },
    };

    return try fmt.allocPrint(scratch, "{d}", .{n});
}

test "renderWatchInteger" {
    var arena = std.heap.ArenaAllocator.init(t.allocator);
    defer arena.deinit();
    const alloc = arena.allocator();

    {
        // one byte
        try t.expectEqualStrings("0", try renderWatchInteger(alloc, &[_]u8{0}, .signed));
        try t.expectEqualStrings("0", try renderWatchInteger(alloc, &[_]u8{0}, .unsigned));

        try t.expectEqualStrings("127", try renderWatchInteger(alloc, &[_]u8{127}, .signed));
        try t.expectEqualStrings("-128", try renderWatchInteger(alloc, &[_]u8{128}, .signed));
        try t.expectEqualStrings("-1", try renderWatchInteger(alloc, &[_]u8{255}, .signed));

        try t.expectEqualStrings("128", try renderWatchInteger(alloc, &[_]u8{128}, .unsigned));
        try t.expectEqualStrings("255", try renderWatchInteger(alloc, &[_]u8{255}, .unsigned));
    }

    {
        // two bytes
        try t.expectEqualStrings("0", try renderWatchInteger(alloc, &[_]u8{ 0, 0 }, .signed));
        try t.expectEqualStrings("0", try renderWatchInteger(alloc, &[_]u8{ 0, 0 }, .unsigned));

        try t.expectEqualStrings("-32768", try renderWatchInteger(alloc, &[_]u8{ 0, 128 }, .signed));
        try t.expectEqualStrings("-1", try renderWatchInteger(alloc, &[_]u8{ 255, 255 }, .signed));

        try t.expectEqualStrings("32768", try renderWatchInteger(alloc, &[_]u8{ 0, 128 }, .unsigned));
        try t.expectEqualStrings("65535", try renderWatchInteger(alloc, &[_]u8{ 255, 255 }, .unsigned));
    }

    {
        // four bytes
        try t.expectEqualStrings("-2147483648", try renderWatchInteger(alloc, &[_]u8{ 0, 0, 0, 128 }, .signed));
        try t.expectEqualStrings("-1", try renderWatchInteger(alloc, &[_]u8{ 255, 255, 255, 255 }, .signed));

        try t.expectEqualStrings("4294967295", try renderWatchInteger(alloc, &[_]u8{ 255, 255, 255, 255 }, .unsigned));
    }

    {
        // eight bytes
        try t.expectEqualStrings("-9223372036854775808", try renderWatchInteger(
            alloc,
            &[_]u8{ 0, 0, 0, 0, 0, 0, 0, 128 },
            .signed,
        ));
        try t.expectEqualStrings("-1", try renderWatchInteger(
            alloc,
            &[_]u8{ 255, 255, 255, 255, 255, 255, 255, 255 },
            .signed,
        ));

        try t.expectEqualStrings("18446744073709551615", try renderWatchInteger(
            alloc,
            &[_]u8{ 255, 255, 255, 255, 255, 255, 255, 255 },
            .unsigned,
        ));
    }

    {
        // sixteen bytes only supports i128, not u128
        try t.expectEqualStrings("-170141183460469231731687303715884105728", try renderWatchInteger(
            alloc,
            &[_]u8{ 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 128 },
            .signed,
        ));
        try t.expectEqualStrings("-1", try renderWatchInteger(
            alloc,
            &[_]u8{ 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255 },
            .signed,
        ));

        // largest possible i128 value
        try t.expectEqualStrings("170141183460469231731687303715884105727", try renderWatchInteger(
            alloc,
            &[_]u8{ 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 127 },
            .unsigned,
        ));
    }

    {
        // invalid number of bytes
        try t.expectError(error.InvalidIntegerSize, renderWatchInteger(alloc, &[_]u8{ 0, 0, 0 }, .unsigned));
    }
}

/// @NEEDSTEST
fn renderWatchFloat(scratch: Allocator, buf: []const u8) ![]const u8 {
    var r: Reader = undefined;
    r.init(buf);

    switch (buf.len) {
        2 => {
            const f = try r.read(f16);
            return try fmt.allocPrint(scratch, "{d}", .{f});
        },
        4 => {
            const f = try r.read(f32);
            return try fmt.allocPrint(scratch, "{d}", .{f});
        },
        8 => {
            const f = try r.read(f64);
            return try fmt.allocPrint(scratch, "{d}", .{f});
        },
        16 => {
            const f = try r.read(f128);
            return try fmt.allocPrint(scratch, "{d}", .{f});
        },
        else => {
            return error.InvalidIntegerSize;
        },
    }
}

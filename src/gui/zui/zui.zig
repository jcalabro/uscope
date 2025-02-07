const std = @import("std");
const mem = std.mem;

const zui = @import("../zui.zig");

const cimgui = @import("cimgui");
const imgui = cimgui.c;

////////////////////////////////////////////////////////////////

pub fn getWindowSize() zui.ImVec2 {
    var size = mem.zeroes(zui.ImVec2);
    imgui.igGetWindowSize(&size);
    return size;
}

////////////////////////////////////////////////////////////////

pub fn begin(name: [:0]const u8, args: zui.Begin) bool {
    return imgui.igBegin(name, args.popen, @bitCast(args.flags));
}

pub fn end() void {
    imgui.igEnd();
}

pub fn beginChild(str_id: [:0]const u8, args: zui.BeginChild) bool {
    const size = zui.ImVec2{ .x = args.w, .y = args.h };
    return imgui.igBeginChild_Str(str_id, size, args.border, @bitCast(args.flags));
}

pub fn endChild() void {
    imgui.igEndChild();
}

pub fn sameLine(args: zui.SameLine) void {
    imgui.igSameLine(args.offset_from_start_x, args.spacing);
}

////////////////////////////////////////////////////////////////

pub fn setNextWindowSize(w: f32, h: f32) void {
    const window_size = zui.ImVec2{
        .x = w,
        .y = h,
    };
    imgui.igSetNextWindowSize(window_size, imgui.ImGuiCond_Always);
}

pub fn setNextWindowPos(x: f32, y: f32, pivot_x: f32, pivot_y: f32) void {
    const window_pos = zui.ImVec2{ .x = x, .y = y };
    const pivot = zui.ImVec2{ .x = pivot_x, .y = pivot_y };
    imgui.igSetNextWindowPos(window_pos, imgui.ImGuiCond_Always, pivot);
}

pub fn setNextWindowFocus() void {
    imgui.igSetNextWindowFocus();
}

pub fn isItemFocused() bool {
    return imgui.igIsItemFocused();
}

pub fn isItemActive() bool {
    return imgui.igIsItemActive();
}

////////////////////////////////////////////////////////////////

pub fn pushItemWidth(width: f32) void {
    imgui.igPushItemWidth(width);
}

pub fn popItemWidth() void {
    imgui.igPopItemWidth();
}

////////////////////////////////////////////////////////////////

pub fn beginPopupModal(label: [:0]const u8, args: zui.BeginPopupModal) bool {
    return imgui.igBeginPopupModal(
        label,
        args.p_open,
        @bitCast(args.flags),
    );
}

pub fn endPopup() void {
    imgui.igEndPopup();
}

pub fn openPopup(label: [:0]const u8, args: zui.OpenPopup) void {
    imgui.igOpenPopup_Str(label, @bitCast(args.flags));
}

pub fn openPopupOnItemClick(label: [:0]const u8, args: zui.OpenPopup) void {
    imgui.igOpenPopup_Str(label, @bitCast(args.flags));
}

pub fn beginPopupContextItem(label: [:0]const u8, args: zui.OpenPopup) bool {
    return imgui.igBeginPopupContextItem(label, @bitCast(args.flags));
}

pub fn beginTooltip() bool {
    return imgui.igBeginTooltip();
}

pub fn endTooltip() void {
    imgui.igEndTooltip();
}

////////////////////////////////////////////////////////////////

pub fn beginMainMenuBar() bool {
    return imgui.igBeginMainMenuBar();
}

pub fn endMainMenuBar() void {
    imgui.igEndMainMenuBar();
}

pub fn beginMenu(label: [*c]const u8, enabled: bool) bool {
    return imgui.igBeginMenu(label, enabled);
}

pub fn endMenu() void {
    imgui.igEndMenu();
}

pub fn menuItem(label: [:0]const u8, args: zui.MenuItem) bool {
    return imgui.igMenuItem_Bool(
        label,
        if (args.shortcut) |s| s.ptr else null,
        args.selected,
        args.enabled,
    );
}

////////////////////////////////////////////////////////////////

pub fn inputTextWithHint(label: [:0]const u8, args: zui.InputTextWithHint) bool {
    return imgui.igInputTextWithHint(
        label,
        args.hint,
        args.buf.ptr,
        args.buf.len,
        @bitCast(args.flags),
        if (args.callback) |cb| @ptrCast(cb) else null,
        args.user_data,
    );
}

pub fn setKeyboardFocusHere(offset: i32) void {
    imgui.igSetKeyboardFocusHere(offset);
}

pub fn getMousePos() zui.ImVec2 {
    var pos = mem.zeroes(zui.ImVec2);
    imgui.igGetMousePos(&pos);
    return pos;
}

pub fn setScrollYFloat(scroll_y: f32) void {
    imgui.igSetScrollY_Float(scroll_y);
}

////////////////////////////////////////////////////////////////

pub fn beginTable(name: [:0]const u8, args: zui.BeginTable) bool {
    const outer_size = zui.ImVec2{
        .x = args.outer_size[0],
        .y = args.outer_size[1],
    };
    return imgui.igBeginTable(
        name,
        args.column,
        @bitCast(args.flags),
        outer_size,
        args.inner_width,
    );
}

pub fn endTable() void {
    imgui.igEndTable();
}

pub fn tableNextRow(args: zui.TableNextRow) void {
    imgui.igTableNextRow(
        @bitCast(args.row_flags),
        args.min_row_height,
    );
}

pub fn tableNextColumn() bool {
    return imgui.igTableNextColumn();
}

pub fn tableHeadersRow() void {
    imgui.igTableHeadersRow();
}

pub fn tableSetupColumn(label: [:0]const u8, args: zui.TableSetupColumn) void {
    imgui.igTableSetupColumn(
        label,
        @bitCast(args.flags),
        args.init_width_or_height,
        args.user_id,
    );
}

pub fn tableSetupScrollFreeze(cols: c_int, rows: c_int) void {
    imgui.igTableSetupScrollFreeze(cols, rows);
}

////////////////////////////////////////////////////////////////

pub fn beginTabBar(name: [:0]const u8, flags: zui.TabBarFlags) bool {
    return imgui.igBeginTabBar(name, @bitCast(flags));
}

pub fn endTabBar() void {
    imgui.igEndTabBar();
}

pub fn beginTabItem(name: [:0]const u8, args: zui.BeginTabItem) bool {
    return imgui.igBeginTabItem(
        name,
        args.p_open,
        @bitCast(args.flags),
    );
}

pub fn endTabItem() void {
    imgui.igEndTabItem();
}

////////////////////////////////////////////////////////////////

pub fn getStyle() *zui.Style {
    const s = imgui.igGetStyle();
    return @ptrCast(s);
}

pub fn pushStyleColor4f(args: zui.PushStyleColor4f) void {
    const idx: c_int = @intCast(@intFromEnum(args.idx));
    imgui.igPushStyleColor_Vec4(idx, .{
        .x = args.c[0],
        .y = args.c[1],
        .z = args.c[2],
        .w = args.c[3],
    });
}

pub fn popStyleColor(args: zui.PopStyleColor) void {
    imgui.igPopStyleColor(args.count);
}

////////////////////////////////////////////////////////////////

pub fn tableSetBgColor(args: zui.TableSetBgColor) void {
    imgui.igTableSetBgColor(
        @intCast(@intFromEnum(args.target)),
        args.color,
        args.column_n,
    );
}

pub fn selectable(name: [:0]const u8, args: zui.Selectable) bool {
    const size = zui.ImVec2{ .x = args.w, .y = args.h };
    return imgui.igSelectable_Bool(
        name,
        args.selected,
        @bitCast(args.flags),
        size,
    );
}

pub fn isItemHovered(flags: zui.HoveredFlags) bool {
    return imgui.igIsItemHovered(@bitCast(flags));
}

////////////////////////////////////////////////////////////////

pub fn button(label: [:0]const u8, args: zui.Button) bool {
    const size = zui.ImVec2{ .x = args.w, .y = args.h };
    return imgui.igButton(label, size);
}

pub fn isMouseClicked(btn: zui.MouseButton) bool {
    return imgui.igIsMouseClicked_Bool(
        @intCast(@intFromEnum(btn)),
        false,
    );
}

pub fn getCursorPosX() f32 {
    return imgui.igGetCursorPosX();
}

pub fn getCursorPosY() f32 {
    return imgui.igGetCursorPosY();
}

pub fn setCursorPosX(pos: f32) void {
    imgui.igSetCursorPosX(pos);
}

pub fn setCursorPosY(pos: f32) void {
    imgui.igSetCursorPosY(pos);
}

////////////////////////////////////////////////////////////////

pub fn colorConvertFloat4ToU32(in: [4]f32) u32 {
    const vec = zui.ImVec4{
        .x = in[0],
        .y = in[1],
        .z = in[2],
        .w = in[3],
    };
    return imgui.igColorConvertFloat4ToU32(vec);
}

////////////////////////////////////////////////////////////////

pub fn formatZ(comptime fmt: []const u8, args: anytype) [:0]const u8 {
    const len = std.fmt.count(fmt ++ "\x00", args);
    if (len > zui.temp_buffer.items.len) zui.temp_buffer.resize(len + 64) catch unreachable;
    return std.fmt.bufPrintZ(zui.temp_buffer.items, fmt, args) catch unreachable;
}

pub fn textUnformatted(txt: []const u8) void {
    imgui.igTextUnformatted(txt.ptr, txt.ptr + txt.len);
}

pub fn textUnformattedColored(color: [4]f32, txt: []const u8) void {
    pushStyleColor4f(.{ .idx = .text, .c = color });
    textUnformatted(txt);
    popStyleColor(.{});
}

pub fn text(comptime fmt: []const u8, args: anytype) void {
    imgui.igText("%s", formatZ(fmt, args).ptr);
}

pub fn textWrapped(comptime fmt: []const u8, args: anytype) void {
    imgui.igTextWrapped("%s", formatZ(fmt, args).ptr);
}

pub fn centerText(txt: [:0]const u8) void {
    const win_size = getWindowSize();
    const txt_size = calcTextSize(txt, .{});

    setCursorPosX((win_size.x - txt_size.x) * 0.5);
    setCursorPosY((win_size.y - txt_size.y) * 0.5);
}

pub fn dummy(size: zui.ImVec2) void {
    imgui.igDummy(size);
}

////////////////////////////////////////////////////////////////

pub fn calcTextSize(txt: []const u8, args: zui.CalcTextSize) zui.ImVec2 {
    var res = zui.ImVec2{};
    imgui.igCalcTextSize(
        &res,
        txt.ptr,
        txt.ptr + txt.len,
        args.hide_text_after_double_hash,
        args.wrap_width,
    );
    return res;
}

////////////////////////////////////////////////////////////////

pub fn bullet() void {
    imgui.igBullet();
}

pub fn checkbox(label: [:0]const u8, args: zui.Checkbox) bool {
    return imgui.igCheckbox(label, args.v);
}

////////////////////////////////////////////////////////////////

pub fn getMainViewport() *imgui.ImGuiViewport {
    return imgui.igGetMainViewport();
}

pub fn getViewportCenter(viewport: *imgui.ImGuiViewport) zui.ImVec2 {
    var out = mem.zeroes(zui.ImVec2);
    imgui.ImGuiViewport_GetCenter(&out, viewport);
    return out;
}

pub fn dockSpaceOverViewport(args: zui.DockSpaceOverViewport) zui.ID {
    const viewport = if (args.viewport) |v| v else getMainViewport();
    return imgui.igDockSpaceOverViewport(
        viewport,
        @bitCast(args.flags),
        @alignCast(@ptrCast(args.window_class)),
    );
}

pub fn dockBuilderGetNode(node_id: imgui.ImGuiID) ?*imgui.ImGuiDockNode {
    return imgui.igDockBuilderGetNode(node_id);
}

pub fn dockBuilderSplitNode(node_id: zui.ID, dir: zui.Direction, size_ratio_for_node_at_dir: f32, out_id_at_dir: *zui.ID, out_id_at_opposite_dir: *zui.ID) zui.ID {
    return imgui.igDockBuilderSplitNode(
        node_id,
        @intFromEnum(dir),
        size_ratio_for_node_at_dir,
        out_id_at_dir,
        out_id_at_opposite_dir,
    );
}

pub fn dockNodeIsSplitNode(node: ?*imgui.ImGuiDockNode) bool {
    return imgui.ImGuiDockNode_IsSplitNode(node);
}

pub fn dockBuilderDockWindow(window_name: [*c]const u8, node_id: imgui.ImGuiID) void {
    imgui.igDockBuilderDockWindow(window_name, node_id);
}

pub fn dockBuilderFinish(node_id: imgui.ImGuiID) void {
    imgui.igDockBuilderFinish(node_id);
}

////////////////////////////////////////////////////////////////

pub fn treeNode(label: [:0]const u8) bool {
    return imgui.igTreeNode_Str(label);
}

pub fn treePop() void {
    return imgui.igTreePop();
}

////////////////////////////////////////////////////////////////

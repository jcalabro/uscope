const std = @import("std");
const builtin = @import("builtin");
const Allocator = mem.Allocator;
const ArrayList = std.ArrayList;
const mem = std.mem;

pub const cimgui = @import("cimgui");
const imgui = cimgui.c;
pub const ImVec2 = imgui.ImVec2;
pub const ImVec4 = imgui.ImVec4;

const zui = switch (builtin.is_test) {
    true => @import("zui/stubs.zig"),
    false => @import("zui/zui.zig"),
};
pub usingnamespace zui;

// @TODO (jrc): better memory management
pub var temp_buffer: ArrayList(u8) = undefined;

pub fn init(alloc: Allocator) void {
    temp_buffer = ArrayList(u8).init(alloc);
}

pub fn deinit() void {
    temp_buffer.deinit();
}

////////////////////////////////////////////////////////////////

//
// @NOTE (jrc): All these packed structs are specifically ported from
// the v1.89.9-docking branch. If upgrading, all these also be checked.
//

////////////////////////////////////////////////////////////////

pub const Context = *opaque {};

pub const ID = c_uint;

pub const Wchar = u16;

pub const Direction = enum(i32) {
    none = -1,
    left = 0,
    right = 1,
    up = 2,
    down = 3,
};

pub const MouseButton = enum(u32) {
    left = 0,
    right = 1,
    middle = 2,
};

pub const Key = enum(u32) {
    none = 0,
    tab = 512,
    left_arrow,
    right_arrow,
    up_arrow,
    down_arrow,
    page_up,
    page_down,
    home,
    end,
    insert,
    delete,
    back_space,
    space,
    enter,
    escape,
    left_ctrl,
    left_shift,
    left_alt,
    left_super,
    right_ctrl,
    right_shift,
    right_alt,
    right_super,
    menu,
    zero,
    one,
    two,
    three,
    four,
    five,
    six,
    seven,
    eight,
    nine,
    a,
    b,
    c,
    d,
    e,
    f,
    g,
    h,
    i,
    j,
    k,
    l,
    m,
    n,
    o,
    p,
    q,
    r,
    s,
    t,
    u,
    v,
    w,
    x,
    y,
    z,
    f1,
    f2,
    f3,
    f4,
    f5,
    f6,
    f7,
    f8,
    f9,
    f10,
    f11,
    f12,
    apostrophe,
    comma,
    minus,
    period,
    slash,
    semicolon,
    equal,
    left_bracket,
    back_slash,
    right_bracket,
    grave_accent,
    caps_lock,
    scroll_lock,
    num_lock,
    print_screen,
    pause,
    keypad_0,
    keypad_1,
    keypad_2,
    keypad_3,
    keypad_4,
    keypad_5,
    keypad_6,
    keypad_7,
    keypad_8,
    keypad_9,
    keypad_decimal,
    keypad_divide,
    keypad_multiply,
    keypad_subtract,
    keypad_add,
    keypad_enter,
    keypad_equal,

    gamepad_start,
    gamepad_back,
    gamepad_faceleft,
    gamepad_faceright,
    gamepad_faceup,
    gamepad_facedown,
    gamepad_dpadleft,
    gamepad_dpadright,
    gamepad_dpadup,
    gamepad_dpaddown,
    gamepad_l1,
    gamepad_r1,
    gamepad_l2,
    gamepad_r2,
    gamepad_l3,
    gamepad_r3,
    gamepad_lstickleft,
    gamepad_lstickright,
    gamepad_lstickup,
    gamepad_lstickdown,
    gamepad_rstickleft,
    gamepad_rstickright,
    gamepad_rstickup,
    gamepad_rstickdown,

    mouse_left,
    mouse_right,
    mouse_middle,
    mouse_x1,
    mouse_x2,

    mouse_wheel_x,
    mouse_wheel_y,

    mod_ctrl = 1 << 12,
    mod_shift = 1 << 13,
    mod_alt = 1 << 14,
    mod_super = 1 << 15,
    mod_mask_ = 0xf000,
};

////////////////////////////////////////////////////////////////

pub const WindowFlags = packed struct(u32) {
    no_title_bar: bool = false,
    no_resize: bool = false,
    no_move: bool = false,
    no_scrollbar: bool = false,
    no_scroll_with_mouse: bool = false,
    no_collapse: bool = false,
    always_auto_resize: bool = false,
    no_background: bool = false,
    no_saved_settings: bool = false,
    no_mouse_inputs: bool = false,
    menu_bar: bool = false,
    horizontal_scrollbar: bool = false,
    no_focus_on_appearing: bool = false,
    no_bring_to_front_on_focus: bool = false,
    always_vertical_scrollbar: bool = false,
    always_horizontal_scrollbar: bool = false,
    always_use_window_padding: bool = false,
    no_nav_inputs: bool = false,
    no_nav_focus: bool = false,
    unsaved_document: bool = false,
    no_docking: bool = false,
    _padding: u11 = 0,

    pub const no_nav = WindowFlags{ .no_nav_inputs = true, .no_nav_focus = true };
    pub const no_decoration = WindowFlags{
        .no_title_bar = true,
        .no_resize = true,
        .no_scrollbar = true,
        .no_collapse = true,
    };
    pub const no_inputs = WindowFlags{
        .no_mouse_inputs = true,
        .no_nav_inputs = true,
        .no_nav_focus = true,
    };
};

pub const Begin = struct {
    popen: ?*bool = null,
    flags: WindowFlags = .{},
};

pub const BeginChild = struct {
    w: f32 = 0.0,
    h: f32 = 0.0,
    border: bool = false,
    flags: WindowFlags = .{},
};

pub const SameLine = struct {
    offset_from_start_x: f32 = 0.0,
    spacing: f32 = -1.0,
};

////////////////////////////////////////////////////////////////

pub const BeginPopupModal = struct {
    p_open: ?*bool = null,
    flags: WindowFlags = .{},
};

pub const PopupFlags = packed struct(u32) {
    mouse_button_left: bool = false,
    mouse_button_right: bool = false,
    mouse_button_middle: bool = false,
    mouse_button_mask_: bool = false,
    mouse_button_default_: bool = false,
    no_open_over_existing_popup: bool = false,
    no_open_over_items: bool = false,
    any_popup_id: bool = false,
    any_popup_level: bool = false,
    any_popup: bool = false,
    _padding: u22 = 0,
};

pub const OpenPopup = struct {
    flags: PopupFlags = .{},
};

////////////////////////////////////////////////////////////////

pub const MenuItem = struct {
    shortcut: ?[:0]const u8 = null,
    selected: bool = false,
    enabled: bool = true,
};

////////////////////////////////////////////////////////////////

pub const InputTextFlags = packed struct(u32) {
    chars_decimal: bool = false,
    chars_hexadecimal: bool = false,
    chars_uppercase: bool = false,
    chars_no_blank: bool = false,
    auto_select_all: bool = false,
    enter_returns_true: bool = false,
    callback_completion: bool = false,
    callback_history: bool = false,
    callback_always: bool = false,
    callback_char_filter: bool = false,
    allow_tab_input: bool = false,
    ctrl_enter_for_new_line: bool = false,
    no_horizontal_scroll: bool = false,
    always_overwrite: bool = false,
    read_only: bool = false,
    password: bool = false,
    no_undo_redo: bool = false,
    chars_scientific: bool = false,
    callback_resize: bool = false,
    callback_edit: bool = false,
    escape_clears_all: bool = false,
    _padding: u11 = 0,
};

pub const InputText = struct {
    buf: []u8,
    flags: InputTextFlags = .{},
    callback: ?InputTextCallback = null,
    user_data: ?*anyopaque = null,
};

pub const InputTextCallbackData = extern struct {
    ctx: *Context,
    event_flag: InputTextFlags,
    flags: InputTextFlags,
    user_data: ?*anyopaque,
    event_char: Wchar,
    event_key: Key,
    buf: [*]u8,
    buf_text_len: i32,
    buf_size: i32,
    buf_dirty: bool,
    cursor_pos: i32,
    selection_start: i32,
    selection_end: i32,
};

pub const InputTextCallback = *const fn (data: [*c]InputTextCallbackData) callconv(.C) i32;

pub const InputTextWithHint = struct {
    hint: [:0]const u8,
    buf: []u8,
    flags: InputTextFlags = .{},
    callback: ?InputTextCallback = null,
    user_data: ?*anyopaque = null,
};

////////////////////////////////////////////////////////////////

pub const TableBorderFlags = packed struct(u4) {
    inner_h: bool = false,
    outer_h: bool = false,
    inner_v: bool = false,
    outer_v: bool = false,

    pub const h = TableBorderFlags{
        .inner_h = true,
        .outer_h = true,
    }; // Draw horizontal borders.
    pub const v = TableBorderFlags{
        .inner_v = true,
        .outer_v = true,
    }; // Draw vertical borders.
    pub const inner = TableBorderFlags{
        .inner_v = true,
        .inner_h = true,
    }; // Draw inner borders.
    pub const outer = TableBorderFlags{
        .outer_v = true,
        .outer_h = true,
    }; // Draw outer borders.
    pub const all = TableBorderFlags{
        .inner_v = true,
        .inner_h = true,
        .outer_v = true,
        .outer_h = true,
    }; // Draw all borders.
};

pub const TableFlags = packed struct(u32) {
    resizable: bool = false,
    reorderable: bool = false,
    hideable: bool = false,
    sortable: bool = false,
    no_saved_settings: bool = false,
    context_menu_in_body: bool = false,
    row_bg: bool = false,
    borders: TableBorderFlags = .{},
    no_borders_in_body: bool = false,
    no_borders_in_body_until_resize: bool = false,

    // Sizing Policy
    sizing: enum(u3) {
        none = 0,
        fixed_fit = 1,
        fixed_same = 2,
        stretch_prop = 3,
        stretch_same = 4,
    } = .none,

    // Sizing Extra Options
    no_host_extend_x: bool = false,
    no_host_extend_y: bool = false,
    no_keep_columns_visible: bool = false,
    precise_widths: bool = false,

    // Clipping
    no_clip: bool = false,

    // Padding
    pad_outer_x: bool = false,
    no_pad_outer_x: bool = false,
    no_pad_inner_x: bool = false,

    // Scrolling
    scroll_x: bool = false,
    scroll_y: bool = false,

    // Sorting
    sort_multi: bool = false,
    sort_tristate: bool = false,

    _padding: u4 = 0,
};

pub const BeginTable = struct {
    column: i32,
    flags: TableFlags = .{},
    outer_size: [2]f32 = .{ 0, 0 },
    inner_width: f32 = 0,
};

pub const TableRowFlags = packed struct(u32) {
    headers: bool = false,

    _padding: u31 = 0,
};

pub const TableNextRow = struct {
    row_flags: TableRowFlags = .{},
    min_row_height: f32 = 0,
};

pub const TableColumnFlags = packed struct(u32) {
    // Input configuration flags
    disabled: bool = false,
    default_hide: bool = false,
    default_sort: bool = false,
    width_stretch: bool = false,
    width_fixed: bool = false,
    no_resize: bool = false,
    no_reorder: bool = false,
    no_hide: bool = false,
    no_clip: bool = false,
    no_sort: bool = false,
    no_sort_ascending: bool = false,
    no_sort_descending: bool = false,
    no_header_label: bool = false,
    no_header_width: bool = false,
    prefer_sort_ascending: bool = false,
    prefer_sort_descending: bool = false,
    indent_enable: bool = false,
    indent_disable: bool = false,

    _padding0: u6 = 0,

    // Output status flags, read-only via TableGetColumnFlags()
    is_enabled: bool = false,
    is_visible: bool = false,
    is_sorted: bool = false,
    is_hovered: bool = false,

    _padding1: u4 = 0,
};

pub const TableSetupColumn = struct {
    flags: TableColumnFlags = .{},
    init_width_or_height: f32 = 0,
    user_id: ID = 0,
};

////////////////////////////////////////////////////////////////

pub const TabBarFlags = packed struct(u32) {
    reorderable: bool = false,
    auto_select_new_tabs: bool = false,
    tab_list_popup_button: bool = false,
    no_close_with_middle_mouse_button: bool = false,
    no_tab_list_scrolling_buttons: bool = false,
    no_tooltip: bool = false,
    fitting_policy_resize_down: bool = false,
    fitting_policy_scroll: bool = false,
    _padding: u24 = 0,

    pub const fitting_policy_mask = TabBarFlags{
        .fitting_policy_resize_down = true,
        .fitting_policy_scroll = true,
    };

    pub const fitting_policy_default = TabBarFlags{ .fitting_policy_resize_down = true };
};

pub const TabItemFlags = packed struct(u32) {
    unsaved_document: bool = false,
    set_selected: bool = false,
    no_close_with_middle_mouse_button: bool = false,
    no_push_id: bool = false,
    no_tooltip: bool = false,
    no_reorder: bool = false,
    leading: bool = false,
    trailing: bool = false,
    _padding: u24 = 0,
};

pub const BeginTabItem = struct {
    p_open: ?*bool = null,
    flags: TabItemFlags = .{},
};

////////////////////////////////////////////////////////////////

pub const StyleCol = enum(u32) {
    text,
    text_disabled,
    window_bg,
    child_bg,
    popup_bg,
    border,
    border_shadow,
    frame_bg,
    frame_bg_hovered,
    frame_bg_active,
    title_bg,
    title_bg_active,
    title_bg_collapsed,
    menu_bar_bg,
    scrollbar_bg,
    scrollbar_grab,
    scrollbar_grab_hovered,
    scrollbar_grab_active,
    check_mark,
    slider_grab,
    slider_grab_active,
    button,
    button_hovered,
    button_active,
    header,
    header_hovered,
    header_active,
    separator,
    separator_hovered,
    separator_active,
    resize_grip,
    resize_grip_hovered,
    resize_grip_active,
    tab,
    tab_hovered,
    tab_active,
    tab_unfocused,
    tab_unfocused_active,
    docking_preview,
    docking_empty_bg,
    plot_lines,
    plot_lines_hovered,
    plot_histogram,
    plot_histogram_hovered,
    table_header_bg,
    table_border_strong,
    table_border_light,
    table_row_bg,
    table_row_bg_alt,
    text_selected_bg,
    drag_drop_target,
    nav_highlight,
    nav_windowing_highlight,
    nav_windowing_dim_bg,
    modal_window_dim_bg,

    pub fn int(self: @This()) usize {
        return @intFromEnum(self);
    }
};

pub const Style = extern struct {
    alpha: f32,
    disabled_alpha: f32,
    window_padding: ImVec2,
    window_rounding: f32,
    window_border_size: f32,
    window_min_size: ImVec2,
    window_title_align: ImVec2,
    window_menu_button_position: imgui.ImGuiDir,
    child_rounding: f32,
    child_border_size: f32,
    popup_rounding: f32,
    popup_border_size: f32,
    frame_padding: ImVec2,
    frame_rounding: f32,
    frame_border_size: f32,
    item_spacing: ImVec2,
    item_inner_spacing: ImVec2,
    cell_padding: ImVec2,
    touch_extra_padding: ImVec2,
    indent_spacing: f32,
    columns_min_spacing: f32,
    scrollbar_size: f32,
    scrollbar_rounding: f32,
    grab_min_size: f32,
    grab_rounding: f32,
    log_slider_deadzone: f32,
    tab_rounding: f32,
    tab_border_size: f32,
    tab_min_width_for_close_button: f32,
    color_button_position: imgui.ImGuiDir,
    button_text_align: ImVec2,
    selectable_text_align: ImVec2,
    separator_text_border_size: f32,
    separator_text_align: ImVec2,
    separator_text_padding: ImVec2,
    display_window_padding: ImVec2,
    display_safe_area_padding: ImVec2,
    docking_separator_size: f32,
    mouse_cursor_scale: f32,
    anti_aliased_lines: bool,
    anti_aliased_lines_use_tex: bool,
    anti_aliased_fill: bool,
    curve_tessellation_tol: f32,
    circle_tessellation_max_error: f32,

    colors: [@typeInfo(StyleCol).Enum.fields.len][4]f32,

    // Behaviors
    hover_stationary_delay: f32,
    hover_delay_short: f32,
    hover_delay_normal: f32,
    hover_flags_for_tooltip_mouse: imgui.ImGuiHoveredFlags,
    hover_flags_for_tooltip_nav: imgui.ImGuiHoveredFlags,

    pub fn getColor(style: Style, idx: StyleCol) [4]f32 {
        return style.colors[@intFromEnum(idx)];
    }

    pub fn setColor(style: *Style, idx: StyleCol, color: [4]f32) void {
        style.colors[@intFromEnum(idx)] = color;
    }
};

pub const PushStyleColor4f = struct {
    idx: StyleCol,
    c: [4]f32,
};

pub const PopStyleColor = struct {
    count: c_int = 1,
};

////////////////////////////////////////////////////////////////

pub const TableBgTarget = enum(u32) {
    none = 0,
    row_bg0 = 1,
    row_bg1 = 2,
    cell_bg = 3,
};

pub const TableSetBgColor = struct {
    target: TableBgTarget,
    color: u32,
    column_n: i32 = -1,
};

pub const SelectableFlags = packed struct(u32) {
    dont_close_popups: bool = false,
    span_all_columns: bool = false,
    allow_double_click: bool = false,
    disabled: bool = false,
    allow_item_overlap: bool = false,
    _padding: u27 = 0,
};

pub const Selectable = struct {
    selected: bool = false,
    flags: SelectableFlags = .{},
    w: f32 = 0,
    h: f32 = 0,
};

pub const HoveredFlags = packed struct(u32) {
    child_windows: bool = false,
    root_window: bool = false,
    any_window: bool = false,
    no_popup_hierarchy: bool = false,
    _reserved0: bool = false,
    allow_when_blocked_by_popup: bool = false,
    _reserved1: bool = false,
    allow_when_blocked_by_active_item: bool = false,
    allow_when_overlapped: bool = false,
    allow_when_disabled: bool = false,
    no_nav_override: bool = false,

    for_tooltip: bool = false,
    stationary: bool = false,
    delay_none: bool = false,
    delay_short: bool = false,
    delay_normal: bool = false,
    delay_no_shared_delay: bool = false,

    _padding: u15 = 0,

    pub const rect_only = HoveredFlags{
        .allow_when_blocked_by_popup = true,
        .allow_when_blocked_by_active_item = true,
        .allow_when_overlapped = true,
    };
    pub const root_and_child_windows = HoveredFlags{ .root_window = true, .child_windows = true };
};

////////////////////////////////////////////////////////////////

pub const Button = struct {
    w: f32 = 0.0,
    h: f32 = 0.0,
};

////////////////////////////////////////////////////////////////

pub const CalcTextSize = struct {
    hide_text_after_double_hash: bool = false,
    wrap_width: f32 = -1.0,
};

////////////////////////////////////////////////////////////////

pub const Checkbox = struct {
    v: *bool,
};

////////////////////////////////////////////////////////////////

pub const DockNodeFlags = packed struct(u32) {
    none: bool = false,
    keep_alive_only: bool = false,
    no_docking_in_central_node: bool = false,
    passthru_central_node: bool = false,
    no_split: bool = false,
    no_resize: bool = false,
    auto_hide_tab_bar: bool = false,

    _padding: u25 = 0,
};

pub const DockSpaceOverViewport = struct {
    viewport: ?*imgui.ImGuiViewport = null,
    flags: DockNodeFlags = DockNodeFlags{},
    window_class: ?*anyopaque = null,
};

////////////////////////////////////////////////////////////////

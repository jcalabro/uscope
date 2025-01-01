const c = @import("c.zig");

pub fn initWithGlSlVersion(
    window: *anyopaque, // zglfw.Window
    glsl_version: ?[:0]const u8, // e.g. "#version 130"
) void {
    if (!c.ImGui_ImplGlfw_InitForOpenGL(@ptrCast(window), true)) {
        unreachable;
    }

    _ = c.ImGui_ImplOpenGL3_Init(@ptrCast(glsl_version));
}

pub fn init(
    window: *const anyopaque, // zglfw.Window
) void {
    initWithGlSlVersion(window, null);
}

pub fn deinit() void {
    c.ImGui_ImplGlfw_Shutdown();
    c.ImGui_ImplOpenGL3_Shutdown();
}

pub fn newFrame(fb_width: u32, fb_height: u32) void {
    c.ImGui_ImplGlfw_NewFrame();
    c.ImGui_ImplOpenGL3_NewFrame();

    const io: *c.ImGuiIO = c.igGetIO();

    io.DisplaySize.x = @floatFromInt(fb_width);
    io.DisplaySize.y = @floatFromInt(fb_height);

    io.DisplayFramebufferScale.x = 1.0;
    io.DisplayFramebufferScale.y = 1.0;

    c.igNewFrame();
}

pub fn draw() void {
    c.igRender();
    c.ImGui_ImplOpenGL3_RenderDrawData(c.igGetDrawData());
}

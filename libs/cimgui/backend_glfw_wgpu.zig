const c = @import("c.zig");

// This call will install GLFW callbacks to handle GUI interactions.
// Those callbacks will chain-call user's previously installed callbacks, if any.
// This means that custom user's callbacks need to be installed *before* calling zgpu.gui.init().
pub fn init(
    window: ?*anyopaque, // zglfw.Window
    wgpu_device: ?*anyopaque, // wgpu.Device
    wgpu_swap_chain_format: u32, // wgpu.TextureFormat
) void {
    if (!c.ImGui_ImplGlfw_InitForOther(@ptrCast(window), true)) {
        unreachable;
    }

    if (!c.ImGui_ImplWGPU_Init(
        @ptrCast(wgpu_device),
        1, // num_frames_in_flight
        wgpu_swap_chain_format, // rt_format
        0, // depth_format
    )) {
        unreachable;
    }
}

pub fn deinit() void {
    c.ImGui_ImplWGPU_Shutdown();
    c.ImGui_ImplGlfw_Shutdown();
}

pub fn newFrame(fb_width: u32, fb_height: u32) void {
    c.ImGui_ImplWGPU_NewFrame();
    c.ImGui_ImplGlfw_NewFrame();

    const io: *c.ImGuiIO = c.igGetIO();

    io.DisplaySize.x = @floatFromInt(fb_width);
    io.DisplaySize.y = @floatFromInt(fb_height);

    io.DisplayFramebufferScale.x = 1.0;
    io.DisplayFramebufferScale.y = 1.0;

    c.igNewFrame();
}

pub fn draw(wgpu_render_pass: *anyopaque) void {
    c.igRender();
    c.ImGui_ImplWGPU_RenderDrawData(c.igGetDrawData(), @ptrCast(wgpu_render_pass));
}

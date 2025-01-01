const c = @cImport({
    @cDefine("CIMGUI_DEFINE_ENUMS_AND_STRUCTS", "1");
    @cInclude("cimgui.h");
    @cInclude("imgui_impl_glfw.h");
    @cInclude("imgui_impl_opengl3.h");
});

// Export all of the C API
pub usingnamespace c;

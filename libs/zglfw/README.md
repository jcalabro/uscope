# [zglfw](https://github.com/zig-gamedev/zglfw)

Zig build package and bindings for [GLFW 3.4](https://github.com/glfw/glfw/releases/tag/3.4)

## Getting started

Example `build.zig`:
```zig
pub fn build(b: *std.Build) void {
    const exe = b.addExecutable(.{ ... });

    const zglfw = b.dependency("zglfw", .{});
    exe.root_module.addImport("zglfw", zglfw.module("root"));

    if (target.result.os.tag != .emscripten) {
        exe.linkLibrary(zglfw.artifact("glfw"));
    }
}
```

Now in your code you may import and use `zglfw`:
```zig
const glfw = @import("zglfw");

pub fn main() !void {
    try glfw.init();
    defer glfw.terminate();

    const window = try glfw.createWindow(600, 600, "zig-gamedev: minimal_glfw_gl", null);
    defer glfw.destroyWindow(window);

    // or, using the equivilent, encapsulated, "objecty" API:
    const window = try glfw.Window.create(600, 600, "zig-gamedev: minimal_glfw_gl", null);
    defer window.destroy();

    // setup your graphics context here

    while (!window.shouldClose()) {
        glfw.pollEvents();

        // render your things here
        
        window.swapBuffers();
    }
}
```

See [zig-gamedev samples](https://github.com/zig-gamedev/zig-gamedev/tree/main/samples) for more complete usage examples.

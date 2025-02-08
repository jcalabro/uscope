# [zig-gamedev system_sdk](https://github.com/zig-gamedev/system_sdk)

System libraries and headers for cross-compiling [zig-gamedev](https://github.com/zig-gamedev) libs and sample applications.

## Usage
build.zig
```zig
    switch (target.os.tag) {
        .windows => {
            if (target.cpu.arch.isX86()) {
                if (target.abi.isGnu() or target.abi.isMusl()) {
                    if (b.lazyDependency("system_sdk", .{})) |system_sdk| {
                        compile_step.addLibraryPath(system_sdk.path("windows/lib/x86_64-windows-gnu"));
                    }
                }
            }
        },
        .macos => {
            if (b.lazyDependency("system_sdk", .{})) |system_sdk| {
                compile_step.addLibraryPath(system_sdk.path("macos12/usr/lib"));
                compile_step.addFrameworkPath(system_sdk.path("macos12/System/Library/Frameworks"));
            }
        },
        .linux => {
            if (target.cpu.arch.isX86()) {
                if (b.lazyDependency("system_sdk", .{})) |system_sdk| {
                    compile_step.addLibraryPath(system_sdk.path("linux/lib/x86_64-linux-gnu"));
                }
            } else if (target.cpu.arch == .aarch64) {
                if (b.lazyDependency("system_sdk", .{})) |system_sdk| {
                    compile_step.addLibraryPath(system_sdk.path("linux/lib/aarch64-linux-gnu"));
                }
            }
        },
        else => {},
    }
```

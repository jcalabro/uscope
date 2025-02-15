const std = @import("std");
const Allocator = std.mem.Allocator;

const types = @import("../types.zig");
const String = @import("../strings.zig").String;

/// Request can be any one of the various debugger commands sent from the
/// client to the server. A Request may or may not have a corresponding Response.
pub const Request = union(enum) {
    get_state: GetStateRequest,
    load_symbols: LoadSymbolsRequest,
    toggle_breakpoint: ToggleBreakpointRequest,
    update_breakpoint: UpdateBreakpointRequest,
    launch: LaunchSubordinateRequest,
    kill: KillSubordinateRequest,
    cont: ContinueRequest,
    step: StepRequest,
    stopped: SubordinateStoppedRequest,
    quit: QuitRequest,
    set_hex_window_address: SetHexWindowAddressRequest,
    set_watch_expressions: SetWatchExpressionsRequest,
    thread_spawned: ThreadSpawnedRequest,
};

/// Response can be any one of the various debugger command sent from the
/// server to the client. A Response may or may not have a corresponding Request.
pub const Response = union(enum) {
    message: MessageResponse,
    get_state: GetStateResponse,
    state_updated: StateUpdatedResponse,
    reset: ResetResponse,
    received_text_output: ReceivedTextOutputResponse,
    load_symbols: LoadSymbolsResponse,
};

/// GetState is a request gives the UI all the data it needs to render each frame
pub const GetStateRequest = struct {
    alloc: Allocator,

    pub fn req(self: @This()) Request {
        return .{ .get_state = self };
    }
};

/// Indicates the severity of a message to be sent from the debugger to the UI
pub const MessageLevel = enum(u8) {
    debug,
    info,
    warning,
    @"error",
};

/// A status message sent from the debugger layer to the UI
pub const MessageResponse = struct {
    /// The log level of the message
    level: MessageLevel,

    /// All allocated memory lives in the response queue's allocator
    message: String,

    pub fn resp(self: @This()) Response {
        return Response{ .message = self };
    }
};

/// Contains all the data required to render a single UI frame
pub const GetStateResponse = struct {
    /// All allocated memory lives in the corresponding GetStateRequest's allocator
    state: types.StateSnapshot,

    pub fn resp(self: @This()) Response {
        return Response{ .get_state = self };
    }
};

/// StateUpdatedResponse indicates to the GUI thread that the Debugger thread has new data for it,
/// and the GUI should enqueue a GetStateRequest.
///
/// @NOTE (jrc): This is a bit of an odd data flow, and is an artifact of wanting to keep the GUI
/// arena allocator totally separate from the Debugger allocator. This is probably not the best way
/// of doing things, esp. once we support remote debugging.
pub const StateUpdatedResponse = struct {
    pub fn resp(self: @This()) Response {
        return .{ .state_updated = self };
    }
};

/// LoadSymbols is a request to load debug symbols from the binary at
/// the given relative or absolute path from disk
pub const LoadSymbolsRequest = struct {
    path: String,

    pub fn req(self: @This()) Request {
        return .{ .load_symbols = self };
    }
};

/// Launch starts the child process if it is not already running
pub const LaunchSubordinateRequest = struct {
    path: String,
    args: String,
    stop_on_entry: bool,

    pub fn req(self: @This()) Request {
        return Request{ .launch = self };
    }
};

/// Force-kills the subordinate if it is running
pub const KillSubordinateRequest = struct {
    pub fn req(self: @This()) Request {
        return Request{ .kill = self };
    }
};

/// Instructs the subordinate to continue execution if it was paused
pub const ContinueRequest = struct {
    pub fn req(self: @This()) Request {
        return Request{ .cont = self };
    }
};

/// Instructs the subordinate to step to a subsequent location if it is paused
pub const StepRequest = struct {
    step_type: StepType,

    pub fn req(self: @This()) Request {
        return Request{ .step = self };
    }
};

/// Defines the type of step the user wishes to perform
pub const StepType = enum(u8) {
    single,
    into,
    out_of,
    over,
};

/// Quit shuts down the application
pub const QuitRequest = struct {
    pub fn req(self: @This()) Request {
        return .{ .quit = self };
    }
};

/// Indicates which breakpoint should be added or removed
pub const UpdateBreakpointLocation = union(enum) {
    bid: types.BID,
    addr: types.Address,
    source: types.SourceLocation,
};

/// Activates or deactivates a breakpoint without removing it
pub const ToggleBreakpointRequest = struct {
    id: types.BID,

    pub fn req(self: @This()) Request {
        return Request{ .toggle_breakpoint = self };
    }
};

/// Adds or removes a breakpoint
pub const UpdateBreakpointRequest = struct {
    loc: UpdateBreakpointLocation,

    pub fn req(self: @This()) Request {
        return Request{ .update_breakpoint = self };
    }
};

/// Sent from the Server to the Client when the subordinate writes to
/// stdout/stderr so it may be displayed to the user
pub const ReceivedTextOutputResponse = struct {
    /// Memory is owned by the response queue's allocator.
    text: String,

    pub fn resp(self: @This()) Response {
        return Response{ .received_text_output = self };
    }
};

/// Indicates that the subordinate has paused execution
pub const SubordinateStoppedRequest = struct {
    /// The PID of the subordinate process/thread that was stopped
    pid: types.PID,

    /// Whether or not the subordinate process exited
    exited: bool,

    /// Sometimes, the subordinate received signals that stop the process but
    /// should not stop the debugger (i.e. on Linux, window resize signals)
    should_stop_debugger: bool = true,

    pub fn req(self: @This()) Request {
        return Request{ .stopped = self };
    }
};

/// Reset informs the GUI that the subordinate has been reset
pub const ResetResponse = struct {
    pub fn resp(self: @This()) Response {
        return .{ .reset = self };
    }
};

/// Informs the GUI that the symbols from the subordinate have been loaded and parsed
pub const LoadSymbolsResponse = struct {
    pub fn resp(self: @This()) Response {
        return .{ .load_symbols = self };
    }

    err: ?anyerror = null,
};

/// Sets the address in the hex window display to the given value
// @TODO (jrc): allow displaying multiple hex windows
pub const SetHexWindowAddressRequest = struct {
    address: types.Address,

    pub fn req(self: @This()) Request {
        return .{ .set_hex_window_address = self };
    }
};

/// Sets the watch expresssions to be calculated and displayed in the watch window
pub const SetWatchExpressionsRequest = struct {
    /// Memory is owned by the debugger thread
    expressions: []String,

    pub fn req(self: @This()) Request {
        return .{ .set_watch_expressions = self };
    }

    pub fn deinit(self: @This(), alloc: Allocator) void {
        for (self.expressions) |e| alloc.free(e);
        alloc.free(self.expressions);
    }
};

/// Informs the debugger that the subordinate process has spawned a new thread
pub const ThreadSpawnedRequest = struct {
    pid: types.PID,

    pub fn req(self: @This()) Request {
        return .{ .thread_spawned = self };
    }
};

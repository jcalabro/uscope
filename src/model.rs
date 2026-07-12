use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{Error, Result};

macro_rules! address_type {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            /// Creates an address from its numeric representation.
            #[must_use]
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            /// Returns the numeric representation of this address.
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }
        }
    };
}

address_type!(
    ImageAddress,
    "An address in the address space described by a module image."
);
address_type!(
    VirtualAddress,
    "An address in the virtual address space of a running process."
);

/// A half-open address range `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddressRange<A> {
    /// The first address included in the range.
    pub start: A,
    /// The first address after the range.
    pub end: A,
}

impl<A: Copy + Ord> AddressRange<A> {
    /// Returns whether the address is contained in this range.
    #[must_use]
    pub fn contains(self, address: A) -> bool {
        self.start <= address && address < self.end
    }
}

macro_rules! id_type {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u32);

        impl $name {
            pub(crate) const fn new(value: u32) -> Self {
                Self(value)
            }
        }
    };
}

id_type!(
    ModuleImageId,
    "Identifies a module image within a debug session."
);
id_type!(
    ModuleId,
    "Identifies a loaded module within a debug session."
);
id_type!(FunctionId, "Identifies a function within a module image.");
id_type!(
    SourceFileId,
    "Identifies a source file within a module image."
);
id_type!(
    SymbolId,
    "Identifies a linker symbol within a module image."
);
id_type!(
    StackFrameId,
    "Identifies a stack frame within one stop revision."
);
id_type!(
    RegisterId,
    "Identifies a register within a target architecture."
);

/// Identifies a thread within a debug session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ThreadId(u64);

impl ThreadId {
    /// Creates a thread identifier from its platform value.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the platform value of this identifier.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// The architecture-independent purpose of a distinguished register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RegisterRole {
    /// The address of the current instruction.
    ProgramCounter,
    /// The address of the top of the current stack.
    StackPointer,
    /// The base address conventionally used for the current stack frame.
    FramePointer,
}

/// Static information about a target register.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterDescriptor {
    /// The register's identifier within its target architecture.
    pub id: RegisterId,
    /// The canonical architecture-defined register name.
    pub name: Arc<str>,
    /// The number of meaningful bits in the register.
    pub bits: u16,
    /// The register's architecture-independent role, when distinguished.
    pub role: Option<RegisterRole>,
}

/// One target register and its captured value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterValue {
    /// The register represented by this value.
    pub register: RegisterDescriptor,
    /// The register bytes in the target's byte order.
    pub bytes: Arc<[u8]>,
}

/// The general register set of a stopped thread at one debugger revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterSnapshot {
    /// The debugger revision at which these values were read.
    pub revision: u64,
    /// The thread whose registers were read.
    pub thread: ThreadId,
    /// The architecture and data representation of the register values.
    pub target: TargetDescription,
    /// Register values in the architecture's canonical display order.
    pub registers: Arc<[RegisterValue]>,
}

/// The target CPU architecture described by a module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Architecture {
    /// The x86-64 architecture.
    X86_64,
    /// The 64-bit Arm architecture.
    Aarch64,
}

/// The byte order used by the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteOrder {
    /// Least-significant byte first.
    Little,
    /// Most-significant byte first.
    Big,
}

/// The width of an address on the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerWidth {
    /// A 32-bit address.
    Bits32,
    /// A 64-bit address.
    Bits64,
}

/// Platform-independent properties of a debug target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetDescription {
    /// The target CPU architecture.
    pub architecture: Architecture,
    /// The target byte order.
    pub byte_order: ByteOrder,
    /// The width of target addresses.
    pub pointer_width: PointerWidth,
}

/// A one-based source line number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LineNumber(u64);

impl LineNumber {
    /// Creates a line number, returning `None` for zero.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    /// Returns the numeric line number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A one-based source column number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ColumnNumber(u64);

impl ColumnNumber {
    /// Creates a column number, returning `None` for zero.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    /// Returns the numeric column number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A source file referenced by debug information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    /// The file's session-scoped identifier.
    pub id: SourceFileId,
    /// The source path resolved from the debug metadata.
    pub path: Arc<PathBuf>,
}

/// A location in a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLocation {
    /// The source file containing the location.
    pub file: SourceFileId,
    /// The one-based source line.
    pub line: LineNumber,
    /// The one-based column, when present in the debug information.
    pub column: Option<ColumnNumber>,
}

/// One numbered line read from a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLine {
    /// The one-based line number.
    pub number: LineNumber,
    /// The source text without its line terminator.
    pub text: Arc<str>,
}

/// Source lines surrounding an execution location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceContext {
    /// The source file that was read.
    pub file: SourceFile,
    /// The execution location within the file.
    pub location: SourceLocation,
    /// Contiguous source lines ordered by line number.
    pub lines: Arc<[SourceLine]>,
}

/// Static information about a function in a module image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionInfo {
    /// The function's session-scoped identifier.
    pub id: FunctionId,
    /// The source-level function name.
    pub name: Arc<str>,
    /// The linker-visible function name, when known.
    pub linkage_name: Option<Arc<str>>,
    /// The image-address ranges occupied by the function.
    pub ranges: Arc<[AddressRange<ImageAddress>]>,
    /// The function's declaration location, when known.
    pub declaration: Option<SourceLocation>,
}

/// A linker symbol exported by a module image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolInfo {
    /// The symbol's session-scoped identifier.
    pub id: SymbolId,
    /// The linker-visible symbol name.
    pub name: Arc<str>,
    /// The symbol's image address.
    pub address: ImageAddress,
}

impl FunctionInfo {
    /// Returns whether the function contains an image address.
    #[must_use]
    pub fn contains(&self, address: ImageAddress) -> bool {
        self.ranges.iter().any(|range| range.contains(address))
    }
}

/// A resolved source and function location in a module image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageLocation {
    /// The address that was resolved.
    pub address: ImageAddress,
    /// The containing function, when known.
    pub function: Option<FunctionInfo>,
    /// The corresponding source location, when known.
    pub source: Option<SourceLocation>,
}

/// The address space in which a breakpoint was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakpointLocation {
    /// A location relative to an immutable module image.
    Image(ImageAddress),
    /// An absolute location in a running process.
    Virtual(VirtualAddress),
}

/// A resolved execution location in a running process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionLocation {
    /// The loaded module containing the address.
    pub module: ModuleId,
    /// The process virtual address.
    pub address: VirtualAddress,
    /// Static metadata resolved from the corresponding module image.
    pub image: ImageLocation,
}

/// Describes how a stack frame was reconstructed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// A normal machine-code activation.
    Physical,
    /// A signal trampoline activation.
    Signal,
}

/// A platform-independent stack frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackFrame {
    /// The frame's identifier within the current stop revision.
    pub id: StackFrameId,
    /// Zero-based position, beginning with the stopped frame.
    pub level: u32,
    /// How the frame was reconstructed.
    pub kind: FrameKind,
    /// The loaded module containing the instruction, when known.
    pub module: Option<ModuleId>,
    /// The exact instruction or resume address for the frame.
    pub instruction: VirtualAddress,
    /// The containing function, when known.
    pub function: Option<FunctionInfo>,
    /// The corresponding source location, when known.
    pub source: Option<SourceLocation>,
}

impl StackFrame {
    pub(crate) fn new(
        level: u32,
        kind: FrameKind,
        module: Option<ModuleId>,
        instruction: VirtualAddress,
        location: Option<ImageLocation>,
    ) -> Self {
        let (function, source) = location.map_or((None, None), |location| {
            (location.function, location.source)
        });

        Self {
            id: StackFrameId::new(level),
            level,
            kind,
            module,
            instruction,
            function,
            source,
        }
    }
}

/// Explains why a backtrace stopped growing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnwindTermination {
    /// The unwind metadata declared that no caller exists.
    Complete,
    /// No unwind information covered the supplied instruction.
    NoUnwindInfo { address: VirtualAddress },
    /// The instruction could not be associated with a loaded module.
    ModuleNotFound { address: VirtualAddress },
    /// Valid metadata used a feature not implemented by this debugger.
    UnsupportedUnwindInfo { feature: Arc<str> },
    /// The unwind metadata was malformed.
    CorruptUnwindInfo { description: Arc<str> },
    /// A register required to reconstruct the caller was unavailable.
    RegisterUnavailable { register: Arc<str> },
    /// Inferior memory required by an unwind rule could not be read.
    MemoryReadFailed { address: VirtualAddress },
    /// The reconstructed caller did not make valid progress.
    InvalidCaller { description: Arc<str> },
    /// A previously visited frame state was encountered again.
    CycleDetected,
    /// The configured maximum frame count was reached.
    DepthLimit,
}

/// A backtrace and the reason its reconstruction ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backtrace {
    /// The thread whose stack was inspected.
    pub thread: ThreadId,
    /// Frames ordered from the stopped frame outward.
    pub frames: Arc<[StackFrame]>,
    /// The completion or failure reason for the trace.
    pub termination: UnwindTermination,
}

/// An internal image-address range associated with a source location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineEntry {
    pub range: AddressRange<ImageAddress>,
    pub location: SourceLocation,
}

/// Immutable, normalized debug metadata for one executable module.
#[derive(Debug)]
pub struct ModuleImage {
    id: ModuleImageId,
    path: Arc<PathBuf>,
    target: TargetDescription,
    address_range: AddressRange<ImageAddress>,
    functions: Arc<[FunctionInfo]>,
    symbols: Arc<[SymbolInfo]>,
    source_files: Arc<[SourceFile]>,
    lines: Arc<[LineEntry]>,
}

impl ModuleImage {
    pub(crate) fn new(
        path: PathBuf,
        target: TargetDescription,
        address_range: AddressRange<ImageAddress>,
        functions: Vec<FunctionInfo>,
        symbols: Vec<SymbolInfo>,
        source_files: Vec<SourceFile>,
        lines: Vec<LineEntry>,
    ) -> Self {
        Self {
            id: ModuleImageId::new(0),
            path: Arc::new(path),
            target,
            address_range,
            functions: functions.into(),
            symbols: symbols.into(),
            source_files: source_files.into(),
            lines: lines.into(),
        }
    }

    /// Returns this image's session-scoped identifier.
    #[must_use]
    pub const fn id(&self) -> ModuleImageId {
        self.id
    }

    /// Returns the executable path used to load this image.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the properties of this image's target.
    #[must_use]
    pub const fn target(&self) -> TargetDescription {
        self.target
    }

    /// Returns whether an image address lies in this module's loadable range.
    #[must_use]
    pub fn contains_address(&self, address: ImageAddress) -> bool {
        self.address_range.contains(address)
    }

    /// Returns all functions described by this image.
    #[must_use]
    pub fn functions(&self) -> &[FunctionInfo] {
        &self.functions
    }

    /// Returns all linker symbols described by this image.
    #[must_use]
    pub fn symbols(&self) -> &[SymbolInfo] {
        &self.symbols
    }

    /// Returns all source files referenced by this image.
    #[must_use]
    pub fn source_files(&self) -> &[SourceFile] {
        &self.source_files
    }

    pub(crate) fn line_entries(&self) -> &[LineEntry] {
        &self.lines
    }

    /// Finds the single function with the supplied source-level name.
    pub fn function_named(&self, name: &str) -> Result<&FunctionInfo> {
        let mut matches = self
            .functions
            .iter()
            .filter(|function| function.name.as_ref() == name);
        let function = matches
            .next()
            .ok_or_else(|| Error::FunctionNotFound(name.to_owned()))?;

        if matches.next().is_some() {
            return Err(Error::DuplicateFunction(name.to_owned()));
        }

        Ok(function)
    }

    /// Finds the single linker symbol with the supplied name.
    pub fn symbol_named(&self, name: &str) -> Result<&SymbolInfo> {
        let mut matches = self
            .symbols
            .iter()
            .filter(|symbol| symbol.name.as_ref() == name);
        let symbol = matches
            .next()
            .ok_or_else(|| Error::SymbolNotFound(name.to_owned()))?;

        if matches.next().is_some() {
            return Err(Error::DuplicateSymbol(name.to_owned()));
        }

        Ok(symbol)
    }

    /// Resolves an image address to its available function and source metadata.
    #[must_use]
    pub fn locate(&self, address: ImageAddress) -> ImageLocation {
        let function = self
            .functions
            .iter()
            .find(|function| function.contains(address))
            .cloned();
        let source = self
            .lines
            .iter()
            .find(|entry| entry.range.contains(address))
            .map(|entry| entry.location.clone());

        ImageLocation {
            address,
            function,
            source,
        }
    }

    /// Looks up a source file by its identifier.
    #[must_use]
    pub fn source_file(&self, id: SourceFileId) -> Option<&SourceFile> {
        self.source_files.iter().find(|file| file.id == id)
    }
}

/// A module image mapped into a running process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadedModule {
    /// The loaded module's session-scoped identifier.
    pub id: ModuleId,
    /// The corresponding immutable module image.
    pub image: ModuleImageId,
    /// The load bias applied to image addresses.
    pub load_bias: u64,
}

impl LoadedModule {
    pub(crate) const fn main(image: ModuleImageId, load_bias: u64) -> Self {
        Self {
            id: ModuleId::new(0),
            image,
            load_bias,
        }
    }

    /// Converts an image address into a process virtual address.
    pub fn virtual_address(self, address: ImageAddress) -> Result<VirtualAddress> {
        self.load_bias
            .checked_add(address.get())
            .map(VirtualAddress::new)
            .ok_or(Error::AddressOverflow)
    }

    /// Converts a process virtual address into an image address.
    pub fn image_address(self, address: VirtualAddress) -> Result<ImageAddress> {
        address
            .get()
            .checked_sub(self.load_bias)
            .map(ImageAddress::new)
            .ok_or(Error::AddressOutsideModule)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loaded_module_translates_between_address_spaces_with_checked_arithmetic() {
        let module = LoadedModule::main(ModuleImageId::new(0), 0x4000);

        assert_eq!(
            module.virtual_address(ImageAddress::new(0x123)).unwrap(),
            VirtualAddress::new(0x4123)
        );
        assert_eq!(
            module.image_address(VirtualAddress::new(0x4123)).unwrap(),
            ImageAddress::new(0x123)
        );
        assert!(matches!(
            module.image_address(VirtualAddress::new(0x3fff)),
            Err(Error::AddressOutsideModule)
        ));
        assert!(matches!(
            module.virtual_address(ImageAddress::new(u64::MAX)),
            Err(Error::AddressOverflow)
        ));
    }
}

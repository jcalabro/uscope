//! The hostile view harness (`plans/views.md` §3.14): every built-in view,
//! and views made of arbitrary text, run over arbitrary memory. Whatever
//! the bytes, a presentation ends in a value or a typed problem within its
//! budget, never panics, and allocates only what its limits allow.

use super::run::{Child, Failure, children, element_place, length, present};
use super::{ViewSet, choose};
use crate::eval::fake::{Place, World};
use crate::{BaseTypeEncoding as E, IntegerValue, SourceLanguage, TypeArgument, TypeReference};

/// Where the harness maps the heap the containers may point into.
const HEAP: u64 = 0x10_0000;
const HEAP_SIZE: usize = 4096;

/// The most units of evaluation work one run may take.
const MAX_WORK: u64 = 65_536;

/// The fuzzer's bytes for memory, read round and round, or zeros when
/// there are none, so a short input still makes a whole world.
struct Input<'a> {
    data: &'a [u8],
    position: usize,
}

impl Input<'_> {
    const fn byte(&mut self) -> u8 {
        let byte = if self.data.is_empty() {
            0
        } else {
            self.data[self.position % self.data.len()]
        };
        self.position = self.position.wrapping_add(1);
        byte
    }

    fn word(&mut self) -> u64 {
        u64::from_le_bytes(std::array::from_fn(|_| self.byte()))
    }

    /// A word a container holds: often a pointer into the heap or a small
    /// count, so that views get past their checks, and otherwise anything.
    fn field(&mut self) -> u64 {
        match self.byte() % 4 {
            0 => HEAP + self.word() % HEAP_SIZE as u64,
            1 => u64::from(self.byte() % 32),
            2 => 0,
            _ => self.word(),
        }
    }

    fn object(&mut self, words: usize) -> Vec<u8> {
        (0..words)
            .flat_map(|_| self.field().to_le_bytes())
            .collect()
    }
}

/// A world of the containers the built-in views present, laid out as their
/// libraries lay them out, each a variable whose bytes come from `input`.
#[expect(clippy::too_many_lines, reason = "one type graph per library")]
fn containers(input: &mut Input<'_>) -> (World, Vec<(String, TypeReference)>) {
    let mut world = World::new();
    let int = world.base("int", E::Signed, 4);
    let char = world.base("char", E::SignedCharacter, 1);
    let uchar = world.base("unsigned char", E::UnsignedCharacter, 1);
    let size = world.base("unsigned long", E::Unsigned, 8);
    let i32 = world.base("i32", E::Signed, 4);
    let u8 = world.base("u8", E::Unsigned, 1);
    let usize = world.base("usize", E::Unsigned, 8);
    let int_pointer = world.pointer(Some(int));
    let char_pointer = world.pointer(Some(char));
    let byte_pointer = world.pointer(Some(u8));
    let unknown = |text: &str| TypeArgument::Unknown(text.into());
    let mut variables = Vec::new();

    // libstdc++
    let implementation = world.record(
        "_Vector_impl",
        24,
        &[
            ("_M_start", int_pointer, 0),
            ("_M_finish", int_pointer, 8),
            ("_M_end_of_storage", int_pointer, 16),
        ],
    );
    let vector = world.record("vector<int>", 24, &[("_M_impl", implementation, 0)]);
    world.identify(
        vector,
        SourceLanguage::Cpp,
        &["std"],
        "vector",
        vec![TypeArgument::Type(int), unknown("allocator")],
    );
    variables.push(("libstdc++ vector".to_owned(), vector));
    let hider = world.record("_Alloc_hider", 8, &[("_M_p", char_pointer, 0)]);
    let buffer = world.array(char, &[16]);
    let string = world.record(
        "string",
        32,
        &[
            ("_M_dataplus", hider, 0),
            ("_M_string_length", size, 8),
            ("_M_local_buf", buffer, 16),
            ("_M_allocated_capacity", size, 16),
        ],
    );
    world.identify(
        string,
        SourceLanguage::Cpp,
        &["std"],
        "basic_string",
        vec![
            TypeArgument::Type(char),
            unknown("traits"),
            unknown("allocator"),
        ],
    );
    variables.push(("libstdc++ string".to_owned(), string));
    let old_string = world.record("string", 8, &[("_M_dataplus", hider, 0)]);
    world.identify(
        old_string,
        SourceLanguage::Cpp,
        &["std"],
        "basic_string",
        vec![
            TypeArgument::Type(char),
            unknown("traits"),
            unknown("allocator"),
        ],
    );
    variables.push(("libstdc++ copy-on-write string".to_owned(), old_string));
    let elements = world.array(int, &[4]);
    let array = world.record("array<int, 4>", 16, &[("_M_elems", elements, 0)]);
    world.identify(
        array,
        SourceLanguage::Cpp,
        &["std"],
        "array",
        vec![
            TypeArgument::Type(int),
            TypeArgument::Value(IntegerValue::Unsigned(4)),
        ],
    );
    variables.push(("std::array".to_owned(), array));
    let extent = world.record("__extent_storage", 8, &[("_M_extent_value", size, 0)]);
    let span = world.record(
        "span<int>",
        16,
        &[("_M_ptr", int_pointer, 0), ("_M_extent", extent, 8)],
    );
    world.identify(
        span,
        SourceLanguage::Cpp,
        &["std"],
        "span",
        vec![
            TypeArgument::Type(int),
            TypeArgument::Value(IntegerValue::Unsigned(u128::from(u64::MAX))),
        ],
    );
    variables.push(("libstdc++ span".to_owned(), span));

    // libc++, whose bit-fields this world lays out as whole bytes.
    let short = world.array(char, &[23]);
    let short_string = world.record(
        "__short",
        24,
        &[
            ("__is_long_", uchar, 0),
            ("__size_", uchar, 0),
            ("__data_", short, 1),
        ],
    );
    let long_string = world.record(
        "__long",
        24,
        &[
            ("__is_long_", uchar, 0),
            ("__cap_", size, 0),
            ("__size_", size, 8),
            ("__data_", char_pointer, 16),
        ],
    );
    let representation = world.record(
        "__rep",
        24,
        &[("__s", short_string, 0), ("__l", long_string, 0)],
    );
    let libcxx_string = world.record("string", 24, &[("__rep_", representation, 0)]);
    world.identify(
        libcxx_string,
        SourceLanguage::Cpp,
        &["std"],
        "basic_string",
        vec![
            TypeArgument::Type(char),
            unknown("traits"),
            unknown("allocator"),
        ],
    );
    variables.push(("libc++ string".to_owned(), libcxx_string));
    let libcxx_vector = world.record(
        "vector<int>",
        24,
        &[
            ("__begin_", int_pointer, 0),
            ("__end_", int_pointer, 8),
            ("__cap_", int_pointer, 16),
        ],
    );
    world.identify(
        libcxx_vector,
        SourceLanguage::Cpp,
        &["std"],
        "vector",
        vec![TypeArgument::Type(int), unknown("allocator")],
    );
    variables.push(("libc++ vector".to_owned(), libcxx_vector));

    // Rust
    let marker = world.record("PhantomData", 0, &[]);
    let global = world.record("Global", 0, &[]);
    let non_null = world.record("NonNull<u8>", 8, &[("pointer", byte_pointer, 0)]);
    let unique = world.record(
        "Unique<u8>",
        8,
        &[("pointer", non_null, 0), ("_marker", marker, 0)],
    );
    let cap = world.record("Cap", 8, &[("__0", usize, 0)]);
    let raw_inner = world.record(
        "RawVecInner",
        16,
        &[("ptr", unique, 0), ("cap", cap, 8), ("alloc", global, 0)],
    );
    let raw = world.record(
        "RawVec",
        16,
        &[("inner", raw_inner, 0), ("_marker", marker, 0)],
    );
    let rust_vector = |world: &mut World, element| {
        let vector = world.record("Vec", 24, &[("buf", raw, 0), ("len", usize, 16)]);
        world.identify(
            vector,
            SourceLanguage::Rust,
            &["alloc", "vec"],
            "Vec",
            vec![TypeArgument::Type(element), TypeArgument::Type(global)],
        );
        vector
    };
    let ints = rust_vector(&mut world, i32);
    variables.push(("Vec<i32>".to_owned(), ints));
    let bytes = rust_vector(&mut world, u8);
    let rust_string = world.record("String", 24, &[("vec", bytes, 0)]);
    world.identify(
        rust_string,
        SourceLanguage::Rust,
        &["alloc", "string"],
        "String",
        Vec::new(),
    );
    variables.push(("String".to_owned(), rust_string));
    let deque = world.record(
        "VecDeque<i32>",
        32,
        &[("head", usize, 0), ("len", usize, 8), ("buf", raw, 16)],
    );
    world.identify(
        deque,
        SourceLanguage::Rust,
        &["alloc", "collections", "vec_deque"],
        "VecDeque",
        vec![TypeArgument::Type(i32), TypeArgument::Type(global)],
    );
    variables.push(("VecDeque<i32>".to_owned(), deque));

    // Zig
    let items = world.slice(i32);
    let list = world.record(
        "array_list.Aligned(i32,null)",
        24,
        &[("items", items, 0), ("capacity", usize, 16)],
    );
    world.identify(
        list,
        SourceLanguage::Zig,
        &["array_list"],
        "Aligned",
        vec![TypeArgument::Type(i32), unknown("null")],
    );
    variables.push(("ArrayList(i32)".to_owned(), list));

    let heap = (0..HEAP_SIZE).map(|_| input.byte()).collect::<Vec<_>>();
    world.map(HEAP, &heap);
    for (name, ty) in &variables {
        let words = usize::try_from(world_size(&world, *ty).div_ceil(8)).unwrap_or(0);
        let object = input.object(words.max(1));
        world.variable(name, *ty, &object);
    }
    (world, variables)
}

fn world_size(world: &World, ty: TypeReference) -> u64 {
    use crate::eval::types::TypeSource as _;
    world
        .type_info(ty)
        .and_then(|info| info.byte_size)
        .unwrap_or(8)
}

/// Runs every view that binds each container over memory from `data`.
pub fn hostile(data: &[u8]) {
    // A budget, the length of a view file of the input's own, which is
    // tried first, the file, and then memory.
    let (&budget, data) = data.split_first().unwrap_or((&0, &[]));
    let (&text_length, data) = data.split_first().unwrap_or((&0, &[]));
    let (text, memory) = data.split_at((usize::from(text_length) * 4).min(data.len()));
    let mut input = Input {
        data: memory,
        position: 0,
    };
    let work = u64::from(budget) * 256 + 16;
    let (mut world, variables) = containers(&mut input);
    let text = String::from_utf8_lossy(text).into_owned();
    let text = if text.starts_with("uscope-views") {
        text
    } else {
        format!("uscope-views 1\n{text}")
    };
    let fuzzed = ViewSet::new([("fuzz.views", text.as_str())]);
    let built_in = ViewSet::built_in();
    for (name, ty) in &variables {
        let Some(bound) = choose(&fuzzed, *ty, &world)
            .bound
            .or_else(|| choose(&built_in, *ty, &world).bound)
        else {
            continue;
        };
        let this = Place::Memory {
            address: world.address_of(name),
            ty: *ty,
        };
        world.work = Some(work.min(MAX_WORK));
        match present(&bound, &mut world, this.clone()) {
            Ok(presented) => {
                if let Some(text) = &presented.text {
                    assert!(text.bytes.len() <= crate::TextSummary::MAX_BYTES, "{name}");
                }
                assert!(
                    presented.summary.len() <= 64 * 1024,
                    "{name}: {}",
                    presented.summary.len()
                );
            }
            Err(Failure::Problem(_)) => {}
            Err(Failure::Debugger(error)) => panic!("{name}: {error}"),
        }
        world.work = Some(work.min(MAX_WORK));
        let offset = u64::from(input.byte()) * 16;
        let limit = u64::from(input.byte());
        if let Ok(page) = children(&bound, &mut world, this.clone(), offset, limit) {
            assert!(
                page.len() as u64 <= limit,
                "{name}: {} of {limit}",
                page.len()
            );
            for child in &page {
                if let Child::Element(index, _) = child {
                    assert!(*index >= offset && *index < offset + limit, "{name}");
                }
            }
        }
        world.work = Some(work.min(MAX_WORK));
        let _ = length(&bound, &mut world, this.clone());
        world.work = Some(work.min(MAX_WORK));
        let _ = element_place(&bound, &mut world, this, i128::from(input.byte()) - 8);
    }
}

/// Which built-in view presents each container of the harness's world, so a
/// change to a view or to the harness that leaves a view unexercised fails.
#[cfg(test)]
pub fn built_in_choices() -> Vec<(String, Option<String>)> {
    let mut input = Input {
        data: &[],
        position: 0,
    };
    let (world, variables) = containers(&mut input);
    let views = ViewSet::built_in();
    variables
        .into_iter()
        .map(|(name, ty)| {
            let choice = choose(&views, ty, &world);
            (
                name,
                choice.bound.map(|bound| bound.view.header.to_string()),
            )
        })
        .collect()
}

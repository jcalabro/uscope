use super::run::{Child, Failure, Presented, children, present};
use super::scan::Checkpoints;
use super::syntax::{self, Language};
use super::{ViewSet, choose};
use crate::eval::fake::{Place, World};
use crate::{
    BaseTypeEncoding as E, IntegerValue, PresentedShape, SourceLanguage, TextCompletion,
    TypeArgument, TypeReference, VariableState, VariableValue,
};

/// A world of hand-rolled C containers and a C++ template instance.
struct Containers {
    world: World,
    intvec: TypeReference,
    text: TypeReference,
}

fn bytes(values: &[u64]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn ints(values: impl IntoIterator<Item = i32>) -> Vec<u8> {
    values.into_iter().flat_map(i32::to_le_bytes).collect()
}

fn containers() -> Containers {
    let mut world = World::new();
    let int = world.base("int", E::Signed, 4);
    let size = world.base("unsigned long", E::Unsigned, 8);
    let char = world.base("char", E::SignedCharacter, 1);
    let int_pointer = world.pointer(Some(int));
    let char_pointer = world.pointer(Some(char));
    let intvec = world.record(
        "intvec",
        24,
        &[("data", int_pointer, 0), ("n", size, 8), ("cap", size, 16)],
    );
    world.identify(intvec, SourceLanguage::C, &[], "intvec", Vec::new());
    let text = world.record("str_t", 16, &[("p", char_pointer, 0), ("len", size, 8)]);
    world.identify(text, SourceLanguage::C, &[], "str_t", Vec::new());

    let elements = world.allocate(&ints([10, 20, 30, 0]));
    world.variable("v", intvec, &bytes(&[elements, 3, 4]));
    world.variable("bad", intvec, &bytes(&[elements, 9, 4]));
    world.variable("none", intvec, &bytes(&[0, 0, 0]));
    let many = world.allocate(&ints(0..300));
    world.variable("big", intvec, &bytes(&[many, 300, 300]));
    world.variable("dangling", intvec, &bytes(&[0xdead_0000, 2, 2]));

    // Text without a length is read a page at a time.
    let mut page = b"hello\0".to_vec();
    page.resize(4096, b'?');
    world.map(0x9_0000, &page);
    world.variable("s", text, &bytes(&[0x9_0000, 5]));
    // Text that runs to the end of the last mapped page.
    world.map(0x7_fffd, b"abc");
    world.variable("cut", text, &bytes(&[0x7_fffd, 300]));
    Containers {
        world,
        intvec,
        text,
    }
}

const VIEWS: &str = "uscope-views 1
view c intvec {
    check n <= cap
    show sequence(n) for i in range(n) => data[i]
    field capacity = cap
}
view c str_t {
    show text(p, len)
}
";

fn set(text: &str) -> ViewSet {
    let set = ViewSet::new([("test.views", text)]);
    assert!(set.errors().is_empty(), "{:?}", set.errors());
    set
}

fn place(world: &World, name: &str, ty: TypeReference) -> Place {
    Place::Memory {
        address: world.address_of(name),
        ty,
    }
}

fn presented(
    world: &mut World,
    views: &ViewSet,
    name: &str,
    ty: TypeReference,
) -> Result<Presented, String> {
    let choice = choose(views, ty, world);
    let Some(bound) = choice.bound else {
        return Err(format!("no view binds: {:?}", choice.candidates));
    };
    let this = place(world, name, ty);
    present(&bound, world, this, &mut Checkpoints::default()).map_err(|failure| match failure {
        Failure::Problem(problem) => problem.to_string(),
        Failure::Debugger(error) => format!("debugger: {error}"),
    })
}

fn summary(world: &mut World, views: &ViewSet, name: &str, ty: TypeReference) -> String {
    presented(world, views, name, ty).map_or_else(
        |problem| format!("problem: {problem}"),
        |presented| presented.summary,
    )
}

#[test]
fn a_sequence_view_presents_elements_count_and_fields() {
    let Containers {
        mut world, intvec, ..
    } = containers();
    let views = set(VIEWS);
    let shown = presented(&mut world, &views, "v", intvec).expect("presents");
    assert_eq!(shown.shape, PresentedShape::Sequence);
    assert_eq!(shown.count, Some(crate::PresentedCount::Exact(3)));
    assert_eq!(shown.summary, "len=3 [10, 20, 30]");
    assert_eq!(summary(&mut world, &views, "none", intvec), "len=0 []");
    assert_eq!(
        summary(&mut world, &views, "big", intvec),
        "len=300 [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, …]"
    );

    let bound = choose(&views, intvec, &world).bound.expect("binds");
    let this = place(&world, "v", intvec);
    let page = children(
        &bound,
        &mut world,
        this,
        3,
        0,
        10,
        &mut Checkpoints::default(),
    )
    .expect("children");
    let rendered = page
        .iter()
        .map(|child| match child {
            Child::Element(index, value) => format!(
                "[{index}] = {}",
                super::summary::value(value.type_info.as_ref(), &value.state)
            ),
            Child::Field(name, value) => format!(
                "{name} = {}",
                super::summary::value(value.type_info.as_ref(), &value.state)
            ),
            Child::Entry(..) => panic!("a sequence has no entries"),
            Child::Raw => "[raw]".to_owned(),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        rendered,
        ["[0] = 10", "[1] = 20", "[2] = 30", "capacity = 4", "[raw]"]
    );
}

/// Child `k` costs one evaluation of the element, wherever it is.
#[test]
fn random_access_children_cost_the_same_at_any_index() {
    let Containers {
        mut world, intvec, ..
    } = containers();
    let views = set(VIEWS);
    let bound = choose(&views, intvec, &world).bound.expect("binds");
    let mut costs = Vec::new();
    for index in [0, 150, 299] {
        world.reads.clear();
        let this = place(&world, "big", intvec);
        let page = children(
            &bound,
            &mut world,
            this,
            300,
            index,
            1,
            &mut Checkpoints::default(),
        )
        .expect("children");
        assert!(matches!(&page[..], [Child::Element(found, _)] if *found == index));
        costs.push(world.reads.len());
    }
    assert!(costs.windows(2).all(|pair| pair[0] == pair[1]), "{costs:?}");
    // The check's two reads, the count's and the range's, and the element's
    // pointer and value.
    assert_eq!(costs[0], 6, "{costs:?}");
}

/// A count as large as a word holds, as garbage often is, pages without
/// overflowing: its last children are still its fields and `[raw]`.
#[test]
fn the_largest_counts_page_without_overflowing() {
    let Containers {
        mut world, intvec, ..
    } = containers();
    world.variable("huge", intvec, &bytes(&[0x10, u64::MAX, u64::MAX]));
    let views = set(VIEWS);
    let bound = choose(&views, intvec, &world).bound.expect("binds");
    let this = place(&world, "huge", intvec);
    let page = children(
        &bound,
        &mut world,
        this,
        u64::MAX,
        u64::MAX - 1,
        4,
        &mut Checkpoints::default(),
    )
    .expect("children");
    assert!(
        matches!(&page[..], [Child::Element(index, _)] if *index == u64::MAX - 1),
        "{page:?}"
    );
}

#[test]
fn a_failed_check_is_a_problem_naming_its_sides() {
    let Containers {
        mut world, intvec, ..
    } = containers();
    let views = set(VIEWS);
    assert_eq!(
        summary(&mut world, &views, "bad", intvec),
        "problem: check `n <= cap` failed: `n` is 9, `cap` is 4"
    );
    // An element the program cannot provide is said so, and ends the
    // preview: never a plausible number.
    assert_eq!(
        summary(&mut world, &views, "dangling", intvec),
        "len=2 [<unavailable>, …]"
    );
}

#[test]
fn text_reads_its_length_or_to_its_terminator_and_says_how_it_ended() {
    let Containers {
        mut world, text, ..
    } = containers();
    let views = set(VIEWS);
    let shown = presented(&mut world, &views, "s", text).expect("presents");
    assert_eq!(shown.shape, PresentedShape::Text);
    assert_eq!(shown.summary, "\"hello\"");
    let cut = presented(&mut world, &views, "cut", text).expect("presents");
    let read = cut.text.expect("text");
    assert_eq!(&*read.bytes, b"abc");
    assert!(
        matches!(read.completion, TextCompletion::Unreadable { address } if address.get() == 0x8_0000),
        "{read:?}"
    );
    assert_eq!(cut.summary, "\"abc\"... <unreadable at 0x80000>");

    let terminated = set("uscope-views 1\nview c str_t { show text(p) }\n");
    assert_eq!(summary(&mut world, &terminated, "s", text), "\"hello\"");
}

#[test]
fn an_alternative_that_does_not_bind_falls_through_and_says_why() {
    let Containers {
        mut world, intvec, ..
    } = containers();
    let views = set("uscope-views 1
view c intvec {
    let size = length
    show sequence(size) for i in range(size) => data[i]
}
view c intvec {
    let size = length or n
    show sequence(size) for i in range(size) => data[i]
}
");
    let choice = choose(&views, intvec, &world);
    assert!(choice.bound.is_some());
    let [rejected, chosen] = choice.candidates.as_slice() else {
        panic!("two candidates: {:?}", choice.candidates);
    };
    assert_eq!(rejected.name.line, 2);
    let reason = rejected.rejection.as_ref().expect("rejected").to_string();
    assert!(
        reason.contains("`length` is neither a member of `intvec` nor a name the view declares"),
        "{reason}"
    );
    assert_eq!(chosen.name.line, 6);
    assert!(chosen.rejection.is_none());
    assert_eq!(
        summary(&mut world, &views, "v", intvec),
        "len=3 [10, 20, 30]"
    );
}

#[test]
fn shapes_choose_branches_and_present_other_values() {
    let mut world = World::new();
    let int = world.base("int", E::Signed, 4);
    let marker = world.record("PhantomData", 0, &[]);
    let pointer = world.pointer(Some(int));
    let boxed = world.record("Box", 8, &[("_marker", marker, 0), ("ptr", pointer, 0)]);
    world.identify(
        boxed,
        SourceLanguage::Rust,
        &["alloc", "boxed"],
        "Box",
        vec![TypeArgument::Type(int)],
    );
    let tagged = world.record("tagged", 8, &[("kind", int, 0), ("value", int, 4)]);
    world.identify(tagged, SourceLanguage::C, &[], "tagged", Vec::new());
    let target = world.allocate(&ints([42]));
    world.variable("b", boxed, &bytes(&[target]));
    world.variable("nothing", tagged, &ints([0, 0]));
    world.variable("something", tagged, &ints([1, 7]));
    let views = set("uscope-views 1
view rust alloc::**::Box<T> {
    show value(*inner(self))
}
view c tagged {
    show if kind == 0 { empty(\"None\") } else { value(value) }
    summary \"tagged {kind}: {value}\"
}
");
    let shown = presented(&mut world, &views, "b", boxed).expect("presents");
    assert_eq!(shown.shape, PresentedShape::Value);
    assert_eq!(shown.summary, "42");
    let inner = shown.inner.expect("the value standing for it");
    assert!(matches!(
        inner.state,
        VariableState::Available {
            value: VariableValue::Scalar(crate::ScalarValue::Signed(42)),
            ..
        }
    ));
    let none = presented(&mut world, &views, "nothing", tagged).expect("presents");
    assert_eq!(
        (none.shape, none.summary.as_str()),
        (PresentedShape::Empty, "tagged 0: 0")
    );
    let some = presented(&mut world, &views, "something", tagged).expect("presents");
    assert_eq!(
        (some.shape, some.summary.as_str()),
        (PresentedShape::Value, "tagged 1: 7")
    );
}

#[test]
fn patterns_capture_arguments_and_anchor_at_the_root() {
    let mut world = World::new();
    let int = world.base("int", E::Signed, 4);
    let array = world.array(int, &[3]);
    let pair = world.record(
        "Pair<int, 3>",
        16,
        &[("first", int, 0), ("items", array, 4)],
    );
    world.identify(
        pair,
        SourceLanguage::Cpp,
        &["app", "detail"],
        "Pair",
        vec![
            TypeArgument::Type(int),
            TypeArgument::Value(IntegerValue::Unsigned(3)),
        ],
    );
    world.variable("p", pair, &ints([1, 2, 3, 4]));
    let shown = |world: &mut World, header: &str| {
        // A pattern that names the count gives the body none to use.
        let count = if header.contains("N>") { "N" } else { "3" };
        let views = ViewSet::new([(
            "test.views",
            format!(
                "uscope-views 1\nview {header} {{\n    show sequence({count}) for i in range({count}) => *((T*)&items[0] + i)\n}}\n"
            )
            .as_str(),
        )]);
        assert!(views.errors().is_empty(), "{:?}", views.errors());
        summary(world, &views, "p", pair)
    };
    assert_eq!(
        shown(&mut world, "c++ app::detail::Pair<T, N>"),
        "len=3 [2, 3, 4]"
    );
    assert_eq!(
        shown(&mut world, "c++ app::**::Pair<T, N>"),
        "len=3 [2, 3, 4]"
    );
    assert_eq!(shown(&mut world, "any **::Pair<T, 3>"), "len=3 [2, 3, 4]");
    for header in [
        "c++ detail::Pair<T, N>",
        "c app::detail::Pair<T, N>",
        "c++ app::detail::Pair<T, 4>",
        "c++ app::detail::Pair<T, N, M>",
        "c++ app::detail::Pair<long, N>",
    ] {
        assert!(
            shown(&mut world, header).starts_with("problem: no view binds"),
            "{header}"
        );
    }
}

/// However little budget there is, a presentation is the whole one or a
/// problem, never a different one.
#[test]
fn too_little_work_ends_in_a_problem_never_a_wrong_presentation() {
    let views = set(VIEWS);
    let Containers {
        mut world, intvec, ..
    } = containers();
    world.work = Some(100_000);
    let full = summary(&mut world, &views, "v", intvec);
    let used = 100_000 - world.work.expect("limited");
    assert_eq!(full, "len=3 [10, 20, 30]");
    for budget in 0..used {
        let Containers {
            mut world, intvec, ..
        } = containers();
        world.work = Some(budget);
        let limited = summary(&mut world, &views, "v", intvec);
        assert!(
            limited == full || limited.contains("limit") || limited.contains("<unavailable>"),
            "{budget} of {used} units gave `{limited}`"
        );
    }
}

#[test]
fn the_built_in_views_parse() {
    let views = ViewSet::built_in();
    assert!(views.errors().is_empty(), "{:?}", views.errors());
    assert!(views.views().len() >= 16, "{}", views.views().len());
}

fn errors(text: &str) -> Vec<String> {
    syntax::parse("bad.views", text)
        .errors
        .iter()
        .map(ToString::to_string)
        .collect()
}

#[test]
fn view_file_errors_point_at_their_line_and_column_and_skip_one_view() {
    assert_eq!(
        errors("view c x { show empty(\"\") }"),
        ["bad.views:1:1: a view file begins with `uscope-views 1`"]
    );
    assert_eq!(
        errors("uscope-views 2\n"),
        ["bad.views:1:1: this uscope reads views of version 1"]
    );
    let file = syntax::parse(
        "bad.views",
        "uscope-views 1
view c first {
    let x = n +
    show empty(\"\")
}
view c second {
    show empty(\"ok\")
}
view c third {
    let x = 1
}
view c fourth {
    show text(p)
    show text(p)
}
view c fifth {
    hide x
    show empty(\"\")
}
view c sixth {
    show sequence(n) for i in list(head) => i
}
view kotlin seventh {
    show empty(\"\")
}
",
    );
    let messages = file
        .errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    assert_eq!(file.views.len(), 1, "{messages:?}");
    assert_eq!(file.views[0].pattern.base, "second");
    assert_eq!(file.views[0].language, Language::C);
    assert_eq!(messages.len(), 6, "{messages:?}");
    assert!(
        messages[0].starts_with("bad.views:3:16: "),
        "{}",
        messages[0]
    );
    assert!(
        messages[1].starts_with("bad.views:9:1: a view needs a `show`"),
        "{}",
        messages[1]
    );
    assert!(
        messages[2].starts_with("bad.views:14:5: a view shows its value once"),
        "{}",
        messages[2]
    );
    assert!(
        messages[3].contains("`hide` is not supported yet"),
        "{}",
        messages[3]
    );
    assert!(
        messages[4].contains("expected `,` before the next argument"),
        "{}",
        messages[4]
    );
    assert!(
        messages[5].contains("expected a language"),
        "{}",
        messages[5]
    );
}

/// Recovering from an error never splits a character, wherever it is.
#[test]
fn recovery_from_an_error_steps_over_whole_characters() {
    let file = syntax::parse(
        "bad.views",
        "uscope-views 1\n\u{2603}é view c x {\nview c second {\n    show empty(\"ok\")\n}\n",
    );
    assert_eq!(file.views.len(), 1, "{:?}", file.errors);
    assert_eq!(file.views[0].pattern.base, "second");
}

#[test]
fn expressions_end_where_their_statement_or_shape_does() {
    let file = syntax::parse(
        "ok.views",
        "uscope-views 1 # the version
view c++ app::Thing<T, _> {   # a comment
    let begin = first
        or `or`.second       # an alternative on the next line
    check begin <= end &&
        end <= limit
    show if len(x) == 0 { empty(\"empty\") }
         else { sequence(end - begin) for i in range(end - begin)
                => begin[i] }
    field total = (a
        + b)
    summary \"{size} \\{braces\\}\"
}
",
    );
    assert!(file.errors.is_empty(), "{:?}", file.errors);
    let statements = &file.views[0].statements;
    let syntax::Statement::Let { alternatives, .. } = &statements[0] else {
        panic!("a let");
    };
    let texts = alternatives
        .iter()
        .map(syntax::Expr::text)
        .collect::<Vec<_>>();
    assert_eq!(texts, ["first", "`or`.second"]);
    let syntax::Statement::Check(check) = &statements[1] else {
        panic!("a check");
    };
    assert_eq!(check.expression.to_string(), "begin <= end && end <= limit");
    let syntax::Statement::Show(syntax::Shape::If { otherwise, .. }) = &statements[2] else {
        panic!("an if");
    };
    let syntax::Shape::Sequence {
        element, clauses, ..
    } = otherwise.as_ref()
    else {
        panic!("a sequence");
    };
    let [
        syntax::Clause {
            generator: syntax::Generator::Range(length),
            ..
        },
    ] = clauses.as_slice()
    else {
        panic!("one range: {clauses:?}");
    };
    assert_eq!((length.text(), element.text()), ("end - begin", "begin[i]"));
    let syntax::Statement::Summary(pieces) = &statements[4] else {
        panic!("a summary");
    };
    assert!(
        matches!(&pieces[..], [syntax::Piece::Hole(_), syntax::Piece::Literal(text)] if text == " {braces}")
    );
}

#[test]
fn bound_views_reject_what_does_not_type_check() {
    let Containers { world, intvec, .. } = containers();
    for (body, reason) in [
        (
            "show sequence(n) for i in range(n) => data[i].x",
            "`data[i]` has no members",
        ),
        (
            "show text(n)",
            "`text` takes a pointer to one-byte characters",
        ),
        (
            "show sequence(data) for i in range(n) => i",
            "is not an integer",
        ),
        ("check n\n    show empty(\"\")", ""),
        (
            "type T = int*\n    show empty(\"\")",
            "write the pointer where it is used",
        ),
        ("type T = typeof(n)\n    show empty(\"\")", ""),
        ("show value(self.missing)", "has no member"),
    ] {
        let views = set(&format!(
            "uscope-views 1\nview c intvec {{\n    {body}\n}}\n"
        ));
        let choice = choose(&views, intvec, &world);
        match (&choice.bound, reason) {
            (Some(_), "") => {}
            (Some(_), _) => panic!("`{body}` bound"),
            (None, _) => {
                let rejection = choice.candidates[0].rejection.as_ref().expect("rejected");
                assert!(
                    rejection.to_string().contains(reason),
                    "`{body}`: {rejection}"
                );
            }
        }
    }
}

/// Views are pure, as the evaluator is: they reach a program only through
/// the evaluator's traits, so none of their code may reach for process
/// control, debug information, I/O, clocks, or threads. Tests are exempt.
#[test]
fn views_stay_pure() {
    const FORBIDDEN: [&str; 11] = [
        "crate::backend",
        "crate::debug_info",
        "crate::sim",
        "nix::",
        "gimli",
        "tokio",
        "std::fs",
        "std::env",
        "std::process",
        "std::thread",
        "std::time",
    ];
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/view");
    let mut checked = 0;
    for entry in std::fs::read_dir(&root).expect("read the views' sources") {
        let path = entry.expect("a directory entry").path();
        if path.file_name().is_some_and(|name| name == "tests.rs")
            || path.extension().is_none_or(|extension| extension != "rs")
        {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("read a source file");
        let source = source.split("#[cfg(test)]").next().unwrap_or_default();
        for forbidden in FORBIDDEN {
            assert!(
                !source.contains(forbidden),
                "{} uses `{forbidden}`",
                path.display()
            );
        }
        checked += 1;
    }
    assert!(checked >= 6, "the views' sources were found");
}

/// The hostile harness's containers are each presented by the built-in
/// view meant for them, so the fuzz target exercises every library's views.
#[test]
fn the_hostile_harness_exercises_every_library() {
    let choices = super::fuzz::built_in_choices();
    let unbound = choices
        .iter()
        .filter(|(_, view)| view.is_none())
        .collect::<Vec<_>>();
    assert!(unbound.is_empty(), "{unbound:?}");
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]

    /// Whatever memory holds, and whatever views a file says, presenting
    /// ends in a value or a typed problem within its budget.
    #[test]
    fn views_over_arbitrary_memory_end_in_values_or_problems(
        data in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..6000),
    ) {
        super::fuzz::hostile(&data);
    }
}

/// The world `docs/views.md` describes: the containers, a tagged union, a
/// Rust box, and a C++ template instance.
fn examples() -> World {
    let Containers { mut world, .. } = containers();
    let int = world.base("int", E::Signed, 4);
    let tagged = world.record("tagged", 8, &[("kind", int, 0), ("value", int, 4)]);
    world.identify(tagged, SourceLanguage::C, &[], "tagged", Vec::new());
    world.variable("nothing", tagged, &ints([0, 0]));
    world.variable("something", tagged, &ints([1, 7]));
    let marker = world.record("PhantomData", 0, &[]);
    let pointer = world.pointer(Some(int));
    let boxed = world.record(
        "Box<i32>",
        8,
        &[("_marker", marker, 0), ("ptr", pointer, 0)],
    );
    world.identify(
        boxed,
        SourceLanguage::Rust,
        &["alloc", "boxed"],
        "Box",
        vec![TypeArgument::Type(int)],
    );
    let target = world.allocate(&ints([42]));
    world.variable("b", boxed, &bytes(&[target]));
    let array = world.array(int, &[3]);
    let pair = world.record(
        "Pair<int, 3>",
        16,
        &[("first", int, 0), ("items", array, 4)],
    );
    world.identify(
        pair,
        SourceLanguage::Cpp,
        &["app", "detail"],
        "Pair",
        vec![
            TypeArgument::Type(int),
            TypeArgument::Value(IntegerValue::Unsigned(3)),
        ],
    );
    world.variable("p", pair, &ints([1, 2, 3, 4]));
    add_linked(&mut world);
    // `app::Handle<int>` holds a `void *` to an `app::Cell<int>`.
    let cell = world.record("Cell<int>", 4, &[("value", int, 0)]);
    world.identify(
        cell,
        SourceLanguage::Cpp,
        &["app"],
        "Cell",
        vec![TypeArgument::Type(int)],
    );
    let void = world.pointer(None);
    let handle = world.record("Handle<int>", 8, &[("cell", void, 0)]);
    world.identify(
        handle,
        SourceLanguage::Cpp,
        &["app"],
        "Handle",
        vec![TypeArgument::Type(int)],
    );
    let stored = world.allocate(&ints([5]));
    world.variable("handle", handle, &bytes(&[stored]));
    world
}

/// What a value shows as an example writes it.
fn example_outcome(views: &ViewSet, name: &str, outcome: &str) -> String {
    let mut world = examples();
    let ty = world.type_of(name);
    let choice = choose(views, ty, &world);
    let Some(bound) = choice.bound else {
        return choice.candidates.first().map_or_else(
            || "unbound: no view's pattern names the type".to_owned(),
            |candidate| {
                format!(
                    "unbound: {}",
                    candidate
                        .rejection
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_default()
                )
            },
        );
    };
    let this = place(&world, name, ty);
    let mut checkpoints = Checkpoints::default();
    if outcome.starts_with("children: ") {
        let elements = match present(&bound, &mut world, this.clone(), &mut checkpoints) {
            Ok(presented) => presented.count.map_or(0, crate::PresentedCount::known),
            Err(Failure::Problem(problem)) => return format!("problem: {problem}"),
            Err(Failure::Debugger(error)) => return format!("debugger: {error}"),
        };
        return match children(&bound, &mut world, this, elements, 0, 64, &mut checkpoints) {
            Ok(children) => {
                let rendered = children
                    .iter()
                    .map(|child| match child {
                        Child::Element(index, value) => format!(
                            "[{index}] = {}",
                            super::summary::value(value.type_info.as_ref(), &value.state)
                        ),
                        Child::Entry(_, key, value) => format!(
                            "{}: {}",
                            super::summary::value(key.type_info.as_ref(), &key.state),
                            super::summary::value(value.type_info.as_ref(), &value.state)
                        ),
                        Child::Field(name, value) => format!(
                            "{name} = {}",
                            super::summary::value(value.type_info.as_ref(), &value.state)
                        ),
                        Child::Raw => "[raw]".to_owned(),
                    })
                    .collect::<Vec<_>>();
                format!("children: {}", rendered.join(", "))
            }
            Err(Failure::Problem(problem)) => format!("problem: {problem}"),
            Err(Failure::Debugger(error)) => format!("debugger: {error}"),
        };
    }
    match present(&bound, &mut world, this, &mut checkpoints) {
        Ok(presented) => presented.summary,
        Err(Failure::Problem(problem)) => format!("problem: {problem}"),
        Err(Failure::Debugger(error)) => format!("debugger: {error}"),
    }
}

/// Every example in the views reference, `docs/views.md`, holds, so the
/// reference and the implementation cannot drift apart.
#[test]
fn every_example_in_the_views_reference_holds() {
    const REFERENCE: &str = include_str!("../../docs/views.md");
    let mut failures = Vec::new();
    let mut blocks = 0;
    let mut rows = 0;
    let mut lines = REFERENCE.lines().enumerate();
    while let Some((_, line)) = lines.next() {
        if line.trim() != "```uscope-view-example" {
            continue;
        }
        blocks += 1;
        let mut file = String::new();
        let mut examples = Vec::new();
        let mut in_rows = false;
        for (index, line) in lines.by_ref() {
            match line.trim() {
                "```" => break,
                "---" => in_rows = true,
                row if in_rows && !row.is_empty() => examples.push((index + 1, row.to_owned())),
                _ if !in_rows => {
                    file.push_str(line);
                    file.push('\n');
                }
                _ => {}
            }
        }
        let parsed = syntax::parse("example.views", &file);
        let views = ViewSet::new([("example.views", file.as_str())]);
        for (line, row) in examples {
            rows += 1;
            let (name, expected) = row
                .split_once(" => ")
                .unwrap_or_else(|| panic!("docs/views.md:{line}: `{row}` has no ` => `"));
            let actual = if name == "file" {
                let errors = parsed
                    .errors
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>();
                let wanted = expected.strip_prefix("error: ").unwrap_or(expected);
                match errors.iter().find(|error| error.contains(wanted)) {
                    Some(_) => expected.to_owned(),
                    None => format!("errors {errors:?}"),
                }
            } else {
                let actual = example_outcome(&views, name, expected);
                match expected.strip_prefix("unbound: ") {
                    Some(reason) if actual.contains(reason) => expected.to_owned(),
                    _ => actual,
                }
            };
            if actual != expected {
                failures.push(format!(
                    "docs/views.md:{line}: `{name}`\n    expected {expected}\n    actual   {actual}"
                ));
            }
        }
    }
    assert!(
        blocks >= 8 && rows >= 15,
        "the reference's examples were found"
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A world of hand-rolled C linked structures: lists of `node {int value;
/// node *next}`, binary trees of `tnode {int key; int value; tnode *left;
/// tnode *right}`, an open-addressed table, and chained buckets.
struct Linked {
    world: World,
    list: TypeReference,
    tree: TypeReference,
    table: TypeReference,
    chained: TypeReference,
}

/// Lays out list nodes holding `values`, each linked to the node `links`
/// names (`None` for null), and returns their addresses.
fn nodes(world: &mut World, values: &[i32], links: &[Option<usize>]) -> Vec<u64> {
    let base = world.allocate(&vec![0; values.len() * 16]);
    let address = |index: usize| base + 16 * index as u64;
    let mut bytes = Vec::new();
    for (value, link) in values.iter().zip(links) {
        bytes.extend_from_slice(&i64::from(*value).to_le_bytes());
        bytes.extend_from_slice(&link.map_or(0, address).to_le_bytes());
    }
    world.map(base, &bytes);
    (0..values.len()).map(address).collect()
}

/// A chain of nodes holding `values`, the last linked to null.
fn chain(world: &mut World, values: &[i32]) -> Vec<u64> {
    let links = (1..=values.len())
        .map(|next| (next < values.len()).then_some(next))
        .collect::<Vec<_>>();
    nodes(world, values, &links)
}

/// Tree nodes `(key, value, left, right)`, children by index.
fn tree_nodes(world: &mut World, nodes: &[(i32, i32, Option<usize>, Option<usize>)]) -> Vec<u64> {
    let base = world.allocate(&vec![0; nodes.len() * 24]);
    let address = |index: usize| base + 24 * index as u64;
    let mut bytes = Vec::new();
    for (key, value, left, right) in nodes {
        bytes.extend_from_slice(&ints([*key, *value]));
        bytes.extend_from_slice(&left.map_or(0, address).to_le_bytes());
        bytes.extend_from_slice(&right.map_or(0, address).to_le_bytes());
    }
    world.map(base, &bytes);
    (0..nodes.len()).map(address).collect()
}

fn linked() -> Linked {
    let mut world = World::new();
    let (list, tree, table, chained) = add_linked(&mut world);
    Linked {
        world,
        list,
        tree,
        table,
        chained,
    }
}

/// Adds the linked structures' types and values to `world`, returning the
/// list, tree, table, and chained types.
fn add_linked(world: &mut World) -> (TypeReference, TypeReference, TypeReference, TypeReference) {
    let int = world.base("int", E::Signed, 4);
    let size = world.base("unsigned long", E::Unsigned, 8);
    let node = world.record("node", 16, &[]);
    let node_pointer = world.pointer(Some(node));
    world.set_members(node, &[("value", int, 0), ("next", node_pointer, 8)]);
    let list = world.record("list", 16, &[("head", node_pointer, 0), ("count", size, 8)]);
    world.identify(list, SourceLanguage::C, &[], "list", Vec::new());
    let tnode = world.record("tnode", 24, &[]);
    let tnode_pointer = world.pointer(Some(tnode));
    world.set_members(
        tnode,
        &[
            ("key", int, 0),
            ("value", int, 4),
            ("left", tnode_pointer, 8),
            ("right", tnode_pointer, 16),
        ],
    );
    let tree = world.record(
        "tree",
        16,
        &[("root", tnode_pointer, 0), ("count", size, 8)],
    );
    world.identify(tree, SourceLanguage::C, &[], "tree", Vec::new());
    let slot = world.record(
        "slot",
        12,
        &[("used", int, 0), ("key", int, 4), ("value", int, 8)],
    );
    let slot_pointer = world.pointer(Some(slot));
    let table = world.record(
        "table",
        24,
        &[
            ("slots", slot_pointer, 0),
            ("cap", size, 8),
            ("n", size, 16),
        ],
    );
    world.identify(table, SourceLanguage::C, &[], "table", Vec::new());
    let buckets = world.pointer(Some(node_pointer));
    let chained = world.record(
        "chained",
        24,
        &[
            ("buckets", buckets, 0),
            ("nbuckets", size, 8),
            ("n", size, 16),
        ],
    );
    world.identify(chained, SourceLanguage::C, &[], "chained", Vec::new());

    // 1 -> 2 -> 3 -> null
    let counted = chain(world, &[1, 2, 3]);
    world.variable("three", list, &bytes(&[counted[0], 3]));
    world.variable("empty_list", list, &bytes(&[0, 0]));
    // A circular list, whose last node leads back to its first.
    let circle = nodes(world, &[4, 5, 6], &[Some(1), Some(2), Some(0)]);
    world.variable("circle", list, &bytes(&[circle[0], 3]));
    // 7 -> 8 -> 9 -> 8 -> …: a cycle that does not pass the head.
    let looped = nodes(world, &[7, 8, 9], &[Some(1), Some(2), Some(1)]);
    world.variable("looped", list, &bytes(&[looped[0], 5]));
    // A list shorter than its count.
    world.variable("short", list, &bytes(&[counted[0], 5]));
    let long = chain(world, &(0..1000).collect::<Vec<_>>());
    world.variable("long", list, &bytes(&[long[0], 1000]));

    //     2
    //    / \
    //   1   3
    let balanced = tree_nodes(
        world,
        &[
            (2, 20, Some(1), Some(2)),
            (1, 10, None, None),
            (3, 30, None, None),
        ],
    );
    world.variable("balanced", tree, &bytes(&[balanced[0], 3]));
    // A tree whose rightmost node leads back to the root.
    let tangled = tree_nodes(
        world,
        &[
            (2, 20, Some(1), Some(2)),
            (1, 10, None, None),
            (3, 30, None, Some(0)),
        ],
    );
    world.variable("tangled", tree, &bytes(&[tangled[0], 5]));
    // A chain of left children deeper than any tree a library builds.
    let deep = (0..200_usize)
        .map(|index| {
            let key = i32::try_from(index).expect("small");
            (key, key, (index + 1 < 200).then_some(index + 1), None)
        })
        .collect::<Vec<_>>();
    let deep = tree_nodes(world, &deep);
    world.variable("deep", tree, &bytes(&[deep[0], 200]));

    // Slots 1 and 3 of 4 are used.
    let mut slots = Vec::new();
    for slot in [[0, 0, 0], [1, 5, 50], [0, 9, 90], [1, 6, 60]] {
        slots.extend_from_slice(&ints(slot));
    }
    let slots = world.allocate(&slots);
    world.variable("sparse", table, &bytes(&[slots, 4, 2]));

    // Bucket 0 holds 1 -> 2, bucket 1 nothing, bucket 2 holds 3.
    let first = chain(world, &[1, 2]);
    let second = chain(world, &[3]);
    let array = world.allocate(&bytes(&[first[0], 0, second[0]]));
    world.variable("buckets", chained, &bytes(&[array, 3, 3]));
    (list, tree, table, chained)
}

const LINKED_VIEWS: &str = "uscope-views 1
view c list {
    show sequence(count) for x in list(head, n => n->next) => x->value
}
view c tree {
    show map(count) for x in inorder(root, n => n->left, n => n->right) => x->key : x->value
}
view c table {
    show map(n) for i in range(cap) if slots[i].used != 0 => slots[i].key : slots[i].value
}
view c chained {
    show sequence(n) for b in range(nbuckets) for x in list(buckets[b], p => p->next) => x->value
}
";

/// Every element or entry of a presented value, in pages of `size`.
fn paged(
    world: &mut World,
    views: &ViewSet,
    name: &str,
    ty: TypeReference,
    size: u64,
) -> Vec<String> {
    let bound = choose(views, ty, world).bound.expect("binds");
    let this = place(world, name, ty);
    let mut checkpoints = Checkpoints::default();
    let presented = present(&bound, world, this.clone(), &mut checkpoints).expect("presents");
    let count = presented.count.map_or(0, crate::PresentedCount::known);
    let mut rendered = Vec::new();
    let mut offset = 0;
    while offset < count {
        let page = children(
            &bound,
            world,
            this.clone(),
            count,
            offset,
            size,
            &mut checkpoints,
        )
        .expect("a page");
        for child in page {
            rendered.push(match child {
                Child::Element(index, value) => format!(
                    "[{index}] {}",
                    super::summary::value(value.type_info.as_ref(), &value.state)
                ),
                Child::Entry(index, key, value) => format!(
                    "[{index}] {}: {}",
                    super::summary::value(key.type_info.as_ref(), &key.state),
                    super::summary::value(value.type_info.as_ref(), &value.state)
                ),
                Child::Field(..) | Child::Raw => continue,
            });
        }
        offset += size;
    }
    rendered
}

#[test]
fn lists_end_at_null_or_their_head_and_never_repeat_a_node() {
    let Linked {
        mut world, list, ..
    } = linked();
    let views = set(LINKED_VIEWS);
    assert_eq!(
        summary(&mut world, &views, "three", list),
        "len=3 [1, 2, 3]"
    );
    assert_eq!(summary(&mut world, &views, "empty_list", list), "len=0 []");
    assert_eq!(
        summary(&mut world, &views, "circle", list),
        "len=3 [4, 5, 6]"
    );
    assert_eq!(
        summary(&mut world, &views, "looped", list),
        "problem: cycle at element 3: it leads back to a node already visited"
    );
    assert_eq!(
        summary(&mut world, &views, "short", list),
        "problem: the view declares 5 elements and generates 3"
    );
    assert_eq!(
        paged(&mut world, &views, "three", list, 2),
        ["[0] 1", "[1] 2", "[2] 3"]
    );
}

#[test]
fn a_list_without_a_count_is_counted_as_far_as_the_budget_allows() {
    let Linked {
        mut world, list, ..
    } = linked();
    let views = set("uscope-views 1
view c list {
    show sequence(_) for x in list(head, n => n->next) => x->value
}
");
    assert_eq!(
        summary(&mut world, &views, "three", list),
        "len=3 [1, 2, 3]"
    );
    assert_eq!(
        summary(&mut world, &views, "long", list),
        "len=1000 [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, …]"
    );
    world.work = Some(400);
    let shown = presented(&mut world, &views, "long", list).expect("presents");
    let Some(crate::PresentedCount::AtLeast(count)) = shown.count else {
        panic!("{:?}", shown.count);
    };
    assert!((16..1000).contains(&count), "{count}");
    assert!(
        shown.summary.starts_with(&format!("len>={count} [0, 1, 2")),
        "{}",
        shown.summary
    );
}

#[test]
fn trees_present_in_order_and_refuse_cycles_and_impossible_depths() {
    let Linked {
        mut world, tree, ..
    } = linked();
    let views = set(LINKED_VIEWS);
    let shown = presented(&mut world, &views, "balanced", tree).expect("presents");
    assert_eq!(shown.shape, PresentedShape::Map);
    assert_eq!(shown.summary, "len=3 {1: 10, 2: 20, 3: 30}");
    assert_eq!(
        paged(&mut world, &views, "balanced", tree, 1),
        ["[0] 1: 10", "[1] 2: 20", "[2] 3: 30"]
    );
    assert_eq!(
        summary(&mut world, &views, "tangled", tree),
        "problem: cycle at element 3: it leads back to a node already visited"
    );
    assert_eq!(
        summary(&mut world, &views, "deep", tree),
        "problem: the tree is deeper than 128 levels"
    );
}

#[test]
fn filters_and_nested_generators_choose_what_is_generated() {
    let Linked {
        mut world,
        table,
        chained,
        ..
    } = linked();
    let views = set(LINKED_VIEWS);
    assert_eq!(
        summary(&mut world, &views, "sparse", table),
        "len=2 {5: 50, 6: 60}"
    );
    assert_eq!(
        summary(&mut world, &views, "buckets", chained),
        "len=3 [1, 2, 3]"
    );
    assert_eq!(
        paged(&mut world, &views, "buckets", chained, 2),
        ["[0] 1", "[1] 2", "[2] 3"]
    );
}

/// A later page resumes from the nearest checkpoint rather than walking
/// from the start again, and pages of any size read the same elements.
#[test]
fn scans_resume_from_checkpoints_and_page_alike_in_any_size() {
    let Linked {
        mut world, list, ..
    } = linked();
    let views = set(LINKED_VIEWS);
    let bound = choose(&views, list, &world).bound.expect("binds");
    let this = place(&world, "long", list);
    let mut fresh = Checkpoints::default();
    world.reads.clear();
    let page =
        children(&bound, &mut world, this.clone(), 1000, 900, 4, &mut fresh).expect("a page");
    assert!(
        matches!(page.first(), Some(Child::Element(900, _))),
        "{page:?}"
    );
    let from_start = world.reads.len();
    world.reads.clear();
    let page = children(&bound, &mut world, this, 1000, 904, 4, &mut fresh).expect("a page");
    assert!(
        matches!(page.first(), Some(Child::Element(904, _))),
        "{page:?}"
    );
    let resumed = world.reads.len();
    assert!(
        resumed * 4 < from_start,
        "{resumed} reads resumed, {from_start} from the start"
    );
    assert_eq!(
        paged(&mut world, &views, "long", list, 7),
        paged(&mut world, &views, "long", list, 300)
    );
}

/// However little budget there is, a scan's presentation is the whole one
/// or a problem, never a different one.
#[test]
fn too_little_work_never_makes_a_scan_present_wrongly() {
    let views = set(LINKED_VIEWS);
    for (name, select) in [("three", 0), ("balanced", 1), ("sparse", 2), ("buckets", 3)] {
        let full = {
            let mut linked = linked();
            let ty = [linked.list, linked.tree, linked.table, linked.chained][select];
            linked.world.work = Some(100_000);
            let full = summary(&mut linked.world, &views, name, ty);
            let used = 100_000 - linked.world.work.expect("limited");
            (full, used)
        };
        for budget in 0..full.1 {
            let mut linked = linked();
            let ty = [linked.list, linked.tree, linked.table, linked.chained][select];
            linked.world.work = Some(budget);
            let limited = summary(&mut linked.world, &views, name, ty);
            assert!(
                limited == full.0 || limited.contains("limit") || limited.contains("<unavailable>"),
                "{name}: {budget} of {} units gave `{limited}`",
                full.1
            );
        }
    }
}

#[test]
fn an_element_a_scan_reaches_is_a_place() {
    let Linked {
        mut world, list, ..
    } = linked();
    let views = set(LINKED_VIEWS);
    let bound = choose(&views, list, &world).bound.expect("binds");
    let this = place(&world, "three", list);
    let mut checkpoints = Checkpoints::default();
    let second = super::run::element_place(&bound, &mut world, this.clone(), 1, &mut checkpoints)
        .expect("a place");
    assert!(
        matches!(
            crate::eval::target::Machine::load(&mut world, &second),
            Ok(VariableValue::Scalar(crate::ScalarValue::Signed(2)))
        ),
        "{second:?}"
    );
    let past = super::run::element_place(&bound, &mut world, this, 3, &mut checkpoints);
    assert!(
        matches!(&past, Err(Failure::Problem(problem)) if problem.to_string().contains("out of bounds") || problem.to_string().contains("index")),
        "{past:?}"
    );
}

/// A `type` with arguments is the one type whose identity it spells, found
/// by identity whatever its name; `offsetof` and `TYPE.Name` reach a
/// record's layout and the types declared inside it.
#[test]
fn types_are_constructed_from_arguments_and_layouts_named() {
    let mut world = World::new();
    let int = world.base("int", E::Signed, 4);
    let long = world.base("long", E::Signed, 8);
    let node = |world: &mut World, element, name: &str, size| {
        let node = world.record(name, size, &[("next", element, 0), ("value", element, 8)]);
        world.identify(
            node,
            SourceLanguage::Cpp,
            &["lib"],
            "node_of",
            vec![TypeArgument::Type(element)],
        );
        node
    };
    let int_node = node(&mut world, int, "node_of<int>", 12);
    let _long_node = node(&mut world, long, "node_of<long int>", 16);
    // A copy of `node_of<int>` from another unit, as DWARF has one per unit.
    let copy = node(&mut world, int, "node_of<int>", 12);
    let holder = world.record("holder<int>", 8, &[("first", int, 0), ("second", int, 4)]);
    world.identify(
        holder,
        SourceLanguage::Cpp,
        &["lib"],
        "holder",
        vec![TypeArgument::Type(int)],
    );
    let header = world.record("holder<int>.Header", 4, &[("size", int, 0)]);
    world.variable("h", holder, &ints([3, 4]));
    let _ = (int_node, copy, header);
    let views = |body: &str| {
        ViewSet::new([(
            "test.views",
            format!("uscope-views 1\nview c++ lib::holder<T> {{\n{body}\n}}\n").as_str(),
        )])
    };
    let shown = |world: &mut World, body: &str| {
        let views = views(body);
        assert!(views.errors().is_empty(), "{:?}", views.errors());
        summary(world, &views, "h", holder)
    };
    assert_eq!(
        shown(
            &mut world,
            "    type Node = lib::node_of<T>\n    show value(sizeof(Node) + offsetof(Node, value))"
        ),
        "20"
    );
    assert_eq!(
        shown(
            &mut world,
            "    type Header = typeof(self).Header\n    show value(sizeof(Header))"
        ),
        "4"
    );
    // Same-named instances of one identity are one type, but different
    // identities are not.
    world.base("char", E::SignedCharacter, 1);
    for (body, reason) in [
        (
            "    type Node = lib::node_of<U>\n    show empty(\"\")",
            "`U` is neither an argument the pattern captured nor a type the view names",
        ),
        (
            "    type Node = lib::node_of<_>\n    show empty(\"\")",
            "names several types",
        ),
        (
            "    type Node = lib::node_of<char>\n    show empty(\"\")",
            "no type is `lib::node_of<char>`",
        ),
        (
            "    type Node = lib::node_of<T>\n    show value(offsetof(Node, third))",
            "has no member `third` of its own",
        ),
        (
            "    type Missing = typeof(self).Missing\n    show empty(\"\")",
            "declares no type `Missing`",
        ),
    ] {
        let views = views(body);
        let choice = choose(&views, holder, &world);
        let rejection = choice
            .candidates
            .first()
            .and_then(|candidate| candidate.rejection.as_ref())
            .unwrap_or_else(|| panic!("`{body}` bound"));
        assert!(
            rejection.to_string().contains(reason),
            "`{body}`: {rejection}"
        );
    }
}

#[test]
fn generators_and_maps_parse_with_their_errors_where_they_are() {
    let file = syntax::parse(
        "ok.views",
        "uscope-views 1
view c a {
    show map(n)
        for x in inorder(root, n => n->left, n => n->right)
            if x->key != 0
        for y in list(x->chain, p => p->next) if y != 0
        => x->flag ? x->key : -1 : std::max::value
}
",
    );
    assert!(file.errors.is_empty(), "{:?}", file.errors);
    let syntax::Statement::Show(syntax::Shape::Map {
        clauses,
        key,
        value,
        ..
    }) = &file.views[0].statements[0]
    else {
        panic!("a map");
    };
    assert_eq!(clauses.len(), 2);
    assert!(matches!(
        clauses[0].items.as_slice(),
        [syntax::Item::Filter(_)]
    ));
    assert_eq!(key.text(), "x->flag ? x->key : -1");
    assert_eq!(value.text(), "std::max::value");
    for (body, error) in [
        (
            "show map(n) for i in range(n) => i",
            "expected `:` between the entry's key and value",
        ),
        (
            "show sequence(n) => i",
            "expected `for` and a generator after the count",
        ),
        (
            "show sequence(n) for i in tree(r) => i",
            "expected a generator",
        ),
        (
            "show sequence(n) for a in range(1) for b in range(1) for c in range(1) for d in range(1) for e in range(1) => a",
            "generators may nest at most 4 deep",
        ),
        (
            "show sequence(n) for x in list(h, => x) => x",
            "expected the node's name",
        ),
    ] {
        let messages = errors(&format!("uscope-views 1\nview c a {{\n    {body}\n}}\n"));
        assert!(
            messages.iter().any(|message| message.contains(error)),
            "`{body}`: {messages:?}"
        );
    }
}

/// A page whose budget runs out while the scan looks for its next element
/// keeps the elements it found: a sparse table's page is never emptied by
/// the empty slots after its last element.
#[test]
fn a_page_keeps_its_elements_when_the_budget_ends_between_them() {
    let Linked {
        mut world, table, ..
    } = linked();
    let mut slots = Vec::new();
    for index in 0..200 {
        let used = i32::from(index % 50 == 0);
        slots.extend_from_slice(&ints([used, index, index * 10]));
    }
    let slots = world.allocate(&slots);
    world.variable("wide", table, &bytes(&[slots, 200, 4]));
    let views = set(LINKED_VIEWS);
    let bound = choose(&views, table, &world).bound.expect("binds");
    let this = place(&world, "wide", table);
    world.work = Some(300);
    let page = children(
        &bound,
        &mut world,
        this,
        4,
        0,
        4,
        &mut Checkpoints::default(),
    );
    let Ok(page) = page else {
        panic!("{page:?}");
    };
    assert!(
        matches!(page.as_slice(), [Child::Entry(0, ..), ..] if page.len() < 4),
        "{page:?}"
    );
    // So is a summary's preview.
    world.work = Some(150);
    assert_eq!(
        summary(&mut world, &views, "wide", table),
        "len=4 {0: 0, <unavailable>, …}"
    );
}

/// A cycle that leads back past the checkpoint a page resumes from, where
/// the nodes visited before it are not remembered, is still found, by
/// Brent's algorithm, before the declared count is reached.
#[test]
fn a_cycle_behind_a_checkpoint_is_still_found() {
    let Linked {
        mut world, list, ..
    } = linked();
    let mut links = (1..300).map(Some).collect::<Vec<_>>();
    links.push(Some(10));
    let values = (0..300).collect::<Vec<_>>();
    let looped = nodes(&mut world, &values, &links);
    world.variable("long_loop", list, &bytes(&[looped[0], 100_000]));
    let views = set(LINKED_VIEWS);
    let bound = choose(&views, list, &world).bound.expect("binds");
    let this = place(&world, "long_loop", list);
    let mut checkpoints = Checkpoints::default();
    let first = children(
        &bound,
        &mut world,
        this.clone(),
        100_000,
        0,
        260,
        &mut checkpoints,
    );
    assert!(matches!(&first, Ok(page) if page.len() == 260), "{first:?}");
    let mut offset = 260;
    let found = loop {
        assert!(offset < 5_000, "no cycle found by element {offset}");
        match children(
            &bound,
            &mut world,
            this.clone(),
            100_000,
            offset,
            200,
            &mut checkpoints,
        ) {
            Ok(_) => offset += 200,
            Err(Failure::Problem(problem)) => break problem,
            Err(Failure::Debugger(error)) => panic!("{error}"),
        }
    };
    assert!(
        matches!(found, crate::ViewProblem::Cycle { at } if at > 300),
        "{found:?}"
    );
}

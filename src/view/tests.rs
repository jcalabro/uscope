use super::run::{Child, Failure, Presented, children, present};
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
    present(&bound, world, this).map_err(|failure| match failure {
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
    assert_eq!(shown.count, Some(3));
    assert_eq!(shown.summary, "len=3 [10, 20, 30]");
    assert_eq!(summary(&mut world, &views, "none", intvec), "len=0 []");
    assert_eq!(
        summary(&mut world, &views, "big", intvec),
        "len=300 [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, …]"
    );

    let bound = choose(&views, intvec, &world).bound.expect("binds");
    let this = place(&world, "v", intvec);
    let page = children(&bound, &mut world, this, 0, 10).expect("children");
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
        let page = children(&bound, &mut world, this, index, 1).expect("children");
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
    let page = children(&bound, &mut world, this, u64::MAX - 1, 4).expect("children");
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
        messages[4].contains("`list` is not supported yet"),
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
        element, length, ..
    } = otherwise.as_ref()
    else {
        panic!("a sequence");
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
    if outcome.starts_with("children: ") {
        return match children(&bound, &mut world, this, 0, 64) {
            Ok(children) => {
                let rendered = children
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
                        Child::Raw => "[raw]".to_owned(),
                    })
                    .collect::<Vec<_>>();
                format!("children: {}", rendered.join(", "))
            }
            Err(Failure::Problem(problem)) => format!("problem: {problem}"),
            Err(Failure::Debugger(error)) => format!("debugger: {error}"),
        };
    }
    match present(&bound, &mut world, this) {
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

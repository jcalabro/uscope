use std::sync::Arc;

use loom::thread;

use crate::in_order::{Add, InOrder};

/// Adds walks to a list, holding until the unit `fails_at`.
struct Log {
    added: Vec<Option<usize>>,
    fails_at: Option<usize>,
}

impl Add<usize> for Log {
    fn add(&mut self, walk: Option<usize>) -> bool {
        self.added.push(walk);
        walk.is_some() && self.fails_at != Some(self.added.len() - 1)
    }
}

/// Groups for `group_of`, each unit's group, named by its first unit: the
/// order they are claimed in, and their members.
fn groups(group_of: &[usize]) -> (&'static [usize], &'static [Vec<usize>]) {
    let mut members = vec![Vec::new(); group_of.len()];
    for (unit, &group) in group_of.iter().enumerate() {
        members[group].push(unit);
    }
    let order = (0..members.len())
        .filter(|&group| !members[group].is_empty())
        .collect::<Vec<_>>();
    (Vec::leak(order), Vec::leak(members))
}

type Walk = fn(&InOrder<'static, usize, Log>, usize, &[usize]) -> Vec<(usize, Option<usize>)>;

/// Two workers, the test's thread and one more, walk `group_of`'s units
/// with `walk` and a window of `ahead`, and `check` what was added.
fn model(
    group_of: &[usize],
    ahead: usize,
    fails_at: Option<usize>,
    walk: Walk,
    check: fn(usize, &Log),
) {
    let (order, members) = groups(group_of);
    loom::model(move || {
        let log = Log {
            added: Vec::new(),
            fails_at,
        };
        let in_order = Arc::new(InOrder::new(order, members, log, ahead));
        let worker = thread::spawn({
            let in_order = Arc::clone(&in_order);
            move || work(&in_order, walk)
        });
        work(&in_order, walk);
        worker.join().expect("the worker finishes");
        let Ok(in_order) = Arc::try_unwrap(in_order) else {
            panic!("both workers are done with it");
        };
        let (added, log) = in_order.into_parts();
        check(added, &log);
    });
}

/// A walk's own panic, which ends only the worker it panics on.
struct WalkPanicked;

/// One worker's share, ended early by a walk that panics, as a worker's
/// thread ends when a walk panics in uscope.
fn work(in_order: &InOrder<'static, usize, Log>, walk: Walk) {
    static QUIET: std::sync::Once = std::sync::Once::new();
    QUIET.call_once(|| {
        let report = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !info.payload().is::<WalkPanicked>() {
                report(info);
            }
        }));
    });
    let worked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        in_order.work(|group, units| walk(in_order, group, units));
    }));
    if let Err(panic) = worked
        && !panic.is::<WalkPanicked>()
    {
        std::panic::resume_unwind(panic);
    }
}

fn walked(
    _: &InOrder<'static, usize, Log>,
    _group: usize,
    units: &[usize],
) -> Vec<(usize, Option<usize>)> {
    units.iter().map(|&unit| (unit, Some(unit))).collect()
}

fn in_unit_order(added: usize, log: &Log) {
    assert_eq!(added, log.added.len());
    assert_eq!(
        log.added,
        (0..added).map(Some).collect::<Vec<_>>(),
        "every unit is added once, in unit order"
    );
}

/// Units walked one to a group, so each worker's adding hands off to the
/// other's. A unit stranded in the finished map fails the count; a worker
/// left waiting for the window fails as a deadlock.
#[test]
fn single_unit_groups_are_added_in_order() {
    model(&[0, 1, 2], 1, None, walked, |added, log| {
        assert_eq!(added, 3);
        in_unit_order(added, log);
    });
}

/// A group whose units are apart: unit 2 is walked with unit 0, before
/// unit 1, and waits for it.
#[test]
fn a_group_split_by_another_waits_for_it() {
    model(&[0, 1, 0], 1, None, walked, |added, log| {
        assert_eq!(added, 3);
        in_unit_order(added, log);
    });
}

/// A wider window lets both workers walk ahead of the units added.
#[test]
fn a_wider_window_still_adds_in_order() {
    model(&[0, 1, 2], 2, None, walked, |added, log| {
        assert_eq!(added, 3);
        in_unit_order(added, log);
    });
}

/// Once the walk stops holding, the units left are added unwalked.
#[test]
fn units_after_the_walk_stops_holding_are_added_unwalked() {
    model(&[0, 1, 2], 1, Some(0), walked, |added, log| {
        assert_eq!(added, 3, "every unit is still added");
        assert_eq!(log.added[0], Some(0));
        // Unit 2's group begins after unit 0 is added, when the walk no
        // longer holds.
        assert_eq!(log.added[2], None);
    });
}

/// A worker whose walk of unit 0 panics abandons the walk as it unwinds,
/// and unit 0 is never added. The worker waiting to walk unit 2, which
/// waits for unit 1 to be added, must be woken all the same.
#[test]
fn a_panicking_walk_wakes_the_waiting_worker() {
    model(
        &[0, 1, 2],
        1,
        None,
        |in_order, group, units| {
            if group == 0 {
                std::panic::panic_any(WalkPanicked);
            }
            walked(in_order, group, units)
        },
        |added, log| {
            assert_eq!(added, 0, "nothing after unit 0 is added");
            assert!(log.added.is_empty());
        },
    );
}

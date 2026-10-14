//! The rings' life cycle, which Miri runs too: `just miri`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use super::*;

/// The rings and the control block are the process's, so one test at a
/// time uses them.
static SERIAL: Mutex<()> = Mutex::new(());

static STATE: AtomicU8 = AtomicU8::new(1);

/// Turns recording on, with the smallest rings, and holds the other tests
/// off. A test starts as a forked child would, taking no ring another
/// test made.
fn listen() -> std::sync::MutexGuard<'static, ()> {
    let serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    FORKS.fetch_add(1, Ordering::Relaxed);
    CONTROL
        .ring_size
        .store(abi::LEAST_RING_SIZE, Ordering::Relaxed);
    CONTROL.heartbeat.store(monotonic(), Ordering::Relaxed);
    CONTROL.timeout.store(u32::MAX, Ordering::Relaxed);
    CONTROL.enable.store(1, Ordering::Release);
    serial
}

/// The ring this thread writes to.
fn mine() -> &'static Ring {
    match RING.with(Cell::get) {
        Slot::Ring(ring) => ring,
        Slot::Unset | Slot::Nothing => panic!("the thread has no ring"),
    }
}

/// Whether the registry uscope reads lists `ring`.
fn registered(ring: &'static Ring) -> bool {
    let rings = CONTROL.rings.load(Ordering::Acquire) as usize;
    CONTROL.registry[..rings]
        .iter()
        .any(|slot| ptr::eq(slot.load(Ordering::Acquire), &ring.header))
}

/// An event as uscope decodes it.
#[derive(Debug)]
struct Event {
    site: u32,
    kind: u8,
    argument: u32,
    more: Vec<u8>,
}

/// Reads `ring`'s records from `tail` to its head, as uscope does, and
/// returns the events and the new tail.
fn read(ring: &Ring, tail: u64) -> (Vec<Event>, u64) {
    let head = ring.header.head.load(Ordering::Acquire);
    assert!(head - tail <= ring.records.len() as u64, "the ring wrapped");
    let record = |position: u64| {
        let words = &ring.records[position as usize & (ring.records.len() - 1)].0;
        let mut bytes = [0; abi::RECORD];
        bytes[..8].copy_from_slice(&words[0].load(Ordering::Relaxed).to_le_bytes());
        bytes[8..].copy_from_slice(&words[1].load(Ordering::Relaxed).to_le_bytes());
        bytes
    };

    let mut events = Vec::new();
    let mut position = tail;
    while position < head {
        let bytes = record(position);
        let word = u32::from_le_bytes(bytes[12..].try_into().unwrap());
        let kind = (word & 0xff) as u8;
        assert_ne!(kind, abi::CONTINUED, "an event starts at {position}");
        let mut event = Event {
            site: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            kind,
            argument: word >> 8,
            more: Vec::new(),
        };

        position += 1;
        while position < head && record(position)[12] == abi::CONTINUED {
            let bytes = record(position);
            event.more.extend_from_slice(&bytes[..12]);
            event.more.extend_from_slice(&bytes[13..]);
            position += 1;
        }

        events.push(event);
    }

    (events, position)
}

/// A message's text, by its length.
fn text(event: &Event) -> &[u8] {
    &event.more[..event.argument as usize]
}

#[test]
fn a_threads_events_are_whole_records_in_its_own_ring() {
    let _serial = listen();
    let (ring, thread) = std::thread::spawn(|| {
        drop(Zone::begin(&STATE));
        __message(&STATE, format_args!("a message of 38 bytes, in 3 records"));
        __plot(&STATE, 1.5);
        (mine(), gettid().cast_unsigned())
    })
    .join()
    .unwrap();

    assert!(registered(ring));
    let header = &ring.header;
    assert_eq!(header.magic, abi::RING_MAGIC);
    assert_eq!(u32::from(header.size), abi::LEAST_RING_SIZE);
    assert_eq!(header.thread.load(Ordering::Relaxed), thread);
    assert_eq!(header.state.load(Ordering::Acquire), abi::RING_RETIRED);
    let events = read(ring, 0).0;
    let kinds = events.iter().map(|event| event.kind).collect::<Vec<_>>();
    assert_eq!(
        kinds,
        [abi::ZONE_BEGIN, abi::ZONE_END, abi::MESSAGE, abi::PLOT]
    );
    assert!(events.iter().all(|event| event.site == site(&STATE)));
    assert_eq!(text(&events[2]), b"a message of 38 bytes, in 3 records");
    assert_eq!(events[2].more.len(), 45, "three continued records");
    assert_eq!(events[3].more[..8], 1.5_f64.to_le_bytes());
    assert_eq!(header.head.load(Ordering::Relaxed), 1 + 1 + 4 + 2);
}

#[test]
fn a_ring_uscope_freed_is_taken_again_but_not_after_a_fork() {
    let _serial = listen();
    let emit = || {
        std::thread::spawn(|| {
            __event(&STATE, abi::TRIGGER);
            (mine(), gettid().cast_unsigned())
        })
        .join()
        .unwrap()
    };

    let (first, _) = emit();
    let (other, _) = emit();
    assert!(!ptr::eq(first, other), "a retired ring is uscope's to read");

    first.header.state.store(abi::RING_FREE, Ordering::Release);
    let (again, thread) = emit();
    assert!(ptr::eq(first, again), "a freed ring is taken again");
    assert_eq!(again.header.thread.load(Ordering::Relaxed), thread);
    let head = again.header.head.load(Ordering::Relaxed);
    assert_eq!(
        read(again, head - 1).0[0].kind,
        abi::TRIGGER,
        "its events follow the last"
    );

    // A forked child's rings are its own: a ring made before the fork
    // stays free.
    again.header.state.store(abi::RING_FREE, Ordering::Release);
    FORKS.fetch_add(1, Ordering::Relaxed);
    let (after, _) = emit();
    assert!(!ptr::eq(again, after));
    assert_eq!(again.header.state.load(Ordering::Relaxed), abi::RING_FREE);
}

/// Threads come and go, writing messages, while a reader reads their rings
/// and frees those they left; every message arrives once, whole, in its
/// thread's order.
#[test]
fn concurrent_threads_reuse_the_rings_a_reader_frees() {
    const LANES: usize = 2;
    const THREADS: usize = 3;
    const MESSAGES: usize = 2;
    let _serial = listen();
    let made = || {
        let made = MADE_COUNT.load(Ordering::Acquire).min(abi::MOST_RINGS);
        MADE[..made].iter().filter_map(OnceLock::get).copied()
    };

    // Where uscope last read each ring that other tests made.
    let mut tails = made()
        .map(|ring| {
            (
                ptr::from_ref(ring).addr(),
                ring.header.head.load(Ordering::Acquire),
            )
        })
        .collect::<HashMap<_, _>>();
    let done = AtomicBool::new(false);
    let seen = std::thread::scope(|scope| {
        let lanes = (0..LANES)
            .map(|lane| {
                scope.spawn(move || {
                    for thread in 0..THREADS {
                        std::thread::spawn(move || {
                            for message in 0..MESSAGES {
                                __message(&STATE, format_args!("{lane}.{thread}.{message}:padded"));
                            }
                        })
                        .join()
                        .unwrap();
                    }
                })
            })
            .collect::<Vec<_>>();
        let done = &done;
        let reader = scope.spawn(move || {
            let mut seen = Vec::new();
            let mut sweep = || {
                // As uscope does, it reads rings as their threads write
                // them, and frees those their threads left.
                for ring in made() {
                    let state = ring.header.state.load(Ordering::Acquire);
                    if state == abi::RING_ACTIVE || state == abi::RING_RETIRED {
                        let tail = tails.entry(ptr::from_ref(ring).addr()).or_default();
                        let (events, next) = read(ring, *tail);
                        *tail = next;
                        seen.extend(events.iter().map(|event| text(event).to_vec()));
                    }

                    if state == abi::RING_RETIRED {
                        ring.header.state.store(abi::RING_FREE, Ordering::Release);
                    }
                }
            };

            while !done.load(Ordering::Acquire) {
                sweep();
                std::thread::yield_now();
            }

            sweep();
            seen
        });
        for lane in lanes {
            lane.join().unwrap();
        }

        done.store(true, Ordering::Release);
        reader.join().unwrap()
    });

    let mut expected = Vec::new();
    for lane in 0..LANES {
        for thread in 0..THREADS {
            for message in 0..MESSAGES {
                expected.push(format!("{lane}.{thread}.{message}:padded").into_bytes());
            }
        }
    }

    let mut sorted = seen.clone();
    sorted.sort();
    assert_eq!(sorted, expected, "every message, once and whole");

    // Rings are read in any order, so only a thread's own are in order.
    for writer in expected.chunks(MESSAGES) {
        let prefix = &writer[0][..4];
        let order = seen
            .iter()
            .filter(|text| text.starts_with(prefix))
            .collect::<Vec<_>>();
        assert_eq!(
            order,
            writer.iter().collect::<Vec<_>>(),
            "a thread's messages in order"
        );
    }
}

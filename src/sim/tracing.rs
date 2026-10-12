//! A model of the BPF engine's tracepoint stream. Each call the simulated
//! kernel served is written as the stream program writes it into a ring
//! of a chosen size, which a consumer drains every so many records, and
//! counted into per-CPU statistics as the count program counts it. The
//! real decoder reads the ring and the real folder the statistics, and
//! the oracle checks that every call served is in the trace or counted
//! lost, and in the statistics.
//!
//! Its shape is drawn from [`Stream::Trace`] alone, so seeds choose as
//! they did before it existed.

use std::collections::{BTreeMap, VecDeque};

use super::choices::{Choices, Stream};
use super::kernel::syscalls::Served;
use super::marks::{Mark, Marks};
use crate::backend::syscall_engines::bpf::{self, RawErrnoKey, RawStatValue, Record};
use crate::syscalls::stats::BUCKETS;
use crate::syscalls::{Abi, Outcome, SyscallId};

/// How many records the ring holds: so few that most are lost, a few, and
/// more than a run makes.
const CAPACITIES: [usize; 4] = [1, 4, 64, 1 << 16];
/// How many records arrive between the consumer's drains.
const DRAINS: [u64; 4] = [1, 3, 16, 1000];
/// The CPUs calls are counted on.
const CPUS: u32 = 4;

/// The stream, the statistics, and what the oracle compares them with.
pub struct TraceModel {
    capacity: usize,
    drain_every: u64,
    since_drain: u64,
    /// Records written and not yet read, with the call each was made of.
    ring: VecDeque<(Served, Vec<u8>)>,
    served: u64,
    traced: u64,
    /// What the stream program counts in its lost counter.
    lost: u64,
    stats: BTreeMap<SyscallId, BTreeMap<u32, RawStatValue>>,
    /// Failures by slot and errno.
    errnos: BTreeMap<(u32, u32), u64>,
    /// Each call's calls and errors, by errno, as served.
    expected: BTreeMap<SyscallId, (u64, u64, BTreeMap<u16, u64>)>,
    /// Drops records without counting them, under
    /// [`super::world::Sabotage::LoseTracedCalls`].
    #[cfg(test)]
    pub uncounted: bool,
}

impl TraceModel {
    pub fn new(choices: &mut Choices) -> Self {
        Self {
            capacity: *choices.pick(Stream::Trace, &CAPACITIES),
            drain_every: *choices.pick(Stream::Trace, &DRAINS),
            since_drain: 0,
            ring: VecDeque::new(),
            served: 0,
            traced: 0,
            lost: 0,
            stats: BTreeMap::new(),
            errnos: BTreeMap::new(),
            expected: BTreeMap::new(),
            #[cfg(test)]
            uncounted: false,
        }
    }

    /// Writes and counts each call served, draining the ring as its
    /// consumer would.
    pub fn feed(&mut self, served: Vec<Served>, marks: &mut Marks) -> Result<(), String> {
        for call in served {
            self.count(&call);
            self.served += 1;
            if self.ring.len() < self.capacity {
                let bytes = record(&call, self.served);
                self.ring.push_back((call, bytes));
            } else {
                marks.hit(Mark::TraceLost);
                #[cfg(test)]
                if self.uncounted {
                    continue;
                }
                self.lost += 1;
            }
            self.since_drain += 1;
            if self.since_drain >= self.drain_every {
                self.drain(marks)?;
            }
        }
        Ok(())
    }

    /// Counts a call as the count program does, on a CPU of its thread's.
    fn count(&mut self, call: &Served) {
        let id = SyscallId::new(Abi::X86_64, call.number);
        let stat = self
            .stats
            .entry(id)
            .or_default()
            .entry(call.tid.cast_unsigned() % CPUS)
            .or_insert(RawStatValue {
                calls: 0,
                errors: 0,
                timed: 0,
                total_ns: 0,
                min_ns: 0,
                max_ns: 0,
                bytes: 0,
                log2_ns: [0; BUCKETS],
            });
        stat.calls += 1;
        let expected = self.expected.entry(id).or_default();
        expected.0 += 1;
        if let Some(errno) = call.returned.and_then(errno_of) {
            stat.errors += 1;
            expected.1 += 1;
            *expected.2.entry(errno).or_default() += 1;
            *self
                .errnos
                .entry((bpf::errno_slot(id), u32::from(errno)))
                .or_default() += 1;
        }
    }

    /// Reads every record in the ring with the real decoder, which must
    /// give back the call it was made of.
    fn drain(&mut self, marks: &mut Marks) -> Result<(), String> {
        self.since_drain = 0;
        while let Some((call, bytes)) = self.ring.pop_front() {
            let event = match bpf::decode(&bytes) {
                Ok(Record::Call(event)) => event,
                other => return Err(format!("the record of {call:?} decoded as {other:?}")),
            };
            let outcome = call
                .returned
                .map_or(Outcome::ProcessExited, Outcome::of_return);
            let read = (
                event.thread,
                event.process,
                event.call,
                event.args,
                &event.outcome,
            );
            let made = (
                call.tid.cast_unsigned(),
                call.tgid.cast_unsigned(),
                SyscallId::new(Abi::X86_64, call.number),
                call.args,
                &outcome,
            );
            if read != made {
                return Err(format!(
                    "the trace shows {read:?}, but the kernel served {made:?}"
                ));
            }
            self.traced += 1;
            marks.hit(Mark::TraceDecoded);
        }
        Ok(())
    }

    /// Drains what is left and judges the run's trace: every call served
    /// was read or counted lost, and the folded statistics count each.
    pub fn finish(&mut self, marks: &mut Marks) -> Result<(), String> {
        self.drain(marks)?;
        if self.traced + self.lost != self.served {
            return Err(format!(
                "the kernel served {} calls, but the trace holds {} and counts {} lost",
                self.served, self.traced, self.lost
            ));
        }
        let folded = bpf::fold(
            0,
            self.stats
                .iter()
                .map(|(&call, cpus)| (call, cpus.values().copied().collect())),
            self.errnos
                .iter()
                .map(|(&(slot, err), &count)| (RawErrnoKey { slot, err }, count)),
        );
        for (call, (calls, errors, errnos)) in &self.expected {
            let stats = folded.by_call.get(&(0, *call));
            let counted = stats.map(|stats| (stats.calls, stats.errors, &stats.errors_by_errno));
            if counted != Some((*calls, *errors, errnos)) {
                return Err(format!(
                    "{} was served {calls} times, {errors} failing as {errnos:?}, but the \
                     statistics count {counted:?}",
                    call.display_name()
                ));
            }
        }
        Ok(())
    }
}

/// A failed call's errno.
fn errno_of(returned: i64) -> Option<u16> {
    (-4095..0)
        .contains(&returned)
        .then(|| u16::try_from(-returned).ok())
        .flatten()
}

/// The stream program's record of `call`, the `sequence`th served, whose
/// times the sequence stands for.
fn record(call: &Served, sequence: u64) -> Vec<u8> {
    bpf::encode_call(
        call.tid.cast_unsigned(),
        call.tgid.cast_unsigned(),
        SyscallId::new(Abi::X86_64, call.number),
        call.args,
        call.returned,
        (2 * sequence, 2 * sequence + 1),
    )
}

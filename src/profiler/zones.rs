//! What a profile's instrumentation recorded: zones and labels, plotted
//! values, the program's markers, and what was lost.

use super::Profile;
use crate::capture::records::{AccountKind, MarkerKind, NONE};

pub use crate::capture::records::{ZONE_BEGAN_UNSEEN, ZONE_ENDED_UNSEEN, ZONE_MISMATCHED};

/// One instrumented zone, or one label's span, of a thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileZone {
    pub pid: u32,
    pub tid: u32,
    /// How many of the thread's zones hold it.
    pub depth: u32,
    /// Its site's name: a zone's name, or a label's key.
    pub name: String,
    /// A label's value; `None` for a zone.
    pub value: Option<String>,
    pub start: i64,
    pub end: i64,
    /// `ZONE_` flags: what of it was not seen.
    pub flags: u32,
}

/// A value of a plotted series.
#[derive(Debug, Clone, PartialEq)]
pub struct PlotValue {
    pub name: String,
    pub tid: u32,
    pub time: i64,
    pub value: f64,
}

/// An instant a program marked on a thread: a frame's end, a message, or
/// a trigger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramMarker {
    pub kind: MarkerKind,
    pub tid: u32,
    pub time: i64,
    /// Its site's name.
    pub name: String,
    /// A message's text.
    pub text: Option<String>,
}

/// What a recording read of a program's instrumentation, and lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProbeCounts {
    pub events: u64,
    /// Records threads wrote over before uscope read them.
    pub overwritten: u64,
    /// Events no ring could take.
    pub no_ring: u64,
    /// Events a signal handler raised while its thread was writing.
    pub interrupted: u64,
    /// Records, and rings, that broke the ABI.
    pub malformed: u64,
}

impl ProbeCounts {
    /// Whether anything was lost.
    #[must_use]
    pub const fn lost(&self) -> bool {
        self.overwritten + self.no_ring + self.interrupted + self.malformed > 0
    }
}

impl Profile {
    /// Every zone and label span, by thread, depth, and start.
    #[must_use]
    pub fn zones(&self) -> Vec<ProfileZone> {
        let capture = &self.capture;
        let mut zones = Vec::with_capacity(capture.zones.len());
        for range in &capture.zone_ranges {
            let thread = &capture.threads[range.thread.get() as usize];
            let pid = capture.processes[thread.process.get() as usize].pid.get();
            let start = range.start.get() as usize;
            for zone in &capture.zones[start..start + range.count.get() as usize] {
                let value = zone.value.get();
                zones.push(ProfileZone {
                    pid,
                    tid: thread.tid.get(),
                    depth: range.depth.get(),
                    name: self.site_name(zone.site.get()),
                    value: (value != NONE).then(|| capture.string(value).to_owned()),
                    start: zone.start.get(),
                    end: zone.end.get(),
                    flags: zone.flags.get(),
                });
            }
        }
        zones
    }

    /// Every plotted value, by series and time.
    #[must_use]
    pub fn plots(&self) -> Vec<PlotValue> {
        let capture = &self.capture;
        capture
            .plots
            .iter()
            .map(|plot| PlotValue {
                name: self.site_name(plot.site.get()),
                tid: self.tid(plot.thread.get()),
                time: plot.time.get(),
                value: f64::from_bits(plot.value.get()),
            })
            .collect()
    }

    /// Every instant the program marked, by time.
    #[must_use]
    pub fn program_markers(&self) -> Vec<ProgramMarker> {
        let capture = &self.capture;
        capture
            .markers
            .iter()
            .filter_map(|marker| {
                let kind = MarkerKind::from_number(marker.kind)?;
                matches!(
                    kind,
                    MarkerKind::Frame | MarkerKind::Message | MarkerKind::Trigger
                )
                .then(|| ProgramMarker {
                    kind,
                    tid: self.tid(marker.thread.get()),
                    time: marker.time.get(),
                    name: self.site_name(marker.site.get()),
                    text: (marker.text.get() != NONE)
                        .then(|| capture.string(marker.text.get()).to_owned()),
                })
            })
            .collect()
    }

    /// Why uscope could not read a module's instrumentation.
    #[must_use]
    pub fn probe_problems(&self) -> Vec<(i64, String)> {
        self.markers(MarkerKind::Probes)
    }

    /// What the recording read of the program's instrumentation, and lost.
    #[must_use]
    pub fn probe_counts(&self) -> ProbeCounts {
        let account = |kind: AccountKind| self.capture.account(kind as u32);
        ProbeCounts {
            events: account(AccountKind::ProbeEvents),
            overwritten: account(AccountKind::ProbeOverwritten),
            no_ring: account(AccountKind::ProbeNoRing),
            interrupted: account(AccountKind::ProbeInterrupted),
            malformed: account(AccountKind::ProbeMalformed),
        }
    }

    fn site_name(&self, site: u32) -> String {
        self.capture
            .sites
            .get(site as usize)
            .map(|site| self.capture.string(site.name.get()).to_owned())
            .unwrap_or_default()
    }

    fn tid(&self, thread: u32) -> u32 {
        self.capture
            .threads
            .get(thread as usize)
            .map_or(0, |thread| thread.tid.get())
    }
}

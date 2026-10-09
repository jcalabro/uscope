//! Spans as Tracy's zones and counters as its plots, while a Tracy viewer
//! is connected. The client listens on this machine only, and records
//! nothing until a viewer connects.

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use tracy_client::{Client, PlotName};

/// The zone of a span named `name`, labelled by `label`, while a viewer is
/// connected.
pub(super) fn zone(
    name: &'static str,
    label: Option<&dyn Fn() -> String>,
) -> Option<tracy_client::Span> {
    if !Client::is_connected() {
        return None;
    }
    let zone = Client::running()?.span_alloc(Some(name), name, "uscope", 0, 0);
    if let Some(label) = label {
        zone.emit_text(&label());
    }
    Some(zone)
}

/// Plots the counter `name`'s total so far, after adding `amount`.
pub(super) fn plot(name: &'static str, amount: u64) {
    static TOTALS: Mutex<Vec<(&'static str, PlotName, u64)>> = Mutex::new(Vec::new());
    if !Client::is_connected() {
        return;
    }
    let Some(client) = Client::running() else {
        return;
    };
    let mut totals = TOTALS.lock().unwrap_or_else(PoisonError::into_inner);
    let index = totals
        .iter()
        .position(|(counter, ..)| *counter == name)
        .unwrap_or_else(|| {
            totals.push((name, PlotName::new_leak(name.to_owned()), 0));
            totals.len() - 1
        });
    totals[index].2 = totals[index].2.saturating_add(amount);
    let (_, plot, total) = totals[index];
    drop(totals);
    #[expect(clippy::cast_precision_loss, reason = "plotted approximately")]
    client.plot(plot, total as f64);
}

/// Waits up to `USCOPE_TRACY_WAIT` seconds for a viewer to connect, so that
/// a short run is recorded from its start.
pub fn wait_for_viewer() {
    let Some(seconds) = std::env::var("USCOPE_TRACY_WAIT")
        .ok()
        .and_then(|seconds| seconds.trim().parse::<u64>().ok())
    else {
        return;
    };
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !Client::is_connected() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
}

//! Data breakpoints: watchpoints on values the client picks.

use std::collections::HashMap;

use serde_json::{Value, json};
use uscope::{
    StackFrameId, StopContext, ValueExpression, VirtualAddress, WatchAccess, WatchScope,
    WatchpointHit, WatchpointId, WatchpointSpec,
};

use super::protocol::{self, DataBreakpointInfoArguments, ErrorBody, SetDataBreakpointsArguments};
use super::session::{Closed, Session, error, parse};

/// What a data id the client was given watches.
#[derive(Debug, Clone)]
pub struct DataTarget {
    spec: WatchpointSpec,
}

/// One data breakpoint the client set.
#[derive(Debug, Clone)]
pub struct DataEntry {
    pub id: i64,
    data_id: String,
    access: WatchAccess,
    pub watchpoint: Result<WatchpointId, String>,
}

/// The data breakpoints of a session.
#[derive(Debug, Default)]
pub struct Data {
    targets: HashMap<String, DataTarget>,
    next_target: u64,
    pub entries: Vec<DataEntry>,
}

impl Data {
    /// The client's ids of the data breakpoints a stop hit.
    pub fn hit(&self, hits: &[WatchpointHit]) -> Vec<i64> {
        self.entries
            .iter()
            .filter(|entry| {
                hits.iter().any(|hit| {
                    entry
                        .watchpoint
                        .as_ref()
                        .is_ok_and(|id| *id == hit.watchpoint)
                })
            })
            .map(|entry| entry.id)
            .collect()
    }

    /// Forgets the data breakpoints of watchpoints that no longer exist and
    /// returns their client ids.
    pub fn forget(&mut self, watchpoints: &[WatchpointId]) -> Vec<i64> {
        let mut forgotten = Vec::new();
        self.entries.retain(|entry| {
            let gone = entry
                .watchpoint
                .as_ref()
                .is_ok_and(|id| watchpoints.contains(id));
            if gone {
                forgotten.push(entry.id);
            }
            !gone
        });
        forgotten
    }
}

const fn access_name(access: WatchAccess) -> &'static str {
    match access {
        WatchAccess::Write => "write",
        WatchAccess::Read => "read",
        WatchAccess::ReadWrite => "readWrite",
    }
}

impl Session {
    pub(super) async fn data_breakpoint_info(
        &mut self,
        arguments: Value,
    ) -> Result<Value, ErrorBody> {
        let arguments =
            parse::<DataBreakpointInfoArguments>(arguments, "dataBreakpointInfo arguments")?;
        let Ok(stop) = self.current_stop() else {
            return Ok(unwatchable("the program must be stopped to watch a value"));
        };
        let handle = self.target_handle()?;
        let (spec, description) = if arguments.as_address == Some(true) {
            let Some(address) = protocol::address(&arguments.name) else {
                return Ok(unwatchable(&format!(
                    "'{}' is not an address",
                    arguments.name
                )));
            };
            let bytes = arguments
                .bytes
                .and_then(|bytes| u64::try_from(bytes).ok())
                .filter(|bytes| *bytes != 0)
                .unwrap_or(1);
            (
                WatchpointSpec::Location {
                    address: VirtualAddress::new(address),
                    byte_size: bytes,
                },
                format!("{bytes} bytes at {address:#x}"),
            )
        } else {
            let (context, expression) = if let Some(reference) = arguments.variables_reference {
                let Some((context, path)) = self.references.child_path(reference, &arguments.name)
                else {
                    return Ok(unwatchable(&format!("{} cannot be named", arguments.name)));
                };
                (context, path)
            } else {
                let context = match arguments.frame_id {
                    Some(frame) => self.references.frame_context(frame).ok_or_else(|| {
                        ErrorBody::new(format!("frame reference {frame} is stale"))
                    })?,
                    None => StopContext {
                        stop: stop.id,
                        thread: stop.thread,
                        frame: StackFrameId::INNERMOST,
                    },
                };
                match uscope::parse_value_expression(arguments.name.trim()) {
                    Ok(parsed) if parsed.range.is_none() => (context, parsed.expression),
                    Ok(_) => return Ok(unwatchable("a range cannot be watched")),
                    Err(error) => return Ok(unwatchable(&error.to_string())),
                }
            };
            match handle.at(context).resolve_watch_target(expression).await {
                Ok(target) => {
                    let description = format!(
                        "{} ({} bytes at {}{})",
                        target.expression(),
                        target.byte_size(),
                        target.address(),
                        scope_text(target.scope())
                    );
                    (WatchpointSpec::Target(Box::new(target)), description)
                }
                Err(error) => return Ok(unwatchable(&error.to_string())),
            }
        };
        self.data.next_target += 1;
        let data_id = format!("data-{}", self.data.next_target);
        self.data
            .targets
            .insert(data_id.clone(), DataTarget { spec });
        let access = handle
            .watchpoint_capabilities()
            .access
            .iter()
            .map(|access| access_name(*access))
            .collect::<Vec<_>>();
        Ok(json!({
            "dataId": data_id,
            "description": description,
            "accessTypes": access,
            "canPersist": false,
        }))
    }

    pub(super) async fn set_data_breakpoints(
        &mut self,
        arguments: Value,
    ) -> Result<Value, ErrorBody> {
        let arguments =
            parse::<SetDataBreakpointsArguments>(arguments, "setDataBreakpoints arguments")?;
        let handle = self.target_handle()?;
        let wanted = arguments
            .breakpoints
            .iter()
            .map(|breakpoint| {
                let access = match breakpoint.access_type.as_deref() {
                    None | Some("write") => Ok(WatchAccess::Write),
                    Some("read") => Ok(WatchAccess::Read),
                    Some("readWrite") => Ok(WatchAccess::ReadWrite),
                    Some(other) => Err(format!("unknown access type '{other}'")),
                };
                (breakpoint, access)
            })
            .collect::<Vec<_>>();
        // Watchpoints use scarce debug registers, so release first.
        let mut kept = HashMap::new();
        for entry in std::mem::take(&mut self.data.entries) {
            let still_wanted = wanted.iter().any(|(breakpoint, access)| {
                breakpoint.data_id == entry.data_id && access.as_ref() == Ok(&entry.access)
            });
            if still_wanted && entry.watchpoint.is_ok() {
                kept.insert((entry.data_id.clone(), entry.access), entry);
            } else if let Ok(watchpoint) = entry.watchpoint {
                match handle.remove_watchpoint(watchpoint).await {
                    Ok(_) | Err(uscope::Error::WatchpointNotFound(_)) => {}
                    Err(error) => return Err(self::error(error)),
                }
            }
        }
        let mut entries = Vec::new();
        for (breakpoint, access) in wanted {
            let entry = match access {
                Ok(access) => {
                    if let Some(entry) = kept.remove(&(breakpoint.data_id.clone(), access)) {
                        entry
                    } else {
                        let watchpoint = self.install_data(breakpoint, access).await;
                        DataEntry {
                            id: self.breakpoints.allocate_id(),
                            data_id: breakpoint.data_id.clone(),
                            access,
                            watchpoint,
                        }
                    }
                }
                Err(message) => DataEntry {
                    id: self.breakpoints.allocate_id(),
                    data_id: breakpoint.data_id.clone(),
                    access: WatchAccess::Write,
                    watchpoint: Err(message),
                },
            };
            entries.push(entry);
        }
        let body = entries.iter().map(data_json).collect::<Vec<_>>();
        self.data.entries = entries;
        Ok(json!({"breakpoints": body}))
    }

    async fn install_data(
        &self,
        breakpoint: &protocol::DataBreakpoint,
        access: WatchAccess,
    ) -> Result<WatchpointId, String> {
        if breakpoint
            .condition
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty())
        {
            return Err("conditions on data breakpoints are not supported".to_owned());
        }
        if breakpoint
            .hit_condition
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty())
        {
            return Err("hit conditions on data breakpoints are not supported".to_owned());
        }
        let target = self.data.targets.get(&breakpoint.data_id).ok_or_else(|| {
            format!(
                "unknown data id '{}'; ask for it with dataBreakpointInfo",
                breakpoint.data_id
            )
        })?;
        let handle = self.target_handle().map_err(|error| error.format)?;
        handle
            .add_watchpoint(target.spec.clone(), access)
            .await
            .map(|watchpoint| watchpoint.id)
            .map_err(|error| error.to_string())
    }

    /// Reports data breakpoints whose watched storage ended.
    pub(super) async fn data_invalidated(
        &mut self,
        invalidated: &[uscope::InvalidatedWatchpoint],
    ) -> Result<(), Closed> {
        for entry in invalidated {
            let ids = self.data.forget(&[entry.watchpoint.id]);
            for id in ids {
                self.client
                    .event(
                        "breakpoint",
                        json!({"reason": "removed", "breakpoint": {"id": id, "verified": false}}),
                    )
                    .await?;
                self.client
                    .event(
                        "output",
                        json!({"category": "console", "output": format!(
                            "data breakpoint {id} on {} was removed: {}\n",
                            crate::cli::format::watch_subject(&entry.watchpoint),
                            invalidation_text(entry.reason)
                        )}),
                    )
                    .await?;
            }
        }
        Ok(())
    }
}

pub const fn invalidation_text(reason: uscope::WatchpointInvalidation) -> &'static str {
    match reason {
        uscope::WatchpointInvalidation::ScopeExited => "its frame or block is no longer active",
        uscope::WatchpointInvalidation::OwnerThreadExited => "the thread owning it exited",
        uscope::WatchpointInvalidation::ModuleUnloaded => "the module owning it was unloaded",
    }
}

fn scope_text(scope: &WatchScope) -> String {
    match scope {
        WatchScope::Location | WatchScope::Static { .. } => String::new(),
        WatchScope::ThreadLocal { thread } => format!(", thread {thread}'s instance"),
        WatchScope::Frame { .. } => ", until its function returns".to_owned(),
    }
}

fn unwatchable(reason: &str) -> Value {
    json!({"dataId": null, "description": reason})
}

fn data_json(entry: &DataEntry) -> Value {
    match &entry.watchpoint {
        Ok(_) => json!({"id": entry.id, "verified": true}),
        Err(message) => {
            json!({"id": entry.id, "verified": false, "message": message, "reason": "failed"})
        }
    }
}

/// The expression a child shown in a variables list evaluates as.
pub fn child_expression(path: &str) -> Option<ValueExpression> {
    uscope::parse_value_expression(path)
        .ok()
        .filter(|parsed| parsed.range.is_none())
        .map(|parsed| parsed.expression)
}

impl Session {
    /// Describes what a data breakpoint stop's accesses did to each value.
    pub(super) async fn watch_description(&self, hits: &[WatchpointHit]) -> String {
        let Ok(handle) = self.target_handle() else {
            return String::new();
        };
        let watchpoints = handle
            .snapshot()
            .await
            .map(|snapshot| snapshot.watchpoints)
            .unwrap_or_default();
        let image = handle.module_image();
        hits.iter()
            .map(|hit| {
                let watchpoint = watchpoints
                    .iter()
                    .find(|watchpoint| watchpoint.id == hit.watchpoint);
                let subject = watchpoint.map_or_else(
                    || format!("watchpoint {}", hit.watchpoint),
                    crate::cli::format::watch_subject,
                );
                let type_info = watchpoint.and_then(|watchpoint| watchpoint.type_info.as_ref());
                let value = |bytes: Option<&[u8]>| {
                    crate::cli::value::watched_bytes(bytes, type_info, Some(image))
                };
                if hit.changed() {
                    format!(
                        "{subject} changed from {} to {}",
                        value(hit.previous.as_deref()),
                        value(hit.current.as_deref())
                    )
                } else {
                    format!(
                        "{subject} was accessed; it is {}",
                        value(hit.current.as_deref())
                    )
                }
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

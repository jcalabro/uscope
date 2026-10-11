//! Profiles the server serves to every tab, whatever it debugs. Each is
//! named by a digest of its bytes, so a link to one opens the same profile
//! whenever a server serves it. Every answer is a query of
//! [`uscope::profiler`], which the CLI's reports read too.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{oneshot, watch};
use uscope::profiler::{
    GapReason, Granularity, KeyKind, Keys, Lanes, NO_TASK, Preset, Profile, Query, RecordOptions,
    SampleSource, Source, Transform, Watch, record_watched,
};

use super::protocol::{
    self, ErrorKind, ProfileCost, ProfileFunctions, ProfileGranularity, ProfileInfo, ProfileKey,
    ProfileKeyKind, ProfileLane, ProfileList, ProfileQuery, ProfileSetting, ProfileSource,
    ProfileSummary, ProfileTimeline, ProfileTree, RecordPreset, RecordProfile, RecordStarted,
    Request, Role, TransformOp,
};
use super::session::Failure;

/// The most slices a timeline is cut into.
const MOST_BUCKETS: u32 = 4096;

/// How often a recording publishes its profile so far.
const LIVE_EVERY: Duration = Duration::from_secs(2);

/// The profiles one server serves.
pub struct Profiles {
    cwd: PathBuf,
    entries: RwLock<BTreeMap<String, Arc<Entry>>>,
    list: watch::Sender<Arc<ProfileList>>,
    recordings: Mutex<BTreeMap<String, Recorder>>,
    /// Counts recordings, so each gets an id of its own.
    recorded: AtomicU64,
}

struct Entry {
    info: ProfileInfo,
    profile: Arc<Profile>,
}

struct Recorder {
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

impl Profiles {
    pub fn new(cwd: PathBuf) -> Arc<Self> {
        let (list, _) = watch::channel(Arc::new(ProfileList {
            profiles: Vec::new(),
        }));
        Arc::new(Self {
            cwd,
            entries: RwLock::new(BTreeMap::new()),
            list,
            recordings: Mutex::new(BTreeMap::new()),
            recorded: AtomicU64::new(0),
        })
    }

    /// Serves the profile in `bytes`, read from `path`, returning its id.
    pub fn add(&self, path: &Path, bytes: &[u8]) -> anyhow::Result<String> {
        let read = Profile::from_bytes(bytes)?;
        if read.truncated {
            eprintln!(
                "warning: {} was cut short; serving the part that was complete",
                path.display()
            );
        }
        let id = format!("{:016x}", twox_hash::XxHash3_64::oneshot(bytes));
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        self.publish(
            ProfileInfo {
                id: id.clone(),
                name,
                recording: false,
                revision: 0,
                ended: None,
            },
            read.profile,
        );
        Ok(id)
    }

    /// Serves `profile` as `info` says, replacing what its id served.
    #[expect(
        clippy::significant_drop_tightening,
        reason = "the list is sent under the lock, so changes publish in order"
    )]
    fn publish(&self, info: ProfileInfo, profile: Profile) {
        let mut entries = self.entries.write().expect("profiles lock");
        let id = info.id.clone();
        entries.insert(
            id,
            Arc::new(Entry {
                info,
                profile: Arc::new(profile),
            }),
        );
        let profiles = entries.values().map(|entry| entry.info.clone()).collect();
        self.list.send_replace(Arc::new(ProfileList { profiles }));
    }

    /// The profiles, now and as they change.
    pub fn subscribe(&self) -> watch::Receiver<Arc<ProfileList>> {
        self.list.subscribe()
    }

    /// A profile's file, by its id, and the name to save it under.
    pub fn file(&self, id: &str) -> Option<(String, Vec<u8>)> {
        let entry = self.entry(id).ok()?;
        let name = if std::path::Path::new(&entry.info.name)
            .extension()
            .is_some_and(|extension| extension == "usprof")
        {
            entry.info.name.clone()
        } else {
            format!("{}.usprof", entry.info.name)
        };
        Some((name, entry.profile.to_bytes()))
    }

    /// Ends every recording, killing its program, and waits for each.
    pub fn shutdown(&self) {
        let recordings = std::mem::take(&mut *self.recordings.lock().expect("recordings lock"));
        for recorder in recordings.values() {
            recorder.stop.store(true, Ordering::Relaxed);
        }
        for (_, recorder) in recordings {
            let _ = recorder.thread.join();
        }
    }

    /// Answers a request about a profile, with the bytes that go before the
    /// answer. Aggregation reads every sample a query selects, so it runs
    /// off the server's threads.
    pub async fn handle(
        self: &Arc<Self>,
        role: Role,
        request: Request,
    ) -> Result<(Value, Vec<u8>), Failure> {
        match request {
            Request::RecordProfile(spec) => {
                let profile = self.record(role, spec).await?;
                Ok(json(&RecordStarted { profile }))
            }
            Request::StopRecording(protocol::ProfileRef { profile, .. }) => {
                controls(role)?;
                let recordings = self.recordings.lock().expect("recordings lock");
                let recorder = recordings.get(&profile).ok_or_else(|| {
                    Failure::new(ErrorKind::Invalid, format!("{profile} is not recording"))
                })?;
                recorder.stop.store(true, Ordering::Relaxed);
                drop(recordings);
                Ok(json(&Value::Null))
            }
            request => {
                let profiles = Arc::clone(self);
                tokio::task::spawn_blocking(move || profiles.answer(request))
                    .await
                    .map_err(|_| Failure::new(ErrorKind::Failed, "the query failed"))?
            }
        }
    }

    /// Starts recording `spec`, answering once its program has started.
    async fn record(self: &Arc<Self>, role: Role, spec: RecordProfile) -> Result<String, Failure> {
        controls(role)?;
        let program = PathBuf::from(&spec.program);
        let mut command = Command::new(&program);
        command
            .args(&spec.arguments)
            .envs(spec.environment.iter().map(|(name, value)| (name, value)))
            .current_dir(
                spec.cwd
                    .as_ref()
                    .map_or_else(|| self.cwd.clone(), |cwd| self.cwd.join(cwd)),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut options = RecordOptions::preset(match spec.preset {
            RecordPreset::Production => Preset::Production,
            RecordPreset::Development => Preset::Development,
            RecordPreset::Maximum => Preset::Maximum,
        });
        if let Some(symbol) = spec.function.filter(|symbol| !symbol.trim().is_empty()) {
            options.source = SampleSource::Function { symbol };
        }
        if let Some(seconds) = spec.window {
            options.window = Some(
                std::time::Duration::try_from_secs_f64(seconds)
                    .ok()
                    .filter(|window| !window.is_zero())
                    .ok_or_else(|| {
                        Failure::new(ErrorKind::Invalid, "a window is a number of seconds")
                    })?,
            );
        }
        let count = self.recorded.fetch_add(1, Ordering::Relaxed);
        let id = format!(
            "{:016x}",
            twox_hash::XxHash3_64::oneshot(
                format!(
                    "{} {count} {:?}",
                    spec.program,
                    std::time::SystemTime::now()
                )
                .as_bytes()
            )
        );
        let name = program.file_name().map_or_else(
            || spec.program.clone(),
            |name| name.to_string_lossy().into_owned(),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let (started, starting) = oneshot::channel::<Result<(), String>>();
        let profiles = Arc::clone(self);
        let thread = std::thread::Builder::new()
            .name("uscope-web-record".into())
            .spawn({
                let (id, stop) = (id.clone(), Arc::clone(&stop));
                move || profiles.run_recording(&id, &name, command, &options, &stop, started)
            })
            .map_err(|error| Failure::new(ErrorKind::Failed, error.to_string()))?;
        self.recordings
            .lock()
            .expect("recordings lock")
            .insert(id.clone(), Recorder { stop, thread });
        match starting.await {
            Ok(Ok(())) => Ok(id),
            Ok(Err(message)) => Err(Failure::new(ErrorKind::Failed, message)),
            Err(_) => Err(Failure::new(
                ErrorKind::Failed,
                "the recording ended before it began",
            )),
        }
    }

    /// Records on this thread, publishing each profile under `id`.
    fn run_recording(
        &self,
        id: &str,
        name: &str,
        command: Command,
        options: &RecordOptions,
        stop: &AtomicBool,
        started: oneshot::Sender<Result<(), String>>,
    ) {
        let mut started = Some(started);
        let mut revision = 0;
        let info = |revision, ended: Option<String>| ProfileInfo {
            id: id.to_owned(),
            name: name.to_owned(),
            recording: ended.is_none(),
            revision,
            ended,
        };
        let recorded = record_watched(
            command,
            options,
            &mut Watch {
                every: LIVE_EVERY,
                publish: &mut |profile| {
                    self.publish(info(revision, None), profile);
                    revision += 1;
                    if let Some(started) = started.take() {
                        let _ = started.send(Ok(()));
                    }
                },
                stop,
                // The page saves what it shows: with a window, the window.
                save: None,
            },
        );
        match recorded {
            Ok(recording) => {
                self.publish(
                    info(revision, Some(recording.status.to_string())),
                    recording.profile,
                );
            }
            Err(error) => {
                if let Some(started) = started.take() {
                    let _ = started.send(Err(error.to_string()));
                } else if let Ok(entry) = self.entry(id) {
                    let profile = Profile::clone(&entry.profile);
                    self.publish(info(revision, Some(format!("failed: {error}"))), profile);
                }
            }
        }
        self.recordings.lock().expect("recordings lock").remove(id);
    }

    fn entry(&self, id: &str) -> Result<Arc<Entry>, Failure> {
        self.entries
            .read()
            .expect("profiles lock")
            .get(id)
            .cloned()
            .ok_or_else(|| Failure::new(ErrorKind::Invalid, format!("no profile {id}")))
    }

    fn answer(&self, request: Request) -> Result<(Value, Vec<u8>), Failure> {
        match request {
            Request::Profile(protocol::ProfileRef { profile, .. }) => {
                Ok(json(&summary(&*self.entry(&profile)?)))
            }
            Request::ProfileTree(query) => {
                let entry = self.entry(&query.profile)?;
                let keys = entry.profile.keys(granularity(query.granularity));
                let tree = keys.aggregate(&resolve(&entry.profile, &keys, &query)?);
                let mut named = Named::new(&keys);
                let mut bytes = Vec::with_capacity(tree.len() * 24);
                for weights in [&tree.own, &tree.total] {
                    for weight in weights {
                        bytes.extend_from_slice(&exact(*weight).to_le_bytes());
                    }
                }
                for key in &tree.key {
                    bytes.extend_from_slice(&named.index(*key).to_le_bytes());
                }
                for depth in &tree.depth {
                    bytes.extend_from_slice(&depth.to_le_bytes());
                }
                let answer = ProfileTree {
                    nodes: u32::try_from(tree.len()).unwrap_or(u32::MAX),
                    weight: tree.weight,
                    samples: tree.samples,
                    keys: named.keys,
                };
                Ok((to_value(&answer), bytes))
            }
            Request::ProfileFunctions(query) => {
                let entry = self.entry(&query.profile)?;
                let keys = entry.profile.keys(granularity(query.granularity));
                let resolved = resolve(&entry.profile, &keys, &query)?;
                let costs = keys.functions(&resolved);
                let mut named = Named::new(&keys);
                let rows = costs
                    .rows
                    .iter()
                    .map(|cost| ProfileCost {
                        key: named.index(cost.key),
                        own: cost.own,
                        total: cost.total,
                    })
                    .collect();
                Ok(json(&ProfileFunctions {
                    weight: costs.weight,
                    rows,
                    keys: named.keys,
                }))
            }
            Request::ProfileTimeline(timeline) => {
                let entry = self.entry(&timeline.query.profile)?;
                let keys = entry.profile.keys(granularity(timeline.query.granularity));
                let mut resolved = resolve(&entry.profile, &keys, &timeline.query)?;
                let (start, end) = entry.profile.span();
                let range = resolved
                    .range
                    .unwrap_or_else(|| (start, end.saturating_add(1)));
                resolved.range = Some(range);
                let buckets = timeline.buckets.clamp(1, MOST_BUCKETS) as usize;
                let by_task = timeline.task_lanes || timeline.query.source == ProfileSource::Waits;
                let of = if by_task {
                    Lanes::Tasks
                } else {
                    Lanes::Threads
                };
                let lane = |id: u32, weights| ProfileLane {
                    thread: (!by_task).then_some(id),
                    task: (by_task && id != NO_TASK).then_some(id),
                    weights,
                    blocked: Vec::new(),
                    preempted: Vec::new(),
                };
                let mut lanes: std::collections::BTreeMap<u32, ProfileLane> = keys
                    .density(&resolved, buckets, of)
                    .into_iter()
                    .map(|(id, weights)| (id, lane(id, weights)))
                    .collect();
                // Time off a CPU is a thread's.
                if !by_task {
                    for off in entry.profile.off_cpu_lanes(&resolved, buckets) {
                        let lane = lanes
                            .entry(off.thread)
                            .or_insert_with(|| lane(off.thread, Vec::new()));
                        lane.blocked = off.blocked;
                        lane.preempted = off.preempted;
                    }
                }
                let lanes = lanes.into_values().collect();
                Ok(json(&ProfileTimeline {
                    range: [range.0 - start, range.1 - start],
                    lanes,
                }))
            }
            _ => unreachable!("only profile requests come here"),
        }
    }
}

/// Whether a request is about a profile.
pub const fn is_profile_request(request: &Request) -> bool {
    matches!(
        request,
        Request::Profile(_)
            | Request::ProfileTree(_)
            | Request::ProfileFunctions(_)
            | Request::ProfileTimeline(_)
            | Request::RecordProfile(_)
            | Request::StopRecording(_)
    )
}

/// Fails unless `role` may run programs.
fn controls(role: Role) -> Result<(), Failure> {
    if role == Role::Control {
        Ok(())
    } else {
        Err(Failure::new(
            ErrorKind::Forbidden,
            "this link can only view; ask for a control link to record",
        ))
    }
}

fn summary(entry: &Entry) -> ProfileSummary {
    let profile = &entry.profile;
    let (start, end) = profile.span();
    ProfileSummary {
        id: entry.info.id.clone(),
        name: entry.info.name.clone(),
        program: profile.setting("program").map(str::to_owned),
        unit: profile
            .setting("weight")
            .unwrap_or("nanoseconds")
            .to_owned(),
        samples: profile.samples() as u64,
        weight: profile.total_weight(),
        duration: end - start,
        threads: profile
            .threads()
            .into_iter()
            .map(|thread| protocol::ProfileThread {
                row: thread.row,
                pid: thread.pid,
                tid: thread.tid,
                name: thread.name,
                samples: thread.samples,
                weight: thread.weight,
                blocked: thread.blocked,
                preempted: thread.preempted,
            })
            .collect(),
        lost: profile.lost_samples(),
        throttles: profile.throttles(),
        recorder_cpu: profile.recorder_cpu().0,
        recorder_loading: profile.recorder_cpu().1,
        gaps: profile
            .paused(GapReason::Budget)
            .into_iter()
            .map(|(from, to)| [from - start, to - start])
            .collect(),
        budget_changes: markers(profile.budget_changes(), start),
        tasks: tasks(profile),
        snapshots: profile.task_snapshots().0,
        snapshot_pause: profile.task_snapshots().1,
        task_samples: profile.task_samples(),
        snapshot_problems: markers(profile.task_snapshot_problems(), start),
        waited: profile.task_waits().iter().map(|wait| wait.weight).sum(),
        truncations: profile
            .truncations()
            .into_iter()
            .map(|(what, count)| protocol::ProfileCount { what, count })
            .collect(),
        settings: profile
            .settings()
            .into_iter()
            .map(|(name, value)| ProfileSetting {
                name: name.to_owned(),
                value: value.to_owned(),
            })
            .collect(),
    }
}

/// Markers as a page shows them, from the profile's `start`.
fn markers(markers: Vec<(i64, String)>, start: i64) -> Vec<protocol::ProfileMarker> {
    markers
        .into_iter()
        .map(|(time, text)| protocol::ProfileMarker {
            time: time - start,
            text,
        })
        .collect()
}

/// A profile's tasks, named as views name their groups, most weight
/// first.
fn tasks(profile: &Profile) -> Vec<protocol::ProfileTask> {
    let keys = profile.keys(Granularity::Function);
    let mut tasks: Vec<protocol::ProfileTask> = profile
        .tasks()
        .into_iter()
        .map(|task| protocol::ProfileTask {
            row: task.row,
            name: keys.name(keys.group(task.row)),
            weight: task.weight,
            waited: task.waited,
        })
        .collect();
    tasks.sort_by(|a, b| (b.weight, b.waited, a.row).cmp(&(a.weight, a.waited, b.row)));
    tasks
}

const fn granularity(granularity: ProfileGranularity) -> Granularity {
    match granularity {
        ProfileGranularity::Function => Granularity::Function,
        ProfileGranularity::Physical => Granularity::PhysicalFunction,
        ProfileGranularity::Line => Granularity::Line,
        ProfileGranularity::Address => Granularity::Address,
    }
}

/// The analysis's query for a tab's, its transforms' names resolved to
/// keys. A name no key has is an error rather than a transform that
/// silently does nothing, and so is a focus on a name several keys share.
fn resolve(profile: &Profile, keys: &Keys<'_>, query: &ProfileQuery) -> Result<Query, Failure> {
    let (start, _) = profile.span();
    let mut transforms = Vec::new();
    for transform in &query.transforms {
        let named = keys.named(&transform.name);
        if named.is_empty() {
            return Err(Failure::new(
                ErrorKind::Invalid,
                format!("nothing in the profile is named {}", transform.name),
            ));
        }
        if transform.op == TransformOp::Focus && named.len() > 1 {
            return Err(Failure::new(
                ErrorKind::Invalid,
                format!("{} things are named {}", named.len(), transform.name),
            ));
        }
        transforms.extend(named.into_iter().map(|key| match transform.op {
            TransformOp::Focus => Transform::Focus(key),
            TransformOp::Merge => Transform::Merge(key),
            TransformOp::Drop => Transform::Drop(key),
            TransformOp::Collapse => Transform::CollapseRecursion(key),
        }));
    }
    Ok(Query {
        source: match query.source {
            ProfileSource::Samples => Source::Samples,
            ProfileSource::Waits => Source::Waits,
        },
        range: query
            .range
            .map(|[from, to]| (start.saturating_add(from), start.saturating_add(to))),
        threads: query.threads.clone(),
        tasks: query
            .tasks
            .as_ref()
            .map(|tasks| tasks.iter().map(|task| task.unwrap_or(NO_TASK)).collect()),
        transforms,
        invert: query.invert,
        logical: !query.native,
        by_task: query.by_task,
    })
}

/// The keys an answer names, each once, in the order first named.
struct Named<'a, 'k> {
    keys_of: &'a Keys<'k>,
    indices: BTreeMap<u32, u32>,
    keys: Vec<ProfileKey>,
}

impl<'a, 'k> Named<'a, 'k> {
    const fn new(keys_of: &'a Keys<'k>) -> Self {
        Self {
            keys_of,
            indices: BTreeMap::new(),
            keys: Vec::new(),
        }
    }

    fn index(&mut self, key: u32) -> u32 {
        if let Some(index) = self.indices.get(&key) {
            return *index;
        }
        let keys = self.keys_of;
        let index = u32::try_from(self.keys.len()).unwrap_or(u32::MAX);
        let location = keys.location(key);
        self.keys.push(ProfileKey {
            name: keys.name(key),
            kind: match keys.kind(key) {
                KeyKind::Function { .. } => ProfileKeyKind::Function,
                KeyKind::Line { .. } => ProfileKeyKind::Line,
                KeyKind::Address { .. } => ProfileKeyKind::Address,
                KeyKind::Truncation { .. } => ProfileKeyKind::Truncation,
                KeyKind::Task { .. } => ProfileKeyKind::Task,
                KeyKind::Group { .. } => ProfileKeyKind::Group,
            },
            module: keys.module(key).map(str::to_owned),
            file: location.map(|(file, _)| file.to_owned()),
            line: location.map(|(_, line)| line),
        });
        self.indices.insert(key, index);
        index
    }
}

/// A weight as a page's numbers hold it: exactly up to 2^53, nanoseconds
/// of more than a hundred days, and beyond to within a part in 2^53.
#[expect(
    clippy::cast_precision_loss,
    reason = "no view can show a part in 2^53"
)]
const fn exact(weight: u64) -> f64 {
    weight as f64
}

/// An answer that is only JSON.
fn json(value: &impl serde::Serialize) -> (Value, Vec<u8>) {
    (to_value(value), Vec::new())
}

fn to_value(value: &impl serde::Serialize) -> Value {
    serde_json::to_value(value).expect("answers serialize")
}

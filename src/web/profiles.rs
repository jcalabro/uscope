//! Profiles the server serves to every tab, whatever it debugs. Each is
//! named by a digest of its bytes, so a link to one opens the same profile
//! whenever a server serves it. Every answer is a query of
//! [`uscope::profiler`], which the CLI's reports read too.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

use serde_json::Value;
use tokio::sync::watch;
use uscope::profiler::{Granularity, KeyKind, Keys, Profile, Query, Transform};

use super::protocol::{
    self, ErrorKind, ProfileCost, ProfileFunctions, ProfileGranularity, ProfileInfo, ProfileKey,
    ProfileKeyKind, ProfileLane, ProfileList, ProfileQuery, ProfileSetting, ProfileSummary,
    ProfileTimeline, ProfileTree, Request, TransformOp,
};
use super::session::Failure;

/// The most slices a timeline is cut into.
const MOST_BUCKETS: u32 = 4096;

/// The profiles one server serves.
pub struct Profiles {
    entries: RwLock<BTreeMap<String, Arc<Entry>>>,
    list: watch::Sender<Arc<ProfileList>>,
}

struct Entry {
    info: ProfileInfo,
    profile: Profile,
}

impl Profiles {
    pub fn new() -> Arc<Self> {
        let (list, _) = watch::channel(Arc::new(ProfileList {
            profiles: Vec::new(),
        }));
        Arc::new(Self {
            entries: RwLock::new(BTreeMap::new()),
            list,
        })
    }

    /// Serves the profile in `bytes`, read from `path`, returning its id.
    #[expect(
        clippy::significant_drop_tightening,
        reason = "the list is sent under the lock, so additions publish in order"
    )]
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
        let entry = Arc::new(Entry {
            info: ProfileInfo {
                id: id.clone(),
                name,
            },
            profile: read.profile,
        });
        let mut entries = self.entries.write().expect("profiles lock");
        entries.insert(id.clone(), entry);
        let profiles = entries.values().map(|entry| entry.info.clone()).collect();
        self.list.send_replace(Arc::new(ProfileList { profiles }));
        Ok(id)
    }

    /// The profiles, now and as they change.
    pub fn subscribe(&self) -> watch::Receiver<Arc<ProfileList>> {
        self.list.subscribe()
    }

    /// Answers a request about a profile, with the bytes that go before the
    /// answer. Aggregation reads every sample a query selects, so it runs
    /// off the server's threads.
    pub async fn handle(self: &Arc<Self>, request: Request) -> Result<(Value, Vec<u8>), Failure> {
        let profiles = Arc::clone(self);
        tokio::task::spawn_blocking(move || profiles.answer(request))
            .await
            .map_err(|_| Failure::new(ErrorKind::Failed, "the query failed"))?
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
            Request::Profile(protocol::ProfileRef { profile }) => {
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
                let lanes = keys
                    .density(&resolved, buckets)
                    .into_iter()
                    .map(|(thread, weights)| ProfileLane { thread, weights })
                    .collect();
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
    )
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
            })
            .collect(),
        lost: profile.lost_samples(),
        throttles: profile.throttles(),
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
        range: query
            .range
            .map(|[from, to]| (start.saturating_add(from), start.saturating_add(to))),
        threads: query.threads.clone(),
        transforms,
        invert: query.invert,
        logical: !query.native,
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

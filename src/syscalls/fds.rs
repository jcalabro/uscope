//! What each process's descriptors refer to, which `-y` prints beside
//! each descriptor.
//!
//! A table starts from what the engine reads in `/proc`
//! and follows the calls that open, duplicate, and close descriptors.
//! A descriptor whose origin the trace did not see is unknown, never a
//! guess, and a gap in the trace marks the process's table stale until it
//! is read again.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::model::{Outcome, SyscallEvent, TraceEvent};
use super::sockaddr::{self, SocketAddress};

/// What one descriptor refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Description {
    /// The text `-y` prints between `<` and `>`: a path, `pipe:[88213]`,
    /// or `TCP:[10.0.3.4:51022->10.0.3.9:5432]`.
    pub text: Arc<str>,
    /// What kind it is, for the files and network views.
    pub kind: Kind,
    /// Closed by a successful `execve`.
    pub cloexec: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    File,
    Socket(Socket),
    Pipe,
    Other,
}

/// What the trace has seen of a socket.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Socket {
    pub family: u16,
    /// `SOCK_STREAM` or the like, without its flags.
    pub kind: u32,
    pub local: Option<String>,
    pub remote: Option<String>,
}

impl Socket {
    /// strace's name for the protocol, as in `TCP:[...]`.
    const fn protocol(&self) -> &'static str {
        const STREAM: u32 = 1;
        const DGRAM: u32 = 2;
        match (self.family, self.kind) {
            (sockaddr::AF_INET, STREAM) => "TCP",
            (sockaddr::AF_INET, DGRAM) => "UDP",
            (sockaddr::AF_INET6, STREAM) => "TCPv6",
            (sockaddr::AF_INET6, DGRAM) => "UDPv6",
            (sockaddr::AF_UNIX, _) => "UNIX",
            (sockaddr::AF_NETLINK, _) => "NETLINK",
            _ => "socket",
        }
    }

    fn text(&self) -> String {
        match (&self.local, &self.remote) {
            (None, None) => format!("{}:[]", self.protocol()),
            (local, remote) => format!(
                "{}:[{}{}{}]",
                self.protocol(),
                local.as_deref().unwrap_or(""),
                if remote.is_some() { "->" } else { "" },
                remote.as_deref().unwrap_or("")
            ),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Process {
    fds: BTreeMap<i64, Description>,
    /// A gap may have hidden calls that changed the table.
    stale: bool,
}

/// Every traced process's descriptors.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FdTable {
    processes: BTreeMap<u32, Process>,
}

const O_CLOEXEC: u64 = 0o2_000_000;
const SOCK_CLOEXEC: u64 = 0o2_000_000;
const F_DUPFD: u64 = 0;
const F_DUPFD_CLOEXEC: u64 = 1030;
const F_SETFD: u64 = 2;
const FD_CLOEXEC: u64 = 1;
const CLOSE_RANGE_CLOEXEC: u64 = 4;
const EINPROGRESS: u16 = 115;

impl FdTable {
    /// Records what the engine read from `/proc` for a process.
    pub fn seed(&mut self, process: u32, fds: impl IntoIterator<Item = (i64, Description)>) {
        self.processes.insert(
            process,
            Process {
                fds: fds.into_iter().collect(),
                stale: false,
            },
        );
    }

    /// Records one descriptor, as an engine that read `/proc/PID/fd/N`
    /// right after the call made it does.
    pub fn set(&mut self, process: u32, fd: i64, description: Description) {
        self.processes
            .entry(process)
            .or_default()
            .fds
            .insert(fd, description);
    }

    /// What a descriptor refers to, when the trace knows.
    #[must_use]
    pub fn describe(&self, process: u32, fd: i64) -> Option<&Description> {
        self.processes.get(&process)?.fds.get(&fd)
    }

    /// Processes whose tables a gap made stale, to read again.
    pub fn stale(&self) -> impl Iterator<Item = u32> + '_ {
        self.processes
            .iter()
            .filter(|(_, process)| process.stale)
            .map(|(&pid, _)| pid)
    }

    /// Every descriptor of a process, in order.
    pub fn of(&self, process: u32) -> impl Iterator<Item = (i64, &Description)> {
        self.processes
            .get(&process)
            .into_iter()
            .flat_map(|process| {
                process
                    .fds
                    .iter()
                    .map(|(&fd, description)| (fd, description))
            })
    }

    pub fn apply(&mut self, event: &TraceEvent) {
        match event {
            TraceEvent::Call(call) => self.call(call),
            TraceEvent::Spawn {
                parent,
                child,
                thread: false,
                ..
            } => {
                let copy = self.processes.get(parent).cloned().unwrap_or_default();
                self.processes.insert(*child, copy);
            }
            TraceEvent::Exec { process, .. } => {
                if let Some(table) = self.processes.get_mut(process) {
                    table.fds.retain(|_, description| !description.cloexec);
                }
            }
            TraceEvent::Exit { process, .. } => {
                self.processes.remove(process);
            }
            TraceEvent::Lost { .. } => {
                for process in self.processes.values_mut() {
                    process.stale = true;
                }
            }
            TraceEvent::Spawn { .. } | TraceEvent::Signal { .. } => {}
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one arm per call that changes descriptors"
    )]
    fn call(&mut self, event: &SyscallEvent) {
        let Some(name) = event.call.name() else {
            return;
        };
        // A non-blocking connect that is still in progress has its peer.
        let in_progress =
            name == "connect" && event.outcome == Outcome::Failed(super::model::Errno(EINPROGRESS));
        let result = match event.outcome {
            Outcome::Returned(result) => result,
            _ if in_progress => 0,
            _ => return,
        };
        let process = self.processes.entry(event.process).or_default();
        let a = event.args;
        let fd = |word: u64| super::filter::fd_value(word);
        let string = |arg: u8| {
            event.capture(arg).map(|capture| {
                String::from_utf8_lossy(capture.bytes.split(|&b| b == 0).next().unwrap_or_default())
                    .into_owned()
            })
        };
        let file = |path: Option<String>, cloexec: bool| Description {
            text: path.unwrap_or_else(|| "?".to_owned()).into(),
            kind: Kind::File,
            cloexec,
        };
        let anonymous = |text: &str, cloexec: bool| Description {
            text: text.into(),
            kind: Kind::Other,
            cloexec,
        };
        match name {
            "open" | "creat" => {
                let cloexec = name == "open" && a[1] & O_CLOEXEC != 0;
                process.fds.insert(result, file(string(0), cloexec));
            }
            "openat" => {
                process
                    .fds
                    .insert(result, file(string(1), a[2] & O_CLOEXEC != 0));
            }
            "openat2" | "open_by_handle_at" | "open_tree" | "fspick" | "fsopen" | "fsmount" => {
                let path = if name == "openat2" || name == "open_tree" || name == "fspick" {
                    string(1)
                } else {
                    None
                };
                process.fds.insert(result, file(path, false));
            }
            "close" => {
                process.fds.remove(&fd(a[0]));
            }
            "close_range" => {
                let (first, last) = (a[0], a[1]);
                if a[2] & CLOSE_RANGE_CLOEXEC != 0 {
                    for (&number, description) in &mut process.fds {
                        if (first..=last).contains(&number.unsigned_abs()) {
                            description.cloexec = true;
                        }
                    }
                } else {
                    process
                        .fds
                        .retain(|&number, _| !(first..=last).contains(&number.unsigned_abs()));
                }
            }
            "dup" | "dup2" | "dup3" => {
                let copy = process.fds.get(&fd(a[0])).cloned();
                let cloexec = name == "dup3" && a[2] & O_CLOEXEC != 0;
                match copy {
                    Some(description) => {
                        process.fds.insert(
                            result,
                            Description {
                                cloexec,
                                ..description
                            },
                        );
                    }
                    None => {
                        process.fds.remove(&result);
                    }
                }
            }
            "fcntl" if a[1] == F_DUPFD || a[1] == F_DUPFD_CLOEXEC => {
                let copy = process.fds.get(&fd(a[0])).cloned();
                if let Some(description) = copy {
                    process.fds.insert(
                        result,
                        Description {
                            cloexec: a[1] == F_DUPFD_CLOEXEC,
                            ..description
                        },
                    );
                }
            }
            "fcntl" if a[1] == F_SETFD => {
                if let Some(description) = process.fds.get_mut(&fd(a[0])) {
                    description.cloexec = a[2] & FD_CLOEXEC != 0;
                }
            }
            "socket" => {
                #[expect(clippy::cast_possible_truncation, reason = "the kernel reads ints")]
                let socket = Socket {
                    family: a[0] as u16,
                    kind: (a[1] & 0xf) as u32,
                    ..Socket::default()
                };
                process.fds.insert(
                    result,
                    Description {
                        text: socket.text().into(),
                        kind: Kind::Socket(socket),
                        cloexec: a[1] & SOCK_CLOEXEC != 0,
                    },
                );
            }
            "connect" | "bind" => Self::address(process, fd(a[0]), event, 1, name == "connect"),
            "accept" | "accept4" => {
                let listener = process.fds.get(&fd(a[0])).cloned();
                let mut socket = match listener.map(|description| description.kind) {
                    Some(Kind::Socket(socket)) => Socket {
                        remote: None,
                        ..socket
                    },
                    _ => Socket::default(),
                };
                socket.remote = event
                    .capture(1)
                    .and_then(|capture| SocketAddress::parse(&capture.bytes))
                    .map(|address| address.endpoint());
                let cloexec = name == "accept4" && a[3] & SOCK_CLOEXEC != 0;
                process.fds.insert(
                    result,
                    Description {
                        text: socket.text().into(),
                        kind: Kind::Socket(socket),
                        cloexec,
                    },
                );
            }
            "pipe" | "pipe2" | "socketpair" => {
                let arg = if name == "socketpair" { 3 } else { 0 };
                let cloexec = match name {
                    "pipe2" => a[1] & O_CLOEXEC != 0,
                    "socketpair" => a[1] & SOCK_CLOEXEC != 0,
                    _ => false,
                };
                if let Some(capture) = event.capture(arg)
                    && capture.bytes.len() >= 8
                {
                    for half in capture.bytes[..8].as_chunks::<4>().0 {
                        let number = i64::from(i32::from_le_bytes(*half));
                        let (text, kind) = if name == "socketpair" {
                            (
                                "UNIX:[]",
                                Kind::Socket(Socket {
                                    family: sockaddr::AF_UNIX,
                                    ..Socket::default()
                                }),
                            )
                        } else {
                            ("pipe:[]", Kind::Pipe)
                        };
                        process.fds.insert(
                            number,
                            Description {
                                text: text.into(),
                                kind,
                                cloexec,
                            },
                        );
                    }
                }
            }
            "eventfd" | "eventfd2" => {
                process.fds.insert(
                    result,
                    anonymous(
                        "anon_inode:[eventfd]",
                        name == "eventfd2" && a[1] & O_CLOEXEC != 0,
                    ),
                );
            }
            "epoll_create" | "epoll_create1" => {
                process.fds.insert(
                    result,
                    anonymous(
                        "anon_inode:[eventpoll]",
                        name == "epoll_create1" && a[0] & O_CLOEXEC != 0,
                    ),
                );
            }
            "timerfd_create" => {
                process.fds.insert(
                    result,
                    anonymous("anon_inode:[timerfd]", a[1] & O_CLOEXEC != 0),
                );
            }
            "signalfd" | "signalfd4" => {
                process.fds.insert(
                    result,
                    anonymous(
                        "anon_inode:[signalfd]",
                        name == "signalfd4" && a[3] & O_CLOEXEC != 0,
                    ),
                );
            }
            "inotify_init" | "inotify_init1" => {
                process.fds.insert(
                    result,
                    anonymous(
                        "anon_inode:inotify",
                        name == "inotify_init1" && a[0] & O_CLOEXEC != 0,
                    ),
                );
            }
            "memfd_create" => {
                let text = format!("/memfd:{} (deleted)", string(0).unwrap_or_default());
                process.fds.insert(result, anonymous(&text, a[1] & 1 != 0));
            }
            "pidfd_open" => {
                process
                    .fds
                    .insert(result, anonymous("anon_inode:[pidfd]", true));
            }
            "io_uring_setup" => {
                process
                    .fds
                    .insert(result, anonymous("anon_inode:[io_uring]", false));
            }
            "userfaultfd"
            | "perf_event_open"
            | "fanotify_init"
            | "bpf"
            | "landlock_create_ruleset"
            | "memfd_secret"
            | "pidfd_getfd" => {
                process.fds.remove(&result);
            }
            _ => {}
        }
    }

    fn address(process: &mut Process, fd: i64, event: &SyscallEvent, arg: u8, remote: bool) {
        let Some(address) = event
            .capture(arg)
            .and_then(|capture| SocketAddress::parse(&capture.bytes))
        else {
            return;
        };
        if let Some(description) = process.fds.get_mut(&fd)
            && let Kind::Socket(socket) = &mut description.kind
        {
            if remote {
                socket.remote = Some(address.endpoint());
            } else {
                socket.local = Some(address.endpoint());
            }
            description.text = socket.text().into();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syscalls::model::{Abi, Capture, CaptureState, EventFlags, ReadAt, SyscallId};

    fn call(name: &str, args: &[u64], result: i64, captured: Vec<(u8, &[u8])>) -> TraceEvent {
        let mut words = [0; 6];
        words[..args.len()].copy_from_slice(args);
        TraceEvent::Call(SyscallEvent {
            thread: 7,
            process: 7,
            call: SyscallId::named(Abi::X86_64, name).expect(name),
            args: words,
            captured: captured
                .into_iter()
                .map(|(arg, bytes)| Capture {
                    arg,
                    when: ReadAt::Exit,
                    state: CaptureState::Complete,
                    bytes: bytes.into(),
                })
                .collect(),
            outcome: Outcome::of_return(result),
            entered: Some(1),
            exited: Some(2),
            stack: None,
            flags: EventFlags::default(),
        })
    }

    fn text(table: &FdTable, fd: i64) -> Option<String> {
        table
            .describe(7, fd)
            .map(|description| description.text.to_string())
    }

    #[test]
    fn opens_dups_closes_and_exec_follow_the_kernel() {
        let mut table = FdTable::default();
        table.apply(&call(
            "openat",
            &[u64::MAX - 99, 0, O_CLOEXEC],
            3,
            vec![(1, b"/etc/hosts\0")],
        ));
        table.apply(&call("dup3", &[3, 9, 0], 9, vec![]));
        table.apply(&call(
            "openat",
            &[u64::MAX - 99, 0, 0],
            -2,
            vec![(1, b"/missing\0")],
        ));
        assert_eq!(text(&table, 3).as_deref(), Some("/etc/hosts"));
        assert_eq!(text(&table, 9).as_deref(), Some("/etc/hosts"));
        assert_eq!(text(&table, 4), None);

        table.apply(&TraceEvent::Exec {
            thread: 7,
            process: 7,
            time: 3,
            path: "/bin/true".into(),
        });
        assert_eq!(text(&table, 3), None, "close-on-exec");
        assert_eq!(text(&table, 9).as_deref(), Some("/etc/hosts"));

        table.apply(&call(
            "close_range",
            &[5, u64::from(u32::MAX), 0],
            0,
            vec![],
        ));
        assert_eq!(text(&table, 9), None);
    }

    #[test]
    fn sockets_gather_their_endpoints_and_children_inherit() {
        let mut table = FdTable::default();
        table.apply(&call("socket", &[2, 1, 0], 5, vec![]));
        let address = [2, 0, 0x15, 0x38, 10, 0, 3, 9, 0, 0, 0, 0, 0, 0, 0, 0];
        table.apply(&call("connect", &[5, 0, 16], -111, vec![(1, &address)]));
        assert_eq!(
            text(&table, 5).as_deref(),
            Some("TCP:[]"),
            "a failed call changes nothing"
        );
        table.apply(&call("connect", &[5, 0, 16], -115, vec![(1, &address)]));
        assert_eq!(text(&table, 5).as_deref(), Some("TCP:[->10.0.3.9:5432]"));

        table.apply(&TraceEvent::Spawn {
            parent_thread: 7,
            parent: 7,
            child: 8,
            thread: false,
            time: 4,
        });
        assert_eq!(
            table.describe(8, 5).map(|d| d.text.to_string()).as_deref(),
            Some("TCP:[->10.0.3.9:5432]")
        );

        let fds = [3, 0, 0, 0, 4, 0, 0, 0];
        table.apply(&call("pipe2", &[0, 0], 0, vec![(0, &fds)]));
        assert_eq!(text(&table, 4).as_deref(), Some("pipe:[]"));

        table.apply(&TraceEvent::Lost { time: 5, count: 1 });
        assert_eq!(table.stale().collect::<Vec<_>>(), [7, 8]);
    }
}

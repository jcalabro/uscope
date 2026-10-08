//! uscope attached to the running `server` fixture: its connection's task
//! is listed as a launched program's would be, a request stops at a
//! breakpoint in the handler on that task, and once detached the server
//! answers the request it was stopped in and goes on serving.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

use uscope::{ExecutionContext, StackFrameId, StopContext, StopReason, TaskState};

use crate::invariants::check_tokio_stop;
use crate::steps::stopped_task;
use crate::stops::{integer, line};
use crate::support::{ExternalProcess, Scenario};
use crate::workers::tasks;

const BUILDS: [&str; 2] = ["tokio-server-o0", "tokio-server-o3"];
const SOURCE: &str = "server/src/main.rs";

/// How long a client waits for an answer, which a server the debugger
/// wrongly left stopped never sends.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(60);

/// One connection to the server.
struct Client {
    writer: TcpStream,
    reader: BufReader<TcpStream>,
}

impl Client {
    fn connect(server: &ExternalProcess) -> Self {
        let address = server
            .ready_line()
            .strip_prefix("READY ")
            .expect("the server's address");
        let stream = TcpStream::connect(address).expect("connect to the server");
        stream
            .set_read_timeout(Some(ANSWER_TIMEOUT))
            .expect("a read timeout");
        Self {
            writer: stream.try_clone().expect("a writer"),
            reader: BufReader::new(stream),
        }
    }

    fn send(&mut self, line: &str) {
        writeln!(self.writer, "{line}").expect("send a line");
    }

    fn answer(&mut self) -> String {
        let mut answer = String::new();
        self.reader.read_line(&mut answer).expect("an answer");
        answer.trim_end().to_owned()
    }
}

/// Attached to a server with one connection, the connection's task is
/// the program's only one, and its frames are the handler's. A request
/// sent while the server is stopped stops at the handler's breakpoint on
/// that task, with the count it answers. Detached, the server answers it
/// and the next, and quits when asked.
#[tokio::test]
async fn an_attached_servers_task_is_listed_and_it_serves_once_detached() {
    for fixture in BUILDS {
        let server = ExternalProcess::spawn(&Scenario::fixture(fixture));
        let mut client = Client::connect(&server);
        client.send("first");
        assert_eq!(client.answer(), "first 1", "{fixture}");

        let mut scenario =
            Scenario::attached(fixture, server.attach().await).checking_stops(check_tokio_stop);
        if let Err(problem) = check_tokio_stop(scenario.handle().clone()).await {
            panic!("{fixture}: at the attach: {problem}");
        }
        let (listed, gaps) = tasks(&scenario, 4096).await;
        assert!(gaps.is_empty(), "{fixture}: {gaps:?}");
        let [task] = &listed[..] else {
            panic!("{fixture}: {listed:#?}");
        };
        // Having answered, the handler waits for the next line, or is on
        // its way back to that await.
        assert!(
            matches!(task.state, TaskState::Blocked | TaskState::Running),
            "{fixture}: {task:#?}"
        );
        let stop = scenario.snapshot().await.stop_id.expect("stopped");
        let trace = scenario
            .operation(
                "backtrace",
                scenario
                    .handle()
                    .at(StopContext {
                        stop,
                        execution: ExecutionContext::Task(task.id),
                        frame: StackFrameId::INNERMOST,
                    })
                    .backtrace(),
            )
            .await;
        assert!(
            trace
                .frames
                .iter()
                .any(|frame| frame.function.as_ref().is_some_and(|f| &*f.name == "serve")),
            "{fixture}: {trace:#?}"
        );

        let breakpoint = scenario
            .add_source_breakpoint(SOURCE, line(SOURCE, "// SERVED: answer"))
            .await;
        client.send("second");
        let reason = scenario.resume_to_stop().await;
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{fixture}: {reason:?}"
        );
        assert_eq!(
            stopped_task(&mut scenario).await,
            task.id.number,
            "{fixture}"
        );
        let count = integer(&scenario, "served").await;
        assert!(
            count == Some(2) || count.is_none() && fixture.ends_with("o3"),
            "{fixture}: {count:?}"
        );

        scenario.remove_breakpoint(breakpoint.id).await;
        scenario.shutdown().await;
        assert_eq!(client.answer(), "second 2", "{fixture}");
        client.send("third");
        assert_eq!(client.answer(), "third 3", "{fixture}");
        client.send("quit");
        assert_eq!(server.wait().code(), Some(0), "{fixture}");
    }
}

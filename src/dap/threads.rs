//! The thread ids a client sees, for the debugger's threads and tasks.
//!
//! DAP names threads with 32-bit ids, while a task's number, such as a
//! goroutine id, is 64-bit. A thread keeps its system id, so a client shows
//! what `ps` does; any other context gets the next id above every system
//! thread id, once for the session, so a client keeps its view of a task
//! across stops and never sees an id reused for another.

use std::collections::HashMap;

use uscope::{ExecutionContext, ThreadId};

use super::protocol::ErrorBody;

/// The first id given to a context that is not named by its system thread
/// id. Every Linux thread id is below it (`PID_MAX_LIMIT` is 2^22).
const ALLOCATED: i64 = 1 << 30;
/// The largest id DAP clients accept: thread ids are 32-bit signed.
const MAX_ID: i64 = i32::MAX as i64;

/// Client thread ids for execution contexts, valid for one session.
#[derive(Debug)]
pub struct ThreadHandles {
    next: i64,
    ids: HashMap<ExecutionContext, i64>,
    contexts: HashMap<i64, ExecutionContext>,
}

impl Default for ThreadHandles {
    fn default() -> Self {
        Self {
            next: ALLOCATED,
            ids: HashMap::new(),
            contexts: HashMap::new(),
        }
    }
}

impl ThreadHandles {
    /// The client's id for `context`, given it on first use.
    pub fn id(&mut self, context: ExecutionContext) -> Result<i64, ErrorBody> {
        if let ExecutionContext::Thread(thread) = context
            && let Some(id) = i64::try_from(thread.get())
                .ok()
                .filter(|id| *id < ALLOCATED)
        {
            return Ok(id);
        }
        if let Some(&id) = self.ids.get(&context) {
            return Ok(id);
        }
        if self.next > MAX_ID {
            return Err(ErrorBody::new(format!(
                "{context} cannot be shown: every client thread id is in use"
            )));
        }
        let id = self.next;
        self.next += 1;
        self.ids.insert(context, id);
        self.contexts.insert(id, context);
        Ok(id)
    }

    /// The context a client's thread id names.
    pub fn context(&self, id: i64) -> Result<ExecutionContext, ErrorBody> {
        if (1..ALLOCATED).contains(&id) {
            return Ok(ExecutionContext::Thread(ThreadId::new(id.unsigned_abs())));
        }
        self.contexts
            .get(&id)
            .copied()
            .ok_or_else(|| ErrorBody::new(format!("there is no thread {id}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uscope::{RuntimeId, TaskId};

    #[test]
    fn threads_keep_their_ids_and_others_get_lasting_ones() {
        let mut handles = ThreadHandles::default();
        let thread = ExecutionContext::Thread(ThreadId::new(4321));
        let task = |number| {
            ExecutionContext::Task(TaskId {
                runtime: RuntimeId::new(0),
                number,
            })
        };
        assert_eq!(handles.id(thread), Ok(4321));
        assert_eq!(handles.context(4321), Ok(thread));

        // A goroutine id wider than DAP's ids still gets one, the same each
        // time, and never one a thread could have.
        let wide = handles.id(task(u64::MAX)).expect("an id");
        assert!(wide >= ALLOCATED);
        assert_eq!(handles.id(task(u64::MAX)), Ok(wide));
        assert_eq!(handles.context(wide), Ok(task(u64::MAX)));
        assert_ne!(handles.id(task(1)), Ok(wide));

        let huge = ExecutionContext::Thread(ThreadId::new(u64::MAX));
        let id = handles.id(huge).expect("an id");
        assert_eq!(handles.context(id), Ok(huge));
        assert!(handles.context(0).is_err());
        assert!(handles.context(MAX_ID).is_err());

        handles.next = MAX_ID + 1;
        assert!(handles.id(task(2)).is_err());
    }
}

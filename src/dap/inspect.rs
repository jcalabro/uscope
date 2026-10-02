//! Requests that inspect a stop: threads, stacks, scopes, variables, and
//! expressions.

use serde_json::{Map, Value, json};
use uscope::{
    InspectionLimits, StackFrame, StackFrameId, StopContext, StopReason, UnwindTermination,
    ValueChildQuery, ValueExpression, ValueIndexRange, ValuePathStep, VariableKind, VariableState,
};

use super::handles::{Exhausted, Variables};
use super::protocol::{
    ErrorBody, EvaluateArguments, ExceptionInfoArguments, ScopesArguments, StackTraceArguments,
    VariablesArguments,
};
use super::session::{Session, error, parse, signal_text, thread_id};
use super::values::{self, Item, Options};

/// The most children one `variables` request returns.
const MAX_CHILDREN: u64 = 1024;
/// The most children one debugger request reads.
const PAGE: u64 = 256;

impl Session {
    pub(super) async fn threads(&self) -> Result<Value, ErrorBody> {
        let handle = self.target_handle().ok();
        let snapshot = match &handle {
            Some(handle) => Some(handle.snapshot().await.map_err(error)?),
            None => None,
        };
        let threads = snapshot
            .iter()
            .flat_map(|snapshot| snapshot.threads.iter())
            .map(|thread| {
                json!({
                    "id": thread.id.get(),
                    "name": thread.name.as_deref().map_or_else(
                        || format!("Thread {}", thread.id),
                        |name| format!("{name} ({})", thread.id)
                    ),
                })
            })
            .collect::<Vec<_>>();
        if !threads.is_empty() {
            return Ok(json!({"threads": threads}));
        }
        // Clients need a thread to show, and to pause, even before the
        // program starts.
        let name = handle.map_or_else(
            || "program".to_owned(),
            |handle| {
                handle.executable().file_name().map_or_else(
                    || handle.executable().display().to_string(),
                    |name| name.to_string_lossy().into_owned(),
                )
            },
        );
        Ok(json!({"threads": [{"id": 1, "name": name}]}))
    }

    pub(super) async fn stack_trace(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<StackTraceArguments>(arguments, "stackTrace arguments")?;
        let stop = self.current_stop()?;
        let thread = thread_id(arguments.thread_id)?;
        let trace = self.backtrace(&stop, thread).await?;
        let abnormal = trace.termination != UnwindTermination::Complete;
        let total = trace.frames.len() + usize::from(abnormal);
        let start = usize::try_from(arguments.start_frame.unwrap_or(0)).unwrap_or(0);
        let levels = arguments
            .levels
            .and_then(|levels| usize::try_from(levels).ok())
            .filter(|levels| *levels != 0)
            .unwrap_or(usize::MAX);
        let mut frames = Vec::new();
        for frame in trace.frames.iter().skip(start).take(levels) {
            frames.push(self.stack_frame(stop.id, thread, frame).await?);
        }
        if abnormal && start + frames.len() < total && frames.len() < levels {
            // A stack cut short says so instead of looking complete.
            let id = self.references.label().map_err(exhausted)?;
            frames.push(json!({
                "id": id,
                "name": format!("<backtrace stopped: {}>", trace.termination),
                "line": 0,
                "column": 0,
                "presentationHint": "label",
            }));
        }
        Ok(json!({"stackFrames": frames, "totalFrames": total}))
    }

    async fn stack_frame(
        &mut self,
        stop: uscope::StopId,
        thread: uscope::ThreadId,
        frame: &StackFrame,
    ) -> Result<Value, ErrorBody> {
        let id = self
            .references
            .frame(StopContext {
                stop,
                thread,
                frame: frame.id,
            })
            .map_err(exhausted)?;
        let mut name = if frame.function.is_none() && frame.symbol.is_none() {
            // Code without a name is named by its address and module.
            let module = match frame.module {
                Some(module) => self.image(module).await.and_then(|image| {
                    image
                        .path()
                        .file_name()
                        .map(|name| format!(" in {}", name.to_string_lossy()))
                }),
                None => None,
            };
            format!(
                "{:#x}{}",
                frame.instruction.get(),
                module.unwrap_or_default()
            )
        } else {
            crate::cli::format::code_name(frame.function.as_ref(), frame.symbol.as_ref())
        };
        if frame.kind == uscope::FrameKind::Inline {
            name.push_str(" [inlined]");
        }
        let mut body = json!({
            "id": id,
            "name": name,
            "line": 0,
            "column": 0,
            "instructionPointerReference": format!("{:#x}", frame.instruction.get()),
        });
        if let Some(module) = frame.module {
            body["moduleId"] = module.get().into();
        }
        let source = match (&frame.source, frame.module) {
            (Some(location), Some(module)) => self.image(module).await.and_then(|image| {
                let file = image.source_file(location.file)?;
                Some((self.local_path(&file.path), location.clone()))
            }),
            _ => None,
        };
        match source {
            Some((path, location)) => {
                body["source"] = json!({
                    "name": path.file_name().map(|name| name.to_string_lossy().into_owned()),
                    "path": path.display().to_string(),
                });
                body["line"] = self.line_to_client(location.line.get()).into();
                body["column"] = location.column.map_or(0, uscope::ColumnNumber::get).into();
            }
            None => body["presentationHint"] = "subtle".into(),
        }
        Ok(body)
    }

    pub(super) async fn scopes(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<ScopesArguments>(arguments, "scopes arguments")?;
        let context = self.frame_context(arguments.frame_id)?;
        let mut scopes = Vec::new();
        match self.frame_variables(context).await {
            Ok(snapshot) => {
                let count = |kind| {
                    snapshot
                        .variables
                        .iter()
                        .filter(|variable| variable.kind == kind)
                        .count()
                };
                let parameters = count(VariableKind::Parameter);
                if parameters != 0 {
                    let reference = self
                        .references
                        .variables(Variables::Scope {
                            context,
                            kind: VariableKind::Parameter,
                        })
                        .map_err(exhausted)?;
                    scopes.push(json!({
                        "name": "Arguments",
                        "presentationHint": "arguments",
                        "variablesReference": reference,
                        "namedVariables": parameters,
                        "expensive": false,
                    }));
                }
                let reference = self
                    .references
                    .variables(Variables::Scope {
                        context,
                        kind: VariableKind::Local,
                    })
                    .map_err(exhausted)?;
                scopes.push(json!({
                    "name": "Locals",
                    "presentationHint": "locals",
                    "variablesReference": reference,
                    "namedVariables": count(VariableKind::Local),
                    "expensive": false,
                }));
            }
            Err(error) => scopes.push(json!({
                "name": format!("No variables: {}", error.format),
                "variablesReference": 0,
                "expensive": false,
            })),
        }
        let reference = self
            .references
            .variables(Variables::Registers { context })
            .map_err(exhausted)?;
        scopes.push(json!({
            "name": "Registers",
            "presentationHint": "registers",
            "variablesReference": reference,
            "expensive": true,
        }));
        Ok(json!({"scopes": scopes}))
    }

    fn frame_context(&self, frame: i64) -> Result<StopContext, ErrorBody> {
        self.current_stop()?;
        self.references
            .frame_context(frame)
            .ok_or_else(|| stale("frame", frame))
    }

    pub(super) async fn variables_request(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<VariablesArguments>(arguments, "variables arguments")?;
        self.current_stop()?;
        let variables = self
            .references
            .variables_of(arguments.variables_reference)
            .cloned()
            .ok_or_else(|| stale("variables", arguments.variables_reference))?;
        let options = Options {
            hex: arguments
                .format
                .as_ref()
                .and_then(|format| format.hex)
                .unwrap_or(false),
            ..self.value_options()
        };
        let window = Window {
            start: u64::try_from(arguments.start.unwrap_or(0)).unwrap_or(0),
            count: arguments
                .count
                .and_then(|count| u64::try_from(count).ok())
                .filter(|count| *count != 0)
                .unwrap_or(u64::MAX)
                .min(MAX_CHILDREN),
            options,
        };
        let rows = match variables {
            Variables::Scope { context, kind } => self.scope_rows(context, kind, window).await?,
            Variables::Registers { context } => {
                let handle = self.target_handle()?;
                let registers = handle.at(context).registers().await.map_err(error)?;
                window
                    .slice(registers.registers.iter())
                    .map(|value| values::register(value, registers.target.byte_order))
                    .collect()
            }
            Variables::Children {
                context,
                reference,
                path,
            } => {
                self.children(context, reference, path.as_ref(), window)
                    .await?
            }
            Variables::Pointee {
                context,
                reference,
                name,
                path,
            } => {
                self.pointee_rows(context, reference, &name, path, window)
                    .await?
            }
            Variables::Range {
                context,
                expression,
                range,
            } => self.range_rows(context, &expression, range, window).await?,
        };
        Ok(json!({"variables": rows}))
    }

    /// Presents a frame's parameters or locals.
    async fn scope_rows(
        &mut self,
        context: StopContext,
        kind: VariableKind,
        window: Window,
    ) -> Result<Vec<Map<String, Value>>, ErrorBody> {
        let snapshot = self.frame_variables(context).await?;
        let mut rows = Vec::new();
        for variable in window.slice(
            snapshot
                .variables
                .iter()
                .filter(|variable| variable.kind == kind),
        ) {
            rows.push(self.present(
                Item {
                    name: &variable.name,
                    path: Some(ValueExpression {
                        steps: [ValuePathStep::Named(variable.name.to_string())].into(),
                    }),
                    type_info: variable.type_info.as_ref(),
                    state: &variable.state,
                },
                context,
                window.options,
            )?);
        }
        if kind == VariableKind::Local
            && let Some(exhaustion) = snapshot.completion.exhaustion()
        {
            rows.push(values::truncation(format!(
                "not every variable was read: the {:?} limit is {}",
                exhaustion.resource, exhaustion.limit
            )));
        }
        Ok(rows)
    }

    /// Presents what a pointer points to: an aggregate's members directly,
    /// or else the one value.
    async fn pointee_rows(
        &mut self,
        context: StopContext,
        reference: uscope::DereferenceReference,
        name: &str,
        path: Option<ValueExpression>,
        window: Window,
    ) -> Result<Vec<Map<String, Value>>, ErrorBody> {
        let handle = self.target_handle()?;
        let pointee = handle.dereference(reference).await.map_err(error)?;
        let path = path.map(|path| values::extended(&path, vec![ValuePathStep::Dereference]));
        if let VariableState::Available {
            children: uscope::ValueChildren::Available(children),
            ..
        } = &pointee.state
        {
            return self
                .children(context, children.clone(), path.as_ref(), window)
                .await;
        }
        if window.start != 0 {
            return Ok(Vec::new());
        }
        Ok(vec![self.present(
            Item {
                name: &format!("*{name}"),
                path,
                type_info: Some(&pointee.type_info),
                state: &pointee.state,
            },
            context,
            window.options,
        )?])
    }

    /// Presents a window of an evaluated range's elements.
    async fn range_rows(
        &mut self,
        context: StopContext,
        expression: &ValueExpression,
        range: ValueIndexRange,
        window: Window,
    ) -> Result<Vec<Map<String, Value>>, ErrorBody> {
        let first = range.start.saturating_add(i128::from(window.start));
        let end = range
            .end
            .min(first.saturating_add(i128::from(window.count.min(PAGE * 4))));
        let mut rows = Vec::new();
        if first >= end {
            return Ok(rows);
        }
        let handle = self.target_handle()?;
        let page = handle
            .at(context)
            .inspect_range_with_limits(
                expression.clone(),
                ValueIndexRange { start: first, end },
                InspectionLimits::default(),
            )
            .await
            .map_err(error)?;
        for (offset, child) in page.children.iter().enumerate() {
            let index = first + i128::try_from(offset).unwrap_or(0);
            rows.push(self.present(
                Item {
                    name: &format!("[{index}]"),
                    path: Some(values::extended(
                        expression,
                        vec![ValuePathStep::Index(index)],
                    )),
                    type_info: Some(&child.type_info),
                    state: &child.state,
                },
                context,
                window.options,
            )?);
        }
        if let Some(exhaustion) = page.completion.exhaustion() {
            rows.push(values::truncation(limit_text(exhaustion)));
        }
        Ok(rows)
    }

    /// Presents a window of an aggregate's children, reading them in pages.
    async fn children(
        &mut self,
        context: StopContext,
        reference: std::sync::Arc<uscope::ValueChildrenReference>,
        path: Option<&ValueExpression>,
        window: Window,
    ) -> Result<Vec<Map<String, Value>>, ErrorBody> {
        let handle = self.target_handle()?;
        let end = reference
            .total()
            .min(window.start.saturating_add(window.count));
        let mut rows = Vec::new();
        let mut offset = window.start;
        while offset < end {
            let limit = (end - offset).min(PAGE);
            let page = handle
                .value_children(
                    reference.clone(),
                    ValueChildQuery {
                        offset,
                        limit: u32::try_from(limit).expect("pages are small"),
                    },
                )
                .await
                .map_err(error)?;
            for child in page.children.iter().filter(|child| values::shown(child)) {
                rows.push(self.present(
                    Item {
                        name: &values::child_name(child),
                        path: values::child_path(path, child),
                        type_info: Some(&child.type_info),
                        state: &child.state,
                    },
                    context,
                    window.options,
                )?);
            }
            if let Some(exhaustion) = page.completion.exhaustion() {
                rows.push(values::truncation(limit_text(exhaustion)));
                break;
            }
            if page.children.is_empty() {
                break;
            }
            offset += page.children.len() as u64;
        }
        Ok(rows)
    }

    fn present(
        &mut self,
        item: Item<'_>,
        context: StopContext,
        options: Options,
    ) -> Result<Map<String, Value>, ErrorBody> {
        values::variable(item, context, options, &mut self.references).map_err(exhausted)
    }

    const fn value_options(&self) -> Options {
        let support = self.support();
        Options {
            types: support.variable_type,
            memory: support.memory_references,
            hex: false,
        }
    }

    pub(super) async fn evaluate(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<EvaluateArguments>(arguments, "evaluate arguments")?;
        let context = match arguments.frame_id {
            Some(frame) => Some(self.frame_context(frame)?),
            None => self.stop.as_ref().map(|stop| StopContext {
                stop: stop.id,
                thread: stop.thread,
                frame: StackFrameId::INNERMOST,
            }),
        };
        let expression = arguments.expression.trim();
        if arguments.context.as_deref() == Some("repl")
            && let Some(output) = self.console_command(expression, context).await?
        {
            return Ok(json!({"result": output, "variablesReference": 0}));
        }
        let context = context.ok_or_else(ErrorBody::not_stopped)?;
        let options = Options {
            hex: arguments
                .format
                .as_ref()
                .and_then(|format| format.hex)
                .unwrap_or(false),
            ..self.value_options()
        };
        let parsed = uscope::parse_value_expression(expression).map_err(error)?;
        let handle = self.target_handle()?;
        let mut body = if let Some(range) = parsed.range {
            let length = range.end.saturating_sub(range.start).max(0);
            let reference = self
                .references
                .variables(Variables::Range {
                    context,
                    expression: parsed.expression,
                    range,
                })
                .map_err(exhausted)?;
            let mut body = Map::new();
            body.insert("value".to_owned(), format!("[<{length} elements>]").into());
            body.insert("variablesReference".to_owned(), reference.into());
            body.insert(
                "indexedVariables".to_owned(),
                u64::try_from(length).unwrap_or(0).into(),
            );
            body
        } else {
            let inspected = handle
                .at(context)
                .inspect(parsed.expression.clone())
                .await
                .map_err(error)?;
            self.present(
                Item {
                    name: expression,
                    path: Some(parsed.expression),
                    type_info: inspected.type_info.as_ref(),
                    state: &inspected.state,
                },
                context,
                options,
            )?
        };
        // An evaluation result names its value `result`.
        body.remove("name");
        body.remove("evaluateName");
        let value = body.remove("value").unwrap_or_default();
        body.insert("result".to_owned(), value);
        Ok(Value::Object(body))
    }

    /// Runs a console command in the frame the client focuses, or returns
    /// `None` when the line is no command.
    async fn console_command(
        &self,
        line: &str,
        context: Option<StopContext>,
    ) -> Result<Option<String>, ErrorBody> {
        let console = self.console()?;
        if let Some(context) = context {
            let handle = self.target_handle()?;
            handle.select_thread(context.thread).await.map_err(error)?;
            handle.select_frame(context.frame).await.map_err(error)?;
        }
        console
            .console(line)
            .await
            .map_err(|error| ErrorBody::new(format!("{error:#}")))
    }

    pub(super) fn exception_info(&self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<ExceptionInfoArguments>(arguments, "exceptionInfo arguments")?;
        let stop = self.current_stop()?;
        if thread_id(arguments.thread_id)? != stop.thread {
            return Err(ErrorBody::new(format!(
                "thread {} did not cause the stop",
                arguments.thread_id
            )));
        }
        let (id, description) = match &stop.reason {
            StopReason::Exception(info)
            | StopReason::CoreDump {
                exception: Some(info),
            } => (signal_text(info.code), info.description.to_string()),
            StopReason::CoreDump { exception: None } => (
                "core dump".to_owned(),
                "the core dump records no signal".to_owned(),
            ),
            StopReason::Exec => (
                "exec".to_owned(),
                "the process replaced its executable image, which is not followed".to_owned(),
            ),
            StopReason::Unclassifiable { description } => {
                ("unclassifiable stop".to_owned(), description.to_string())
            }
            StopReason::WatchpointArmFailed { description, .. } => {
                ("watchpoint failure".to_owned(), description.to_string())
            }
            _ => return Err(ErrorBody::new("the stop was not caused by an exception")),
        };
        Ok(json!({
            "exceptionId": id,
            "description": description,
            "breakMode": "always",
            "details": {"message": description, "typeName": id},
        }))
    }
}

/// The part of a list of variables a request asks for, and how to show it.
#[derive(Clone, Copy)]
struct Window {
    start: u64,
    count: u64,
    options: Options,
}

impl Window {
    fn slice<T>(self, items: impl Iterator<Item = T>) -> impl Iterator<Item = T> {
        items
            .skip(usize::try_from(self.start).unwrap_or(usize::MAX))
            .take(usize::try_from(self.count).unwrap_or(usize::MAX))
    }
}

fn stale(kind: &str, id: i64) -> ErrorBody {
    ErrorBody::new(format!(
        "{kind} reference {id} is stale: it belongs to an earlier stop, or never existed"
    ))
}

fn limit_text(exhaustion: uscope::InspectionExhaustion) -> String {
    format!(
        "inspection stopped at its {:?} limit of {}",
        exhaustion.resource, exhaustion.limit
    )
}

fn exhausted(exhausted: Exhausted) -> ErrorBody {
    ErrorBody::new(exhausted.to_string())
}

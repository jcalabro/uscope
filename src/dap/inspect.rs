//! Requests that inspect a stop: threads, stacks, scopes, variables, and
//! expressions.

use std::fmt::Write as _;
use std::sync::Arc;

use serde_json::{Map, Value, json};
use uscope::{
    InspectionLimits, StackFrame, StackFrameId, StopContext, StopReason, UnwindTermination,
    ValueChildQuery, VariableKind, VariableState,
};

use super::handles::{Exhausted, Location, Variables};
use super::protocol::{
    CompletionsArguments, ErrorBody, EvaluateArguments, ExceptionInfoArguments, LocationsArguments,
    ScopesArguments, SetExpressionArguments, SetVariableArguments, StackFrameFormat,
    StackTraceArguments, VariablesArguments,
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
        let format = arguments.format.unwrap_or_default();
        let mut frames = Vec::new();
        for frame in trace.frames.iter().skip(start).take(levels) {
            let mut body = self.stack_frame(stop.id, thread, frame).await?;
            self.decorate(
                &mut body,
                frame,
                &format,
                StopContext {
                    stop: stop.id,
                    thread,
                    frame: frame.id,
                },
            )
            .await;
            frames.push(body);
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
            body["moduleId"] = module.get().to_string().into();
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
                body["column"] = location
                    .column
                    .map_or(0, |column| self.column_to_client(column.get()))
                    .into();
            }
            None => body["presentationHint"] = "subtle".into(),
        }
        Ok(body)
    }

    /// Adds what a client's frame format asks for to a frame's name: its
    /// parameters, line, and module.
    async fn decorate(
        &mut self,
        body: &mut Value,
        frame: &StackFrame,
        format: &StackFrameFormat,
        context: StopContext,
    ) {
        let mut name = body["name"].as_str().unwrap_or_default().to_owned();
        if format.parameters == Some(true) {
            let names = format.parameter_names.unwrap_or(true);
            let types = format.parameter_types.unwrap_or(false);
            let values = format.parameter_values.unwrap_or(true);
            let hex = format.hex.unwrap_or(self.display.hex);
            let parameters = self.frame_variables(context).await.map_or_else(
                |_| Vec::new(),
                |snapshot| {
                    snapshot
                        .variables
                        .iter()
                        .filter(|variable| variable.kind == VariableKind::Parameter)
                        .map(|variable| {
                            let mut text = String::new();
                            if types {
                                text.push_str(
                                    variable.type_info.as_ref().map_or("?", |info| &info.name),
                                );
                                text.push(' ');
                            }
                            if names {
                                text.push_str(&variable.name);
                            }
                            if values {
                                if names {
                                    text.push_str(" = ");
                                }
                                text.push_str(&values::text(
                                    variable.type_info.as_ref(),
                                    &variable.state,
                                    hex,
                                ));
                            }
                            text.trim().to_owned()
                        })
                        .collect::<Vec<_>>()
                },
            );
            name = format!("{name}({})", parameters.join(", "));
        }
        if format.line == Some(true) && body["line"].as_u64().is_some_and(|line| line != 0) {
            name = format!("{name} Line {}", body["line"]);
        }
        if format.module == Some(true)
            && let Some(module) = frame.module
            && let Some(image) = self.image(module).await
            && let Some(file) = image.path().file_name()
        {
            name = format!("{name} [{}]", file.to_string_lossy());
        }
        body["name"] = name.into();
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
        if let Some((module, file)) = self.frame_file(context).await {
            let reference = self
                .references
                .variables(Variables::Statics {
                    context,
                    module,
                    file,
                })
                .map_err(exhausted)?;
            scopes.push(json!({
                "name": "Statics",
                "variablesReference": reference,
                "expensive": true,
            }));
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
                .unwrap_or(self.display.hex),
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
        // A client pages by the count the list was given, so the other
        // kind of row has none to show.
        let wanted = match arguments.filter.as_deref() {
            Some("indexed") => Some(true),
            Some("named") => Some(false),
            _ => None,
        };
        if wanted.is_some_and(|wanted| variables.indexed() == Some(!wanted)) {
            return Ok(json!({"variables": []}));
        }
        let list = arguments.variables_reference;
        let context = variables.context();
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
            Variables::Statics {
                context,
                module,
                file,
            } => self.static_rows(context, module, file, window).await?,
            Variables::Children {
                context,
                reference,
                path,
                ..
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
            } => self.range_rows(context, &expression, window).await?,
        };
        // Data breakpoints name rows by their list and name.
        for row in &rows {
            if let (Some(name), Some(path)) = (
                row.get("name").and_then(Value::as_str),
                row.get("evaluateName")
                    .and_then(Value::as_str)
                    .and_then(super::watch::child_expression),
            ) {
                self.references
                    .record_path(list, name.to_owned(), context, path);
            }
        }
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
        let module = self.frame_file(context).await.map(|(module, _)| module);
        let unnamed = self.unnamed_variables(context, &snapshot).await;
        let mut rows = Vec::new();
        for (index, variable) in window.slice(
            snapshot
                .variables
                .iter()
                .enumerate()
                .filter(|(_, variable)| variable.kind == kind),
        ) {
            let path = if unnamed.contains(&index) {
                None
            } else {
                uscope::Expression::name(&variable.name)
            };
            rows.push(self.present(
                Item {
                    name: &variable.name,
                    path,
                    type_info: variable.type_info.as_ref(),
                    state: &variable.state,
                    declaration: module.zip(variable.declaration.clone()),
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

    /// The frame's variables their names do not reach, because another of
    /// the frame's variables has the same name, such as one an inner block
    /// hides: all but the one the name binds, told apart by their storage,
    /// or all of them when their storage cannot tell.
    async fn unnamed_variables(
        &self,
        context: StopContext,
        snapshot: &uscope::VariableSnapshot,
    ) -> std::collections::BTreeSet<usize> {
        let storage = |state: &VariableState| match state {
            VariableState::Available {
                source:
                    source @ (uscope::VariableValueSource::Memory(_)
                    | uscope::VariableValueSource::Register(_)),
                ..
            } => Some(source.clone()),
            _ => None,
        };
        let mut by_name = std::collections::BTreeMap::<&str, Vec<usize>>::new();
        for (index, variable) in snapshot.variables.iter().enumerate() {
            by_name.entry(&variable.name).or_default().push(index);
        }
        let mut unnamed = std::collections::BTreeSet::new();
        for (name, indices) in by_name.into_iter().filter(|(_, indices)| indices.len() > 1) {
            let bound = if let (Ok(handle), Some(expression)) =
                (self.target_handle(), uscope::Expression::name(name))
                && let Ok(uscope::Evaluation::Value { value, .. }) =
                    handle.at(context).evaluate(&expression).await
            {
                storage(&value.state)
            } else {
                None
            };
            let matching = indices
                .iter()
                .copied()
                .filter(|index| {
                    bound.is_some() && storage(&snapshot.variables[*index].state) == bound
                })
                .collect::<Vec<_>>();
            for index in indices {
                if matching != [index] {
                    unnamed.insert(index);
                }
            }
        }
        unnamed
    }

    /// The module and source file of a frame's location, when it has one.
    async fn frame_file(
        &mut self,
        context: StopContext,
    ) -> Option<(uscope::ModuleId, uscope::SourceFileId)> {
        let stop = self.current_stop().ok()?;
        let trace = self.backtrace(&stop, context.thread).await.ok()?;
        let frame = trace
            .frames
            .iter()
            .find(|frame| frame.id == context.frame)?;
        Some((frame.module?, frame.source.as_ref()?.file))
    }

    /// Presents the static variables declared in a frame's source file, as
    /// the frame's thread sees them.
    async fn static_rows(
        &mut self,
        context: StopContext,
        module: uscope::ModuleId,
        file: uscope::SourceFileId,
        window: Window,
    ) -> Result<Vec<Map<String, Value>>, ErrorBody> {
        let image = self
            .image(module)
            .await
            .ok_or_else(|| ErrorBody::new("the frame's module is no longer loaded"))?;
        let handle = self.target_handle()?;
        let mut rows = Vec::new();
        let declared = image.globals().iter().filter(|global| {
            global
                .declaration
                .as_ref()
                .is_some_and(|declaration| declaration.file == file)
        });
        for global in window.slice(declared) {
            let variable = handle
                .at(context)
                .global_with_limits(
                    uscope::GlobalVariableReference {
                        module,
                        image: image.id(),
                        variable: global.id,
                    },
                    InspectionLimits::default(),
                )
                .await
                .map_err(error)?;
            rows.push(
                self.present(
                    Item {
                        name: &variable.name,
                        path: global_expression(&image, global),
                        type_info: variable.type_info.as_ref(),
                        state: &variable.state,
                        declaration: global
                            .declaration
                            .clone()
                            .map(|declaration| (module, declaration)),
                    },
                    context,
                    window.options,
                )?,
            );
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
        path: Option<uscope::Expression>,
        window: Window,
    ) -> Result<Vec<Map<String, Value>>, ErrorBody> {
        let handle = self.target_handle()?;
        let pointee = handle.dereference(reference).await.map_err(error)?;
        let path = path.and_then(|path| path.dereferenced());
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
                declaration: None,
            },
            context,
            window.options,
        )?])
    }

    /// Presents a window of an evaluated range's elements.
    async fn range_rows(
        &mut self,
        context: StopContext,
        expression: &uscope::Expression,
        window: Window,
    ) -> Result<Vec<Map<String, Value>>, ErrorBody> {
        let handle = self.target_handle()?;
        let evaluation = handle
            .at(context)
            .evaluate(expression)
            .await
            .map_err(error)?;
        let uscope::Evaluation::Range(page) = evaluation else {
            return Err(ErrorBody::new("the range no longer evaluates to elements"));
        };
        let base = expression.range_base();
        let mut rows = Vec::new();
        let start = usize::try_from(window.start).unwrap_or(usize::MAX);
        let count = usize::try_from(window.count).unwrap_or(usize::MAX);
        for child in page.children.iter().skip(start).take(count) {
            let name = match &child.relationship {
                uscope::ValueChildRelationship::ArrayElement { indices, .. } => {
                    indices.iter().fold(String::new(), |mut name, index| {
                        let _ = write!(name, "[{index}]");
                        name
                    })
                }
                uscope::ValueChildRelationship::SliceElement { index } => format!("[{index}]"),
                _ => "?".to_owned(),
            };
            rows.push(self.present(
                Item {
                    name: &name,
                    path: values::child_path(base.as_ref(), child),
                    type_info: Some(&child.type_info),
                    state: &child.state,
                    declaration: None,
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
        path: Option<&uscope::Expression>,
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
                        declaration: None,
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
        let code = self.code();
        values::variable(item, context, options, &mut self.references, &code).map_err(exhausted)
    }

    const fn value_options(&self) -> Options {
        let support = self.support();
        Options {
            types: support.variable_type,
            memory: support.memory_references,
            hex: self.display.hex,
        }
    }

    /// Evaluates an expression for a watch, a hover, the clipboard, or the
    /// debug console. A console line is a command when it starts with a
    /// command's name, unless it is an expression whose first name the frame
    /// knows, so variables such as `x`, `n`, or `list` read as themselves.
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
        let repl = arguments.context.as_deref() == Some("repl");
        let command = repl
            .then(|| crate::cli::commands::line_command(expression))
            .flatten();
        let parsed = match (uscope::Expression::parse(expression), command) {
            (Ok(parsed), _) => parsed,
            (Err(_), Some(_)) => return self.console_line(expression, context).await,
            (Err(failure), None) => return Err(expression_failure(expression, &failure, repl)),
        };
        let Some(context) = context else {
            if command.is_some() {
                return self.console_line(expression, None).await;
            }
            return Err(ErrorBody::not_stopped());
        };
        let options = Options {
            hex: arguments
                .format
                .as_ref()
                .and_then(|format| format.hex)
                .unwrap_or(self.display.hex),
            ..self.value_options()
        };
        let handle = self.target_handle()?;
        let mode = if repl {
            uscope::EvaluationMode::Assign
        } else {
            uscope::EvaluationMode::Read
        };
        let evaluation = match handle
            .at(context)
            .evaluate_with(&parsed, mode, uscope::InspectionLimits::default())
            .await
        {
            Ok(evaluation) => evaluation,
            // The frame does not know the command's name, so it is the command.
            Err(uscope::Error::Expression(failure))
                if command.is_some_and(|(_, name)| names_only(&failure, name)) =>
            {
                return self.console_line(expression, Some(context)).await;
            }
            Err(uscope::Error::Expression(failure)) => {
                return Err(expression_failure(expression, &failure, repl));
            }
            Err(other) => return Err(error(other)),
        };
        if parsed.assignment_target().is_some() {
            self.forget_reads();
            self.invalidate_values().await;
        }
        let mut body = match evaluation {
            uscope::Evaluation::Value { value, .. } => self.present(
                Item {
                    name: expression,
                    path: Some(parsed),
                    type_info: value.type_info.as_ref(),
                    state: &value.state,
                    declaration: None,
                },
                context,
                options,
            )?,
            uscope::Evaluation::Range(page) => {
                let length = page.children.len();
                let reference = self
                    .references
                    .variables(Variables::Range {
                        context,
                        expression: parsed,
                    })
                    .map_err(exhausted)?;
                let mut body = Map::new();
                body.insert("value".to_owned(), format!("[<{length} elements>]").into());
                body.insert("variablesReference".to_owned(), reference.into());
                body.insert("indexedVariables".to_owned(), length.into());
                body
            }
            _ => {
                return Err(ErrorBody::new(
                    "the evaluation produced an unknown kind of result",
                ));
            }
        };
        // An evaluation result names its value `result`.
        body.remove("name");
        body.remove("evaluateName");
        let value = body.remove("value").unwrap_or_default();
        body.insert("result".to_owned(), value);
        Ok(Value::Object(body))
    }

    /// Runs a console command in the frame the client focuses, as the
    /// evaluation of its line.
    async fn console_line(
        &mut self,
        line: &str,
        context: Option<StopContext>,
    ) -> Result<Value, ErrorBody> {
        let console = self.console()?;
        if let Some(context) = context {
            let handle = self.target_handle()?;
            handle.select_thread(context.thread).await.map_err(error)?;
            handle.select_frame(context.frame).await.map_err(error)?;
        }
        let output = console
            .console(line)
            .await
            .map_err(|error| ErrorBody::new(format!("{error:#}")))?
            .ok_or_else(|| ErrorBody::new(format!("'{line}' is not a command")))?;
        // `set` changes values the client shows.
        if crate::cli::commands::line_command(line)
            .is_some_and(|(spec, _)| spec.command == crate::cli::commands::Command::Set)
        {
            self.forget_reads();
            self.invalidate_values().await;
        }
        Ok(json!({"result": output, "variablesReference": 0}))
    }

    /// Where a location reference leads: where a variable is declared, or
    /// where the function a pointer enters is declared, or else the line
    /// of its first instruction.
    pub(super) fn locations(&self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<LocationsArguments>(arguments, "locations arguments")?;
        let reference = arguments.location_reference;
        let (image, location) = match self
            .references
            .location_of(reference)
            .ok_or_else(|| stale("location", reference))?
        {
            Location::Code(address) => {
                let code = self.code();
                let (image, address) = code.function_entry(*address).ok_or_else(|| {
                    ErrorBody::new(format!("no loaded module has code at {address:#x}"))
                })?;
                let located = image.locate(address);
                let location = located
                    .function
                    .and_then(|function| function.declaration)
                    .or(located.source)
                    .ok_or_else(|| {
                        ErrorBody::new(format!(
                            "the code at {:#x} has no source location",
                            address.get()
                        ))
                    })?;
                (Arc::clone(image), location)
            }
            Location::Declared { module, location } => (
                self.loaded_image(*module)
                    .ok_or_else(|| ErrorBody::new("the module is no longer loaded"))?,
                location.clone(),
            ),
        };
        let file = image
            .source_file(location.file)
            .ok_or_else(|| ErrorBody::new("the source file is missing from its module"))?;
        let path = self.local_path(&file.path);
        let mut body = json!({
            "source": super::sources::source_json(&path),
            "line": self.line_to_client(location.line.get()),
        });
        if let Some(column) = location.column {
            body["column"] = self.column_to_client(column.get()).into();
        }
        Ok(body)
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
            StopReason::Exec { followed: true } => (
                "exec".to_owned(),
                "the process executed its program again".to_owned(),
            ),
            StopReason::Exec { followed: false } => (
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

impl Session {
    pub(super) async fn set_variable(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<SetVariableArguments>(arguments, "setVariable arguments")?;
        self.current_stop()?;
        let (context, path) = self
            .references
            .child_path(arguments.variables_reference, &arguments.name)
            .ok_or_else(|| {
                ErrorBody::new(format!(
                    "{} cannot be changed: it has no name the debugger can evaluate",
                    arguments.name
                ))
            })?;
        let hex = arguments
            .format
            .and_then(|format| format.hex)
            .unwrap_or(self.display.hex);
        self.assign(context, &arguments.name, path, &arguments.value, hex)
            .await
    }

    pub(super) async fn set_expression(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<SetExpressionArguments>(arguments, "setExpression arguments")?;
        let stop = self.current_stop()?;
        let context = match arguments.frame_id {
            Some(frame) => self.frame_context(frame)?,
            None => StopContext {
                stop: stop.id,
                thread: stop.thread,
                frame: StackFrameId::INNERMOST,
            },
        };
        let parsed = uscope::Expression::parse(arguments.expression.trim())
            .map_err(|failure| error(uscope::Error::Expression(failure)))?;
        let hex = arguments
            .format
            .and_then(|format| format.hex)
            .unwrap_or(self.display.hex);
        self.assign(
            context,
            arguments.expression.trim(),
            parsed,
            &arguments.value,
            hex,
        )
        .await
    }

    /// Assigns a value and presents the result as both requests answer.
    async fn assign(
        &mut self,
        context: StopContext,
        name: &str,
        path: uscope::Expression,
        value: &str,
        hex: bool,
    ) -> Result<Value, ErrorBody> {
        let handle = self.target_handle()?;
        let assignment = uscope::Expression::parse(&format!("{path} = {value}"))
            .map_err(|failure| error(uscope::Error::Expression(failure)))?;
        let evaluation = handle
            .at(context)
            .evaluate_with(
                &assignment,
                uscope::EvaluationMode::Assign,
                uscope::InspectionLimits::default(),
            )
            .await
            .map_err(error)?;
        let uscope::Evaluation::Value {
            value: assigned, ..
        } = evaluation
        else {
            return Err(ErrorBody::new("the assignment produced no value"));
        };
        self.forget_reads();
        let options = Options {
            hex,
            ..self.value_options()
        };
        let mut body = self.present(
            Item {
                name,
                path: Some(path),
                type_info: assigned.type_info.as_ref(),
                state: &assigned.state,
                declaration: None,
            },
            context,
            options,
        )?;
        for key in ["name", "evaluateName", "presentationHint"] {
            body.remove(key);
        }
        self.invalidate_values().await;
        Ok(Value::Object(body))
    }

    pub(super) async fn completions(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<CompletionsArguments>(arguments, "completions arguments")?;
        // Columns count characters; a column past the text completes all of it.
        let column = usize::try_from(arguments.column).unwrap_or(0);
        let offset = if self.support().columns_start_at1 {
            column.saturating_sub(1)
        } else {
            column
        };
        let typed = arguments.text.chars().take(offset).collect::<String>();
        let word_start = typed
            .rfind(|character: char| character.is_whitespace())
            .map_or(0, |index| index + 1);
        let word = &typed[word_start..];
        let first_word = !typed[..word_start]
            .chars()
            .any(|character| !character.is_whitespace());
        let command = typed.split_whitespace().next().unwrap_or_default();
        let mut candidates = Vec::new();
        if first_word {
            for spec in crate::cli::commands::COMMANDS {
                candidates.push((spec.name.to_owned(), "keyword"));
            }
        } else if command == "info" {
            for subcommand in ["breakpoints", "watchpoints", "signals", "core", "symbol"] {
                candidates.push((subcommand.to_owned(), "value"));
            }
        } else if command == "handle" {
            for code in uscope::signal_codes() {
                if let Some(name) = uscope::signal_name(code) {
                    candidates.push((name, "value"));
                }
            }
        }
        let context = match arguments.frame_id {
            Some(frame) => self.references.frame_context(frame),
            None => self.stop.as_ref().map(|stop| StopContext {
                stop: stop.id,
                thread: stop.thread,
                frame: StackFrameId::INNERMOST,
            }),
        };
        if command != "info"
            && command != "handle"
            && let Some(context) = context
            && let Ok(snapshot) = self.frame_variables(context).await
        {
            for variable in snapshot.variables.iter() {
                candidates.push((variable.name.to_string(), "variable"));
            }
        }
        let start = u64::try_from(typed[..word_start].chars().count()).unwrap_or(0)
            + u64::from(self.support().columns_start_at1);
        let mut seen = std::collections::BTreeSet::new();
        let targets = candidates
            .into_iter()
            .filter(|(label, _)| label.starts_with(word) && seen.insert(label.clone()))
            .map(|(label, kind)| {
                json!({
                    "label": label,
                    "type": kind,
                    "start": start,
                    "length": word.chars().count(),
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({"targets": targets}))
    }
}

/// A name that reaches a global from any frame, which the frame's own
/// locals cannot shadow: its qualified name, or, when other files declare
/// the same, that name qualified by its file's name or path.
fn global_expression(
    image: &uscope::ModuleImage,
    global: &uscope::GlobalVariableInfo,
) -> Option<uscope::Expression> {
    let mut selectors = vec![global.qualified_name.to_string()];
    if let Some(file) = global
        .declaration
        .as_ref()
        .and_then(|declaration| image.source_file(declaration.file))
    {
        if let Some(name) = file.path.file_name() {
            selectors.push(format!(
                "{}::{}",
                name.to_string_lossy(),
                global.qualified_name
            ));
        }
        selectors.push(format!(
            "{}::{}",
            file.path.display(),
            global.qualified_name
        ));
    }
    selectors
        .into_iter()
        .find(|selector| {
            image
                .global_named(selector)
                .is_ok_and(|found| found.id == global.id)
        })
        .and_then(|selector| uscope::Expression::outermost(&selector))
}

/// Whether an evaluation failed only because the frame does not know the
/// name that starts the text.
fn names_only(failure: &uscope::ExpressionError, name: &str) -> bool {
    failure.kind == uscope::ExpressionErrorKind::UnknownName
        && failure.span.start == 0
        && failure.span.end as usize == name.len()
}

/// An expression's mistake: in the console, pointing at the text it is
/// about, as the terminal debugger prints it; elsewhere, on one line.
fn expression_failure(text: &str, failure: &uscope::ExpressionError, console: bool) -> ErrorBody {
    if console {
        ErrorBody::new(crate::cli::format::expression_error(text, failure))
    } else {
        error(uscope::Error::Expression(failure.clone()))
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

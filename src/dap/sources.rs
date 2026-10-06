//! Modules, source files, and the lines breakpoints can use.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::{Value, json};
use uscope::{LineNumber, LoadedModuleRecord, ModuleImage};

use super::protocol::{BreakpointLocationsArguments, ErrorBody, ModulesArguments};
use super::session::{Closed, Session, error, parse};

impl Session {
    pub(super) async fn modules(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments = parse::<ModulesArguments>(arguments, "modules arguments")?;
        let handle = self.target_handle()?;
        let records = match handle.loaded_modules().await {
            Ok(snapshot) => snapshot.modules.to_vec(),
            Err(uscope::Error::NotRunning) => Vec::new(),
            Err(other) => return Err(error(other)),
        };
        let start = usize::try_from(arguments.start_module.unwrap_or(0)).unwrap_or(0);
        let count = arguments
            .module_count
            .and_then(|count| usize::try_from(count).ok())
            .filter(|count| *count != 0)
            .unwrap_or(usize::MAX);
        let mut modules = Vec::new();
        for record in records.iter().skip(start).take(count) {
            let image = self.image(record.module.id).await;
            modules.push(module_json(record, image.as_deref()));
        }
        Ok(json!({"modules": modules, "totalModules": records.len()}))
    }

    /// Brings the client's modules up to date: announces the loaded ones it
    /// has not heard about, such as the program itself, which no load event
    /// reports, and removes those unloaded while events were missed.
    pub(super) async fn announce_modules(&mut self) -> Result<(), Closed> {
        let Some(handle) = self.target_handle().ok() else {
            return Ok(());
        };
        let Ok(snapshot) = handle.loaded_modules().await else {
            return Ok(());
        };
        let loaded = snapshot
            .modules
            .iter()
            .map(|record| record.module.id)
            .collect::<std::collections::BTreeSet<_>>();
        let unloaded = self
            .modules
            .keys()
            .filter(|id| !loaded.contains(id))
            .copied()
            .collect::<Vec<_>>();
        for id in unloaded {
            self.forget_module(id).await?;
        }
        for record in snapshot.modules.iter() {
            self.announce_module(record).await?;
        }
        Ok(())
    }

    pub(super) async fn announce_module(
        &mut self,
        record: &LoadedModuleRecord,
    ) -> Result<(), Closed> {
        if self.modules.contains_key(&record.module.id) {
            return Ok(());
        }
        self.modules.insert(record.module.id, record.clone());
        let image = self.image(record.module.id).await;
        self.client
            .event(
                "module",
                json!({"reason": "new", "module": module_json(record, image.as_deref())}),
            )
            .await?;
        // What kept a library's own views out, once, when it loads; the
        // program's are said when the session starts.
        if let (Some(image), Ok(handle)) = (&image, self.target_handle())
            && image.id() != handle.module_image().id()
        {
            for error in image.view_errors() {
                self.client.important(format!("views: {error}")).await?;
            }
        }
        Ok(())
    }

    pub(super) async fn loaded_sources(&mut self) -> Result<Value, ErrorBody> {
        let handle = self.target_handle()?;
        let mut images = vec![std::sync::Arc::clone(handle.module_image())];
        if let Ok(snapshot) = handle.loaded_modules().await {
            for record in snapshot.modules.iter() {
                if let Some(image) = self.image(record.module.id).await
                    && image.id() != handle.module_image().id()
                {
                    images.push(image);
                }
            }
        }
        let mut paths = BTreeSet::new();
        for image in &images {
            for file in image.source_files() {
                paths.insert(self.local_path(&file.path));
            }
        }
        Ok(json!({"sources": paths.iter().map(|path| source_json(path)).collect::<Vec<_>>()}))
    }

    pub(super) fn breakpoint_locations(&mut self, arguments: Value) -> Result<Value, ErrorBody> {
        let arguments =
            parse::<BreakpointLocationsArguments>(arguments, "breakpointLocations arguments")?;
        let handle = self.target_handle()?;
        let path = arguments
            .source
            .path
            .ok_or_else(|| ErrorBody::new("the source has no path"))?;
        let recorded = self.recorded_path(std::path::Path::new(&path));
        let image = handle.module_image();
        let Ok(file) = image.source_file_matching(&recorded) else {
            return Ok(json!({"breakpoints": []}));
        };
        let line = |line: i64| self.line_from_client(line).and_then(LineNumber::new);
        let (Some(first), Some(last)) = (
            line(arguments.line),
            line(arguments.end_line.unwrap_or(arguments.line)),
        ) else {
            return Ok(json!({"breakpoints": []}));
        };
        let lines = image
            .breakpoint_lines(file.id, first..=last)
            .map(|line| json!({"line": self.line_to_client(line.get())}))
            .collect::<Vec<_>>();
        Ok(json!({"breakpoints": lines}))
    }
}

pub(super) fn module_json(record: &LoadedModuleRecord, image: Option<&ModuleImage>) -> Value {
    let mut module = json!({
        "id": record.module.id.get().to_string(),
        "name": record
            .path
            .file_name()
            .map_or_else(|| record.path.display().to_string(), |name| name.to_string_lossy().into_owned()),
        "path": record.path.display().to_string(),
    });
    if let Some(image) = image {
        module["symbolStatus"] = if !image.functions().is_empty() {
            "debug information loaded"
        } else if !image.symbols().is_empty() {
            "symbols only, no debug information"
        } else {
            "no symbols"
        }
        .into();
    }
    module
}

pub fn source_json(path: &Path) -> Value {
    json!({
        "name": path.file_name().map(|name| name.to_string_lossy().into_owned()),
        "path": path.display().to_string(),
    })
}

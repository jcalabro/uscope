//! The calls a module describes (DWARF 5 section 3.4.1), which recover the
//! values a function's parameters held on entry from the call that entered
//! it, and the tail calls that may have entered it since.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use gimli::{Location, Value};

use crate::debug_info::dwarf::{DieKey, DwarfError, Reader, die_reference};
use crate::debug_info::{
    CallSite, CallSiteId, CallTarget, EntryParameter, VariableRuntime, VariableRuntimeError,
};
use crate::{
    AddressRange, EntryValueUnavailableReason, ImageAddress, VariableUnavailableReason,
    VirtualAddress,
};

use super::codec::bytes_to_u64;
use super::die::{flag_with_origins, origin_chain, string_with_origins};
use super::evaluate::{EvaluateError, FrameBase, FrameBaseCache, FrameBaseContext, evaluate};
use super::location::{Expression, LocationDescription, copy_expression, copy_optional_location};
use super::{DwarfVariableInfo, InspectionBudget, Metadata, MetadataAbsence};

/// How many functions a search for tail calls may visit.
const MAX_TAIL_CALL_FUNCTIONS: usize = 4_096;

/// Every call site of a module, and what each function says of its calls.
pub(super) struct CallSiteCatalog {
    sites: Vec<CatalogCallSite>,
    /// One per cataloged function, in the same order.
    functions: Vec<CallingFunction>,
    /// The calls that return to each address; more than one is malformed.
    returns: BTreeMap<ImageAddress, Vec<usize>>,
}

struct CallingFunction {
    frame_base: Metadata<LocationDescription>,
    /// Whether the function describes every tail call it makes.
    tail_calls_described: bool,
    tail_calls: Vec<usize>,
}

struct CatalogCallSite {
    function: usize,
    /// The instruction after the call, which a frame it entered returns to.
    return_address: Option<ImageAddress>,
    target: SiteTarget,
    /// For a tail call, the function it enters in this module, if known.
    enters: Option<usize>,
    parameters: Vec<SiteParameter>,
    malformed: Option<Arc<str>>,
}

enum SiteTarget {
    /// A function's entry, until the catalog resolves it.
    Entry(DieKey),
    Code(ImageAddress),
    Symbol(Arc<str>),
    Computed(Metadata<LocationDescription>),
    Unknown,
}

struct SiteParameter {
    /// The register the parameter is passed in.
    register: Option<u16>,
    /// The `.debug_info` offset of the callee's parameter entry.
    parameter: Option<u64>,
    value: Option<Expression>,
    /// The value the passed address pointed at.
    data_value: Option<Expression>,
}

/// Builds the catalog while the variable catalog walks every entry.
#[derive(Default)]
pub(super) struct CallSiteBuilder {
    sites: Vec<CatalogCallSite>,
    functions: Vec<CallingFunction>,
    /// The first address of each function entry with code.
    starts: HashMap<DieKey, ImageAddress>,
    /// Each function with code, by its first address.
    code: Vec<(ImageAddress, usize)>,
    /// The abstract origin of each function with code.
    origins: Vec<(DieKey, usize, ImageAddress)>,
    /// Functions with code that other units may call by linkage name.
    external: HashMap<Arc<str>, Vec<usize>>,
    /// The site whose parameters follow, with its entry's depth.
    open: Option<(usize, usize)>,
}

impl CallSiteBuilder {
    /// Records the subprogram that the next cataloged function describes.
    pub(super) fn function<'data>(
        &mut self,
        dwarf: &gimli::Dwarf<Reader<'data>>,
        units: &[gimli::Unit<Reader<'data>>],
        unit_index: usize,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        ranges: &[AddressRange<ImageAddress>],
        frame_base: Metadata<LocationDescription>,
    ) {
        let index = self.functions.len();
        let start = ranges.iter().map(|range| range.start).min();
        let described = [
            gimli::DW_AT_call_all_calls,
            gimli::DW_AT_call_all_tail_calls,
            gimli::DW_AT_GNU_all_call_sites,
            gimli::DW_AT_GNU_all_tail_call_sites,
        ]
        .into_iter()
        .any(|attribute| {
            matches!(
                entry.attr_value(attribute),
                Some(gimli::AttributeValue::Flag(true))
            )
        });
        if let Some(start) = start {
            let key = DieKey {
                unit: unit_index,
                offset: entry.offset().0,
            };
            self.starts.insert(key, start);
            self.code.push((start, index));
            if let Ok(Some(origin)) = die_reference(
                entry.attr_value(gimli::DW_AT_abstract_origin),
                unit_index,
                units,
            ) {
                self.origins.push((origin, index, start));
            }
            if let Some(name) = external_name(dwarf, units, unit_index, entry) {
                self.external.entry(name).or_default().push(index);
            }
        }
        self.functions.push(CallingFunction {
            frame_base,
            tail_calls_described: described,
            tail_calls: Vec::new(),
        });
    }

    /// Records a call site of the cataloged function `function`.
    pub(super) fn site<'data>(
        &mut self,
        dwarf: &gimli::Dwarf<Reader<'data>>,
        units: &[gimli::Unit<Reader<'data>>],
        unit_index: usize,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        function: usize,
        depth: usize,
    ) {
        let unit = &units[unit_index];
        let mut site = CatalogCallSite {
            function,
            return_address: None,
            target: SiteTarget::Unknown,
            enters: None,
            parameters: Vec::new(),
            malformed: None,
        };
        match site_attributes(dwarf, units, unit_index, unit, entry) {
            Ok((return_address, tail, target)) => {
                site.target = target;
                // A tail call returns nowhere, whatever address follows it.
                site.return_address = return_address.filter(|_| !tail);
                if tail {
                    self.functions[function].tail_calls.push(self.sites.len());
                }
            }
            Err(error) => site.malformed = Some(error.to_string().into()),
        }
        self.open = Some((depth, self.sites.len()));
        self.sites.push(site);
    }

    /// Records a parameter of the site whose entry contains it.
    pub(super) fn parameter(
        &mut self,
        dwarf: &gimli::Dwarf<Reader<'_>>,
        units: &[gimli::Unit<Reader<'_>>],
        unit_index: usize,
        entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
        depth: usize,
    ) {
        let Some((site_depth, site)) = self.open else {
            return;
        };
        if depth != site_depth + 1 {
            return;
        }
        let site = &mut self.sites[site];
        match site_parameter(dwarf, units, unit_index, entry) {
            Ok(parameter) => site.parameters.push(parameter),
            Err(error) => {
                site.malformed
                    .get_or_insert_with(|| error.to_string().into());
            }
        }
    }

    pub(super) fn finish(mut self) -> CallSiteCatalog {
        let mut code = std::mem::take(&mut self.code);
        code.sort_unstable();
        let mut concrete = HashMap::<DieKey, Vec<ImageAddress>>::new();
        for (origin, _, start) in &self.origins {
            concrete.entry(*origin).or_default().push(*start);
        }
        let mut returns = BTreeMap::<ImageAddress, Vec<usize>>::new();
        for (index, site) in self.sites.iter_mut().enumerate() {
            if let SiteTarget::Entry(key) = site.target {
                // A function with code is entered there; an abstract one
                // where its one out-of-line instance is.
                site.target = match (self.starts.get(&key), concrete.get(&key).map(Vec::as_slice)) {
                    (Some(start), _) | (_, Some([start])) => SiteTarget::Code(*start),
                    _ => SiteTarget::Unknown,
                };
            }
            if let Some(address) = site.return_address {
                returns.entry(address).or_default().push(index);
            }
        }
        // What each tail call enters, when it stays in this module.
        let function_at = |address: ImageAddress| {
            code.binary_search_by_key(&address, |(start, _)| *start)
                .ok()
                .map(|position| code[position].1)
        };
        for function in &self.functions {
            for &site in &function.tail_calls {
                let enters = match &self.sites[site].target {
                    SiteTarget::Code(address) => function_at(*address),
                    SiteTarget::Symbol(name) => match self.external.get(name).map(Vec::as_slice) {
                        Some([only]) => Some(*only),
                        _ => None,
                    },
                    _ => None,
                };
                self.sites[site].enters = enters;
            }
        }
        CallSiteCatalog {
            sites: self.sites,
            functions: self.functions,
            returns,
        }
    }
}

/// A call site's return address, whether it is a tail call, and its target.
fn site_attributes<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    units: &[gimli::Unit<Reader<'data>>],
    unit_index: usize,
    unit: &gimli::Unit<Reader<'data>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
) -> Result<(Option<ImageAddress>, bool, SiteTarget), DwarfError> {
    let return_address = match entry
        .attr_value(gimli::DW_AT_call_return_pc)
        .or_else(|| entry.attr_value(gimli::DW_AT_low_pc))
    {
        Some(value) => dwarf.attr_address(unit, value)?.map(ImageAddress::new),
        None => None,
    };
    let tail = [gimli::DW_AT_call_tail_call, gimli::DW_AT_GNU_tail_call]
        .into_iter()
        .any(|attribute| {
            matches!(
                entry.attr_value(attribute),
                Some(gimli::AttributeValue::Flag(true))
            )
        });
    let origin = entry
        .attr_value(gimli::DW_AT_call_origin)
        .or_else(|| entry.attr_value(gimli::DW_AT_abstract_origin));
    let target = if let Some(key) = die_reference(origin, unit_index, units)? {
        let origin_unit = units
            .get(key.unit)
            .ok_or(DwarfError::ReferenceOutsideUnits(key.offset))?;
        let origin = origin_unit.entry(gimli::UnitOffset(key.offset))?;
        if matches!(
            origin.attr_value(gimli::DW_AT_declaration),
            Some(gimli::AttributeValue::Flag(true))
        ) {
            linkage_name(dwarf, units, key.unit, &origin)?
                .map_or(SiteTarget::Unknown, SiteTarget::Symbol)
        } else {
            SiteTarget::Entry(key)
        }
    } else if let Some(value) = entry
        .attr_value(gimli::DW_AT_call_target)
        .or_else(|| entry.attr_value(gimli::DW_AT_GNU_call_site_target))
    {
        SiteTarget::Computed(copy_optional_location(
            dwarf,
            unit_index,
            unit,
            Some(value),
            MetadataAbsence::NotApplicable,
        ))
    } else {
        SiteTarget::Unknown
    };
    Ok((return_address, tail, target))
}

fn site_parameter(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    units: &[gimli::Unit<Reader<'_>>],
    unit_index: usize,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
) -> Result<SiteParameter, DwarfError> {
    let unit = &units[unit_index];
    let expression = |attributes: [gimli::DwAt; 2]| -> Result<Option<Expression>, DwarfError> {
        attributes
            .into_iter()
            .find_map(|attribute| entry.attr_value(attribute))
            .and_then(|value| value.exprloc_value())
            .map(|expression| copy_expression(dwarf, unit_index, unit, expression, unit.encoding()))
            .transpose()
    };
    let register = match entry
        .attr_value(gimli::DW_AT_location)
        .and_then(|value| value.exprloc_value())
    {
        Some(location) => {
            let mut operations = location.operations(unit.encoding());
            match (operations.next()?, operations.next()?) {
                (Some(gimli::Operation::Register { register }), None) => Some(register.0),
                _ => None,
            }
        }
        None => None,
    };
    let parameter = die_reference(
        entry
            .attr_value(gimli::DW_AT_call_parameter)
            .or_else(|| entry.attr_value(gimli::DW_AT_abstract_origin)),
        unit_index,
        units,
    )?
    .and_then(|key| {
        gimli::UnitOffset(key.offset)
            .to_debug_info_offset(&units[key.unit].header)
            .map(|offset| u64::try_from(offset.0).expect("DWARF offset fits u64"))
    });
    Ok(SiteParameter {
        register,
        parameter,
        value: expression([gimli::DW_AT_call_value, gimli::DW_AT_GNU_call_site_value])?,
        data_value: expression([
            gimli::DW_AT_call_data_value,
            gimli::DW_AT_GNU_call_site_data_value,
        ])?,
    })
}

/// The name a linker symbol gives a function: its linkage name, or else
/// its name.
fn linkage_name<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    units: &[gimli::Unit<Reader<'data>>],
    unit_index: usize,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
) -> Result<Option<Arc<str>>, DwarfError> {
    let chain = origin_chain(units, unit_index, entry)?;
    for attribute in [
        gimli::DW_AT_linkage_name,
        gimli::DW_AT_MIPS_linkage_name,
        gimli::DW_AT_name,
    ] {
        if let Some(name) =
            string_with_origins(dwarf, units, &units[unit_index], entry, &chain, attribute)?
        {
            return Ok(Some(name));
        }
    }
    Ok(None)
}

/// The linkage name of a function other units may call.
fn external_name<'data>(
    dwarf: &gimli::Dwarf<Reader<'data>>,
    units: &[gimli::Unit<Reader<'data>>],
    unit_index: usize,
    entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
) -> Option<Arc<str>> {
    let chain = origin_chain(units, unit_index, entry).ok()?;
    if flag_with_origins(entry, &chain, gimli::DW_AT_external) != Some(true) {
        return None;
    }
    linkage_name(dwarf, units, unit_index, entry).ok().flatten()
}

const fn unavailable(reason: EntryValueUnavailableReason) -> VariableRuntimeError {
    VariableRuntimeError::Unavailable(VariableUnavailableReason::EntryValue(reason))
}

impl CatalogCallSite {
    /// What the call passed for `parameter`: the value, or for a referent
    /// the value at the address passed.
    fn passed(&self, parameter: EntryParameter) -> Result<&Expression, VariableRuntimeError> {
        let mut entries = self.parameters.iter().filter(|passed| match parameter {
            EntryParameter::Register(register) | EntryParameter::Referent(register) => {
                passed.register == Some(register)
            }
            EntryParameter::Parameter(offset) => passed.parameter == Some(offset),
        });
        let not_passed = || unavailable(EntryValueUnavailableReason::NoParameter);
        let passed = entries.next().ok_or_else(not_passed)?;
        if entries.next().is_some() {
            return Err(VariableRuntimeError::Malformed(
                "a call site describes one parameter twice".into(),
            ));
        }
        match parameter {
            EntryParameter::Referent(_) => passed.data_value.as_ref(),
            EntryParameter::Register(_) | EntryParameter::Parameter(_) => passed.value.as_ref(),
        }
        .ok_or_else(not_passed)
    }
}

impl CallSiteCatalog {
    /// The chain of tail calls from function `from`, entered by a call, to
    /// function `to`, when exactly one is possible.
    fn tail_path(&self, from: usize, to: usize) -> Result<Vec<usize>, VariableRuntimeError> {
        // Every function a chain from `from` may reach must describe its
        // tail calls, each of which must stay within this module.
        let mut reached = vec![from];
        let mut seen = HashSet::from([from]);
        let mut next = 0;
        while let Some(&function) = reached.get(next) {
            next += 1;
            let calling = &self.functions[function];
            if !calling.tail_calls_described {
                return Err(unavailable(EntryValueUnavailableReason::TailCalls));
            }
            for &site in &calling.tail_calls {
                let enters = self.sites[site]
                    .enters
                    .ok_or_else(|| unavailable(EntryValueUnavailableReason::TailCalls))?;
                if seen.insert(enters) {
                    if reached.len() == MAX_TAIL_CALL_FUNCTIONS {
                        return Err(VariableUnavailableReason::EvaluationLimit.into());
                    }
                    reached.push(enters);
                }
            }
        }
        if !seen.contains(&to) {
            return Err(unavailable(EntryValueUnavailableReason::TargetMismatch));
        }

        // The functions that lead to `to`, by walking tail calls backwards.
        let mut callers = HashMap::<usize, Vec<usize>>::new();
        for &function in &reached {
            for &site in &self.functions[function].tail_calls {
                let enters = self.sites[site].enters.expect("checked above");
                callers.entry(enters).or_default().push(function);
            }
        }
        let mut leads = HashSet::from([to]);
        let mut pending = vec![to];
        while let Some(function) = pending.pop() {
            for &caller in callers.get(&function).into_iter().flatten() {
                if leads.insert(caller) {
                    pending.push(caller);
                }
            }
        }

        // Count the chains to `to`; a cycle among the functions leading to it
        // allows endlessly many.
        let mut chains = HashMap::<usize, u64>::new();
        let mut active = HashSet::new();
        self.count_chains(from, to, &leads, &mut chains, &mut active)?;
        if chains[&from] != 1 {
            return Err(unavailable(EntryValueUnavailableReason::TailCalls));
        }
        let mut path = Vec::new();
        let mut function = from;
        while function != to {
            let site = self.functions[function]
                .tail_calls
                .iter()
                .copied()
                .find(|site| {
                    let enters = self.sites[*site].enters.expect("checked above");
                    chains.get(&enters) == Some(&1)
                })
                .expect("the one chain continues");
            path.push(site);
            function = self.sites[site].enters.expect("checked above");
        }
        Ok(path)
    }

    fn count_chains(
        &self,
        function: usize,
        to: usize,
        leads: &HashSet<usize>,
        chains: &mut HashMap<usize, u64>,
        active: &mut HashSet<usize>,
    ) -> Result<u64, VariableRuntimeError> {
        if let Some(count) = chains.get(&function) {
            return Ok(*count);
        }
        if !active.insert(function) {
            return Err(unavailable(EntryValueUnavailableReason::TailCalls));
        }
        let mut count = u64::from(function == to);
        for &site in &self.functions[function].tail_calls {
            let enters = self.sites[site].enters.expect("checked above");
            if leads.contains(&enters) {
                count = count.saturating_add(self.count_chains(enters, to, leads, chains, active)?);
            }
        }
        active.remove(&function);
        chains.insert(function, count);
        Ok(count)
    }
}

impl DwarfVariableInfo {
    pub(super) fn described_call_site(
        &self,
        return_address: ImageAddress,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Option<CallSite>, VariableRuntimeError> {
        let Some(sites) = self.call_sites.returns.get(&return_address) else {
            return Ok(None);
        };
        let [index] = sites.as_slice() else {
            return Err(VariableRuntimeError::Malformed(
                format!("several call sites return to {return_address}").into(),
            ));
        };
        let site = &self.call_sites.sites[*index];
        if let Some(description) = &site.malformed {
            return Err(VariableRuntimeError::Malformed(Arc::clone(description)));
        }
        let target = match &site.target {
            SiteTarget::Code(address) => CallTarget::Code(*address),
            SiteTarget::Symbol(name) => CallTarget::Symbol(Arc::clone(name)),
            SiteTarget::Computed(location) => {
                let location = match location {
                    Metadata::Value(location) => location,
                    Metadata::Malformed(description) => {
                        return Err(VariableRuntimeError::Malformed(Arc::clone(description)));
                    }
                    Metadata::Absent(_) => unreachable!("a computed target has a location"),
                };
                // A target the caller can no longer compute is unknown.
                match self.site_word(*index, location, runtime, budget) {
                    Ok(address) => CallTarget::Computed(VirtualAddress::new(address)),
                    Err(VariableRuntimeError::Unavailable(
                        reason @ (VariableUnavailableReason::EvaluationLimit
                        | VariableUnavailableReason::InspectionLimit(_)),
                    )) => return Err(reason.into()),
                    Err(VariableRuntimeError::Unavailable(_)) => CallTarget::Unknown,
                    Err(error) => return Err(error),
                }
            }
            SiteTarget::Unknown => CallTarget::Unknown,
            SiteTarget::Entry(_) => unreachable!("the catalog resolves entries"),
        };
        Ok(Some(CallSite {
            id: CallSiteId(*index),
            target,
        }))
    }

    pub(super) fn tail_call_path(
        &self,
        from: ImageAddress,
        to: ImageAddress,
    ) -> Result<Arc<[CallSiteId]>, VariableRuntimeError> {
        let from = self
            .function_index_at(from)
            .ok_or_else(|| unavailable(EntryValueUnavailableReason::UnknownTarget))?;
        let to = self
            .function_index_at(to)
            .ok_or_else(|| unavailable(EntryValueUnavailableReason::UnknownTarget))?;
        Ok(self
            .call_sites
            .tail_path(from, to)?
            .into_iter()
            .map(CallSiteId)
            .collect())
    }

    pub(super) fn site_parameter_value(
        &self,
        site: CallSiteId,
        parameter: EntryParameter,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<u64, VariableRuntimeError> {
        let index = site.0;
        let catalog_site = self
            .call_sites
            .sites
            .get(index)
            .ok_or_else(|| VariableRuntimeError::Fatal("unknown call site".into()))?;
        if let Some(description) = &catalog_site.malformed {
            return Err(VariableRuntimeError::Malformed(Arc::clone(description)));
        }
        let expression = catalog_site.passed(parameter)?;
        let location = LocationDescription {
            entries: vec![super::location::LocationEntry {
                range: None,
                expression: expression.clone(),
            }]
            .into(),
        };
        self.site_word(index, &location, runtime, budget)
    }

    /// Evaluates one of a call site's expressions to a word, in the state of
    /// the frame that made the call: the value it computes, or the address
    /// it describes.
    fn site_word(
        &self,
        site: usize,
        location: &LocationDescription,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<u64, VariableRuntimeError> {
        let catalog_site = &self.call_sites.sites[site];
        // Within the call instruction, which the caller's frame describes.
        let address = catalog_site
            .return_address
            .and_then(|address| address.get().checked_sub(1))
            .map(ImageAddress::new);
        let expression = location
            .expression(address)
            .map_err(|error| match error {
                super::location::LocationSelectionError::Unavailable(reason) => {
                    VariableRuntimeError::Unavailable(reason)
                }
                super::location::LocationSelectionError::Malformed(description) => {
                    VariableRuntimeError::Malformed(description)
                }
            })?
            .ok_or(VariableRuntimeError::Unavailable(
                VariableUnavailableReason::UnavailableAtInstruction,
            ))?;
        let mut cache = FrameBaseCache::Empty;
        let mut frame_base = FrameBase::Lazy(FrameBaseContext {
            location: &self.call_sites.functions[catalog_site.function].frame_base,
            address,
            cache: &mut cache,
        });
        let pieces = evaluate(
            expression,
            self.endian,
            address,
            &mut frame_base,
            &self.evaluation_units,
            runtime,
            budget,
        )
        .map_err(runtime_error)?;
        let [piece] = pieces.as_slice() else {
            return Err(VariableRuntimeError::Malformed(
                "a call site's value has several pieces".into(),
            ));
        };
        if piece.size_in_bits.is_some() || piece.bit_offset.is_some() {
            return Err(VariableRuntimeError::Malformed(
                "a call site's value is a partial piece".into(),
            ));
        }
        match &piece.location {
            // Without DW_OP_stack_value, the address an expression computes
            // is its value.
            Location::Address { address } => Ok(*address),
            Location::Value { value } => Ok(word(*value)),
            Location::Register { register } => {
                let register = runtime.register(register.0)?;
                bytes_to_u64(&register.bytes, self.endian).map_err(runtime_error)
            }
            Location::Bytes { value } => {
                bytes_to_u64(value.slice(), self.endian).map_err(runtime_error)
            }
            _ => Err(VariableRuntimeError::Malformed(
                "a call site's value is not a value".into(),
            )),
        }
    }
}

/// A DWARF value as the register-sized word that holds it.
fn word(value: Value) -> u64 {
    match value {
        Value::Generic(value) | Value::U64(value) => value,
        Value::U8(value) => u64::from(value),
        Value::U16(value) => u64::from(value),
        Value::U32(value) => u64::from(value),
        Value::I8(value) => i64::from(value).cast_unsigned(),
        Value::I16(value) => i64::from(value).cast_unsigned(),
        Value::I32(value) => i64::from(value).cast_unsigned(),
        Value::I64(value) => value.cast_unsigned(),
        Value::F32(value) => u64::from(value.to_bits()),
        Value::F64(value) => value.to_bits(),
    }
}

pub(super) fn runtime_error(error: EvaluateError) -> VariableRuntimeError {
    match error {
        EvaluateError::Unavailable(reason) => VariableRuntimeError::Unavailable(reason),
        EvaluateError::Malformed(description) => VariableRuntimeError::Malformed(description),
        EvaluateError::Fatal(description) => VariableRuntimeError::Fatal(description),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A catalog of functions `0..described.len()`, of which those
    /// `described` say what tail calls they make: `tail_calls`, from one
    /// function to another or to code outside the module.
    fn catalog(described: &[bool], tail_calls: &[(usize, Option<usize>)]) -> CallSiteCatalog {
        let mut functions = described
            .iter()
            .map(|&tail_calls_described| CallingFunction {
                frame_base: Metadata::Absent(MetadataAbsence::NotApplicable),
                tail_calls_described,
                tail_calls: Vec::new(),
            })
            .collect::<Vec<_>>();
        let sites = tail_calls
            .iter()
            .enumerate()
            .map(|(site, &(function, enters))| {
                functions[function].tail_calls.push(site);
                CatalogCallSite {
                    function,
                    return_address: None,
                    target: SiteTarget::Unknown,
                    enters,
                    parameters: Vec::new(),
                    malformed: None,
                }
            })
            .collect();
        CallSiteCatalog {
            sites,
            functions,
            returns: BTreeMap::new(),
        }
    }

    /// What a call passed is what the one parameter entry naming the place
    /// says; a call that describes one place twice could have passed either.
    #[test]
    fn a_call_passes_what_its_one_parameter_entry_says() {
        let value = |bytes: &[u8]| Expression {
            bytes: Arc::from(bytes),
            encoding: gimli::Encoding {
                format: gimli::Format::Dwarf32,
                version: 5,
                address_size: 8,
            },
            unit: 0,
            indexed_addresses: Arc::default(),
            procedures: Arc::default(),
        };
        let site = |parameters| CatalogCallSite {
            function: 0,
            return_address: None,
            target: SiteTarget::Unknown,
            enters: None,
            parameters,
            malformed: None,
        };
        let in_register = |register, bytes: &[u8]| SiteParameter {
            register: Some(register),
            parameter: None,
            value: Some(value(bytes)),
            data_value: None,
        };
        let one = site(vec![in_register(5, &[1]), in_register(4, &[2])]);
        let passed = |site: &CatalogCallSite, parameter| match site.passed(parameter) {
            Ok(expression) => Ok(expression.bytes.to_vec()),
            Err(VariableRuntimeError::Unavailable(VariableUnavailableReason::EntryValue(
                EntryValueUnavailableReason::NoParameter,
            ))) => Err("not passed"),
            Err(VariableRuntimeError::Malformed(_)) => Err("malformed"),
            Err(VariableRuntimeError::Unavailable(_) | VariableRuntimeError::Fatal(_)) => {
                panic!("refused for another reason")
            }
        };
        assert_eq!(passed(&one, EntryParameter::Register(4)), Ok(vec![2]));
        assert_eq!(passed(&one, EntryParameter::Register(1)), Err("not passed"));
        assert_eq!(passed(&one, EntryParameter::Referent(4)), Err("not passed"));
        let twice = site(vec![in_register(5, &[1]), in_register(5, &[2])]);
        assert_eq!(
            passed(&twice, EntryParameter::Register(5)),
            Err("malformed")
        );
    }

    /// A frame's function was entered by the call its caller made, or by
    /// the one chain of tail calls from that call's target that reaches it.
    /// Any doubt about which chain ran refuses.
    #[test]
    fn only_one_possible_chain_of_tail_calls_is_followed() {
        use EntryValueUnavailableReason::{TailCalls, TargetMismatch};

        let all = [true; 4];
        let cases: [(&str, CallSiteCatalog, usize, Result<Vec<usize>, _>); 9] = [
            ("entered directly", catalog(&all, &[]), 0, Ok(vec![])),
            (
                "tail calls that lead elsewhere",
                catalog(&all, &[(0, Some(1)), (1, Some(1))]),
                0,
                Ok(vec![]),
            ),
            (
                "a chain",
                catalog(&all, &[(0, Some(1)), (1, Some(2)), (0, Some(3))]),
                2,
                Ok(vec![0, 1]),
            ),
            (
                "two chains",
                catalog(
                    &all,
                    &[(0, Some(1)), (0, Some(2)), (1, Some(3)), (2, Some(3))],
                ),
                3,
                Err(TailCalls),
            ),
            (
                "a cycle through the frame's function",
                catalog(&all, &[(0, Some(1)), (1, Some(0))]),
                0,
                Err(TailCalls),
            ),
            (
                "a cycle on the way",
                catalog(&all, &[(0, Some(1)), (1, Some(0)), (1, Some(2))]),
                2,
                Err(TailCalls),
            ),
            (
                "a tail call out of the module",
                catalog(&all, &[(0, Some(1)), (1, None)]),
                1,
                Err(TailCalls),
            ),
            (
                "a function silent about its tail calls",
                catalog(&[true, false, true, true], &[(0, Some(1))]),
                1,
                Err(TailCalls),
            ),
            (
                "an unreachable function",
                catalog(&all, &[(0, Some(1))]),
                2,
                Err(TargetMismatch),
            ),
        ];
        for (case, catalog, to, expected) in cases {
            let path = catalog.tail_path(0, to).map_err(|error| match error {
                VariableRuntimeError::Unavailable(VariableUnavailableReason::EntryValue(
                    reason,
                )) => reason,
                _ => panic!("{case}: refused for no reason of entry values"),
            });
            assert_eq!(path, expected, "{case}");
        }
    }
}

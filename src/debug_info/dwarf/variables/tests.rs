use std::collections::BTreeMap;

use gimli::write::{
    AttributeValue as WriteAttributeValue, Dwarf as WriteDwarf, EndianVec, LineProgram, Sections,
    Unit,
};
use gimli::{Encoding, Format, LittleEndian};
use gimli::{Location, Value};

use crate::debug_info::VariableRuntimeError;
use crate::{
    Accessibility, Architecture, ArrayDimension, BaseType, BaseTypeEncoding, FloatValue,
    IntegerValue, NamedTypeRelationship, RecordKind, RecordMember, RecordMemberLayout, ScalarValue,
    TypeKind, TypeModifier, VariableInvalidReason, VariableUnavailableReason, VariableValueSource,
    Variant, VariantSelection, VariantSelector, VirtualAddress,
};

use super::codec::{
    decode_address, decode_integer_value, decode_scalar, extract_bit_field, read_sleb128_i128,
    read_uleb128_u128,
};
use super::die::checked_reference_chain;
use super::evaluate::{
    EvaluateError, FrameBase, FrameBaseCache, FrameBaseContext, dwarf_value_bytes, evaluate,
    materialize_constant, materialize_piece,
};
use super::globals::{DefinitionIndex, DefinitionResolution};
use super::inspect::{
    PathStep, ScalarDecodeError, array_byte_offset, evaluate_error_state, implicit_pointer_range,
    static_member_layout_is_valid,
};
use super::location::{EvaluationUnit, Expression, LocationDescription, LocationEntry};
use super::shape::{ValueShape, ValueShapeError, value_shape_from};
use super::types::{
    TypeArenaBuilder, TypeEntry, TypeResolution, inline_storage_cycle_nodes,
    propagate_wrapper_sizes, zig_error_union_type_names, zig_optional_payload_name,
};
use super::variant::{
    VariantMetadataBudget, VariantMetadataError, parse_discriminant_list,
    validate_variant_selections,
};
use super::*;

#[test]
fn array_indices_honor_lower_bounds_and_reject_overflow() {
    let step = |dimensions: &[(i128, u64)], element_size| PathStep::ArrayIndex {
        dimensions: dimensions
            .iter()
            .map(|&(lower_bound, count)| ArrayDimension { lower_bound, count })
            .collect(),
        element_size,
    };
    let bounded = step(&[(-2, 3), (10, 2)], 4);
    assert_eq!(array_byte_offset(&bounded, &[0, 11]).ok(), Some(Some(20)));
    for (indices, bad_index, bad_lower_bound, bad_count) in
        [([-3, 10], -3, -2, 3), ([0, 12], 12, 10, 2)]
    {
        assert!(matches!(
            array_byte_offset(&bounded, &indices),
            Err(Error::ValueIndexOutOfBounds { index, lower_bound, count })
                if (index, lower_bound, count) == (bad_index, bad_lower_bound, bad_count)
        ));
    }
    assert!(matches!(
        array_byte_offset(&bounded, &[0]),
        Err(Error::InvalidValueExpression(_))
    ));

    let overflowing = step(&[(0, u64::MAX), (0, 2)], 1);
    assert!(matches!(
        array_byte_offset(&overflowing, &[i128::from(u64::MAX - 1), 1]),
        Err(Error::InvalidValueExpression(message)) if message.contains("row-major")
    ));
}

#[test]
fn declaration_canonicalization_cannot_launder_a_non_type_reference() {
    let encoding = Encoding {
        format: Format::Dwarf32,
        version: 5,
        address_size: 8,
    };
    let mut written = WriteDwarf::new();
    let unit_id = written.units.add(Unit::new(encoding, LineProgram::none()));
    let unit = written.units.get_mut(unit_id);
    let root = unit.root();
    let non_type = unit.add(root, gimli::DW_TAG_variable);
    let definition = unit.add(root, gimli::DW_TAG_base_type);
    unit.get_mut(definition).set(
        gimli::DW_AT_specification,
        WriteAttributeValue::UnitRef(non_type),
    );
    unit.get_mut(definition)
        .set(gimli::DW_AT_encoding, WriteAttributeValue::Data1(5));
    unit.get_mut(definition)
        .set(gimli::DW_AT_byte_size, WriteAttributeValue::Udata(4));
    let variable = unit.add(root, gimli::DW_TAG_variable);
    unit.get_mut(variable)
        .set(gimli::DW_AT_type, WriteAttributeValue::UnitRef(non_type));

    let mut sections = Sections::new(EndianVec::new(LittleEndian));
    written.write(&mut sections).expect("write test DWARF");
    let dwarf = gimli::Dwarf::load(|id| {
        let bytes = sections.get(id).map(EndianVec::slice).unwrap_or_default();
        Ok::<_, gimli::Error>(Reader::new(bytes, RunTimeEndian::Little))
    })
    .expect("read test DWARF");
    let mut headers = dwarf.units();
    let header = headers
        .next()
        .expect("read unit header")
        .expect("one test unit");
    let units = vec![dwarf.unit(header).expect("read test unit")];
    let type_value = {
        let mut entries = units[0].entries();
        let mut value = None;
        while let Some(entry) = entries.next_dfs().expect("read test DIE") {
            if entry.tag() == gimli::DW_TAG_variable
                && let Some(candidate) = entry.attr_value(gimli::DW_AT_type)
            {
                value = Some(candidate);
            }
        }
        value.expect("referencing variable")
    };
    let signatures = HashMap::new();
    let mut arena = TypeArenaBuilder::new(
        &dwarf,
        &units,
        &signatures,
        ModuleImageId::new(0),
        ByteOrder::Little,
    );

    assert!(matches!(
        arena.variable_type(0, Some(type_value)),
        TypeResolution::Malformed(description)
            if description.contains("non-type tag")
    ));
}

#[test]
fn discriminant_leb128_parsers_cover_full_width_and_reject_overflow() {
    let mut unsigned_max = vec![0xff; 18];
    unsigned_max.push(0x03);
    let mut cursor = 0;
    assert_eq!(
        read_uleb128_u128(&unsigned_max, &mut cursor).unwrap(),
        u128::MAX
    );
    assert_eq!(cursor, unsigned_max.len());

    let mut unsigned_overflow = vec![0xff; 18];
    unsigned_overflow.push(0x04);
    assert!(read_uleb128_u128(&unsigned_overflow, &mut 0).is_err());
    assert_eq!(read_sleb128_i128(&[0x7d], &mut 0).unwrap(), -3);
    assert!(read_sleb128_i128(&[0x80; 20], &mut 0).is_err());
}

#[test]
fn variant_selection_validation_rejects_overlap_and_multiple_defaults() {
    let variant = |selection| Variant {
        name: None,
        selection,
        members: Arc::from([]),
    };
    let overlapping = [
        variant(VariantSelection::Selectors(Arc::from([
            VariantSelector::Range {
                low: IntegerValue::Signed(-3),
                high: IntegerValue::Signed(3),
            },
        ]))),
        variant(VariantSelection::Selectors(Arc::from([
            VariantSelector::Value(IntegerValue::Signed(3)),
        ]))),
    ];
    assert!(validate_variant_selections(&overlapping).is_err());
    let adjacent = [
        variant(VariantSelection::Selectors(Arc::from([
            VariantSelector::Range {
                low: IntegerValue::Signed(-3),
                high: IntegerValue::Signed(2),
            },
        ]))),
        variant(VariantSelection::Selectors(Arc::from([
            VariantSelector::Value(IntegerValue::Signed(3)),
        ]))),
        variant(VariantSelection::Default),
    ];
    assert!(validate_variant_selections(&adjacent).is_ok());
    assert!(
        validate_variant_selections(&[
            variant(VariantSelection::Default),
            variant(VariantSelection::Default),
        ])
        .is_err()
    );
}

#[test]
fn explicit_discriminant_lists_are_nonempty_and_share_one_metadata_budget() {
    let representation = scalar_type(BaseTypeEncoding::Unsigned, 1);
    let mut budget = VariantMetadataBudget::default();
    assert!(matches!(
        parse_discriminant_list(&[], &representation, &mut budget),
        Err(VariantMetadataError::Malformed(_))
    ));

    for _ in 0..MAX_VARIANT_METADATA - 1 {
        budget.consume().expect("budget entry");
    }
    let selector = [gimli::DW_DSC_label.0, 1];
    assert!(parse_discriminant_list(&selector, &representation, &mut budget).is_ok());
    assert_eq!(
        parse_discriminant_list(&selector, &representation, &mut budget),
        Err(VariantMetadataError::Limit)
    );
}

#[test]
fn zig_synthetic_variant_names_require_canonical_type_syntax() {
    assert_eq!(zig_optional_payload_name("?u32"), Some("u32"));
    assert_eq!(zig_optional_payload_name("named.?u32"), None);
    assert_eq!(
        zig_error_union_type_names("error{bad}!u32"),
        Some(("error{bad}", "u32"))
    );
    assert_eq!(
        zig_error_union_type_names("anyerror!*const u8"),
        Some(("anyerror", "*const u8"))
    );
    assert_eq!(zig_error_union_type_names("user!record"), None);
    assert_eq!(zig_error_union_type_names("error{bad}!"), None);
}

#[test]
fn specification_chains_reject_cycles() {
    let first = DieKey {
        unit: 0,
        offset: 0x10,
    };
    let second = DieKey {
        unit: 0,
        offset: 0x20,
    };
    let references = HashMap::from([(first, second), (second, first)]);

    assert!(matches!(
        checked_reference_chain(Some(first), |key| Ok(references.get(&key).copied())),
        Err(DwarfError::ReferenceCycle)
    ));
}

#[test]
fn definition_index_collapses_aliases_but_never_conflicting_identities() {
    let identities = |die: &str, linkage: &str| [Arc::from(die), Arc::from(linkage)];
    let mut definitions = DefinitionIndex::default();

    assert_eq!(
        definitions.resolve(&identities("die:one", "linkage:same"), 0),
        DefinitionResolution::New
    );
    assert_eq!(
        definitions.resolve(&identities("die:two", "linkage:same"), 1),
        DefinitionResolution::Existing(0)
    );
    assert_eq!(
        definitions.resolve(&identities("die:three", "linkage:other"), 1),
        DefinitionResolution::New
    );
    assert_eq!(
        definitions.resolve(&identities("die:three", "linkage:same"), 2),
        DefinitionResolution::Conflict
    );
}

struct Runtime {
    registers: BTreeMap<u16, u64>,
    cfa: std::result::Result<VirtualAddress, VariableUnavailableReason>,
    memory: Option<Arc<[u8]>>,
    memory_reads: u32,
}

impl VariableRuntime for Runtime {
    fn register(
        &mut self,
        register: u16,
    ) -> std::result::Result<crate::debug_info::VariableRegister, VariableRuntimeError> {
        let value = self.registers.get(&register).copied().ok_or_else(|| {
            VariableRuntimeError::Unavailable(VariableUnavailableReason::RegisterUnavailable(
                register.to_string().into(),
            ))
        })?;
        Ok(crate::debug_info::VariableRegister {
            descriptor: crate::RegisterDescriptor {
                id: crate::RegisterId::new(u32::from(register)),
                name: format!("r{register}").into(),
                bits: 64,
                role: None,
            },
            bytes: Arc::from(value.to_le_bytes()),
        })
    }

    fn call_frame_cfa(&self) -> std::result::Result<VirtualAddress, VariableRuntimeError> {
        self.cfa.clone().map_err(VariableRuntimeError::Unavailable)
    }

    fn tls_address(
        &mut self,
        _offset: u64,
    ) -> std::result::Result<VirtualAddress, VariableUnavailableReason> {
        Err(crate::UnsupportedVariableFeature::Tls.into())
    }

    fn relocate(&self, address: ImageAddress) -> std::result::Result<VirtualAddress, Arc<str>> {
        Ok(VirtualAddress::new(address.get()))
    }

    fn image_address(&self, address: VirtualAddress) -> Option<ImageAddress> {
        Some(ImageAddress::new(address.get()))
    }

    fn read_memory(
        &mut self,
        _address: VirtualAddress,
        size: usize,
    ) -> std::result::Result<Arc<[u8]>, VariableRuntimeError> {
        self.memory_reads += 1;
        self.memory
            .as_ref()
            .map(|memory| Arc::from(&memory[..size]))
            .ok_or_else(|| VariableRuntimeError::Fatal("unexpected memory read".into()))
    }
}

impl Runtime {
    fn new(registers: impl IntoIterator<Item = (u16, u64)>) -> Self {
        Self {
            registers: registers.into_iter().collect(),
            cfa: Ok(VirtualAddress::new(0x3000)),
            memory: None,
            memory_reads: 0,
        }
    }
}

/// Evaluates an expression with no frame base and an unlimited budget.
fn run<'a>(
    expression: &'a Expression,
    units: &[EvaluationUnit],
    runtime: &mut Runtime,
) -> std::result::Result<Vec<gimli::Piece<Reader<'a>>>, EvaluateError> {
    evaluate(
        expression,
        RunTimeEndian::Little,
        &mut FrameBase::Unsupported,
        units,
        runtime,
        &mut InspectionBudget::default(),
    )
}

fn reference(id: u32) -> TypeReference {
    TypeReference {
        image: ModuleImageId::new(7),
        id: TypeId::new(id),
    }
}

fn node(id: u32, name: &str, byte_size: Option<u64>, kind: TypeKind) -> TypeEntry {
    TypeEntry::Resolved(TypeInfo {
        reference: reference(id),
        name: name.into(),
        byte_size,
        kind,
        identity: None,
    })
}

/// Two wrapper types that wrap each other.
fn wrapper_cycle() -> [TypeEntry; 2] {
    [
        node(
            0,
            "left",
            Some(8),
            TypeKind::Named {
                target: Some(reference(1)),
                relationship: NamedTypeRelationship::Synonym,
            },
        ),
        node(
            1,
            "right",
            Some(8),
            TypeKind::Modified {
                modifier: TypeModifier::Const,
                target: reference(0),
            },
        ),
    ]
}

fn expression(bytes: &[u8]) -> Expression {
    Expression {
        bytes: Arc::from(bytes),
        encoding: gimli::Encoding {
            format: gimli::Format::Dwarf32,
            version: 5,
            address_size: 8,
        },
        unit: 0,
        indexed_addresses: Arc::new(HashMap::new()),
    }
}

fn scalar_type(encoding: BaseTypeEncoding, byte_size: u64) -> BaseType {
    BaseType {
        name: "test".into(),
        base_name: "test".into(),
        encoding,
        byte_size,
        bit_size: None,
    }
}

fn target(byte_order: ByteOrder) -> TargetDescription {
    TargetDescription {
        architecture: Architecture::X86_64,
        byte_order,
        pointer_width: crate::PointerWidth::Bits64,
    }
}

fn units(base_types: impl IntoIterator<Item = (usize, gimli::ValueType)>) -> Vec<EvaluationUnit> {
    vec![EvaluationUnit {
        base_types: base_types.into_iter().collect(),
        language: None,
    }]
}

#[test]
fn malformed_backward_branch_expression_fails_instead_of_hanging() {
    let mut runtime = Runtime::new([]);
    // DW_OP_skip with a -3 offset branches back onto itself forever.
    let looping = expression(&[gimli::DW_OP_skip.0, 0xfd, 0xff]);
    let mut budget = InspectionBudget::default();
    let result = evaluate(
        &looping,
        RunTimeEndian::Little,
        &mut FrameBase::Unsupported,
        &units([]),
        &mut runtime,
        &mut budget,
    );
    assert!(matches!(
        result,
        Err(EvaluateError::Unavailable(
            VariableUnavailableReason::EvaluationLimit
        ))
    ));
    assert_eq!(budget.completion(), crate::InspectionCompletion::Complete);
}

#[test]
fn typed_register_values_are_evaluated_with_the_referenced_base_type() {
    let mut runtime = Runtime::new([(6, 0x2000)]);
    // DW_OP_regval_type register 6, base type DIE offset 0x10.
    let typed = expression(&[
        gimli::DW_OP_regval_type.0,
        6,
        0x10,
        gimli::DW_OP_stack_value.0,
    ]);
    let result = run(
        &typed,
        &units([(0x10, gimli::ValueType::U64)]),
        &mut runtime,
    )
    .expect("typed register expression");
    assert!(matches!(
        result.as_slice(),
        [gimli::Piece {
            location: Location::Value {
                value: Value::U64(0x2000)
            },
            ..
        }]
    ));
}

#[test]
fn implicit_and_computed_values_materialize_with_source_provenance() {
    let mut runtime = Runtime::new([]);
    let implicit = expression(&[gimli::DW_OP_implicit_value.0, 4, 0xd6, 0xff, 0xff, 0xff]);
    let pieces = run(&implicit, &units([]), &mut runtime).expect("implicit scalar expression");
    let materialized = materialize_piece(
        &pieces[0],
        4,
        Some(&scalar_type(BaseTypeEncoding::Signed, 4)),
        RunTimeEndian::Little,
        target(ByteOrder::Little),
        &mut runtime,
        &mut InspectionBudget::default(),
    )
    .expect("implicit scalar value");
    assert_eq!(materialized.0, VariableValueSource::Constant);
    assert_eq!(materialized.1.as_ref(), &[0xd6, 0xff, 0xff, 0xff]);

    let computed = dwarf_value_bytes(
        Value::Generic(u64::MAX - 1),
        &scalar_type(BaseTypeEncoding::Boolean, 1),
        target(ByteOrder::Little),
    )
    .expect("word-sized boolean expression");
    assert_eq!(computed.as_ref(), &[0]);
}

#[test]
fn undefined_pieces_are_unavailable_rather_than_filled() {
    let value = |value| gimli::Piece::<Reader<'_>> {
        location: Location::Value {
            value: Value::Generic(value),
        },
        size_in_bits: Some(8),
        bit_offset: None,
    };
    let empty = |size| gimli::Piece::<Reader<'_>> {
        location: Location::Empty,
        size_in_bits: Some(size),
        bit_offset: None,
    };
    let pieces = [value(0x12), empty(16), value(0x34)];
    let mut runtime = Runtime::new([]);
    let mut budget = InspectionBudget::default();
    let assembled =
        super::pieces::assemble(&pieces, 4, RunTimeEndian::Little, &mut runtime, &mut budget);
    let Ok(crate::model::ValueStorage::Bytes {
        raw, unavailable, ..
    }) = &assembled
    else {
        panic!("a partly defined value is captured: {assembled:?}");
    };
    assert_eq!((raw[0], raw[3]), (0x12, 0x34));
    assert_eq!(
        super::pieces::unavailable_within(unavailable, 0, 4),
        Some(VariableUnavailableReason::OptimizedOut(
            crate::OptimizedOutReason::UndefinedPieces {
                ranges: Arc::from([crate::ValueBitRange {
                    offset: 8,
                    size: 16,
                }]),
            },
        ))
    );
    assert!(matches!(
        super::pieces::assemble(&pieces, 3, RunTimeEndian::Little, &mut runtime, &mut budget),
        Err(EvaluateError::Malformed(_))
    ));
    assert_eq!(
        super::pieces::assemble(
            &[empty(32)],
            8,
            RunTimeEndian::Little,
            &mut runtime,
            &mut budget
        ),
        Err(EvaluateError::Unavailable(
            VariableUnavailableReason::OptimizedOut(crate::OptimizedOutReason::EmptyLocation)
        ))
    );
}

#[test]
fn an_empty_location_expression_describes_an_optimized_out_value() {
    let empty = expression(&[]);
    let mut runtime = Runtime::new([]);
    let pieces = run(&empty, &units([]), &mut runtime).expect("an empty expression is valid");

    assert_eq!(
        materialize_piece(
            &pieces[0],
            8,
            None,
            RunTimeEndian::Little,
            target(ByteOrder::Little),
            &mut runtime,
            &mut InspectionBudget::default(),
        ),
        Err(EvaluateError::Unavailable(
            VariableUnavailableReason::OptimizedOut(crate::OptimizedOutReason::EmptyLocation)
        ))
    );
}

#[test]
fn deferred_operations_and_missing_types_have_stable_typed_reasons() {
    let mut runtime = Runtime::new([(0, 1)]);
    let entry = expression(&[
        gimli::DW_OP_entry_value.0,
        1,
        gimli::DW_OP_reg0.0,
        gimli::DW_OP_stack_value.0,
    ]);
    assert_eq!(
        run(&entry, &units([]), &mut runtime),
        Err(crate::UnsupportedVariableFeature::EntryValue.into())
    );

    let missing_type = expression(&[
        gimli::DW_OP_regval_type.0,
        0,
        0x10,
        gimli::DW_OP_stack_value.0,
    ]);
    assert_eq!(
        run(&missing_type, &units([]), &mut runtime),
        Err(crate::UnsupportedVariableFeature::TypedValue.into())
    );
}

#[test]
fn expression_memory_reads_are_strictly_bounded() {
    let mut bytes = Vec::new();
    for address in 0_u32..=64 {
        bytes.push(gimli::DW_OP_addr.0);
        bytes.extend_from_slice(&u64::from(address).to_le_bytes());
        bytes.extend_from_slice(&[gimli::DW_OP_deref_size.0, 1, gimli::DW_OP_drop.0]);
    }
    bytes.extend_from_slice(&[gimli::DW_OP_lit0.0, gimli::DW_OP_stack_value.0]);
    let expression = expression(&bytes);
    let mut runtime = Runtime {
        memory: Some(Arc::from([0_u8; 16])),
        ..Runtime::new([])
    };
    assert_eq!(
        evaluate(
            &expression,
            RunTimeEndian::Little,
            &mut FrameBase::Unsupported,
            &units([]),
            &mut runtime,
            &mut InspectionBudget::new(crate::InspectionLimits {
                memory_reads: 64,
                ..crate::InspectionLimits::default()
            }),
        ),
        Err(
            VariableUnavailableReason::InspectionLimit(crate::InspectionExhaustion {
                resource: crate::InspectionLimit::MemoryReads,
                limit: 64,
                used: 64,
                requested: 1,
            })
            .into()
        )
    );
    assert_eq!(runtime.memory_reads, 64);
}

#[test]
fn operational_memory_failures_escape_the_per_variable_result_lane() {
    let mut bytes = vec![gimli::DW_OP_addr.0];
    bytes.extend_from_slice(&0x1000_u64.to_le_bytes());
    bytes.extend_from_slice(&[gimli::DW_OP_deref.0, gimli::DW_OP_stack_value.0]);
    let mut runtime = Runtime::new([]);

    assert_eq!(
        run(&expression(&bytes), &units([]), &mut runtime),
        Err(EvaluateError::Fatal("unexpected memory read".into()))
    );
    assert!(matches!(
        evaluate_error_state(
            EvaluateError::Fatal("ptrace failed".into()),
            VariableMalformedKind::InvalidExpression
        ),
        Err(Error::VariableRuntime(description)) if description.as_ref() == "ptrace failed"
    ));
}

#[test]
fn fixed_form_constants_zero_extend_and_signed_forms_sign_extend() {
    let little = target(ByteOrder::Little);
    // A fixed data form supplies zero high bits: DW_FORM_data1 0xff for a
    // signed 4-byte type is 255; producers use DW_FORM_sdata for -1.
    assert_eq!(
        materialize_constant(&ConstantValue::Fixed(0xff), 4, little)
            .expect("zero-extended fixed-form constant")
            .as_ref(),
        &[0xff, 0x00, 0x00, 0x00]
    );
    assert_eq!(
        materialize_constant(&ConstantValue::Signed(-1), 4, little)
            .expect("sign-extended constant")
            .as_ref(),
        &[0xff, 0xff, 0xff, 0xff]
    );
    // Clang writes an unsigned short's 65535 as the 64-bit sign extension
    // of its pattern, in an unsigned form.
    assert_eq!(
        materialize_constant(&ConstantValue::Unsigned(u128::from(u64::MAX)), 2, little)
            .expect("a sign-extended pattern in an unsigned form")
            .as_ref(),
        &[0xff, 0xff]
    );
    // A value that is neither the pattern nor its extension does not fit.
    assert!(materialize_constant(&ConstantValue::Unsigned(0x1_0000), 2, little).is_err());
    assert!(
        materialize_constant(
            &ConstantValue::Unsigned(u128::from(u64::MAX - 1) << 1),
            2,
            little
        )
        .is_err()
    );
}

#[test]
fn unexecuted_fbreg_branches_do_not_require_a_frame_base() {
    let mut runtime = Runtime::new([]);
    // DW_OP_lit1 then DW_OP_bra +2 skips the DW_OP_fbreg on the executed
    // path; the frame base must not be resolved eagerly.
    let branching = expression(&[
        gimli::DW_OP_lit1.0,
        gimli::DW_OP_bra.0,
        0x02,
        0x00,
        gimli::DW_OP_fbreg.0,
        0x00,
        gimli::DW_OP_lit0.0,
        gimli::DW_OP_stack_value.0,
    ]);
    let location = Metadata::Absent(MetadataAbsence::NoFrameBase);
    let mut cache = FrameBaseCache::Empty;
    let pieces = evaluate(
        &branching,
        RunTimeEndian::Little,
        &mut FrameBase::Lazy(FrameBaseContext {
            location: &location,
            address: Some(ImageAddress::new(0)),
            cache: &mut cache,
        }),
        &units([]),
        &mut runtime,
        &mut InspectionBudget::default(),
    )
    .expect("executed path never needs the frame base");
    assert!(matches!(
        pieces.as_slice(),
        [gimli::Piece {
            location: Location::Value {
                value: Value::Generic(0)
            },
            ..
        }]
    ));
    assert!(
        matches!(cache, FrameBaseCache::Empty),
        "frame base must not be resolved for an unexecuted branch"
    );

    // The same expression taking the fbreg path surfaces the metadata
    // failure lazily.
    let taken = expression(&[gimli::DW_OP_fbreg.0, 0x00, gimli::DW_OP_stack_value.0]);
    let result = evaluate(
        &taken,
        RunTimeEndian::Little,
        &mut FrameBase::Lazy(FrameBaseContext {
            location: &location,
            address: Some(ImageAddress::new(0)),
            cache: &mut cache,
        }),
        &units([]),
        &mut runtime,
        &mut InspectionBudget::default(),
    );
    assert!(matches!(result, Err(EvaluateError::Unavailable(_))));
}

#[test]
fn specific_location_entries_override_default_entries() {
    let range = |start: u64, end: u64| {
        Some(AddressRange {
            start: ImageAddress::new(start),
            end: ImageAddress::new(end),
        })
    };
    let description = LocationDescription {
        entries: vec![
            LocationEntry {
                range: None,
                expression: expression(&[gimli::DW_OP_reg0.0]),
            },
            LocationEntry {
                range: range(0x100, 0x200),
                expression: expression(&[gimli::DW_OP_reg1.0]),
            },
        ]
        .into(),
    };

    let specific = description
        .expression(Some(ImageAddress::new(0x150)))
        .expect("specific entry wins inside its range")
        .expect("an expression is active");
    assert_eq!(specific.bytes.as_ref(), &[gimli::DW_OP_reg1.0]);

    let fallback = description
        .expression(Some(ImageAddress::new(0x300)))
        .expect("default entry applies outside all ranges")
        .expect("an expression is active");
    assert_eq!(fallback.bytes.as_ref(), &[gimli::DW_OP_reg0.0]);

    // A range-less default still resolves without an instruction context.
    let without_context = description
        .expression(None)
        .expect("default entry applies without a context")
        .expect("an expression is active");
    assert_eq!(without_context.bytes.as_ref(), &[gimli::DW_OP_reg0.0]);

    // A location with only range-gated entries must refuse to guess when no
    // instruction context is available rather than silently resolving.
    let ranged_only = LocationDescription {
        entries: vec![LocationEntry {
            range: range(0x100, 0x200),
            expression: expression(&[gimli::DW_OP_reg1.0]),
        }]
        .into(),
    };
    assert!(ranged_only.expression(None).is_err());

    let overlapping = LocationDescription {
        entries: vec![
            LocationEntry {
                range: range(0x100, 0x200),
                expression: expression(&[gimli::DW_OP_reg0.0]),
            },
            LocationEntry {
                range: range(0x180, 0x280),
                expression: expression(&[gimli::DW_OP_reg1.0]),
            },
        ]
        .into(),
    };
    assert!(
        overlapping
            .expression(Some(ImageAddress::new(0x190)))
            .is_err()
    );
}

#[test]
fn scalar_decoding_obeys_bit_width_sign_and_target_byte_order() {
    for (bytes, byte_order, signed, unsigned) in [
        (&[0xfe][..], ByteOrder::Little, -2_i128, 254_u128),
        (&[0xfe, 0xff], ByteOrder::Little, -2, 65_534),
        (&[0xff, 0xfe], ByteOrder::Big, -2, 65_534),
        (
            &[0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
            ByteOrder::Little,
            -2,
            u128::from(u64::MAX - 1),
        ),
    ] {
        assert_eq!(
            decode_scalar(
                &scalar_type(BaseTypeEncoding::Signed, bytes.len() as u64),
                bytes,
                target(byte_order),
            )
            .expect("signed scalar"),
            ScalarValue::Signed(signed)
        );
        assert_eq!(
            decode_scalar(
                &scalar_type(BaseTypeEncoding::Unsigned, bytes.len() as u64),
                bytes,
                target(byte_order),
            )
            .expect("unsigned scalar"),
            ScalarValue::Unsigned(unsigned)
        );
    }

    // A sub-byte integer masks its padding and sign-extends from its width.
    let base = |encoding, bit_size| BaseType {
        name: "bits".into(),
        base_name: "bits".into(),
        encoding,
        byte_size: 1,
        bit_size: Some(bit_size),
    };
    assert_eq!(
        decode_integer_value(
            &base(BaseTypeEncoding::Unsigned, 3),
            &[0xff],
            ByteOrder::Little
        )
        .unwrap(),
        IntegerValue::Unsigned(7)
    );
    assert_eq!(
        decode_integer_value(
            &base(BaseTypeEncoding::Signed, 3),
            &[0x05],
            ByteOrder::Little
        )
        .unwrap(),
        IntegerValue::Signed(-3)
    );
}

#[test]
fn pointer_decoding_obeys_target_width_and_byte_order_without_truncation() {
    assert_eq!(
        decode_address(&[0x78, 0x56, 0x34, 0x12], 4, target(ByteOrder::Little),)
            .expect("little-endian 32-bit address"),
        VirtualAddress::new(0x1234_5678),
    );
    assert_eq!(
        decode_address(&[0x12, 0x34, 0x56, 0x78], 4, target(ByteOrder::Big),)
            .expect("big-endian 32-bit address"),
        VirtualAddress::new(0x1234_5678),
    );
    assert_eq!(
        decode_address(
            &0x0123_4567_89ab_cdef_u64.to_le_bytes(),
            8,
            target(ByteOrder::Little),
        )
        .expect("64-bit address"),
        VirtualAddress::new(0x0123_4567_89ab_cdef),
    );
    assert!(decode_address(&[0; 9], 9, target(ByteOrder::Little)).is_err());
    assert!(decode_address(&[0; 4], 8, target(ByteOrder::Little)).is_err());
}

#[test]
fn implicit_pointer_offsets_are_bounded_and_never_return_partial_values() {
    assert_eq!(implicit_pointer_range(4, 4, 8), Ok((4, 8)));
    for (offset, size) in [(-1, 4), (6, 4)] {
        assert!(
            matches!(
                implicit_pointer_range(offset, size, 8),
                Err(VariableUnavailableReason::ValueAccess(
                    crate::ValueAccessUnavailableReason::ImplicitPointerOutOfBounds { .. }
                ))
            ),
            "{offset} {size}"
        );
    }
    assert_eq!(
        implicit_pointer_range(i64::MAX, u64::MAX, 8),
        Err(VariableUnavailableReason::EvaluationLimit)
    );
}

#[test]
fn static_member_layouts_cannot_escape_their_containing_record() {
    let bits = |bit_offset, bit_size| RecordMemberLayout::BitRange {
        bit_offset,
        bit_size,
    };
    for (record, member, layout, valid) in [
        (Some(8), Some(4), RecordMemberLayout::ByteOffset(4), true),
        (Some(8), Some(4), RecordMemberLayout::ByteOffset(5), false),
        (
            Some(u64::MAX),
            Some(2),
            RecordMemberLayout::ByteOffset(u64::MAX),
            false,
        ),
        (Some(8), Some(1), bits(63, 1), true),
        (Some(8), Some(1), bits(63, 2), false),
        (None, Some(1), RecordMemberLayout::ByteOffset(0), false),
        (None, Some(1), RecordMemberLayout::Runtime, true),
    ] {
        assert_eq!(
            static_member_layout_is_valid(record, member, layout),
            valid,
            "{record:?} {member:?} {layout:?}"
        );
    }
}

#[test]
fn value_shapes_separate_malformed_from_unsupported_types() {
    let pointer = |byte_size, address_class| {
        node(
            0,
            "p *",
            byte_size,
            TypeKind::Pointer {
                target: Some(reference(0)),
                address_class,
            },
        )
    };
    let representation = node(
        1,
        "representation",
        Some(8),
        TypeKind::Opaque {
            description: "test representation".into(),
        },
    );
    let encoded = node(
        0,
        "encoded",
        Some(4),
        TypeKind::Named {
            target: Some(reference(1)),
            relationship: NamedTypeRelationship::Encoding,
        },
    );
    let shared = node(
        0,
        "shared representation",
        Some(8),
        TypeKind::Modified {
            modifier: TypeModifier::Shared,
            target: reference(1),
        },
    );
    let broken = TypeEntry::Malformed("broken representation".into());
    let empty = node(
        0,
        "empty",
        Some(0),
        TypeKind::Base(scalar_type(BaseTypeEncoding::Unsigned, 0)),
    );
    for (types, malformed, message) in [
        (wrapper_cycle().to_vec(), true, "type wrapper cycle"),
        (
            vec![encoded.clone(), representation.clone()],
            false,
            "transparent type wrapper size 4 differs from target size 8",
        ),
        (vec![encoded, broken.clone()], true, "broken representation"),
        (
            vec![shared.clone(), representation],
            false,
            "shared-qualified values require UPC distributed-memory semantics",
        ),
        (vec![shared, broken], true, "broken representation"),
        (
            vec![pointer(None, 2)],
            false,
            "pointer representation for address class 2 is unsupported",
        ),
        (
            vec![pointer(None, 0)],
            true,
            "pointer type has no byte size",
        ),
        (
            vec![pointer(Some(0), 0)],
            true,
            "pointer type has a zero byte size",
        ),
        (vec![empty], true, "base type has a zero byte size"),
        (
            vec![pointer(Some(16), 0)],
            false,
            "pointer type occupies 16 bytes; addresses wider than 8 bytes are unsupported",
        ),
    ] {
        let error = value_shape_from(&types, TypeId::new(0)).unwrap_err();
        let (ValueShapeError::Malformed(description) | ValueShapeError::Unsupported(description)) =
            &error;
        assert_eq!(
            (
                matches!(error, ValueShapeError::Malformed(_)),
                description.as_ref()
            ),
            (malformed, message)
        );
    }

    // A pointer may point to itself.
    assert!(matches!(
        value_shape_from(&[pointer(Some(8), 0)], TypeId::new(0)),
        Ok(ValueShape::Indirection {
            target: Some(id),
            byte_size: 8,
            address_class: 0,
        }) if id == TypeId::new(0)
    ));
}

#[test]
fn graph_finalization_propagates_wrapper_sizes_to_a_fixpoint() {
    let mut types = [
        node(
            0,
            "outer",
            None,
            TypeKind::Named {
                target: Some(reference(1)),
                relationship: NamedTypeRelationship::Synonym,
            },
        ),
        node(
            1,
            "const inner",
            None,
            TypeKind::Modified {
                modifier: TypeModifier::Const,
                target: reference(2),
            },
        ),
        node(
            2,
            "inner",
            Some(8),
            TypeKind::Opaque {
                description: "test representation".into(),
            },
        ),
    ];

    propagate_wrapper_sizes(&mut types);

    assert!(
        types
            .iter()
            .all(|entry| matches!(entry, TypeEntry::Resolved(info) if info.byte_size == Some(8))),
        "{types:?}"
    );
}

#[test]
fn inline_cycle_analysis_distinguishes_storage_from_indirection() {
    let mut cycle_nodes = inline_storage_cycle_nodes(&wrapper_cycle());
    cycle_nodes.sort_unstable();
    assert_eq!(cycle_nodes, [0, 1]);

    let pointer_recursion = [
        node(
            0,
            "node",
            Some(8),
            TypeKind::Record {
                kind: RecordKind::Struct,
                members: Arc::from([RecordMember {
                    name: Some("next".into()),
                    type_ref: reference(1),
                    layout: RecordMemberLayout::ByteOffset(0),
                    accessibility: Accessibility::Public,
                    artificial: false,
                    embedded: false,
                    declaration: None,
                }]),
                bases: Arc::default(),
                incomplete: false,
            },
        ),
        node(
            1,
            "node *",
            Some(8),
            TypeKind::Pointer {
                target: Some(reference(0)),
                address_class: 0,
            },
        ),
    ];
    assert!(inline_storage_cycle_nodes(&pointer_recursion).is_empty());
}

#[test]
fn boolean_and_float_decoding_preserve_exact_representations() {
    let little = target(ByteOrder::Little);
    assert_eq!(
        decode_scalar(&scalar_type(BaseTypeEncoding::Boolean, 1), &[0], little,).expect("false"),
        ScalarValue::Boolean(false)
    );
    assert_eq!(
        decode_scalar(&scalar_type(BaseTypeEncoding::Boolean, 1), &[2], little),
        Err(ScalarDecodeError::Invalid(
            VariableInvalidReason::BooleanRepresentation(2)
        ))
    );
    assert_eq!(
        decode_scalar(
            &scalar_type(BaseTypeEncoding::Floating, 4),
            &1.25_f32.to_bits().to_le_bytes(),
            little,
        )
        .expect("binary32"),
        ScalarValue::Floating(FloatValue::Binary32(1.25_f32.to_bits()))
    );
    assert_eq!(
        decode_scalar(
            &scalar_type(BaseTypeEncoding::Floating, 8),
            &(-0.0_f64).to_bits().to_le_bytes(),
            little,
        )
        .expect("binary64"),
        ScalarValue::Floating(FloatValue::Binary64((-0.0_f64).to_bits()))
    );

    let mut extended = [0xa5_u8; 16];
    extended[..8].copy_from_slice(&0xc800_0000_0000_0000_u64.to_le_bytes());
    extended[8..10].copy_from_slice(&0x4000_u16.to_le_bytes());
    let sixteen_bytes = |name: &str| BaseType {
        base_name: name.into(),
        ..scalar_type(BaseTypeEncoding::Floating, 16)
    };
    assert_eq!(
        decode_scalar(&sixteen_bytes("long double"), &extended, little).expect("x87 extended"),
        ScalarValue::Floating(FloatValue::X87Extended {
            significand: 0xc800_0000_0000_0000,
            sign_exponent: 0x4000,
        })
    );
    // IEEE binary128 shares the size but not the representation.
    for name in ["__float128", "_Float128", "f128"] {
        assert!(
            matches!(
                decode_scalar(&sixteen_bytes(name), &extended, little),
                Err(ScalarDecodeError::Unavailable(_))
            ),
            "{name}"
        );
    }
}

#[test]
fn bit_field_extraction_is_endian_aware_and_bounded() {
    assert_eq!(
        extract_bit_field(&[0b1010_1101], 0, 3, ByteOrder::Little).expect("little field"),
        0b101
    );
    assert_eq!(
        extract_bit_field(&[0b1010_1101], 0, 3, ByteOrder::Big).expect("big field"),
        0b101
    );
    assert_eq!(
        extract_bit_field(&[0b1010_1101], 3, 5, ByteOrder::Little).expect("little tail"),
        0b10101
    );
    assert_eq!(
        extract_bit_field(&[0b1010_1101], 3, 5, ByteOrder::Big).expect("big tail"),
        0b01101
    );
    assert!(extract_bit_field(&[0], 7, 2, ByteOrder::Little).is_err());
    assert!(extract_bit_field(&[0], 0, 0, ByteOrder::Little).is_err());
}

/// A global whose type has no usable shape is a malformed type graph, as a
/// local, a member, or a pointee of that type is.
#[test]
fn a_global_of_a_malformed_type_reports_a_malformed_type_graph() {
    use object::write::Object;

    let encoding = Encoding {
        format: Format::Dwarf32,
        version: 5,
        address_size: 8,
    };
    let mut written = WriteDwarf::new();
    let unit_id = written.units.add(Unit::new(encoding, LineProgram::none()));
    let unit = written.units.get_mut(unit_id);
    let root = unit.root();
    unit.get_mut(root).set(
        gimli::DW_AT_language,
        WriteAttributeValue::Language(gimli::DW_LANG_C11),
    );
    let empty = unit.add(root, gimli::DW_TAG_pointer_type);
    unit.get_mut(empty)
        .set(gimli::DW_AT_byte_size, WriteAttributeValue::Udata(0));
    let global = unit.add(root, gimli::DW_TAG_variable);
    unit.get_mut(global).set(
        gimli::DW_AT_name,
        WriteAttributeValue::String(b"nothing".to_vec()),
    );
    unit.get_mut(global)
        .set(gimli::DW_AT_type, WriteAttributeValue::UnitRef(empty));
    unit.get_mut(global)
        .set(gimli::DW_AT_external, WriteAttributeValue::Flag(true));
    let mut location = gimli::write::Expression::new();
    location.op_addr(gimli::write::Address::Constant(0x10));
    unit.get_mut(global).set(
        gimli::DW_AT_location,
        WriteAttributeValue::Exprloc(location),
    );
    let mut sections = Sections::new(EndianVec::new(LittleEndian));
    written.write(&mut sections).expect("write test DWARF");

    let mut elf = Object::new(
        object::BinaryFormat::Elf,
        object::Architecture::X86_64,
        object::Endianness::Little,
    );
    let data = elf.add_section(Vec::new(), b".data".to_vec(), object::SectionKind::Data);
    elf.append_section_data(data, &[0; 0x20], 8);
    sections
        .for_each(|id, section| -> std::result::Result<(), ()> {
            if !section.slice().is_empty() {
                let debug = elf.add_section(
                    Vec::new(),
                    id.name().as_bytes().to_vec(),
                    object::SectionKind::Debug,
                );
                elf.append_section_data(debug, section.slice(), 1);
            }
            Ok(())
        })
        .expect("add the DWARF sections");
    let bytes = elf.write().expect("write the test object");
    let debug_info = crate::debug_info::load_bytes(std::path::Path::new("malformed.o"), &bytes)
        .expect("load the test object");
    assert_eq!(debug_info.image.globals().len(), 1);

    let mut runtime = Runtime::new([]);
    runtime.memory = Some(Arc::from([0_u8; 0x20]));
    let context = crate::debug_info::VariableContext {
        stop_id: crate::StopId::new(1),
        thread: crate::ThreadId::new(1),
        frame: crate::StackFrameId::new(0),
        module: crate::ModuleId::new(0),
        image: ModuleImageId::new(0),
        address: None,
    };
    let variable = debug_info
        .variables
        .inspect_global(
            GlobalVariableId::new(0),
            None,
            context,
            &mut runtime,
            &mut InspectionBudget::default(),
        )
        .expect("inspect the global");
    let VariableState::Malformed(reason) = &variable.state else {
        panic!("{variable:?}");
    };
    assert_eq!(
        (reason.kind, reason.description.as_ref()),
        (
            VariableMalformedKind::InvalidTypeGraph,
            "pointer type has a zero byte size"
        )
    );
}

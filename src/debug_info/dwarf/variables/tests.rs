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
    evaluate_frame_base, incomplete_piece_reason, materialize_constant, materialize_pieces,
};
use super::globals::{DefinitionIndex, DefinitionResolution};
use super::inspect::{
    ArrayIndexCalculationError, ScalarDecodeError, implicit_pointer_range, path_error_state,
    row_major_array_index, static_member_layout_is_valid,
};
use super::location::{EvaluationUnit, Expression, LocationDescription, LocationEntry};
use super::shape::{ValueShape, ValueShapeError, ValueShapeKind, value_shape_from};
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
fn row_major_array_indices_honor_lower_bounds_and_reject_overflow() {
    let dimensions = [
        ArrayDimension {
            lower_bound: -2,
            count: 3,
        },
        ArrayDimension {
            lower_bound: 10,
            count: 2,
        },
    ];
    assert_eq!(row_major_array_index(&dimensions, &[0, 11]), Ok(5));
    assert!(matches!(
        row_major_array_index(&dimensions, &[-3, 10]),
        Err(ArrayIndexCalculationError::OutOfBounds {
            index: -3,
            lower_bound: -2,
            count: 3,
        })
    ));
    assert!(matches!(
        row_major_array_index(&dimensions, &[0, 12]),
        Err(ArrayIndexCalculationError::OutOfBounds {
            index: 12,
            lower_bound: 10,
            count: 2,
        })
    ));

    let overflowing = [
        ArrayDimension {
            lower_bound: 0,
            count: u64::MAX,
        },
        ArrayDimension {
            lower_bound: 0,
            count: 2,
        },
    ];
    assert_eq!(
        row_major_array_index(&overflowing, &[i128::from(u64::MAX - 1), 1]),
        Err(ArrayIndexCalculationError::Overflow)
    );
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
fn sub_byte_integer_representations_mask_and_sign_extend() {
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
fn frame_base_register_and_fbreg_location_have_distinct_meanings() {
    let mut runtime = Runtime {
        registers: BTreeMap::from([(6, 0x2000)]),
        cfa: Ok(VirtualAddress::new(0x3000)),
        memory: None,
        memory_reads: 0,
    };
    let frame_base = evaluate_frame_base(
        &expression(&[gimli::DW_OP_reg6.0]),
        RunTimeEndian::Little,
        &units([]),
        &mut runtime,
        &mut InspectionBudget::default(),
    )
    .expect("register-valued frame base");
    assert_eq!(frame_base, VirtualAddress::new(0x2000));

    let fbreg = expression(&[gimli::DW_OP_fbreg.0, 0x70]);
    let location = Metadata::Value(LocationDescription {
        entries: vec![LocationEntry {
            range: None,
            expression: expression(&[gimli::DW_OP_reg6.0]),
        }]
        .into(),
    });
    let mut cache = FrameBaseCache::Empty;
    let pieces = evaluate(
        &fbreg,
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
    .expect("frame-relative memory location");
    assert!(matches!(cache, FrameBaseCache::Available(_)));
    assert!(matches!(
        pieces.as_slice(),
        [gimli::Piece {
            location: Location::Address { address: 0x1ff0 },
            ..
        }]
    ));

    let direct_register = expression(&[gimli::DW_OP_reg6.0]);
    let pieces = evaluate(
        &direct_register,
        RunTimeEndian::Little,
        &mut FrameBase::Unsupported,
        &units([]),
        &mut runtime,
        &mut InspectionBudget::default(),
    )
    .expect("direct register location");
    assert!(matches!(
        pieces.as_slice(),
        [gimli::Piece {
            location: Location::Register {
                register: gimli::Register(6)
            },
            ..
        }]
    ));
}

#[test]
fn cfa_expression_limit_remains_a_typed_unavailable_reason() {
    let mut runtime = Runtime {
        registers: BTreeMap::new(),
        cfa: Err(VariableUnavailableReason::CallFrameUnavailable(
            crate::CallFrameUnavailableReason::NoInstructionContext,
        )),
        memory: None,
        memory_reads: 0,
    };
    assert_eq!(
        evaluate_frame_base(
            &expression(&[gimli::DW_OP_call_frame_cfa.0]),
            RunTimeEndian::Little,
            &units([]),
            &mut runtime,
            &mut InspectionBudget::default(),
        ),
        Err(EvaluateError::Unavailable(
            VariableUnavailableReason::CallFrameUnavailable(
                crate::CallFrameUnavailableReason::NoInstructionContext,
            )
        ))
    );
}

#[test]
fn malformed_backward_branch_expression_fails_instead_of_hanging() {
    let mut runtime = Runtime {
        registers: BTreeMap::new(),
        cfa: Ok(VirtualAddress::new(0x3000)),
        memory: None,
        memory_reads: 0,
    };
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
    let mut runtime = Runtime {
        registers: BTreeMap::from([(6, 0x2000)]),
        cfa: Ok(VirtualAddress::new(0x3000)),
        memory: None,
        memory_reads: 0,
    };
    // DW_OP_regval_type register 6, base type DIE offset 0x10.
    let typed = expression(&[
        gimli::DW_OP_regval_type.0,
        6,
        0x10,
        gimli::DW_OP_stack_value.0,
    ]);
    let result = evaluate(
        &typed,
        RunTimeEndian::Little,
        &mut FrameBase::Unsupported,
        &units([(0x10, gimli::ValueType::U64)]),
        &mut runtime,
        &mut InspectionBudget::default(),
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
    let mut runtime = Runtime {
        registers: BTreeMap::new(),
        cfa: Ok(VirtualAddress::new(0x3000)),
        memory: None,
        memory_reads: 0,
    };
    let implicit = expression(&[gimli::DW_OP_implicit_value.0, 4, 0xd6, 0xff, 0xff, 0xff]);
    let pieces = evaluate(
        &implicit,
        RunTimeEndian::Little,
        &mut FrameBase::Unsupported,
        &units([]),
        &mut runtime,
        &mut InspectionBudget::default(),
    )
    .expect("implicit scalar expression");
    let materialized = materialize_pieces(
        &pieces,
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
fn undefined_location_pieces_report_exact_destination_ranges() {
    let pieces = [
        gimli::Piece::<Reader<'_>> {
            location: Location::Value {
                value: Value::Generic(0x12),
            },
            size_in_bits: Some(8),
            bit_offset: None,
        },
        gimli::Piece::<Reader<'_>> {
            location: Location::Empty,
            size_in_bits: Some(16),
            bit_offset: None,
        },
        gimli::Piece::<Reader<'_>> {
            location: Location::Value {
                value: Value::Generic(0x34),
            },
            size_in_bits: Some(8),
            bit_offset: None,
        },
    ];

    assert_eq!(
        incomplete_piece_reason(&pieces, 32),
        Ok(Some(VariableUnavailableReason::OptimizedOut(
            crate::OptimizedOutReason::UndefinedPieces {
                ranges: Arc::from([crate::ValueBitRange {
                    offset: 8,
                    size: 16,
                }]),
            },
        )))
    );
    assert!(matches!(
        incomplete_piece_reason(&pieces, 24),
        Err(EvaluateError::Malformed(_))
    ));
}

#[test]
fn an_empty_location_expression_describes_an_optimized_out_value() {
    let empty = expression(&[]);
    let pieces = evaluate(
        &empty,
        RunTimeEndian::Little,
        &mut FrameBase::Unsupported,
        &units([]),
        &mut Runtime {
            registers: BTreeMap::new(),
            cfa: Ok(VirtualAddress::new(0x3000)),
            memory: None,
            memory_reads: 0,
        },
        &mut InspectionBudget::default(),
    )
    .expect("an empty expression is valid");

    assert_eq!(
        incomplete_piece_reason(&pieces, 64),
        Ok(Some(VariableUnavailableReason::OptimizedOut(
            crate::OptimizedOutReason::EmptyLocation
        )))
    );
}

#[test]
fn deferred_operations_and_missing_types_have_stable_typed_reasons() {
    let mut runtime = Runtime {
        registers: BTreeMap::from([(0, 1)]),
        cfa: Ok(VirtualAddress::new(0x3000)),
        memory: None,
        memory_reads: 0,
    };
    let entry = expression(&[
        gimli::DW_OP_entry_value.0,
        1,
        gimli::DW_OP_reg0.0,
        gimli::DW_OP_stack_value.0,
    ]);
    assert_eq!(
        evaluate(
            &entry,
            RunTimeEndian::Little,
            &mut FrameBase::Unsupported,
            &units([]),
            &mut runtime,
            &mut InspectionBudget::default(),
        ),
        Err(crate::UnsupportedVariableFeature::EntryValue.into())
    );

    let missing_type = expression(&[
        gimli::DW_OP_regval_type.0,
        0,
        0x10,
        gimli::DW_OP_stack_value.0,
    ]);
    assert_eq!(
        evaluate(
            &missing_type,
            RunTimeEndian::Little,
            &mut FrameBase::Unsupported,
            &units([]),
            &mut runtime,
            &mut InspectionBudget::default(),
        ),
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
        registers: BTreeMap::new(),
        cfa: Ok(VirtualAddress::new(0x3000)),
        memory: Some(Arc::from([0_u8; 16])),
        memory_reads: 0,
    };
    assert_eq!(
        evaluate(
            &expression,
            RunTimeEndian::Little,
            &mut FrameBase::Unsupported,
            &units([]),
            &mut runtime,
            &mut InspectionBudget::default(),
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
    let mut runtime = Runtime {
        registers: BTreeMap::new(),
        cfa: Ok(VirtualAddress::new(0x3000)),
        memory: None,
        memory_reads: 0,
    };

    assert_eq!(
        evaluate(
            &expression(&bytes),
            RunTimeEndian::Little,
            &mut FrameBase::Unsupported,
            &units([]),
            &mut runtime,
            &mut InspectionBudget::default(),
        ),
        Err(EvaluateError::Fatal("unexpected memory read".into()))
    );
    assert!(matches!(
        path_error_state(EvaluateError::Fatal("ptrace failed".into())),
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
    let mut runtime = Runtime {
        registers: BTreeMap::new(),
        cfa: Ok(VirtualAddress::new(0x3000)),
        memory: None,
        memory_reads: 0,
    };
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
fn scalar_decoding_obeys_width_sign_and_target_byte_order() {
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
    assert!(static_member_layout_is_valid(
        Some(8),
        Some(4),
        RecordMemberLayout::ByteOffset(4)
    ));
    assert!(!static_member_layout_is_valid(
        Some(8),
        Some(4),
        RecordMemberLayout::ByteOffset(5)
    ));
    assert!(!static_member_layout_is_valid(
        Some(u64::MAX),
        Some(2),
        RecordMemberLayout::ByteOffset(u64::MAX)
    ));
    assert!(static_member_layout_is_valid(
        Some(8),
        Some(1),
        RecordMemberLayout::BitRange {
            bit_offset: 63,
            bit_size: 1,
        }
    ));
    assert!(!static_member_layout_is_valid(
        Some(8),
        Some(1),
        RecordMemberLayout::BitRange {
            bit_offset: 63,
            bit_size: 2,
        }
    ));
    assert!(!static_member_layout_is_valid(
        None,
        Some(1),
        RecordMemberLayout::ByteOffset(0)
    ));
    assert!(static_member_layout_is_valid(
        None,
        Some(1),
        RecordMemberLayout::Runtime
    ));
}

#[test]
fn type_graph_rejects_wrapper_cycles_but_permits_recursive_pointer_edges() {
    let image = ModuleImageId::new(7);
    let reference = |id| TypeReference {
        image,
        id: TypeId::new(id),
    };
    let cycle = [
        TypeEntry::Resolved(TypeInfo {
            reference: reference(0),
            name: "left".into(),
            byte_size: Some(8),
            kind: TypeKind::Named {
                target: Some(reference(1)),
                relationship: NamedTypeRelationship::Synonym,
            },
        }),
        TypeEntry::Resolved(TypeInfo {
            reference: reference(1),
            name: "right".into(),
            byte_size: Some(8),
            kind: TypeKind::Modified {
                modifier: TypeModifier::Const,
                target: reference(0),
            },
        }),
    ];
    let cycle_error = value_shape_from(&cycle, TypeId::new(0)).unwrap_err();
    assert!(
        matches!(&cycle_error, ValueShapeError::Malformed(description) if description.as_ref() == "type wrapper cycle"),
        "wrapper cycle must classify as malformed metadata",
    );

    let recursive_pointer = [TypeEntry::Resolved(TypeInfo {
        reference: reference(0),
        name: "node *".into(),
        byte_size: Some(8),
        kind: TypeKind::Pointer {
            target: Some(reference(0)),
            address_class: 0,
        },
    })];
    assert!(matches!(
        value_shape_from(&recursive_pointer, TypeId::new(0)),
        Ok(ValueShape { kind: ValueShapeKind::Indirection {
            target: Some(id),
            byte_size: 8,
            address_class: 0,
        }, .. }) if id == TypeId::new(0)
    ));
}

#[test]
fn graph_finalization_propagates_wrapper_sizes_to_a_fixpoint() {
    let image = ModuleImageId::new(7);
    let reference = |id| TypeReference {
        image,
        id: TypeId::new(id),
    };
    let mut types = [
        TypeEntry::Resolved(TypeInfo {
            reference: reference(0),
            name: "outer".into(),
            byte_size: None,
            kind: TypeKind::Named {
                target: Some(reference(1)),
                relationship: NamedTypeRelationship::Synonym,
            },
        }),
        TypeEntry::Resolved(TypeInfo {
            reference: reference(1),
            name: "const inner".into(),
            byte_size: None,
            kind: TypeKind::Modified {
                modifier: TypeModifier::Const,
                target: reference(2),
            },
        }),
        TypeEntry::Resolved(TypeInfo {
            reference: reference(2),
            name: "inner".into(),
            byte_size: Some(8),
            kind: TypeKind::Opaque {
                description: "test representation".into(),
            },
        }),
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
    let image = ModuleImageId::new(7);
    let reference = |id| TypeReference {
        image,
        id: TypeId::new(id),
    };
    let by_value_cycle = [
        TypeEntry::Resolved(TypeInfo {
            reference: reference(0),
            name: "left".into(),
            byte_size: Some(8),
            kind: TypeKind::Named {
                target: Some(reference(1)),
                relationship: NamedTypeRelationship::Synonym,
            },
        }),
        TypeEntry::Resolved(TypeInfo {
            reference: reference(1),
            name: "right".into(),
            byte_size: Some(8),
            kind: TypeKind::Modified {
                modifier: TypeModifier::Const,
                target: reference(0),
            },
        }),
    ];
    let mut cycle_nodes = inline_storage_cycle_nodes(&by_value_cycle);
    cycle_nodes.sort_unstable();
    assert_eq!(cycle_nodes, [0, 1]);

    let pointer_recursion = [
        TypeEntry::Resolved(TypeInfo {
            reference: reference(0),
            name: "node".into(),
            byte_size: Some(8),
            kind: TypeKind::Record {
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
        }),
        TypeEntry::Resolved(TypeInfo {
            reference: reference(1),
            name: "node *".into(),
            byte_size: Some(8),
            kind: TypeKind::Pointer {
                target: Some(reference(0)),
                address_class: 0,
            },
        }),
    ];
    assert!(inline_storage_cycle_nodes(&pointer_recursion).is_empty());
}

#[test]
fn transparent_wrappers_reject_incompatible_storage_sizes() {
    let image = ModuleImageId::new(7);
    let reference = |id| TypeReference {
        image,
        id: TypeId::new(id),
    };
    let types = [
        TypeEntry::Resolved(TypeInfo {
            reference: reference(0),
            name: "encoded".into(),
            byte_size: Some(4),
            kind: TypeKind::Named {
                target: Some(reference(1)),
                relationship: NamedTypeRelationship::Encoding,
            },
        }),
        TypeEntry::Resolved(TypeInfo {
            reference: reference(1),
            name: "representation".into(),
            byte_size: Some(8),
            kind: TypeKind::Opaque {
                description: "test representation".into(),
            },
        }),
    ];

    assert!(matches!(
        value_shape_from(&types, TypeId::new(0)),
        Err(ValueShapeError::Unsupported(description))
            if description.contains("wrapper size 4 differs from target size 8")
    ));

    let malformed_target = [
        types[0].clone(),
        TypeEntry::Malformed("broken representation".into()),
    ];
    assert!(matches!(
        value_shape_from(&malformed_target, TypeId::new(0)),
        Err(ValueShapeError::Malformed(description))
            if description.as_ref() == "broken representation"
    ));

    let shared = [
        TypeEntry::Resolved(TypeInfo {
            reference: reference(0),
            name: "shared representation".into(),
            byte_size: Some(8),
            kind: TypeKind::Modified {
                modifier: TypeModifier::Shared,
                target: reference(1),
            },
        }),
        types[1].clone(),
    ];
    assert!(matches!(
        value_shape_from(&shared, TypeId::new(0)),
        Err(ValueShapeError::Unsupported(description))
            if description.contains("distributed-memory semantics")
    ));
    let malformed_shared_target = [
        shared[0].clone(),
        TypeEntry::Malformed("broken shared representation".into()),
    ];
    assert!(matches!(
        value_shape_from(&malformed_shared_target, TypeId::new(0)),
        Err(ValueShapeError::Malformed(description))
            if description.as_ref() == "broken shared representation"
    ));
}

#[test]
fn sizeless_pointers_separate_unsupported_address_classes_from_defective_metadata() {
    let reference = |id| TypeReference {
        image: ModuleImageId::new(7),
        id: TypeId::new(id),
    };
    let sizeless = |address_class| {
        [TypeEntry::Resolved(TypeInfo {
            reference: reference(0),
            name: "opaque *".into(),
            byte_size: None,
            kind: TypeKind::Pointer {
                target: Some(reference(0)),
                address_class,
            },
        })]
    };

    // A non-default address class the backend cannot size is valid but
    // unsupported metadata, not a defect.
    let unsupported = value_shape_from(&sizeless(2), TypeId::new(0)).unwrap_err();
    assert!(
        matches!(&unsupported, ValueShapeError::Unsupported(description)
            if description.contains("address class 2")),
        "non-default address class without a size must be unsupported: {unsupported:?}",
    );

    // A missing size under the default address class is an internal
    // inconsistency the builder never emits, so it stays malformed.
    let malformed = value_shape_from(&sizeless(0), TypeId::new(0)).unwrap_err();
    assert!(
        matches!(&malformed, ValueShapeError::Malformed(description)
            if description.as_ref() == "pointer type has no byte size"),
        "default address class without a size must be malformed: {malformed:?}",
    );

    // A zero-byte indirection cannot hold an address, so it is defective
    // rather than a usable shape that would later read zero bytes.
    let zero_sized = [TypeEntry::Resolved(TypeInfo {
        reference: reference(0),
        name: "opaque *".into(),
        byte_size: Some(0),
        kind: TypeKind::Pointer {
            target: Some(reference(0)),
            address_class: 0,
        },
    })];
    let zero_error = value_shape_from(&zero_sized, TypeId::new(0)).unwrap_err();
    assert!(
        matches!(&zero_error, ValueShapeError::Malformed(description)
            if description.as_ref() == "pointer type has a zero byte size"),
        "zero-sized pointer must be malformed: {zero_error:?}",
    );

    // A scalar encoding likewise cannot occupy zero bytes.
    let zero_scalar = [TypeEntry::Resolved(TypeInfo {
        reference: reference(0),
        name: "empty".into(),
        byte_size: Some(0),
        kind: TypeKind::Base(scalar_type(BaseTypeEncoding::Unsigned, 0)),
    })];
    let zero_scalar_error = value_shape_from(&zero_scalar, TypeId::new(0)).unwrap_err();
    assert!(
        matches!(&zero_scalar_error, ValueShapeError::Malformed(description)
            if description.as_ref() == "base type has a zero byte size"),
        "zero-sized base type must be malformed: {zero_scalar_error:?}",
    );

    // A width wider than a decodable address is valid metadata this backend
    // cannot use, so it is unsupported rather than a usable shape that would
    // drive a doomed inferior read.
    let oversized = [TypeEntry::Resolved(TypeInfo {
        reference: reference(0),
        name: "wide *".into(),
        byte_size: Some(16),
        kind: TypeKind::Pointer {
            target: Some(reference(0)),
            address_class: 0,
        },
    })];
    let oversized_error = value_shape_from(&oversized, TypeId::new(0)).unwrap_err();
    assert!(
        matches!(&oversized_error, ValueShapeError::Unsupported(description)
            if description.contains("wider than")),
        "over-wide pointer must be unsupported: {oversized_error:?}",
    );
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

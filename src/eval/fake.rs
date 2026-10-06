//! A deterministic world of types, variables, memory, and registers that
//! the evaluator runs against in tests: a stand-in for data access with the
//! simplest layouts, not a second debug-info provider.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::error::ErrorKind;
use super::number::Exact;
use super::target::{
    Lookup, Machine, Planned, Refusal, Register, Scope, StepKind, Stop, TypeLookup, TypeQuery,
};
use super::types::{TypeSource, c_type_key_of_name};
use crate::{
    AddressValue, ArrayDimension, BaseType, BaseTypeEncoding, ByteOrder, DereferenceState,
    EnumerationOrigin, Enumerator, FloatValue, InspectedValue, InspectionCompletion,
    InspectionUsage, IntegerValue, ModuleImageId, NamedTypeRelationship, OptimizedOutReason,
    RecordKind, RecordMember, RecordMemberLayout, ScalarValue, TextCompletion, TextSummary, TypeId,
    TypeInfo, TypeKind, TypeModifier, TypeReference, ValueAccessUnavailableReason, ValueChildren,
    VariableState, VariableUnavailableReason, VariableValue, VariableValueSource, VirtualAddress,
};

const IMAGE: ModuleImageId = ModuleImageId::new(1);

#[derive(Debug, Clone)]
enum Storage {
    Memory(u64),
    Register { name: &'static str, bytes: Vec<u8> },
    OptimizedOut,
}

#[derive(Debug, Clone)]
struct Object {
    name: String,
    ty: TypeReference,
    storage: Storage,
}

/// Where a value of the world is.
#[derive(Debug, Clone)]
pub enum Place {
    Memory {
        address: u64,
        ty: TypeReference,
    },
    Register {
        name: &'static str,
        bytes: Vec<u8>,
        ty: TypeReference,
    },
}

impl Place {
    const fn ty(&self) -> TypeReference {
        match self {
            Self::Memory { ty, .. } | Self::Register { ty, .. } => *ty,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Step {
    Deref(TypeReference),
    Member {
        offset: u64,
        ty: TypeReference,
    },
    Array {
        dimensions: Arc<[ArrayDimension]>,
        element_size: u64,
        ty: TypeReference,
    },
    Slice {
        element_size: u64,
        ty: TypeReference,
    },
}

/// A world the evaluator binds and runs in.
#[derive(Debug, Clone, Default)]
pub struct World {
    types: Vec<TypeInfo>,
    objects: Vec<Object>,
    /// Mapped regions by their first address.
    memory: BTreeMap<u64, Vec<u8>>,
    /// Ranges that reading fails the test: what a short circuit skips.
    poisoned: Vec<(u64, u64)>,
    registers: BTreeMap<&'static str, Option<u128>>,
    next: u64,
    /// Every memory read, in order.
    pub reads: Vec<(u64, usize)>,
    /// Units of work left, when limited.
    pub work: Option<u64>,
}

impl World {
    pub fn new() -> Self {
        Self {
            next: 0x1000,
            ..Self::default()
        }
    }

    fn add(&mut self, name: &str, byte_size: Option<u64>, kind: TypeKind) -> TypeReference {
        let reference = TypeReference {
            image: IMAGE,
            id: TypeId::new(u32::try_from(self.types.len()).expect("few types")),
        };
        self.types.push(TypeInfo {
            reference,
            name: name.into(),
            byte_size,
            kind,
            identity: None,
        });
        reference
    }

    fn info(&self, ty: TypeReference) -> &TypeInfo {
        &self.types[usize::try_from(ty.id.get()).expect("small ids")]
    }

    pub fn base(
        &mut self,
        name: &str,
        encoding: BaseTypeEncoding,
        byte_size: u64,
    ) -> TypeReference {
        self.add(
            name,
            Some(byte_size),
            TypeKind::Base(BaseType {
                name: name.into(),
                base_name: name.into(),
                encoding,
                byte_size,
                bit_size: None,
            }),
        )
    }

    pub fn record(
        &mut self,
        name: &str,
        byte_size: u64,
        members: &[(&str, TypeReference, u64)],
    ) -> TypeReference {
        let members: Vec<RecordMember> = members
            .iter()
            .map(|(member, ty, offset)| RecordMember {
                name: Some((*member).into()),
                type_ref: *ty,
                layout: RecordMemberLayout::ByteOffset(*offset),
                accessibility: crate::Accessibility::Public,
                artificial: false,
                embedded: false,
                declaration: None,
            })
            .collect();
        self.add(
            name,
            Some(byte_size),
            TypeKind::Record {
                kind: RecordKind::Struct,
                members: members.into(),
                bases: Arc::from([]),
                incomplete: false,
            },
        )
    }

    pub fn pointer(&mut self, target: Option<TypeReference>) -> TypeReference {
        let name = target.map_or_else(
            || "void*".to_owned(),
            |target| format!("{}*", self.info(target).name),
        );
        self.add(
            &name,
            Some(8),
            TypeKind::Pointer {
                target,
                address_class: 0,
            },
        )
    }

    pub fn reference(&mut self, target: TypeReference) -> TypeReference {
        let name = format!("{}&", self.info(target).name);
        self.add(
            &name,
            Some(8),
            TypeKind::Reference {
                kind: crate::ReferenceKind::Lvalue,
                target,
                address_class: 0,
            },
        )
    }

    pub fn array(&mut self, element: TypeReference, counts: &[u64]) -> TypeReference {
        let size =
            self.info(element).byte_size.unwrap_or_default() * counts.iter().product::<u64>();
        let name = format!(
            "{}{}",
            self.info(element).name,
            counts.iter().fold(String::new(), |mut name, count| {
                use std::fmt::Write as _;
                let _ = write!(name, "[{count}]");
                name
            })
        );
        let dimensions: Vec<ArrayDimension> = counts
            .iter()
            .map(|&count| ArrayDimension {
                lower_bound: 0,
                count,
            })
            .collect();
        self.add(
            &name,
            Some(size),
            TypeKind::Array {
                element,
                dimensions: dimensions.into(),
            },
        )
    }

    pub fn slice(&mut self, element: TypeReference) -> TypeReference {
        let name = format!("[]{}", self.info(element).name);
        self.add(
            &name,
            Some(16),
            TypeKind::Slice {
                element,
                has_capacity: false,
                text: false,
            },
        )
    }

    pub fn enumeration(
        &mut self,
        name: &str,
        representation: TypeReference,
        values: &[(&str, i128)],
    ) -> TypeReference {
        let TypeKind::Base(base) = self.info(representation).kind.clone() else {
            panic!("an enumeration's representation is a base type");
        };
        let enumerators: Vec<Enumerator> = values
            .iter()
            .map(|(enumerator, value)| Enumerator {
                name: (*enumerator).into(),
                value: if matches!(
                    base.encoding,
                    BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter
                ) {
                    IntegerValue::Signed(*value)
                } else {
                    IntegerValue::Unsigned(value.cast_unsigned())
                },
            })
            .collect();
        let size = base.byte_size;
        self.add(
            name,
            Some(size),
            TypeKind::Enumeration {
                representation: base,
                underlying: Some(representation),
                enumerators: enumerators.into(),
                origin: EnumerationOrigin::Language,
                scoped: false,
            },
        )
    }

    /// Gives a type the identity a view's pattern matches.
    pub fn identify(
        &mut self,
        ty: TypeReference,
        language: crate::SourceLanguage,
        path: &[&str],
        base: &str,
        arguments: Vec<crate::TypeArgument>,
    ) {
        let index = usize::try_from(ty.id.get()).expect("small ids");
        self.types[index].identity = Some(Arc::new(crate::TypeIdentity {
            language,
            path: path.iter().map(|segment| Arc::from(*segment)).collect(),
            inline_namespaces: Arc::default(),
            base: base.into(),
            origin: if arguments.is_empty() {
                crate::ArgumentOrigin::None
            } else {
                crate::ArgumentOrigin::Dwarf
            },
            arguments: arguments.into(),
            go: None,
        }));
    }

    pub fn typedef(&mut self, name: &str, target: TypeReference) -> TypeReference {
        let size = self.info(target).byte_size;
        self.add(
            name,
            size,
            TypeKind::Named {
                target: Some(target),
                relationship: NamedTypeRelationship::Synonym,
            },
        )
    }

    pub fn constant(&mut self, target: TypeReference) -> TypeReference {
        let name = format!("const {}", self.info(target).name);
        let size = self.info(target).byte_size;
        self.add(
            &name,
            size,
            TypeKind::Modified {
                modifier: TypeModifier::Const,
                target,
            },
        )
    }

    /// Maps `bytes` at a fresh address, returning it.
    pub fn allocate(&mut self, bytes: &[u8]) -> u64 {
        let address = self.next;
        self.memory.insert(address, bytes.to_vec());
        self.next += (bytes.len() as u64).div_ceil(16) * 16 + 16;
        address
    }

    /// Maps `bytes` at `address`, which nothing else may use.
    pub fn map(&mut self, address: u64, bytes: &[u8]) {
        self.memory.insert(address, bytes.to_vec());
    }

    /// A variable in memory, returning its address.
    pub fn variable(&mut self, name: &str, ty: TypeReference, bytes: &[u8]) -> u64 {
        let address = self.allocate(bytes);
        self.objects.push(Object {
            name: name.to_owned(),
            ty,
            storage: Storage::Memory(address),
        });
        address
    }

    pub fn register_variable(
        &mut self,
        name: &str,
        ty: TypeReference,
        register: &'static str,
        bytes: &[u8],
    ) {
        self.objects.push(Object {
            name: name.to_owned(),
            ty,
            storage: Storage::Register {
                name: register,
                bytes: bytes.to_vec(),
            },
        });
    }

    pub fn optimized_out(&mut self, name: &str, ty: TypeReference) {
        self.objects.push(Object {
            name: name.to_owned(),
            ty,
            storage: Storage::OptimizedOut,
        });
    }

    pub fn set_register(&mut self, name: &'static str, value: u128) {
        self.registers.insert(name, Some(value));
    }

    /// A register unwinding could not recover.
    pub fn lost_register(&mut self, name: &'static str) {
        self.registers.insert(name, None);
    }

    /// Replaces a variable's bytes in memory.
    pub fn set(&mut self, name: &str, bytes: &[u8]) {
        let address = self.address_of(name);
        let region = self.memory.get_mut(&address).expect("a mapped variable");
        region[..bytes.len()].copy_from_slice(bytes);
    }

    /// Stores bytes at a place, as the debugger would.
    pub fn write(&mut self, at: &Place, bytes: &[u8]) {
        match at {
            Place::Memory { address, .. } => {
                let (start, region) = self
                    .memory
                    .range_mut(..=*address)
                    .next_back()
                    .expect("a mapped place");
                let offset = usize::try_from(address - start).expect("small offsets");
                region[offset..offset + bytes.len()].copy_from_slice(bytes);
            }
            Place::Register { name, .. } => {
                for object in &mut self.objects {
                    if let Storage::Register {
                        name: register,
                        bytes: stored,
                    } = &mut object.storage
                        && register == name
                    {
                        stored[..bytes.len()].copy_from_slice(bytes);
                    }
                }
            }
        }
    }

    /// Makes reading `[address, address + size)` fail the test.
    pub fn poison(&mut self, address: u64, size: u64) {
        self.poisoned.push((address, address + size));
    }

    /// The type of the variable `name`.
    pub fn type_of(&self, name: &str) -> TypeReference {
        self.objects
            .iter()
            .find(|object| object.name == name)
            .map_or_else(|| panic!("no variable `{name}`"), |object| object.ty)
    }

    pub fn address_of(&self, name: &str) -> u64 {
        match self
            .objects
            .iter()
            .find(|object| object.name == name)
            .map(|object| &object.storage)
        {
            Some(Storage::Memory(address)) => *address,
            _ => panic!("`{name}` is not in memory"),
        }
    }

    fn representation(&self, mut ty: TypeReference) -> &TypeInfo {
        loop {
            match &self.info(ty).kind {
                TypeKind::Named {
                    target: Some(target),
                    ..
                }
                | TypeKind::Modified { target, .. } => {
                    ty = *target;
                }
                _ => return self.info(ty),
            }
        }
    }

    fn size(&self, ty: TypeReference) -> usize {
        usize::try_from(self.info(ty).byte_size.unwrap_or_default()).expect("small sizes")
    }

    fn read_memory(&mut self, address: u64, size: usize) -> Result<Vec<u8>, Stop> {
        let end = address.saturating_add(size as u64);
        if let Some((start, stop)) = self
            .poisoned
            .iter()
            .find(|(start, stop)| address < *stop && *start < end)
        {
            panic!("read {address:#x}..{end:#x} overlaps poisoned memory {start:#x}..{stop:#x}");
        }
        self.reads.push((address, size));
        let region = self
            .memory
            .range(..=address)
            .next_back()
            .filter(|(start, bytes)| end <= **start + bytes.len() as u64);
        let Some((start, bytes)) = region else {
            return Err(Stop::missing(VariableState::Unavailable(
                VariableUnavailableReason::MemoryInaccessible {
                    address: VirtualAddress::new(address),
                    requested: size as u64,
                    completed: 0,
                    next_address: VirtualAddress::new(address),
                },
            )));
        };
        let offset = usize::try_from(address - start).expect("small offsets");
        Ok(bytes[offset..offset + size].to_vec())
    }

    fn bytes(&mut self, place: &Place) -> Result<Vec<u8>, Stop> {
        let size = self.size(place.ty());
        match place {
            Place::Memory { address, .. } => self.read_memory(*address, size),
            Place::Register { bytes, .. } => Ok(bytes[..size.min(bytes.len())].to_vec()),
        }
    }

    fn decode(&self, ty: TypeReference, bytes: &[u8]) -> VariableValue {
        let mut wide = [0_u8; 16];
        wide[..bytes.len().min(16)].copy_from_slice(&bytes[..bytes.len().min(16)]);
        let raw = u128::from_le_bytes(wide);
        let signed = |size: usize| {
            let shift = 128 - size * 8;
            (raw.cast_signed() << shift) >> shift
        };
        match &self.representation(ty).kind {
            TypeKind::Base(base) => VariableValue::Scalar(match base.encoding {
                BaseTypeEncoding::Boolean => ScalarValue::Boolean(raw != 0),
                BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter => {
                    ScalarValue::Signed(signed(bytes.len()))
                }
                BaseTypeEncoding::Unsigned | BaseTypeEncoding::UnsignedCharacter => {
                    ScalarValue::Unsigned(raw)
                }
                BaseTypeEncoding::Floating => ScalarValue::Floating(match bytes.len() {
                    4 => FloatValue::Binary32(u32::try_from(raw).expect("four bytes")),
                    8 => FloatValue::Binary64(u64::try_from(raw).expect("eight bytes")),
                    _ => FloatValue::X87Extended {
                        significand: u64::try_from(raw & u128::from(u64::MAX))
                            .expect("eight bytes"),
                        sign_exponent: u16::try_from((raw >> 64) & 0xffff).expect("two bytes"),
                    },
                }),
            }),
            TypeKind::Enumeration {
                representation,
                enumerators,
                ..
            } => {
                let value = if matches!(
                    representation.encoding,
                    BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter
                ) {
                    IntegerValue::Signed(signed(bytes.len()))
                } else {
                    IntegerValue::Unsigned(raw)
                };
                let matches: Vec<Enumerator> = enumerators
                    .iter()
                    .filter(|enumerator| enumerator.value == value)
                    .cloned()
                    .collect();
                VariableValue::Enumeration {
                    value,
                    matches: matches.into(),
                }
            }
            TypeKind::Pointer { .. } | TypeKind::Reference { .. } => {
                VariableValue::Address(AddressValue {
                    address: VirtualAddress::new(u64::try_from(raw).expect("eight bytes")),
                })
            }
            TypeKind::Array { dimensions, .. } => VariableValue::Array {
                dimensions: Arc::clone(dimensions),
            },
            _ => VariableValue::Record,
        }
    }

    fn available(
        &self,
        ty: TypeReference,
        source: VariableValueSource,
        bytes: Vec<u8>,
    ) -> VariableState {
        VariableState::Available {
            source,
            value: self.decode(ty, &bytes),
            raw: Some(bytes.into()),
            dereference: DereferenceState::NotApplicable,
            children: ValueChildren::NotApplicable,
            text: None,
            presentation: None,
        }
    }

    fn inspected(&self, ty: TypeReference, state: VariableState) -> InspectedValue {
        InspectedValue {
            type_info: Some(self.info(ty).clone()),
            state,
            completion: InspectionCompletion::Complete,
            usage: InspectionUsage::default(),
        }
    }

    fn is_char(&self, ty: TypeReference) -> bool {
        matches!(
            &self.representation(ty).kind,
            TypeKind::Base(BaseType {
                encoding: BaseTypeEncoding::SignedCharacter | BaseTypeEncoding::UnsignedCharacter,
                ..
            })
        )
    }

    /// The bytes up to a NUL from `address`, at most `limit`.
    fn c_string(&mut self, address: u64, limit: usize) -> TextSummary {
        let mut bytes = Vec::new();
        for offset in 0..limit {
            let at = address.wrapping_add(offset as u64);
            match self.read_memory(at, 1) {
                Ok(byte) if byte[0] == 0 => {
                    return TextSummary {
                        bytes: bytes.into(),
                        completion: TextCompletion::Complete,
                    };
                }
                Ok(byte) => bytes.push(byte[0]),
                Err(_) => {
                    return TextSummary {
                        bytes: bytes.into(),
                        completion: TextCompletion::Unreadable {
                            address: VirtualAddress::new(at),
                        },
                    };
                }
            }
        }
        TextSummary {
            bytes: bytes.into(),
            completion: TextCompletion::Truncated { length: None },
        }
    }
}

impl TypeSource for World {
    fn type_info(&self, ty: TypeReference) -> Option<TypeInfo> {
        (ty.image == IMAGE).then(|| self.info(ty).clone())
    }

    fn pointer_size(&self) -> u8 {
        8
    }

    fn byte_order(&self) -> ByteOrder {
        ByteOrder::Little
    }
}

fn type_error(message: impl Into<String>) -> Refusal {
    Refusal::new(ErrorKind::Type, message)
}

impl Scope for World {
    type Object = usize;
    type Step = Step;

    fn lookup(&self, name: &str, _outermost: bool) -> Result<Lookup<usize>, Refusal> {
        let objects: Vec<usize> = (0..self.objects.len())
            .filter(|&index| self.objects[index].name == name)
            .collect();
        match objects.as_slice() {
            [index] => {
                return Ok(Lookup::Object {
                    object: *index,
                    ty: Ok(self.objects[*index].ty),
                });
            }
            [_, ..] => {
                return Ok(Lookup::Ambiguous(
                    (0..objects.len())
                        .map(|copy| format!("unit{copy}.c::{name}"))
                        .collect(),
                ));
            }
            [] => {}
        }
        let mut found = Vec::new();
        for info in &self.types {
            if let TypeKind::Enumeration { enumerators, .. } = &info.kind {
                for enumerator in enumerators.iter() {
                    let qualified = format!("{}::{}", info.name, enumerator.name);
                    if enumerator.name.as_ref() == name || qualified == name {
                        let value = match enumerator.value {
                            IntegerValue::Signed(value) => Exact::from(value),
                            IntegerValue::Unsigned(value) => Exact::from(value),
                        };
                        found.push((qualified, value, info.reference));
                    }
                }
            }
        }
        Ok(match found.as_slice() {
            [] => Lookup::NotFound,
            [(_, value, ty)] => Lookup::Enumerator {
                value: *value,
                ty: *ty,
            },
            _ => Lookup::Ambiguous(found.into_iter().map(|(qualified, ..)| qualified).collect()),
        })
    }

    fn lookup_type(&self, query: &TypeQuery) -> TypeLookup {
        let found: Vec<TypeReference> = self
            .types
            .iter()
            .filter(|info| {
                !matches!(
                    info.kind,
                    TypeKind::Pointer { .. } | TypeKind::Array { .. } | TypeKind::Reference { .. }
                )
            })
            .filter(|info| {
                info.name.as_ref() == query.name
                    || c_type_key_of_name(&info.name).is_some_and(|key| key == query.name)
            })
            .map(|info| info.reference)
            .collect();
        match found.as_slice() {
            [] => TypeLookup::NotFound,
            [ty] => TypeLookup::Found(*ty),
            _ => TypeLookup::Ambiguous(
                found
                    .iter()
                    .map(|ty| self.info(*ty).name.to_string())
                    .collect(),
            ),
        }
    }

    fn plan(&self, from: TypeReference, step: StepKind<'_>) -> Result<Planned<Step>, Refusal> {
        let info = self.representation(from);
        let planned = |step, result: TypeReference, consumed| Planned {
            step,
            result: Some(result),
            consumed,
        };
        match (step, &info.kind) {
            (
                StepKind::Deref,
                TypeKind::Pointer {
                    target: Some(target),
                    ..
                }
                | TypeKind::Reference { target, .. },
            ) => Ok(planned(Step::Deref(*target), *target, 0)),
            (StepKind::Member(name), TypeKind::Record { members, .. }) => {
                let member = members
                    .iter()
                    .find(|member| member.name.as_deref() == Some(name))
                    .ok_or_else(|| type_error(format!("`{}` has no member `{name}`", info.name)))?;
                let RecordMemberLayout::ByteOffset(offset) = member.layout else {
                    panic!("the world lays members out at byte offsets");
                };
                Ok(planned(
                    Step::Member {
                        offset,
                        ty: member.type_ref,
                    },
                    member.type_ref,
                    0,
                ))
            }
            (
                StepKind::Index { available },
                TypeKind::Array {
                    element,
                    dimensions,
                },
            ) => {
                if available < dimensions.len() {
                    return Err(type_error(format!(
                        "`{}` takes {} indices",
                        info.name,
                        dimensions.len()
                    )));
                }
                Ok(planned(
                    Step::Array {
                        dimensions: Arc::clone(dimensions),
                        element_size: self.info(*element).byte_size.unwrap_or_default(),
                        ty: *element,
                    },
                    *element,
                    dimensions.len(),
                ))
            }
            (StepKind::Index { .. }, TypeKind::Slice { element, .. }) => Ok(planned(
                Step::Slice {
                    element_size: self.info(*element).byte_size.unwrap_or_default(),
                    ty: *element,
                },
                *element,
                1,
            )),
            (step, _) => Err(type_error(format!(
                "`{}` does not take {step:?}",
                info.name
            ))),
        }
    }

    fn register(&self, name: &str) -> Option<Register> {
        let name = match name {
            "pc" => "rip",
            "sp" => "rsp",
            "fp" => "rbp",
            other => other,
        };
        let number = self
            .registers
            .keys()
            .position(|register| *register == name)?;
        Some(Register {
            number: u16::try_from(number).ok()?,
            width: 64,
        })
    }
}

impl Machine for World {
    type Object = usize;
    type Step = Step;
    type Place = Place;

    fn charge(&mut self) -> Result<(), Stop> {
        match &mut self.work {
            Some(0) => Err(Stop::missing(VariableState::Unavailable(
                VariableUnavailableReason::EvaluationLimit,
            ))),
            Some(work) => {
                *work -= 1;
                Ok(())
            }
            None => Ok(()),
        }
    }

    fn locate(&mut self, object: &usize) -> Result<Place, Stop> {
        let object = &self.objects[*object];
        match &object.storage {
            Storage::Memory(address) => Ok(Place::Memory {
                address: *address,
                ty: object.ty,
            }),
            Storage::Register { name, bytes } => Ok(Place::Register {
                name,
                bytes: bytes.clone(),
                ty: object.ty,
            }),
            Storage::OptimizedOut => Err(Stop::missing(VariableState::Unavailable(
                VariableUnavailableReason::OptimizedOut(OptimizedOutReason::NoLocation),
            ))),
        }
    }

    fn check_indices(&self, step: &Step, indices: &[i128]) -> Result<(), Stop> {
        if let Step::Array { dimensions, .. } = step {
            for (index, dimension) in indices.iter().zip(dimensions.iter()) {
                if *index < dimension.lower_bound
                    || *index >= dimension.lower_bound + i128::from(dimension.count)
                {
                    return Err(Stop::Refused(Refusal::new(
                        ErrorKind::Bounds,
                        format!("the index {index} is outside 0..{}", dimension.count),
                    )));
                }
            }
        }
        Ok(())
    }

    fn step(&mut self, from: &Place, step: &Step, indices: &[i128]) -> Result<Place, Stop> {
        let offset_place = |place: &Place, offset: u64, ty| match place {
            Place::Memory { address, .. } => Place::Memory {
                address: address.wrapping_add(offset),
                ty,
            },
            Place::Register { name, bytes, .. } => Place::Register {
                name,
                bytes: bytes[usize::try_from(offset).expect("small")..].to_vec(),
                ty,
            },
        };
        match step {
            Step::Deref(target) => {
                let bytes = self.bytes(from)?;
                let address = match self.decode(from.ty(), &bytes) {
                    VariableValue::Address(address) => address.address.get(),
                    _ => panic!("the world dereferences pointers only"),
                };
                if address == 0 {
                    return Err(Stop::missing(VariableState::Unavailable(
                        VariableUnavailableReason::ValueAccess(
                            ValueAccessUnavailableReason::NullPointer,
                        ),
                    )));
                }
                Ok(Place::Memory {
                    address,
                    ty: *target,
                })
            }
            Step::Member { offset, ty } => Ok(offset_place(from, *offset, *ty)),
            Step::Array {
                dimensions,
                element_size,
                ty,
            } => {
                let mut linear = 0_u64;
                for (index, dimension) in indices.iter().zip(dimensions.iter()) {
                    linear = linear * dimension.count
                        + u64::try_from(index - dimension.lower_bound).expect("checked");
                }
                Ok(offset_place(from, linear * element_size, *ty))
            }
            Step::Slice { element_size, ty } => {
                let descriptor = self.bytes(from)?;
                let data = u64::from_le_bytes(descriptor[..8].try_into().expect("eight bytes"));
                let length = u64::from_le_bytes(descriptor[8..16].try_into().expect("eight bytes"));
                let index = indices[0];
                if index < 0 || index >= i128::from(length) {
                    return Err(Stop::missing(VariableState::Unavailable(
                        VariableUnavailableReason::IndexOutOfBounds {
                            index,
                            lower_bound: 0,
                            count: length,
                        },
                    )));
                }
                Ok(Place::Memory {
                    address: data.wrapping_add(
                        u64::try_from(index)
                            .expect("checked")
                            .wrapping_mul(*element_size),
                    ),
                    ty: *ty,
                })
            }
        }
    }

    fn place_at(&mut self, address: u64, ty: TypeReference) -> Result<Place, Stop> {
        Ok(Place::Memory { address, ty })
    }

    fn address(&self, at: &Place) -> Result<u64, Stop> {
        match at {
            Place::Memory { address, .. } => Ok(*address),
            Place::Register { name, .. } => Err(Stop::Refused(Refusal::new(
                ErrorKind::NotAnLvalue,
                format!("the value is in register {name}, which has no address"),
            ))),
        }
    }

    fn load(&mut self, at: &Place) -> Result<VariableValue, Stop> {
        let bytes = self.bytes(at)?;
        Ok(self.decode(at.ty(), &bytes))
    }

    fn read(&mut self, address: u64, size: usize) -> Result<Vec<u8>, Stop> {
        self.read_memory(address, size)
    }

    fn text(&mut self, at: &Place) -> Result<Option<TextSummary>, Stop> {
        match self.representation(at.ty()).kind.clone() {
            TypeKind::Pointer {
                target: Some(target),
                ..
            } if self.is_char(target) => {
                let VariableValue::Address(address) = self.load(at)? else {
                    unreachable!("pointers decode to addresses")
                };
                Ok(Some(
                    self.c_string(address.address.get(), TextSummary::MAX_BYTES),
                ))
            }
            TypeKind::Array { element, .. } if self.is_char(element) => {
                let bytes = self.bytes(at)?;
                let end = bytes
                    .iter()
                    .position(|&byte| byte == 0)
                    .unwrap_or(bytes.len());
                Ok(Some(TextSummary {
                    bytes: bytes[..end].to_vec().into(),
                    completion: TextCompletion::Complete,
                }))
            }
            _ => Ok(None),
        }
    }

    fn length(&mut self, at: &Place) -> Result<u64, Stop> {
        let descriptor = self.bytes(at)?;
        Ok(u64::from_le_bytes(
            descriptor[8..16].try_into().expect("eight bytes"),
        ))
    }

    fn register(&mut self, register: &Register) -> Result<u128, Stop> {
        let (name, value) = self
            .registers
            .iter()
            .nth(usize::from(register.number))
            .expect("a register the scope named");
        value.ok_or_else(|| {
            Stop::missing(VariableState::Unavailable(
                VariableUnavailableReason::RegisterNotSaved((*name).into()),
            ))
        })
    }

    fn present(&mut self, at: &Place) -> Result<InspectedValue, Stop> {
        let source = match at {
            Place::Memory { address, .. } => {
                VariableValueSource::Memory(VirtualAddress::new(*address))
            }
            Place::Register { .. } => VariableValueSource::Computed,
        };
        let bytes = match &self.representation(at.ty()).kind {
            TypeKind::Record { .. } => Vec::new(),
            _ => self.bytes(at)?,
        };
        let state = self.available(at.ty(), source, bytes);
        Ok(self.inspected(at.ty(), state))
    }

    fn present_bytes(&mut self, ty: TypeReference, bytes: &[u8]) -> Result<InspectedValue, Stop> {
        let state = self.available(ty, VariableValueSource::Computed, bytes.to_vec());
        Ok(self.inspected(ty, state))
    }

    fn present_pointer(
        &mut self,
        address: u64,
        _pointee: Option<TypeReference>,
        type_info: TypeInfo,
    ) -> Result<InspectedValue, Stop> {
        Ok(self.finish(
            Some(type_info),
            VariableState::Available {
                source: VariableValueSource::Computed,
                raw: Some(address.to_le_bytes().to_vec().into()),
                value: VariableValue::Address(AddressValue {
                    address: VirtualAddress::new(address),
                }),
                dereference: DereferenceState::NotApplicable,
                children: ValueChildren::NotApplicable,
                text: None,
                presentation: None,
            },
        ))
    }

    fn finish(&self, type_info: Option<TypeInfo>, state: VariableState) -> InspectedValue {
        InspectedValue {
            type_info,
            state,
            completion: InspectionCompletion::Complete,
            usage: InspectionUsage::default(),
        }
    }
}

/// A world by the name an example block gives.
pub fn world(name: &str) -> World {
    match name {
        "scalars" => scalars(),
        "memory" => memory(),
        other => panic!("no world named {other}"),
    }
}

/// Scalars of every C width, named as GCC names them.
pub fn scalars() -> World {
    use BaseTypeEncoding as E;
    let mut world = World::new();
    let uchar = world.base("unsigned char", E::UnsignedCharacter, 1);
    let schar = world.base("signed char", E::SignedCharacter, 1);
    world.base("short int", E::Signed, 2);
    let int = world.base("int", E::Signed, 4);
    let uint = world.base("unsigned int", E::Unsigned, 4);
    let long = world.base("long int", E::Signed, 8);
    world.base("long unsigned int", E::Unsigned, 8);
    let ushort = world.base("short unsigned int", E::Unsigned, 2);
    let ulonglong = world.base("long long unsigned int", E::Unsigned, 8);
    let float = world.base("float", E::Floating, 4);
    let double = world.base("double", E::Floating, 8);
    let long_double = world.base("long double", E::Floating, 16);
    let boolean = world.base("_Bool", E::Boolean, 1);
    world.variable("uc", uchar, &[250]);
    world.variable("sc", schar, &(-7_i8).to_le_bytes());
    world.variable("u32v", uint, &4_000_000_000_u32.to_le_bytes());
    world.variable("i32v", int, &(-123_456_i32).to_le_bytes());
    world.variable("i64v", long, &(-100_i64).to_le_bytes());
    world.variable("u16v", ushort, &u16::MAX.to_le_bytes());
    world.variable("u64v", ulonglong, &u64::MAX.to_le_bytes());
    world.variable("f", float, &1.5_f32.to_le_bytes());
    world.variable("d", double, &10.0_f64.to_le_bytes());
    // 1.25 in x87 extended precision, padded to sixteen bytes.
    let mut extended = 0xa000_0000_0000_0000_u64.to_le_bytes().to_vec();
    extended.extend_from_slice(&0x3fff_u16.to_le_bytes());
    extended.resize(16, 0);
    world.variable("ld", long_double, &extended);
    world.variable("flag", boolean, &[1]);
    world
}

/// Records, one of them zero-sized, pointers, arrays, a slice, text,
/// enumerations, a typedef, a constant, a reference, values in a register or
/// optimized out, and names that need qualifying.
pub fn memory() -> World {
    use BaseTypeEncoding as E;
    let mut world = World::new();
    let int = world.base("int", E::Signed, 4);
    let long = world.base("long int", E::Signed, 8);
    let char = world.base("char", E::SignedCharacter, 1);
    let uint = world.base("unsigned int", E::Unsigned, 4);
    let record = world.record("S", 16, &[("a", int, 0), ("b", long, 8)]);
    let record_pointer = world.pointer(Some(record));
    let int_array = world.array(int, &[4]);
    let int_pointer = world.pointer(Some(int));
    let matrix = world.array(int, &[2, 3]);
    let char_pointer = world.pointer(Some(char));
    let char_array = world.array(char, &[8]);
    let color = world.enumeration("Color", uint, &[("RED", 0), ("GREEN", 1), ("BLUE", 2)]);
    let sign = world.enumeration("Sign", int, &[("NEGATIVE", -1), ("POSITIVE", 1)]);
    let uchar = world.base("unsigned char", E::UnsignedCharacter, 1);
    world.enumeration("Small", uchar, &[("ONE", 1), ("TWO", 2), ("HIGH", 0x80)]);
    let void_pointer = world.pointer(None);
    let slice = world.slice(int);
    let empty = world.record("Empty", 0, &[]);
    let empty_pointer = world.pointer(Some(empty));

    let mut s = 5_i32.to_le_bytes().to_vec();
    s.extend_from_slice(&[0; 4]);
    s.extend_from_slice(&7_i64.to_le_bytes());
    let s_address = world.variable("s", record, &s);
    world.variable("ptr", record_pointer, &s_address.to_le_bytes());
    world.variable("null_ptr", record_pointer, &0_u64.to_le_bytes());
    let arr: Vec<u8> = [11_i32, 22, 33, 44]
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    let arr_address = world.variable("arr", int_array, &arr);
    world.variable("ip", int_pointer, &(arr_address + 4).to_le_bytes());
    let m: Vec<u8> = (1..=6_i32).flat_map(i32::to_le_bytes).collect();
    world.variable("m", matrix, &m);
    let hello = world.allocate(b"hello\0");
    world.variable("name", char_pointer, &hello.to_le_bytes());
    world.variable("buf", char_array, b"abc\0\0\0\0\0");
    world.variable("color", color, &2_u32.to_le_bytes());
    world.variable("sign", sign, &(-1_i32).to_le_bytes());
    world.variable("vp", void_pointer, &s_address.to_le_bytes());
    world.variable("ep", empty_pointer, &s_address.to_le_bytes());
    let items: Vec<u8> = [10_i32, 20, 30]
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    let items_address = world.allocate(&items);
    let mut descriptor = items_address.to_le_bytes().to_vec();
    descriptor.extend_from_slice(&3_u64.to_le_bytes());
    world.variable("items", slice, &descriptor);
    let counter = world.typedef("counter_t", long);
    world.variable("count", counter, &12_i64.to_le_bytes());
    let constant = world.constant(int);
    world.variable("limit", constant, &100_i32.to_le_bytes());
    let int_reference = world.reference(int);
    world.variable("first", int_reference, &arr_address.to_le_bytes());
    world.register_variable("r", int, "rbx", &9_i32.to_le_bytes());
    world.optimized_out("gone", int);
    world.set_register("rip", 0x40_1000);
    world.set_register("rsp", 0x7ffe_0000);
    world.lost_register("rbp");

    // Text that runs into unmapped memory before it ends.
    let cut = world.allocate(b"hel");
    world.variable("partial", char_pointer, &cut.to_le_bytes());
    // Two variables of one name, and an enumerator two enumerations share.
    world.variable("twice", int, &1_i32.to_le_bytes());
    world.variable("twice", int, &2_i32.to_le_bytes());
    let light = world.enumeration("Light", uint, &[("RED", 10), ("AMBER", 11)]);
    world.variable("light", light, &11_u32.to_le_bytes());
    world
}

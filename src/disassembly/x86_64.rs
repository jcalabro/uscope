//! The x86-64 instruction decoder and renderer, built on iced-x86.

use iced_x86::{
    DecoderError, DecoderOptions, FlowControl, Formatter, FormatterOutput, FormatterTextKind,
    GasFormatter, Instruction, IntelFormatter, OpKind, Register,
};

use super::{
    AssemblySyntax, ControlFlow, InstructionReferenceKind, InstructionToken, InstructionTokenKind,
    RawDecode,
};

/// The longest x86-64 instruction encoding the architecture permits.
const MAX_INSTRUCTION_LENGTH: usize = 15;

pub struct Decoder {
    formatter: Box<dyn Formatter>,
}

impl Decoder {
    pub fn new(syntax: AssemblySyntax) -> Self {
        let mut formatter: Box<dyn Formatter> = match syntax {
            AssemblySyntax::Intel => Box::new(IntelFormatter::new()),
            AssemblySyntax::Att => Box::new(GasFormatter::new()),
        };
        let options = formatter.options_mut();
        options.set_hex_prefix("0x");
        options.set_hex_suffix("");
        options.set_uppercase_hex(false);
        options.set_space_after_operand_separator(true);
        // Program-counter-relative operands keep their encoded form; the
        // absolute address they name is reported as a reference instead.
        options.set_rip_relative_addresses(true);
        options.set_branch_leading_zeros(false);
        options.set_show_branch_size(false);
        Self { formatter }
    }
}

impl super::InstructionDecoder for Decoder {
    fn max_instruction_length(&self) -> usize {
        MAX_INSTRUCTION_LENGTH
    }

    fn decode(&mut self, address: u64, bytes: &[u8]) -> RawDecode {
        let mut decoder = iced_x86::Decoder::with_ip(64, bytes, address, DecoderOptions::NONE);
        let instruction = decoder.decode();
        if instruction.is_invalid() {
            return match decoder.last_error() {
                DecoderError::NoMoreBytes => RawDecode::Incomplete,
                _ => RawDecode::Invalid,
            };
        }
        let mut tokens = Tokens(Vec::new());
        self.formatter.format(&instruction, &mut tokens);
        RawDecode::Instruction {
            length: instruction.len(),
            tokens: tokens.0,
            flow: flow(&instruction),
            references: references(&instruction),
        }
    }
}

struct Tokens(Vec<InstructionToken>);

impl FormatterOutput for Tokens {
    fn write(&mut self, text: &str, kind: FormatterTextKind) {
        let kind = match kind {
            FormatterTextKind::Mnemonic => InstructionTokenKind::Mnemonic,
            FormatterTextKind::Prefix => InstructionTokenKind::Prefix,
            FormatterTextKind::Keyword
            | FormatterTextKind::Directive
            | FormatterTextKind::Decorator => InstructionTokenKind::Keyword,
            FormatterTextKind::Register => InstructionTokenKind::Register,
            FormatterTextKind::Number | FormatterTextKind::SelectorValue => {
                InstructionTokenKind::Number
            }
            FormatterTextKind::LabelAddress | FormatterTextKind::FunctionAddress => {
                InstructionTokenKind::Address
            }
            FormatterTextKind::Punctuation | FormatterTextKind::Operator => {
                InstructionTokenKind::Punctuation
            }
            _ => InstructionTokenKind::Text,
        };
        self.0.push(InstructionToken {
            kind,
            text: text.into(),
        });
    }
}

fn flow(instruction: &Instruction) -> ControlFlow {
    match instruction.flow_control() {
        FlowControl::Next => ControlFlow::Sequential,
        FlowControl::UnconditionalBranch => ControlFlow::Jump,
        FlowControl::ConditionalBranch => ControlFlow::ConditionalJump,
        FlowControl::IndirectBranch => ControlFlow::IndirectJump,
        FlowControl::Call => ControlFlow::Call,
        FlowControl::IndirectCall => ControlFlow::IndirectCall,
        FlowControl::Return => ControlFlow::Return,
        FlowControl::Interrupt => ControlFlow::Interrupt,
        FlowControl::XbeginXabortXend => ControlFlow::Transaction,
        FlowControl::Exception => ControlFlow::Exception,
    }
}

/// Returns the addresses an instruction alone determines: direct branch
/// targets and memory operands addressed relative to the instruction or by
/// an absolute displacement. An FS- or GS-relative operand addresses
/// thread-local storage rather than the flat address space, so it names no
/// address.
fn references(instruction: &Instruction) -> Vec<(InstructionReferenceKind, u64)> {
    let mut references = Vec::new();
    for operand in 0..instruction.op_count() {
        match instruction.op_kind(operand) {
            OpKind::NearBranch16 | OpKind::NearBranch32 | OpKind::NearBranch64 => {
                references.push((
                    InstructionReferenceKind::BranchTarget,
                    instruction.near_branch_target(),
                ));
            }
            OpKind::Memory => {
                if matches!(instruction.segment_prefix(), Register::FS | Register::GS) {
                    continue;
                }
                if instruction.is_ip_rel_memory_operand() {
                    references.push((
                        InstructionReferenceKind::MemoryOperand,
                        instruction.ip_rel_memory_address(),
                    ));
                } else if instruction.memory_base() == Register::None
                    && instruction.memory_index() == Register::None
                {
                    references.push((
                        InstructionReferenceKind::MemoryOperand,
                        instruction.memory_displacement64(),
                    ));
                }
            }
            _ => {}
        }
    }
    references
}

//! Encoding a new value for a scalar, enumeration, or pointer.

use crate::condition::Operand;
use crate::{ByteOrder, Enumerator, FloatValue, IntegerValue, ScalarValue, VariableValue};

/// Encodes `operand` as the bytes of a value shaped like `current`, which
/// occupies `size` bytes. A value that does not fit is refused rather than
/// truncated.
pub fn encode(
    operand: Operand,
    current: &VariableValue,
    size: usize,
    byte_order: ByteOrder,
) -> Result<Vec<u8>, String> {
    let integer = |signed: bool| -> Result<Vec<u8>, String> {
        let Operand::Integer(value) = operand else {
            return Err(match operand {
                Operand::Float(_) => "a floating-point value cannot be stored in an integer; \
                                      write an integer"
                    .to_owned(),
                _ => "a boolean cannot be stored in an integer; write 0 or 1".to_owned(),
            });
        };
        let bits = size * 8;
        let fits = if bits >= 128 {
            signed || value >= 0
        } else if signed {
            let limit = 1_i128 << (bits - 1);
            (-limit..limit).contains(&value)
        } else {
            (0..1_i128 << bits).contains(&value)
        };
        if !fits {
            return Err(format!(
                "{value} does not fit a {} {bits}-bit value",
                if signed { "signed" } else { "unsigned" }
            ));
        }
        Ok(ordered(&value.to_le_bytes()[..size], byte_order))
    };
    match current {
        VariableValue::Scalar(ScalarValue::Boolean(_)) => {
            let truth = match operand {
                Operand::Boolean(value) => value,
                Operand::Integer(value @ (0 | 1)) => value == 1,
                _ => return Err("a boolean is true, false, 0, or 1".to_owned()),
            };
            let mut bytes = vec![0; size];
            bytes[0] = u8::from(truth);
            Ok(ordered(&bytes, byte_order))
        }
        VariableValue::Scalar(ScalarValue::Signed(_))
        | VariableValue::Enumeration {
            value: IntegerValue::Signed(_),
            ..
        } => integer(true),
        VariableValue::Scalar(ScalarValue::Unsigned(_))
        | VariableValue::Enumeration {
            value: IntegerValue::Unsigned(_),
            ..
        }
        | VariableValue::Address(_) => integer(false),
        VariableValue::Scalar(ScalarValue::Floating(format)) => {
            let value = match operand {
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "an integer stored in a float takes its nearest value, as in C"
                )]
                Operand::Integer(value) => value as f64,
                Operand::Float(value) => value,
                Operand::Boolean(_) => {
                    return Err("a boolean cannot be stored in a floating-point value".to_owned());
                }
            };
            Ok(match format {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "a float takes the double's nearest value, as in C"
                )]
                FloatValue::Binary32(_) => ordered(&(value as f32).to_le_bytes(), byte_order),
                FloatValue::Binary64(_) => ordered(&value.to_le_bytes(), byte_order),
                FloatValue::X87Extended { .. } => {
                    let mut bytes = x87_bytes(value).to_vec();
                    bytes.resize(size, 0);
                    bytes
                }
            })
        }
        _ => Err("only numbers, booleans, enumerations, and pointers can be assigned".to_owned()),
    }
}

/// Finds an enumerator by name.
pub fn enumerator(enumerators: &[Enumerator], name: &str) -> Option<Operand> {
    enumerators
        .iter()
        .find(|enumerator| &*enumerator.name == name)
        .map(|enumerator| {
            Operand::Integer(match enumerator.value {
                IntegerValue::Signed(value) => value,
                IntegerValue::Unsigned(value) => i128::try_from(value).unwrap_or(i128::MAX),
            })
        })
}

/// Arranges little-endian bytes in the target's order.
fn ordered(little: &[u8], byte_order: ByteOrder) -> Vec<u8> {
    let mut bytes = little.to_vec();
    if byte_order == ByteOrder::Big {
        bytes.reverse();
    }
    bytes
}

/// Converts a double to the ten bytes of an x87 extended value.
fn x87_bytes(value: f64) -> [u8; 10] {
    let bits = value.to_bits();
    let sign = u16::from(bits >> 63 == 1) << 15;
    let exponent = i32::try_from((bits >> 52) & 0x7ff).expect("eleven bits");
    let fraction = bits & ((1 << 52) - 1);
    let (exponent, significand) = match exponent {
        0 if fraction == 0 => (0, 0),
        0 => {
            // A subnormal double is a normal extended value.
            let shift = fraction.leading_zeros();
            let exponent = 16383 - 1022 - i32::try_from(shift).expect("small") + 11;
            (exponent, fraction << shift)
        }
        0x7ff => (0x7fff, (1 << 63) | (fraction << 11)),
        _ => (exponent - 1023 + 16383, (1 << 63) | (fraction << 11)),
    };
    let mut bytes = [0; 10];
    bytes[..8].copy_from_slice(&significand.to_le_bytes());
    let top = sign | u16::try_from(exponent).expect("fifteen bits");
    bytes[8..].copy_from_slice(&top.to_le_bytes());
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scalar(value: ScalarValue) -> VariableValue {
        VariableValue::Scalar(value)
    }

    #[test]
    fn integers_are_encoded_in_their_width_and_refused_when_they_do_not_fit() {
        let signed = scalar(ScalarValue::Signed(0));
        let unsigned = scalar(ScalarValue::Unsigned(0));
        let little = ByteOrder::Little;
        assert_eq!(
            encode(Operand::Integer(-2), &signed, 4, little),
            Ok(vec![0xfe, 0xff, 0xff, 0xff])
        );
        assert_eq!(
            encode(Operand::Integer(0x1234), &unsigned, 2, ByteOrder::Big),
            Ok(vec![0x12, 0x34])
        );
        assert_eq!(
            encode(Operand::Integer(255), &unsigned, 1, little),
            Ok(vec![255])
        );
        assert_eq!(
            encode(Operand::Integer(256), &unsigned, 1, little),
            Err("256 does not fit a unsigned 8-bit value".to_owned())
        );
        assert_eq!(
            encode(Operand::Integer(-1), &unsigned, 4, little),
            Err("-1 does not fit a unsigned 32-bit value".to_owned())
        );
        assert_eq!(
            encode(Operand::Integer(128), &signed, 1, little),
            Err("128 does not fit a signed 8-bit value".to_owned())
        );
        assert!(encode(Operand::Float(1.5), &signed, 4, little).is_err());
    }

    #[test]
    fn booleans_floats_and_extended_floats_are_encoded() {
        let little = ByteOrder::Little;
        let boolean = scalar(ScalarValue::Boolean(false));
        assert_eq!(
            encode(Operand::Boolean(true), &boolean, 1, little),
            Ok(vec![1])
        );
        assert_eq!(
            encode(Operand::Integer(0), &boolean, 1, little),
            Ok(vec![0])
        );
        assert!(encode(Operand::Integer(2), &boolean, 1, little).is_err());
        let single = scalar(ScalarValue::Floating(FloatValue::Binary32(0)));
        assert_eq!(
            encode(Operand::Float(1.25), &single, 4, little),
            Ok(1.25_f32.to_le_bytes().to_vec())
        );
        let double = scalar(ScalarValue::Floating(FloatValue::Binary64(0)));
        assert_eq!(
            encode(Operand::Integer(3), &double, 8, little),
            Ok(3.0_f64.to_le_bytes().to_vec())
        );
        // 3.125 is 1.5625 * 2^1: exponent 16384, significand 0xc8 << 56.
        let extended = scalar(ScalarValue::Floating(FloatValue::X87Extended {
            significand: 0,
            sign_exponent: 0,
        }));
        let mut expected = (0xc800_0000_0000_0000_u64).to_le_bytes().to_vec();
        expected.extend_from_slice(&0x4000_u16.to_le_bytes());
        expected.resize(16, 0);
        assert_eq!(
            encode(Operand::Float(3.125), &extended, 16, little),
            Ok(expected)
        );
    }
}

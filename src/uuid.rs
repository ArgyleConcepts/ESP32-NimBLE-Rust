//! Bluetooth UUIDs in 16-, 32-, and 128-bit widths.
//!
//! # Byte order
//!
//! Two representations are used, and every conversion names the one it uses:
//!
//! - **Canonical** order is the order of the text form: the most significant
//!   byte first. [`Uuid::to_canonical_bytes`] of
//!   `6e400001-b5a3-f393-e0a9-e50e24dcca9e` starts `6e 40 00 01`.
//! - **Wire** order is the Bluetooth protocol order used by ATT PDUs,
//!   advertising data, and NimBLE's `ble_uuid*_t` values: the least significant
//!   byte first. [`Uuid::to_wire_bytes`] of the same UUID starts `9e ca dc 24`.
//!
//! A UUID keeps the width it was created with. A 16-bit UUID and its 128-bit
//! expansion over the Bluetooth Base UUID are different values; compare
//! [`Uuid::to_u128`] results to treat them as the same attribute type.

use crate::codec::{Decode, DecodeError, Encode, EncodeError, ValueReader, ValueWriter};
use std::fmt;
use std::str::FromStr;

/// The Bluetooth Base UUID, `00000000-0000-1000-8000-00805f9b34fb`, which
/// 16- and 32-bit UUIDs abbreviate.
pub const BLUETOOTH_BASE_UUID: u128 = 0x0000_0000_0000_1000_8000_0080_5f9b_34fb;

/// A Bluetooth UUID of a fixed width.
///
/// Each variant holds the numeric value; the 128-bit value is the canonical
/// text form read as one big-endian number, so
/// `Uuid::Uuid128(0x6e400001_b5a3_f393_e0a9_e50e24dcca9e)` displays as
/// `6e400001-b5a3-f393-e0a9-e50e24dcca9e`.
///
/// ```
/// use argyle_nimble::Uuid;
///
/// const BATTERY_SERVICE: Uuid = Uuid::Uuid16(0x180f);
/// const CUSTOM: Uuid = match Uuid::parse("6e400001-b5a3-f393-e0a9-e50e24dcca9e") {
///     Ok(uuid) => uuid,
///     Err(_) => panic!("invalid UUID literal"),
/// };
///
/// assert_eq!(BATTERY_SERVICE.to_string(), "180f");
/// assert_eq!(CUSTOM.to_wire_bytes().as_ref()[..2], [0x9e, 0xca]);
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Uuid {
    /// A 16-bit UUID abbreviating a value over the Bluetooth Base UUID.
    Uuid16(u16),
    /// A 32-bit UUID abbreviating a value over the Bluetooth Base UUID.
    Uuid32(u32),
    /// A full 128-bit UUID.
    Uuid128(u128),
}

/// Why a UUID could not be parsed or decoded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum UuidError {
    /// The input length matches no UUID width. Text must be 4, 8, or 36
    /// characters; bytes must be 2, 4, or 16.
    InvalidLength {
        /// The rejected length in characters or bytes.
        length: usize,
    },
    /// A text character is not a hexadecimal digit where one is required.
    InvalidCharacter {
        /// Byte offset of the character in the input.
        index: usize,
    },
    /// A 128-bit text form lacks a hyphen at one of the canonical positions
    /// (8, 13, 18, and 23).
    MissingHyphen {
        /// Byte offset where the hyphen was expected.
        index: usize,
    },
}

impl fmt::Display for UuidError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLength { length } => write!(
                formatter,
                "UUID length {length} matches no width (expected 4, 8, or 36 characters or 2, 4, or 16 bytes)"
            ),
            Self::InvalidCharacter { index } => {
                write!(formatter, "UUID character at index {index} is not hexadecimal")
            }
            Self::MissingHyphen { index } => {
                write!(formatter, "UUID is missing a hyphen at index {index}")
            }
        }
    }
}

impl std::error::Error for UuidError {}

/// The bytes of a UUID in one byte order: 2, 4, or 16 bytes depending on the
/// width. Use [`AsRef<[u8]>`](AsRef) to read them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UuidBytes {
    bytes: [u8; 16],
    length: u8,
}

impl AsRef<[u8]> for UuidBytes {
    fn as_ref(&self) -> &[u8] {
        &self.bytes[..usize::from(self.length)]
    }
}

impl UuidBytes {
    fn new(source: &[u8]) -> Self {
        let mut bytes = [0; 16];
        bytes[..source.len()].copy_from_slice(source);
        Self {
            bytes,
            length: source.len() as u8,
        }
    }
}

const HYPHENS: [usize; 4] = [8, 13, 18, 23];

const fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

impl Uuid {
    /// Parse the text form, accepting either case: 4 hexadecimal digits for a
    /// 16-bit UUID, 8 for a 32-bit UUID, or the 36-character hyphenated form
    /// for a 128-bit UUID. No prefixes, braces, or surrounding whitespace are
    /// accepted, so the width is never ambiguous.
    ///
    /// This is a `const fn`, so constants can be checked at compile time.
    pub const fn parse(text: &str) -> Result<Self, UuidError> {
        let bytes = text.as_bytes();
        let length = bytes.len();
        if length != 4 && length != 8 && length != 36 {
            return Err(UuidError::InvalidLength { length });
        }
        let mut value: u128 = 0;
        let mut index = 0;
        while index < length {
            let byte = bytes[index];
            if length == 36
                && (index == HYPHENS[0]
                    || index == HYPHENS[1]
                    || index == HYPHENS[2]
                    || index == HYPHENS[3])
            {
                if byte != b'-' {
                    return Err(UuidError::MissingHyphen { index });
                }
            } else {
                match hex_digit(byte) {
                    Some(digit) => value = (value << 4) | digit as u128,
                    None => return Err(UuidError::InvalidCharacter { index }),
                }
            }
            index += 1;
        }
        Ok(match length {
            4 => Self::Uuid16(value as u16),
            8 => Self::Uuid32(value as u32),
            _ => Self::Uuid128(value),
        })
    }

    /// The UUID width in bytes: 2, 4, or 16.
    pub const fn len(&self) -> usize {
        match self {
            Self::Uuid16(_) => 2,
            Self::Uuid32(_) => 4,
            Self::Uuid128(_) => 16,
        }
    }

    /// Always `false`; present for the `len` convention.
    pub const fn is_empty(&self) -> bool {
        false
    }

    /// The full 128-bit value. 16- and 32-bit UUIDs expand over
    /// [`BLUETOOTH_BASE_UUID`]; a 128-bit UUID is returned unchanged.
    pub const fn to_u128(&self) -> u128 {
        match *self {
            Self::Uuid16(value) => BLUETOOTH_BASE_UUID | ((value as u128) << 96),
            Self::Uuid32(value) => BLUETOOTH_BASE_UUID | ((value as u128) << 96),
            Self::Uuid128(value) => value,
        }
    }

    /// The bytes in canonical (text, most significant first) order.
    pub fn to_canonical_bytes(&self) -> UuidBytes {
        match *self {
            Self::Uuid16(value) => UuidBytes::new(&value.to_be_bytes()),
            Self::Uuid32(value) => UuidBytes::new(&value.to_be_bytes()),
            Self::Uuid128(value) => UuidBytes::new(&value.to_be_bytes()),
        }
    }

    /// The bytes in Bluetooth wire (least significant first) order, as used
    /// by ATT and NimBLE.
    pub fn to_wire_bytes(&self) -> UuidBytes {
        match *self {
            Self::Uuid16(value) => UuidBytes::new(&value.to_le_bytes()),
            Self::Uuid32(value) => UuidBytes::new(&value.to_le_bytes()),
            Self::Uuid128(value) => UuidBytes::new(&value.to_le_bytes()),
        }
    }

    /// Build a UUID from canonical-order bytes; the width follows the length
    /// (2, 4, or 16).
    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, UuidError> {
        Self::from_bytes(bytes, false)
    }

    /// Build a UUID from wire-order bytes; the width follows the length
    /// (2, 4, or 16).
    pub fn from_wire_bytes(bytes: &[u8]) -> Result<Self, UuidError> {
        Self::from_bytes(bytes, true)
    }

    fn from_bytes(bytes: &[u8], wire: bool) -> Result<Self, UuidError> {
        let length = bytes.len();
        let mut buffer = [0; 16];
        buffer[..length.min(16)].copy_from_slice(&bytes[..length.min(16)]);
        if wire {
            buffer[..length.min(16)].reverse();
        }
        Ok(match length {
            2 => Self::Uuid16(u16::from_be_bytes([buffer[0], buffer[1]])),
            4 => Self::Uuid32(u32::from_be_bytes([
                buffer[0], buffer[1], buffer[2], buffer[3],
            ])),
            16 => Self::Uuid128(u128::from_be_bytes(buffer)),
            _ => return Err(UuidError::InvalidLength { length }),
        })
    }
}

/// Lowercase text in the form [`Uuid::parse`] accepts, so display and parse
/// round-trip for every width.
impl fmt::Display for Uuid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Uuid16(value) => write!(formatter, "{value:04x}"),
            Self::Uuid32(value) => write!(formatter, "{value:08x}"),
            Self::Uuid128(value) => write!(
                formatter,
                "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
                value >> 96,
                (value >> 80) & 0xffff,
                (value >> 64) & 0xffff,
                (value >> 48) & 0xffff,
                value & 0xffff_ffff_ffff,
            ),
        }
    }
}

impl FromStr for Uuid {
    type Err = UuidError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

/// Encodes the wire-order bytes with no length prefix.
impl Encode for Uuid {
    fn encode(&self, writer: &mut ValueWriter<'_>) -> Result<(), EncodeError> {
        writer.write_bytes(self.to_wire_bytes().as_ref())
    }
}

/// Decodes all remaining bytes as a wire-order UUID; the width follows their
/// length (2, 4, or 16).
impl Decode<'_> for Uuid {
    fn decode(reader: &mut ValueReader<'_>) -> Result<Self, DecodeError> {
        let bytes = reader.peek_remaining();
        let uuid = Self::from_wire_bytes(bytes).map_err(|_| DecodeError::InvalidLength {
            length: bytes.len(),
        })?;
        reader.read_remaining();
        Ok(uuid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode_value, encode_value, test_support::Rng};

    const NUS: u128 = 0x6e40_0001_b5a3_f393_e0a9_e50e_24dc_ca9e;

    #[test]
    fn golden_vectors_for_each_width_and_byte_order() {
        let cases: [(Uuid, &str, &[u8], &[u8]); 3] = [
            (Uuid::Uuid16(0x180f), "180f", &[0x18, 0x0f], &[0x0f, 0x18]),
            (
                Uuid::Uuid32(0x1234_5678),
                "12345678",
                &[0x12, 0x34, 0x56, 0x78],
                &[0x78, 0x56, 0x34, 0x12],
            ),
            (
                Uuid::Uuid128(NUS),
                "6e400001-b5a3-f393-e0a9-e50e24dcca9e",
                &[
                    0x6e, 0x40, 0x00, 0x01, 0xb5, 0xa3, 0xf3, 0x93, 0xe0, 0xa9, 0xe5, 0x0e, 0x24,
                    0xdc, 0xca, 0x9e,
                ],
                &[
                    0x9e, 0xca, 0xdc, 0x24, 0x0e, 0xe5, 0xa9, 0xe0, 0x93, 0xf3, 0xa3, 0xb5, 0x01,
                    0x00, 0x40, 0x6e,
                ],
            ),
        ];
        for (uuid, text, canonical, wire) in cases {
            assert_eq!(uuid.to_string(), text);
            assert_eq!(Uuid::parse(text), Ok(uuid));
            assert_eq!(uuid.to_canonical_bytes().as_ref(), canonical);
            assert_eq!(uuid.to_wire_bytes().as_ref(), wire);
            assert_eq!(Uuid::from_canonical_bytes(canonical), Ok(uuid));
            assert_eq!(Uuid::from_wire_bytes(wire), Ok(uuid));
            assert_eq!(uuid.len(), wire.len());
            assert_eq!(encode_value(&uuid).unwrap(), wire);
            assert_eq!(decode_value::<Uuid>(wire), Ok(uuid));
        }
    }

    #[test]
    fn short_uuids_expand_over_the_base_uuid_but_keep_their_width() {
        let short = Uuid::Uuid16(0x2902);
        let long = Uuid::parse("00002902-0000-1000-8000-00805f9b34fb").unwrap();
        assert_eq!(short.to_u128(), long.to_u128());
        assert_ne!(short, long);
        assert_eq!(
            Uuid::Uuid32(0xabcd_0001).to_string().len(),
            8,
            "a 32-bit UUID keeps its width"
        );
        assert_eq!(
            Uuid::Uuid32(0xabcd_0001).to_u128(),
            0xabcd_0001_0000_1000_8000_0080_5f9b_34fb
        );
        assert_eq!(Uuid::Uuid128(NUS).to_u128(), NUS);
    }

    #[test]
    fn parsing_accepts_either_case_and_boundary_values() {
        assert_eq!(Uuid::parse("ABCD"), Ok(Uuid::Uuid16(0xabcd)));
        assert_eq!(Uuid::parse("0000"), Ok(Uuid::Uuid16(0)));
        assert_eq!(Uuid::parse("ffffffff"), Ok(Uuid::Uuid32(u32::MAX)));
        assert_eq!(
            Uuid::parse("FFFFFFFF-FFFF-FFFF-FFFF-FFFFFFFFFFFF"),
            Ok(Uuid::Uuid128(u128::MAX))
        );
        assert_eq!(
            "00000000-0000-0000-0000-000000000000".parse::<Uuid>(),
            Ok(Uuid::Uuid128(0))
        );
    }

    #[test]
    fn invalid_text_is_rejected_with_its_position() {
        let cases = [
            ("", UuidError::InvalidLength { length: 0 }),
            ("180", UuidError::InvalidLength { length: 3 }),
            ("0x180f", UuidError::InvalidLength { length: 6 }),
            (" 180f", UuidError::InvalidLength { length: 5 }),
            ("180g", UuidError::InvalidCharacter { index: 3 }),
            ("1234567-", UuidError::InvalidCharacter { index: 7 }),
            (
                "6e4000011b5a3-f393-e0a9-e50e24dcca9e",
                UuidError::MissingHyphen { index: 8 },
            ),
            (
                "6e400001-b5a3-f393-e0a9+e50e24dcca9e",
                UuidError::MissingHyphen { index: 23 },
            ),
            (
                "6e400001-b5a3-f393-e0a9-e50e24dcca9-",
                UuidError::InvalidCharacter { index: 35 },
            ),
            (
                "{6e40001-b5a3-f393-e0a9-e50e24dcca9e",
                UuidError::InvalidCharacter { index: 0 },
            ),
            // Multi-byte UTF-8 is rejected by position, not split.
            ("18é", UuidError::InvalidCharacter { index: 2 }),
        ];
        for (text, error) in cases {
            assert_eq!(Uuid::parse(text), Err(error), "{text:?}");
            assert!(!error.to_string().is_empty());
        }
    }

    #[test]
    fn invalid_byte_lengths_are_rejected() {
        for length in [0, 1, 3, 5, 15, 17, 32] {
            let bytes = vec![0xa5; length];
            assert_eq!(
                Uuid::from_wire_bytes(&bytes),
                Err(UuidError::InvalidLength { length })
            );
            assert_eq!(
                Uuid::from_canonical_bytes(&bytes),
                Err(UuidError::InvalidLength { length })
            );
        }
        assert_eq!(
            decode_value::<Uuid>(&[1, 2, 3]),
            Err(DecodeError::InvalidLength { length: 3 })
        );
    }

    #[test]
    fn random_uuids_round_trip_through_every_representation() {
        let mut rng = Rng::new(0x5eed_0001);
        for _ in 0..2_000 {
            let value = rng.next_u128();
            for uuid in [
                Uuid::Uuid16(value as u16),
                Uuid::Uuid32(value as u32),
                Uuid::Uuid128(value),
            ] {
                let text = uuid.to_string();
                assert_eq!(text.parse::<Uuid>(), Ok(uuid));
                assert_eq!(Uuid::parse(&text.to_uppercase()), Ok(uuid));
                assert_eq!(
                    Uuid::from_canonical_bytes(uuid.to_canonical_bytes().as_ref()),
                    Ok(uuid)
                );
                assert_eq!(
                    Uuid::from_wire_bytes(uuid.to_wire_bytes().as_ref()),
                    Ok(uuid)
                );
                let mut reversed = uuid.to_canonical_bytes().as_ref().to_vec();
                reversed.reverse();
                assert_eq!(reversed, uuid.to_wire_bytes().as_ref());
                assert_eq!(
                    decode_value::<Uuid>(&encode_value(&uuid).unwrap()),
                    Ok(uuid)
                );
            }
        }
    }

    #[test]
    fn parsing_works_in_constant_evaluation() {
        const PARSED: Result<Uuid, UuidError> = Uuid::parse("2a19");
        const REJECTED: Result<Uuid, UuidError> = Uuid::parse("2a1");
        assert_eq!(PARSED, Ok(Uuid::Uuid16(0x2a19)));
        assert_eq!(REJECTED, Err(UuidError::InvalidLength { length: 3 }));
    }
}

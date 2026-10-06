//! Explicit wire encoding for attribute values.
//!
//! Values cross the BLE boundary only as bytes produced by an [`Encode`]
//! implementation and consumed by a [`Decode`] implementation. Nothing here
//! serializes a Rust memory layout: every built-in codec states its length and
//! byte order, and decoding rejects input that does not match.
//!
//! # Built-in codecs
//!
//! | Type | Encoding | Length |
//! | --- | --- | --- |
//! | `u8`–`u128`, `i8`–`i128` | two's complement, little-endian | type size |
//! | `f32`, `f64` | IEEE 754 bit pattern, little-endian | 4 or 8 |
//! | [`BigEndian<T>`] for the types above | big-endian | type size |
//! | `bool` | `0x00` false, `0x01` true; other bytes are rejected | 1 |
//! | `[u8; N]` | the bytes | `N` |
//! | `&[u8]`, `Vec<u8>` | the bytes | all remaining |
//! | `&str`, `String` | UTF-8 with no terminator or length prefix | all remaining |
//! | [`Uuid`](crate::Uuid) | wire (least significant first) order | all remaining: 2, 4, or 16 |
//!
//! Little-endian is the Bluetooth convention for multi-byte fields; use
//! [`BigEndian`] when an application protocol requires the other order.
//! Floats keep their exact bit pattern, including NaN payloads.
//!
//! Variable-length types consume every remaining byte, so they must be the last
//! field of a value. An application codec that needs a variable-length field
//! elsewhere reads it with an explicit length using
//! [`ValueReader::read_bytes`] or [`ValueReader::read_str`].
//!
//! # Application codecs
//!
//! Implement [`Encode`] and [`Decode`] for application types. Implementations
//! only see a [`ValueWriter`] or [`ValueReader`] over Rust-owned bytes; neither
//! exposes native buffers, pointers, or SDK types, so a safe codec cannot reach
//! NimBLE memory.
//!
//! ```
//! use argyle_nimble::codec::{
//!     decode_value, encode_value, Decode, DecodeError, Encode, EncodeError, ValueReader,
//!     ValueWriter,
//! };
//!
//! /// A temperature in hundredths of a degree followed by a sensor name.
//! #[derive(Debug, PartialEq)]
//! struct Reading {
//!     centidegrees: i16,
//!     sensor: String,
//! }
//!
//! impl Encode for Reading {
//!     fn encode(&self, writer: &mut ValueWriter<'_>) -> Result<(), EncodeError> {
//!         writer.write(&self.centidegrees)?;
//!         writer.write(self.sensor.as_str())
//!     }
//! }
//!
//! impl Decode<'_> for Reading {
//!     fn decode(reader: &mut ValueReader<'_>) -> Result<Self, DecodeError> {
//!         let centidegrees = reader.read()?;
//!         let sensor = reader.read()?;
//!         Ok(Self { centidegrees, sensor })
//!     }
//! }
//!
//! let reading = Reading { centidegrees: -125, sensor: "probe".into() };
//! let bytes = encode_value(&reading)?;
//! assert_eq!(bytes, b"\x83\xffprobe");
//! assert_eq!(decode_value::<Reading>(&bytes)?, reading);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::fmt;

/// The largest attribute value the Bluetooth Core Specification allows
/// (Vol 3, Part F, 3.2.9). [`encode_value`] uses it as the output capacity.
pub const MAX_ATTRIBUTE_VALUE_LEN: usize = 512;

/// Why a value could not be encoded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum EncodeError {
    /// The encoded value does not fit in the writer's remaining capacity.
    CapacityExceeded {
        /// Bytes the failed write needed.
        needed: usize,
        /// Bytes that were still available.
        remaining: usize,
    },
    /// The value cannot be represented in its wire encoding. Application
    /// codecs return this for domain values they refuse to encode.
    InvalidValue {
        /// A short, static description for diagnostics.
        reason: &'static str,
    },
}

impl fmt::Display for EncodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityExceeded { needed, remaining } => write!(
                formatter,
                "encoding needed {needed} bytes but only {remaining} remained"
            ),
            Self::InvalidValue { reason } => write!(formatter, "value cannot be encoded: {reason}"),
        }
    }
}

impl std::error::Error for EncodeError {}

/// Why received bytes could not be decoded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DecodeError {
    /// The input ended before a fixed-length field.
    Truncated {
        /// Bytes the field needed.
        needed: usize,
        /// Bytes that were left.
        available: usize,
    },
    /// Bytes remained after the complete value was decoded.
    TrailingBytes {
        /// The number of unread bytes.
        count: usize,
    },
    /// The input length is not one the type accepts, such as a UUID that is
    /// not 2, 4, or 16 bytes.
    InvalidLength {
        /// The rejected length in bytes.
        length: usize,
    },
    /// A text field is not valid UTF-8.
    InvalidUtf8 {
        /// Bytes of the field that were valid before the error.
        valid_up_to: usize,
    },
    /// The bytes are well-formed but encode a value outside the type's
    /// domain, such as a `bool` byte other than 0 or 1. Application codecs
    /// return this for domain violations.
    InvalidValue {
        /// A short, static description for diagnostics.
        reason: &'static str,
    },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { needed, available } => write!(
                formatter,
                "value truncated: needed {needed} bytes but {available} remained"
            ),
            Self::TrailingBytes { count } => {
                write!(formatter, "{count} unexpected bytes followed the value")
            }
            Self::InvalidLength { length } => {
                write!(formatter, "a value of {length} bytes is not a valid length")
            }
            Self::InvalidUtf8 { valid_up_to } => write!(
                formatter,
                "text is not valid UTF-8 after {valid_up_to} bytes"
            ),
            Self::InvalidValue { reason } => write!(formatter, "invalid value: {reason}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// A type with an explicit wire encoding.
///
/// Implementations write through the [`ValueWriter`] only. Return
/// [`EncodeError::InvalidValue`] for values that have no valid encoding.
pub trait Encode {
    /// Append this value's encoding to `writer`.
    fn encode(&self, writer: &mut ValueWriter<'_>) -> Result<(), EncodeError>;
}

/// A type that can be decoded from its wire encoding.
///
/// `'a` is the lifetime of the received bytes, which lets borrowed types such
/// as `&'a str` decode without copying. Implementations read through the
/// [`ValueReader`] only and must reject input outside the type's domain.
pub trait Decode<'a>: Sized {
    /// Read one value from `reader`.
    fn decode(reader: &mut ValueReader<'a>) -> Result<Self, DecodeError>;
}

/// A type that decodes without borrowing from the received bytes, such as
/// `u16`, `String`, or `Vec<u8>` but not `&str`. Implemented automatically.
///
/// Values handed to application code after the received bytes are released,
/// such as written characteristic values, require this bound.
pub trait DecodeOwned: for<'a> Decode<'a> {}

impl<T: for<'a> Decode<'a>> DecodeOwned for T {}

/// A bounded writer that appends encoded bytes to a caller's `Vec<u8>`.
///
/// The capacity limits the bytes this writer may add, not the vector's
/// allocation. [`write_bytes`](Self::write_bytes) and [`write`](Self::write)
/// either succeed completely or leave the output unchanged; calling
/// [`Encode::encode`] directly gives no such rollback.
#[derive(Debug)]
pub struct ValueWriter<'a> {
    output: &'a mut Vec<u8>,
    start: usize,
    capacity: usize,
}

impl<'a> ValueWriter<'a> {
    /// A writer that may append up to `capacity` bytes after the existing
    /// contents of `output`.
    pub fn new(output: &'a mut Vec<u8>, capacity: usize) -> Self {
        let start = output.len();
        Self {
            output,
            start,
            capacity,
        }
    }

    /// The most bytes this writer may append.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Bytes appended so far.
    pub fn len(&self) -> usize {
        self.output.len() - self.start
    }

    /// Whether nothing has been appended.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes that may still be appended.
    pub fn remaining(&self) -> usize {
        self.capacity - self.len()
    }

    /// The bytes appended so far.
    pub fn written(&self) -> &[u8] {
        &self.output[self.start..]
    }

    /// Append `bytes`, or fail without writing if they do not fit.
    pub fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), EncodeError> {
        let remaining = self.remaining();
        if bytes.len() > remaining {
            return Err(EncodeError::CapacityExceeded {
                needed: bytes.len(),
                remaining,
            });
        }
        self.output.extend_from_slice(bytes);
        Ok(())
    }

    /// Append `value`'s encoding. If encoding fails part-way, the bytes it
    /// wrote are removed before the error is returned.
    pub fn write<T: Encode + ?Sized>(&mut self, value: &T) -> Result<(), EncodeError> {
        let length = self.output.len();
        // Encode through a reborrowed writer: even an implementation that
        // replaces the writer it is given cannot detach this one from its
        // output, so the rollback below always applies to the right bytes.
        let result = value.encode(&mut ValueWriter {
            output: &mut *self.output,
            start: self.start,
            capacity: self.capacity,
        });
        if result.is_err() {
            self.output.truncate(length);
        }
        result
    }
}

/// A cursor over received bytes.
///
/// Reads never panic: a read past the end returns
/// [`DecodeError::Truncated`] and consumes nothing. [`read`](Self::read)
/// rewinds the reader if decoding fails; calling [`Decode::decode`] directly
/// gives no such rollback.
#[derive(Clone, Debug)]
pub struct ValueReader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> ValueReader<'a> {
    /// A reader positioned at the start of `data`.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    /// Bytes consumed so far.
    pub fn position(&self) -> usize {
        self.position
    }

    /// Bytes not yet consumed.
    pub fn remaining(&self) -> usize {
        self.data.len() - self.position
    }

    /// Whether every byte has been consumed.
    pub fn is_finished(&self) -> bool {
        self.remaining() == 0
    }

    /// Consume exactly `length` bytes.
    pub fn read_bytes(&mut self, length: usize) -> Result<&'a [u8], DecodeError> {
        let available = self.remaining();
        if length > available {
            return Err(DecodeError::Truncated {
                needed: length,
                available,
            });
        }
        let bytes = &self.data[self.position..self.position + length];
        self.position += length;
        Ok(bytes)
    }

    /// Consume exactly `N` bytes into an array.
    pub fn read_array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let bytes = self.read_bytes(N)?;
        let mut array = [0; N];
        array.copy_from_slice(bytes);
        Ok(array)
    }

    /// Consume exactly `length` bytes as UTF-8 text. Invalid UTF-8 consumes
    /// nothing.
    pub fn read_str(&mut self, length: usize) -> Result<&'a str, DecodeError> {
        let start = self.position;
        let bytes = self.read_bytes(length)?;
        std::str::from_utf8(bytes).map_err(|error| {
            self.position = start;
            DecodeError::InvalidUtf8 {
                valid_up_to: error.valid_up_to(),
            }
        })
    }

    /// The unread bytes, without consuming them.
    pub fn peek_remaining(&self) -> &'a [u8] {
        &self.data[self.position..]
    }

    /// Consume and return every unread byte.
    pub fn read_remaining(&mut self) -> &'a [u8] {
        let bytes = self.peek_remaining();
        self.position = self.data.len();
        bytes
    }

    /// Decode one `T`. If decoding fails, the reader is left exactly as it
    /// was, even if the implementation replaced it.
    pub fn read<T: Decode<'a>>(&mut self) -> Result<T, DecodeError> {
        let saved = self.clone();
        let result = T::decode(self);
        if result.is_err() {
            *self = saved;
        }
        result
    }

    /// Require that every byte was consumed.
    pub fn finish(self) -> Result<(), DecodeError> {
        match self.remaining() {
            0 => Ok(()),
            count => Err(DecodeError::TrailingBytes { count }),
        }
    }
}

/// Encode a complete attribute value of at most
/// [`MAX_ATTRIBUTE_VALUE_LEN`] bytes.
pub fn encode_value<T: Encode + ?Sized>(value: &T) -> Result<Vec<u8>, EncodeError> {
    let mut output = Vec::new();
    ValueWriter::new(&mut output, MAX_ATTRIBUTE_VALUE_LEN).write(value)?;
    Ok(output)
}

/// Decode a complete value, rejecting trailing bytes.
pub fn decode_value<'a, T: Decode<'a>>(bytes: &'a [u8]) -> Result<T, DecodeError> {
    let mut reader = ValueReader::new(bytes);
    let value = reader.read()?;
    reader.finish()?;
    Ok(value)
}

/// Encodes and decodes the wrapped scalar in big-endian byte order instead of
/// the default little-endian order.
///
/// ```
/// use argyle_nimble::codec::{encode_value, BigEndian};
///
/// assert_eq!(encode_value(&BigEndian(0x1234_u16))?, [0x12, 0x34]);
/// assert_eq!(encode_value(&0x1234_u16)?, [0x34, 0x12]);
/// # Ok::<(), argyle_nimble::codec::EncodeError>(())
/// ```
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BigEndian<T>(pub T);

macro_rules! scalar_codecs {
    ($($scalar:ty),* $(,)?) => {$(
        impl Encode for $scalar {
            fn encode(&self, writer: &mut ValueWriter<'_>) -> Result<(), EncodeError> {
                writer.write_bytes(&self.to_le_bytes())
            }
        }

        impl Decode<'_> for $scalar {
            fn decode(reader: &mut ValueReader<'_>) -> Result<Self, DecodeError> {
                Ok(Self::from_le_bytes(
                    reader.read_array::<{ std::mem::size_of::<$scalar>() }>()?,
                ))
            }
        }

        impl Encode for BigEndian<$scalar> {
            fn encode(&self, writer: &mut ValueWriter<'_>) -> Result<(), EncodeError> {
                writer.write_bytes(&self.0.to_be_bytes())
            }
        }

        impl Decode<'_> for BigEndian<$scalar> {
            fn decode(reader: &mut ValueReader<'_>) -> Result<Self, DecodeError> {
                Ok(Self(<$scalar>::from_be_bytes(
                    reader.read_array::<{ std::mem::size_of::<$scalar>() }>()?,
                )))
            }
        }
    )*};
}

scalar_codecs!(u8, u16, u32, u64, u128, i8, i16, i32, i64, i128, f32, f64);

impl Encode for bool {
    fn encode(&self, writer: &mut ValueWriter<'_>) -> Result<(), EncodeError> {
        writer.write_bytes(&[u8::from(*self)])
    }
}

impl Decode<'_> for bool {
    fn decode(reader: &mut ValueReader<'_>) -> Result<Self, DecodeError> {
        match reader.read_array::<1>()? {
            [0] => Ok(false),
            [1] => Ok(true),
            _ => Err(DecodeError::InvalidValue {
                reason: "a bool must be 0x00 or 0x01",
            }),
        }
    }
}

impl<const N: usize> Encode for [u8; N] {
    fn encode(&self, writer: &mut ValueWriter<'_>) -> Result<(), EncodeError> {
        writer.write_bytes(self)
    }
}

impl<const N: usize> Decode<'_> for [u8; N] {
    fn decode(reader: &mut ValueReader<'_>) -> Result<Self, DecodeError> {
        reader.read_array()
    }
}

impl Encode for [u8] {
    fn encode(&self, writer: &mut ValueWriter<'_>) -> Result<(), EncodeError> {
        writer.write_bytes(self)
    }
}

impl<'a> Decode<'a> for &'a [u8] {
    fn decode(reader: &mut ValueReader<'a>) -> Result<Self, DecodeError> {
        Ok(reader.read_remaining())
    }
}

impl Encode for Vec<u8> {
    fn encode(&self, writer: &mut ValueWriter<'_>) -> Result<(), EncodeError> {
        writer.write_bytes(self)
    }
}

impl Decode<'_> for Vec<u8> {
    fn decode(reader: &mut ValueReader<'_>) -> Result<Self, DecodeError> {
        Ok(reader.read_remaining().to_vec())
    }
}

impl Encode for str {
    fn encode(&self, writer: &mut ValueWriter<'_>) -> Result<(), EncodeError> {
        writer.write_bytes(self.as_bytes())
    }
}

impl<'a> Decode<'a> for &'a str {
    fn decode(reader: &mut ValueReader<'a>) -> Result<Self, DecodeError> {
        reader.read_str(reader.remaining())
    }
}

impl Encode for String {
    fn encode(&self, writer: &mut ValueWriter<'_>) -> Result<(), EncodeError> {
        writer.write_bytes(self.as_bytes())
    }
}

impl Decode<'_> for String {
    fn decode(reader: &mut ValueReader<'_>) -> Result<Self, DecodeError> {
        reader.read_str(reader.remaining()).map(str::to_owned)
    }
}

impl<T: Encode + ?Sized> Encode for &T {
    fn encode(&self, writer: &mut ValueWriter<'_>) -> Result<(), EncodeError> {
        (**self).encode(writer)
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    /// A seeded xorshift generator so property tests repeat exactly.
    pub(crate) struct Rng(u64);

    impl Rng {
        pub(crate) fn new(seed: u64) -> Self {
            Self(seed.max(1))
        }

        pub(crate) fn next_u64(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        pub(crate) fn next_u128(&mut self) -> u128 {
            (u128::from(self.next_u64()) << 64) | u128::from(self.next_u64())
        }

        pub(crate) fn bytes(&mut self, max_length: usize) -> Vec<u8> {
            let length = (self.next_u64() as usize) % (max_length + 1);
            (0..length).map(|_| self.next_u64() as u8).collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::Rng;
    use super::*;
    use std::fmt::Debug;

    fn round_trip<T>(value: T, expected: &[u8])
    where
        T: Encode + for<'a> Decode<'a> + PartialEq + Debug,
    {
        assert_eq!(encode_value(&value).unwrap(), expected, "{value:?}");
        assert_eq!(decode_value::<T>(expected), Ok(value));
    }

    #[test]
    fn integer_golden_vectors_are_little_endian_by_default() {
        round_trip(0_u8, &[0x00]);
        round_trip(u8::MAX, &[0xff]);
        round_trip(0x1234_u16, &[0x34, 0x12]);
        round_trip(0x1234_5678_u32, &[0x78, 0x56, 0x34, 0x12]);
        round_trip(
            0x0102_0304_0506_0708_u64,
            &[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01],
        );
        round_trip(1_u128, &[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        round_trip(-1_i8, &[0xff]);
        round_trip(i16::MIN, &[0x00, 0x80]);
        round_trip(-2_i32, &[0xfe, 0xff, 0xff, 0xff]);
        round_trip(i64::MAX, &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f]);
        round_trip(i128::MIN, &{
            let mut bytes = [0; 16];
            bytes[15] = 0x80;
            bytes
        });
    }

    #[test]
    fn big_endian_wrapper_reverses_the_byte_order() {
        round_trip(BigEndian(0x1234_u16), &[0x12, 0x34]);
        round_trip(BigEndian(0x1234_5678_u32), &[0x12, 0x34, 0x56, 0x78]);
        round_trip(BigEndian(-2_i16), &[0xff, 0xfe]);
        round_trip(BigEndian(1.0_f32), &[0x3f, 0x80, 0x00, 0x00]);
        round_trip(BigEndian(0xab_u8), &[0xab]);
    }

    #[test]
    fn floats_keep_their_ieee_754_bit_patterns() {
        round_trip(1.0_f32, &[0x00, 0x00, 0x80, 0x3f]);
        round_trip(-2.5_f64, &[0, 0, 0, 0, 0, 0, 0x04, 0xc0]);
        round_trip(f32::INFINITY, &[0x00, 0x00, 0x80, 0x7f]);
        assert_eq!(encode_value(&-0.0_f32).unwrap(), [0, 0, 0, 0x80]);
        // A NaN with a payload survives exactly; NaN != NaN, so compare bits.
        let nan = f64::from_bits(0x7ff8_0000_dead_beef);
        let encoded = encode_value(&nan).unwrap();
        assert_eq!(encoded, 0x7ff8_0000_dead_beef_u64.to_le_bytes());
        assert_eq!(
            decode_value::<f64>(&encoded).unwrap().to_bits(),
            nan.to_bits()
        );
    }

    #[test]
    fn bool_accepts_only_zero_and_one() {
        round_trip(false, &[0]);
        round_trip(true, &[1]);
        for byte in [2_u8, 0x80, 0xff] {
            assert!(matches!(
                decode_value::<bool>(&[byte]),
                Err(DecodeError::InvalidValue { .. })
            ));
        }
    }

    #[test]
    fn bytes_and_text_use_every_remaining_byte() {
        round_trip([1_u8, 2, 3], &[1, 2, 3]);
        round_trip([0_u8; 0], &[]);
        round_trip(vec![9_u8, 8], &[9, 8]);
        round_trip(Vec::<u8>::new(), &[]);
        round_trip(String::from("héllo"), "héllo".as_bytes());
        round_trip(String::new(), &[]);
        assert_eq!(decode_value::<&str>(b"abc"), Ok("abc"));
        assert_eq!(decode_value::<&[u8]>(&[7, 7]), Ok(&[7_u8, 7][..]));
        assert_eq!(encode_value("abc").unwrap(), b"abc");
        assert_eq!(encode_value(&b"xy"[..]).unwrap(), b"xy");
    }

    #[test]
    fn malformed_utf8_is_rejected_with_its_valid_prefix() {
        assert_eq!(
            decode_value::<String>(&[b'o', b'k', 0xff]),
            Err(DecodeError::InvalidUtf8 { valid_up_to: 2 })
        );
        // A truncated multi-byte sequence is also invalid.
        assert_eq!(
            decode_value::<&str>(&[0xc3]),
            Err(DecodeError::InvalidUtf8 { valid_up_to: 0 })
        );
        let mut reader = ValueReader::new(&[b'a', 0xc3, 0x28]);
        assert_eq!(
            reader.read_str(3),
            Err(DecodeError::InvalidUtf8 { valid_up_to: 1 })
        );
        assert_eq!(reader.position(), 0, "a failed read consumes nothing");
    }

    #[test]
    fn truncated_and_oversized_inputs_are_rejected() {
        assert_eq!(
            decode_value::<u32>(&[1, 2, 3]),
            Err(DecodeError::Truncated {
                needed: 4,
                available: 3
            })
        );
        assert_eq!(
            decode_value::<u8>(&[]),
            Err(DecodeError::Truncated {
                needed: 1,
                available: 0
            })
        );
        assert_eq!(
            decode_value::<u16>(&[1, 2, 3]),
            Err(DecodeError::TrailingBytes { count: 1 })
        );
        assert_eq!(
            decode_value::<[u8; 2]>(&[1]),
            Err(DecodeError::Truncated {
                needed: 2,
                available: 1
            })
        );
        assert_eq!(
            decode_value::<bool>(&[1, 0]),
            Err(DecodeError::TrailingBytes { count: 1 })
        );
    }

    #[test]
    fn output_capacity_failures_leave_the_output_unchanged() {
        let mut output = vec![0xee];
        let mut writer = ValueWriter::new(&mut output, 3);
        assert_eq!(writer.write(&0xaabb_u16), Ok(()));
        assert_eq!(
            writer.write(&0x1122_u16),
            Err(EncodeError::CapacityExceeded {
                needed: 2,
                remaining: 1
            })
        );
        assert_eq!((writer.len(), writer.remaining()), (2, 1));
        assert_eq!(writer.written(), [0xbb, 0xaa]);
        assert_eq!(writer.write(&7_u8), Ok(()));
        assert_eq!(
            writer.write_bytes(&[1]),
            Err(EncodeError::CapacityExceeded {
                needed: 1,
                remaining: 0
            })
        );
        assert_eq!(output, [0xee, 0xbb, 0xaa, 7], "existing contents are kept");

        let mut empty = Vec::new();
        let mut zero = ValueWriter::new(&mut empty, 0);
        assert!(zero.is_empty());
        assert_eq!(zero.write(""), Ok(()), "an empty value fits anywhere");
        assert!(zero.write(&0_u8).is_err());
    }

    #[test]
    fn values_up_to_the_attribute_limit_encode_and_longer_ones_fail() {
        let largest = vec![0x5a; MAX_ATTRIBUTE_VALUE_LEN];
        assert_eq!(
            encode_value(&largest).unwrap().len(),
            MAX_ATTRIBUTE_VALUE_LEN
        );
        let too_long = vec![0x5a; MAX_ATTRIBUTE_VALUE_LEN + 1];
        assert_eq!(
            encode_value(&too_long),
            Err(EncodeError::CapacityExceeded {
                needed: MAX_ATTRIBUTE_VALUE_LEN + 1,
                remaining: MAX_ATTRIBUTE_VALUE_LEN
            })
        );
    }

    /// An application codec that writes a field, then fails.
    struct HalfWritten;

    impl Encode for HalfWritten {
        fn encode(&self, writer: &mut ValueWriter<'_>) -> Result<(), EncodeError> {
            writer.write(&0x0102_u16)?;
            Err(EncodeError::InvalidValue {
                reason: "second field out of range",
            })
        }
    }

    /// An application codec with a length-prefixed name and a domain check.
    #[derive(Debug, PartialEq)]
    struct Named<'a> {
        level: u8,
        name: &'a str,
        flags: BigEndian<u16>,
    }

    impl Encode for Named<'_> {
        fn encode(&self, writer: &mut ValueWriter<'_>) -> Result<(), EncodeError> {
            if self.level > 100 {
                return Err(EncodeError::InvalidValue {
                    reason: "level above 100",
                });
            }
            let length = u8::try_from(self.name.len()).map_err(|_| EncodeError::InvalidValue {
                reason: "name longer than 255 bytes",
            })?;
            writer.write(&self.level)?;
            writer.write(&length)?;
            writer.write(self.name)?;
            writer.write(&self.flags)
        }
    }

    impl<'a> Decode<'a> for Named<'a> {
        fn decode(reader: &mut ValueReader<'a>) -> Result<Self, DecodeError> {
            let level: u8 = reader.read()?;
            if level > 100 {
                return Err(DecodeError::InvalidValue {
                    reason: "level above 100",
                });
            }
            let length: u8 = reader.read()?;
            let name = reader.read_str(usize::from(length))?;
            let flags = reader.read()?;
            Ok(Self { level, name, flags })
        }
    }

    #[test]
    fn application_codecs_compose_and_fail_atomically() {
        let value = Named {
            level: 42,
            name: "fan",
            flags: BigEndian(0x0102),
        };
        let bytes = encode_value(&value).unwrap();
        assert_eq!(bytes, [42, 3, b'f', b'a', b'n', 0x01, 0x02]);
        assert_eq!(decode_value::<Named<'_>>(&bytes), Ok(value));

        let mut output = Vec::new();
        let mut writer = ValueWriter::new(&mut output, 16);
        writer.write(&9_u8).unwrap();
        assert!(matches!(
            writer.write(&HalfWritten),
            Err(EncodeError::InvalidValue { .. })
        ));
        assert_eq!(writer.written(), [9], "the partial field was removed");

        assert!(matches!(
            encode_value(&Named {
                level: 101,
                name: "",
                flags: BigEndian(0)
            }),
            Err(EncodeError::InvalidValue { .. })
        ));
        assert!(matches!(
            decode_value::<Named<'_>>(&[101, 0, 0, 0]),
            Err(DecodeError::InvalidValue { .. })
        ));
        assert_eq!(
            decode_value::<Named<'_>>(&[1, 5, b'a']),
            Err(DecodeError::Truncated {
                needed: 5,
                available: 1
            })
        );
        let mut reader = ValueReader::new(&[1, 9]);
        assert!(reader.read::<Named<'_>>().is_err());
        assert_eq!(reader.position(), 0, "a failed read rewinds the reader");
    }

    /// Codecs that replace the writer or reader they are given, then fail.
    struct ReplacesWriter;

    impl Encode for ReplacesWriter {
        fn encode(&self, writer: &mut ValueWriter<'_>) -> Result<(), EncodeError> {
            writer.write_bytes(&[0xaa])?;
            *writer = ValueWriter::new(Box::leak(Box::default()), 0);
            Err(EncodeError::InvalidValue { reason: "replaced" })
        }
    }

    struct ReplacesReader;

    impl<'a> Decode<'a> for ReplacesReader {
        fn decode(reader: &mut ValueReader<'a>) -> Result<Self, DecodeError> {
            reader.read_bytes(1)?;
            *reader = ValueReader::new(&[]);
            Err(DecodeError::InvalidValue { reason: "replaced" })
        }
    }

    #[test]
    fn rollback_survives_codecs_that_replace_their_writer_or_reader() {
        let mut output = vec![1];
        let mut writer = ValueWriter::new(&mut output, 4);
        writer.write(&2_u8).unwrap();
        assert!(writer.write(&ReplacesWriter).is_err());
        assert_eq!((writer.len(), writer.remaining()), (1, 3));
        writer.write(&3_u8).unwrap();
        assert_eq!(output, [1, 2, 3]);

        let mut reader = ValueReader::new(&[1, 2, 3]);
        assert_eq!(reader.read::<u8>(), Ok(1));
        assert!(reader.read::<ReplacesReader>().is_err());
        assert_eq!((reader.position(), reader.remaining()), (1, 2));
        assert_eq!(reader.read::<u16>(), Ok(0x0302));
    }

    #[test]
    fn reader_cursor_reports_progress() {
        let mut reader = ValueReader::new(&[1, 2, 3, 4]);
        assert_eq!(reader.read_array::<1>(), Ok([1]));
        assert_eq!(reader.peek_remaining(), [2, 3, 4]);
        assert_eq!((reader.position(), reader.remaining()), (1, 3));
        assert_eq!(reader.read_bytes(0), Ok(&[][..]));
        assert_eq!(reader.read_remaining(), [2, 3, 4]);
        assert!(reader.is_finished());
        assert_eq!(reader.read_remaining(), [] as [u8; 0]);
        assert_eq!(reader.finish(), Ok(()));
    }

    #[test]
    fn errors_describe_themselves() {
        let errors: [&dyn std::error::Error; 7] = [
            &EncodeError::CapacityExceeded {
                needed: 2,
                remaining: 1,
            },
            &EncodeError::InvalidValue { reason: "r" },
            &DecodeError::Truncated {
                needed: 2,
                available: 1,
            },
            &DecodeError::TrailingBytes { count: 1 },
            &DecodeError::InvalidLength { length: 3 },
            &DecodeError::InvalidUtf8 { valid_up_to: 0 },
            &DecodeError::InvalidValue { reason: "r" },
        ];
        for error in errors {
            assert!(!error.to_string().is_empty());
        }
    }

    #[test]
    fn random_values_round_trip_and_random_input_never_panics() {
        let mut rng = Rng::new(0x5eed_0002);
        for _ in 0..2_000 {
            let raw = rng.next_u128();
            assert_eq!(
                decode_value::<u64>(&encode_value(&(raw as u64)).unwrap()),
                Ok(raw as u64)
            );
            assert_eq!(
                decode_value::<BigEndian<i32>>(&encode_value(&BigEndian(raw as i32)).unwrap()),
                Ok(BigEndian(raw as i32))
            );
            let float = f32::from_bits(raw as u32);
            assert_eq!(
                decode_value::<f32>(&encode_value(&float).unwrap())
                    .unwrap()
                    .to_bits(),
                float.to_bits()
            );
            let bytes = rng.bytes(64);
            assert_eq!(
                decode_value::<Vec<u8>>(&encode_value(&bytes).unwrap()),
                Ok(bytes.clone())
            );
            // Arbitrary input either decodes or returns an error; it never panics.
            let _ = decode_value::<String>(&bytes);
            let _ = decode_value::<Named<'_>>(&bytes);
            let _ = decode_value::<crate::Uuid>(&bytes);
            let _ = decode_value::<bool>(&bytes);
            if let Ok(text) = std::str::from_utf8(&bytes) {
                assert_eq!(decode_value::<&str>(&bytes), Ok(text));
            }
        }
    }
}

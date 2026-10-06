//! ATT response errors and framework errors.
//!
//! The two are separate types on purpose:
//!
//! - [`AttError`] is a protocol result an attribute handler returns to the
//!   connected client, such as "write not permitted". It is a valid ATT error
//!   code, not a failure of this framework.
//! - [`Error`] reports a failure of the framework itself: lifecycle misuse,
//!   native backend failures, time limits, values that cannot be encoded, or
//!   invalid GATT definitions. It is never sent to the client.
//!
//! No conversion exists from [`AttError`] to [`Error`]. A decode failure in a
//! handler converts to the ATT error the client should see with `?`; see the
//! `From` implementations on [`AttError`].

use crate::codec::{DecodeError, EncodeError};
use crate::Uuid;
use std::fmt;

/// An ATT error code returned to the client in an Error Response.
///
/// Codes are defined by the Bluetooth Core Specification (Vol 3, Part F,
/// 3.4.1.1) and the Core Specification Supplement (Part B) for the common
/// profile codes. Constructors accept only codes those documents allocate:
///
/// - `0x01..=0x13`: protocol errors, available as associated constants;
/// - `0x80..=0x9F`: application errors, defined by the application's profile;
/// - `0xFC..=0xFF`: common profile and service errors.
///
/// `0x00` and every range reserved for future use (`0x14..=0x7F`,
/// `0xA0..=0xFB`) are rejected. Some allocated protocol codes, such as
/// [`INVALID_PDU`](Self::INVALID_PDU), describe conditions the host detects
/// itself; attribute handlers normally return permission, length, value, or
/// application errors.
///
/// ```
/// use argyle_nimble::AttError;
///
/// fn check_level(level: u8) -> Result<(), AttError> {
///     if level > 100 {
///         return Err(AttError::VALUE_NOT_ALLOWED);
///     }
///     Ok(())
/// }
///
/// assert_eq!(check_level(101), Err(AttError::VALUE_NOT_ALLOWED));
/// assert_eq!(AttError::application(0x80).map(AttError::code), Ok(0x80));
/// assert!(AttError::from_code(0x14).is_err());
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AttError(u8);

/// A code that is not an allocated ATT error: `0x00` or a reserved range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidAttErrorCode {
    code: u8,
}

impl InvalidAttErrorCode {
    /// The rejected code.
    pub const fn code(&self) -> u8 {
        self.code
    }
}

impl fmt::Display for InvalidAttErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "0x{:02x} is not an allocated ATT error code",
            self.code
        )
    }
}

impl std::error::Error for InvalidAttErrorCode {}

impl AttError {
    /// `0x01`: the attribute handle is invalid on this server.
    pub const INVALID_HANDLE: Self = Self(0x01);
    /// `0x02`: the attribute cannot be read.
    pub const READ_NOT_PERMITTED: Self = Self(0x02);
    /// `0x03`: the attribute cannot be written.
    pub const WRITE_NOT_PERMITTED: Self = Self(0x03);
    /// `0x04`: the request PDU was invalid.
    pub const INVALID_PDU: Self = Self(0x04);
    /// `0x05`: authentication is required.
    pub const INSUFFICIENT_AUTHENTICATION: Self = Self(0x05);
    /// `0x06`: the server does not support the request.
    pub const REQUEST_NOT_SUPPORTED: Self = Self(0x06);
    /// `0x07`: the offset is past the end of the attribute.
    pub const INVALID_OFFSET: Self = Self(0x07);
    /// `0x08`: authorization is required.
    pub const INSUFFICIENT_AUTHORIZATION: Self = Self(0x08);
    /// `0x09`: too many prepared writes are queued.
    pub const PREPARE_QUEUE_FULL: Self = Self(0x09);
    /// `0x0A`: no attribute was found in the requested range.
    pub const ATTRIBUTE_NOT_FOUND: Self = Self(0x0a);
    /// `0x0B`: the attribute cannot be read with a blob request.
    pub const ATTRIBUTE_NOT_LONG: Self = Self(0x0b);
    /// `0x0C`: the encryption key size is too small.
    pub const INSUFFICIENT_ENCRYPTION_KEY_SIZE: Self = Self(0x0c);
    /// `0x0D`: the value length is invalid for this attribute.
    pub const INVALID_ATTRIBUTE_VALUE_LENGTH: Self = Self(0x0d);
    /// `0x0E`: the request failed because of an unlikely error.
    pub const UNLIKELY: Self = Self(0x0e);
    /// `0x0F`: encryption is required.
    pub const INSUFFICIENT_ENCRYPTION: Self = Self(0x0f);
    /// `0x10`: the grouping attribute type is not supported.
    pub const UNSUPPORTED_GROUP_TYPE: Self = Self(0x10);
    /// `0x11`: the server lacks resources to complete the request.
    pub const INSUFFICIENT_RESOURCES: Self = Self(0x11);
    /// `0x12`: the server's database is out of sync with the client.
    pub const DATABASE_OUT_OF_SYNC: Self = Self(0x12);
    /// `0x13`: the value is not allowed.
    pub const VALUE_NOT_ALLOWED: Self = Self(0x13);
    /// `0xFC`: the write request was rejected (Core Specification
    /// Supplement, Part B).
    pub const WRITE_REQUEST_REJECTED: Self = Self(0xfc);
    /// `0xFD`: a Client Characteristic Configuration descriptor is improperly
    /// configured.
    pub const CCCD_IMPROPERLY_CONFIGURED: Self = Self(0xfd);
    /// `0xFE`: a procedure is already in progress.
    pub const PROCEDURE_ALREADY_IN_PROGRESS: Self = Self(0xfe);
    /// `0xFF`: the value is out of range.
    pub const OUT_OF_RANGE: Self = Self(0xff);

    /// Validate an allocated ATT error code.
    pub const fn from_code(code: u8) -> Result<Self, InvalidAttErrorCode> {
        match code {
            0x01..=0x13 | 0x80..=0x9f | 0xfc..=0xff => Ok(Self(code)),
            _ => Err(InvalidAttErrorCode { code }),
        }
    }

    /// An application error in `0x80..=0x9F`, whose meaning the application's
    /// profile defines.
    pub const fn application(code: u8) -> Result<Self, InvalidAttErrorCode> {
        match code {
            0x80..=0x9f => Ok(Self(code)),
            _ => Err(InvalidAttErrorCode { code }),
        }
    }

    /// The code sent to the client.
    pub const fn code(self) -> u8 {
        self.0
    }

    /// Whether this is an application error (`0x80..=0x9F`).
    pub const fn is_application(self) -> bool {
        matches!(self.0, 0x80..=0x9f)
    }

    const fn name(self) -> Option<&'static str> {
        Some(match self.0 {
            0x01 => "invalid handle",
            0x02 => "read not permitted",
            0x03 => "write not permitted",
            0x04 => "invalid PDU",
            0x05 => "insufficient authentication",
            0x06 => "request not supported",
            0x07 => "invalid offset",
            0x08 => "insufficient authorization",
            0x09 => "prepare queue full",
            0x0a => "attribute not found",
            0x0b => "attribute not long",
            0x0c => "insufficient encryption key size",
            0x0d => "invalid attribute value length",
            0x0e => "unlikely error",
            0x0f => "insufficient encryption",
            0x10 => "unsupported group type",
            0x11 => "insufficient resources",
            0x12 => "database out of sync",
            0x13 => "value not allowed",
            0xfc => "write request rejected",
            0xfd => "CCCD improperly configured",
            0xfe => "procedure already in progress",
            0xff => "out of range",
            _ => return None,
        })
    }
}

impl fmt::Display for AttError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(name) => write!(formatter, "ATT error 0x{:02x} ({name})", self.0),
            None => write!(formatter, "ATT application error 0x{:02x}", self.0),
        }
    }
}

impl std::error::Error for AttError {}

impl TryFrom<u8> for AttError {
    type Error = InvalidAttErrorCode;

    fn try_from(code: u8) -> Result<Self, Self::Error> {
        Self::from_code(code)
    }
}

/// A received value the handler cannot decode is the client's error: a length
/// mismatch is [`AttError::INVALID_ATTRIBUTE_VALUE_LENGTH`], and malformed or
/// out-of-domain content is [`AttError::VALUE_NOT_ALLOWED`].
impl From<DecodeError> for AttError {
    fn from(error: DecodeError) -> Self {
        match error {
            DecodeError::Truncated { .. }
            | DecodeError::TrailingBytes { .. }
            | DecodeError::InvalidLength { .. } => Self::INVALID_ATTRIBUTE_VALUE_LENGTH,
            DecodeError::InvalidUtf8 { .. } | DecodeError::InvalidValue { .. } => {
                Self::VALUE_NOT_ALLOWED
            }
        }
    }
}

/// A value the server cannot encode is a server fault, not the client's: it
/// is reported as [`AttError::UNLIKELY`].
impl From<EncodeError> for AttError {
    fn from(_: EncodeError) -> Self {
        Self::UNLIKELY
    }
}

/// The category of a framework [`Error`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// An operation was used in a state that does not allow it, such as a
    /// second owner of the BLE host or a reentrant shutdown.
    Lifecycle,
    /// The native NimBLE host reported a failure. The error's
    /// [`source`](std::error::Error::source) is a [`BackendError`].
    Backend,
    /// An outgoing value could not be encoded. The error's
    /// [`source`](std::error::Error::source) is the [`EncodeError`].
    Encode,
    /// An operation did not complete within its time limit, such as host
    /// synchronization during startup.
    Timeout,
    /// A GATT definition is structurally invalid, such as a characteristic
    /// with no read, write, or notify capability, or a reserved or repeated
    /// descriptor UUID.
    Definition,
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Lifecycle => "lifecycle error",
            Self::Backend => "native backend error",
            Self::Encode => "encoding error",
            Self::Timeout => "timed out",
            Self::Definition => "invalid GATT definition",
        })
    }
}

/// A failure of the framework, as distinct from an [`AttError`] sent to the
/// client.
///
/// Match on [`kind`](Error::kind) to decide how to react, and use the
/// [`source`](std::error::Error::source) chain or `Display` for diagnostics.
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    cause: Cause,
}

#[derive(Debug)]
enum Cause {
    Message {
        operation: Option<&'static str>,
        message: &'static str,
    },
    Backend(BackendError),
    Encode(EncodeError),
    // Boxed so the common error paths stay small.
    Definition(Box<DefinitionLocation>),
}

#[derive(Debug)]
struct DefinitionLocation {
    service: Option<Uuid>,
    characteristic: Option<Uuid>,
    descriptor: Option<Uuid>,
    problem: &'static str,
}

impl Error {
    /// The category of this failure.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The native failure, for [`ErrorKind::Backend`] errors.
    pub fn backend(&self) -> Option<&BackendError> {
        match &self.cause {
            Cause::Backend(error) => Some(error),
            _ => None,
        }
    }

    /// A failure described by a fixed message, optionally naming the
    /// operation that failed.
    pub(crate) fn new(
        kind: ErrorKind,
        operation: Option<&'static str>,
        message: &'static str,
    ) -> Self {
        Self {
            kind,
            cause: Cause::Message { operation, message },
        }
    }
}

impl Error {
    /// An invalid GATT definition, located by service, characteristic, and
    /// descriptor UUID where known.
    pub(crate) fn definition(
        service: Option<Uuid>,
        characteristic: Option<Uuid>,
        descriptor: Option<Uuid>,
        problem: &'static str,
    ) -> Self {
        Self {
            kind: ErrorKind::Definition,
            cause: Cause::Definition(Box::new(DefinitionLocation {
                service,
                characteristic,
                descriptor,
                problem,
            })),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.cause {
            Cause::Definition(location) => {
                let DefinitionLocation {
                    service,
                    characteristic,
                    descriptor,
                    problem,
                } = location.as_ref();
                write!(formatter, "{}: ", self.kind)?;
                if let Some(service) = service {
                    write!(formatter, "service {service}: ")?;
                }
                if let Some(characteristic) = characteristic {
                    write!(formatter, "characteristic {characteristic}: ")?;
                }
                if let Some(descriptor) = descriptor {
                    write!(formatter, "descriptor {descriptor}: ")?;
                }
                formatter.write_str(problem)
            }
            Cause::Message {
                operation: Some(operation),
                message,
            } => write!(formatter, "{}: {operation}: {message}", self.kind),
            Cause::Message {
                operation: None,
                message,
            } => write!(formatter, "{}: {message}", self.kind),
            Cause::Backend(error) => write!(formatter, "{}: {error}", self.kind),
            Cause::Encode(error) => write!(formatter, "{}: {error}", self.kind),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.cause {
            Cause::Message { .. } | Cause::Definition(_) => None,
            Cause::Backend(error) => Some(error),
            Cause::Encode(error) => Some(error),
        }
    }
}

impl From<EncodeError> for Error {
    fn from(error: EncodeError) -> Self {
        Self {
            kind: ErrorKind::Encode,
            cause: Cause::Encode(error),
        }
    }
}

impl From<BackendError> for Error {
    fn from(error: BackendError) -> Self {
        Self {
            kind: ErrorKind::Backend,
            cause: Cause::Backend(error),
        }
    }
}

/// A failure reported by ESP-IDF or the native NimBLE host.
///
/// It names the native operation and keeps the SDK status, labelled with the
/// status family the operation returns, in its `Display` and `Debug` output
/// for diagnosis, including statuses this crate does not recognise. When a
/// NimBLE host status carries an ATT error, [`att_error`](Self::att_error)
/// returns it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BackendError {
    operation: &'static str,
    detail: BackendDetail,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BackendDetail {
    /// An `esp_err_t` from an ESP-IDF port function.
    EspError(i32),
    /// A NimBLE host status (`BLE_HS_*`, or an ATT/HCI/L2CAP/security error
    /// offset into its range).
    HostStatus(i32),
    /// A NimBLE OS-layer status (`os_error_t`) or a shim's `-1`.
    OsStatus(i32),
    /// A NimBLE port status that is either a `BLE_HS_*` host status or a
    /// `ble_npl_error_t`, which share small values.
    PortStatus(i32),
    OutOfMemory,
    InvalidLength(usize),
    OutOfRange {
        offset: usize,
        length: usize,
    },
}

/// NimBLE offsets ATT error codes into host statuses from this base
/// (`BLE_HS_ERR_ATT_BASE`). ESP builds check it against the SDK.
pub(crate) const ATT_STATUS_BASE: i32 = 0x100;

impl BackendError {
    pub(crate) fn new(operation: &'static str, detail: BackendDetail) -> Self {
        Self { operation, detail }
    }

    /// The native operation that failed, for diagnostics.
    pub fn operation(&self) -> &'static str {
        self.operation
    }

    /// The ATT error carried by a NimBLE host status, if it has one. ESP-IDF
    /// and OS-layer statuses never carry one, even when their values fall in
    /// the same numeric range. A status with an unallocated ATT code is not
    /// reported here but stays visible in `Display`.
    pub fn att_error(&self) -> Option<AttError> {
        match self.detail {
            BackendDetail::HostStatus(status)
                if (ATT_STATUS_BASE..ATT_STATUS_BASE + 0x100).contains(&status) =>
            {
                AttError::from_code((status - ATT_STATUS_BASE) as u8).ok()
            }
            _ => None,
        }
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let operation = self.operation;
        match self.detail {
            BackendDetail::EspError(status) => write!(
                formatter,
                "{operation} failed with ESP-IDF error {status} (0x{status:x})"
            ),
            BackendDetail::HostStatus(status) => {
                write!(
                    formatter,
                    "{operation} failed with NimBLE host status {status} (0x{status:x})"
                )?;
                match self.att_error() {
                    Some(att) => write!(formatter, ": {att}"),
                    None if (ATT_STATUS_BASE..ATT_STATUS_BASE + 0x100).contains(&status) => write!(
                        formatter,
                        ": unallocated ATT error 0x{:02x}",
                        status - ATT_STATUS_BASE
                    ),
                    None => Ok(()),
                }
            }
            BackendDetail::OsStatus(status) => write!(
                formatter,
                "{operation} failed with NimBLE OS status {status} (0x{status:x})"
            ),
            BackendDetail::PortStatus(status) => write!(
                formatter,
                "{operation} failed with NimBLE port status {status} (0x{status:x}; a BLE_HS_* or ble_npl_error_t value)"
            ),
            BackendDetail::OutOfMemory => write!(formatter, "{operation} could not allocate"),
            BackendDetail::InvalidLength(length) => {
                write!(formatter, "{operation} rejected a length of {length} bytes")
            }
            BackendDetail::OutOfRange { offset, length } => write!(
                formatter,
                "{operation} range {offset}+{length} is outside the buffer"
            ),
        }
    }
}

impl std::error::Error for BackendError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn every_code_is_classified_once() {
        for code in 0..=u8::MAX {
            let accepted = AttError::from_code(code);
            let sendable = matches!(code, 0x01..=0x13 | 0x80..=0x9f | 0xfc..=0xff);
            assert_eq!(accepted.is_ok(), sendable, "0x{code:02x}");
            assert_eq!(AttError::try_from(code), accepted);
            match accepted {
                Ok(error) => {
                    assert_eq!(error.code(), code);
                    assert_eq!(error.is_application(), (0x80..=0x9f).contains(&code));
                    assert!(error.to_string().contains(&format!("0x{code:02x}")));
                }
                Err(rejected) => {
                    assert_eq!(rejected.code(), code);
                    assert!(rejected.to_string().contains(&format!("0x{code:02x}")));
                }
            }
            assert_eq!(
                AttError::application(code).is_ok(),
                (0x80..=0x9f).contains(&code)
            );
        }
    }

    #[test]
    fn named_constants_carry_their_specification_codes() {
        let constants = [
            (AttError::INVALID_HANDLE, 0x01),
            (AttError::READ_NOT_PERMITTED, 0x02),
            (AttError::WRITE_NOT_PERMITTED, 0x03),
            (AttError::INVALID_PDU, 0x04),
            (AttError::INSUFFICIENT_AUTHENTICATION, 0x05),
            (AttError::REQUEST_NOT_SUPPORTED, 0x06),
            (AttError::INVALID_OFFSET, 0x07),
            (AttError::INSUFFICIENT_AUTHORIZATION, 0x08),
            (AttError::PREPARE_QUEUE_FULL, 0x09),
            (AttError::ATTRIBUTE_NOT_FOUND, 0x0a),
            (AttError::ATTRIBUTE_NOT_LONG, 0x0b),
            (AttError::INSUFFICIENT_ENCRYPTION_KEY_SIZE, 0x0c),
            (AttError::INVALID_ATTRIBUTE_VALUE_LENGTH, 0x0d),
            (AttError::UNLIKELY, 0x0e),
            (AttError::INSUFFICIENT_ENCRYPTION, 0x0f),
            (AttError::UNSUPPORTED_GROUP_TYPE, 0x10),
            (AttError::INSUFFICIENT_RESOURCES, 0x11),
            (AttError::DATABASE_OUT_OF_SYNC, 0x12),
            (AttError::VALUE_NOT_ALLOWED, 0x13),
            (AttError::WRITE_REQUEST_REJECTED, 0xfc),
            (AttError::CCCD_IMPROPERLY_CONFIGURED, 0xfd),
            (AttError::PROCEDURE_ALREADY_IN_PROGRESS, 0xfe),
            (AttError::OUT_OF_RANGE, 0xff),
        ];
        for (error, code) in constants {
            assert_eq!(error.code(), code);
            assert_eq!(AttError::from_code(code), Ok(error));
            assert!(error.name().is_some());
        }
        assert_eq!(
            AttError::application(0x9f).unwrap().to_string(),
            "ATT application error 0x9f"
        );
        for reserved in [0x00, 0x14, 0x7f, 0xa0, 0xe0, 0xfb] {
            assert_eq!(
                AttError::from_code(reserved).map_err(|error| error.code()),
                Err(reserved)
            );
        }
    }

    #[test]
    fn decode_failures_become_the_client_facing_att_error() {
        let length = AttError::INVALID_ATTRIBUTE_VALUE_LENGTH;
        let value = AttError::VALUE_NOT_ALLOWED;
        let cases = [
            (
                DecodeError::Truncated {
                    needed: 2,
                    available: 1,
                },
                length,
            ),
            (DecodeError::TrailingBytes { count: 1 }, length),
            (DecodeError::InvalidLength { length: 3 }, length),
            (DecodeError::InvalidUtf8 { valid_up_to: 0 }, value),
            (DecodeError::InvalidValue { reason: "r" }, value),
        ];
        for (decode, att) in cases {
            assert_eq!(AttError::from(decode), att);
        }
        assert_eq!(
            AttError::from(EncodeError::CapacityExceeded {
                needed: 1,
                remaining: 0
            }),
            AttError::UNLIKELY
        );

        fn handler(bytes: &[u8]) -> Result<u16, AttError> {
            Ok(crate::codec::decode_value::<u16>(bytes)?)
        }
        assert_eq!(handler(&[1, 0]), Ok(1));
        assert_eq!(handler(&[1]), Err(length));
    }

    #[test]
    fn backend_errors_stay_diagnosable_and_expose_att_causes() {
        let unknown = BackendError::new("ble_gap_terminate", BackendDetail::HostStatus(-7_654));
        assert_eq!(unknown.operation(), "ble_gap_terminate");
        assert_eq!(unknown.att_error(), None);
        assert!(unknown.to_string().contains("-7654"), "{unknown}");

        // ESP_ERR_NO_MEM (0x101) and an OS status in the same numeric range
        // are not mistaken for ATT errors.
        for detail in [
            BackendDetail::EspError(0x101),
            BackendDetail::OsStatus(0x103),
        ] {
            let error = BackendError::new("op", detail);
            assert_eq!(error.att_error(), None);
            assert!(!error.to_string().contains("ATT"), "{error}");
        }
        assert!(
            BackendError::new("nimble_port_init", BackendDetail::EspError(0x101))
                .to_string()
                .contains("ESP-IDF error 257 (0x101)")
        );

        let att = BackendError::new("ble_gatts_notify_custom", BackendDetail::HostStatus(0x10d));
        assert_eq!(
            att.att_error(),
            Some(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH)
        );
        assert!(att.to_string().contains("0x10d"), "{att}");
        assert!(att.to_string().contains("invalid attribute value length"));

        // An ATT code no server may return is not reported as an AttError.
        let reserved = BackendError::new("op", BackendDetail::HostStatus(0x150));
        assert_eq!(reserved.att_error(), None);
        assert!(reserved.to_string().contains("0x150"));
        assert!(
            reserved.to_string().ends_with("unallocated ATT error 0x50"),
            "{reserved}"
        );
        let port = BackendError::new("nimble_port_stop", BackendDetail::PortStatus(3));
        assert_eq!(port.att_error(), None);
        assert!(port.to_string().contains("ble_npl_error_t"), "{port}");
        for status in [0, 6, 0xff, 0x200, 0x20d] {
            assert_eq!(
                BackendError::new("op", BackendDetail::HostStatus(status)).att_error(),
                None,
                "0x{status:x}"
            );
        }

        let error = Error::from(att);
        assert_eq!(error.kind(), ErrorKind::Backend);
        assert_eq!(error.backend(), Some(&att));
        assert!(error.to_string().starts_with("native backend error: "));
        let source = error.source().expect("backend errors keep their cause");
        assert_eq!(source.downcast_ref::<BackendError>(), Some(&att));

        for detail in [
            BackendDetail::OutOfMemory,
            BackendDetail::InvalidLength(70_000),
            BackendDetail::OutOfRange {
                offset: 4,
                length: 9,
            },
        ] {
            let error = BackendError::new("mbuf", detail);
            assert!(error.to_string().starts_with("mbuf "), "{error}");
            assert_eq!(error.att_error(), None);
        }
    }

    #[test]
    fn framework_errors_carry_kind_and_cause() {
        let encode = EncodeError::CapacityExceeded {
            needed: 3,
            remaining: 1,
        };
        let error = Error::from(encode);
        assert_eq!(error.kind(), ErrorKind::Encode);
        assert_eq!(error.backend(), None);
        assert_eq!(
            error.source().and_then(|s| s.downcast_ref::<EncodeError>()),
            Some(&encode)
        );

        let lifecycle = Error::new(ErrorKind::Lifecycle, None, "the host is already owned");
        assert_eq!(lifecycle.kind(), ErrorKind::Lifecycle);
        assert!(lifecycle.source().is_none());
        assert_eq!(
            lifecycle.to_string(),
            "lifecycle error: the host is already owned"
        );
        assert_eq!(
            Error::new(ErrorKind::Lifecycle, Some("start"), "x").to_string(),
            "lifecycle error: start: x"
        );
    }
}

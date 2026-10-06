//! Internal boundary between framework logic and the native NimBLE host.
//!
//! Framework code calls native operations only through [`Backend`]. The ESP-IDF
//! implementation wraps the private generated bindings; the test-only fake
//! records the same operations on the host. Both deliver native callbacks
//! through the shared [`EventDispatcher`](super::dispatch::EventDispatcher), so
//! host tests exercise the dispatch and ownership logic that production uses.
//!
//! The trait grows with each feature ticket (GATT registration, advertising
//! parameters, connection state). It is crate-private and is not a public
//! backend-implementation API.

use super::dispatch::EventDispatcher;
use super::gap::GapEvent;
use crate::error::{BackendDetail, BackendError, Error, ErrorKind};
use crate::gatt::registration::GattPlan;
use std::ffi::CStr;
use std::fmt;
use std::sync::Arc;

/// Native operation identity used in errors, scripted fake results, and call
/// ordering assertions.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum Operation {
    HostInit,
    HostDeinit,
    InstallCallbacks,
    RemoveCallbacks,
    HostStart,
    HostStop,
    MbufFromFlat,
    MbufLen,
    MbufAppend,
    MbufCopy,
    MbufFree,
    Notify,
    Terminate,
    AdvertisingStop,
    Mtu,
    InferAddress,
    GattCount,
    GattAdd,
    DeviceName,
    AdvertisingData,
    ScanResponseData,
    AdvertisingStart,
}

impl Operation {
    /// The SDK function or step, as reported in public error messages.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::HostInit => "nimble_port_init",
            Self::HostDeinit => "nimble_port_deinit",
            Self::InstallCallbacks => "install host callbacks",
            Self::RemoveCallbacks => "remove host callbacks",
            Self::HostStart => "nimble_port_freertos_init",
            Self::HostStop => "nimble_port_stop",
            Self::MbufFromFlat => "ble_hs_mbuf_from_flat",
            Self::MbufLen => "os_mbuf_len",
            Self::MbufAppend => "os_mbuf_append",
            Self::MbufCopy => "os_mbuf_copydata",
            Self::MbufFree => "os_mbuf_free_chain",
            Self::Notify => "ble_gatts_notify_custom",
            Self::Terminate => "ble_gap_terminate",
            Self::AdvertisingStop => "ble_gap_adv_stop",
            Self::Mtu => "ble_att_mtu",
            Self::InferAddress => "ble_hs_id_infer_auto",
            Self::GattCount => "ble_gatts_count_cfg",
            Self::GattAdd => "ble_gatts_add_svcs",
            Self::DeviceName => "ble_svc_gap_device_name_set",
            Self::AdvertisingData => "ble_gap_adv_set_data",
            Self::ScanResponseData => "ble_gap_adv_rsp_set_data",
            Self::AdvertisingStart => "ble_gap_adv_start",
        }
    }

    /// Interpret a nonzero status in the family this operation returns.
    fn status_detail(self, code: i32) -> BackendDetail {
        match self {
            // `nimble_port_init` and `nimble_port_deinit` return `esp_err_t`.
            Self::HostInit | Self::HostDeinit => BackendDetail::EspError(code),
            // The mbuf wrappers return `os_error_t` values or the shim's -1.
            Self::MbufFromFlat
            | Self::MbufLen
            | Self::MbufAppend
            | Self::MbufCopy
            | Self::MbufFree => BackendDetail::OsStatus(code),
            // `nimble_port_stop` returns a `ble_npl_error_t` if its semaphore
            // cannot be created and a host status if the host cannot stop.
            Self::HostStop => BackendDetail::PortStatus(code),
            Self::InstallCallbacks
            | Self::RemoveCallbacks
            | Self::HostStart
            | Self::Notify
            | Self::Terminate
            | Self::AdvertisingStop
            | Self::Mtu
            | Self::InferAddress
            | Self::GattCount
            | Self::GattAdd
            | Self::DeviceName
            | Self::AdvertisingData
            | Self::ScanResponseData
            | Self::AdvertisingStart => BackendDetail::HostStatus(code),
        }
    }
}

/// A failed native operation. Status codes are the SDK's own values; they are
/// reported, not reinterpreted, at this boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeError {
    /// The SDK returned a nonzero status.
    Status { operation: Operation, code: i32 },
    /// The SDK could not allocate a buffer.
    OutOfMemory { operation: Operation },
    /// A length does not fit the SDK's parameter type.
    InvalidLength { operation: Operation, length: usize },
    /// A copy range is outside the buffer.
    OutOfRange {
        operation: Operation,
        offset: usize,
        length: usize,
    },
    /// Callbacks are already installed, so another owner holds the host.
    Busy { operation: Operation },
    /// The call was made from inside a native callback delivery, where it
    /// would have to wait for itself. Nothing was changed.
    Reentrant { operation: Operation },
}

impl NativeError {
    pub(crate) fn operation(&self) -> Operation {
        match *self {
            Self::Status { operation, .. }
            | Self::OutOfMemory { operation }
            | Self::InvalidLength { operation, .. }
            | Self::OutOfRange { operation, .. }
            | Self::Busy { operation }
            | Self::Reentrant { operation } => operation,
        }
    }
}

impl fmt::Display for NativeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Status { operation, code } => {
                write!(formatter, "{operation:?} failed with native status {code}")
            }
            Self::OutOfMemory { operation } => {
                write!(
                    formatter,
                    "{operation:?} could not allocate a native buffer"
                )
            }
            Self::InvalidLength { operation, length } => write!(
                formatter,
                "{operation:?} length {length} exceeds the native limit"
            ),
            Self::OutOfRange {
                operation,
                offset,
                length,
            } => write!(
                formatter,
                "{operation:?} range at offset {offset} with length {length} is outside the buffer"
            ),
            Self::Reentrant { operation } => write!(
                formatter,
                "{operation:?} cannot run from inside a native callback"
            ),
            Self::Busy { operation } => {
                write!(
                    formatter,
                    "{operation:?} found native callbacks already installed"
                )
            }
        }
    }
}

impl std::error::Error for NativeError {}

/// Ownership conflicts become lifecycle errors; every other native failure
/// becomes a [`BackendError`] that keeps the operation and SDK status.
impl From<NativeError> for Error {
    fn from(error: NativeError) -> Self {
        let operation = error.operation().name();
        let detail = match error {
            NativeError::Status { operation, code } => operation.status_detail(code),
            NativeError::OutOfMemory { .. } => BackendDetail::OutOfMemory,
            NativeError::InvalidLength { length, .. } => BackendDetail::InvalidLength(length),
            NativeError::OutOfRange { offset, length, .. } => {
                BackendDetail::OutOfRange { offset, length }
            }
            NativeError::Busy { .. } => {
                return Error::new(
                    ErrorKind::Lifecycle,
                    Some(operation),
                    "the native host callbacks already have an owner",
                )
            }
            NativeError::Reentrant { .. } => {
                return Error::new(
                    ErrorKind::Lifecycle,
                    Some(operation),
                    "the operation cannot run from inside a BLE callback",
                )
            }
        };
        BackendError::new(operation, detail).into()
    }
}

pub(crate) type NativeResult<T> = Result<T, NativeError>;

/// Map an SDK status (0 for success, for both NimBLE and `esp_err_t`).
pub(crate) fn check(operation: Operation, code: i32) -> NativeResult<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(NativeError::Status { operation, code })
    }
}

/// Convert a Rust length to the SDK's `u16` length parameter.
pub(crate) fn native_length(operation: Operation, length: usize) -> NativeResult<u16> {
    u16::try_from(length).map_err(|_| NativeError::InvalidLength { operation, length })
}

/// Native callbacks after translation into owned Rust values. Pointers from
/// the SDK never cross this boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NativeEvent {
    /// The host and controller are synchronized.
    HostSynced,
    /// The host reset; the reason is the SDK's status code.
    HostReset { reason: i32 },
    /// A supported GAP event copied out of the borrowed SDK event.
    Gap(GapEvent),
}

/// Native NimBLE operations used by framework logic.
///
/// Implementations must not call back into framework code synchronously except
/// through the dispatcher installed with [`Backend::install_callbacks`].
/// Clones refer to the same native host.
pub(crate) trait Backend: Clone + Send + Sync + 'static {
    /// The most links NimBLE holds at once (`CONFIG_BT_NIMBLE_MAX_CONNECTIONS`
    /// on ESP builds).
    const MAX_LINKS: usize;
    /// An owned native buffer handle. It is not `Clone`: each handle is freed
    /// or transferred exactly once.
    type Mbuf;
    /// Native GATT tables built from a plan. They must stay alive, unmoved,
    /// until the host is deinitialized, because NimBLE keeps pointers to
    /// them, including after a failed registration.
    type Registration: Send + Sync;

    /// Initialize the NimBLE port and controller.
    fn host_init(&self) -> NativeResult<()>;
    /// Release the NimBLE port after the host task has stopped.
    fn host_deinit(&self) -> NativeResult<()>;
    /// Route host sync/reset callbacks to `dispatcher`. Fails with
    /// [`NativeError::Busy`] if callbacks are already installed.
    fn install_callbacks(&self, dispatcher: Arc<EventDispatcher>) -> NativeResult<()>;
    /// Clear native callbacks and wait for in-flight deliveries to finish.
    /// Fails with [`NativeError::Reentrant`], changing nothing, when called
    /// from inside a delivery.
    fn remove_callbacks(&self) -> NativeResult<()>;
    /// Start the host task.
    fn host_start(&self) -> NativeResult<()>;
    /// Ask the host task to stop.
    fn host_stop(&self) -> NativeResult<()>;

    /// Allocate a buffer containing `data`.
    fn mbuf_from_flat(&self, data: &[u8]) -> NativeResult<Self::Mbuf>;
    /// Length of the full buffer chain.
    fn mbuf_len(&self, mbuf: &Self::Mbuf) -> usize;
    /// Append `data` to the chain. On failure the buffer remains owned, but
    /// NimBLE does not roll back: the chain may already hold a prefix of
    /// `data` and report the longer length.
    fn mbuf_append(&self, mbuf: &mut Self::Mbuf, data: &[u8]) -> NativeResult<()>;
    /// Copy `destination.len()` bytes starting at `offset`.
    fn mbuf_copy(
        &self,
        mbuf: &Self::Mbuf,
        offset: usize,
        destination: &mut [u8],
    ) -> NativeResult<()>;
    /// Free the whole chain.
    fn mbuf_free(&self, mbuf: Self::Mbuf) -> NativeResult<()>;

    /// Send a notification. The SDK takes ownership of `mbuf` whether or not
    /// the call succeeds. Before returning, NimBLE reports the result through
    /// the connection's GAP callback on the calling thread, so a
    /// [`GapEvent::NotifyTransmit`] can be delivered synchronously from inside
    /// this call. Callers must not hold locks that their event sink takes.
    fn notify(&self, connection: u16, attribute: u16, mbuf: Self::Mbuf) -> NativeResult<()>;
    /// Terminate a connection as a remote-user termination. The result
    /// arrives later as a [`GapEvent::Disconnect`]; nothing is delivered
    /// from inside this call. A link the host no longer knows fails with
    /// `BLE_HS_ENOTCONN`, one the controller no longer knows with the HCI
    /// Unknown Connection Identifier status, and one already being
    /// terminated with `BLE_HS_EALREADY` (`ble_gap_terminate_with_conn`).
    /// The ESP backend reports that last case as success.
    fn terminate(&self, connection: u16) -> NativeResult<()>;
    /// Set the GAP Device Name characteristic's value. Call after host
    /// initialization; NimBLE copies the name.
    fn set_device_name(&self, name: &CStr) -> NativeResult<()>;
    /// Set the legacy advertising data, at most 31 bytes; NimBLE copies it to
    /// the controller.
    fn set_advertising_data(&self, data: &[u8]) -> NativeResult<()>;
    /// Set the legacy scan response data, at most 31 bytes; NimBLE copies it
    /// to the controller.
    fn set_scan_response_data(&self, data: &[u8]) -> NativeResult<()>;
    /// Start connectable, generally discoverable legacy advertising without
    /// a time limit, with NimBLE's default intervals. Its GAP events, and
    /// those of a connection it accepts, reach the installed dispatcher.
    /// Returns whether a new advertising procedure started: starting while
    /// one is already active succeeds without starting another (NimBLE's
    /// `BLE_HS_EALREADY`). Nothing is delivered from inside this call.
    fn advertising_start(&self, address_type: u8) -> NativeResult<bool>;
    /// Stop advertising, returning whether an advertising procedure was
    /// active; stopping when none is succeeds (`BLE_HS_EALREADY`). Nothing
    /// is delivered from inside this call.
    fn advertising_stop(&self) -> NativeResult<bool>;
    /// Whether the host is synchronized with the controller now. It turns
    /// false at the start of a host reset, before the reset's GAP events and
    /// reset callback are delivered.
    fn is_synced(&self) -> bool;
    /// ATT MTU for a connection, or `None` when the SDK reports no connection.
    fn mtu(&self, connection: u16) -> Option<u16>;
    /// The own-address type to advertise with, without privacy. Valid only
    /// after the host has synchronized.
    fn infer_address_type(&self) -> NativeResult<u8>;
    /// Whether the caller is running on the native host task. Operations that
    /// wait for the host task, such as stopping it, would wait for themselves
    /// there; this covers every native callback, not only dispatched events.
    fn is_host_task(&self) -> bool;
    /// Build native tables for `plan` without any native call.
    fn prepare_gatt(&self, plan: &GattPlan) -> Self::Registration;
    /// Count and add the prepared services. Call after host
    /// initialization and before the host starts.
    fn register_gatt(&self, registration: &Self::Registration) -> NativeResult<()>;
    /// Characteristic value handles in plan order, as assigned when the host
    /// started; zero before then.
    fn value_handles(&self, registration: &Self::Registration) -> Vec<u16>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_and_lengths_map_to_typed_errors() {
        assert_eq!(check(Operation::HostInit, 0), Ok(()));
        let status = check(Operation::HostInit, 0x103).unwrap_err();
        assert_eq!(status.operation(), Operation::HostInit);
        assert_eq!(status.to_string(), "HostInit failed with native status 259");
        assert_eq!(native_length(Operation::MbufAppend, 65_535), Ok(u16::MAX));
        let length = native_length(Operation::MbufAppend, 65_536).unwrap_err();
        assert_eq!(
            length,
            NativeError::InvalidLength {
                operation: Operation::MbufAppend,
                length: 65_536
            }
        );
        assert_eq!(
            length.to_string(),
            "MbufAppend length 65536 exceeds the native limit"
        );
        for error in [
            NativeError::OutOfMemory {
                operation: Operation::MbufFromFlat,
            },
            NativeError::OutOfRange {
                operation: Operation::MbufCopy,
                offset: 1,
                length: 2,
            },
            NativeError::Reentrant {
                operation: Operation::RemoveCallbacks,
            },
            NativeError::Busy {
                operation: Operation::InstallCallbacks,
            },
        ] {
            assert!(error
                .to_string()
                .starts_with(&format!("{:?}", error.operation())));
        }
    }

    #[test]
    fn native_errors_convert_to_framework_errors_with_their_causes() {
        use crate::AttError;
        use std::error::Error as _;

        let unknown = Error::from(NativeError::Status {
            operation: Operation::Terminate,
            code: -31_337,
        });
        assert_eq!(unknown.kind(), ErrorKind::Backend);
        let backend = unknown.backend().expect("a backend cause");
        assert_eq!(backend.operation(), "ble_gap_terminate");
        assert_eq!(backend.att_error(), None);
        assert!(unknown.to_string().contains("ble_gap_terminate"));
        assert!(unknown.to_string().contains("-31337"));
        assert!(unknown.source().is_some());

        // ESP_ERR_NO_MEM from the port and an OS status in the ATT numeric
        // range keep their own meaning.
        let esp = Error::from(NativeError::Status {
            operation: Operation::HostInit,
            code: 0x101,
        });
        assert_eq!(esp.backend().and_then(BackendError::att_error), None);
        assert!(
            esp.to_string().contains("ESP-IDF error 257 (0x101)"),
            "{esp}"
        );
        let os = Error::from(NativeError::Status {
            operation: Operation::MbufAppend,
            code: 0x101,
        });
        assert_eq!(os.backend().and_then(BackendError::att_error), None);
        assert!(os.to_string().contains("OS status"), "{os}");
        let stop = Error::from(NativeError::Status {
            operation: Operation::HostStop,
            code: 3,
        });
        assert!(stop.to_string().contains("port status 3"), "{stop}");

        let att = Error::from(NativeError::Status {
            operation: Operation::Notify,
            code: 0x111,
        });
        assert_eq!(
            att.backend().and_then(BackendError::att_error),
            Some(AttError::INSUFFICIENT_RESOURCES)
        );

        for (error, name) in [
            (
                NativeError::OutOfMemory {
                    operation: Operation::MbufFromFlat,
                },
                "ble_hs_mbuf_from_flat",
            ),
            (
                NativeError::InvalidLength {
                    operation: Operation::MbufAppend,
                    length: 70_000,
                },
                "os_mbuf_append",
            ),
            (
                NativeError::OutOfRange {
                    operation: Operation::MbufCopy,
                    offset: 3,
                    length: 9,
                },
                "os_mbuf_copydata",
            ),
        ] {
            let converted = Error::from(error);
            assert_eq!(converted.kind(), ErrorKind::Backend);
            assert_eq!(converted.backend().map(BackendError::operation), Some(name));
        }

        for (error, name) in [
            (
                NativeError::Busy {
                    operation: Operation::InstallCallbacks,
                },
                "install host callbacks",
            ),
            (
                NativeError::Reentrant {
                    operation: Operation::RemoveCallbacks,
                },
                "remove host callbacks",
            ),
        ] {
            let converted = Error::from(error);
            assert_eq!(converted.kind(), ErrorKind::Lifecycle);
            assert!(converted.backend().is_none());
            assert!(converted.to_string().contains(name), "{converted}");
        }
    }

    #[test]
    fn every_operation_has_a_distinct_name() {
        let operations = [
            Operation::HostInit,
            Operation::HostDeinit,
            Operation::InstallCallbacks,
            Operation::RemoveCallbacks,
            Operation::HostStart,
            Operation::HostStop,
            Operation::MbufFromFlat,
            Operation::MbufLen,
            Operation::MbufAppend,
            Operation::MbufCopy,
            Operation::MbufFree,
            Operation::Notify,
            Operation::Terminate,
            Operation::AdvertisingStop,
            Operation::Mtu,
            Operation::InferAddress,
            Operation::GattCount,
            Operation::GattAdd,
            Operation::DeviceName,
            Operation::AdvertisingData,
            Operation::ScanResponseData,
            Operation::AdvertisingStart,
        ];
        let names: std::collections::BTreeSet<_> = operations
            .iter()
            .map(|operation| operation.name())
            .collect();
        assert_eq!(names.len(), operations.len());
    }
}

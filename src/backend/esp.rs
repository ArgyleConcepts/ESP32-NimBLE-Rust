//! ESP-IDF implementation of the native backend boundary.
//!
//! This is the only module that calls the private generated bindings. The C
//! callback trampolines copy borrowed SDK data into owned values and hand them
//! to the shared [`EventDispatcher`]; they contain no framework logic. They are
//! `extern "C"` functions, so a panic aborts instead of unwinding into C (ESP
//! targets also build with `panic=abort`).

use super::bindings;
use super::dispatch::{CallbackSlot, EventDispatcher};
use super::gap::{GapCodes, GapEvent, GapEventView};
use super::native::{
    check, native_length, Backend, NativeError, NativeEvent, NativeResult, Operation,
};
use crate::ble::advertising::{ad, AdvertisingFields, LEGACY_PAYLOAD_CAPACITY};
use crate::ble::connection::{
    ATT_CHANNEL, ATT_DEFAULT_MTU, HCI_STATUS_BASE, HOST_EAGAIN, HOST_ENOTCONN,
};
use crate::error::ATT_STATUS_BASE;
use crate::{AttError, Uuid};
use std::ffi::{c_int, c_void, CStr};
use std::ptr::{null, null_mut, NonNull};
use std::sync::Arc;

/// SDK GAP codes from the consumer's generated bindings.
pub(crate) const GAP_CODES: GapCodes = GapCodes {
    connect: bindings::BLE_GAP_EVENT_CONNECT,
    disconnect: bindings::BLE_GAP_EVENT_DISCONNECT,
    connection_update: bindings::BLE_GAP_EVENT_CONN_UPDATE,
    advertising_complete: bindings::BLE_GAP_EVENT_ADV_COMPLETE,
    notify_transmit: bindings::BLE_GAP_EVENT_NOTIFY_TX,
    subscribe: bindings::BLE_GAP_EVENT_SUBSCRIBE,
    mtu: bindings::BLE_GAP_EVENT_MTU,
    subscribe_write: bindings::BLE_GAP_SUBSCRIBE_REASON_WRITE,
    subscribe_terminated: bindings::BLE_GAP_SUBSCRIBE_REASON_TERM,
    subscribe_restore: bindings::BLE_GAP_SUBSCRIBE_REASON_RESTORE,
};

// The public ATT error codes are the Bluetooth specification's values; check
// that the consumer's NimBLE uses the same ones, so handler results pass
// through unchanged. A mismatch fails compilation.
macro_rules! assert_att_codes {
    ($($public:ident = $native:ident),* $(,)?) => {$(
        const _: () = assert!(AttError::$public.code() as u32 == bindings::$native as u32);
    )*};
}

assert_att_codes!(
    INVALID_HANDLE = BLE_ATT_ERR_INVALID_HANDLE,
    READ_NOT_PERMITTED = BLE_ATT_ERR_READ_NOT_PERMITTED,
    WRITE_NOT_PERMITTED = BLE_ATT_ERR_WRITE_NOT_PERMITTED,
    INVALID_PDU = BLE_ATT_ERR_INVALID_PDU,
    INSUFFICIENT_AUTHENTICATION = BLE_ATT_ERR_INSUFFICIENT_AUTHEN,
    REQUEST_NOT_SUPPORTED = BLE_ATT_ERR_REQ_NOT_SUPPORTED,
    INVALID_OFFSET = BLE_ATT_ERR_INVALID_OFFSET,
    INSUFFICIENT_AUTHORIZATION = BLE_ATT_ERR_INSUFFICIENT_AUTHOR,
    PREPARE_QUEUE_FULL = BLE_ATT_ERR_PREPARE_QUEUE_FULL,
    ATTRIBUTE_NOT_FOUND = BLE_ATT_ERR_ATTR_NOT_FOUND,
    ATTRIBUTE_NOT_LONG = BLE_ATT_ERR_ATTR_NOT_LONG,
    INSUFFICIENT_ENCRYPTION_KEY_SIZE = BLE_ATT_ERR_INSUFFICIENT_KEY_SZ,
    INVALID_ATTRIBUTE_VALUE_LENGTH = BLE_ATT_ERR_INVALID_ATTR_VALUE_LEN,
    UNLIKELY = BLE_ATT_ERR_UNLIKELY,
    INSUFFICIENT_ENCRYPTION = BLE_ATT_ERR_INSUFFICIENT_ENC,
    UNSUPPORTED_GROUP_TYPE = BLE_ATT_ERR_UNSUPPORTED_GROUP,
    INSUFFICIENT_RESOURCES = BLE_ATT_ERR_INSUFFICIENT_RES,
    DATABASE_OUT_OF_SYNC = BLE_ATT_ERR_DB_OUT_OF_SYNC,
    VALUE_NOT_ALLOWED = BLE_ATT_ERR_VALUE_NOT_ALLOWED,
);

const _: () = assert!(ATT_STATUS_BASE as u32 == bindings::BLE_HS_ERR_ATT_BASE as u32);

// The advertising payloads are encoded in platform-neutral code from the
// Bluetooth Assigned Numbers and Core Specification; check those values
// against the consumer's NimBLE. A mismatch fails compilation.
const _: () = {
    assert!(ad::FLAGS as u32 == bindings::BLE_HS_ADV_TYPE_FLAGS as u32);
    assert!(ad::INCOMPLETE_UUIDS16 as u32 == bindings::BLE_HS_ADV_TYPE_INCOMP_UUIDS16 as u32);
    assert!(ad::COMPLETE_UUIDS16 as u32 == bindings::BLE_HS_ADV_TYPE_COMP_UUIDS16 as u32);
    assert!(ad::INCOMPLETE_UUIDS128 as u32 == bindings::BLE_HS_ADV_TYPE_INCOMP_UUIDS128 as u32);
    assert!(ad::COMPLETE_UUIDS128 as u32 == bindings::BLE_HS_ADV_TYPE_COMP_UUIDS128 as u32);
    assert!(ad::SHORTENED_NAME as u32 == bindings::BLE_HS_ADV_TYPE_INCOMP_NAME as u32);
    assert!(ad::COMPLETE_NAME as u32 == bindings::BLE_HS_ADV_TYPE_COMP_NAME as u32);
    assert!(ad::GENERAL_DISCOVERABLE as u32 == bindings::BLE_HS_ADV_F_DISC_GEN as u32);
    assert!(ad::BREDR_UNSUPPORTED as u32 == bindings::BLE_HS_ADV_F_BREDR_UNSUP as u32);
    assert!(LEGACY_PAYLOAD_CAPACITY as u32 == bindings::BLE_HCI_MAX_ADV_DATA_LEN as u32);
    assert!(LEGACY_PAYLOAD_CAPACITY as u32 == bindings::BLE_HCI_MAX_SCAN_RSP_DATA_LEN as u32);
    assert!(ATT_DEFAULT_MTU as u32 == bindings::BLE_ATT_MTU_DFLT as u32);
    assert!(ATT_CHANNEL as u32 == bindings::BLE_L2CAP_CID_ATT as u32);
    assert!(HCI_STATUS_BASE as u32 == bindings::BLE_HS_ERR_HCI_BASE as u32);
    assert!(HOST_EAGAIN as u32 == bindings::BLE_HS_EAGAIN as u32);
    assert!(HOST_ENOTCONN as u32 == bindings::BLE_HS_ENOTCONN as u32);
    // `ble_gap_adv_start` takes the duration as an `int32_t`.
    assert!(bindings::ARGYLE_NIMBLE_HS_FOREVER as i64 == i32::MAX as i64);
};

// One client is served, but NimBLE must be able to hold a second link: in
// ESP-IDF 6.1 a peripheral connection that fails before it is reported
// (`ble_gap_conn_broken` in `ble_gap.c`) delivers its failed-connection event
// before freeing the link, and a failed feature exchange leaves the link
// open until it is terminated. Restarting advertising from that event needs
// a free connection slot (`ble_hs_conn_can_alloc` in `ble_hs_conn.c`), and
// no later event would retry it.
const _: () = assert!(
    bindings::CONFIG_BT_NIMBLE_MAX_CONNECTIONS >= 2,
    "argyle-nimble requires CONFIG_BT_NIMBLE_MAX_CONNECTIONS of at least 2 (ESP-IDF's default is 3)"
);

/// NimBLE's "already in that state" status, which advertising start and stop
/// report when there is nothing to change.
const ALREADY: c_int = bindings::BLE_HS_EALREADY as c_int;

/// Copy a payload into a full-size buffer, so the SDK always receives a
/// valid pointer, even for an empty payload.
fn payload(
    operation: Operation,
    data: &[u8],
) -> NativeResult<([u8; LEGACY_PAYLOAD_CAPACITY], c_int)> {
    if data.len() > LEGACY_PAYLOAD_CAPACITY {
        return Err(NativeError::InvalidLength {
            operation,
            length: data.len(),
        });
    }
    let mut buffer = [0; LEGACY_PAYLOAD_CAPACITY];
    buffer[..data.len()].copy_from_slice(data);
    Ok((buffer, data.len() as c_int))
}

/// The dispatcher receiving host callbacks. NimBLE's sync and reset callbacks
/// carry no user argument, so the single installed dispatcher is kept here.
static CALLBACKS: CallbackSlot = CallbackSlot::new();

fn deliver(event: NativeEvent) {
    let _ = CALLBACKS.deliver(event);
}

extern "C" fn on_host_sync() {
    deliver(NativeEvent::HostSynced);
}

extern "C" fn on_host_reset(reason: c_int) {
    deliver(NativeEvent::HostReset { reason });
}

/// GAP callback for advertising and connections. The event pointer is only
/// borrowed for this call; the shim copies the supported fields first.
extern "C" fn on_gap_event(event: *mut bindings::ble_gap_event, _argument: *mut c_void) -> c_int {
    let mut view = bindings::argyle_nimble_gap_event_view {
        type_: 0,
        subscribe_reason: 0,
        notify_enabled: 0,
        indicate_enabled: 0,
        conn_handle: 0,
        channel_id: 0,
        attr_handle: 0,
        mtu: 0,
        status: 0,
        reason: 0,
        indication: 0,
    };
    // SAFETY: NimBLE passes a valid event for the duration of this callback;
    // the shim rejects null pointers and writes only the fixed view.
    if unsafe { bindings::argyle_nimble_gap_event_extract(event, &mut view) } != 0 {
        return 0;
    }
    let view = GapEventView {
        kind: u32::from(view.type_),
        subscribe_reason: u32::from(view.subscribe_reason),
        notify_enabled: view.notify_enabled != 0,
        indicate_enabled: view.indicate_enabled != 0,
        connection: view.conn_handle,
        channel: view.channel_id,
        attribute: view.attr_handle,
        mtu: view.mtu,
        status: view.status,
        reason: view.reason,
        indication: view.indication != 0,
    };
    if let Some(event) = GapEvent::from_view(&view, &GAP_CODES) {
        deliver(NativeEvent::Gap(event));
    }
    0
}

/// The GAP callback passed with advertising requests; connections accepted
/// by that advertising inherit it. Its argument is unused (null).
const GAP_EVENT_CALLBACK: bindings::ble_gap_event_fn = Some(on_gap_event);

std::thread_local! {
    /// Set on the NimBLE host task, where every native callback runs.
    static ON_HOST_TASK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

extern "C" fn host_task(_argument: *mut c_void) {
    ON_HOST_TASK.with(|flag| flag.set(true));
    // SAFETY: called on the task NimBLE created for the host; `nimble_port_run`
    // returns after `nimble_port_stop`, and the task must then delete itself.
    unsafe {
        bindings::nimble_port_run();
        bindings::nimble_port_freertos_deinit();
    }
}

/// An owned native buffer chain.
pub(crate) struct EspMbuf(pub(super) NonNull<bindings::os_mbuf>);

/// Native advertising fields: the UUID arrays `ble_hs_adv_fields` points to.
///
/// `ble_gap_adv_set_fields` copies the fields structure, including these
/// pointers, into `ble_adv_reattempt.fields` (`ble_gap.c`), and ESP-IDF's
/// connection re-attempt encodes from that copy later, without any event. The
/// arrays are therefore owned by the connection runtime, never changed after
/// they are built (so their heap buffers never move), and dropped only after
/// the host is deinitialized (or never, if the host is poisoned). The next
/// start of the host replaces NimBLE's copy before it advertises, so a stale
/// copy cannot be used.
pub(crate) struct EspAdvertisingFields {
    flags: u8,
    uuids16: Vec<bindings::ble_uuid16_t>,
    uuids16_complete: bool,
    uuids128: Vec<bindings::ble_uuid128_t>,
    uuids128_complete: bool,
}

fn native_uuid128(value: u128) -> bindings::ble_uuid128_t {
    let bytes = Uuid::Uuid128(value).to_wire_bytes();
    // SAFETY: `ble_uuid128_t` is plain data; zero is a valid value.
    let mut native: bindings::ble_uuid128_t = unsafe { std::mem::zeroed() };
    // SAFETY: `bytes` holds 16 readable bytes in wire order and `native` is
    // a separate writable UUID; the shim fails only for null pointers.
    let status = unsafe { bindings::argyle_nimble_uuid128(bytes.as_ref().as_ptr(), &mut native) };
    debug_assert_eq!(status, 0);
    native
}

/// The ESP-IDF NimBLE host. There is one native host per firmware; the
/// [`Ble`](crate::Ble) owner controls its lifecycle.
#[derive(Clone)]
pub(crate) struct EspBackend;

impl Backend for EspBackend {
    type Mbuf = EspMbuf;
    type Registration = super::esp_gatt::EspRegistration;
    type AdvertisingFields = EspAdvertisingFields;

    fn prepare_gatt(&self, plan: &crate::gatt::registration::GattPlan) -> Self::Registration {
        super::esp_gatt::prepare(plan)
    }

    fn register_gatt(&self, registration: &Self::Registration) -> NativeResult<()> {
        super::esp_gatt::register(registration)
    }

    fn value_handles(&self, registration: &Self::Registration) -> Vec<u16> {
        super::esp_gatt::value_handles(registration)
    }

    fn host_init(&self) -> NativeResult<()> {
        // SAFETY: plain SDK call with no arguments.
        check(Operation::HostInit, unsafe { bindings::nimble_port_init() })?;
        // SAFETY: the standard GAP/GATT services are registered after
        // `nimble_port_init` and before the host task starts.
        unsafe {
            bindings::ble_svc_gap_init();
            bindings::ble_svc_gatt_init();
        }
        Ok(())
    }

    fn host_deinit(&self) -> NativeResult<()> {
        // SAFETY: plain SDK call; the caller has stopped the host task.
        check(Operation::HostDeinit, unsafe {
            bindings::nimble_port_deinit()
        })
    }

    fn install_callbacks(&self, dispatcher: Arc<EventDispatcher>) -> NativeResult<()> {
        CALLBACKS
            .install(dispatcher)
            .map_err(|_| NativeError::Busy {
                operation: Operation::InstallCallbacks,
            })?;
        // SAFETY: the shims store these function pointers in `ble_hs_cfg`;
        // they are `'static` and remain valid for the program's lifetime.
        unsafe {
            bindings::argyle_nimble_set_sync_callback(Some(on_host_sync));
            bindings::argyle_nimble_set_reset_callback(Some(on_host_reset));
        }
        Ok(())
    }

    fn remove_callbacks(&self) -> NativeResult<()> {
        CALLBACKS
            .remove(|| {
                // SAFETY: clearing the callbacks stops new native deliveries;
                // the shims only assign fields in `ble_hs_cfg`.
                unsafe {
                    bindings::argyle_nimble_set_sync_callback(None);
                    bindings::argyle_nimble_set_reset_callback(None);
                }
            })
            .map_err(|_| NativeError::Reentrant {
                operation: Operation::RemoveCallbacks,
            })
    }

    fn host_start(&self) -> NativeResult<()> {
        // SAFETY: `host_task` is a valid `'static` task entry point.
        unsafe { bindings::nimble_port_freertos_init(Some(host_task)) };
        Ok(())
    }

    fn host_stop(&self) -> NativeResult<()> {
        // SAFETY: plain SDK call; it makes `nimble_port_run` return.
        check(Operation::HostStop, unsafe { bindings::nimble_port_stop() })
    }

    fn mbuf_from_flat(&self, data: &[u8]) -> NativeResult<EspMbuf> {
        let length = native_length(Operation::MbufFromFlat, data.len())?;
        // SAFETY: `data` is readable for `length` bytes; the SDK copies it.
        let raw = unsafe { bindings::ble_hs_mbuf_from_flat(data.as_ptr().cast(), length) };
        NonNull::new(raw)
            .map(EspMbuf)
            .ok_or(NativeError::OutOfMemory {
                operation: Operation::MbufFromFlat,
            })
    }

    fn mbuf_len(&self, mbuf: &EspMbuf) -> usize {
        // SAFETY: `mbuf` is a live chain owned by the caller.
        usize::from(unsafe { bindings::argyle_nimble_mbuf_len(mbuf.0.as_ptr()) })
    }

    fn mbuf_append(&self, mbuf: &mut EspMbuf, data: &[u8]) -> NativeResult<()> {
        let length = native_length(Operation::MbufAppend, data.len())?;
        // SAFETY: `mbuf` is a live chain owned by the caller and `data` is
        // readable for `length` bytes.
        let code = unsafe {
            bindings::argyle_nimble_mbuf_append(mbuf.0.as_ptr(), data.as_ptr().cast(), length)
        };
        check(Operation::MbufAppend, code)
    }

    fn mbuf_copy(&self, mbuf: &EspMbuf, offset: usize, destination: &mut [u8]) -> NativeResult<()> {
        let out_of_range = NativeError::OutOfRange {
            operation: Operation::MbufCopy,
            offset,
            length: destination.len(),
        };
        let native_offset = c_int::try_from(offset).map_err(|_| out_of_range)?;
        let native_length = c_int::try_from(destination.len()).map_err(|_| out_of_range)?;
        // SAFETY: `mbuf` is live and `destination` is writable for its length.
        let code = unsafe {
            bindings::argyle_nimble_mbuf_copydata(
                mbuf.0.as_ptr(),
                native_offset,
                native_length,
                destination.as_mut_ptr().cast(),
            )
        };
        check(Operation::MbufCopy, code)
    }

    fn mbuf_free(&self, mbuf: EspMbuf) -> NativeResult<()> {
        // SAFETY: `mbuf` is consumed, so the chain is freed exactly once.
        check(Operation::MbufFree, unsafe {
            bindings::argyle_nimble_mbuf_free_chain(mbuf.0.as_ptr())
        })
    }

    fn notify(&self, connection: u16, attribute: u16, mbuf: EspMbuf) -> NativeResult<()> {
        // SAFETY: ownership of the live chain passes to the SDK, which frees
        // it in all cases; `mbuf` is consumed so Rust never uses it again.
        let code =
            unsafe { bindings::ble_gatts_notify_custom(connection, attribute, mbuf.0.as_ptr()) };
        check(Operation::Notify, code)
    }

    fn terminate(&self, connection: u16) -> NativeResult<()> {
        let reason = u8::try_from(bindings::ARGYLE_NIMBLE_ERR_REM_USER_CONN_TERM)
            .expect("the remote-user termination reason is an HCI byte");
        // SAFETY: plain SDK call with value arguments.
        check(Operation::Terminate, unsafe {
            bindings::ble_gap_terminate(connection, reason)
        })
    }

    fn set_device_name(&self, name: &CStr) -> NativeResult<()> {
        // SAFETY: `name` is NUL-terminated and readable for the call; NimBLE
        // copies it (or rejects it as too long).
        check(Operation::DeviceName, unsafe {
            bindings::ble_svc_gap_device_name_set(name.as_ptr())
        })
    }

    fn prepare_advertising_fields(&self, fields: &AdvertisingFields) -> EspAdvertisingFields {
        EspAdvertisingFields {
            flags: fields.flags,
            uuids16: fields
                .uuids16
                .iter()
                // SAFETY: the shim builds a UUID value from a plain argument.
                .map(|value| unsafe { bindings::argyle_nimble_uuid16(*value) })
                .collect(),
            uuids16_complete: fields.uuids16_complete,
            uuids128: fields
                .uuids128
                .iter()
                .copied()
                .map(native_uuid128)
                .collect(),
            uuids128_complete: fields.uuids128_complete,
        }
    }

    fn set_advertising_fields(&self, fields: &EspAdvertisingFields) -> NativeResult<()> {
        let count = |length: usize| {
            u8::try_from(length).map_err(|_| NativeError::InvalidLength {
                operation: Operation::AdvertisingData,
                length,
            })
        };
        // SAFETY: the fields are plain data; zero (null pointers, no
        // entries) leaves every other AD type out.
        let mut native: bindings::ble_hs_adv_fields = unsafe { std::mem::zeroed() };
        native.flags = fields.flags;
        if !fields.uuids16.is_empty() {
            native.uuids16 = fields.uuids16.as_ptr();
            native.num_uuids16 = count(fields.uuids16.len())?;
            native.set_uuids16_is_complete(fields.uuids16_complete.into());
        }
        if !fields.uuids128.is_empty() {
            native.uuids128 = fields.uuids128.as_ptr();
            native.num_uuids128 = count(fields.uuids128.len())?;
            native.set_uuids128_is_complete(fields.uuids128_complete.into());
        }
        // SAFETY: `native` is valid for the call and its arrays live in
        // `fields`, which outlives every later use NimBLE makes of its copy
        // (see `EspAdvertisingFields`).
        check(Operation::AdvertisingData, unsafe {
            bindings::ble_gap_adv_set_fields(&native)
        })
    }

    fn set_scan_response_data(&self, data: &[u8]) -> NativeResult<()> {
        let (buffer, length) = payload(Operation::ScanResponseData, data)?;
        // SAFETY: as above; the pointer is valid even for an empty payload,
        // which NimBLE passes to `memcpy`.
        check(Operation::ScanResponseData, unsafe {
            bindings::ble_gap_adv_rsp_set_data(buffer.as_ptr(), length)
        })
    }

    fn advertising_start(&self, address_type: u8) -> NativeResult<()> {
        // SAFETY: the parameters are plain data; zero is a valid value and
        // selects NimBLE's default intervals, all channels, and no filter.
        let mut parameters: bindings::ble_gap_adv_params = unsafe { std::mem::zeroed() };
        parameters.conn_mode = bindings::BLE_GAP_CONN_MODE_UND as u8;
        parameters.disc_mode = bindings::BLE_GAP_DISC_MODE_GEN as u8;
        // SAFETY: undirected advertising takes no peer address; NimBLE copies
        // `parameters` during the call; the callback is a `'static` function
        // whose (null) argument is unused.
        let code = unsafe {
            bindings::ble_gap_adv_start(
                address_type,
                null(),
                bindings::ARGYLE_NIMBLE_HS_FOREVER as i32,
                &parameters,
                GAP_EVENT_CALLBACK,
                null_mut(),
            )
        };
        // `ble_gap_adv_validate` reports an advertising procedure that is
        // already running as EALREADY.
        if code == ALREADY {
            return Ok(());
        }
        check(Operation::AdvertisingStart, code)
    }

    fn advertising_stop(&self) -> NativeResult<()> {
        // SAFETY: plain SDK call with no arguments.
        let code = unsafe { bindings::ble_gap_adv_stop() };
        // NimBLE stops the controller either way and reports EALREADY when
        // no advertising procedure was active.
        if code == ALREADY {
            return Ok(());
        }
        check(Operation::AdvertisingStop, code)
    }

    fn is_synced(&self) -> bool {
        // SAFETY: plain SDK call with no arguments; it reads the sync state.
        unsafe { bindings::ble_hs_synced() != 0 }
    }

    fn infer_address_type(&self) -> NativeResult<u8> {
        let mut address_type = 0;
        // SAFETY: the out pointer refers to a live local for the call; no
        // privacy (0) is requested.
        check(Operation::InferAddress, unsafe {
            bindings::ble_hs_id_infer_auto(0, &mut address_type)
        })?;
        Ok(address_type)
    }

    fn is_host_task(&self) -> bool {
        ON_HOST_TASK.with(std::cell::Cell::get)
    }

    fn mtu(&self, connection: u16) -> Option<u16> {
        // SAFETY: plain SDK call with a value argument; 0 means no connection.
        match unsafe { bindings::ble_att_mtu(connection) } {
            0 => None,
            mtu => Some(mtu),
        }
    }
}

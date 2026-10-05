//! ESP-IDF implementation of the native backend boundary.
//!
//! This is the only module that calls the private generated bindings. The C
//! callback trampolines copy borrowed SDK data into owned values and hand them
//! to the shared [`EventDispatcher`]; they contain no framework logic. They are
//! `extern "C"` functions, so a panic aborts instead of unwinding into C (ESP
//! targets also build with `panic=abort`).

use super::bindings;
use super::dispatch::EventDispatcher;
use super::gap::{GapCodes, GapEvent, GapEventView};
use super::native::{
    check, native_length, Backend, NativeError, NativeEvent, NativeResult, Operation,
};
use std::ffi::{c_int, c_void};
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, MutexGuard};

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

/// The dispatcher receiving host callbacks. NimBLE's sync and reset callbacks
/// carry no user argument, so the single installed dispatcher is stored here.
static CALLBACK_DISPATCHER: Mutex<Option<Arc<EventDispatcher>>> = Mutex::new(None);

fn installed() -> MutexGuard<'static, Option<Arc<EventDispatcher>>> {
    CALLBACK_DISPATCHER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Deliver without holding the slot lock, so removal can wait for quiescence
/// in the dispatcher instead of blocking here.
fn deliver(event: NativeEvent) {
    let dispatcher = installed().clone();
    if let Some(dispatcher) = dispatcher {
        dispatcher.deliver(event);
    }
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

/// The GAP callback to pass with advertising requests (added with the
/// advertising ticket).
pub(crate) const GAP_EVENT_CALLBACK: bindings::ble_gap_event_fn = Some(on_gap_event);

extern "C" fn host_task(_argument: *mut c_void) {
    // SAFETY: called on the task NimBLE created for the host; `nimble_port_run`
    // returns after `nimble_port_stop`, and the task must then delete itself.
    unsafe {
        bindings::nimble_port_run();
        bindings::nimble_port_freertos_deinit();
    }
}

/// An owned native buffer chain.
pub(crate) struct EspMbuf(NonNull<bindings::os_mbuf>);

/// The ESP-IDF NimBLE host. There is one native host per firmware; callers
/// own its lifecycle (see the controller in later tickets).
pub(crate) struct EspBackend;

impl Backend for EspBackend {
    type Mbuf = EspMbuf;

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
        let mut slot = installed();
        if slot.is_some() {
            return Err(NativeError::Busy {
                operation: Operation::InstallCallbacks,
            });
        }
        *slot = Some(dispatcher);
        drop(slot);
        // SAFETY: the shims store these function pointers in `ble_hs_cfg`;
        // they are `'static` and remain valid for the program's lifetime.
        unsafe {
            bindings::argyle_nimble_set_sync_callback(Some(on_host_sync));
            bindings::argyle_nimble_set_reset_callback(Some(on_host_reset));
        }
        Ok(())
    }

    fn remove_callbacks(&self) -> NativeResult<()> {
        // SAFETY: clearing the callbacks stops new native deliveries.
        unsafe {
            bindings::argyle_nimble_set_sync_callback(None);
            bindings::argyle_nimble_set_reset_callback(None);
        }
        let dispatcher = installed().take();
        if let Some(dispatcher) = dispatcher {
            // Wait for callbacks that started before the clear. A removal from
            // inside a callback cannot wait for itself and leaves delivery to
            // finish on its own.
            let _ = dispatcher.detach();
        }
        Ok(())
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

    fn advertising_stop(&self) -> NativeResult<()> {
        // SAFETY: plain SDK call with no arguments.
        check(Operation::AdvertisingStop, unsafe {
            bindings::ble_gap_adv_stop()
        })
    }

    fn mtu(&self, connection: u16) -> Option<u16> {
        // SAFETY: plain SDK call with a value argument; 0 means no connection.
        match unsafe { bindings::ble_att_mtu(connection) } {
            0 => None,
            mtu => Some(mtu),
        }
    }
}

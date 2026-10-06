//! NimBLE GATT tables and access trampolines for the ESP backend.
//!
//! [`prepare`] turns a [`GattPlan`] into the pointer-based definitions NimBLE
//! registers: a service array, a characteristic array per service, and a
//! descriptor array per characteristic that has descriptors, each ended by a
//! zeroed terminator, plus heap-allocated UUIDs and value-handle slots. Every
//! table is owned by the returned [`EspRegistration`], which the owner keeps
//! until the host is deinitialized, so the pointers NimBLE keeps stay valid
//! even after a failed registration. Tables are `Vec`s and UUIDs raw heap
//! allocations, so moving the registration never invalidates those pointers.
//!
//! The decisions here (flag mapping, handle-slot order, handler lookup, and
//! request handling) live in platform-neutral code with host tests; this file
//! only fills in NimBLE's structures. The trampolines check that the context
//! is present, read only its plain `op` and `om` fields (never its union),
//! and never free the context's buffer, which NimBLE owns. A panic in a
//! handler aborts (`extern "C"` and the ESP targets' `panic=abort`) rather
//! than unwinding into C.

use super::bindings;
use super::esp::{EspBackend, EspMbuf};
use super::native::{check, NativeResult, Operation};
use crate::gatt::registration::{
    characteristic_flags, descriptor_flags, dispatch_access, AccessBuffer, AccessCodes,
    AttributeKind, FlagCodes, GattPlan,
};
use crate::{AttError, Uuid};
use std::ffi::{c_int, c_void};
use std::mem::zeroed;
use std::ptr::{null_mut, NonNull};
use std::sync::atomic::{AtomicU16, Ordering};

const ACCESS_CODES: AccessCodes = AccessCodes {
    read_characteristic: bindings::BLE_GATT_ACCESS_OP_READ_CHR,
    write_characteristic: bindings::BLE_GATT_ACCESS_OP_WRITE_CHR,
    read_descriptor: bindings::BLE_GATT_ACCESS_OP_READ_DSC,
    write_descriptor: bindings::BLE_GATT_ACCESS_OP_WRITE_DSC,
};

const FLAG_CODES: FlagCodes = FlagCodes {
    characteristic_read: bindings::BLE_GATT_CHR_F_READ,
    characteristic_write: bindings::BLE_GATT_CHR_F_WRITE,
    characteristic_write_without_response: bindings::BLE_GATT_CHR_F_WRITE_NO_RSP,
    characteristic_notify: bindings::BLE_GATT_CHR_F_NOTIFY,
    attribute_read: bindings::BLE_ATT_F_READ,
    attribute_write: bindings::BLE_ATT_F_WRITE,
};

/// A heap-allocated NimBLE UUID; NimBLE keeps a pointer to its `ble_uuid_t`
/// header. It is held as a raw allocation, freed on drop, so no `Box` is
/// moved or reborrowed after the header pointer is handed out.
enum NativeUuid {
    U16(NonNull<bindings::ble_uuid16_t>),
    U128(NonNull<bindings::ble_uuid128_t>),
}

impl NativeUuid {
    /// Register 16-bit UUIDs as such and everything else as 128-bit, the
    /// widths ATT carries (see `Uuid::att_form`).
    fn new(uuid: Uuid) -> Self {
        match uuid.att_form() {
            Uuid::Uuid16(value) => {
                // SAFETY: the shim builds a UUID value from a plain argument.
                let native = unsafe { bindings::argyle_nimble_uuid16(value) };
                Self::U16(NonNull::from(Box::leak(Box::new(native))))
            }
            uuid => {
                let bytes = uuid.to_wire_bytes();
                // SAFETY: `ble_uuid128_t` is plain data; zero is a valid value.
                let native: &mut bindings::ble_uuid128_t = Box::leak(Box::new(unsafe { zeroed() }));
                // SAFETY: `bytes` holds 16 readable bytes in wire order, as
                // `ble_uuid128_t` stores them, and `native` is writable and
                // separate from them. The shim fails only for null pointers.
                let status =
                    unsafe { bindings::argyle_nimble_uuid128(bytes.as_ref().as_ptr(), native) };
                debug_assert_eq!(status, 0);
                Self::U128(NonNull::from(native))
            }
        }
    }

    fn header(&self) -> *const bindings::ble_uuid_t {
        // SAFETY: the allocations are live until `drop`; only the header's
        // address is taken.
        unsafe {
            match self {
                Self::U16(uuid) => &raw const (*uuid.as_ptr()).u,
                Self::U128(uuid) => &raw const (*uuid.as_ptr()).u,
            }
        }
    }
}

impl Drop for NativeUuid {
    fn drop(&mut self) {
        // SAFETY: each pointer came from `Box::leak` in `new` and is freed
        // once, after NimBLE stopped using it (see `EspRegistration`).
        unsafe {
            match self {
                Self::U16(uuid) => drop(Box::from_raw(uuid.as_ptr())),
                Self::U128(uuid) => drop(Box::from_raw(uuid.as_ptr())),
            }
        }
    }
}

/// NimBLE's tables for one registered server.
pub(crate) struct EspRegistration {
    services: Vec<bindings::ble_gatt_svc_def>,
    // Referenced by `services` and each other; kept alive, never read. Moving
    // a `Vec` does not move or retag its heap buffer.
    _characteristics: Vec<Vec<bindings::ble_gatt_chr_def>>,
    _descriptors: Vec<Vec<bindings::ble_gatt_dsc_def>>,
    _uuids: Vec<NativeUuid>,
    handles: Vec<AtomicU16>,
}

// SAFETY: the raw pointers point into this registration's own heap buffers
// and into the server's stable handler slots; Rust never mutates either
// after `prepare`, and NimBLE reads them on its host task. Value handles are
// atomics.
unsafe impl Send for EspRegistration {}
// SAFETY: as above.
unsafe impl Sync for EspRegistration {}

fn native_uuid(uuids: &mut Vec<NativeUuid>, uuid: Uuid) -> *const bindings::ble_uuid_t {
    let native = NativeUuid::new(uuid);
    let header = native.header();
    uuids.push(native);
    header
}

/// Build NimBLE's tables for `plan` without any native registration call.
pub(crate) fn prepare(plan: &GattPlan) -> EspRegistration {
    let handles: Vec<AtomicU16> = plan.characteristics().map(|_| AtomicU16::new(0)).collect();
    let mut uuids = Vec::new();
    let mut characteristic_tables = Vec::with_capacity(plan.services.len());
    let mut descriptor_tables = Vec::new();
    let mut services = Vec::with_capacity(plan.services.len() + 1);

    for service in &plan.services {
        let mut characteristics = Vec::with_capacity(service.characteristics.len() + 1);
        for characteristic in &service.characteristics {
            let descriptors = if characteristic.descriptors.is_empty() {
                null_mut()
            } else {
                let mut table = Vec::with_capacity(characteristic.descriptors.len() + 1);
                for descriptor in &characteristic.descriptors {
                    // SAFETY: the definition is plain data; zero is a valid
                    // value, and every used field is set below.
                    let mut definition: bindings::ble_gatt_dsc_def = unsafe { zeroed() };
                    definition.uuid = native_uuid(&mut uuids, descriptor.uuid);
                    definition.att_flags = descriptor_flags(descriptor.access, &FLAG_CODES) as u8;
                    definition.access_cb = Some(descriptor_access);
                    definition.arg = descriptor.slot.as_arg();
                    table.push(definition);
                }
                // SAFETY: a zeroed definition (null UUID) ends the array.
                table.push(unsafe { zeroed() });
                let pointer = table.as_mut_ptr();
                descriptor_tables.push(table);
                pointer
            };
            // SAFETY: as above; `cpfd` and unused fields stay null.
            let mut definition: bindings::ble_gatt_chr_def = unsafe { zeroed() };
            definition.uuid = native_uuid(&mut uuids, characteristic.uuid);
            definition.access_cb = Some(characteristic_access);
            definition.arg = characteristic.slot.as_arg();
            definition.descriptors = descriptors;
            definition.flags = characteristic_flags(characteristic.access, &FLAG_CODES)
                as bindings::ble_gatt_chr_flags;
            definition.val_handle = handles[characteristic.handle_index].as_ptr();
            characteristics.push(definition);
        }
        // SAFETY: a zeroed definition (null UUID) ends the array.
        characteristics.push(unsafe { zeroed() });
        // SAFETY: as above.
        let mut definition: bindings::ble_gatt_svc_def = unsafe { zeroed() };
        definition.type_ = bindings::BLE_GATT_SVC_TYPE_PRIMARY as u8;
        definition.uuid = native_uuid(&mut uuids, service.uuid);
        definition.characteristics = characteristics.as_ptr();
        characteristic_tables.push(characteristics);
        services.push(definition);
    }
    // SAFETY: a zeroed definition (`BLE_GATT_SVC_TYPE_END`) ends the array.
    services.push(unsafe { zeroed() });
    const _: () = assert!(bindings::BLE_GATT_SVC_TYPE_END == 0);

    EspRegistration {
        services,
        _characteristics: characteristic_tables,
        _descriptors: descriptor_tables,
        _uuids: uuids,
        handles,
    }
}

/// Count and add the prepared services.
pub(crate) fn register(registration: &EspRegistration) -> NativeResult<()> {
    let services = registration.services.as_ptr();
    // SAFETY: `services` is a terminated array whose tables, UUIDs, and
    // handle slots live in `registration`, which outlives the host.
    check(Operation::GattCount, unsafe {
        bindings::ble_gatts_count_cfg(services)
    })?;
    // SAFETY: as above; NimBLE keeps the pointer until it is deinitialized.
    check(Operation::GattAdd, unsafe {
        bindings::ble_gatts_add_svcs(services)
    })
}

pub(crate) fn value_handles(registration: &EspRegistration) -> Vec<u16> {
    registration
        .handles
        .iter()
        .map(|handle| handle.load(Ordering::Acquire))
        .collect()
}

/// Shared body of both trampolines.
///
/// # Safety
///
/// `context` must be null or valid for this callback, and `argument` null or
/// the callback argument registered for `kind`.
unsafe fn access(
    kind: AttributeKind,
    context: *mut bindings::ble_gatt_access_ctxt,
    argument: *mut c_void,
) -> c_int {
    // SAFETY: NimBLE passes this access's context, valid for the call.
    let Some(context) = (unsafe { context.as_ref() }) else {
        return c_int::from(AttError::UNLIKELY.code());
    };
    let mut buffer = NonNull::new(context.om).map(EspMbuf);
    let buffer = buffer
        .as_mut()
        .map(|buffer| AccessBuffer::new(&EspBackend, buffer));
    // SAFETY: the argument is the slot registered with this callback kind;
    // the server it points into outlives the registration.
    unsafe { dispatch_access(kind, argument, u32::from(context.op), &ACCESS_CODES, buffer) }
}

/// Access callback of every registered characteristic.
unsafe extern "C" fn characteristic_access(
    _connection: u16,
    _attribute: u16,
    context: *mut bindings::ble_gatt_access_ctxt,
    argument: *mut c_void,
) -> c_int {
    // SAFETY: NimBLE calls this with its context and the registered argument.
    unsafe { access(AttributeKind::Characteristic, context, argument) }
}

/// Access callback of every registered descriptor.
unsafe extern "C" fn descriptor_access(
    _connection: u16,
    _attribute: u16,
    context: *mut bindings::ble_gatt_access_ctxt,
    argument: *mut c_void,
) -> c_int {
    // SAFETY: NimBLE calls this with its context and the registered argument.
    unsafe { access(AttributeKind::Descriptor, context, argument) }
}

//! NimBLE GATT tables and access trampolines for the ESP backend.
//!
//! [`prepare`] turns a [`GattPlan`] into the pointer-based definitions NimBLE
//! registers: a service array, a characteristic array per service, and a
//! descriptor array per characteristic that has descriptors, each ended by a
//! zeroed terminator, plus boxed UUIDs and value-handle slots. Every table is
//! heap-allocated and owned by the returned [`EspRegistration`], which the
//! owner keeps until the host is deinitialized, so the pointers NimBLE keeps
//! stay valid even after a failed registration.
//!
//! The access trampolines never touch the access context's union: the
//! callback argument identifies the attribute, and only the plain `op` and
//! `om` fields are read. They never free the context's buffer, which NimBLE
//! owns. A panic in a handler aborts (`extern "C"` and the ESP targets'
//! `panic=abort`) rather than unwinding into C.

use super::bindings;
use super::esp::{EspBackend, EspMbuf};
use super::native::{check, NativeResult, Operation};
use crate::gatt::registration::{
    access_status, serve_access, AccessCodes, AccessOp, CharacteristicSlot, DescriptorSlot,
    GattPlan, Target,
};
use crate::gatt::{Access, DescriptorAccess};
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

/// A boxed NimBLE UUID; NimBLE keeps a pointer to its `ble_uuid_t` header.
enum NativeUuid {
    U16(Box<bindings::ble_uuid16_t>),
    U128(Box<bindings::ble_uuid128_t>),
}

impl NativeUuid {
    /// Register 16-bit UUIDs as such and everything else as 128-bit, the
    /// widths ATT carries (see `Uuid::att_form`).
    fn new(uuid: Uuid) -> Self {
        match uuid.att_form() {
            // SAFETY: the shim builds a UUID value from a plain argument.
            Uuid::Uuid16(value) => {
                Self::U16(Box::new(unsafe { bindings::argyle_nimble_uuid16(value) }))
            }
            uuid => {
                let bytes = uuid.to_wire_bytes();
                // SAFETY: `ble_uuid128_t` is plain data; zero is a valid value.
                let mut native: Box<bindings::ble_uuid128_t> = Box::new(unsafe { zeroed() });
                // SAFETY: `bytes` holds 16 readable bytes in wire order, as
                // `ble_uuid128_t` stores them, and `native` is writable and
                // separate from them. The shim fails only for null pointers.
                let status = unsafe {
                    bindings::argyle_nimble_uuid128(bytes.as_ref().as_ptr(), &mut *native)
                };
                debug_assert_eq!(status, 0);
                Self::U128(native)
            }
        }
    }

    fn header(&self) -> *const bindings::ble_uuid_t {
        match self {
            Self::U16(uuid) => &uuid.u,
            Self::U128(uuid) => &uuid.u,
        }
    }
}

/// NimBLE's tables for one registered server.
pub(crate) struct EspRegistration {
    services: Box<[bindings::ble_gatt_svc_def]>,
    // Referenced by `services` and each other; kept alive, never read.
    _characteristics: Vec<Box<[bindings::ble_gatt_chr_def]>>,
    _descriptors: Vec<Box<[bindings::ble_gatt_dsc_def]>>,
    _uuids: Vec<NativeUuid>,
    handles: Box<[AtomicU16]>,
}

// SAFETY: the raw pointers point into this registration's own boxes and into
// the server's stable handler slots; Rust never mutates either after
// `prepare`, and NimBLE reads them on its host task. Value handles are
// atomics.
unsafe impl Send for EspRegistration {}
// SAFETY: as above.
unsafe impl Sync for EspRegistration {}

fn characteristic_flags(access: Access) -> bindings::ble_gatt_chr_flags {
    let mut flags = 0;
    if access.read {
        flags |= bindings::BLE_GATT_CHR_F_READ;
    }
    if access.write {
        flags |= bindings::BLE_GATT_CHR_F_WRITE;
    }
    if access.write_without_response {
        flags |= bindings::BLE_GATT_CHR_F_WRITE_NO_RSP;
    }
    if access.notify {
        flags |= bindings::BLE_GATT_CHR_F_NOTIFY;
    }
    flags as bindings::ble_gatt_chr_flags
}

fn descriptor_flags(access: DescriptorAccess) -> u8 {
    let mut flags = 0;
    if access.read {
        flags |= bindings::BLE_ATT_F_READ;
    }
    if access.write {
        flags |= bindings::BLE_ATT_F_WRITE;
    }
    flags as u8
}

fn native_uuid(uuids: &mut Vec<NativeUuid>, uuid: Uuid) -> *const bindings::ble_uuid_t {
    let native = NativeUuid::new(uuid);
    let header = native.header();
    uuids.push(native);
    header
}

/// Build NimBLE's tables for `plan` without any native registration call.
pub(crate) fn prepare(plan: &GattPlan) -> EspRegistration {
    let handles: Box<[AtomicU16]> = plan.characteristics().map(|_| AtomicU16::new(0)).collect();
    let mut uuids = Vec::new();
    let mut characteristic_tables = Vec::new();
    let mut descriptor_tables = Vec::new();
    let mut services = Vec::with_capacity(plan.services.len() + 1);
    let mut handle_slots = handles.iter();

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
                    definition.att_flags = descriptor_flags(descriptor.access);
                    definition.access_cb = Some(descriptor_access);
                    definition.arg = descriptor.slot.as_arg();
                    table.push(definition);
                }
                // SAFETY: a zeroed definition (null UUID) ends the array.
                table.push(unsafe { zeroed() });
                let table = table.into_boxed_slice();
                let pointer = table.as_ptr().cast_mut();
                descriptor_tables.push(table);
                pointer
            };
            // SAFETY: as above; `cpfd` and unused fields stay null.
            let mut definition: bindings::ble_gatt_chr_def = unsafe { zeroed() };
            definition.uuid = native_uuid(&mut uuids, characteristic.uuid);
            definition.access_cb = Some(characteristic_access);
            definition.arg = characteristic.slot.as_arg();
            definition.descriptors = descriptors;
            definition.flags = characteristic_flags(characteristic.access);
            definition.val_handle = handle_slots
                .next()
                .expect("one handle slot per characteristic")
                .as_ptr();
            characteristics.push(definition);
        }
        // SAFETY: a zeroed definition (null UUID) ends the array.
        characteristics.push(unsafe { zeroed() });
        let characteristics = characteristics.into_boxed_slice();
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
        services: services.into_boxed_slice(),
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

fn unlikely() -> c_int {
    c_int::from(AttError::UNLIKELY.code())
}

/// Access callback of every registered characteristic.
unsafe extern "C" fn characteristic_access(
    _connection: u16,
    _attribute: u16,
    context: *mut bindings::ble_gatt_access_ctxt,
    argument: *mut c_void,
) -> c_int {
    // SAFETY: NimBLE passes this access's context, valid for the call.
    let Some(context) = (unsafe { context.as_ref() }) else {
        return unlikely();
    };
    // SAFETY: the argument is the slot registered with this callback; the
    // server it points into outlives the registration.
    let Some(target) = (unsafe { CharacteristicSlot::from_arg(argument) }) else {
        return unlikely();
    };
    let op = AccessOp::from_code(u32::from(context.op), &ACCESS_CODES);
    let mut buffer = NonNull::new(context.om).map(EspMbuf);
    access_status(serve_access(
        &EspBackend,
        Target::Characteristic(target),
        op,
        buffer.as_mut(),
    ))
}

/// Access callback of every registered descriptor.
unsafe extern "C" fn descriptor_access(
    _connection: u16,
    _attribute: u16,
    context: *mut bindings::ble_gatt_access_ctxt,
    argument: *mut c_void,
) -> c_int {
    // SAFETY: NimBLE passes this access's context, valid for the call.
    let Some(context) = (unsafe { context.as_ref() }) else {
        return unlikely();
    };
    // SAFETY: the argument is the slot registered with this callback; the
    // server it points into outlives the registration.
    let Some(target) = (unsafe { DescriptorSlot::from_arg(argument) }) else {
        return unlikely();
    };
    let op = AccessOp::from_code(u32::from(context.op), &ACCESS_CODES);
    let mut buffer = NonNull::new(context.om).map(EspMbuf);
    access_status(serve_access(
        &EspBackend,
        Target::Descriptor(target),
        op,
        buffer.as_mut(),
    ))
}

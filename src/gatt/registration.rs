//! Translation of a frozen [`GattServer`] into a native registration plan,
//! and dispatch of native attribute access to the typed handlers.
//!
//! The plan is platform-neutral: the ESP backend turns it into NimBLE's
//! pointer-based tables, and the fake backend records it. Each plan entry
//! points at the stable, boxed slot that holds its type-erased handler inside
//! the server, which outlives the registration (see [`crate::ble`]).
//!
//! Access dispatch never frees or keeps the native buffer it is given: NimBLE
//! owns the request and response buffers of an access callback. Reads append
//! the encoded value; writes are length-checked against the attribute's
//! `MAX_LEN` before anything is copied, then copied, decoded, and delivered.
//!
//! # Offsets and long values
//!
//! This follows what ESP-IDF 6.1's NimBLE does around an access callback
//! (`ble_gatts_val_access` in `ble_gatts.c` and the request handlers in
//! `ble_att_svr.c`):
//!
//! - **Read and Read Blob.** NimBLE calls the callback for every request,
//!   with the request's offset in the context. At offset 0 the callback
//!   appends to the response, which already holds the ATT opcode; otherwise
//!   it appends to a fresh buffer, from which NimBLE copies the bytes after
//!   the offset (`BLE_ATT_ERR_INVALID_OFFSET` if the value is shorter) and
//!   frees it. NimBLE then truncates the response to the ATT MTU. A read
//!   therefore always appends the handler's complete value and never applies
//!   the offset itself.
//! - **Write Request, Write Command, and Signed Write.** One callback with
//!   the whole PDU value at offset 0.
//! - **Prepare Write and Execute Write.** NimBLE queues the prepared parts
//!   itself without calling back. On execute it requires each attribute's
//!   parts to start at offset 0 and be contiguous (otherwise
//!   `BLE_ATT_ERR_INVALID_OFFSET`) and, unless `BLE_GATT_BLOB_TRANSFER` is
//!   enabled, to total at most `BLE_ATT_ATTR_MAX_LEN` (512) bytes (otherwise
//!   `BLE_ATT_ERR_INVALID_ATTR_VALUE_LEN`). It then calls back once per
//!   attribute with the parts chained into one buffer at offset 0.
//!
//! So every write reaching a callback is a complete value at offset 0. A
//! write at any other offset would be part of a value, which this framework
//! does not reassemble; it is refused with
//! [`AttError::REQUEST_NOT_SUPPORTED`] before the buffer is read.

// Used by the ESP backend and tests; host builds have no native registration.
#![cfg_attr(not(any(test, argyle_nimble_esp)), allow(dead_code))]

#[cfg(test)]
pub(crate) mod att_model;

use super::descriptor::{DescriptorAccess, RegisteredDescriptor};
use super::{Access, EndpointId, GattServer, RegisteredCharacteristic};
use crate::backend::native::Backend;
use crate::{AttError, Uuid};
use std::ffi::c_void;

/// The stable slot of a registered characteristic, passed to native code as
/// its callback argument.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CharacteristicSlot(*const Box<dyn RegisteredCharacteristic>);

/// The stable slot of a registered descriptor, passed to native code as its
/// callback argument.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DescriptorSlot(*const Box<dyn RegisteredDescriptor>);

impl CharacteristicSlot {
    pub(crate) fn as_arg(self) -> *mut c_void {
        self.0.cast_mut().cast()
    }

    /// Recover the handler from a callback argument.
    ///
    /// # Safety
    ///
    /// `arg` must be null or come from [`as_arg`](Self::as_arg) of a slot
    /// whose server is still alive.
    pub(crate) unsafe fn from_arg<'a>(
        arg: *mut c_void,
    ) -> Option<&'a dyn RegisteredCharacteristic> {
        // SAFETY: the caller guarantees a null or live slot pointer.
        unsafe { arg.cast::<Box<dyn RegisteredCharacteristic>>().as_ref() }.map(|slot| &**slot)
    }
}

impl DescriptorSlot {
    pub(crate) fn as_arg(self) -> *mut c_void {
        self.0.cast_mut().cast()
    }

    /// Recover the handler from a callback argument.
    ///
    /// # Safety
    ///
    /// `arg` must be null or come from [`as_arg`](Self::as_arg) of a slot
    /// whose server is still alive.
    pub(crate) unsafe fn from_arg<'a>(arg: *mut c_void) -> Option<&'a dyn RegisteredDescriptor> {
        // SAFETY: the caller guarantees a null or live slot pointer.
        unsafe { arg.cast::<Box<dyn RegisteredDescriptor>>().as_ref() }.map(|slot| &**slot)
    }
}

/// One descriptor to register.
#[derive(Debug)]
pub(crate) struct PlannedDescriptor {
    pub(crate) uuid: Uuid,
    pub(crate) access: DescriptorAccess,
    pub(crate) slot: DescriptorSlot,
}

/// One characteristic to register, with its custom descriptors. The
/// stack-managed CCCD of a notify-capable characteristic is not listed.
#[derive(Debug)]
pub(crate) struct PlannedCharacteristic {
    pub(crate) uuid: Uuid,
    pub(crate) access: Access,
    pub(crate) slot: CharacteristicSlot,
    pub(crate) descriptors: Vec<PlannedDescriptor>,
    /// The index of this characteristic's value-handle slot; slots are in
    /// registration order.
    pub(crate) handle_index: usize,
}

/// One primary service to register.
#[derive(Debug)]
pub(crate) struct PlannedService {
    pub(crate) uuid: Uuid,
    pub(crate) characteristics: Vec<PlannedCharacteristic>,
}

/// The services of a server in registration order. UUIDs are in their ATT
/// form (16- or 128-bit). Characteristic value handles, once assigned, are reported
/// in the same order as [`characteristics`](Self::characteristics).
#[derive(Debug)]
pub(crate) struct GattPlan {
    pub(crate) services: Vec<PlannedService>,
    endpoints: Vec<Option<EndpointId>>,
}

impl GattPlan {
    /// Plan the registration of `server`. The plan points into `server`, so
    /// it is valid only while `server` is alive and not moved out of its
    /// heap storage; the server's contents never move once built.
    pub(crate) fn new(server: &GattServer) -> Self {
        let mut endpoints = Vec::new();
        let services = server
            .services()
            .iter()
            .map(|service| PlannedService {
                uuid: service.uuid().att_form(),
                characteristics: service
                    .characteristics()
                    .iter()
                    .map(|characteristic| {
                        let handle_index = endpoints.len();
                        endpoints.push(characteristic.endpoint().cloned());
                        PlannedCharacteristic {
                            handle_index,
                            uuid: characteristic.uuid().att_form(),
                            access: characteristic.access(),
                            slot: CharacteristicSlot(characteristic),
                            descriptors: characteristic
                                .descriptors()
                                .iter()
                                .map(|descriptor| PlannedDescriptor {
                                    uuid: descriptor.uuid().att_form(),
                                    access: descriptor.access(),
                                    slot: DescriptorSlot(descriptor),
                                })
                                .collect(),
                        }
                    })
                    .collect(),
            })
            .collect();
        Self {
            services,
            endpoints,
        }
    }

    /// Every characteristic in registration order.
    pub(crate) fn characteristics(&self) -> impl Iterator<Item = &PlannedCharacteristic> {
        self.services
            .iter()
            .flat_map(|service| service.characteristics.iter())
    }

    /// The notify endpoint of each characteristic, in registration order.
    pub(crate) fn endpoints(&self) -> &[Option<EndpointId>] {
        &self.endpoints
    }
}

/// The native access operation, translated from the SDK's code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AccessOp {
    ReadCharacteristic,
    WriteCharacteristic,
    ReadDescriptor,
    WriteDescriptor,
    /// An operation this framework does not know.
    Unknown,
}

/// The SDK's access operation codes, supplied by the backend.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AccessCodes {
    pub(crate) read_characteristic: u32,
    pub(crate) write_characteristic: u32,
    pub(crate) read_descriptor: u32,
    pub(crate) write_descriptor: u32,
}

impl AccessOp {
    pub(crate) fn from_code(code: u32, codes: &AccessCodes) -> Self {
        if code == codes.read_characteristic {
            Self::ReadCharacteristic
        } else if code == codes.write_characteristic {
            Self::WriteCharacteristic
        } else if code == codes.read_descriptor {
            Self::ReadDescriptor
        } else if code == codes.write_descriptor {
            Self::WriteDescriptor
        } else {
            Self::Unknown
        }
    }
}

/// The handler an access callback reached.
#[derive(Clone, Copy)]
pub(crate) enum Target<'a> {
    Characteristic(&'a dyn RegisteredCharacteristic),
    Descriptor(&'a dyn RegisteredDescriptor),
}

impl Target<'_> {
    fn max_len(self) -> usize {
        match self {
            Self::Characteristic(target) => target.max_len(),
            Self::Descriptor(target) => target.max_len(),
        }
    }

    fn read(self, output: &mut Vec<u8>) -> Result<(), AttError> {
        match self {
            Self::Characteristic(target) => target.read(output),
            Self::Descriptor(target) => target.read(output),
        }
    }

    fn writable(self) -> bool {
        match self {
            Self::Characteristic(target) => {
                let access = target.access();
                access.write || access.write_without_response
            }
            Self::Descriptor(target) => target.access().write,
        }
    }

    fn write(self, data: &[u8]) -> Result<(), AttError> {
        match self {
            Self::Characteristic(target) => target.write(data),
            Self::Descriptor(target) => target.write(data),
        }
    }
}

/// A borrowed view of an access context's buffer, which NimBLE owns: the
/// response for reads, the request for writes. It offers only length, append,
/// and copy, so the buffer cannot be freed, transferred, or replaced through
/// it.
pub(crate) struct AccessBuffer<'a, B: Backend> {
    backend: &'a B,
    mbuf: &'a mut B::Mbuf,
}

impl<'a, B: Backend> AccessBuffer<'a, B> {
    pub(crate) fn new(backend: &'a B, mbuf: &'a mut B::Mbuf) -> Self {
        Self { backend, mbuf }
    }

    fn len(&self) -> usize {
        self.backend.mbuf_len(self.mbuf)
    }

    fn append(&mut self, data: &[u8]) -> Result<(), AttError> {
        // A failed append may leave part of the value in NimBLE's buffer;
        // NimBLE discards the response when an error is returned.
        self.backend
            .mbuf_append(self.mbuf, data)
            .map_err(|_| AttError::INSUFFICIENT_RESOURCES)
    }

    fn copy(&self, destination: &mut [u8]) -> Result<(), AttError> {
        self.backend
            .mbuf_copy(self.mbuf, 0, destination)
            .map_err(|_| AttError::UNLIKELY)
    }
}

/// Serve one native attribute access at `offset` through `buffer`, which is
/// never freed or kept here.
///
/// An operation for the other kind of attribute, an unknown operation, or a
/// missing buffer is answered with [`AttError::UNLIKELY`]. Undeclared
/// operations are refused by the handler's declared access, independent of
/// the native permission flags.
///
/// A read encodes the handler's complete value into a Rust-owned buffer of
/// at most `MAX_LEN` bytes and appends it in one call only if encoding
/// succeeded, whatever `offset` is; NimBLE slices it (see the
/// [module documentation](self#offsets-and-long-values)). Nothing a failed
/// handler or codec produced reaches the response. If the append itself
/// fails part-way, the error status makes NimBLE discard what was appended:
/// it frees the fresh buffer of a nonzero offset, and `ble_att_svr_tx_rsp`
/// empties the response before sending an Error Response.
///
/// A write is refused, in this order, if it is undeclared, at a nonzero
/// `offset`, or longer than `MAX_LEN`, all before any byte is copied. It is
/// then copied from every segment of the chain into a zeroed, owned buffer
/// of exactly its length, decoded, and delivered; a failed copy reaches no
/// handler.
pub(crate) fn serve_access<B: Backend>(
    target: Target<'_>,
    op: AccessOp,
    offset: u16,
    buffer: Option<AccessBuffer<'_, B>>,
) -> Result<(), AttError> {
    let mut buffer = buffer.ok_or(AttError::UNLIKELY)?;
    let reading = match (target, op) {
        (Target::Characteristic(_), AccessOp::ReadCharacteristic)
        | (Target::Descriptor(_), AccessOp::ReadDescriptor) => true,
        (Target::Characteristic(_), AccessOp::WriteCharacteristic)
        | (Target::Descriptor(_), AccessOp::WriteDescriptor) => false,
        _ => return Err(AttError::UNLIKELY),
    };
    if reading {
        let mut value = Vec::new();
        target.read(&mut value)?;
        buffer.append(&value)
    } else {
        // Permission comes first, so an undeclared write is refused as such
        // whatever its offset or length.
        if !target.writable() {
            return Err(AttError::WRITE_NOT_PERMITTED);
        }
        if offset != 0 {
            return Err(AttError::REQUEST_NOT_SUPPORTED);
        }
        let length = buffer.len();
        if length > target.max_len() {
            return Err(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH);
        }
        let mut data = vec![0; length];
        buffer.copy(&mut data)?;
        target.write(&data)
    }
}

/// Which kind of attribute a native access callback serves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AttributeKind {
    Characteristic,
    Descriptor,
}

/// The body of a native access callback after the context was found: recover
/// the handler from the callback argument and serve the operation at the
/// context's `offset`, returning the status for NimBLE. A null argument is
/// answered with [`AttError::UNLIKELY`].
///
/// # Safety
///
/// `argument` must be null or the callback argument of a `kind` slot from
/// [`GattPlan`] whose server is still alive.
pub(crate) unsafe fn dispatch_access<B: Backend>(
    kind: AttributeKind,
    argument: *mut c_void,
    op: u32,
    offset: u16,
    codes: &AccessCodes,
    buffer: Option<AccessBuffer<'_, B>>,
) -> i32 {
    // SAFETY: the caller guarantees a null or live slot argument of `kind`.
    let target = unsafe {
        match kind {
            AttributeKind::Characteristic => {
                CharacteristicSlot::from_arg(argument).map(Target::Characteristic)
            }
            AttributeKind::Descriptor => DescriptorSlot::from_arg(argument).map(Target::Descriptor),
        }
    };
    access_status(match target {
        Some(target) => serve_access(target, AccessOp::from_code(op, codes), offset, buffer),
        None => Err(AttError::UNLIKELY),
    })
}

/// The SDK's characteristic property and attribute permission bits, supplied
/// by the backend.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FlagCodes {
    pub(crate) characteristic_read: u32,
    pub(crate) characteristic_write: u32,
    pub(crate) characteristic_write_without_response: u32,
    pub(crate) characteristic_notify: u32,
    pub(crate) attribute_read: u32,
    pub(crate) attribute_write: u32,
}

/// Native characteristic flags for declared capabilities.
pub(crate) fn characteristic_flags(access: Access, codes: &FlagCodes) -> u32 {
    let mut flags = 0;
    if access.read {
        flags |= codes.characteristic_read;
    }
    if access.write {
        flags |= codes.characteristic_write;
    }
    if access.write_without_response {
        flags |= codes.characteristic_write_without_response;
    }
    if access.notify {
        flags |= codes.characteristic_notify;
    }
    flags
}

/// Native attribute permissions for a descriptor's declared access.
pub(crate) fn descriptor_flags(access: DescriptorAccess, codes: &FlagCodes) -> u32 {
    let mut flags = 0;
    if access.read {
        flags |= codes.attribute_read;
    }
    if access.write {
        flags |= codes.attribute_write;
    }
    flags
}

/// The status an access callback returns to NimBLE: zero or an ATT error.
pub(crate) fn access_status(result: Result<(), AttError>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(error) => i32::from(error.code()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::{FakeBackend, NativeCall};
    use crate::backend::native::Operation;
    use crate::gatt::{
        Characteristic, CharacteristicDef, Descriptor, DescriptorDef, Readable, ReadableDescriptor,
        Service, Writable, WritableDescriptor,
    };
    use std::sync::{Arc, Mutex};

    /// A characteristic or descriptor storing one byte, tagged by name so
    /// tests can tell which handler ran.
    #[derive(Clone)]
    struct Cell {
        name: &'static str,
        uuid: Uuid,
        value: Arc<Mutex<u8>>,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl Cell {
        fn new(name: &'static str, uuid: Uuid, log: &Arc<Mutex<Vec<String>>>) -> Self {
            Self {
                name,
                uuid,
                value: Arc::default(),
                log: log.clone(),
            }
        }
    }

    impl Characteristic for Cell {
        type Value = u8;
        fn uuid(&self) -> Uuid {
            self.uuid
        }
    }

    impl Readable for Cell {
        fn read(&self) -> Result<u8, AttError> {
            self.log.lock().unwrap().push(format!("read {}", self.name));
            Ok(*self.value.lock().unwrap())
        }
    }

    impl Writable for Cell {
        fn write(&self, value: u8) -> Result<(), AttError> {
            self.log
                .lock()
                .unwrap()
                .push(format!("write {} {value}", self.name));
            *self.value.lock().unwrap() = value;
            Ok(())
        }
    }

    impl Descriptor for Cell {
        type Value = u8;
        fn uuid(&self) -> Uuid {
            self.uuid
        }
    }

    impl ReadableDescriptor for Cell {
        fn read(&self) -> Result<u8, AttError> {
            self.log.lock().unwrap().push(format!("read {}", self.name));
            Ok(*self.value.lock().unwrap())
        }
    }

    impl WritableDescriptor for Cell {
        fn write(&self, value: u8) -> Result<(), AttError> {
            self.log
                .lock()
                .unwrap()
                .push(format!("write {} {value}", self.name));
            *self.value.lock().unwrap() = value;
            Ok(())
        }
    }

    /// A text characteristic limited to 4 bytes.
    struct Label;

    impl Characteristic for Label {
        type Value = String;
        const MAX_LEN: usize = 4;
        fn uuid(&self) -> Uuid {
            Uuid::Uuid128(0x1234_5678_9abc_4def_8123_4567_89ab_cdef)
        }
    }

    impl Writable for Label {
        fn write(&self, _: String) -> Result<(), AttError> {
            Ok(())
        }
    }

    const CUSTOM: u128 = 0x6e40_0001_b5a3_f393_e0a9_e50e_24dc_ca9e;

    /// Two services: a battery service with a read/notify characteristic and
    /// two descriptors, and a custom service with a read/write
    /// characteristic and a write-only label.
    fn server(log: &Arc<Mutex<Vec<String>>>) -> (GattServer, crate::gatt::NotifyEndpoint<u8>) {
        let (level, endpoint) =
            CharacteristicDef::new(Cell::new("level", Uuid::Uuid16(0x2a19), log))
                .readable()
                .descriptor(
                    DescriptorDef::new(Cell::new("level-description", Uuid::Uuid16(0x2901), log))
                        .unwrap()
                        .readable(),
                )
                .descriptor(
                    DescriptorDef::new(Cell::new("level-setting", Uuid::Uuid16(0xff01), log))
                        .unwrap()
                        .readable()
                        .writable(),
                )
                .notifiable();
        let battery =
            Service::primary(Uuid::Uuid128(Uuid::Uuid16(0x180f).to_u128())).characteristic(level);
        let custom = Service::primary(Uuid::Uuid128(CUSTOM))
            .characteristic(
                CharacteristicDef::new(Cell::new("mode", Uuid::Uuid32(0x0000_ff02), log))
                    .readable()
                    .writable(),
            )
            .characteristic(CharacteristicDef::new(Label).writable_without_response());
        (GattServer::new([battery, custom]).unwrap(), endpoint)
    }

    fn read_access(
        fake: &FakeBackend,
        target: Target<'_>,
        op: AccessOp,
    ) -> (Result<(), AttError>, Vec<u8>) {
        let mut buffer = fake.mbuf_from_flat(&[]).unwrap();
        let id = buffer.id();
        let result = serve_access(target, op, 0, Some(AccessBuffer::new(fake, &mut buffer)));
        let data = fake.mbuf_data(id).unwrap();
        fake.mbuf_free(buffer).unwrap();
        (result, data)
    }

    fn write_access(
        fake: &FakeBackend,
        target: Target<'_>,
        op: AccessOp,
        data: &[u8],
    ) -> Result<(), AttError> {
        let mut buffer = fake.mbuf_from_flat(data).unwrap();
        let result = serve_access(target, op, 0, Some(AccessBuffer::new(fake, &mut buffer)));
        fake.mbuf_free(buffer).unwrap();
        result
    }

    #[test]
    fn the_plan_mirrors_the_hierarchy_with_att_form_uuids() {
        let log = Arc::default();
        let (server, endpoint) = server(&log);
        let plan = GattPlan::new(&server);
        let fake = FakeBackend::new();
        let registration = fake.prepare_gatt(&plan);

        let summary: Vec<_> = registration
            .services
            .iter()
            .map(|service| {
                (
                    service.uuid,
                    service
                        .characteristics
                        .iter()
                        .map(|characteristic| {
                            (
                                characteristic.uuid,
                                characteristic.access,
                                characteristic
                                    .descriptors
                                    .iter()
                                    .map(|(uuid, access, _)| (*uuid, access.read, access.write))
                                    .collect::<Vec<_>>(),
                            )
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        let access = |read, write, write_without_response, notify| crate::gatt::Access {
            read,
            write,
            write_without_response,
            notify,
        };
        assert_eq!(
            summary,
            [
                (
                    Uuid::Uuid16(0x180f),
                    vec![(
                        Uuid::Uuid16(0x2a19),
                        access(true, false, false, true),
                        vec![
                            (Uuid::Uuid16(0x2901), true, false),
                            (Uuid::Uuid16(0xff01), true, true)
                        ],
                    )],
                ),
                (
                    Uuid::Uuid128(CUSTOM),
                    vec![
                        (
                            Uuid::Uuid16(0xff02),
                            access(true, true, false, false),
                            vec![]
                        ),
                        (
                            Uuid::Uuid128(0x1234_5678_9abc_4def_8123_4567_89ab_cdef),
                            access(false, false, true, false),
                            vec![],
                        ),
                    ],
                ),
            ]
        );
        assert_eq!(plan.endpoints(), [Some(endpoint.id().clone()), None, None]);
        assert_eq!(
            fake.value_handles(&registration),
            [0, 0, 0],
            "assigned at registration"
        );
        drop(registration);
        assert_eq!(fake.calls(), [NativeCall::RegistrationFreed]);
    }

    #[test]
    fn callback_arguments_route_to_the_registered_handlers() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let (server, _) = server(&log);
        let plan = GattPlan::new(&server);
        let fake = FakeBackend::new();
        let registration = fake.prepare_gatt(&plan);

        // Recover every handler from its native callback argument.
        let level = &registration.services[0].characteristics[0];
        let mode = &registration.services[1].characteristics[0];
        // SAFETY: the arguments come from slots of the live `server`.
        let (level_target, mode_target, setting_target, description_target) = unsafe {
            (
                CharacteristicSlot::from_arg(level.slot.as_arg()).unwrap(),
                CharacteristicSlot::from_arg(mode.slot.as_arg()).unwrap(),
                DescriptorSlot::from_arg(level.descriptors[1].2.as_arg()).unwrap(),
                DescriptorSlot::from_arg(level.descriptors[0].2.as_arg()).unwrap(),
            )
        };
        // SAFETY: a null argument is rejected rather than dereferenced.
        assert!(unsafe { CharacteristicSlot::from_arg(std::ptr::null_mut()) }.is_none());
        assert!(unsafe { DescriptorSlot::from_arg(std::ptr::null_mut()) }.is_none());

        assert_eq!(
            write_access(
                &fake,
                Target::Characteristic(mode_target),
                AccessOp::WriteCharacteristic,
                &[7]
            ),
            Ok(())
        );
        assert_eq!(
            write_access(
                &fake,
                Target::Descriptor(setting_target),
                AccessOp::WriteDescriptor,
                &[9]
            ),
            Ok(())
        );
        assert_eq!(
            read_access(
                &fake,
                Target::Characteristic(mode_target),
                AccessOp::ReadCharacteristic
            ),
            (Ok(()), vec![7])
        );
        assert_eq!(
            read_access(
                &fake,
                Target::Descriptor(setting_target),
                AccessOp::ReadDescriptor
            ),
            (Ok(()), vec![9])
        );
        assert_eq!(
            read_access(
                &fake,
                Target::Characteristic(level_target),
                AccessOp::ReadCharacteristic
            ),
            (Ok(()), vec![0]),
            "the parent is unaffected by its descriptor's write"
        );
        assert_eq!(
            read_access(
                &fake,
                Target::Descriptor(description_target),
                AccessOp::ReadDescriptor
            ),
            (Ok(()), vec![0])
        );
        assert_eq!(
            *log.lock().unwrap(),
            [
                "write mode 7",
                "write level-setting 9",
                "read mode",
                "read level-setting",
                "read level",
                "read level-description"
            ]
        );
        drop(registration);
        assert_eq!(
            fake.assert_balanced(),
            Ok(()),
            "every buffer was freed by its owner"
        );
        assert!(fake.violations().is_empty());
    }

    #[test]
    fn malformed_and_undeclared_requests_are_refused() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let (server, _) = server(&log);
        let fake = FakeBackend::new();
        let services = server.services();
        let level = Target::Characteristic(services[0].characteristics()[0].as_ref());
        let description =
            Target::Descriptor(services[0].characteristics()[0].descriptors()[0].as_ref());
        let label = Target::Characteristic(services[1].characteristics()[1].as_ref());

        // An operation for the other kind of attribute, or an unknown one.
        for (target, op) in [
            (level, AccessOp::ReadDescriptor),
            (level, AccessOp::WriteDescriptor),
            (description, AccessOp::ReadCharacteristic),
            (description, AccessOp::WriteCharacteristic),
            (level, AccessOp::Unknown),
            (description, AccessOp::Unknown),
        ] {
            assert_eq!(
                read_access(&fake, target, op).0,
                Err(AttError::UNLIKELY),
                "{op:?}"
            );
        }
        // No buffer in the context.
        assert_eq!(
            serve_access::<FakeBackend>(level, AccessOp::ReadCharacteristic, 0, None),
            Err(AttError::UNLIKELY)
        );
        // Operations that were not declared stay refused even if NimBLE
        // passed them through.
        assert_eq!(
            write_access(&fake, level, AccessOp::WriteCharacteristic, &[1]),
            Err(AttError::WRITE_NOT_PERMITTED)
        );
        assert_eq!(
            write_access(&fake, description, AccessOp::WriteDescriptor, &[1]),
            Err(AttError::WRITE_NOT_PERMITTED)
        );
        assert_eq!(
            read_access(&fake, label, AccessOp::ReadCharacteristic).0,
            Err(AttError::READ_NOT_PERMITTED)
        );
        assert!(log.lock().unwrap().is_empty(), "no handler ran");
        assert_eq!(fake.assert_balanced(), Ok(()));
    }

    #[test]
    fn write_lengths_are_checked_before_copying() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let (server, _) = server(&log);
        let fake = FakeBackend::new();
        let label = Target::Characteristic(server.services()[1].characteristics()[1].as_ref());
        let mode = Target::Characteristic(server.services()[1].characteristics()[0].as_ref());

        assert_eq!(
            write_access(&fake, label, AccessOp::WriteCharacteristic, b"abcd"),
            Ok(())
        );
        assert_eq!(
            write_access(&fake, label, AccessOp::WriteCharacteristic, b""),
            Ok(())
        );
        let copies_before = fake
            .calls()
            .iter()
            .filter(|call| matches!(call, NativeCall::MbufCopy { .. }))
            .count();
        assert_eq!(
            write_access(&fake, label, AccessOp::WriteCharacteristic, b"abcde"),
            Err(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH)
        );
        let copies_after = fake
            .calls()
            .iter()
            .filter(|call| matches!(call, NativeCall::MbufCopy { .. }))
            .count();
        assert_eq!(
            copies_before, copies_after,
            "an oversized write is not copied"
        );
        // Decode failures keep their ATT meaning.
        assert_eq!(
            write_access(&fake, mode, AccessOp::WriteCharacteristic, &[]),
            Err(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH)
        );
        assert_eq!(
            write_access(&fake, label, AccessOp::WriteCharacteristic, &[0xff]),
            Err(AttError::VALUE_NOT_ALLOWED)
        );
        assert_eq!(fake.assert_balanced(), Ok(()));
    }

    #[test]
    fn native_buffer_failures_become_att_errors_without_leaks() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let (server, _) = server(&log);
        let fake = FakeBackend::new();
        let mode = Target::Characteristic(server.services()[1].characteristics()[0].as_ref());

        fake.fail_next(Operation::MbufAppend, 6);
        let (result, _) = read_access(&fake, mode, AccessOp::ReadCharacteristic);
        assert_eq!(result, Err(AttError::INSUFFICIENT_RESOURCES));

        fake.fail_next(Operation::MbufCopy, 6);
        assert_eq!(
            write_access(&fake, mode, AccessOp::WriteCharacteristic, &[3]),
            Err(AttError::UNLIKELY)
        );
        assert_eq!(
            *log.lock().unwrap(),
            ["read mode"],
            "the failed write never reached the handler"
        );
        assert_eq!(
            fake.assert_balanced(),
            Ok(()),
            "the access path never frees or keeps buffers"
        );
        assert!(fake.violations().is_empty());
    }

    #[test]
    fn statuses_and_operation_codes_translate() {
        assert_eq!(access_status(Ok(())), 0);
        assert_eq!(
            access_status(Err(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH)),
            0x0d
        );
        let codes = AccessCodes {
            read_characteristic: 10,
            write_characteristic: 11,
            read_descriptor: 12,
            write_descriptor: 13,
        };
        for (code, op) in [
            (10, AccessOp::ReadCharacteristic),
            (11, AccessOp::WriteCharacteristic),
            (12, AccessOp::ReadDescriptor),
            (13, AccessOp::WriteDescriptor),
            (0, AccessOp::Unknown),
            (u32::MAX, AccessOp::Unknown),
        ] {
            assert_eq!(AccessOp::from_code(code, &codes), op);
        }
    }

    /// Raw bytes, up to 6.
    struct Bytes(Arc<Mutex<Vec<u8>>>);

    impl Characteristic for Bytes {
        type Value = Vec<u8>;
        const MAX_LEN: usize = 6;
        fn uuid(&self) -> Uuid {
            Uuid::Uuid32(0xabcd_0001)
        }
    }

    impl Readable for Bytes {
        fn read(&self) -> Result<Vec<u8>, AttError> {
            Ok(self.0.lock().unwrap().clone())
        }
    }

    impl Writable for Bytes {
        fn write(&self, value: Vec<u8>) -> Result<(), AttError> {
            *self.0.lock().unwrap() = value;
            Ok(())
        }
    }

    fn copies(fake: &FakeBackend) -> Vec<NativeCall> {
        fake.calls()
            .into_iter()
            .filter(|call| matches!(call, NativeCall::MbufCopy { .. }))
            .collect()
    }

    #[test]
    fn chained_buffers_are_read_and_written_across_segment_boundaries() {
        let stored = Arc::new(Mutex::new(Vec::new()));
        let definition = CharacteristicDef::new(Bytes(stored.clone()))
            .readable()
            .writable();
        let server =
            GattServer::new([Service::primary(Uuid::Uuid16(0x181c)).characteristic(definition)])
                .unwrap();
        let target = Target::Characteristic(server.services()[0].characteristics()[0].as_ref());
        let fake = FakeBackend::new();

        // A write spread over segments, including an empty one, is copied
        // whole in one call and delivered intact.
        let mut chain = fake.mbuf_from_segments(&[&[1, 2], &[], &[3], &[4, 5, 6]]);
        assert_eq!(fake.mbuf_segments(chain.id()), Some(vec![2, 0, 1, 3]));
        assert_eq!(
            serve_access(
                target,
                AccessOp::WriteCharacteristic,
                0,
                Some(AccessBuffer::new(&fake, &mut chain))
            ),
            Ok(())
        );
        assert_eq!(*stored.lock().unwrap(), [1, 2, 3, 4, 5, 6]);
        assert!(matches!(
            copies(&fake)[..],
            [NativeCall::MbufCopy {
                offset: 0,
                length: 6,
                ..
            }]
        ));
        fake.mbuf_free(chain).unwrap();

        // One byte past MAX_LEN across segments is refused before copying.
        let mut chain = fake.mbuf_from_segments(&[&[1, 2, 3], &[4, 5, 6], &[7]]);
        assert_eq!(
            serve_access(
                target,
                AccessOp::WriteCharacteristic,
                0,
                Some(AccessBuffer::new(&fake, &mut chain))
            ),
            Err(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH)
        );
        assert_eq!(copies(&fake).len(), 1);
        fake.mbuf_free(chain).unwrap();

        // A copy failing within the chain reaches no handler.
        fake.fail_next(Operation::MbufCopy, -1);
        let mut chain = fake.mbuf_from_segments(&[&[9], &[9, 9]]);
        assert_eq!(
            serve_access(
                target,
                AccessOp::WriteCharacteristic,
                0,
                Some(AccessBuffer::new(&fake, &mut chain))
            ),
            Err(AttError::UNLIKELY)
        );
        assert_eq!(*stored.lock().unwrap(), [1, 2, 3, 4, 5, 6]);
        fake.mbuf_free(chain).unwrap();

        // A read appends after what the response buffer already holds.
        let mut response = fake.mbuf_from_segments(&[&[0xaa]]);
        assert_eq!(
            serve_access(
                target,
                AccessOp::ReadCharacteristic,
                0,
                Some(AccessBuffer::new(&fake, &mut response))
            ),
            Ok(())
        );
        assert_eq!(
            fake.mbuf_data(response.id()),
            Some(vec![0xaa, 1, 2, 3, 4, 5, 6])
        );
        fake.mbuf_free(response).unwrap();

        assert_eq!(fake.assert_balanced(), Ok(()));
        assert!(fake.violations().is_empty());
    }

    #[test]
    fn an_undeclared_write_is_refused_before_its_length_is_checked() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let (server, _) = server(&log);
        let fake = FakeBackend::new();
        let level = Target::Characteristic(server.services()[0].characteristics()[0].as_ref());
        let description =
            Target::Descriptor(server.services()[0].characteristics()[0].descriptors()[0].as_ref());
        let oversized = vec![0; 600];
        for (target, op) in [
            (level, AccessOp::WriteCharacteristic),
            (description, AccessOp::WriteDescriptor),
        ] {
            assert_eq!(
                write_access(&fake, target, op, &oversized),
                Err(AttError::WRITE_NOT_PERMITTED)
            );
        }
        assert!(copies(&fake).is_empty(), "nothing was copied");
        assert_eq!(fake.assert_balanced(), Ok(()));
    }

    #[test]
    fn handles_are_assigned_when_the_host_starts_and_32_bit_uuids_register_as_128_bit() {
        let stored = Arc::default();
        let server = GattServer::new([Service::primary(Uuid::Uuid16(0x181c))
            .characteristic(CharacteristicDef::new(Bytes(stored)).readable())])
        .unwrap();
        let plan = GattPlan::new(&server);
        assert_eq!(
            plan.services[0].characteristics[0].uuid,
            Uuid::Uuid128(Uuid::Uuid32(0xabcd_0001).to_u128()),
            "ATT carries only 16- and 128-bit UUIDs"
        );
        let fake = FakeBackend::new();
        let registration = fake.prepare_gatt(&plan);
        fake.register_gatt(&registration).unwrap();
        assert_eq!(
            fake.value_handles(&registration),
            [0],
            "not before the host starts"
        );
        fake.host_start().unwrap();
        assert_eq!(fake.value_handles(&registration), [0x13]);
    }

    const CODES: AccessCodes = AccessCodes {
        read_characteristic: 20,
        write_characteristic: 21,
        read_descriptor: 22,
        write_descriptor: 23,
    };

    #[test]
    fn the_trampoline_body_validates_its_argument_operation_and_buffer() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let (server, _) = server(&log);
        let plan = GattPlan::new(&server);
        let fake = FakeBackend::new();
        let mode = plan.services[1].characteristics[0].slot.as_arg();
        let setting = plan.services[0].characteristics[0].descriptors[1]
            .slot
            .as_arg();
        let unlikely = i32::from(AttError::UNLIKELY.code());

        let dispatch = |kind, argument, op, data: Option<&[u8]>| {
            let mut buffer = data.map(|data| fake.mbuf_from_flat(data).unwrap());
            // SAFETY: arguments are null or slots of the live `server` of the
            // matching kind.
            let status = unsafe {
                dispatch_access(
                    kind,
                    argument,
                    op,
                    0,
                    &CODES,
                    buffer
                        .as_mut()
                        .map(|buffer| AccessBuffer::new(&fake, buffer)),
                )
            };
            let contents = buffer
                .as_ref()
                .and_then(|buffer| fake.mbuf_data(buffer.id()));
            if let Some(buffer) = buffer {
                fake.mbuf_free(buffer).unwrap();
            }
            (status, contents)
        };

        // A null argument, an unknown operation, or a missing buffer.
        assert_eq!(
            dispatch(
                AttributeKind::Characteristic,
                std::ptr::null_mut(),
                20,
                Some(&[])
            )
            .0,
            unlikely
        );
        assert_eq!(
            dispatch(
                AttributeKind::Descriptor,
                std::ptr::null_mut(),
                22,
                Some(&[])
            )
            .0,
            unlikely
        );
        assert_eq!(
            dispatch(AttributeKind::Characteristic, mode, 99, Some(&[])).0,
            unlikely
        );
        assert_eq!(
            dispatch(AttributeKind::Characteristic, mode, 20, None).0,
            unlikely
        );
        // Valid requests reach their handlers and return NimBLE statuses.
        assert_eq!(
            dispatch(AttributeKind::Characteristic, mode, 21, Some(&[5])).0,
            0
        );
        assert_eq!(
            dispatch(AttributeKind::Characteristic, mode, 20, Some(&[])),
            (0, Some(vec![5]))
        );
        assert_eq!(
            dispatch(AttributeKind::Descriptor, setting, 23, Some(&[6])).0,
            0
        );
        assert_eq!(
            dispatch(AttributeKind::Descriptor, setting, 22, Some(&[])),
            (0, Some(vec![6]))
        );
        // A descriptor operation code on a characteristic is refused.
        assert_eq!(
            dispatch(AttributeKind::Characteristic, mode, 22, Some(&[])).0,
            unlikely
        );
        assert_eq!(
            dispatch(AttributeKind::Characteristic, mode, 21, Some(&[1, 2])).0,
            i32::from(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH.code())
        );
        assert_eq!(
            *log.lock().unwrap(),
            [
                "write mode 5",
                "read mode",
                "write level-setting 6",
                "read level-setting"
            ]
        );
        assert_eq!(fake.assert_balanced(), Ok(()));
    }

    #[test]
    fn native_flags_follow_declared_access() {
        let codes = FlagCodes {
            characteristic_read: 0x01,
            characteristic_write: 0x02,
            characteristic_write_without_response: 0x04,
            characteristic_notify: 0x08,
            attribute_read: 0x10,
            attribute_write: 0x20,
        };
        let access = |read, write, write_without_response, notify| crate::gatt::Access {
            read,
            write,
            write_without_response,
            notify,
        };
        assert_eq!(
            characteristic_flags(access(false, false, false, false), &codes),
            0
        );
        assert_eq!(
            characteristic_flags(access(true, false, false, false), &codes),
            0x01
        );
        assert_eq!(
            characteristic_flags(access(false, true, true, false), &codes),
            0x06
        );
        assert_eq!(
            characteristic_flags(access(true, true, true, true), &codes),
            0x0f
        );
        assert_eq!(
            characteristic_flags(access(false, false, false, true), &codes),
            0x08
        );
        let descriptor = |read, write| DescriptorAccess { read, write };
        assert_eq!(descriptor_flags(descriptor(true, false), &codes), 0x10);
        assert_eq!(descriptor_flags(descriptor(false, true), &codes), 0x20);
        assert_eq!(descriptor_flags(descriptor(true, true), &codes), 0x30);
    }

    #[test]
    fn handle_slots_follow_registration_order() {
        let log = Arc::default();
        let (server, _) = server(&log);
        let plan = GattPlan::new(&server);
        let indexes: Vec<_> = plan
            .characteristics()
            .map(|characteristic| characteristic.handle_index)
            .collect();
        assert_eq!(indexes, [0, 1, 2]);
        assert_eq!(plan.endpoints().len(), indexes.len());
    }

    /// Text up to 12 bytes, as a characteristic and as a descriptor. The
    /// handler refuses text starting with `!`.
    #[derive(Clone)]
    struct Text {
        value: Arc<Mutex<String>>,
        written: Arc<Mutex<Vec<String>>>,
    }

    impl Text {
        fn new(value: &str) -> Self {
            Self {
                value: Arc::new(Mutex::new(value.into())),
                written: Arc::default(),
            }
        }

        fn accept(&self, value: String) -> Result<(), AttError> {
            if value.starts_with('!') {
                return Err(AttError::WRITE_REQUEST_REJECTED);
            }
            self.written.lock().unwrap().push(value.clone());
            *self.value.lock().unwrap() = value;
            Ok(())
        }

        fn written(&self) -> Vec<String> {
            self.written.lock().unwrap().clone()
        }
    }

    impl Characteristic for Text {
        type Value = String;
        const MAX_LEN: usize = 12;
        fn uuid(&self) -> Uuid {
            Uuid::Uuid16(0xfff5)
        }
    }

    impl Readable for Text {
        fn read(&self) -> Result<String, AttError> {
            Ok(self.value.lock().unwrap().clone())
        }
    }

    impl Writable for Text {
        fn write(&self, value: String) -> Result<(), AttError> {
            self.accept(value)
        }
    }

    impl Descriptor for Text {
        type Value = String;
        const MAX_LEN: usize = 12;
        fn uuid(&self) -> Uuid {
            Uuid::Uuid16(0xfff6)
        }
    }

    impl ReadableDescriptor for Text {
        fn read(&self) -> Result<String, AttError> {
            Ok(self.value.lock().unwrap().clone())
        }
    }

    impl WritableDescriptor for Text {
        fn write(&self, value: String) -> Result<(), AttError> {
            self.accept(value)
        }
    }

    /// One server with `text` as a characteristic and as its descriptor.
    fn text_server(text: &Text) -> GattServer {
        let definition = CharacteristicDef::new(text.clone())
            .readable()
            .writable()
            .descriptor(
                DescriptorDef::new(text.clone())
                    .unwrap()
                    .readable()
                    .writable(),
            );
        GattServer::new([Service::primary(Uuid::Uuid16(0x181c)).characteristic(definition)])
            .unwrap()
    }

    /// Both attributes of [`text_server`] with their read and write
    /// operations.
    fn text_targets(server: &GattServer) -> [(Target<'_>, AccessOp, AccessOp); 2] {
        let characteristic = server.services()[0].characteristics()[0].as_ref();
        [
            (
                Target::Characteristic(characteristic),
                AccessOp::ReadCharacteristic,
                AccessOp::WriteCharacteristic,
            ),
            (
                Target::Descriptor(characteristic.descriptors()[0].as_ref()),
                AccessOp::ReadDescriptor,
                AccessOp::WriteDescriptor,
            ),
        ]
    }

    /// Serve one access over a NimBLE-owned chain of `segments`, then free
    /// it as NimBLE would; returns the result and the buffer's final bytes.
    fn serve(
        fake: &FakeBackend,
        target: Target<'_>,
        op: AccessOp,
        offset: u16,
        segments: &[&[u8]],
    ) -> (Result<(), AttError>, Vec<u8>) {
        let mut buffer = fake.mbuf_from_segments(segments);
        let result = serve_access(
            target,
            op,
            offset,
            Some(AccessBuffer::new(fake, &mut buffer)),
        );
        let data = fake.mbuf_data(buffer.id()).unwrap();
        fake.mbuf_free(buffer).unwrap();
        (result, data)
    }

    fn appended(fake: &FakeBackend) -> Vec<usize> {
        fake.calls()
            .into_iter()
            .filter_map(|call| match call {
                NativeCall::MbufAppend { length, .. } => Some(length),
                _ => None,
            })
            .collect()
    }

    fn touched(fake: &FakeBackend) -> usize {
        fake.calls()
            .iter()
            .filter(|call| {
                matches!(
                    call,
                    NativeCall::MbufLen { .. } | NativeCall::MbufCopy { .. }
                )
            })
            .count()
    }

    /// The Read Response opcode NimBLE puts in the response before calling
    /// back.
    const OPCODE: &[u8] = &[0x0b];

    #[test]
    fn empty_exact_and_over_limit_text_follows_max_len() {
        let text = Text::new("");
        let server = text_server(&text);
        let fake = FakeBackend::new();
        for (target, read, write) in text_targets(&server) {
            text.written.lock().unwrap().clear();
            assert_eq!(serve(&fake, target, write, 0, &[]).0, Ok(()));
            assert_eq!(text.written(), [""], "an empty write is a value");
            assert_eq!(
                serve(&fake, target, read, 0, &[OPCODE]),
                (Ok(()), OPCODE.to_vec())
            );
            assert_eq!(appended(&fake).last(), Some(&0));

            let exact = "twelve bytes";
            assert_eq!(
                serve(&fake, target, write, 0, &[exact.as_bytes()]).0,
                Ok(())
            );
            assert_eq!(
                serve(&fake, target, read, 0, &[OPCODE]),
                (Ok(()), [OPCODE, exact.as_bytes()].concat())
            );

            let before = touched(&fake);
            assert_eq!(
                serve(&fake, target, write, 0, &[b"thirteen", b"bytes"]).0,
                Err(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH)
            );
            assert_eq!(touched(&fake), before + 1, "measured, never copied");
            assert_eq!(text.written(), ["", exact]);

            // A handler value over MAX_LEN is the server's fault and is
            // never truncated into the response.
            *text.value.lock().unwrap() = "thirteen byte".into();
            let appends = appended(&fake).len();
            assert_eq!(
                serve(&fake, target, read, 0, &[OPCODE]),
                (Err(AttError::UNLIKELY), OPCODE.to_vec())
            );
            assert_eq!(appended(&fake).len(), appends, "nothing appended");
        }
        assert_eq!(fake.assert_balanced(), Ok(()));
        assert!(fake.violations().is_empty());
    }

    #[test]
    fn text_split_at_every_segment_boundary_arrives_whole() {
        let text = Text::new("");
        let server = text_server(&text);
        let fake = FakeBackend::new();
        // One-, two-, three-, and four-byte characters: 11 bytes.
        let value = "aé€😀b";
        let bytes = value.as_bytes();
        // The same text with the emoji's last byte corrupted.
        let mut malformed = bytes.to_vec();
        malformed[9] = b'!';
        for (target, _, write) in text_targets(&server) {
            for first in 0..=bytes.len() {
                for second in first..=bytes.len() {
                    let split = |data: &[u8]| {
                        serve(
                            &fake,
                            target,
                            write,
                            0,
                            &[&data[..first], &data[first..second], &data[second..]],
                        )
                    };
                    text.written.lock().unwrap().clear();
                    assert_eq!(split(bytes).0, Ok(()), "split at {first}, {second}");
                    assert_eq!(text.written(), [value]);
                    assert_eq!(split(&malformed).0, Err(AttError::VALUE_NOT_ALLOWED));
                    assert_eq!(text.written(), [value], "the handler never saw it");
                }
            }
        }
        assert_eq!(fake.assert_balanced(), Ok(()));
    }

    #[test]
    fn malformed_text_is_refused_before_the_handler() {
        let text = Text::new("keep");
        let server = text_server(&text);
        let fake = FakeBackend::new();
        let malformed: [&[u8]; 6] = [
            &[0xff],
            &[b'o', b'k', 0xc3],       // truncated sequence
            &[0xc0, 0xaf],             // overlong encoding
            &[0xed, 0xa0, 0x80],       // UTF-16 surrogate
            &[0xf4, 0x90, 0x80, 0x80], // above U+10FFFF
            &[0xe2, 0x82, b'x'],       // bad continuation byte
        ];
        for (target, read, write) in text_targets(&server) {
            for value in malformed {
                assert_eq!(
                    serve(&fake, target, write, 0, &[value]).0,
                    Err(AttError::VALUE_NOT_ALLOWED),
                    "{value:x?}"
                );
            }
            // NUL is valid UTF-8 and the encoding has no terminator.
            assert_eq!(serve(&fake, target, write, 0, &[b"a\0b"]).0, Ok(()));
            assert_eq!(
                serve(&fake, target, read, 0, &[OPCODE]).1,
                [OPCODE, b"a\0b"].concat()
            );
            *text.value.lock().unwrap() = "keep".into();
        }
        assert_eq!(text.written(), ["a\0b", "a\0b"]);
        assert_eq!(fake.assert_balanced(), Ok(()));
    }

    #[test]
    fn write_failures_at_every_stage_leave_state_and_request_untouched() {
        let text = Text::new("keep");
        let server = text_server(&text);
        let fake = FakeBackend::new();
        // The request's segments, a native fault to inject, and the result.
        type Case<'a> = (&'a [&'a [u8]], Option<Operation>, AttError);
        for (target, _, write) in text_targets(&server) {
            let cases: [Case<'_>; 4] = [
                (
                    &[b"too long ", b"by far"],
                    None,
                    AttError::INVALID_ATTRIBUTE_VALUE_LENGTH,
                ),
                (
                    &[b"ok", b"ay"],
                    Some(Operation::MbufCopy),
                    AttError::UNLIKELY,
                ),
                (&[b"bad", &[0xff]], None, AttError::VALUE_NOT_ALLOWED),
                (&[b"!", b"no"], None, AttError::WRITE_REQUEST_REJECTED),
            ];
            for (segments, fault, error) in cases {
                if let Some(operation) = fault {
                    fake.fail_next(operation, -1);
                }
                let (result, request) = serve(&fake, target, write, 0, segments);
                assert_eq!(result, Err(error));
                assert_eq!(request, segments.concat(), "the request is never modified");
                assert_eq!(*text.value.lock().unwrap(), "keep");
            }
        }
        assert!(text.written().is_empty());
        assert_eq!(
            fake.assert_balanced(),
            Ok(()),
            "the request is never freed here"
        );
        assert!(fake.violations().is_empty());
    }

    #[test]
    fn writes_at_an_offset_are_refused_and_reads_ignore_it() {
        let text = Text::new("value");
        let server = text_server(&text);
        let fake = FakeBackend::new();
        for (target, read, write) in text_targets(&server) {
            for offset in [1, 22, u16::MAX] {
                let before = touched(&fake);
                assert_eq!(
                    serve(&fake, target, write, offset, &[b"part"]).0,
                    Err(AttError::REQUEST_NOT_SUPPORTED)
                );
                assert_eq!(touched(&fake), before, "the buffer was not read");
                // NimBLE slices reads itself, so the full value is appended
                // to the fresh buffer it passes for a nonzero offset.
                assert_eq!(
                    serve(&fake, target, read, offset, &[]),
                    (Ok(()), b"value".to_vec())
                );
            }
        }
        // An undeclared write is refused as such, whatever its offset.
        let read_only = CharacteristicDef::new(text.clone()).readable();
        assert_eq!(
            serve(
                &fake,
                Target::Characteristic(&read_only),
                AccessOp::WriteCharacteristic,
                3,
                &[b"x"]
            )
            .0,
            Err(AttError::WRITE_NOT_PERMITTED)
        );
        assert!(text.written().is_empty());
        assert_eq!(fake.assert_balanced(), Ok(()));
    }

    /// A value whose encoding fails part-way or overruns its capacity.
    #[derive(Clone, Copy, Debug)]
    enum Fault {
        Partial,
        Overrun,
        Valid,
    }

    impl crate::codec::Encode for Fault {
        fn encode(
            &self,
            writer: &mut crate::codec::ValueWriter<'_>,
        ) -> Result<(), crate::codec::EncodeError> {
            match self {
                Self::Partial => {
                    writer.write_bytes(&[1, 2, 3])?;
                    Err(crate::codec::EncodeError::InvalidValue { reason: "partial" })
                }
                Self::Overrun => {
                    writer.write_bytes(&[4; 8])?;
                    writer.write_bytes(&[5])
                }
                Self::Valid => writer.write_bytes(&[6; 8]),
            }
        }
    }

    /// Reads [`Fault`] values up to 8 bytes, or fails in its handler.
    struct Faulty(Mutex<Result<Fault, AttError>>);

    impl Characteristic for Faulty {
        type Value = Fault;
        const MAX_LEN: usize = 8;
        fn uuid(&self) -> Uuid {
            Uuid::Uuid16(0xfff7)
        }
    }

    impl Readable for Faulty {
        fn read(&self) -> Result<Fault, AttError> {
            *self.0.lock().unwrap()
        }
    }

    #[test]
    fn failed_or_partial_encodings_never_reach_the_response() {
        let application = AttError::application(0x80).unwrap();
        let faulty = CharacteristicDef::new(Faulty(Mutex::new(Ok(Fault::Valid)))).readable();
        let target = Target::Characteristic(&faulty);
        let fake = FakeBackend::new();
        let read = |state, offset| {
            *faulty.characteristic.0.lock().unwrap() = state;
            serve(
                &fake,
                target,
                AccessOp::ReadCharacteristic,
                offset,
                &[OPCODE],
            )
        };
        for (state, error) in [
            (Ok(Fault::Partial), AttError::UNLIKELY),
            (Ok(Fault::Overrun), AttError::UNLIKELY),
            (Err(application), application),
        ] {
            for offset in [0, 5] {
                assert_eq!(
                    read(state, offset),
                    (Err(error), OPCODE.to_vec()),
                    "{state:?}"
                );
            }
        }
        assert!(appended(&fake).is_empty(), "no handler output was appended");

        assert_eq!(
            read(Ok(Fault::Valid), 0),
            (Ok(()), [OPCODE, &[6; 8]].concat())
        );
        assert_eq!(appended(&fake), [8]);
        // An append failing part-way returns the status NimBLE discards the
        // response for; nothing beyond the value was written.
        fake.fail_next(Operation::MbufAppend, 6);
        let (result, response) = read(Ok(Fault::Valid), 0);
        assert_eq!(result, Err(AttError::INSUFFICIENT_RESOURCES));
        assert_eq!(response, [OPCODE, &[6; 4]].concat());
        assert_eq!(fake.assert_balanced(), Ok(()));
        assert!(fake.violations().is_empty());
    }
}

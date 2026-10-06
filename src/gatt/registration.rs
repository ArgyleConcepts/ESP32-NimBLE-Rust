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

// Used by the ESP backend and tests; host builds have no native registration.
#![cfg_attr(not(any(test, argyle_nimble_esp)), allow(dead_code))]

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
                        endpoints.push(characteristic.endpoint().cloned());
                        PlannedCharacteristic {
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

/// Serve one native attribute access. `buffer` is the access context's
/// buffer, which NimBLE owns: the response for reads, the request for
/// writes. It is never freed or kept here.
///
/// An operation for the other kind of attribute, an unknown operation, or a
/// missing buffer is answered with [`AttError::UNLIKELY`]. Undeclared
/// operations are refused by the handler's declared access, independent of
/// the native permission flags.
pub(crate) fn serve_access<B: Backend>(
    backend: &B,
    target: Target<'_>,
    op: AccessOp,
    buffer: Option<&mut B::Mbuf>,
) -> Result<(), AttError> {
    let buffer = buffer.ok_or(AttError::UNLIKELY)?;
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
        // A failed append may leave part of the value in NimBLE's buffer;
        // NimBLE discards the response when an error is returned.
        backend
            .mbuf_append(buffer, &value)
            .map_err(|_| AttError::INSUFFICIENT_RESOURCES)
    } else {
        // Permission comes first, so an undeclared write is refused as such
        // whatever its length.
        if !target.writable() {
            return Err(AttError::WRITE_NOT_PERMITTED);
        }
        let length = backend.mbuf_len(buffer);
        if length > target.max_len() {
            return Err(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH);
        }
        let mut data = vec![0; length];
        backend
            .mbuf_copy(buffer, 0, &mut data)
            .map_err(|_| AttError::UNLIKELY)?;
        target.write(&data)
    }
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
        let result = serve_access(fake, target, op, Some(&mut buffer));
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
        let result = serve_access(fake, target, op, Some(&mut buffer));
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
            serve_access(&fake, level, AccessOp::ReadCharacteristic, None),
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
                &fake,
                target,
                AccessOp::WriteCharacteristic,
                Some(&mut chain)
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
                &fake,
                target,
                AccessOp::WriteCharacteristic,
                Some(&mut chain)
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
                &fake,
                target,
                AccessOp::WriteCharacteristic,
                Some(&mut chain)
            ),
            Err(AttError::UNLIKELY)
        );
        assert_eq!(*stored.lock().unwrap(), [1, 2, 3, 4, 5, 6]);
        fake.mbuf_free(chain).unwrap();

        // A read appends after what the response buffer already holds.
        let mut response = fake.mbuf_from_segments(&[&[0xaa]]);
        assert_eq!(
            serve_access(
                &fake,
                target,
                AccessOp::ReadCharacteristic,
                Some(&mut response)
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
}

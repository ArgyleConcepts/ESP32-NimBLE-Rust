//! Declarative GATT server definitions.
//!
//! An application describes its attribute database as owned Rust values:
//!
//! 1. Each characteristic is an application type implementing
//!    [`Characteristic`], which names its UUID and value type. It also
//!    implements [`Readable`] and/or [`Writable`] to supply handlers.
//! 2. A [`CharacteristicDef`] declares which capabilities the characteristic
//!    offers. Each declaration method requires the matching handler trait, so
//!    a capability without its handler does not compile.
//! 3. A [`Service`] groups characteristic definitions under a service UUID.
//! 4. [`GattServer::new`] validates the services and freezes them into an
//!    owned server definition, ready to transfer to the BLE owner.
//!
//! ```
//! use argyle_nimble::gatt::{Characteristic, CharacteristicDef, GattServer, Readable, Service, Writable};
//! use argyle_nimble::{AttError, Uuid};
//! use std::sync::atomic::{AtomicU8, Ordering};
//! use std::sync::Arc;
//!
//! /// Application state shared with the rest of the program.
//! struct Device {
//!     level: AtomicU8,
//! }
//!
//! struct Level(Arc<Device>);
//!
//! impl Characteristic for Level {
//!     type Value = u8;
//!     fn uuid(&self) -> Uuid {
//!         Uuid::Uuid16(0x2a19)
//!     }
//! }
//!
//! impl Readable for Level {
//!     fn read(&self) -> Result<u8, AttError> {
//!         Ok(self.0.level.load(Ordering::Relaxed))
//!     }
//! }
//!
//! impl Writable for Level {
//!     fn write(&self, level: u8) -> Result<(), AttError> {
//!         if level > 100 {
//!             return Err(AttError::VALUE_NOT_ALLOWED);
//!         }
//!         self.0.level.store(level, Ordering::Relaxed);
//!         Ok(())
//!     }
//! }
//!
//! let device = Arc::new(Device { level: AtomicU8::new(50) });
//! let (level, level_updates) = CharacteristicDef::new(Level(device.clone()))
//!     .readable()
//!     .writable()
//!     .notifiable();
//! let service = Service::primary(Uuid::Uuid16(0x180f)).characteristic(level);
//! let server = GattServer::new([service])?;
//! // `server` goes to the BLE owner; `level_updates` later identifies the
//! // characteristic when sending notifications.
//! # let _ = (server, level_updates);
//! # Ok::<(), argyle_nimble::Error>(())
//! ```
//!
//! # Guarantees
//!
//! **At compile time:**
//!
//! - Declaring read access requires [`Readable`]; declaring write access
//!   requires [`Writable`]; declaring notifications requires an encodable
//!   value type.
//! - Readable and notifiable values implement [`Encode`]; writable values
//!   implement [`DecodeOwned`], so a handler never receives a borrow of
//!   released native data.
//! - Characteristic types are `Send + Sync + 'static`. Handlers cannot retain
//!   borrowed data, and state that is not thread-safe, such as `Rc` or
//!   `RefCell`, is rejected.
//! - Definitions move into their service and services into the server, so
//!   nothing can change after [`GattServer::new`]: the server has no methods
//!   that add, remove, or mutably borrow its contents.
//! - The declared capabilities are the only source of the characteristic's
//!   properties and access. No public flag mask, native definition, pointer,
//!   or attribute handle exists, and a [`NotifyEndpoint`] can only come from
//!   [`CharacteristicDef::notifiable`].
//!
//! **At runtime**, [`GattServer::new`] rejects a server without services, a
//! characteristic without any capability, and a
//! [`Characteristic::MAX_LEN`] above the 512-byte attribute limit.
//!
//! # Planned request handling
//!
//! Registering a server with NimBLE is not implemented yet, so no handler is
//! called by the BLE stack today. The following is the contract the
//! registration work will implement; the request logic it will use is in
//! place and unit-tested:
//!
//! - A written value longer than `MAX_LEN` is rejected with
//!   [`AttError::INVALID_ATTRIBUTE_VALUE_LENGTH`] before decoding, decode
//!   failures become the matching [`AttError`], and a read value that cannot
//!   be encoded within `MAX_LEN` is reported to the client as
//!   [`AttError::UNLIKELY`].
//! - Handlers are synchronous and will run on the NimBLE host task. Keep them
//!   short and non-blocking: the host processes no other BLE events while a
//!   handler runs.
//! - The framework holds no locks of its own while calling a handler, so a
//!   handler may take application locks; avoiding deadlocks between those
//!   locks and other application threads is the application's
//!   responsibility.
//! - [`Readable::read`] may be called more than once for one client read:
//!   NimBLE reads values longer than one ATT packet in pieces and calls the
//!   handler for each piece. A value that changes between calls can reach the
//!   client mixed; keep long values stable or keep them within one packet.
//!
//! # Shared state
//!
//! Share state with the rest of the program through thread-safe types such as
//! `Arc<Mutex<_>>` or atomics. Handlers and application codec implementations
//! ([`Encode`] and [`Decode`](crate::codec::Decode)) must not panic; how a
//! panic is contained will be defined when handlers are connected to NimBLE.
//!
//! Handlers receive no request context, such as the connection, yet. Phase 1
//! serves one client with open access; if context is added, it will arrive
//! as new provided trait methods so existing handlers keep compiling.
//!
//! Registration with NimBLE, descriptors, and sending notifications are not
//! part of this module yet.

use crate::codec::{decode_value, DecodeOwned, Encode, ValueWriter, MAX_ATTRIBUTE_VALUE_LEN};
use crate::{AttError, Error, Uuid};
use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;

/// An application characteristic: its UUID and the type of its value.
///
/// Implement [`Readable`] and/or [`Writable`] alongside this trait to handle
/// requests, then declare the capabilities with [`CharacteristicDef`].
///
/// The `Send + Sync + 'static` bounds let the BLE host task call handlers
/// while the application keeps using shared state from other threads.
pub trait Characteristic: Send + Sync + 'static {
    /// The value read, written, or notified.
    type Value;

    /// The largest encoded value, in bytes, this characteristic produces or
    /// accepts. It must not exceed [`MAX_ATTRIBUTE_VALUE_LEN`], the default.
    const MAX_LEN: usize = MAX_ATTRIBUTE_VALUE_LEN;

    /// The characteristic UUID. It is read once, when the characteristic is
    /// defined.
    fn uuid(&self) -> Uuid;
}

/// Handles reads of a characteristic's value.
pub trait Readable: Characteristic<Value: Encode> {
    /// Return the current value, or the ATT error to send to the client.
    /// One client read of a long value may call this more than once; see the
    /// [module documentation](crate::gatt#planned-request-handling).
    fn read(&self) -> Result<Self::Value, AttError>;
}

/// Handles writes of a characteristic's value.
pub trait Writable: Characteristic<Value: DecodeOwned> {
    /// Accept a written value, already decoded, or return the ATT error to
    /// send to the client.
    fn write(&self, value: Self::Value) -> Result<(), AttError>;
}

/// The capabilities a characteristic declares, from which native properties
/// and permissions are derived.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Access {
    pub(crate) read: bool,
    pub(crate) write: bool,
    pub(crate) write_without_response: bool,
    pub(crate) notify: bool,
}

impl Access {
    fn is_empty(self) -> bool {
        self == Self::default()
    }
}

/// The identity of one notifiable characteristic: a shared allocation whose
/// address no other live endpoint can have. Unlike a counter, it cannot wrap,
/// and it needs no 64-bit atomics, which the ESP32-C3 and ESP32-S3 lack.
#[derive(Clone, Debug)]
pub(crate) struct EndpointId(Arc<()>);

impl EndpointId {
    fn new() -> Self {
        Self(Arc::new(()))
    }
}

impl PartialEq for EndpointId {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for EndpointId {}

impl std::hash::Hash for EndpointId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}

/// Identifies a notifiable characteristic and the type of value it sends.
///
/// Only [`CharacteristicDef::notifiable`] creates endpoints; there is no way
/// to build one from a number or native handle. Each endpoint refers to
/// exactly one characteristic definition, and clones refer to the same one.
pub struct NotifyEndpoint<V> {
    id: EndpointId,
    value: PhantomData<fn(&V)>,
}

impl<V> NotifyEndpoint<V> {
    // Read by the notification runtime in a later ticket and by tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn id(&self) -> &EndpointId {
        &self.id
    }
}

impl<V> Clone for NotifyEndpoint<V> {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            value: PhantomData,
        }
    }
}

impl<V> PartialEq for NotifyEndpoint<V> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<V> Eq for NotifyEndpoint<V> {}

impl<V> fmt::Debug for NotifyEndpoint<V> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NotifyEndpoint")
            .finish_non_exhaustive()
    }
}

type ReadThunk<C> = fn(&C, &mut ValueWriter<'_>) -> Result<(), AttError>;
type WriteThunk<C> = fn(&C, &[u8]) -> Result<(), AttError>;

fn read_thunk<C: Readable>(
    characteristic: &C,
    writer: &mut ValueWriter<'_>,
) -> Result<(), AttError> {
    let value = characteristic.read()?;
    writer.write(&value).map_err(AttError::from)
}

fn write_thunk<C: Writable>(characteristic: &C, data: &[u8]) -> Result<(), AttError> {
    let value = decode_value::<C::Value>(data)?;
    characteristic.write(value)
}

/// A characteristic with its declared capabilities.
///
/// Each capability method requires the handler or value bounds it needs, so
/// an undeclarable capability fails to compile. Repeating a declaration has
/// no further effect.
pub struct CharacteristicDef<C: Characteristic> {
    // Called through `RegisteredCharacteristic` by registration in a later
    // ticket and by tests.
    #[cfg_attr(not(test), allow(dead_code))]
    characteristic: C,
    uuid: Uuid,
    access: Access,
    read: Option<ReadThunk<C>>,
    write: Option<WriteThunk<C>>,
    endpoint: Option<EndpointId>,
}

impl<C: Characteristic> CharacteristicDef<C> {
    /// Start a definition with no capabilities. At least one must be declared
    /// before the server is built.
    pub fn new(characteristic: C) -> Self {
        let uuid = characteristic.uuid();
        Self {
            characteristic,
            uuid,
            access: Access::default(),
            read: None,
            write: None,
            endpoint: None,
        }
    }

    /// Allow clients to read the value through [`Readable::read`].
    pub fn readable(mut self) -> Self
    where
        C: Readable,
    {
        self.access.read = true;
        self.read = Some(read_thunk::<C>);
        self
    }

    /// Advertise write requests, which the client sees acknowledged, handled
    /// by [`Writable::write`].
    ///
    /// NimBLE grants write access for both write requests and write commands
    /// once either is declared, and the handler cannot tell them apart. The
    /// two declarations therefore choose the advertised properties, not
    /// separately enforced permissions.
    pub fn writable(mut self) -> Self
    where
        C: Writable,
    {
        self.access.write = true;
        self.write = Some(write_thunk::<C>);
        self
    }

    /// Advertise write commands, which are not acknowledged, handled by
    /// [`Writable::write`]. The protocol carries no response to a command, so
    /// the client never sees an error the handler returns for one.
    ///
    /// As with [`writable`](Self::writable), NimBLE grants write access for
    /// both kinds of write once either is declared.
    pub fn writable_without_response(mut self) -> Self
    where
        C: Writable,
    {
        self.access.write_without_response = true;
        self.write = Some(write_thunk::<C>);
        self
    }

    /// Allow notifications, and return the endpoint that identifies this
    /// characteristic when sending them. Repeated calls return the same
    /// endpoint.
    pub fn notifiable(mut self) -> (Self, NotifyEndpoint<C::Value>)
    where
        C::Value: Encode,
    {
        self.access.notify = true;
        let id = self.endpoint.get_or_insert_with(EndpointId::new).clone();
        (
            self,
            NotifyEndpoint {
                id,
                value: PhantomData,
            },
        )
    }
}

impl<C: Characteristic> fmt::Debug for CharacteristicDef<C> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CharacteristicDef")
            .field("uuid", &self.uuid)
            .field("access", &self.access)
            .finish_non_exhaustive()
    }
}

/// A characteristic definition with its type erased, as stored in a frozen
/// server and dispatched by the registration code (a later ticket).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) trait RegisteredCharacteristic: Send + Sync {
    fn uuid(&self) -> Uuid;
    fn access(&self) -> Access;
    fn max_len(&self) -> usize;
    fn endpoint(&self) -> Option<&EndpointId>;
    /// Encode the current value into `output`. A notify-only characteristic
    /// has no read handler, but NimBLE's own notification paths
    /// (`ble_gatts_notify`, `ble_gatts_chr_updated`, and CCCD restore) read
    /// the value through the access callback. Notification sending must
    /// therefore always supply its payload explicitly
    /// (`ble_gatts_notify_custom`) or keep the last value to serve here.
    fn read(&self, output: &mut Vec<u8>) -> Result<(), AttError>;
    /// Validate, decode, and deliver a written value.
    fn write(&self, data: &[u8]) -> Result<(), AttError>;
}

impl<C: Characteristic> RegisteredCharacteristic for CharacteristicDef<C> {
    fn uuid(&self) -> Uuid {
        self.uuid
    }

    fn access(&self) -> Access {
        self.access
    }

    fn max_len(&self) -> usize {
        C::MAX_LEN
    }

    fn endpoint(&self) -> Option<&EndpointId> {
        self.endpoint.as_ref()
    }

    fn read(&self, output: &mut Vec<u8>) -> Result<(), AttError> {
        let read = self.read.ok_or(AttError::READ_NOT_PERMITTED)?;
        read(
            &self.characteristic,
            &mut ValueWriter::new(output, C::MAX_LEN),
        )
    }

    fn write(&self, data: &[u8]) -> Result<(), AttError> {
        let write = self.write.ok_or(AttError::WRITE_NOT_PERMITTED)?;
        if data.len() > C::MAX_LEN {
            return Err(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH);
        }
        write(&self.characteristic, data)
    }
}

/// A primary service and its characteristics, in declaration order.
pub struct Service {
    uuid: Uuid,
    characteristics: Vec<Box<dyn RegisteredCharacteristic>>,
}

impl Service {
    /// Start a primary service.
    pub fn primary(uuid: Uuid) -> Self {
        Self {
            uuid,
            characteristics: Vec::new(),
        }
    }

    /// Add a characteristic after those already added.
    pub fn characteristic<C: Characteristic>(mut self, definition: CharacteristicDef<C>) -> Self {
        self.characteristics.push(Box::new(definition));
        self
    }

    // Read by registration in a later ticket and by tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn uuid(&self) -> Uuid {
        self.uuid
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn characteristics(&self) -> &[Box<dyn RegisteredCharacteristic>] {
        &self.characteristics
    }
}

impl fmt::Debug for Service {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let characteristics: Vec<_> = self
            .characteristics
            .iter()
            .map(|characteristic| (characteristic.uuid(), characteristic.access()))
            .collect();
        formatter
            .debug_struct("Service")
            .field("uuid", &self.uuid)
            .field("characteristics", &characteristics)
            .finish()
    }
}

/// A validated, frozen set of services, ready to transfer to the BLE owner.
///
/// It is `Send + Sync`, owns every characteristic, and offers no way to change
/// its structure.
#[derive(Debug)]
pub struct GattServer {
    services: Box<[Service]>,
}

impl GattServer {
    /// Validate and freeze `services`, in order.
    ///
    /// Returns an [`ErrorKind::Definition`](crate::ErrorKind::Definition)
    /// error if there are no services, a characteristic declares no
    /// capability, or a characteristic's `MAX_LEN` exceeds
    /// [`MAX_ATTRIBUTE_VALUE_LEN`].
    pub fn new(services: impl IntoIterator<Item = Service>) -> Result<Self, Error> {
        let services: Box<[Service]> = services.into_iter().collect();
        if services.is_empty() {
            return Err(Error::definition(
                None,
                None,
                "a server needs at least one service",
            ));
        }
        for service in services.iter() {
            for characteristic in &service.characteristics {
                let located = |problem| {
                    Error::definition(Some(service.uuid), Some(characteristic.uuid()), problem)
                };
                if characteristic.access().is_empty() {
                    return Err(located("no read, write, or notify capability is declared"));
                }
                if characteristic.max_len() > MAX_ATTRIBUTE_VALUE_LEN {
                    return Err(located("MAX_LEN exceeds the 512-byte attribute limit"));
                }
            }
        }
        Ok(Self { services })
    }

    // Read by registration in a later ticket and by tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn services(&self) -> &[Service] {
        &self.services
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{Decode, DecodeError, EncodeError, ValueReader};
    use crate::ErrorKind;
    use std::sync::{Arc, Mutex, OnceLock};

    #[derive(Default)]
    struct Shared {
        level: Mutex<u8>,
        log: Mutex<Vec<String>>,
    }

    struct Level(Arc<Shared>);

    impl Characteristic for Level {
        type Value = u8;
        fn uuid(&self) -> Uuid {
            Uuid::Uuid16(0x2a19)
        }
    }

    impl Readable for Level {
        fn read(&self) -> Result<u8, AttError> {
            Ok(*self.0.level.lock().unwrap())
        }
    }

    impl Writable for Level {
        fn write(&self, value: u8) -> Result<(), AttError> {
            if value > 100 {
                return Err(AttError::VALUE_NOT_ALLOWED);
            }
            *self.0.level.lock().unwrap() = value;
            Ok(())
        }
    }

    /// A text characteristic limited to 8 bytes.
    struct Name(Arc<Shared>);

    impl Characteristic for Name {
        type Value = String;
        const MAX_LEN: usize = 8;
        fn uuid(&self) -> Uuid {
            Uuid::parse("6e400002-b5a3-f393-e0a9-e50e24dcca9e").unwrap()
        }
    }

    impl Readable for Name {
        fn read(&self) -> Result<String, AttError> {
            Ok(self.0.log.lock().unwrap().join(","))
        }
    }

    impl Writable for Name {
        fn write(&self, value: String) -> Result<(), AttError> {
            self.0.log.lock().unwrap().push(value);
            Ok(())
        }
    }

    /// A notify-only characteristic.
    struct Alert;

    impl Characteristic for Alert {
        type Value = [u8; 2];
        fn uuid(&self) -> Uuid {
            Uuid::Uuid32(0x0001_0002)
        }
    }

    /// A characteristic whose value refuses to encode.
    struct Broken;

    struct Unencodable;

    impl Encode for Unencodable {
        fn encode(&self, _: &mut ValueWriter<'_>) -> Result<(), EncodeError> {
            Err(EncodeError::InvalidValue { reason: "always" })
        }
    }

    impl Decode<'_> for Unencodable {
        fn decode(_: &mut ValueReader<'_>) -> Result<Self, DecodeError> {
            Err(DecodeError::InvalidValue { reason: "always" })
        }
    }

    impl Characteristic for Broken {
        type Value = Unencodable;
        fn uuid(&self) -> Uuid {
            Uuid::Uuid16(0xfff0)
        }
    }

    impl Readable for Broken {
        fn read(&self) -> Result<Unencodable, AttError> {
            Ok(Unencodable)
        }
    }

    impl Writable for Broken {
        fn write(&self, _: Unencodable) -> Result<(), AttError> {
            unreachable!("decoding always fails")
        }
    }

    struct TooLong;

    impl Characteristic for TooLong {
        type Value = Vec<u8>;
        const MAX_LEN: usize = MAX_ATTRIBUTE_VALUE_LEN + 1;
        fn uuid(&self) -> Uuid {
            Uuid::Uuid16(0xfff1)
        }
    }

    impl Readable for TooLong {
        fn read(&self) -> Result<Vec<u8>, AttError> {
            Ok(Vec::new())
        }
    }

    fn read(characteristic: &dyn RegisteredCharacteristic) -> Result<Vec<u8>, AttError> {
        let mut output = Vec::new();
        characteristic.read(&mut output).map(|()| output)
    }

    fn access(read: bool, write: bool, write_without_response: bool, notify: bool) -> Access {
        Access {
            read,
            write,
            write_without_response,
            notify,
        }
    }

    #[test]
    fn a_multi_service_hierarchy_keeps_order_uuids_and_derived_access() {
        let shared = Arc::new(Shared::default());
        let (level, level_endpoint) = CharacteristicDef::new(Level(shared.clone()))
            .readable()
            .writable()
            .notifiable();
        let (alert, alert_endpoint) = CharacteristicDef::new(Alert).notifiable();
        let battery = Service::primary(Uuid::Uuid16(0x180f))
            .characteristic(level)
            .characteristic(CharacteristicDef::new(Name(shared.clone())).readable());
        let custom = Service::primary(Uuid::Uuid128(0xabcd))
            .characteristic(
                CharacteristicDef::new(Name(shared.clone())).writable_without_response(),
            )
            .characteristic(alert)
            .characteristic(
                CharacteristicDef::new(Level(shared))
                    .writable()
                    .writable_without_response(),
            );
        let server = GattServer::new([battery, custom]).unwrap();

        let layout: Vec<_> = server
            .services()
            .iter()
            .map(|service| {
                (
                    service.uuid(),
                    service
                        .characteristics()
                        .iter()
                        .map(|c| (c.uuid(), c.access(), c.max_len(), c.endpoint().cloned()))
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        let name = Uuid::parse("6e400002-b5a3-f393-e0a9-e50e24dcca9e").unwrap();
        assert_eq!(
            layout,
            [
                (
                    Uuid::Uuid16(0x180f),
                    vec![
                        (
                            Uuid::Uuid16(0x2a19),
                            access(true, true, false, true),
                            512,
                            Some(level_endpoint.id().clone())
                        ),
                        (name, access(true, false, false, false), 8, None),
                    ]
                ),
                (
                    Uuid::Uuid128(0xabcd),
                    vec![
                        (name, access(false, false, true, false), 8, None),
                        (
                            Uuid::Uuid32(0x0001_0002),
                            access(false, false, false, true),
                            512,
                            Some(alert_endpoint.id().clone())
                        ),
                        (
                            Uuid::Uuid16(0x2a19),
                            access(false, true, true, false),
                            512,
                            None
                        ),
                    ]
                ),
            ]
        );
        let debug = format!("{server:?}");
        assert!(debug.contains("Uuid16(6159)") && debug.contains("notify: true"));
    }

    #[test]
    fn dispatch_calls_handlers_with_decoded_values_and_shared_state() {
        let shared = Arc::new(Shared::default());
        let level = CharacteristicDef::new(Level(shared.clone()))
            .readable()
            .writable();
        assert_eq!(read(&level), Ok(vec![0]));
        assert_eq!(level.write(&[42]), Ok(()));
        assert_eq!(*shared.level.lock().unwrap(), 42);
        assert_eq!(read(&level), Ok(vec![42]));

        // Handler and decode failures become ATT errors; state is unchanged.
        assert_eq!(level.write(&[101]), Err(AttError::VALUE_NOT_ALLOWED));
        assert_eq!(
            level.write(&[]),
            Err(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH)
        );
        assert_eq!(
            level.write(&[1, 2]),
            Err(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH)
        );
        assert_eq!(*shared.level.lock().unwrap(), 42);

        let name = CharacteristicDef::new(Name(shared.clone()))
            .readable()
            .writable();
        assert_eq!(name.write(b"ab"), Ok(()));
        assert_eq!(name.write(b"12345678"), Ok(()), "exactly MAX_LEN");
        assert_eq!(name.write(&[0xff]), Err(AttError::VALUE_NOT_ALLOWED));
        assert_eq!(read(&name), Err(AttError::UNLIKELY), "11 bytes > MAX_LEN");
        shared.log.lock().unwrap().truncate(1);
        assert_eq!(read(&name), Ok(b"ab".to_vec()));
    }

    #[test]
    fn oversized_writes_are_rejected_before_decoding() {
        let shared = Arc::new(Shared::default());
        let name = CharacteristicDef::new(Name(shared.clone())).writable();
        assert_eq!(
            name.write(b"123456789"),
            Err(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH)
        );
        assert!(shared.log.lock().unwrap().is_empty(), "handler not called");
    }

    #[test]
    fn undeclared_operations_are_refused_at_dispatch() {
        let shared = Arc::new(Shared::default());
        let write_only = CharacteristicDef::new(Level(shared.clone())).writable();
        assert_eq!(read(&write_only), Err(AttError::READ_NOT_PERMITTED));
        let read_only = CharacteristicDef::new(Level(shared.clone())).readable();
        assert_eq!(read_only.write(&[1]), Err(AttError::WRITE_NOT_PERMITTED));
        let (notify_only, _) = CharacteristicDef::new(Alert).notifiable();
        assert_eq!(read(&notify_only), Err(AttError::READ_NOT_PERMITTED));
        assert_eq!(
            notify_only.write(&[1, 2]),
            Err(AttError::WRITE_NOT_PERMITTED)
        );
        assert_eq!(*shared.level.lock().unwrap(), 0);
    }

    #[test]
    fn write_commands_reach_the_same_handler_and_checks() {
        let shared = Arc::new(Shared::default());
        let commands = CharacteristicDef::new(Level(shared.clone())).writable_without_response();
        assert_eq!(commands.write(&[9]), Ok(()));
        assert_eq!(*shared.level.lock().unwrap(), 9);
        assert_eq!(commands.write(&[200]), Err(AttError::VALUE_NOT_ALLOWED));
        assert_eq!(
            commands.write(&[1, 2]),
            Err(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH)
        );
        assert_eq!(read(&commands), Err(AttError::READ_NOT_PERMITTED));
        assert_eq!(*shared.level.lock().unwrap(), 9);
    }

    #[test]
    fn encode_and_decode_failures_map_to_att_errors() {
        let broken = CharacteristicDef::new(Broken).readable().writable();
        assert_eq!(read(&broken), Err(AttError::UNLIKELY));
        assert_eq!(broken.write(&[0]), Err(AttError::VALUE_NOT_ALLOWED));
        let mut output = vec![9];
        assert!(broken.read(&mut output).is_err());
        assert_eq!(output, [9], "a failed read leaves the output unchanged");
    }

    #[test]
    fn notify_endpoints_are_unique_typed_and_stable() {
        let (definition, first) = CharacteristicDef::new(Alert).notifiable();
        let (definition, again) = definition.notifiable();
        assert_eq!(first, again, "repeating the declaration keeps the endpoint");
        assert_eq!(definition.endpoint(), Some(first.id()));
        let (_, other) = CharacteristicDef::new(Alert).notifiable();
        assert_ne!(first, other);
        let clone = first.clone();
        assert_eq!(clone, first);
        drop(definition);
        assert_eq!(clone, first, "identity outlives the definition");
        assert_eq!(format!("{first:?}"), "NotifyEndpoint { .. }");
        let _: NotifyEndpoint<[u8; 2]> = first;
    }

    #[test]
    fn invalid_definitions_are_rejected_with_their_location() {
        let empty = GattServer::new([]).unwrap_err();
        assert_eq!(empty.kind(), ErrorKind::Definition);
        assert_eq!(
            empty.to_string(),
            "invalid GATT definition: a server needs at least one service"
        );

        let bare = Service::primary(Uuid::Uuid16(0x180f))
            .characteristic(CharacteristicDef::new(Level(Arc::default())));
        let error = GattServer::new([bare]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Definition);
        assert_eq!(
            error.to_string(),
            "invalid GATT definition: service 180f: characteristic 2a19: no read, write, or notify capability is declared"
        );

        let too_long = Service::primary(Uuid::Uuid16(0x1800))
            .characteristic(CharacteristicDef::new(TooLong).readable());
        let error = GattServer::new([too_long]).unwrap_err();
        assert!(error.to_string().contains("characteristic fff1: MAX_LEN"));

        // A service without characteristics is valid on its own.
        assert!(GattServer::new([Service::primary(Uuid::Uuid16(0x1801))]).is_ok());
    }

    #[test]
    fn the_server_transfers_to_another_thread() {
        fn assert_owned<T: Send + Sync + 'static>() {}
        assert_owned::<GattServer>();
        assert_owned::<NotifyEndpoint<String>>();

        let shared = Arc::new(Shared::default());
        let service = Service::primary(Uuid::Uuid16(0x180f))
            .characteristic(CharacteristicDef::new(Level(shared.clone())).writable());
        let server = GattServer::new([service]).unwrap();
        std::thread::spawn(move || {
            server.services()[0].characteristics()[0]
                .write(&[7])
                .unwrap()
        })
        .join()
        .unwrap();
        assert_eq!(*shared.level.lock().unwrap(), 7);
    }

    /// Reads another characteristic of the same server from its own handler.
    struct Mirror(Arc<OnceLock<GattServer>>);

    impl Characteristic for Mirror {
        type Value = Vec<u8>;
        fn uuid(&self) -> Uuid {
            Uuid::Uuid16(0xfff2)
        }
    }

    impl Readable for Mirror {
        fn read(&self) -> Result<Vec<u8>, AttError> {
            let server = self.0.get().ok_or(AttError::UNLIKELY)?;
            read(server.services()[0].characteristics()[0].as_ref())
        }
    }

    #[test]
    fn handlers_can_reenter_dispatch_because_no_framework_lock_is_held() {
        let cell = Arc::new(OnceLock::new());
        let shared = Arc::new(Shared::default());
        *shared.level.lock().unwrap() = 33;
        let service = Service::primary(Uuid::Uuid16(0x180f))
            .characteristic(CharacteristicDef::new(Level(shared)).readable())
            .characteristic(CharacteristicDef::new(Mirror(cell.clone())).readable());
        assert!(cell.set(GattServer::new([service]).unwrap()).is_ok());
        let server = cell.get().unwrap();
        assert_eq!(
            read(server.services()[0].characteristics()[1].as_ref()),
            Ok(vec![33])
        );
    }
}

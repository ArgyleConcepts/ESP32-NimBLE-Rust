//! Custom characteristic descriptors.

use super::GATT_DECLARATIONS;
use crate::codec::{decode_value, DecodeOwned, Encode, ValueWriter, MAX_ATTRIBUTE_VALUE_LEN};
use crate::{AttError, Error, Uuid};
use std::fmt;

/// The Client Characteristic Configuration descriptor (CCCD). NimBLE adds
/// and manages it for every notify-capable characteristic.
const CCCD: u128 = Uuid::Uuid16(0x2902).to_u128();
const EXTENDED_PROPERTIES: u128 = Uuid::Uuid16(0x2900).to_u128();
const SERVER_CONFIGURATION: u128 = Uuid::Uuid16(0x2903).to_u128();
const AGGREGATE_FORMAT: u128 = Uuid::Uuid16(0x2905).to_u128();
const USER_DESCRIPTION: u128 = Uuid::Uuid16(0x2901).to_u128();
const PRESENTATION_FORMAT: u128 = Uuid::Uuid16(0x2904).to_u128();

/// An application descriptor: its UUID and the type of its value.
///
/// Descriptors have their own traits, separate from [`Characteristic`]
/// and its handlers, so a type's descriptor and characteristic roles, access,
/// and handlers cannot be mixed up. Implement [`ReadableDescriptor`] and/or
/// [`WritableDescriptor`], then attach a [`DescriptorDef`] to a characteristic
/// with [`CharacteristicDef::descriptor`].
///
/// [`Characteristic`]: super::Characteristic
/// [`CharacteristicDef::descriptor`]: super::CharacteristicDef::descriptor
pub trait Descriptor: Send + Sync + 'static {
    /// The value read or written.
    type Value;

    /// The largest encoded value, in bytes, this descriptor produces or
    /// accepts. It must not exceed [`MAX_ATTRIBUTE_VALUE_LEN`], the default.
    /// Reads up to this length work at every ATT MTU; writes longer than one
    /// packet need a GATT long write, which NimBLE's buffers limit. See
    /// [long values](crate::gatt#long-values-and-offsets).
    const MAX_LEN: usize = MAX_ATTRIBUTE_VALUE_LEN;

    /// The descriptor UUID. It is read once, by [`DescriptorDef::new`], which
    /// rejects UUIDs the stack reserves.
    fn uuid(&self) -> Uuid;
}

/// Handles reads of a descriptor's value.
pub trait ReadableDescriptor: Descriptor<Value: Encode> {
    /// Return the current value, or the ATT error to send to the client.
    /// One client read of a long value may call this more than once; see
    /// [long values](crate::gatt#long-values-and-offsets).
    fn read(&self) -> Result<Self::Value, AttError>;
}

/// Handles writes of a descriptor's value.
pub trait WritableDescriptor: Descriptor<Value: DecodeOwned> {
    /// Accept a written value, already decoded, or return the ATT error to
    /// send to the client. The value is always complete: NimBLE reassembles
    /// a long write before it reaches this handler.
    fn write(&self, value: Self::Value) -> Result<(), AttError>;
}

/// The access a descriptor declares. NimBLE takes descriptor permissions
/// directly, separately from the characteristic's properties.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct DescriptorAccess {
    pub(crate) read: bool,
    pub(crate) write: bool,
}

impl DescriptorAccess {
    pub(super) fn is_empty(self) -> bool {
        self == Self::default()
    }
}

type ReadThunk<D> = fn(&D, &mut ValueWriter<'_>) -> Result<(), AttError>;
type WriteThunk<D> = fn(&D, &[u8]) -> Result<(), AttError>;

fn read_thunk<D: ReadableDescriptor>(
    descriptor: &D,
    writer: &mut ValueWriter<'_>,
) -> Result<(), AttError> {
    let value = descriptor.read()?;
    writer.write(&value).map_err(AttError::from)
}

fn write_thunk<D: WritableDescriptor>(descriptor: &D, data: &[u8]) -> Result<(), AttError> {
    let value = decode_value::<D::Value>(data)?;
    descriptor.write(value)
}

/// Why a descriptor UUID is reserved, if it is.
///
/// Besides the CCCD and declaration types, descriptors whose values describe
/// state this crate owns are reserved, since an application value could
/// contradict it:
///
/// - Characteristic Extended Properties (`0x2900`) mirrors property bits
///   (reliable and auxiliary writes) that are derived from the declared
///   capabilities and never set.
/// - Server Characteristic Configuration (`0x2903`) configures broadcast,
///   which is not supported.
/// - Characteristic Aggregate Format (`0x2905`) lists attribute handles,
///   which applications never see.
///
/// User Description (`0x2901`), Presentation Format (`0x2904`), and other
/// descriptors are application-defined; [`write_restriction`] keeps the first
/// two read-only.
fn reserved(uuid: Uuid) -> Option<&'static str> {
    let value = uuid.to_u128();
    if value == CCCD {
        Some("the Client Characteristic Configuration descriptor (0x2902) is managed by the stack for notify-capable characteristics")
    } else if GATT_DECLARATIONS.contains(&value) {
        Some("the UUID is a GATT declaration type")
    } else if value == EXTENDED_PROPERTIES {
        Some("the Characteristic Extended Properties descriptor (0x2900) is derived from declared capabilities")
    } else if value == SERVER_CONFIGURATION {
        Some("the Server Characteristic Configuration descriptor (0x2903) requires broadcast, which is not supported")
    } else if value == AGGREGATE_FORMAT {
        Some("the Characteristic Aggregate Format descriptor (0x2905) lists attribute handles, which applications cannot see")
    } else {
        None
    }
}

/// Why a descriptor type cannot be writable, if it cannot.
pub(super) fn write_restriction(uuid: Uuid) -> Option<&'static str> {
    let value = uuid.to_u128();
    if value == USER_DESCRIPTION {
        Some("the User Description descriptor (0x2901) is read-only: writing it requires the Writable Auxiliaries extended property, which is not supported")
    } else if value == PRESENTATION_FORMAT {
        Some("the Characteristic Presentation Format descriptor (0x2904) is read-only")
    } else {
        None
    }
}

/// A descriptor with its declared access.
///
/// Each access method requires the matching handler trait, so undeclarable
/// access fails to compile. Repeating a declaration has no further effect.
///
/// ```
/// use argyle_nimble::gatt::{
///     Characteristic, CharacteristicDef, Descriptor, DescriptorDef, GattServer, Readable,
///     ReadableDescriptor, Service,
/// };
/// use argyle_nimble::{AttError, Uuid};
///
/// struct Level;
///
/// impl Characteristic for Level {
///     type Value = u8;
///     fn uuid(&self) -> Uuid {
///         Uuid::Uuid16(0x2a19)
///     }
/// }
///
/// impl Readable for Level {
///     fn read(&self) -> Result<u8, AttError> {
///         Ok(80)
///     }
/// }
///
/// /// A User Description, which is read-only.
/// struct Label;
///
/// impl Descriptor for Label {
///     type Value = String;
///     fn uuid(&self) -> Uuid {
///         Uuid::Uuid16(0x2901)
///     }
/// }
///
/// impl ReadableDescriptor for Label {
///     fn read(&self) -> Result<String, AttError> {
///         Ok("main battery".into())
///     }
/// }
///
/// let level = CharacteristicDef::new(Level)
///     .readable()
///     .descriptor(DescriptorDef::new(Label)?.readable());
/// let server = GattServer::new([Service::primary(Uuid::Uuid16(0x180f)).characteristic(level)])?;
/// # let _ = server;
///
/// // The stack manages the CCCD, so a manual one is rejected.
/// struct Subscriptions;
/// impl Descriptor for Subscriptions {
///     type Value = u16;
///     fn uuid(&self) -> Uuid {
///         Uuid::Uuid16(0x2902)
///     }
/// }
/// assert!(DescriptorDef::new(Subscriptions).is_err());
/// # Ok::<(), argyle_nimble::Error>(())
/// ```
pub struct DescriptorDef<D: Descriptor> {
    // Called through `RegisteredDescriptor` by the registration access path
    // and by tests.
    #[cfg_attr(not(test), allow(dead_code))]
    descriptor: D,
    uuid: Uuid,
    access: DescriptorAccess,
    read: Option<ReadThunk<D>>,
    write: Option<WriteThunk<D>>,
}

impl<D: Descriptor> DescriptorDef<D> {
    /// Start a definition with no access; at least one kind must be declared
    /// before the server is built.
    ///
    /// Returns an [`ErrorKind::Definition`](crate::ErrorKind::Definition)
    /// error if the UUID is reserved: the Client Characteristic Configuration
    /// descriptor (`0x2902`), which NimBLE manages for notify-capable
    /// characteristics; a GATT declaration type (`0x2800`–`0x2803`); or a
    /// descriptor whose value would describe state this crate controls
    /// (`0x2900` Extended Properties, `0x2903` Server Characteristic
    /// Configuration, `0x2905` Aggregate Format). Every UUID width is checked
    /// by its 128-bit form. UUIDs are runtime values, so this is the earliest
    /// point the check can run.
    pub fn new(descriptor: D) -> Result<Self, Error> {
        let uuid = descriptor.uuid();
        if let Some(problem) = reserved(uuid) {
            return Err(Error::definition(None, None, Some(uuid), problem));
        }
        Ok(Self {
            descriptor,
            uuid,
            access: DescriptorAccess::default(),
            read: None,
            write: None,
        })
    }

    /// Allow clients to read the value through [`ReadableDescriptor::read`].
    pub fn readable(mut self) -> Self
    where
        D: ReadableDescriptor,
    {
        self.access.read = true;
        self.read = Some(read_thunk::<D>);
        self
    }

    /// Allow clients to write the value through [`WritableDescriptor::write`].
    pub fn writable(mut self) -> Self
    where
        D: WritableDescriptor,
    {
        self.access.write = true;
        self.write = Some(write_thunk::<D>);
        self
    }
}

impl<D: Descriptor> fmt::Debug for DescriptorDef<D> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DescriptorDef")
            .field("uuid", &self.uuid)
            .field("access", &self.access)
            .finish_non_exhaustive()
    }
}

/// A descriptor definition with its type erased, stored under its parent
/// characteristic and dispatched by `gatt::registration`.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) trait RegisteredDescriptor: Send + Sync {
    fn uuid(&self) -> Uuid;
    fn access(&self) -> DescriptorAccess;
    fn max_len(&self) -> usize;
    /// Encode the current value into `output`.
    fn read(&self, output: &mut Vec<u8>) -> Result<(), AttError>;
    /// Validate, decode, and deliver a written value.
    fn write(&self, data: &[u8]) -> Result<(), AttError>;
}

impl<D: Descriptor> RegisteredDescriptor for DescriptorDef<D> {
    fn uuid(&self) -> Uuid {
        self.uuid
    }

    fn access(&self) -> DescriptorAccess {
        self.access
    }

    fn max_len(&self) -> usize {
        D::MAX_LEN
    }

    fn read(&self, output: &mut Vec<u8>) -> Result<(), AttError> {
        let read = self.read.ok_or(AttError::READ_NOT_PERMITTED)?;
        read(&self.descriptor, &mut ValueWriter::new(output, D::MAX_LEN))
    }

    fn write(&self, data: &[u8]) -> Result<(), AttError> {
        let write = self.write.ok_or(AttError::WRITE_NOT_PERMITTED)?;
        if data.len() > D::MAX_LEN {
            return Err(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH);
        }
        write(&self.descriptor, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;
    use std::sync::{Arc, Mutex};

    /// A descriptor whose UUID is chosen at runtime.
    struct Runtime(Uuid);

    impl Descriptor for Runtime {
        type Value = u8;
        fn uuid(&self) -> Uuid {
            self.0
        }
    }

    impl ReadableDescriptor for Runtime {
        fn read(&self) -> Result<u8, AttError> {
            Ok(1)
        }
    }

    /// A writable text descriptor limited to 6 bytes.
    struct Label(Arc<Mutex<String>>);

    impl Descriptor for Label {
        type Value = String;
        const MAX_LEN: usize = 6;
        fn uuid(&self) -> Uuid {
            Uuid::parse("9b2a1c4e-7d3f-4e8a-b5c6-2f1d0e9a8b7c").unwrap()
        }
    }

    impl ReadableDescriptor for Label {
        fn read(&self) -> Result<String, AttError> {
            Ok(self.0.lock().unwrap().clone())
        }
    }

    impl WritableDescriptor for Label {
        fn write(&self, value: String) -> Result<(), AttError> {
            if value.is_empty() {
                return Err(AttError::VALUE_NOT_ALLOWED);
            }
            *self.0.lock().unwrap() = value;
            Ok(())
        }
    }

    fn read(descriptor: &dyn RegisteredDescriptor) -> Result<Vec<u8>, AttError> {
        let mut output = Vec::new();
        descriptor.read(&mut output).map(|()| output)
    }

    #[test]
    fn the_cccd_and_declaration_types_are_rejected_in_every_width() {
        let cccd = "00002902-0000-1000-8000-00805f9b34fb";
        let reserved = [
            (Uuid::Uuid16(0x2902), "Client Characteristic Configuration"),
            (Uuid::Uuid32(0x2902), "Client Characteristic Configuration"),
            (
                Uuid::parse(cccd).unwrap(),
                "Client Characteristic Configuration",
            ),
            (Uuid::Uuid16(0x2800), "GATT declaration type"),
            (Uuid::Uuid16(0x2801), "GATT declaration type"),
            (Uuid::Uuid32(0x2802), "GATT declaration type"),
            (
                Uuid::Uuid128(Uuid::Uuid16(0x2803).to_u128()),
                "GATT declaration type",
            ),
            (Uuid::Uuid16(0x2900), "Extended Properties"),
            (Uuid::Uuid32(0x2903), "Server Characteristic Configuration"),
            (
                Uuid::Uuid128(Uuid::Uuid16(0x2905).to_u128()),
                "Aggregate Format",
            ),
        ];
        for (uuid, reason) in reserved {
            let error = DescriptorDef::new(Runtime(uuid)).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Definition);
            let message = error.to_string();
            assert!(
                message.contains(&format!("descriptor {uuid}: ")),
                "{message}"
            );
            assert!(message.contains(reason), "{message}");
        }
    }

    #[test]
    fn custom_and_standard_descriptors_are_accepted() {
        for uuid in [
            Uuid::Uuid16(0x2901),
            Uuid::Uuid16(0x2904),
            Uuid::Uuid16(0x2906),
            Uuid::Uuid16(0x2804),
            Uuid::Uuid32(0x0001_2902),
            Uuid::parse("00002902-0000-1000-8000-00805f9b34fc").unwrap(),
            Uuid::Uuid128(0x2902),
        ] {
            let definition = DescriptorDef::new(Runtime(uuid)).unwrap().readable();
            assert_eq!(definition.uuid(), uuid);
        }
    }

    #[test]
    fn descriptor_requests_follow_the_characteristic_contract() {
        let label = Arc::new(Mutex::new(String::from("fan")));
        let both = DescriptorDef::new(Label(label.clone()))
            .unwrap()
            .readable()
            .writable();
        assert_eq!(
            both.access(),
            DescriptorAccess {
                read: true,
                write: true
            }
        );
        assert_eq!(both.max_len(), 6);
        assert_eq!(read(&both), Ok(b"fan".to_vec()));
        assert_eq!(both.write(b"pump"), Ok(()));
        assert_eq!(read(&both), Ok(b"pump".to_vec()));

        // Length, decode, and handler failures become ATT errors.
        assert_eq!(
            both.write(b"1234567"),
            Err(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH)
        );
        assert_eq!(both.write(&[0xff]), Err(AttError::VALUE_NOT_ALLOWED));
        assert_eq!(both.write(b""), Err(AttError::VALUE_NOT_ALLOWED));
        *label.lock().unwrap() = String::from("toolong");
        assert_eq!(read(&both), Err(AttError::UNLIKELY));
        *label.lock().unwrap() = String::from("pump");

        let read_only = DescriptorDef::new(Label(label.clone())).unwrap().readable();
        assert_eq!(read_only.write(b"x"), Err(AttError::WRITE_NOT_PERMITTED));
        let write_only = DescriptorDef::new(Label(label.clone())).unwrap().writable();
        assert_eq!(read(&write_only), Err(AttError::READ_NOT_PERMITTED));
        assert_eq!(
            write_only.access(),
            DescriptorAccess {
                read: false,
                write: true
            }
        );
        let bare = DescriptorDef::new(Label(label.clone())).unwrap();
        assert!(bare.access().is_empty());
        assert_eq!(*label.lock().unwrap(), "pump");
        assert!(format!("{bare:?}").contains("Uuid128("));
    }

    /// Reports a safe UUID the first time and the CCCD afterwards.
    struct Shifty(std::sync::atomic::AtomicBool);

    impl Descriptor for Shifty {
        type Value = u8;
        fn uuid(&self) -> Uuid {
            if self.0.swap(true, std::sync::atomic::Ordering::Relaxed) {
                Uuid::Uuid16(0x2902)
            } else {
                Uuid::Uuid16(0xff10)
            }
        }
    }

    impl ReadableDescriptor for Shifty {
        fn read(&self) -> Result<u8, AttError> {
            Ok(0)
        }
    }

    #[test]
    fn the_uuid_is_read_once_so_a_later_value_cannot_bypass_the_check() {
        let definition = DescriptorDef::new(Shifty(Default::default()))
            .unwrap()
            .readable();
        assert_eq!(definition.descriptor.uuid(), Uuid::Uuid16(0x2902));
        assert_eq!(
            RegisteredDescriptor::uuid(&definition),
            Uuid::Uuid16(0xff10)
        );
    }
}

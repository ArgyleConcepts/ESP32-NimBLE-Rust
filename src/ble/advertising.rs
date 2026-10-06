//! Legacy advertising payloads: validation, placement, and encoding. The
//! public contract is documented on [`Advertising`].

use crate::gatt::GattServer;
use crate::{Error, Uuid};
use std::ffi::CString;
use std::fmt;

/// The largest advertising data or scan response payload of legacy
/// advertising, in bytes (Core Specification Vol 6, Part B, 2.3.1.1 and
/// 2.3.2.2).
pub const LEGACY_PAYLOAD_CAPACITY: usize = 31;

/// The largest GAP Device Name, in bytes (Core Specification Vol 3, Part C,
/// 12.1).
pub const MAX_DEVICE_NAME_LEN: usize = 248;

/// AD types from the Bluetooth Assigned Numbers. ESP builds check them
/// against the SDK's definitions.
pub(crate) mod ad {
    pub(crate) const FLAGS: u8 = 0x01;
    pub(crate) const INCOMPLETE_UUIDS16: u8 = 0x02;
    pub(crate) const COMPLETE_UUIDS16: u8 = 0x03;
    pub(crate) const INCOMPLETE_UUIDS128: u8 = 0x06;
    pub(crate) const COMPLETE_UUIDS128: u8 = 0x07;
    pub(crate) const SHORTENED_NAME: u8 = 0x08;
    pub(crate) const COMPLETE_NAME: u8 = 0x09;
    /// Flags: LE General Discoverable Mode (bit 1) and BR/EDR Not Supported
    /// (bit 2), Core Specification Supplement, Part A, 1.3.
    pub(crate) const GENERAL_DISCOVERABLE: u8 = 0x02;
    pub(crate) const BREDR_UNSUPPORTED: u8 = 0x04;
}

/// The length and type bytes before each AD structure's data.
const AD_HEADER_LEN: usize = 2;
/// The Flags structure: header and one flags byte.
const FLAGS_LEN: usize = AD_HEADER_LEN + 1;
/// The longest local name the scan response can hold.
const NAME_CAPACITY: usize = LEGACY_PAYLOAD_CAPACITY - AD_HEADER_LEN;

/// Why an advertising configuration was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AdvertisingError {
    /// The name is empty.
    EmptyName,
    /// The name contains a NUL byte, which the GAP Device Name cannot carry
    /// through NimBLE.
    NameContainsNul,
    /// The name is longer than [`MAX_DEVICE_NAME_LEN`].
    NameTooLong {
        /// The name's length in bytes.
        length: usize,
    },
    /// The name does not fit the scan response: as a Complete Local Name,
    /// or, when shortening is allowed, as a Shortened Local Name of at least
    /// `minimum` bytes.
    NameDoesNotFit {
        /// The name's length in bytes.
        length: usize,
        /// The longest name the scan response holds.
        capacity: usize,
        /// The shortest shortened name allowed, when shortening is allowed.
        minimum: Option<usize>,
    },
    /// [`AdvertisingBuilder::shortened_name`] was given a minimum of zero; a
    /// Shortened Local Name cannot be empty.
    ZeroShortenedLength,
    /// The service UUID is listed more than once.
    DuplicateService(Uuid),
    /// The Flags structure and the service UUID lists need `length` bytes,
    /// more than the advertising data holds.
    ServicesDoNotFit {
        /// The bytes the advertising data would need.
        length: usize,
        /// The advertising data's capacity, [`LEGACY_PAYLOAD_CAPACITY`].
        capacity: usize,
    },
    /// The service UUID is not a primary service of the GATT server being
    /// started.
    UnknownService(Uuid),
}

impl fmt::Display for AdvertisingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::EmptyName => formatter.write_str("the name is empty"),
            Self::NameContainsNul => formatter.write_str("the name contains a NUL byte"),
            Self::NameTooLong { length } => write!(
                formatter,
                "the name is {length} bytes, longer than the {MAX_DEVICE_NAME_LEN}-byte GAP Device Name limit"
            ),
            Self::NameDoesNotFit {
                length,
                capacity,
                minimum: None,
            } => write!(
                formatter,
                "the {length}-byte name does not fit the scan response, which holds a name of at most {capacity} bytes, and shortening is not allowed"
            ),
            Self::NameDoesNotFit {
                length,
                capacity,
                minimum: Some(minimum),
            } => write!(
                formatter,
                "the {length}-byte name does not fit the scan response, which holds a name of at most {capacity} bytes, even shortened to the {minimum}-byte minimum"
            ),
            Self::ZeroShortenedLength => {
                formatter.write_str("a shortened name needs a minimum length of at least 1 byte")
            }
            Self::DuplicateService(uuid) => {
                write!(formatter, "service {uuid} is listed more than once")
            }
            Self::ServicesDoNotFit { length, capacity } => write!(
                formatter,
                "the flags and service UUIDs need {length} bytes, more than the {capacity}-byte advertising data holds"
            ),
            Self::UnknownService(uuid) => write!(
                formatter,
                "service {uuid} is advertised but is not a primary service of the GATT server"
            ),
        }
    }
}

impl std::error::Error for AdvertisingError {}

/// The local name as advertised in the scan response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalName<'a> {
    /// The whole name, as a Complete Local Name.
    Complete(&'a str),
    /// A prefix of the name, as a Shortened Local Name.
    Shortened(&'a str),
}

/// Collects an advertising configuration; [`build`](Self::build) checks it.
#[derive(Clone, Debug)]
pub struct AdvertisingBuilder {
    name: Option<String>,
    shortened_minimum: Option<usize>,
    services: Vec<Uuid>,
    remain_available: bool,
}

impl AdvertisingBuilder {
    /// Set the device name: the GAP Device Name and the advertised local
    /// name. A later call replaces it.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Allow a Shortened Local Name of at least `minimum` bytes when the
    /// whole name does not fit the scan response. Without this call such a
    /// name is an error; the name is never shortened silently. The GAP
    /// Device Name always keeps the whole name. Has no effect without a
    /// name.
    pub fn shortened_name(mut self, minimum: usize) -> Self {
        self.shortened_minimum = Some(minimum);
        self
    }

    /// Advertise a primary service UUID, after those already added.
    pub fn service(mut self, uuid: Uuid) -> Self {
        self.services.push(uuid);
        self
    }

    /// Whether advertising restarts by itself after a client disconnects,
    /// after a connection attempt fails, after advertising stops without a
    /// connection (for example when NimBLE preempts it), and after the host
    /// resynchronizes following a reset. The default is `true`.
    ///
    /// With `false`, advertising starts once when the host starts, and again
    /// only when the application calls
    /// [`Ble::start_advertising`](crate::Ble::start_advertising).
    pub fn remain_available(mut self, remain_available: bool) -> Self {
        self.remain_available = remain_available;
        self
    }

    /// Check the configuration; see [`Advertising`] for the rules.
    pub fn build(self) -> Result<Advertising, Error> {
        if self.shortened_minimum == Some(0) {
            return Err(AdvertisingError::ZeroShortenedLength.into());
        }
        let local_name = match &self.name {
            None => None,
            Some(name) => Some(local_name(name, self.shortened_minimum)?),
        };

        let mut advertised: Vec<Uuid> = Vec::with_capacity(self.services.len());
        for uuid in &self.services {
            let form = uuid.att_form();
            if advertised.contains(&form) {
                return Err(AdvertisingError::DuplicateService(*uuid).into());
            }
            advertised.push(form);
        }
        let length = advertising_data_len(&advertised);
        if length > LEGACY_PAYLOAD_CAPACITY {
            return Err(AdvertisingError::ServicesDoNotFit {
                length,
                capacity: LEGACY_PAYLOAD_CAPACITY,
            }
            .into());
        }

        Ok(Advertising {
            name: self.name,
            local_name,
            services: self.services,
            advertised,
            remain_available: self.remain_available,
        })
    }
}

/// The advertised name: its length in bytes and whether it is shortened.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NamePlacement {
    length: usize,
    shortened: bool,
}

fn local_name(name: &str, shortened_minimum: Option<usize>) -> Result<NamePlacement, Error> {
    if name.is_empty() {
        return Err(AdvertisingError::EmptyName.into());
    }
    if name.as_bytes().contains(&0) {
        return Err(AdvertisingError::NameContainsNul.into());
    }
    if name.len() > MAX_DEVICE_NAME_LEN {
        return Err(AdvertisingError::NameTooLong { length: name.len() }.into());
    }
    if name.len() <= NAME_CAPACITY {
        return Ok(NamePlacement {
            length: name.len(),
            shortened: false,
        });
    }
    let does_not_fit = AdvertisingError::NameDoesNotFit {
        length: name.len(),
        capacity: NAME_CAPACITY,
        minimum: shortened_minimum,
    };
    let minimum = shortened_minimum.ok_or(does_not_fit)?;
    // A shortened name is a prefix of the whole name and stays valid UTF-8.
    let mut length = NAME_CAPACITY;
    while !name.is_char_boundary(length) {
        length -= 1;
    }
    if length < minimum {
        return Err(does_not_fit.into());
    }
    Ok(NamePlacement {
        length,
        shortened: true,
    })
}

fn advertising_data_len(advertised: &[Uuid]) -> usize {
    let list = |width: usize| {
        let count = advertised
            .iter()
            .filter(|uuid| uuid.byte_len() == width)
            .count();
        if count == 0 {
            0
        } else {
            AD_HEADER_LEN + count * width
        }
    };
    FLAGS_LEN + list(2) + list(16)
}

/// A checked advertising configuration, ready for
/// [`Ble::advertise`](crate::Ble::advertise).
///
/// It describes what the peripheral advertises while it waits for a client.
/// Build and check one with [`Advertising::builder`].
///
/// # Placement
///
/// Legacy advertising sends two packets of at most
/// [`LEGACY_PAYLOAD_CAPACITY`] (31) bytes each: the advertising data, which
/// every scanner receives, and the scan response, which active scanners
/// request. Each holds a sequence of AD structures: a length byte, an AD type,
/// and the data (Core Specification Vol 3, Part C, Section 11; AD types from
/// the Core Specification Supplement, Part A). The placement is fixed:
///
/// - **Advertising data:** the Flags structure (LE General Discoverable,
///   BR/EDR Not Supported) and then the service UUID lists: 16-bit UUIDs
///   first, then 128-bit UUIDs, each in configuration order. UUIDs are
///   advertised in the form ATT carries them (see [`Uuid`]): a 32-bit UUID,
///   or a 128-bit UUID over the Bluetooth Base UUID that has a 16-bit form,
///   is listed in that form.
/// - **Scan response:** the local name, if one is configured.
///
/// Nothing is moved between packets to make it fit; a configuration that
/// does not fit fails instead.
///
/// # Validation
///
/// [`AdvertisingBuilder::build`] rejects, with an
/// [`ErrorKind::Advertising`](crate::ErrorKind::Advertising) error whose
/// [`AdvertisingError`] says which rule failed:
///
/// - an empty name, a name containing a NUL byte, or a name longer than the
///   248-byte GAP Device Name limit (Core Specification Vol 3, Part C, 12.1);
/// - a name that does not fit the scan response as a Complete Local Name,
///   unless [`AdvertisingBuilder::shortened_name`] allows a Shortened Local
///   Name; a shortened name is a prefix of the name, cut at a UTF-8 character
///   boundary, and must keep at least the requested number of bytes;
/// - a service UUID listed twice (compared in advertised form), or service
///   UUIDs that do not fit the advertising data with the Flags structure.
///
/// When the host starts, [`Ble::start`](crate::Ble::start) also checks the
/// configuration against the GATT server before any native call: every
/// advertised service UUID must be a primary service of that server. A list
/// uses the Complete List AD type when it names every primary service of the
/// application's server with that UUID width, and the Incomplete List type
/// otherwise. Completeness is relative to the application's services: the
/// GAP (`0x1800`) and GATT (`0x1801`) services NimBLE adds itself are never
/// advertised and do not make a list incomplete.
///
/// The configured name is also set as the GAP Device Name characteristic when
/// the host starts. NimBLE limits that value to
/// `CONFIG_BT_NIMBLE_GAP_DEVICE_NAME_MAX_LEN` bytes (31 by default); a longer
/// name fails startup at [`StartStage::DeviceName`](crate::StartStage::DeviceName).
/// Without a configured name, no name is advertised and the GAP Device Name
/// is not set; it keeps NimBLE's current value. In ESP-IDF 6.1 that is
/// `CONFIG_BT_NIMBLE_SVC_GAP_DEVICE_NAME` after each host initialization when
/// `CONFIG_BT_NIMBLE_STATIC_TO_DYNAMIC` is enabled (the default; deinitializing
/// the host frees the name). With it disabled, the name is a static buffer
/// that deinitialization does not reset, so a name configured in an earlier
/// start of the host stays in effect for a later start without one.
///
/// ```
/// use argyle_nimble::{Advertising, LocalName, Uuid};
///
/// let advertising = Advertising::builder()
///     .name("argyle-demo")
///     .service(Uuid::Uuid16(0x180f))
///     .build()?;
/// assert_eq!(advertising.local_name(), Some(LocalName::Complete("argyle-demo")));
///
/// // A name too long for the scan response fails unless shortening is allowed.
/// let long = "argyle-demo-with-a-long-descriptive-name";
/// assert!(Advertising::builder().name(long).build().is_err());
/// let shortened = Advertising::builder().name(long).shortened_name(8).build()?;
/// assert_eq!(
///     shortened.local_name(),
///     Some(LocalName::Shortened("argyle-demo-with-a-long-descr"))
/// );
/// # Ok::<(), argyle_nimble::Error>(())
/// ```
#[derive(Clone, Debug)]
pub struct Advertising {
    name: Option<String>,
    local_name: Option<NamePlacement>,
    services: Vec<Uuid>,
    /// `services` in advertised (ATT) form, in the same order.
    advertised: Vec<Uuid>,
    remain_available: bool,
}

impl Advertising {
    /// Start a configuration: flags only, no name, no services, and
    /// advertising that restarts by itself.
    pub fn builder() -> AdvertisingBuilder {
        AdvertisingBuilder {
            name: None,
            shortened_minimum: None,
            services: Vec::new(),
            remain_available: true,
        }
    }

    /// The device name: the GAP Device Name the host is given.
    pub fn device_name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// The local name as the scan response carries it.
    pub fn local_name(&self) -> Option<LocalName<'_>> {
        let name = self.name.as_deref()?;
        let placement = self.local_name?;
        let advertised = &name[..placement.length];
        Some(if placement.shortened {
            LocalName::Shortened(advertised)
        } else {
            LocalName::Complete(advertised)
        })
    }

    /// The advertised service UUIDs, as configured.
    pub fn services(&self) -> &[Uuid] {
        &self.services
    }

    /// Whether advertising restarts by itself; see
    /// [`AdvertisingBuilder::remain_available`].
    pub fn remains_available(&self) -> bool {
        self.remain_available
    }

    /// Encode the packets for `server`, checking that every advertised
    /// service is one of its primary services. No native call is involved.
    pub(crate) fn plan(&self, server: &GattServer) -> Result<AdvertisingPlan, AdvertisingError> {
        let primary: Vec<Uuid> = server
            .services()
            .iter()
            .map(|service| service.uuid().att_form())
            .collect();
        if let Some(index) = self
            .advertised
            .iter()
            .position(|uuid| !primary.contains(uuid))
        {
            return Err(AdvertisingError::UnknownService(self.services[index]));
        }

        let mut fields = AdvertisingFields {
            flags: ad::GENERAL_DISCOVERABLE | ad::BREDR_UNSUPPORTED,
            uuids16: Vec::new(),
            uuids16_complete: false,
            uuids128: Vec::new(),
            uuids128_complete: false,
        };
        let mut advertising_data = vec![(FLAGS_LEN - 1) as u8, ad::FLAGS, fields.flags];
        for (width, complete_type, incomplete_type) in [
            (2, ad::COMPLETE_UUIDS16, ad::INCOMPLETE_UUIDS16),
            (16, ad::COMPLETE_UUIDS128, ad::INCOMPLETE_UUIDS128),
        ] {
            let listed: Vec<&Uuid> = self
                .advertised
                .iter()
                .filter(|uuid| uuid.byte_len() == width)
                .collect();
            if listed.is_empty() {
                continue;
            }
            let complete = primary
                .iter()
                .filter(|uuid| uuid.byte_len() == width)
                .all(|uuid| self.advertised.contains(uuid));
            advertising_data.push((1 + listed.len() * width) as u8);
            advertising_data.push(if complete {
                complete_type
            } else {
                incomplete_type
            });
            for uuid in listed {
                advertising_data.extend_from_slice(uuid.to_wire_bytes().as_ref());
                match *uuid {
                    Uuid::Uuid16(value) => fields.uuids16.push(value),
                    _ => fields.uuids128.push(uuid.to_u128()),
                }
            }
            if width == 2 {
                fields.uuids16_complete = complete;
            } else {
                fields.uuids128_complete = complete;
            }
        }
        debug_assert!(advertising_data.len() <= LEGACY_PAYLOAD_CAPACITY);

        let mut scan_response = Vec::new();
        if let Some(name) = self.local_name() {
            let (name_type, text) = match name {
                LocalName::Complete(text) => (ad::COMPLETE_NAME, text),
                LocalName::Shortened(text) => (ad::SHORTENED_NAME, text),
            };
            scan_response.push((1 + text.len()) as u8);
            scan_response.push(name_type);
            scan_response.extend_from_slice(text.as_bytes());
        }
        debug_assert!(scan_response.len() <= LEGACY_PAYLOAD_CAPACITY);

        Ok(AdvertisingPlan {
            fields,
            advertising_data,
            scan_response,
            device_name: self
                .name
                .as_deref()
                .map(|name| CString::new(name).expect("names with NUL bytes were rejected")),
            remain_available: self.remain_available,
        })
    }
}

/// The advertising data's contents as NimBLE's `ble_hs_adv_fields` carries
/// them: flags, then 16-bit and 128-bit service UUID lists in that order.
///
/// The advertising data is handed to NimBLE as fields rather than raw bytes
/// because ESP-IDF's connection re-attempt (`CONFIG_BT_NIMBLE_ENABLE_CONN_REATTEMPT`)
/// restarts advertising by itself, without any event, from the fields last
/// passed to `ble_gap_adv_set_fields` (`ble_gap_slave_adv_reattempt` in
/// `ble_gap.c`); raw data would leave it re-advertising an empty packet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AdvertisingFields {
    pub(crate) flags: u8,
    pub(crate) uuids16: Vec<u16>,
    pub(crate) uuids16_complete: bool,
    /// Canonical 128-bit values; NimBLE stores them in wire order.
    pub(crate) uuids128: Vec<u128>,
    pub(crate) uuids128_complete: bool,
}

/// The encoded packets and policy of a configuration checked against its
/// server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AdvertisingPlan {
    /// The advertising data as NimBLE's field encoder receives it; see
    /// [`AdvertisingFields`].
    pub(crate) fields: AdvertisingFields,
    /// The advertising data as this crate encodes and validates it. NimBLE
    /// encodes `fields` to the same bytes (checked by host tests against a
    /// model of `ble_hs_adv_set_fields`).
    pub(crate) advertising_data: Vec<u8>,
    pub(crate) scan_response: Vec<u8>,
    pub(crate) device_name: Option<CString>,
    pub(crate) remain_available: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gatt::{Characteristic, CharacteristicDef, Readable, Service};
    use crate::{AttError, ErrorKind};

    struct Value;

    impl Characteristic for Value {
        type Value = u8;
        fn uuid(&self) -> Uuid {
            Uuid::Uuid16(0x2a19)
        }
    }

    impl Readable for Value {
        fn read(&self) -> Result<u8, AttError> {
            Ok(0)
        }
    }

    fn server(services: &[Uuid]) -> GattServer {
        GattServer::new(services.iter().map(|uuid| {
            Service::primary(*uuid).characteristic(CharacteristicDef::new(Value).readable())
        }))
        .unwrap()
    }

    const CUSTOM: Uuid = Uuid::Uuid128(0x6e40_0001_b5a3_f393_e0a9_e50e_24dc_ca9e);
    const OTHER: Uuid = Uuid::Uuid128(0x3e1f_7a2c_9b4d_4e6f_8a1b_2c3d_4e5f_6a7b);

    fn rejected(builder: AdvertisingBuilder) -> AdvertisingError {
        let error = builder.build().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Advertising);
        let cause = *error.advertising().expect("an advertising cause");
        assert_eq!(
            std::error::Error::source(&error)
                .and_then(|source| source.downcast_ref::<AdvertisingError>()),
            Some(&cause)
        );
        assert!(error.to_string().contains(&cause.to_string()), "{error}");
        cause
    }

    #[test]
    fn flags_services_and_name_are_placed_in_their_packets() {
        let advertising = Advertising::builder()
            .name("argyle-demo")
            .service(Uuid::Uuid16(0x180f))
            .service(CUSTOM)
            .build()
            .unwrap();
        assert_eq!(advertising.device_name(), Some("argyle-demo"));
        assert!(advertising.remains_available());
        let plan = advertising
            .plan(&server(&[Uuid::Uuid16(0x180f), CUSTOM]))
            .unwrap();
        let mut expected = vec![0x02, 0x01, 0x06, 0x03, 0x03, 0x0f, 0x18, 0x11, 0x07];
        expected.extend_from_slice(CUSTOM.to_wire_bytes().as_ref());
        assert_eq!(plan.advertising_data, expected);
        assert_eq!(plan.advertising_data.len(), 3 + 4 + 18);
        let mut response = vec![12, 0x09];
        response.extend_from_slice(b"argyle-demo");
        assert_eq!(plan.scan_response, response);
        assert_eq!(
            plan.device_name.as_deref(),
            Some(c"argyle-demo"),
            "the GAP Device Name"
        );
        assert!(plan.remain_available);
    }

    #[test]
    fn a_minimal_configuration_advertises_flags_only() {
        let advertising = Advertising::builder()
            .remain_available(false)
            .build()
            .unwrap();
        assert_eq!(advertising.local_name(), None);
        assert_eq!(advertising.device_name(), None);
        assert!(!advertising.remains_available());
        let plan = advertising.plan(&server(&[CUSTOM])).unwrap();
        assert_eq!(plan.advertising_data, [0x02, 0x01, 0x06]);
        assert!(plan.scan_response.is_empty());
        assert_eq!(plan.device_name, None);
        assert!(!plan.remain_available);
    }

    #[test]
    fn lists_are_complete_only_when_they_name_every_server_service_of_their_width() {
        let advertising = Advertising::builder()
            .service(Uuid::Uuid16(0x180f))
            .service(CUSTOM)
            .build()
            .unwrap();
        // Every 16-bit service is listed, but OTHER is a 128-bit service
        // that is not.
        let plan = advertising
            .plan(&server(&[Uuid::Uuid16(0x180f), CUSTOM, OTHER]))
            .unwrap();
        assert_eq!(plan.advertising_data[4], ad::COMPLETE_UUIDS16);
        assert_eq!(plan.advertising_data[8], ad::INCOMPLETE_UUIDS128);

        let plan = advertising
            .plan(&server(&[
                Uuid::Uuid16(0x180f),
                Uuid::Uuid16(0x181c),
                CUSTOM,
            ]))
            .unwrap();
        assert_eq!(plan.advertising_data[4], ad::INCOMPLETE_UUIDS16);
        assert_eq!(plan.advertising_data[8], ad::COMPLETE_UUIDS128);

        // A width with no advertised UUID has no list at all, even if the
        // server has services of that width.
        let plan = Advertising::builder()
            .service(CUSTOM)
            .build()
            .unwrap()
            .plan(&server(&[Uuid::Uuid16(0x180f), CUSTOM]))
            .unwrap();
        assert_eq!(plan.advertising_data.len(), 3 + 18);
        assert_eq!(plan.advertising_data[4], ad::COMPLETE_UUIDS128);
    }

    #[test]
    fn uuids_are_advertised_in_their_att_form() {
        // A 32-bit UUID is listed as 128-bit, and a 128-bit UUID over the
        // Bluetooth Base UUID as 16-bit, matching how the server registers
        // them; either spelling of a service matches it.
        let base_battery = Uuid::Uuid128(Uuid::Uuid16(0x180f).to_u128());
        let advertising = Advertising::builder()
            .service(Uuid::Uuid32(0x0001_0000))
            .service(base_battery)
            .build()
            .unwrap();
        assert_eq!(
            advertising.services(),
            [Uuid::Uuid32(0x0001_0000), base_battery]
        );
        let plan = advertising
            .plan(&server(&[
                Uuid::Uuid16(0x180f),
                Uuid::Uuid128(Uuid::Uuid32(0x0001_0000).to_u128()),
            ]))
            .unwrap();
        let mut expected = vec![0x02, 0x01, 0x06, 0x03, 0x03, 0x0f, 0x18, 0x11, 0x07];
        expected.extend_from_slice(
            Uuid::Uuid128(Uuid::Uuid32(0x0001_0000).to_u128())
                .to_wire_bytes()
                .as_ref(),
        );
        assert_eq!(plan.advertising_data, expected);
    }

    #[test]
    fn duplicates_are_rejected_in_any_spelling() {
        assert_eq!(
            rejected(
                Advertising::builder()
                    .service(Uuid::Uuid16(0x180f))
                    .service(Uuid::Uuid16(0x180f))
            ),
            AdvertisingError::DuplicateService(Uuid::Uuid16(0x180f))
        );
        let base_battery = Uuid::Uuid128(Uuid::Uuid16(0x180f).to_u128());
        assert_eq!(
            rejected(
                Advertising::builder()
                    .service(Uuid::Uuid16(0x180f))
                    .service(base_battery)
            ),
            AdvertisingError::DuplicateService(base_battery)
        );
    }

    #[test]
    fn service_lists_are_checked_against_the_advertising_data_capacity() {
        // Flags (3) + one 128-bit list (2 + 16) + a 16-bit list (2 + 2n):
        // four 16-bit UUIDs reach exactly 31 bytes.
        let full = |count: u16| {
            (0..count).fold(Advertising::builder().service(CUSTOM), |builder, index| {
                builder.service(Uuid::Uuid16(0x1810 + index))
            })
        };
        let advertising = full(4).build().unwrap();
        let services: Vec<Uuid> = advertising.services().to_vec();
        let plan = advertising.plan(&server(&services)).unwrap();
        assert_eq!(plan.advertising_data.len(), LEGACY_PAYLOAD_CAPACITY);
        assert_eq!(
            rejected(full(5)),
            AdvertisingError::ServicesDoNotFit {
                length: 33,
                capacity: 31
            }
        );
        // Two 128-bit UUIDs never fit beside the flags.
        assert_eq!(
            rejected(Advertising::builder().service(CUSTOM).service(OTHER)),
            AdvertisingError::ServicesDoNotFit {
                length: 3 + 2 + 32,
                capacity: 31
            }
        );
        // Thirteen 16-bit UUIDs fit exactly; fourteen do not.
        let sixteen = |count: u16| {
            (0..count).fold(Advertising::builder(), |builder, index| {
                builder.service(Uuid::Uuid16(0x1810 + index))
            })
        };
        let advertising = sixteen(13).build().unwrap();
        let services: Vec<Uuid> = advertising.services().to_vec();
        let plan = advertising.plan(&server(&services)).unwrap();
        assert_eq!(plan.advertising_data.len(), LEGACY_PAYLOAD_CAPACITY);
        assert_eq!(
            rejected(sixteen(14)),
            AdvertisingError::ServicesDoNotFit {
                length: 33,
                capacity: 31
            }
        );
    }

    #[test]
    fn names_fill_the_scan_response_up_to_its_capacity() {
        let exact = "n".repeat(29);
        let advertising = Advertising::builder().name(&exact).build().unwrap();
        assert_eq!(
            advertising.local_name(),
            Some(LocalName::Complete(&exact[..]))
        );
        let plan = advertising.plan(&server(&[CUSTOM])).unwrap();
        assert_eq!(plan.scan_response.len(), LEGACY_PAYLOAD_CAPACITY);
        assert_eq!(plan.scan_response[..2], [30, ad::COMPLETE_NAME]);

        let over = "n".repeat(30);
        assert_eq!(
            rejected(Advertising::builder().name(&over)),
            AdvertisingError::NameDoesNotFit {
                length: 30,
                capacity: 29,
                minimum: None
            }
        );
        let advertising = Advertising::builder()
            .name(&over)
            .shortened_name(29)
            .build()
            .unwrap();
        assert_eq!(
            advertising.local_name(),
            Some(LocalName::Shortened(&exact[..]))
        );
        assert_eq!(advertising.device_name(), Some(&over[..]), "kept whole");
        let plan = advertising.plan(&server(&[CUSTOM])).unwrap();
        assert_eq!(plan.scan_response[..2], [30, ad::SHORTENED_NAME]);
        assert_eq!(plan.device_name.unwrap().as_bytes(), over.as_bytes());
        assert_eq!(
            rejected(Advertising::builder().name(&over).shortened_name(30)),
            AdvertisingError::NameDoesNotFit {
                length: 30,
                capacity: 29,
                minimum: Some(30)
            }
        );
        // A name that fits is complete whatever the minimum.
        let advertising = Advertising::builder()
            .name("short")
            .shortened_name(200)
            .build()
            .unwrap();
        assert_eq!(advertising.local_name(), Some(LocalName::Complete("short")));
    }

    #[test]
    fn shortened_names_end_on_a_character_boundary() {
        // 27 ASCII bytes, then a 3-byte character spanning bytes 27..30.
        let name = format!("{}\u{2603}tail", "a".repeat(27));
        let advertising = Advertising::builder()
            .name(&name)
            .shortened_name(1)
            .build()
            .unwrap();
        assert_eq!(
            advertising.local_name(),
            Some(LocalName::Shortened(&name[..27]))
        );
        assert_eq!(
            rejected(Advertising::builder().name(&name).shortened_name(28)),
            AdvertisingError::NameDoesNotFit {
                length: name.len(),
                capacity: 29,
                minimum: Some(28)
            }
        );
    }

    #[test]
    fn invalid_names_and_minimums_are_rejected() {
        assert_eq!(
            rejected(Advertising::builder().name("")),
            AdvertisingError::EmptyName
        );
        assert_eq!(
            rejected(Advertising::builder().name("argyle\0demo")),
            AdvertisingError::NameContainsNul
        );
        let longest = "n".repeat(MAX_DEVICE_NAME_LEN);
        assert!(Advertising::builder()
            .name(&longest)
            .shortened_name(1)
            .build()
            .is_ok());
        assert_eq!(
            rejected(
                Advertising::builder()
                    .name("n".repeat(MAX_DEVICE_NAME_LEN + 1))
                    .shortened_name(1)
            ),
            AdvertisingError::NameTooLong { length: 249 }
        );
        assert_eq!(
            rejected(Advertising::builder().name("demo").shortened_name(0)),
            AdvertisingError::ZeroShortenedLength
        );
        // Without a name, a shortening allowance has nothing to shorten.
        assert_eq!(
            Advertising::builder()
                .shortened_name(4)
                .build()
                .unwrap()
                .local_name(),
            None
        );
    }

    #[test]
    fn services_must_belong_to_the_server() {
        let advertising = Advertising::builder()
            .service(Uuid::Uuid16(0x180f))
            .service(OTHER)
            .build()
            .unwrap();
        assert_eq!(
            advertising.plan(&server(&[Uuid::Uuid16(0x180f)])),
            Err(AdvertisingError::UnknownService(OTHER))
        );
        let error = Error::from(AdvertisingError::UnknownService(OTHER));
        assert!(
            error.to_string().contains("not a primary service"),
            "{error}"
        );
    }

    #[test]
    fn nimbles_field_encoder_produces_the_validated_payload() {
        use crate::backend::fake::nimble_encode;
        let sixteen = |count: u16| (0..count).map(|index| Uuid::Uuid16(0x1810 + index));
        let cases: Vec<(Vec<Uuid>, Vec<Uuid>)> = vec![
            (vec![], vec![CUSTOM]),
            (
                vec![Uuid::Uuid16(0x180f)],
                vec![Uuid::Uuid16(0x180f), CUSTOM],
            ),
            (vec![CUSTOM], vec![Uuid::Uuid16(0x180f), CUSTOM, OTHER]),
            (
                vec![Uuid::Uuid16(0x180f), CUSTOM],
                vec![Uuid::Uuid16(0x180f), Uuid::Uuid16(0x181c), CUSTOM, OTHER],
            ),
            (sixteen(13).collect(), sixteen(13).collect()),
            (
                std::iter::once(CUSTOM).chain(sixteen(4)).collect(),
                std::iter::once(CUSTOM).chain(sixteen(5)).collect(),
            ),
        ];
        for (advertised, services) in cases {
            let advertising = advertised
                .iter()
                .fold(
                    Advertising::builder().name("argyle-demo"),
                    |builder, uuid| builder.service(*uuid),
                )
                .build()
                .unwrap();
            let plan = advertising.plan(&server(&services)).unwrap();
            assert_eq!(
                nimble_encode(&plan.fields),
                Ok(plan.advertising_data.clone()),
                "{advertised:?} of {services:?}"
            );
        }

        // NimBLE's size accounting rejects what the builder rejects.
        let mut fields = AdvertisingFields {
            flags: ad::GENERAL_DISCOVERABLE | ad::BREDR_UNSUPPORTED,
            uuids16: (0..14).collect(),
            uuids16_complete: true,
            uuids128: Vec::new(),
            uuids128_complete: false,
        };
        assert_eq!(nimble_encode(&fields), Err(4));
        fields.uuids16.pop();
        assert_eq!(nimble_encode(&fields).map(|data| data.len()), Ok(31));
        fields.uuids16 = vec![1, 2, 3, 4, 5];
        fields.uuids128 = vec![CUSTOM.to_u128()];
        assert_eq!(nimble_encode(&fields), Err(4));
        fields.uuids16.pop();
        assert_eq!(nimble_encode(&fields).map(|data| data.len()), Ok(31));
    }

    #[test]
    fn every_error_has_a_distinct_message() {
        let errors = [
            AdvertisingError::EmptyName,
            AdvertisingError::NameContainsNul,
            AdvertisingError::NameTooLong { length: 249 },
            AdvertisingError::NameDoesNotFit {
                length: 30,
                capacity: 29,
                minimum: None,
            },
            AdvertisingError::NameDoesNotFit {
                length: 30,
                capacity: 29,
                minimum: Some(30),
            },
            AdvertisingError::ZeroShortenedLength,
            AdvertisingError::DuplicateService(CUSTOM),
            AdvertisingError::ServicesDoNotFit {
                length: 33,
                capacity: 31,
            },
            AdvertisingError::UnknownService(CUSTOM),
        ];
        let messages: std::collections::BTreeSet<String> =
            errors.iter().map(ToString::to_string).collect();
        assert_eq!(messages.len(), errors.len());
    }
}

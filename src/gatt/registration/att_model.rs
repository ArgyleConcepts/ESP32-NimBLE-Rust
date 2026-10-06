//! A host model of the ESP-IDF 6.1 NimBLE ATT server procedures that reach
//! GATT access callbacks. It drives [`dispatch_access`], the body of the ESP
//! trampolines, over the fake backend's buffers, so tests see what a client
//! would get back from a server built with this framework.
//!
//! Transcribed from `ble_gatts.c` (`ble_gatts_val_access`), `ble_att_svr.c`
//! (Read, Read Blob, Write, Prepare Write, and Execute Write handling), and
//! `ble_att.c` (`ble_att_truncate_to_mtu`); see the
//! [registration module](super#offsets-and-long-values) for the behavior
//! relied on. Permissions, security, MTU exchange, the queued-write timeout,
//! and the transport are not modeled, and NimBLE's buffer pool only roughly
//! (see [`AttModel::set_part_budget`]). Passing tests here are evidence about
//! the framework's side of these procedures against this transcription, not
//! about NimBLE, a target, or hardware.

use super::{dispatch_access, AccessBuffer, AccessCodes, AttributeKind, GattPlan};
use crate::backend::fake::{FakeBackend, FakeMbuf};
use crate::backend::native::Backend;
use crate::gatt::GattServer;
use crate::AttError;
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::marker::PhantomData;

/// NimBLE's `BLE_GATT_ACCESS_OP_*` values.
const CODES: AccessCodes = AccessCodes {
    read_characteristic: 0,
    write_characteristic: 1,
    read_descriptor: 2,
    write_descriptor: 3,
};

/// `BLE_ATT_OP_READ_RSP` and `BLE_ATT_OP_READ_BLOB_RSP`.
const READ_RESPONSE: u8 = 0x0b;
const READ_BLOB_RESPONSE: u8 = 0x0d;

/// `BLE_ATT_ATTR_MAX_LEN`, which Execute Write enforces unless blob transfer
/// is enabled.
const ATTRIBUTE_MAX_LEN: usize = 512;

/// The default of `CONFIG_BT_NIMBLE_ATT_MAX_PREP_ENTRIES`.
const MAX_PREPARED: usize = 64;

/// The ESP32-C3 and ESP32-S3 default of `CONFIG_BT_NIMBLE_MSYS_1_BLOCK_COUNT`.
pub(crate) const MSYS_1_BLOCKS: usize = 12;

/// An attribute handle in the model's database. Handles only order the
/// prepared-write queue; they are not NimBLE's.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct Handle(u16);

struct Attribute {
    kind: AttributeKind,
    argument: *mut c_void,
}

struct Prepared {
    handle: Handle,
    offset: usize,
    part: Vec<u8>,
}

/// One connection's view of a registered server.
pub(crate) struct AttModel<'s> {
    fake: FakeBackend,
    mtu: usize,
    attributes: BTreeMap<Handle, Attribute>,
    characteristics: Vec<Handle>,
    descriptors: Vec<Vec<Handle>>,
    queue: Vec<Prepared>,
    max_prepared: usize,
    part_budget: usize,
    blob_transfer: bool,
    // The callback arguments point into the server.
    server: PhantomData<&'s GattServer>,
}

impl<'s> AttModel<'s> {
    /// A connection with the negotiated ATT `mtu`, at least the default 23.
    pub(crate) fn new(fake: &FakeBackend, server: &'s GattServer, mtu: u16) -> Self {
        assert!(mtu >= 23, "the ATT MTU is at least 23");
        let plan = GattPlan::new(server);
        let mut attributes = BTreeMap::new();
        let mut characteristics = Vec::new();
        let mut descriptors = Vec::new();
        let mut next = 0_u16;
        let mut handle = || {
            next += 1;
            Handle(next)
        };
        for characteristic in plan.characteristics() {
            let value = handle();
            attributes.insert(
                value,
                Attribute {
                    kind: AttributeKind::Characteristic,
                    argument: characteristic.slot.as_arg(),
                },
            );
            characteristics.push(value);
            descriptors.push(
                characteristic
                    .descriptors
                    .iter()
                    .map(|descriptor| {
                        let handle = handle();
                        attributes.insert(
                            handle,
                            Attribute {
                                kind: AttributeKind::Descriptor,
                                argument: descriptor.slot.as_arg(),
                            },
                        );
                        handle
                    })
                    .collect(),
            );
        }
        Self {
            fake: fake.clone(),
            mtu: usize::from(mtu),
            attributes,
            characteristics,
            descriptors,
            queue: Vec::new(),
            max_prepared: MAX_PREPARED,
            part_budget: MSYS_1_BLOCKS,
            blob_transfer: false,
            server: PhantomData,
        }
    }

    pub(crate) fn mtu(&self) -> usize {
        self.mtu
    }

    /// The value handle of the `index`th characteristic in registration
    /// order.
    pub(crate) fn characteristic(&self, index: usize) -> Handle {
        self.characteristics[index]
    }

    /// The handle of a characteristic's `index`th custom descriptor.
    pub(crate) fn descriptor(&self, characteristic: usize, index: usize) -> Handle {
        self.descriptors[characteristic][index]
    }

    /// Limit the prepared-write queue, `BLE_ATT_SVR_MAX_PREP_ENTRIES`.
    pub(crate) fn set_max_prepared(&mut self, entries: usize) {
        self.max_prepared = entries;
    }

    /// Limit how many prepared parts NimBLE's buffers can hold; by default
    /// [`MSYS_1_BLOCKS`], as on the ESP32-C3 and ESP32-S3.
    ///
    /// `ble_att_svr_prep_alloc` gives each queued part its own buffer from
    /// the MSYS_1 pool (`ble_hs_mbuf_l2cap_pkt`), and the pool is chosen by
    /// size with no fallback (`os_msys_get_pkthdr`), so a part that finds the
    /// pool empty is refused with `BLE_ATT_ERR_INSUFFICIENT_RES`. This is an
    /// approximation: the pool is shared with all other traffic, so fewer
    /// blocks are usually free, and a part larger than one block's data area
    /// takes more than one.
    pub(crate) fn set_part_budget(&mut self, parts: usize) {
        self.part_budget = parts;
    }

    /// Model `CONFIG_BT_NIMBLE_BLE_GATT_BLOB_TRANSFER`, which removes Execute
    /// Write's 512-byte limit.
    pub(crate) fn set_blob_transfer(&mut self, enabled: bool) {
        self.blob_transfer = enabled;
    }

    /// Call back for `handle` as `ble_gatts_val_access` does.
    fn access(
        &self,
        handle: Handle,
        write: bool,
        offset: usize,
        buffer: &mut FakeMbuf,
    ) -> Result<(), AttError> {
        let attribute = &self.attributes[&handle];
        let op = match (attribute.kind, write) {
            (AttributeKind::Characteristic, false) => CODES.read_characteristic,
            (AttributeKind::Characteristic, true) => CODES.write_characteristic,
            (AttributeKind::Descriptor, false) => CODES.read_descriptor,
            (AttributeKind::Descriptor, true) => CODES.write_descriptor,
        };
        let offset = u16::try_from(offset).expect("ATT offsets are 16-bit");
        // SAFETY: the argument is a slot of the server, which is borrowed
        // for the model's lifetime.
        let status = unsafe {
            dispatch_access(
                attribute.kind,
                attribute.argument,
                op,
                offset,
                &CODES,
                Some(AccessBuffer::new(&self.fake, buffer)),
            )
        };
        match status {
            0 => Ok(()),
            code => Err(u8::try_from(code)
                .ok()
                .and_then(|code| AttError::from_code(code).ok())
                .expect("callbacks return ATT error codes")),
        }
    }

    /// A Read Request.
    pub(crate) fn read(&self, handle: Handle) -> Result<Vec<u8>, AttError> {
        self.read_at(handle, 0, READ_RESPONSE)
    }

    /// A Read Blob Request.
    pub(crate) fn read_blob(&self, handle: Handle, offset: usize) -> Result<Vec<u8>, AttError> {
        self.read_at(handle, offset, READ_BLOB_RESPONSE)
    }

    fn read_at(&self, handle: Handle, offset: usize, opcode: u8) -> Result<Vec<u8>, AttError> {
        let response = if offset == 0 {
            // The callback appends to the response, which already holds the
            // opcode; on an error NimBLE empties it for an Error Response.
            let mut response = self.fake.mbuf_from_segments(&[&[opcode]]);
            let result = self.access(handle, false, 0, &mut response);
            let data = self.fake.mbuf_data(response.id()).unwrap();
            self.fake.mbuf_free(response).unwrap();
            result.map(|()| data)
        } else {
            // A fresh buffer for the value; the bytes after the offset go to
            // the response, and the buffer is freed.
            let mut value = self.fake.mbuf_from_segments(&[]);
            let result = self.access(handle, false, offset, &mut value);
            let data = self.fake.mbuf_data(value.id()).unwrap();
            self.fake.mbuf_free(value).unwrap();
            result.and_then(|()| {
                let tail = data.get(offset..).ok_or(AttError::INVALID_OFFSET)?;
                Ok([&[opcode][..], tail].concat())
            })
        }?;
        // `ble_att_truncate_to_mtu`, then strip the opcode.
        Ok(response[1..response.len().min(self.mtu)].to_vec())
    }

    /// The client's Read Long Characteristic Value procedure (Core
    /// Specification Vol 3, Part G, 4.8.3): a Read, then Read Blob at the
    /// received length for as long as responses are full (MTU - 1 bytes).
    pub(crate) fn read_long(&self, handle: Handle) -> Result<Vec<u8>, AttError> {
        let full = self.mtu - 1;
        let mut value = self.read(handle)?;
        let mut last = value.len();
        while last == full {
            let part = self.read_blob(handle, value.len())?;
            last = part.len();
            value.extend(part);
        }
        Ok(value)
    }

    /// A Write Request or Write Command, which carries at most MTU - 3
    /// value bytes, delivered at offset 0.
    pub(crate) fn write(&self, handle: Handle, value: &[u8]) -> Result<(), AttError> {
        assert!(
            value.len() <= self.mtu - 3,
            "a write PDU carries at most ATT_MTU - 3 bytes"
        );
        self.deliver(handle, &[value])
    }

    /// Deliver one write whose value NimBLE holds as a chain of `segments`.
    pub(crate) fn deliver(&self, handle: Handle, segments: &[&[u8]]) -> Result<(), AttError> {
        let mut request = self.fake.mbuf_from_segments(segments);
        let result = self.access(handle, true, 0, &mut request);
        // NimBLE frees the request after the callback.
        self.fake.mbuf_free(request).unwrap();
        result
    }

    /// A Prepare Write Request, which carries at most MTU - 5 value bytes.
    /// NimBLE queues it, ordered by handle and offset, without calling back.
    pub(crate) fn prepare(
        &mut self,
        handle: Handle,
        offset: usize,
        part: &[u8],
    ) -> Result<(), AttError> {
        assert!(
            part.len() <= self.mtu - 5,
            "a Prepare Write carries at most ATT_MTU - 5 bytes"
        );
        assert!(self.attributes.contains_key(&handle), "unknown handle");
        if self.queue.len() >= self.max_prepared {
            return Err(AttError::PREPARE_QUEUE_FULL);
        }
        if self.queue.len() >= self.part_budget {
            return Err(AttError::INSUFFICIENT_RESOURCES);
        }
        let position = self
            .queue
            .iter()
            .position(|entry| {
                entry.handle > handle || (entry.handle == handle && entry.offset > offset)
            })
            .unwrap_or(self.queue.len());
        self.queue.insert(
            position,
            Prepared {
                handle,
                offset,
                part: part.to_vec(),
            },
        );
        Ok(())
    }

    /// An Execute Write Request. Cancelling discards the queue. Committing
    /// validates the whole queue first, then writes each attribute's parts
    /// as one chained value at offset 0, stopping at the first failure; the
    /// queue is discarded either way.
    pub(crate) fn execute(&mut self, commit: bool) -> Result<(), AttError> {
        let queue = std::mem::take(&mut self.queue);
        if !commit {
            return Ok(());
        }
        let mut previous: Option<&Prepared> = None;
        for entry in &queue {
            let expected = match previous {
                Some(previous) if previous.handle == entry.handle => {
                    previous.offset + previous.part.len()
                }
                _ => 0,
            };
            if entry.offset != expected {
                return Err(AttError::INVALID_OFFSET);
            }
            if !self.blob_transfer && entry.offset + entry.part.len() > ATTRIBUTE_MAX_LEN {
                return Err(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH);
            }
            previous = Some(entry);
        }
        for parts in queue.chunk_by(|first, second| first.handle == second.handle) {
            let segments: Vec<&[u8]> = parts.iter().map(|entry| entry.part.as_slice()).collect();
            self.deliver(parts[0].handle, &segments)?;
        }
        Ok(())
    }

    /// The client's Write Long Characteristic Values procedure (Vol 3,
    /// Part G, 4.9.4): Prepare Write in parts of MTU - 5 bytes, then Execute
    /// Write. A refused part cancels the queue.
    pub(crate) fn write_long(&mut self, handle: Handle, value: &[u8]) -> Result<(), AttError> {
        let part_len = self.mtu - 5;
        for (index, part) in value.chunks(part_len).enumerate() {
            if let Err(error) = self.prepare(handle, index * part_len, part) {
                self.execute(false)?;
                return Err(error);
            }
        }
        self.execute(true)
    }

    /// The link dropped: `ble_hs_conn_free` discards prepared writes.
    pub(crate) fn disconnect(&mut self) {
        self.queue.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::NativeCall;
    use crate::gatt::{
        Characteristic, CharacteristicDef, Descriptor, DescriptorDef, Readable, ReadableDescriptor,
        Service, Writable, WritableDescriptor,
    };
    use crate::Uuid;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// Application state behind a byte characteristic: its value, how often
    /// it was read, and every value written.
    #[derive(Clone, Default)]
    struct Store {
        value: Arc<Mutex<Vec<u8>>>,
        reads: Arc<AtomicUsize>,
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl Store {
        fn reads(&self) -> usize {
            self.reads.load(Ordering::Relaxed)
        }

        fn writes(&self) -> Vec<Vec<u8>> {
            self.writes.lock().unwrap().clone()
        }

        fn set(&self, value: &[u8]) {
            *self.value.lock().unwrap() = value.to_vec();
        }
    }

    /// Bytes up to `MAX`.
    struct Blob<const MAX: usize>(Store);

    impl<const MAX: usize> Characteristic for Blob<MAX> {
        type Value = Vec<u8>;
        const MAX_LEN: usize = MAX;
        fn uuid(&self) -> Uuid {
            Uuid::Uuid16(0xfff0)
        }
    }

    impl<const MAX: usize> Readable for Blob<MAX> {
        fn read(&self) -> Result<Vec<u8>, AttError> {
            self.0.reads.fetch_add(1, Ordering::Relaxed);
            Ok(self.0.value.lock().unwrap().clone())
        }
    }

    impl<const MAX: usize> Writable for Blob<MAX> {
        fn write(&self, value: Vec<u8>) -> Result<(), AttError> {
            self.0.writes.lock().unwrap().push(value.clone());
            *self.0.value.lock().unwrap() = value;
            Ok(())
        }
    }

    /// A text descriptor up to 64 bytes.
    struct Note(Arc<Mutex<String>>);

    impl Descriptor for Note {
        type Value = String;
        const MAX_LEN: usize = 64;
        fn uuid(&self) -> Uuid {
            Uuid::Uuid16(0xfff9)
        }
    }

    impl ReadableDescriptor for Note {
        fn read(&self) -> Result<String, AttError> {
            Ok(self.0.lock().unwrap().clone())
        }
    }

    impl WritableDescriptor for Note {
        fn write(&self, value: String) -> Result<(), AttError> {
            *self.0.lock().unwrap() = value;
            Ok(())
        }
    }

    fn blob<const MAX: usize>(store: &Store) -> CharacteristicDef<Blob<MAX>> {
        CharacteristicDef::new(Blob::<MAX>(store.clone()))
            .readable()
            .writable()
    }

    fn server(characteristics: impl IntoIterator<Item = Service>) -> GattServer {
        GattServer::new(characteristics).unwrap()
    }

    fn service() -> Service {
        Service::primary(Uuid::Uuid16(0x181c))
    }

    fn copies(fake: &FakeBackend) -> Vec<(u32, usize, usize)> {
        fake.calls()
            .into_iter()
            .filter_map(|call| match call {
                NativeCall::MbufCopy { id, offset, length } => Some((id, offset, length)),
                _ => None,
            })
            .collect()
    }

    fn lengths_read(fake: &FakeBackend) -> usize {
        fake.calls()
            .iter()
            .filter(|call| matches!(call, NativeCall::MbufLen { .. }))
            .count()
    }

    #[test]
    fn long_reads_reconstruct_every_length_at_every_negotiated_mtu() {
        let store = Store::default();
        let server = server([service().characteristic(blob::<512>(&store))]);
        let fake = FakeBackend::new();
        for mtu in [23_u16, 24, 64, 185, 247, 512, 517] {
            let model = AttModel::new(&fake, &server, mtu);
            let handle = model.characteristic(0);
            let full = model.mtu() - 1;
            let lengths = [0, 1, full - 1, full, full + 1, 2 * full, 511, 512];
            for length in lengths.into_iter().filter(|length| *length <= 512) {
                let value: Vec<u8> = (0..length)
                    .map(|index| (index * 7 + length) as u8)
                    .collect();
                store.set(&value);
                let before = store.reads();
                assert_eq!(
                    model.read_long(handle),
                    Ok(value),
                    "MTU {mtu}, {length} bytes"
                );
                // A Read, then a Read Blob after every full response; each
                // request calls the handler again.
                assert_eq!(store.reads() - before, length / full + 1, "MTU {mtu}");
            }
        }

        // One Read of a 512-byte value at the default MTU: the framework
        // appended all of it; the response carries what fits.
        store.set(&[0x5a; 512]);
        let model = AttModel::new(&fake, &server, 23);
        assert_eq!(model.read(model.characteristic(0)), Ok(vec![0x5a; 22]));
        assert!(matches!(
            fake.calls().last(),
            Some(NativeCall::MbufFree { .. })
        ));
        let appended: Vec<_> = fake
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                NativeCall::MbufAppend { length, .. } => Some(length),
                _ => None,
            })
            .collect();
        assert_eq!(appended.last(), Some(&512));
        assert_eq!(fake.assert_balanced(), Ok(()));
    }

    #[test]
    fn read_blob_offsets_past_the_value_are_refused_by_the_stack() {
        let store = Store::default();
        store.set(&[1; 30]);
        let server = server([service().characteristic(blob::<512>(&store))]);
        let fake = FakeBackend::new();
        let model = AttModel::new(&fake, &server, 23);
        let handle = model.characteristic(0);
        assert_eq!(model.read_blob(handle, 22), Ok(vec![1; 8]));
        assert_eq!(model.read_blob(handle, 30), Ok(vec![]), "the end is valid");
        assert_eq!(model.read_blob(handle, 31), Err(AttError::INVALID_OFFSET));
        assert_eq!(
            store.reads(),
            3,
            "the handler runs before the offset is checked"
        );
        assert_eq!(fake.assert_balanced(), Ok(()));
    }

    /// Returns the next queued value on each read.
    struct Changing(Mutex<VecDeque<Vec<u8>>>);

    impl Characteristic for Changing {
        type Value = Vec<u8>;
        fn uuid(&self) -> Uuid {
            Uuid::Uuid16(0xfff1)
        }
    }

    impl Readable for Changing {
        fn read(&self) -> Result<Vec<u8>, AttError> {
            self.0.lock().unwrap().pop_front().ok_or(AttError::UNLIKELY)
        }
    }

    #[test]
    fn a_value_that_changes_between_blobs_reaches_the_client_mixed() {
        // The documented consequence of NimBLE calling back per request:
        // consistency across a long read is the application's to provide.
        let changing = Changing(Mutex::new(VecDeque::from([vec![0xaa; 40], vec![0xbb; 40]])));
        let server =
            server([service().characteristic(CharacteristicDef::new(changing).readable())]);
        let fake = FakeBackend::new();
        let model = AttModel::new(&fake, &server, 23);
        let mixed = model.read_long(model.characteristic(0)).unwrap();
        assert_eq!(mixed, [vec![0xaa; 22], vec![0xbb; 18]].concat());

        // A value that shrinks below the client's offset ends the read.
        let shrinking = Changing(Mutex::new(VecDeque::from([vec![1; 40], vec![2; 10]])));
        let server =
            self::server([service().characteristic(CharacteristicDef::new(shrinking).readable())]);
        let model = AttModel::new(&fake, &server, 23);
        assert_eq!(
            model.read_long(model.characteristic(0)),
            Err(AttError::INVALID_OFFSET)
        );
        assert_eq!(fake.assert_balanced(), Ok(()));
    }

    #[test]
    fn long_writes_reach_the_handler_once_as_one_chained_value() {
        let store = Store::default();
        let server = server([service().characteristic(blob::<512>(&store))]);
        let fake = FakeBackend::new();
        for mtu in [23_u16, 64, 247] {
            let mut model = AttModel::new(&fake, &server, mtu);
            // 512 bytes at MTU 23 take 29 parts, more than the default pool
            // holds: as with CONFIG_BT_NIMBLE_MSYS_1_BLOCK_COUNT raised to at
            // least 29 and that many blocks free. MTUs 64 and 247 take 9 and
            // 3 parts, within the default.
            if mtu == 23 {
                model.set_part_budget(29);
            }
            let handle = model.characteristic(0);
            let part = model.mtu() - 5;
            for length in [model.mtu() - 2, 100, 512] {
                let value: Vec<u8> = (0..length).map(|index| (index % 251) as u8).collect();
                store.writes.lock().unwrap().clear();
                assert_eq!(model.write_long(handle, &value), Ok(()), "MTU {mtu}");
                assert_eq!(store.writes(), [value], "MTU {mtu}, {length} bytes");
                // Copied whole, in one call, from a chain of the prepared parts.
                let (id, offset, copied) = *copies(&fake).last().unwrap();
                assert_eq!((offset, copied), (0, length));
                let segments = fake.mbuf_segments(id).unwrap();
                assert_eq!(segments.len(), length.div_ceil(part));
                assert!(segments.iter().all(|segment| *segment <= part));
            }
        }
        assert_eq!(fake.assert_balanced(), Ok(()));
    }

    #[test]
    fn long_writes_are_bounded_by_max_len_not_by_the_mtu() {
        let small = Store::default();
        let large = Store::default();
        let server = server([service()
            .characteristic(blob::<100>(&small))
            .characteristic(blob::<512>(&large))]);
        let fake = FakeBackend::new();
        let mut model = AttModel::new(&fake, &server, 23);
        let (small_handle, large_handle) = (model.characteristic(0), model.characteristic(1));

        assert_eq!(model.write_long(small_handle, &[7; 100]), Ok(()));
        let copied = copies(&fake).len();
        assert_eq!(
            model.write_long(small_handle, &[8; 101]),
            Err(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH)
        );
        assert_eq!(copies(&fake).len(), copied, "refused before copying");
        assert_eq!(small.writes(), [vec![7; 100]]);

        // Beyond 512 bytes NimBLE refuses before calling back. These
        // writes use MTU 247, so their three parts fit the default pool.
        let mut model = AttModel::new(&fake, &server, 247);
        let measured = lengths_read(&fake);
        assert_eq!(
            model.write_long(large_handle, &[9; 513]),
            Err(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH)
        );
        assert_eq!(lengths_read(&fake), measured, "no callback");
        // ...and with blob transfer enabled the framework still does.
        model.set_blob_transfer(true);
        assert_eq!(
            model.write_long(large_handle, &[9; 600]),
            Err(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH)
        );
        assert_eq!(lengths_read(&fake), measured + 1, "the callback ran");
        assert_eq!(copies(&fake).len(), copied);
        assert!(large.writes().is_empty());
        assert_eq!(fake.assert_balanced(), Ok(()));
    }

    #[test]
    fn long_writes_beyond_the_buffer_pool_fail_without_reaching_the_handler() {
        let store = Store::default();
        let server = server([service().characteristic(blob::<512>(&store))]);
        let fake = FakeBackend::new();
        let mut model = AttModel::new(&fake, &server, 23);
        let handle = model.characteristic(0);

        // With the C3/S3 default of 12 blocks, 12 parts of 18 bytes fit and a
        // 13th does not; the client's procedure then cancels the queue.
        assert_eq!(model.write_long(handle, &[1; 12 * 18]), Ok(()));
        for length in [12 * 18 + 1, 512] {
            assert_eq!(
                model.write_long(handle, &vec![2; length]),
                Err(AttError::INSUFFICIENT_RESOURCES),
                "{length} bytes"
            );
        }
        assert_eq!(store.writes(), [vec![1; 12 * 18]], "only the fitting write");
        assert_eq!(copies(&fake).len(), 1);
        assert_eq!(model.execute(true), Ok(()), "nothing left queued");

        // A larger MTU needs fewer parts: 512 bytes in 3 at MTU 247.
        let mut model = AttModel::new(&fake, &server, 247);
        assert_eq!(model.write_long(handle, &[3; 512]), Ok(()));
        assert_eq!(store.writes().last(), Some(&vec![3; 512]));
        assert_eq!(fake.assert_balanced(), Ok(()));
    }

    #[test]
    fn malformed_cancelled_and_abandoned_queues_reach_no_handler() {
        let store = Store::default();
        let server = server([service().characteristic(blob::<512>(&store))]);
        let fake = FakeBackend::new();
        let mut model = AttModel::new(&fake, &server, 23);
        let handle = model.characteristic(0);

        // Not starting at 0, a gap, and an overlap.
        for parts in [
            &[(5, &[1_u8; 4][..])][..],
            &[(0, &[1; 4][..]), (5, &[2; 4][..])],
            &[(0, &[1; 4][..]), (3, &[2; 4][..])],
        ] {
            for (offset, part) in parts {
                model.prepare(handle, *offset, part).unwrap();
            }
            assert_eq!(model.execute(true), Err(AttError::INVALID_OFFSET));
        }
        // Prepared parts arrive in any order; NimBLE sorts them.
        model.prepare(handle, 4, &[2; 4]).unwrap();
        model.prepare(handle, 0, &[1; 4]).unwrap();
        model.execute(false).unwrap();
        // The queue's limit, after which the client's procedure cancels.
        model.set_max_prepared(3);
        assert_eq!(
            model.write_long(handle, &[3; 18 * 3 + 1]),
            Err(AttError::PREPARE_QUEUE_FULL)
        );
        assert_eq!(model.execute(true), Ok(()), "nothing left to execute");
        // A disconnect discards a half-finished queue.
        model.prepare(handle, 0, &[4; 18]).unwrap();
        model.disconnect();
        assert_eq!(model.execute(true), Ok(()));

        assert!(store.writes().is_empty());
        assert!(copies(&fake).is_empty());
        assert_eq!(fake.assert_balanced(), Ok(()));
    }

    #[test]
    fn queued_writes_to_several_attributes_apply_in_turn_and_are_not_atomic() {
        let first = Store::default();
        let second = Store::default();
        let server = server([service()
            .characteristic(blob::<512>(&first))
            .characteristic(blob::<4>(&second))]);
        let fake = FakeBackend::new();
        let mut model = AttModel::new(&fake, &server, 23);
        let (one, two) = (model.characteristic(0), model.characteristic(1));
        model.prepare(two, 0, &[2; 5]).unwrap();
        model.prepare(one, 0, &[1; 18]).unwrap();
        model.prepare(one, 18, &[1; 2]).unwrap();
        assert_eq!(
            model.execute(true),
            Err(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH),
            "the second attribute's MAX_LEN is 4"
        );
        assert_eq!(first.writes(), [vec![1; 20]], "already applied");
        assert!(second.writes().is_empty());
        assert_eq!(fake.assert_balanced(), Ok(()));
    }

    #[test]
    fn descriptor_text_is_written_and_read_in_parts_that_split_characters() {
        let store = Store::default();
        let note = Arc::new(Mutex::new(String::new()));
        let definition = blob::<512>(&store).descriptor(
            DescriptorDef::new(Note(note.clone()))
                .unwrap()
                .readable()
                .writable(),
        );
        let server = server([service().characteristic(definition)]);
        let fake = FakeBackend::new();
        let mut model = AttModel::new(&fake, &server, 23);
        let handle = model.descriptor(0, 0);
        // 18-byte parts split the two-byte 'é' at bytes 17 and 18.
        let text = "seventeen bytes..é and more ünïcödé";
        assert_eq!(text.as_bytes()[17..19], "é".as_bytes()[..]);
        assert_eq!(model.write_long(handle, text.as_bytes()), Ok(()));
        assert_eq!(*note.lock().unwrap(), text);
        assert_eq!(model.read_long(handle), Ok(text.as_bytes().to_vec()));
        assert_eq!(fake.assert_balanced(), Ok(()));
    }

    #[test]
    fn write_pdus_up_to_the_mtu_reach_the_handler_whole() {
        let store = Store::default();
        let server = server([service().characteristic(blob::<512>(&store))]);
        let fake = FakeBackend::new();
        for mtu in [23_u16, 247, 515] {
            let model = AttModel::new(&fake, &server, mtu);
            let value = vec![mtu as u8; model.mtu() - 3];
            assert_eq!(model.write(model.characteristic(0), &value), Ok(()));
            assert_eq!(store.writes().last(), Some(&value));
            assert_eq!(model.write(model.characteristic(0), &[]), Ok(()));
            assert_eq!(store.writes().last(), Some(&vec![]));
        }
        // A PDU can carry more than MAX_LEN at a large MTU; it is refused.
        let model = AttModel::new(&fake, &server, 517);
        let writes = store.writes().len();
        assert_eq!(
            model.write(model.characteristic(0), &[1; 514]),
            Err(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH)
        );
        assert_eq!(store.writes().len(), writes);
        assert_eq!(fake.assert_balanced(), Ok(()));
    }
}

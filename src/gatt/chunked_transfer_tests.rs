//! The documented chunked-transfer example, served through the registration
//! dispatch to clients modeled at several ATT MTUs, with failures, retries,
//! a full hand-off channel, and disconnection. It checks the rules stated in
//! the [module documentation](super#chunked-transfers).

/// The example exactly as documented, with its `argyle_nimble` paths
/// resolving to this crate.
mod example {
    use crate as argyle_nimble;
    include!("transfer_example.rs");
}

use super::registration::att_model::{AttModel, Handle};
use super::GattServer;
use crate::backend::fake::{FakeBackend, NativeCall};
use crate::backend::native::Operation;
use crate::AttError;
use example::*;
use std::sync::mpsc::{sync_channel, Receiver};
use std::sync::Arc;

/// The client's view of the transfer service.
struct Client<'s> {
    model: AttModel<'s>,
    control: Handle,
    status: Handle,
    data: Handle,
}

impl<'s> Client<'s> {
    fn connect(fake: &FakeBackend, server: &'s GattServer, mtu: u16) -> Self {
        let model = AttModel::new(fake, server, mtu);
        Self {
            control: model.characteristic(0),
            status: model.characteristic(1),
            data: model.characteristic(2),
            model,
        }
    }

    fn begin(&self, total: u32) -> Result<(), AttError> {
        self.model
            .write(self.control, &[&[0x01][..], &total.to_le_bytes()].concat())
    }

    fn commit(&self) -> Result<(), AttError> {
        self.model.write(self.control, &[0x02])
    }

    fn abort(&self) -> Result<(), AttError> {
        self.model.write(self.control, &[0x03])
    }

    /// Bytes received and expected, as the status reports them.
    fn progress(&self) -> (usize, usize) {
        let status = self.model.read(self.status).unwrap();
        let field = |range: std::ops::Range<usize>| {
            u32::from_le_bytes(status[range].try_into().unwrap()) as usize
        };
        assert_eq!(status.len(), 8);
        (field(0..4), field(4..8))
    }

    /// The documented chunk size: as much as one Write Request carries
    /// after the offset, up to the data characteristic's limit.
    fn chunk_len(&self) -> usize {
        (self.model.mtu() - 3 - 4).min(CHUNK_LEN)
    }

    /// Send one chunk in one Write Request; the model refuses to build a
    /// request larger than the MTU allows.
    fn chunk(&self, offset: usize, bytes: &[u8]) -> Result<(), AttError> {
        let value = [&(offset as u32).to_le_bytes()[..], bytes].concat();
        self.model.write(self.data, &value)
    }

    /// Send `bytes` from `from` in chunks of the documented size.
    fn send(&self, bytes: &[u8], from: usize) -> Result<(), AttError> {
        let chunk_len = self.chunk_len();
        for (index, part) in bytes[from..].chunks(chunk_len).enumerate() {
            self.chunk(from + index * chunk_len, part)?;
        }
        Ok(())
    }
}

/// Where the application learns that the connection ended: NimBLE drops
/// queued long-write parts, and the application resets its session. The
/// framework's connection events do not exist yet, so tests call this
/// directly.
fn on_disconnect(client: &mut Client<'_>, transfer: &Transfer) {
    client.model.disconnect();
    transfer.reset();
}

/// The application's state with a hand-off channel holding `waiting`
/// committed transfers.
fn application(waiting: usize) -> (Arc<Transfer>, Receiver<Vec<u8>>) {
    let (completed, committed) = sync_channel(waiting);
    (Arc::new(Transfer::new(completed)), committed)
}

fn image(length: usize, seed: u8) -> Vec<u8> {
    (0..length)
        .map(|index| (index as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn copies(fake: &FakeBackend) -> usize {
    fake.calls()
        .iter()
        .filter(|call| matches!(call, NativeCall::MbufCopy { .. }))
        .count()
}

#[test]
fn a_multi_chunk_transfer_reconstructs_the_bytes_at_every_mtu() {
    let (transfer, committed) = application(1);
    let server = GattServer::new([transfer_service(&transfer)]).unwrap();
    let fake = FakeBackend::new();
    let bytes = image(1000, 7);
    for mtu in [23_u16, 64, 185, 247, 517] {
        let client = Client::connect(&fake, &server, mtu);
        let before = copies(&fake);
        client.begin(1000).unwrap();
        client.send(&bytes, 0).unwrap();
        assert_eq!(client.progress(), (1000, 1000));
        client.commit().unwrap();
        assert_eq!(committed.try_recv().as_ref(), Ok(&bytes), "MTU {mtu}");
        assert_eq!(client.progress(), (0, 0), "committed transfers leave");
        // Every chunk and command reached its handler once, each in one
        // Write Request: the transfer used no long write.
        assert_eq!(
            copies(&fake) - before,
            1000_usize.div_ceil(client.chunk_len()) + 2,
            "MTU {mtu}"
        );
    }
    assert!(committed.try_recv().is_err(), "handed over once each");
    assert_eq!(fake.assert_balanced(), Ok(()));
}

#[test]
fn refused_and_failed_chunks_change_nothing_and_the_client_resumes() {
    let (transfer, committed) = application(1);
    let server = GattServer::new([transfer_service(&transfer)]).unwrap();
    let fake = FakeBackend::new();
    let client = Client::connect(&fake, &server, 23);
    let bytes = image(100, 3);
    assert_eq!(client.chunk_len(), 16);

    client.begin(100).unwrap();
    client.chunk(0, &bytes[..16]).unwrap();
    // A native copy failure, a repeated chunk, a skipped chunk, a value too
    // short for its offset, and an empty chunk.
    fake.fail_next(Operation::MbufCopy, -1);
    assert_eq!(client.chunk(16, &bytes[16..32]), Err(AttError::UNLIKELY));
    assert_eq!(client.chunk(0, &bytes[..16]), Err(UNEXPECTED_OFFSET));
    assert_eq!(client.chunk(32, &bytes[32..48]), Err(UNEXPECTED_OFFSET));
    assert_eq!(
        client.model.write(client.data, &[16, 0, 0]),
        Err(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH)
    );
    assert_eq!(
        client.chunk(16, &[]),
        Err(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH)
    );
    // A chunk over the data characteristic's MAX_LEN, from a client whose
    // MTU lets one Write Request carry it.
    let large = Client::connect(&fake, &server, 251);
    assert_eq!(
        large.chunk(16, &[0; CHUNK_LEN + 1]),
        Err(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH)
    );
    assert_eq!(client.commit(), Err(INCOMPLETE));
    assert_eq!(client.progress(), (16, 100), "nothing changed");

    // Resume from the reported offset; a chunk past the total is refused.
    let (received, _) = client.progress();
    client.send(&bytes[..96], received).unwrap();
    assert_eq!(
        client.chunk(96, &[0; 5]),
        Err(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH)
    );
    client.chunk(96, &bytes[96..]).unwrap();
    client.commit().unwrap();
    assert_eq!(committed.try_recv(), Ok(bytes));

    // Commands are checked too.
    assert_eq!(client.commit(), Err(NO_TRANSFER));
    assert_eq!(client.chunk(0, &[1]), Err(NO_TRANSFER));
    assert_eq!(
        client.model.write(client.control, &[0x09]),
        Err(AttError::VALUE_NOT_ALLOWED)
    );
    assert_eq!(
        client.model.write(client.control, &[0x02, 0x00]),
        Err(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH)
    );
    assert_eq!(
        client.begin(TRANSFER_LIMIT as u32 + 1),
        Err(AttError::OUT_OF_RANGE)
    );
    client.begin(4).unwrap();
    assert_eq!(
        client.begin(4),
        Err(AttError::PROCEDURE_ALREADY_IN_PROGRESS)
    );
    assert_eq!(fake.assert_balanced(), Ok(()));
    assert!(fake.violations().is_empty());
}

#[test]
fn committed_transfers_survive_a_later_begin_abort_or_disconnect() {
    let (transfer, committed) = application(1);
    let server = GattServer::new([transfer_service(&transfer)]).unwrap();
    let fake = FakeBackend::new();
    let mut client = Client::connect(&fake, &server, 23);
    let first = image(50, 1);
    client.begin(50).unwrap();
    client.send(&first, 0).unwrap();
    client.commit().unwrap();

    // The application has not looked yet; the client goes on.
    client.begin(30).unwrap();
    client.chunk(0, &[9; 16]).unwrap();
    client.abort().unwrap();
    client.begin(30).unwrap();
    client.chunk(0, &[9; 16]).unwrap();
    on_disconnect(&mut client, &transfer);
    assert_eq!(
        client.progress(),
        (0, 0),
        "only the uncommitted one is gone"
    );

    assert_eq!(committed.try_recv(), Ok(first));
    assert!(committed.try_recv().is_err());
    assert_eq!(fake.assert_balanced(), Ok(()));
}

#[test]
fn a_full_hand_off_refuses_commit_and_the_client_commits_again() {
    let (transfer, committed) = application(1);
    let server = GattServer::new([transfer_service(&transfer)]).unwrap();
    let fake = FakeBackend::new();
    let client = Client::connect(&fake, &server, 247);
    let (first, second) = (image(300, 1), image(300, 2));
    client.begin(300).unwrap();
    client.send(&first, 0).unwrap();
    client.commit().unwrap();

    // The channel holds the first transfer, so the second cannot be
    // committed yet; it stays complete and changes nothing.
    client.begin(300).unwrap();
    client.send(&second, 0).unwrap();
    assert_eq!(client.commit(), Err(BUSY));
    assert_eq!(client.commit(), Err(BUSY));
    assert_eq!(client.progress(), (300, 300));

    assert_eq!(committed.try_recv(), Ok(first));
    client.commit().unwrap();
    assert_eq!(committed.try_recv(), Ok(second));

    // An application that stopped receiving is a server fault; the
    // transfer is kept rather than lost.
    client.begin(1).unwrap();
    client.chunk(0, &[5]).unwrap();
    drop(committed);
    assert_eq!(client.commit(), Err(AttError::UNLIKELY));
    assert_eq!(client.progress(), (1, 1));
    assert_eq!(fake.assert_balanced(), Ok(()));
}

#[test]
fn disconnect_and_abort_discard_the_uncommitted_transfer() {
    let (transfer, committed) = application(1);
    let server = GattServer::new([transfer_service(&transfer)]).unwrap();
    let fake = FakeBackend::new();
    let mut client = Client::connect(&fake, &server, 23);
    let first = image(300, 1);

    // The link drops mid-transfer, with a long-write part a client queued
    // anyway.
    client.begin(300).unwrap();
    client.send(&first[..128], 0).unwrap();
    client
        .model
        .prepare(client.data, 0, &(128_u32).to_le_bytes())
        .unwrap();
    on_disconnect(&mut client, &transfer);
    assert_eq!(client.model.execute(true), Ok(()), "nothing was queued");
    assert_eq!(client.progress(), (0, 0));
    assert_eq!(client.chunk(128, &first[128..144]), Err(NO_TRANSFER));

    // A new client aborts, then begins, as the pattern recommends; an
    // abort part-way discards that transfer too.
    let client = Client::connect(&fake, &server, 247);
    client.abort().unwrap();
    client.begin(300).unwrap();
    client.send(&image(300, 2), 0).unwrap();
    client.abort().unwrap();
    assert_eq!(client.progress(), (0, 0));
    assert_eq!(client.commit(), Err(NO_TRANSFER));
    client.begin(300).unwrap();
    client.send(&first, 0).unwrap();
    client.commit().unwrap();
    assert_eq!(committed.try_recv(), Ok(first));
    assert!(committed.try_recv().is_err());
    assert_eq!(fake.assert_balanced(), Ok(()));
}

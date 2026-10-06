//! The documented chunked-transfer example, served through the registration
//! dispatch to clients modeled at several ATT MTUs, with failures, retries,
//! and disconnection. It checks the rules stated in the
//! [module documentation](super#chunked-transfers).

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

    /// Send one chunk: in a Write Request when it fits the MTU, otherwise
    /// as a GATT long write of the one chunk value.
    fn chunk(&mut self, offset: usize, bytes: &[u8]) -> Result<(), AttError> {
        let value = [&(offset as u32).to_le_bytes()[..], bytes].concat();
        if value.len() <= self.model.mtu() - 3 {
            self.model.write(self.data, &value)
        } else {
            self.model.write_long(self.data, &value)
        }
    }

    /// Send `bytes` from `from` in chunks of `chunk_len`.
    fn send(&mut self, bytes: &[u8], from: usize, chunk_len: usize) -> Result<(), AttError> {
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
    let transfer = Arc::new(Transfer::default());
    let server = GattServer::new([transfer_service(&transfer)]).unwrap();
    let fake = FakeBackend::new();
    let bytes = image(1000, 7);
    for mtu in [23_u16, 185, 247, 517] {
        let mut client = Client::connect(&fake, &server, mtu);
        // The largest chunk that fits one Write Request, then the largest
        // the data characteristic accepts, which may need long writes.
        let fitting = (client.model.mtu() - 3 - 4).min(CHUNK_LEN);
        for chunk_len in [fitting, CHUNK_LEN] {
            let before = copies(&fake);
            client.begin(1000).unwrap();
            client.send(&bytes, 0, chunk_len).unwrap();
            assert_eq!(client.progress(), (1000, 1000));
            client.commit().unwrap();
            assert_eq!(
                transfer.take_completed().as_ref(),
                Some(&bytes),
                "MTU {mtu}"
            );
            // Every chunk, long-written or not, reached the handler once;
            // every command once.
            assert_eq!(
                copies(&fake) - before,
                1000_usize.div_ceil(chunk_len) + 2,
                "MTU {mtu}, chunks of {chunk_len}"
            );
        }
    }
    assert_eq!(transfer.take_completed(), None, "taken once");
    assert_eq!(fake.assert_balanced(), Ok(()));
}

#[test]
fn refused_and_failed_chunks_change_nothing_and_the_client_resumes() {
    let transfer = Arc::new(Transfer::default());
    let server = GattServer::new([transfer_service(&transfer)]).unwrap();
    let fake = FakeBackend::new();
    let mut client = Client::connect(&fake, &server, 23);
    let bytes = image(100, 3);
    let chunk_len = 16;

    client.begin(100).unwrap();
    client.chunk(0, &bytes[..16]).unwrap();
    // A native copy failure, a repeated chunk, a skipped chunk, a value too
    // short for its offset, an empty chunk, and a framework length refusal.
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
    assert_eq!(
        client.chunk(16, &[0; CHUNK_LEN + 1]),
        Err(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH),
        "over the data characteristic's MAX_LEN"
    );
    assert_eq!(client.commit(), Err(INCOMPLETE));
    assert_eq!(client.progress(), (16, 100), "nothing changed");

    // Resume from the reported offset; a chunk past the total is refused.
    let (received, _) = client.progress();
    client.send(&bytes[..96], received, chunk_len).unwrap();
    assert_eq!(
        client.chunk(96, &[0; 5]),
        Err(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH)
    );
    client.chunk(96, &bytes[96..]).unwrap();
    client.commit().unwrap();
    assert_eq!(transfer.take_completed(), Some(bytes));

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
fn disconnect_and_abort_discard_the_session_and_a_new_transfer_completes() {
    let transfer = Arc::new(Transfer::default());
    let server = GattServer::new([transfer_service(&transfer)]).unwrap();
    let fake = FakeBackend::new();
    let mut client = Client::connect(&fake, &server, 23);
    let first = image(300, 1);

    // The link drops mid-transfer with a long write half-queued.
    client.begin(300).unwrap();
    client.send(&first[..120], 0, 40).unwrap();
    client
        .model
        .prepare(client.data, 0, &(120_u32).to_le_bytes())
        .unwrap();
    on_disconnect(&mut client, &transfer);
    assert_eq!(client.model.execute(true), Ok(()), "nothing was queued");
    assert_eq!(client.progress(), (0, 0));
    assert_eq!(client.chunk(120, &first[120..160]), Err(NO_TRANSFER));

    // A new client aborts, then begins, as the pattern recommends.
    let mut client = Client::connect(&fake, &server, 247);
    client.abort().unwrap();
    let second = image(300, 2);
    client.begin(300).unwrap();
    client.send(&second, 0, CHUNK_LEN).unwrap();
    client.commit().unwrap();

    // A completed transfer the application has not taken is also discarded.
    on_disconnect(&mut client, &transfer);
    assert_eq!(transfer.take_completed(), None);

    let mut client = Client::connect(&fake, &server, 247);
    client.begin(300).unwrap();
    client.send(&second, 0, CHUNK_LEN).unwrap();
    client.abort().unwrap();
    assert_eq!(client.progress(), (0, 0));
    assert_eq!(client.commit(), Err(NO_TRANSFER));
    client.begin(300).unwrap();
    client.send(&first, 0, 100).unwrap();
    client.commit().unwrap();
    assert_eq!(transfer.take_completed(), Some(first));
    assert_eq!(fake.assert_balanced(), Ok(()));
}

// An application-managed chunked transfer: the client begins a transfer of a
// known length, writes chunks at increasing offsets, and commits, which hands
// the bytes to the application. Framing, limits, errors, and session state
// below belong to the application, not the framework; this is one generic
// way to build such a protocol.

use argyle_nimble::codec::{Decode, DecodeError, Encode, EncodeError, ValueReader, ValueWriter};
use argyle_nimble::gatt::{Characteristic, CharacteristicDef, Readable, Service, Writable};
use argyle_nimble::{AttError, Uuid};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

// Example UUIDs only; generate your own for a real service.
const SERVICE: Uuid = Uuid::Uuid128(0x8d4f_0000_5c1e_4b7a_9f62_0d3c_2a1b_9e70);
const CONTROL: Uuid = Uuid::Uuid128(0x8d4f_0001_5c1e_4b7a_9f62_0d3c_2a1b_9e70);
const STATUS: Uuid = Uuid::Uuid128(0x8d4f_0002_5c1e_4b7a_9f62_0d3c_2a1b_9e70);
const DATA: Uuid = Uuid::Uuid128(0x8d4f_0003_5c1e_4b7a_9f62_0d3c_2a1b_9e70);

/// The most bytes one transfer may carry, which bounds the receive buffer.
pub const TRANSFER_LIMIT: usize = 4096;

/// The most data bytes the data characteristic accepts in one chunk. A
/// client sends at most MTU - 7 bytes per chunk (a Write Request's MTU - 3,
/// less the 4-byte offset), so each chunk is a single Write Request and never
/// a GATT long write; 240 bytes fill a Write Request at a 247-byte MTU.
pub const CHUNK_LEN: usize = 240;

/// The chunk is not the next one expected; read the status to resume.
pub const UNEXPECTED_OFFSET: AttError = application_error(0x80);
/// No transfer is in progress.
pub const NO_TRANSFER: AttError = application_error(0x81);
/// Commit was requested before every byte arrived.
pub const INCOMPLETE: AttError = application_error(0x82);
/// The application has not yet taken earlier transfers; commit again later.
pub const BUSY: AttError = application_error(0x83);

const fn application_error(code: u8) -> AttError {
    match AttError::application(code) {
        Ok(error) => error,
        Err(_) => panic!("not an application error code"),
    }
}

/// A control write: `01` and a little-endian `u32` total, `02`, or `03`.
#[derive(Debug, PartialEq)]
pub enum Command {
    /// Start receiving `total` bytes.
    Begin { total: u32 },
    /// Hand a transfer whose bytes have all arrived to the application.
    Commit,
    /// Discard the transfer in progress.
    Abort,
}

impl Decode<'_> for Command {
    fn decode(reader: &mut ValueReader<'_>) -> Result<Self, DecodeError> {
        match reader.read::<u8>()? {
            0x01 => Ok(Self::Begin {
                total: reader.read()?,
            }),
            0x02 => Ok(Self::Commit),
            0x03 => Ok(Self::Abort),
            _ => Err(DecodeError::InvalidValue {
                reason: "unknown command",
            }),
        }
    }
}

/// A data write: the chunk's offset in the transfer as a little-endian
/// `u32`, then its bytes.
#[derive(Debug, PartialEq)]
pub struct Chunk {
    pub offset: u32,
    pub bytes: Vec<u8>,
}

impl Decode<'_> for Chunk {
    fn decode(reader: &mut ValueReader<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            offset: reader.read()?,
            bytes: reader.read()?,
        })
    }
}

/// A status read: bytes received and expected, both zero when idle.
#[derive(Debug, PartialEq)]
pub struct Progress {
    pub received: u32,
    pub total: u32,
}

impl Encode for Progress {
    fn encode(&self, writer: &mut ValueWriter<'_>) -> Result<(), EncodeError> {
        writer.write(&self.received)?;
        writer.write(&self.total)
    }
}

/// A transfer in progress.
struct Receiving {
    total: usize,
    data: Vec<u8>,
}

/// The application's transfer state, shared by its characteristics.
pub struct Transfer {
    session: Mutex<Option<Receiving>>,
    completed: SyncSender<Vec<u8>>,
}

impl Transfer {
    /// State that hands each committed transfer to `completed`. Its bound
    /// is how many committed transfers may wait for the application.
    pub fn new(completed: SyncSender<Vec<u8>>) -> Self {
        Self {
            session: Mutex::new(None),
            completed,
        }
    }

    fn session(&self) -> MutexGuard<'_, Option<Receiving>> {
        // Handlers must not panic, so a poisoned lock is recovered.
        self.session.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Discard the transfer in progress, if any. Committed transfers were
    /// already handed over and are unaffected. Call this when the client's
    /// connection ends.
    pub fn reset(&self) {
        *self.session() = None;
    }

    fn command(&self, command: Command) -> Result<(), AttError> {
        let mut session = self.session();
        match command {
            Command::Begin { total } => {
                if session.is_some() {
                    return Err(AttError::PROCEDURE_ALREADY_IN_PROGRESS);
                }
                let total = usize::try_from(total)
                    .ok()
                    .filter(|total| *total <= TRANSFER_LIMIT)
                    .ok_or(AttError::OUT_OF_RANGE)?;
                *session = Some(Receiving {
                    total,
                    data: Vec::with_capacity(total),
                });
            }
            Command::Commit => {
                let receiving = session.take().ok_or(NO_TRANSFER)?;
                if receiving.data.len() != receiving.total {
                    *session = Some(receiving);
                    return Err(INCOMPLETE);
                }
                // The bytes change hands now or not at all: if the
                // application cannot take them, the transfer stays complete
                // and uncommitted, and the client commits again later.
                let total = receiving.total;
                if let Err(refused) = self.completed.try_send(receiving.data) {
                    let (error, data) = match refused {
                        TrySendError::Full(data) => (BUSY, data),
                        TrySendError::Disconnected(data) => (AttError::UNLIKELY, data),
                    };
                    *session = Some(Receiving { total, data });
                    return Err(error);
                }
            }
            Command::Abort => *session = None,
        }
        Ok(())
    }

    fn chunk(&self, chunk: Chunk) -> Result<(), AttError> {
        let mut session = self.session();
        let Some(Receiving { total, data }) = &mut *session else {
            return Err(NO_TRANSFER);
        };
        // Only the next chunk is accepted. A repeated or skipped chunk is
        // refused and changes nothing; the client resumes from the status.
        if usize::try_from(chunk.offset) != Ok(data.len()) {
            return Err(UNEXPECTED_OFFSET);
        }
        if chunk.bytes.is_empty() || data.len() + chunk.bytes.len() > *total {
            return Err(AttError::INVALID_ATTRIBUTE_VALUE_LENGTH);
        }
        data.extend_from_slice(&chunk.bytes);
        Ok(())
    }

    fn progress(&self) -> Progress {
        let (received, total) = match &*self.session() {
            None => (0, 0),
            Some(Receiving { total, data }) => (data.len(), *total),
        };
        // Both are at most TRANSFER_LIMIT.
        Progress {
            received: received as u32,
            total: total as u32,
        }
    }
}

/// Accepts commands.
pub struct Control(pub Arc<Transfer>);

impl Characteristic for Control {
    type Value = Command;
    const MAX_LEN: usize = 5;
    fn uuid(&self) -> Uuid {
        CONTROL
    }
}

impl Writable for Control {
    fn write(&self, command: Command) -> Result<(), AttError> {
        self.0.command(command)
    }
}

/// Reports progress. It fits one read response at any MTU, so it cannot
/// change part-way through a read.
pub struct Status(pub Arc<Transfer>);

impl Characteristic for Status {
    type Value = Progress;
    const MAX_LEN: usize = 8;
    fn uuid(&self) -> Uuid {
        STATUS
    }
}

impl Readable for Status {
    fn read(&self) -> Result<Progress, AttError> {
        Ok(self.0.progress())
    }
}

/// Accepts chunks. The framework refuses anything over `MAX_LEN` before it
/// is decoded.
pub struct Data(pub Arc<Transfer>);

impl Characteristic for Data {
    type Value = Chunk;
    const MAX_LEN: usize = 4 + CHUNK_LEN;
    fn uuid(&self) -> Uuid {
        DATA
    }
}

impl Writable for Data {
    fn write(&self, chunk: Chunk) -> Result<(), AttError> {
        self.0.chunk(chunk)
    }
}

/// The transfer service: control, status, and data, in that order. The
/// application keeps `transfer` to reset it when the connection ends.
pub fn transfer_service(transfer: &Arc<Transfer>) -> Service {
    Service::primary(SERVICE)
        .characteristic(CharacteristicDef::new(Control(transfer.clone())).writable())
        .characteristic(CharacteristicDef::new(Status(transfer.clone())).readable())
        .characteristic(CharacteristicDef::new(Data(transfer.clone())).writable())
}

//! Deterministic fake backend for host tests. Compiled only under `cfg(test)`:
//! consumers cannot build or run it.
//!
//! The fake records every native call in order, returns scripted SDK status
//! codes, keeps a ledger of buffer ownership, and delivers injected callbacks
//! through the same [`EventDispatcher`] production uses. [`Gate`] and
//! [`FakeBackend::hold`] let tests stop a thread at a known point and release
//! it explicitly, so overlapping operations are coordinated without sleeps.
//!
//! Passing tests here are host evidence about framework logic only; they are
//! not target, SDK, or hardware validation.

use super::dispatch::{CallbackSlot, Delivery, EventDispatcher};
use super::gap::GapEvent;
use super::native::{
    check, native_length, Backend, NativeError, NativeEvent, NativeResult, Operation,
};
use crate::gatt::registration::{CharacteristicSlot, DescriptorSlot, GattPlan};
use crate::Uuid;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

/// One-shot rendezvous: a worker calls [`Gate::pass`] and blocks until the
/// test calls [`Gate::release`]; the test can first wait for the worker to
/// arrive with [`Gate::wait_entered`].
pub(crate) struct Gate {
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

impl Gate {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new((false, false)),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, (bool, bool)> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Wait until `done` holds, failing the test after a generous limit
    /// instead of hanging when the awaited step never happens.
    fn wait_for(&self, what: &str, done: impl Fn(&(bool, bool)) -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut state = self.lock();
        while !done(&state) {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!remaining.is_zero(), "gate: {what} did not happen");
            state = self
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }

    /// Mark arrival and wait for release.
    pub(crate) fn pass(&self) {
        self.lock().0 = true;
        self.changed.notify_all();
        self.wait_for("the release", |state| state.1);
    }

    /// Block until a worker has arrived at the gate.
    pub(crate) fn wait_entered(&self) {
        self.wait_for("the worker's arrival", |state| state.0);
    }

    /// Whether a worker has arrived at the gate.
    pub(crate) fn entered(&self) -> bool {
        self.lock().0
    }

    pub(crate) fn release(&self) {
        self.lock().1 = true;
        self.changed.notify_all();
    }
}

/// Native calls in the order the backend received them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NativeCall {
    HostInit,
    HostDeinit,
    InstallCallbacks,
    RemoveCallbacks,
    HostStart,
    HostStop,
    MbufFromFlat {
        id: u32,
        length: usize,
    },
    MbufLen {
        id: u32,
    },
    MbufAppend {
        id: u32,
        length: usize,
    },
    MbufCopy {
        id: u32,
        offset: usize,
        length: usize,
    },
    MbufFree {
        id: u32,
    },
    Notify {
        connection: u16,
        attribute: u16,
        id: u32,
    },
    Terminate {
        connection: u16,
    },
    AdvertisingStop,
    Mtu {
        connection: u16,
    },
    InferAddress,
    GattCount,
    GattAdd,
    /// The registration's tables were freed.
    RegistrationFreed,
    SetDeviceName {
        name: String,
    },
    AdvertisingData(Vec<u8>),
    ScanResponseData(Vec<u8>),
    AdvertisingStart {
        address_type: u8,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MbufState {
    Live,
    Freed,
    Transferred,
}

/// Misuse that native code could not detect and that would corrupt memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LedgerViolation {
    /// The handle was used after it had been freed or transferred.
    UseAfterRelease {
        id: u32,
        operation: Operation,
        state: MbufState,
    },
    /// The handle never existed.
    UnknownHandle { id: u32, operation: Operation },
}

/// A fake native buffer handle. Like a native pointer wrapper, it is not
/// `Clone`; [`FakeBackend::forge_mbuf`] simulates a duplicated raw pointer.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct FakeMbuf {
    id: u32,
}

impl FakeMbuf {
    pub(crate) fn id(&self) -> u32 {
        self.id
    }
}

struct MbufRecord {
    data: Vec<u8>,
    state: MbufState,
    /// Segment lengths of the chain, in order; they sum to `data.len()`.
    segments: Vec<usize>,
}

struct Hold {
    gate: Arc<Gate>,
}

#[derive(Default)]
struct FakeState {
    calls: Vec<NativeCall>,
    scripted: HashMap<Operation, VecDeque<i32>>,
    mbufs: BTreeMap<u32, MbufRecord>,
    next_mbuf: u32,
    violations: Vec<LedgerViolation>,
    holds: HashMap<Operation, Hold>,
    notifications: Vec<(u16, u16, Vec<u8>)>,
    /// Links NimBLE knows, by handle, with their ATT MTU, as `ble_att_mtu`
    /// reports it.
    links: HashMap<u16, Link>,
    /// Whether an advertising procedure is active.
    advertising: bool,
    host_thread: Option<std::thread::ThreadId>,
    /// Handle slots and layout of the last registration, filled in when the
    /// host starts, as NimBLE does.
    registered: Option<(Arc<[AtomicU16]>, Layout)>,
    /// Leave value handles unassigned at host start, as when NimBLE's
    /// attribute allocation fails with assertions disabled.
    skip_handle_assignment: bool,
    /// NimBLE's sync state: set by injected sync and reset callbacks, as
    /// NimBLE sets it before delivering them, or by the test.
    synced: bool,
}

/// A link as the fake host knows it.
#[derive(Clone, Copy, Debug)]
struct Link {
    mtu: u16,
    /// The controller already dropped it; the host has not freed it yet.
    controller_gone: bool,
    /// A termination was requested.
    terminating: bool,
}

impl Link {
    fn new(mtu: u16) -> Self {
        Self {
            mtu,
            controller_gone: false,
            terminating: false,
        }
    }
}

/// Per service, each characteristic's notify flag, descriptor count, and
/// handle-slot index.
type Layout = Vec<Vec<(bool, usize, usize)>>;

/// A characteristic as the fake registered it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FakeCharacteristic {
    pub(crate) uuid: Uuid,
    pub(crate) access: crate::gatt::Access,
    pub(crate) slot: CharacteristicSlot,
    pub(crate) descriptors: Vec<(Uuid, crate::gatt::DescriptorAccess, DescriptorSlot)>,
    pub(crate) handle_index: usize,
}

/// A service as the fake registered it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FakeService {
    pub(crate) uuid: Uuid,
    pub(crate) characteristics: Vec<FakeCharacteristic>,
}

/// The fake's tables: a snapshot of the plan and the value-handle slots it
/// fills on registration. Dropping it records
/// [`NativeCall::RegistrationFreed`].
pub(crate) struct FakeRegistration {
    pub(crate) services: Vec<FakeService>,
    handles: Arc<[AtomicU16]>,
    state: Arc<Mutex<FakeState>>,
}

// SAFETY: the slot pointers are only compared and dereferenced by tests while
// the server they point into is alive, as native code would.
unsafe impl Send for FakeRegistration {}
// SAFETY: as above; the handles are atomics.
unsafe impl Sync for FakeRegistration {}

impl Drop for FakeRegistration {
    fn drop(&mut self) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .calls
            .push(NativeCall::RegistrationFreed);
    }
}

/// Cheaply cloneable handle to one shared fake host.
#[derive(Clone)]
pub(crate) struct FakeBackend {
    state: Arc<Mutex<FakeState>>,
    /// The same callback slot type the ESP backend uses.
    callbacks: Arc<CallbackSlot>,
}

impl Default for FakeBackend {
    fn default() -> Self {
        Self {
            state: Arc::default(),
            callbacks: Arc::new(CallbackSlot::new()),
        }
    }
}

impl FakeBackend {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, FakeState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Make the next call of `operation` return `code` (FIFO per operation).
    /// Unscripted calls succeed.
    pub(crate) fn fail_next(&self, operation: Operation, code: i32) {
        self.lock()
            .scripted
            .entry(operation)
            .or_default()
            .push_back(code);
    }

    /// Stop the next call of `operation` after it is recorded and before it
    /// completes. Use the returned gate to wait for it and release it.
    pub(crate) fn hold(&self, operation: Operation) -> Arc<Gate> {
        let gate = Arc::new(Gate::new());
        self.lock()
            .holds
            .insert(operation, Hold { gate: gate.clone() });
        gate
    }

    /// Make the next host start leave every value handle unassigned.
    pub(crate) fn skip_handle_assignment(&self) {
        self.lock().skip_handle_assignment = true;
    }

    /// Treat `thread` as the native host task.
    pub(crate) fn set_host_thread(&self, thread: std::thread::ThreadId) {
        self.lock().host_thread = Some(thread);
    }

    /// Set the sync state without a callback, as NimBLE does at the start of
    /// a host reset before it reports the reset's GAP events.
    pub(crate) fn set_synced(&self, synced: bool) {
        self.lock().synced = synced;
    }

    /// Model a link NimBLE knows, with its ATT MTU (23 until an exchange).
    pub(crate) fn set_mtu(&self, connection: u16, mtu: u16) {
        self.lock()
            .links
            .entry(connection)
            .or_insert(Link::new(mtu))
            .mtu = mtu;
    }

    /// Model the controller accepting a client: NimBLE creates the link and
    /// ends advertising (`ble_gap_rx_conn_complete` resets the advertising
    /// state) before it reports the connection, which ESP-IDF does only
    /// after reading the client's version and features.
    pub(crate) fn create_link(&self, connection: u16) {
        let mut state = self.lock();
        state.links.entry(connection).or_insert(Link::new(23));
        state.advertising = false;
    }

    /// Model the controller dropping a link the host still holds:
    /// terminating it then fails with HCI Unknown Connection Identifier.
    pub(crate) fn drop_controller_link(&self, connection: u16) {
        if let Some(link) = self.lock().links.get_mut(&connection) {
            link.controller_gone = true;
        }
    }

    pub(crate) fn calls(&self) -> Vec<NativeCall> {
        self.lock().calls.clone()
    }

    pub(crate) fn notifications(&self) -> Vec<(u16, u16, Vec<u8>)> {
        self.lock().notifications.clone()
    }

    pub(crate) fn mbuf_state(&self, id: u32) -> Option<MbufState> {
        self.lock().mbufs.get(&id).map(|record| record.state)
    }

    /// Current bytes of a buffer, including after it was released.
    /// A live chained buffer holding `segments` in order, as NimBLE delivers
    /// a long write; NimBLE created it, so no native call is recorded. The
    /// test frees it, as NimBLE frees access buffers.
    pub(crate) fn mbuf_from_segments(&self, segments: &[&[u8]]) -> FakeMbuf {
        let mut state = self.lock();
        state.next_mbuf += 1;
        let id = state.next_mbuf;
        state.mbufs.insert(
            id,
            MbufRecord {
                data: segments.concat(),
                state: MbufState::Live,
                segments: segments.iter().map(|segment| segment.len()).collect(),
            },
        );
        FakeMbuf { id }
    }

    pub(crate) fn mbuf_segments(&self, id: u32) -> Option<Vec<usize>> {
        self.lock()
            .mbufs
            .get(&id)
            .map(|record| record.segments.clone())
    }

    pub(crate) fn mbuf_data(&self, id: u32) -> Option<Vec<u8>> {
        self.lock().mbufs.get(&id).map(|record| record.data.clone())
    }

    pub(crate) fn violations(&self) -> Vec<LedgerViolation> {
        self.lock().violations.clone()
    }

    /// Buffers allocated and never freed or transferred.
    pub(crate) fn live_mbufs(&self) -> Vec<u32> {
        self.lock()
            .mbufs
            .iter()
            .filter(|(_, record)| record.state == MbufState::Live)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Fail unless every buffer was released exactly once without misuse.
    pub(crate) fn assert_balanced(&self) -> Result<(), String> {
        let live = self.live_mbufs();
        let violations = self.violations();
        if live.is_empty() && violations.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "leaked buffers {live:?}; ownership violations {violations:?}"
            ))
        }
    }

    /// Simulate a second raw pointer to an existing buffer.
    pub(crate) fn forge_mbuf(&self, id: u32) -> FakeMbuf {
        FakeMbuf { id }
    }

    /// Deliver a native callback through the installed dispatcher, as the
    /// NimBLE host task would. Sync and reset callbacks first update the
    /// sync state, as NimBLE does before calling them.
    pub(crate) fn inject(&self, event: NativeEvent) -> Option<Delivery> {
        let mut free_after = None;
        {
            let mut state = self.lock();
            match &event {
                NativeEvent::HostSynced => state.synced = true,
                NativeEvent::HostReset { .. } => {
                    state.synced = false;
                    state.links.clear();
                    state.advertising = false;
                }
                // Reporting a connection does not change advertising, which
                // ended when the link was created (see `create_link`).
                NativeEvent::Gap(GapEvent::Connect {
                    connection,
                    status: 0,
                }) => {
                    state.links.entry(*connection).or_insert(Link::new(23));
                }
                // `ble_gap_conn_broken` reports a link that broke before it
                // was reported as failed with BLE_HS_EAGAIN while it still
                // holds the link, and frees it afterwards.
                NativeEvent::Gap(GapEvent::Connect { connection, status }) if *status == 1 => {
                    free_after = Some(*connection);
                }
                // NimBLE frees a link before reporting its disconnection.
                NativeEvent::Gap(GapEvent::Disconnect { connection, .. }) => {
                    state.links.remove(connection);
                }
                NativeEvent::Gap(GapEvent::AdvertisingComplete { .. }) => {
                    state.advertising = false;
                }
                NativeEvent::Gap(_) => {}
            }
        }
        let delivery = self.callbacks.deliver(event);
        if let Some(connection) = free_after {
            self.lock().links.remove(&connection);
        }
        delivery
    }

    /// Whether an advertising procedure is active.
    pub(crate) fn is_advertising(&self) -> bool {
        self.lock().advertising
    }

    /// Deliver a GAP event, as the NimBLE host task would.
    pub(crate) fn inject_gap(&self, event: GapEvent) -> Option<Delivery> {
        self.inject(NativeEvent::Gap(event))
    }

    /// Like an HCI command from a task other than NimBLE's: a scripted
    /// result, or `BLE_HS_ENOTSYNCED` (22) while the host is not
    /// synchronized (`ble_hs_hci_cmd_send_buf`), checked when the call
    /// proceeds past any hold.
    fn enter_hci(&self, operation: Operation, call: NativeCall) -> i32 {
        let code = self.enter(operation, call);
        if code == 0 && !self.lock().synced {
            22
        } else {
            code
        }
    }

    /// Record a call, consume a scripted result, then stop at a hold if one
    /// is armed for this operation. The state lock is released while held.
    fn enter(&self, operation: Operation, call: NativeCall) -> i32 {
        let (code, hold) = {
            let mut state = self.lock();
            state.calls.push(call);
            let code = state
                .scripted
                .get_mut(&operation)
                .and_then(VecDeque::pop_front)
                .unwrap_or(0);
            (code, state.holds.remove(&operation))
        };
        if let Some(hold) = hold {
            hold.gate.pass();
        }
        code
    }

    fn release(&self, id: u32, operation: Operation, to: MbufState) -> bool {
        let mut state = self.lock();
        match state.mbufs.get_mut(&id) {
            Some(record) if record.state == MbufState::Live => {
                record.state = to;
                true
            }
            Some(record) => {
                let previous = record.state;
                state.violations.push(LedgerViolation::UseAfterRelease {
                    id,
                    operation,
                    state: previous,
                });
                false
            }
            None => {
                state
                    .violations
                    .push(LedgerViolation::UnknownHandle { id, operation });
                false
            }
        }
    }

    fn with_live<T>(
        &self,
        id: u32,
        operation: Operation,
        action: impl FnOnce(&mut MbufRecord) -> T,
    ) -> Option<T> {
        let mut state = self.lock();
        let violation = match state.mbufs.get_mut(&id) {
            Some(record) if record.state == MbufState::Live => return Some(action(record)),
            Some(record) => LedgerViolation::UseAfterRelease {
                id,
                operation,
                state: record.state,
            },
            None => LedgerViolation::UnknownHandle { id, operation },
        };
        state.violations.push(violation);
        None
    }
}

impl Backend for FakeBackend {
    type Mbuf = FakeMbuf;
    type Registration = FakeRegistration;
    const MAX_LINKS: usize = 3;

    fn host_init(&self) -> NativeResult<()> {
        check(
            Operation::HostInit,
            self.enter(Operation::HostInit, NativeCall::HostInit),
        )
    }

    fn host_deinit(&self) -> NativeResult<()> {
        check(
            Operation::HostDeinit,
            self.enter(Operation::HostDeinit, NativeCall::HostDeinit),
        )
    }

    fn install_callbacks(&self, dispatcher: Arc<EventDispatcher>) -> NativeResult<()> {
        let code = self.enter(Operation::InstallCallbacks, NativeCall::InstallCallbacks);
        check(Operation::InstallCallbacks, code)?;
        self.callbacks
            .install(dispatcher)
            .map_err(|_| NativeError::Busy {
                operation: Operation::InstallCallbacks,
            })
    }

    fn remove_callbacks(&self) -> NativeResult<()> {
        let reentrant = NativeError::Reentrant {
            operation: Operation::RemoveCallbacks,
        };
        // Refuse before recording anything, as the ESP backend does.
        if self.callbacks.is_delivering_on_current_thread() {
            return Err(reentrant);
        }
        let code = self.enter(Operation::RemoveCallbacks, NativeCall::RemoveCallbacks);
        check(Operation::RemoveCallbacks, code)?;
        self.callbacks.remove(|| {}).map_err(|_| reentrant)
    }

    /// Like NimBLE's host start, which runs `ble_gatts_start`: assigns the
    /// registered attributes' handles, starting after the stack's own
    /// services. Each service has a declaration, then per characteristic a
    /// declaration, the value, the CCCD when notify-capable, and its
    /// descriptors.
    fn host_start(&self) -> NativeResult<()> {
        check(
            Operation::HostStart,
            self.enter(Operation::HostStart, NativeCall::HostStart),
        )?;
        let registered = self.lock().registered.take();
        if let Some((handles, layout)) = registered {
            if self.lock().skip_handle_assignment {
                return Ok(());
            }
            // `next` is the next free handle; the stack's services end at 0x10.
            let mut next = 0x0011_u16;
            for service in layout {
                next += 1; // service declaration
                for (notify, descriptors, slot) in service {
                    let value = next + 1; // after the characteristic declaration
                    handles[slot].store(value, Ordering::Relaxed);
                    next = value + 1 + u16::from(notify) + descriptors as u16;
                }
            }
        }
        Ok(())
    }

    fn host_stop(&self) -> NativeResult<()> {
        check(
            Operation::HostStop,
            self.enter(Operation::HostStop, NativeCall::HostStop),
        )
    }

    fn mbuf_from_flat(&self, data: &[u8]) -> NativeResult<FakeMbuf> {
        native_length(Operation::MbufFromFlat, data.len())?;
        let id = {
            let mut state = self.lock();
            state.next_mbuf += 1;
            state.next_mbuf
        };
        let code = self.enter(
            Operation::MbufFromFlat,
            NativeCall::MbufFromFlat {
                id,
                length: data.len(),
            },
        );
        if code != 0 {
            // The SDK reports allocation failure as a null buffer.
            return Err(NativeError::OutOfMemory {
                operation: Operation::MbufFromFlat,
            });
        }
        self.lock().mbufs.insert(
            id,
            MbufRecord {
                data: data.to_vec(),
                state: MbufState::Live,
                segments: vec![data.len()],
            },
        );
        Ok(FakeMbuf { id })
    }

    fn mbuf_len(&self, mbuf: &FakeMbuf) -> usize {
        self.enter(Operation::MbufLen, NativeCall::MbufLen { id: mbuf.id });
        self.with_live(mbuf.id, Operation::MbufLen, |record| record.data.len())
            .unwrap_or(0)
    }

    fn mbuf_append(&self, mbuf: &mut FakeMbuf, data: &[u8]) -> NativeResult<()> {
        native_length(Operation::MbufAppend, data.len())?;
        let code = self.enter(
            Operation::MbufAppend,
            NativeCall::MbufAppend {
                id: mbuf.id,
                length: data.len(),
            },
        );
        // Like `os_mbuf_append`, a failure does not roll back: model an
        // allocation that ran out after copying the first half.
        let copied = if code == 0 {
            data.len()
        } else {
            data.len() / 2
        };
        self.with_live(mbuf.id, Operation::MbufAppend, |record| {
            record.data.extend_from_slice(&data[..copied]);
            record.segments.push(copied);
        })
        .ok_or(NativeError::Status {
            operation: Operation::MbufAppend,
            code: -1,
        })?;
        check(Operation::MbufAppend, code)
    }

    fn mbuf_copy(
        &self,
        mbuf: &FakeMbuf,
        offset: usize,
        destination: &mut [u8],
    ) -> NativeResult<()> {
        let code = self.enter(
            Operation::MbufCopy,
            NativeCall::MbufCopy {
                id: mbuf.id,
                offset,
                length: destination.len(),
            },
        );
        check(Operation::MbufCopy, code)?;
        let out_of_range = NativeError::OutOfRange {
            operation: Operation::MbufCopy,
            offset,
            length: destination.len(),
        };
        self.with_live(mbuf.id, Operation::MbufCopy, |record| {
            let source = offset
                .checked_add(destination.len())
                .and_then(|end| record.data.get(offset..end))
                .ok_or(out_of_range)?;
            destination.copy_from_slice(source);
            Ok(())
        })
        .unwrap_or(Err(out_of_range))
    }

    fn mbuf_free(&self, mbuf: FakeMbuf) -> NativeResult<()> {
        let code = self.enter(Operation::MbufFree, NativeCall::MbufFree { id: mbuf.id });
        // The chain is released even when the SDK reports an error.
        self.release(mbuf.id, Operation::MbufFree, MbufState::Freed);
        check(Operation::MbufFree, code)
    }

    fn notify(&self, connection: u16, attribute: u16, mbuf: FakeMbuf) -> NativeResult<()> {
        let code = self.enter(
            Operation::Notify,
            NativeCall::Notify {
                connection,
                attribute,
                id: mbuf.id,
            },
        );
        // NimBLE consumes the buffer whether or not the notification succeeds.
        let data = self.with_live(mbuf.id, Operation::Notify, |record| {
            record.state = MbufState::Transferred;
            record.data.clone()
        });
        if let (Some(data), 0) = (data, code) {
            self.lock()
                .notifications
                .push((connection, attribute, data));
        }
        // NimBLE reports the result through the connection's GAP callback on
        // this thread before returning, for success and failure alike. The
        // fake models a connection whose GAP callback is set (as it is once
        // advertising has started); the event is dropped while no
        // dispatcher is installed.
        self.inject(NativeEvent::Gap(GapEvent::NotifyTransmit {
            connection,
            attribute,
            status: code,
            indication: false,
        }));
        check(Operation::Notify, code)
    }

    /// Like `ble_gap_terminate`: an unknown link fails with
    /// `BLE_HS_ENOTCONN` (7), a link the controller already dropped with HCI
    /// Unknown Connection Identifier (0x202), and a repeated request succeeds
    /// as the ESP backend maps `BLE_HS_EALREADY`; a scripted result wins.
    fn terminate(&self, connection: u16) -> NativeResult<()> {
        let code = self.enter_hci(Operation::Terminate, NativeCall::Terminate { connection });
        let mut state = self.lock();
        let code = match state.links.get_mut(&connection) {
            _ if code != 0 => code,
            None => 7,
            Some(link) if link.controller_gone => 0x202,
            Some(link) => {
                link.terminating = true;
                0
            }
        };
        drop(state);
        check(Operation::Terminate, code)
    }

    fn set_device_name(&self, name: &std::ffi::CStr) -> NativeResult<()> {
        let call = NativeCall::SetDeviceName {
            name: name.to_string_lossy().into_owned(),
        };
        check(
            Operation::DeviceName,
            self.enter(Operation::DeviceName, call),
        )
    }

    fn set_advertising_data(&self, data: &[u8]) -> NativeResult<()> {
        check(
            Operation::AdvertisingData,
            self.enter_hci(
                Operation::AdvertisingData,
                NativeCall::AdvertisingData(data.to_vec()),
            ),
        )
    }

    fn set_scan_response_data(&self, data: &[u8]) -> NativeResult<()> {
        check(
            Operation::ScanResponseData,
            self.enter_hci(
                Operation::ScanResponseData,
                NativeCall::ScanResponseData(data.to_vec()),
            ),
        )
    }

    /// Starting while advertising succeeds, as the ESP backend maps
    /// `BLE_HS_EALREADY`.
    fn advertising_start(&self, address_type: u8) -> NativeResult<()> {
        let code = self.enter_hci(
            Operation::AdvertisingStart,
            NativeCall::AdvertisingStart { address_type },
        );
        if code == 0 {
            self.lock().advertising = true;
        }
        check(Operation::AdvertisingStart, code)
    }

    fn advertising_stop(&self) -> NativeResult<()> {
        let code = self.enter_hci(Operation::AdvertisingStop, NativeCall::AdvertisingStop);
        // NimBLE stops the controller before it can fail.
        self.lock().advertising = false;
        check(Operation::AdvertisingStop, code)
    }

    fn is_synced(&self) -> bool {
        self.lock().synced
    }

    fn mtu(&self, connection: u16) -> Option<u16> {
        self.enter(Operation::Mtu, NativeCall::Mtu { connection });
        self.lock().links.get(&connection).map(|link| link.mtu)
    }

    fn prepare_gatt(&self, plan: &GattPlan) -> FakeRegistration {
        let services: Vec<FakeService> = plan
            .services
            .iter()
            .map(|service| FakeService {
                uuid: service.uuid,
                characteristics: service
                    .characteristics
                    .iter()
                    .map(|characteristic| FakeCharacteristic {
                        uuid: characteristic.uuid,
                        access: characteristic.access,
                        slot: characteristic.slot,
                        handle_index: characteristic.handle_index,
                        descriptors: characteristic
                            .descriptors
                            .iter()
                            .map(|descriptor| (descriptor.uuid, descriptor.access, descriptor.slot))
                            .collect(),
                    })
                    .collect(),
            })
            .collect();
        let count = plan.characteristics().count();
        FakeRegistration {
            services,
            handles: (0..count).map(|_| AtomicU16::new(0)).collect(),
            state: self.state.clone(),
        }
    }

    /// Like `ble_gatts_count_cfg` and `ble_gatts_add_svcs`: records the
    /// tables for the host start, which assigns handles. Both calls are
    /// all-or-nothing.
    fn register_gatt(&self, registration: &FakeRegistration) -> NativeResult<()> {
        check(
            Operation::GattCount,
            self.enter(Operation::GattCount, NativeCall::GattCount),
        )?;
        check(
            Operation::GattAdd,
            self.enter(Operation::GattAdd, NativeCall::GattAdd),
        )?;
        let layout = registration
            .services
            .iter()
            .map(|service| {
                service
                    .characteristics
                    .iter()
                    .map(|characteristic| {
                        (
                            characteristic.access.notify,
                            characteristic.descriptors.len(),
                            characteristic.handle_index,
                        )
                    })
                    .collect()
            })
            .collect();
        self.lock().registered = Some((registration.handles.clone(), layout));
        Ok(())
    }

    fn value_handles(&self, registration: &FakeRegistration) -> Vec<u16> {
        registration
            .handles
            .iter()
            .map(|handle| handle.load(Ordering::Relaxed))
            .collect()
    }

    fn is_host_task(&self) -> bool {
        self.lock().host_thread == Some(std::thread::current().id())
    }

    /// Reports a public address (type 0) unless a failure is scripted.
    fn infer_address_type(&self) -> NativeResult<u8> {
        let code = self.enter(Operation::InferAddress, NativeCall::InferAddress);
        check(Operation::InferAddress, code)?;
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::dispatch::EventSink;
    use crate::backend::mbuf::OwnedMbuf;
    use std::sync::mpsc;
    use std::thread;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<NativeEvent>>);

    impl EventSink for Recorder {
        fn on_event(&self, event: NativeEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    #[test]
    fn calls_are_logged_in_order_and_scripted_codes_are_consumed_fifo() {
        let fake = FakeBackend::new();
        fake.set_synced(true);
        fake.fail_next(Operation::HostInit, 0x103);
        fake.fail_next(Operation::HostInit, 5);
        assert_eq!(
            fake.host_init(),
            Err(NativeError::Status {
                operation: Operation::HostInit,
                code: 0x103
            })
        );
        assert_eq!(fake.host_start(), Ok(()));
        assert_eq!(
            fake.host_init(),
            Err(NativeError::Status {
                operation: Operation::HostInit,
                code: 5
            })
        );
        assert_eq!(fake.host_init(), Ok(()));
        fake.set_mtu(1, 185);
        assert_eq!(fake.mtu(1), Some(185));
        assert_eq!(fake.mtu(2), None);
        assert_eq!(fake.terminate(1), Ok(()));
        assert_eq!(fake.advertising_stop(), Ok(()));
        assert_eq!(fake.host_stop(), Ok(()));
        assert_eq!(fake.host_deinit(), Ok(()));
        assert_eq!(
            fake.calls(),
            [
                NativeCall::HostInit,
                NativeCall::HostStart,
                NativeCall::HostInit,
                NativeCall::HostInit,
                NativeCall::Mtu { connection: 1 },
                NativeCall::Mtu { connection: 2 },
                NativeCall::Terminate { connection: 1 },
                NativeCall::AdvertisingStop,
                NativeCall::HostStop,
                NativeCall::HostDeinit,
            ]
        );
    }

    #[test]
    fn injected_events_reach_the_installed_dispatcher_only_while_installed() {
        let fake = FakeBackend::new();
        assert_eq!(fake.inject(NativeEvent::HostSynced), None);
        let dispatcher = Arc::new(EventDispatcher::new());
        let recorder = Arc::new(Recorder::default());
        dispatcher.attach(recorder.clone()).unwrap();
        fake.install_callbacks(dispatcher.clone()).unwrap();
        assert_eq!(
            fake.install_callbacks(Arc::new(EventDispatcher::new())),
            Err(NativeError::Busy {
                operation: Operation::InstallCallbacks
            })
        );
        let gap = NativeEvent::Gap(GapEvent::Mtu {
            connection: 1,
            channel: 4,
            mtu: 247,
        });
        assert_eq!(
            fake.inject(NativeEvent::HostSynced),
            Some(Delivery::Delivered)
        );
        assert_eq!(fake.inject(gap.clone()), Some(Delivery::Delivered));
        fake.remove_callbacks().unwrap();
        assert_eq!(fake.inject(NativeEvent::HostSynced), None);
        assert!(!dispatcher.is_attached());
        assert_eq!(*recorder.0.lock().unwrap(), [NativeEvent::HostSynced, gap]);
    }

    #[test]
    fn a_deliberately_leaked_buffer_is_reported() {
        let fake = FakeBackend::new();
        std::mem::forget(OwnedMbuf::from_slice(&fake, b"leak").unwrap());
        assert_eq!(fake.live_mbufs(), [1]);
        assert!(fake
            .assert_balanced()
            .unwrap_err()
            .contains("leaked buffers [1]"));
    }

    #[test]
    fn a_double_free_is_reported() {
        let fake = FakeBackend::new();
        OwnedMbuf::from_slice(&fake, b"x").unwrap().free().unwrap();
        // A second raw pointer to the same chain, as an FFI bug would create.
        fake.mbuf_free(fake.forge_mbuf(1)).unwrap();
        assert_eq!(
            fake.violations(),
            [LedgerViolation::UseAfterRelease {
                id: 1,
                operation: Operation::MbufFree,
                state: MbufState::Freed
            }]
        );
        assert!(fake.assert_balanced().is_err());
    }

    #[test]
    fn a_buffer_transferred_twice_is_reported_and_not_sent_again() {
        let fake = FakeBackend::new();
        OwnedMbuf::from_slice(&fake, b"once")
            .unwrap()
            .notify(1, 9)
            .unwrap();
        fake.notify(1, 9, fake.forge_mbuf(1)).unwrap();
        assert_eq!(fake.notifications(), [(1, 9, b"once".to_vec())]);
        assert_eq!(
            fake.violations(),
            [LedgerViolation::UseAfterRelease {
                id: 1,
                operation: Operation::Notify,
                state: MbufState::Transferred
            }]
        );
    }

    #[test]
    fn use_after_free_and_unknown_handles_are_reported() {
        let fake = FakeBackend::new();
        OwnedMbuf::from_slice(&fake, b"x").unwrap().free().unwrap();
        let mut stale = fake.forge_mbuf(1);
        assert!(fake.mbuf_append(&mut stale, b"y").is_err());
        assert_eq!(fake.mbuf_len(&fake.forge_mbuf(7)), 0);
        assert_eq!(
            fake.violations(),
            [
                LedgerViolation::UseAfterRelease {
                    id: 1,
                    operation: Operation::MbufAppend,
                    state: MbufState::Freed
                },
                LedgerViolation::UnknownHandle {
                    id: 7,
                    operation: Operation::MbufLen
                },
            ]
        );
    }

    /// Blocks the first event inside the sink until its gate is released.
    struct GatedRecorder {
        gate: Arc<Gate>,
        events: Mutex<Vec<NativeEvent>>,
    }

    impl EventSink for GatedRecorder {
        fn on_event(&self, event: NativeEvent) {
            let first = self.events.lock().unwrap().is_empty();
            self.events.lock().unwrap().push(event);
            if first {
                self.gate.pass();
            }
        }
    }

    /// Notify is held mid-call and a callback is held inside the sink while
    /// another thread removes callbacks. Removal must wait for the in-flight
    /// callback, later events must be dropped, and the observable result must
    /// be the same on every run.
    fn overlapping_notify_callback_and_shutdown() -> (
        Vec<NativeCall>,
        Vec<NativeEvent>,
        Option<Delivery>,
        Vec<u32>,
    ) {
        let fake = FakeBackend::new();
        let dispatcher = Arc::new(EventDispatcher::new());
        let callback_gate = Arc::new(Gate::new());
        let sink = Arc::new(GatedRecorder {
            gate: callback_gate.clone(),
            events: Mutex::new(Vec::new()),
        });
        dispatcher.attach(sink.clone()).unwrap();
        fake.install_callbacks(dispatcher.clone()).unwrap();
        let held_notify = fake.hold(Operation::Notify);

        let notifying = {
            let fake = fake.clone();
            thread::spawn(move || {
                OwnedMbuf::from_slice(&fake, b"payload")
                    .unwrap()
                    .notify(4, 12)
            })
        };
        held_notify.wait_entered();

        let callback = {
            let fake = fake.clone();
            thread::spawn(move || fake.inject(NativeEvent::HostSynced))
        };
        callback_gate.wait_entered();

        let (removed_tx, removed_rx) = mpsc::channel();
        let removing = {
            let fake = fake.clone();
            thread::spawn(move || {
                fake.remove_callbacks().unwrap();
                removed_tx.send(()).unwrap();
            })
        };
        dispatcher.wait_until_detaching();
        assert!(
            removed_rx.try_recv().is_err(),
            "removal returned while a callback was still running"
        );
        let late = fake.inject(NativeEvent::HostReset { reason: 2 });

        callback_gate.release();
        assert_eq!(callback.join().unwrap(), Some(Delivery::Delivered));
        removed_rx.recv().unwrap();
        removing.join().unwrap();
        held_notify.release();
        notifying.join().unwrap().unwrap();
        fake.assert_balanced().unwrap();
        let events = sink.events.lock().unwrap().clone();
        (fake.calls(), events, late, fake.live_mbufs())
    }

    #[test]
    fn coordinated_overlap_is_deterministic_across_repeated_runs() {
        let first = overlapping_notify_callback_and_shutdown();
        assert_eq!(
            first.0,
            [
                NativeCall::InstallCallbacks,
                NativeCall::MbufFromFlat { id: 1, length: 7 },
                NativeCall::Notify {
                    connection: 4,
                    attribute: 12,
                    id: 1
                },
                NativeCall::RemoveCallbacks,
            ]
        );
        // The in-flight callback completed; the late event and the notify's
        // transmit event, both after removal, were not delivered.
        assert_eq!(first.1, [NativeEvent::HostSynced]);
        assert_eq!(first.2, None);
        assert!(first.3.is_empty());
        for _ in 0..50 {
            assert_eq!(overlapping_notify_callback_and_shutdown(), first);
        }
    }

    #[test]
    fn notify_reports_its_result_through_the_gap_callback_before_returning() {
        struct ThreadRecorder(Mutex<Vec<(thread::ThreadId, NativeEvent)>>);
        impl EventSink for ThreadRecorder {
            fn on_event(&self, event: NativeEvent) {
                self.0.lock().unwrap().push((thread::current().id(), event));
            }
        }
        let fake = FakeBackend::new();
        let dispatcher = Arc::new(EventDispatcher::new());
        let sink = Arc::new(ThreadRecorder(Mutex::new(Vec::new())));
        dispatcher.attach(sink.clone()).unwrap();
        fake.install_callbacks(dispatcher).unwrap();

        OwnedMbuf::from_slice(&fake, b"ok")
            .unwrap()
            .notify(1, 5)
            .unwrap();
        fake.fail_next(Operation::Notify, 14);
        assert!(OwnedMbuf::from_slice(&fake, b"no")
            .unwrap()
            .notify(1, 5)
            .is_err());
        let caller = thread::current().id();
        assert_eq!(
            *sink.0.lock().unwrap(),
            [
                (
                    caller,
                    NativeEvent::Gap(GapEvent::NotifyTransmit {
                        connection: 1,
                        attribute: 5,
                        status: 0,
                        indication: false
                    })
                ),
                (
                    caller,
                    NativeEvent::Gap(GapEvent::NotifyTransmit {
                        connection: 1,
                        attribute: 5,
                        status: 14,
                        indication: false
                    })
                ),
            ]
        );
        fake.remove_callbacks().unwrap();
        fake.assert_balanced().unwrap();
    }

    #[test]
    fn removing_callbacks_from_inside_a_callback_is_refused_and_changes_nothing() {
        struct Remover {
            fake: FakeBackend,
            result: Mutex<Option<NativeResult<()>>>,
        }
        impl EventSink for Remover {
            fn on_event(&self, _event: NativeEvent) {
                *self.result.lock().unwrap() = Some(self.fake.remove_callbacks());
            }
        }
        let fake = FakeBackend::new();
        let dispatcher = Arc::new(EventDispatcher::new());
        let sink = Arc::new(Remover {
            fake: fake.clone(),
            result: Mutex::new(None),
        });
        dispatcher.attach(sink.clone()).unwrap();
        fake.install_callbacks(dispatcher.clone()).unwrap();
        assert_eq!(
            fake.inject(NativeEvent::HostSynced),
            Some(Delivery::Delivered)
        );
        assert_eq!(
            *sink.result.lock().unwrap(),
            Some(Err(NativeError::Reentrant {
                operation: Operation::RemoveCallbacks
            }))
        );
        // Still installed and attached; a later removal from outside works.
        assert!(dispatcher.is_attached());
        assert_eq!(
            fake.inject(NativeEvent::HostSynced),
            Some(Delivery::Delivered)
        );
        fake.remove_callbacks().unwrap();
        assert!(!dispatcher.is_attached());
        assert_eq!(fake.inject(NativeEvent::HostSynced), None);
    }

    #[test]
    fn a_concurrent_second_removal_waits_for_the_first_removals_callbacks() {
        let fake = FakeBackend::new();
        let dispatcher = Arc::new(EventDispatcher::new());
        let gate = Arc::new(Gate::new());
        dispatcher
            .attach(Arc::new(GatedRecorder {
                gate: gate.clone(),
                events: Mutex::new(Vec::new()),
            }))
            .unwrap();
        fake.install_callbacks(dispatcher.clone()).unwrap();
        let callback = {
            let fake = fake.clone();
            thread::spawn(move || fake.inject(NativeEvent::HostSynced))
        };
        gate.wait_entered();

        let (done_tx, done_rx) = mpsc::channel();
        let removers = (0..2)
            .map(|index| {
                let fake = fake.clone();
                let done_tx = done_tx.clone();
                thread::spawn(move || {
                    fake.remove_callbacks().unwrap();
                    done_tx.send(index).unwrap();
                })
            })
            .collect::<Vec<_>>();
        // One remover detaches; the other waits for that detach to finish.
        dispatcher.wait_until_detaching();
        dispatcher.wait_until_detach_waiting();
        assert!(
            done_rx.try_recv().is_err(),
            "a removal returned while a callback was still running"
        );
        // Installing during removal is refused.
        assert_eq!(
            fake.install_callbacks(Arc::new(EventDispatcher::new())),
            Err(NativeError::Busy {
                operation: Operation::InstallCallbacks
            })
        );

        gate.release();
        assert_eq!(callback.join().unwrap(), Some(Delivery::Delivered));
        for remover in removers {
            remover.join().unwrap();
        }
        assert_eq!(done_rx.try_iter().count(), 2);
        assert_eq!(dispatcher.delivering_count(), 0);
        fake.install_callbacks(Arc::new(EventDispatcher::new()))
            .unwrap();
    }

    #[test]
    fn a_failed_raw_append_keeps_a_partial_payload_like_nimble() {
        let fake = FakeBackend::new();
        let mut raw = fake.mbuf_from_flat(b"ab").unwrap();
        fake.fail_next(Operation::MbufAppend, 3);
        assert!(fake.mbuf_append(&mut raw, b"cdef").is_err());
        assert_eq!(fake.mbuf_data(1), Some(b"abcd".to_vec()));
        assert_eq!(fake.mbuf_state(1), Some(MbufState::Live));
        fake.mbuf_free(raw).unwrap();
        fake.assert_balanced().unwrap();
    }

    #[test]
    fn hci_commands_are_refused_while_the_host_is_not_synchronized() {
        let fake = FakeBackend::new();
        let refused =
            |result: NativeResult<()>| matches!(result, Err(NativeError::Status { code: 22, .. }));
        assert!(refused(fake.set_advertising_data(&[])));
        assert!(refused(fake.set_scan_response_data(&[])));
        assert!(refused(fake.advertising_start(0)));
        assert!(refused(fake.advertising_stop()));
        assert!(refused(fake.terminate(1)));
        assert!(!fake.is_advertising());
        fake.set_synced(true);
        assert_eq!(fake.advertising_start(0), Ok(()));
        assert!(fake.is_advertising());
    }

    #[test]
    fn a_hold_stops_only_the_next_call_of_its_operation() {
        let fake = FakeBackend::new();
        fake.set_synced(true);
        let held = fake.hold(Operation::HostStop);
        let stopping = {
            let fake = fake.clone();
            thread::spawn(move || fake.host_stop())
        };
        held.wait_entered();
        // Other operations proceed while HostStop is held.
        assert_eq!(fake.advertising_stop(), Ok(()));
        held.release();
        assert_eq!(stopping.join().unwrap(), Ok(()));
        // The hold was consumed; a second stop does not block.
        assert_eq!(fake.host_stop(), Ok(()));
        assert_eq!(
            fake.calls(),
            [
                NativeCall::HostStop,
                NativeCall::AdvertisingStop,
                NativeCall::HostStop
            ]
        );
    }
}

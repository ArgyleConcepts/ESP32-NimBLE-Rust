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

use super::dispatch::{Delivery, EventDispatcher};
use super::gap::GapEvent;
use super::native::{
    check, native_length, Backend, NativeError, NativeEvent, NativeResult, Operation,
};
use std::collections::{BTreeMap, HashMap, VecDeque};
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

    /// Mark arrival and wait for release.
    pub(crate) fn pass(&self) {
        let mut state = self.lock();
        state.0 = true;
        self.changed.notify_all();
        while !state.1 {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    /// Block until a worker has arrived at the gate.
    pub(crate) fn wait_entered(&self) {
        let mut state = self.lock();
        while !state.0 {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
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

struct MbufRecord {
    data: Vec<u8>,
    state: MbufState,
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
    dispatcher: Option<Arc<EventDispatcher>>,
    holds: HashMap<Operation, Hold>,
    notifications: Vec<(u16, u16, Vec<u8>)>,
    mtu: HashMap<u16, u16>,
}

/// Cheaply cloneable handle to one shared fake host.
#[derive(Clone, Default)]
pub(crate) struct FakeBackend {
    state: Arc<Mutex<FakeState>>,
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

    pub(crate) fn set_mtu(&self, connection: u16, mtu: u16) {
        self.lock().mtu.insert(connection, mtu);
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
    /// NimBLE host task would.
    pub(crate) fn inject(&self, event: NativeEvent) -> Option<Delivery> {
        // Register the delivery while holding the fake's callback slot, as
        // the ESP trampolines do, so `remove_callbacks` waits for it.
        let state = self.lock();
        let dispatcher = state.dispatcher.clone()?;
        let active = dispatcher.begin();
        drop(state);
        Some(match active {
            Some(active) => {
                active.deliver(event);
                Delivery::Delivered
            }
            None => Delivery::Dropped,
        })
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
        let mut state = self.lock();
        if state.dispatcher.is_some() {
            return Err(NativeError::Busy {
                operation: Operation::InstallCallbacks,
            });
        }
        state.dispatcher = Some(dispatcher);
        Ok(())
    }

    fn remove_callbacks(&self) -> NativeResult<()> {
        let code = self.enter(Operation::RemoveCallbacks, NativeCall::RemoveCallbacks);
        check(Operation::RemoveCallbacks, code)?;
        let mut state = self.lock();
        if state
            .dispatcher
            .as_ref()
            .is_some_and(|dispatcher| dispatcher.is_delivering_on_current_thread())
        {
            return Err(NativeError::Reentrant {
                operation: Operation::RemoveCallbacks,
            });
        }
        let dispatcher = state.dispatcher.take();
        drop(state);
        if let Some(dispatcher) = dispatcher {
            // Same quiescence rule as the ESP backend: no delivery that saw
            // the installed callbacks is still running once this returns.
            dispatcher
                .detach()
                .expect("a non-reentrant detach cannot fail");
        }
        Ok(())
    }

    fn host_start(&self) -> NativeResult<()> {
        check(
            Operation::HostStart,
            self.enter(Operation::HostStart, NativeCall::HostStart),
        )
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
            record.data.extend_from_slice(&data[..copied])
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
        // this thread before returning, for success and failure alike.
        self.inject(NativeEvent::Gap(GapEvent::NotifyTransmit {
            connection,
            attribute,
            status: code,
            indication: false,
        }));
        check(Operation::Notify, code)
    }

    fn terminate(&self, connection: u16) -> NativeResult<()> {
        check(
            Operation::Terminate,
            self.enter(Operation::Terminate, NativeCall::Terminate { connection }),
        )
    }

    fn advertising_stop(&self) -> NativeResult<()> {
        check(
            Operation::AdvertisingStop,
            self.enter(Operation::AdvertisingStop, NativeCall::AdvertisingStop),
        )
    }

    fn mtu(&self, connection: u16) -> Option<u16> {
        self.enter(Operation::Mtu, NativeCall::Mtu { connection });
        self.lock().mtu.get(&connection).copied()
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
    fn a_hold_stops_only_the_next_call_of_its_operation() {
        let fake = FakeBackend::new();
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

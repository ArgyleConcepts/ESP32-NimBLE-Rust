//! Exclusive ownership of the BLE host and its startup.
//!
//! NimBLE is process-global, and native code keeps references to callback
//! state. [`Ble`] is the single owner of that host, and its type parameter
//! records the lifecycle stage:
//!
//! 1. [`Ble::take`] acquires the process-wide owner as `Ble<Configuring>`.
//!    While one owner exists, further calls fail; the owner cannot be cloned.
//! 2. Configuration methods, such as [`Ble::sync_timeout`], exist only on
//!    `Ble<Configuring>`.
//! 3. [`Ble::start`] consumes the configuring owner, takes the frozen
//!    [`GattServer`] and an explicit [`Access`] choice, starts the host, and
//!    returns `Ble<Running>` only once the host is ready.
//! 4. Dropping a running owner, or calling [`Ble::shutdown`], stops and
//!    deinitializes the host and releases ownership.
//!
//! Starting twice and configuring a running owner are compile errors, not
//! runtime checks.
//!
//! ```no_run
//! use argyle_nimble::gatt::{Characteristic, CharacteristicDef, GattServer, Readable, Service};
//! use argyle_nimble::{Access, AttError, Ble, Uuid};
//! use std::time::Duration;
//!
//! struct Level;
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
//!         Ok(90)
//!     }
//! }
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let server = GattServer::new([Service::primary(Uuid::Uuid16(0x180f))
//!     .characteristic(CharacteristicDef::new(Level).readable())])?;
//! let ble = Ble::take()?.sync_timeout(Duration::from_secs(3));
//! let running = ble.start(server, Access::Open)?;
//! // ... later work uses `running`; dropping it shuts the host down.
//! running.shutdown()?;
//! # Ok(())
//! # }
//! ```
//!
//! # Startup
//!
//! [`Ble::start`] runs these stages in order; each failure is reported as a
//! [`StartError`] naming its [`StartStage`]:
//!
//! 1. [`StartStage::HostInit`]: `nimble_port_init`, which also initializes
//!    the Bluetooth controller, then the standard GAP and GATT services.
//! 2. [`StartStage::InstallCallbacks`]: the host sync and reset callbacks.
//! 3. [`StartStage::HostStart`]: the NimBLE host task.
//! 4. [`StartStage::Synchronization`]: wait until the host reports that it
//!    is synchronized with the controller. Host resets during the wait are
//!    recorded, and NimBLE retries synchronization by itself. If the host
//!    has not synchronized within the sync timeout ([`DEFAULT_SYNC_TIMEOUT`]
//!    unless configured), startup fails with an
//!    [`ErrorKind::Timeout`] error, the last reset
//!    reason, and the host is shut down. A timeout too large to represent as
//!    a deadline waits without a limit.
//! 5. [`StartStage::AddressInference`]: choose the own-address type to
//!    advertise with, without privacy.
//!
//! The GATT server is moved into heap storage owned by the running owner
//! before any native call, so its address stays fixed however the owner
//! moves. Registering it with NimBLE is not implemented yet; until then the
//! host serves only the standard GAP and GATT services.
//!
//! # Cleanup and ownership of shared resources
//!
//! On a failed start, on drop, and in [`Ble::shutdown`], the framework undoes
//! only the stages it completed, in reverse: stop the host task,
//! deinitialize the host, then remove the callbacks and free the storage.
//! If `nimble_port_init` fails (for example because the application already
//! initialized NimBLE), nothing is deinitialized, so resources the
//! application owns are left alone.
//!
//! The framework never initializes, erases, or repairs NVS. If the
//! application's configuration uses NVS (for example PHY calibration data
//! or persisted host state), the application initializes it first and keeps
//! ownership of it.
//!
//! If a cleanup step fails, or cleanup would run inside a BLE callback on
//! the host task (where stopping the host would wait for itself), the host
//! is **poisoned**: the framework keeps its storage alive for the rest of the
//! program so native code can never reach freed memory, and every later
//! [`Ble::take`] fails until the device restarts.
//!
//! Recovery from host faults while running, and quiescing connections and
//! notifications before shutdown, are not implemented yet.
//!
//! # Builds without NimBLE
//!
//! Host builds (not ESP-IDF targets) have the same types, but [`Ble::take`]
//! always fails with an [`ErrorKind::Lifecycle`]
//! error, since there is no NimBLE host to own.

use crate::backend::dispatch::{EventDispatcher, EventSink};
use crate::backend::native::{Backend, NativeEvent};
use crate::gatt::GattServer;
use crate::{Error, ErrorKind};
use std::fmt;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How long [`Ble::start`] waits for host synchronization unless
/// [`Ble::sync_timeout`] sets another limit.
pub const DEFAULT_SYNC_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SlotState {
    Free,
    Owned,
    Poisoned,
}

/// The process-wide ownership record for one native host.
pub(crate) struct OwnerSlot {
    state: Mutex<SlotState>,
}

// Host builds have no platform owner; tests use these with the fake backend.
#[cfg_attr(not(any(test, argyle_nimble_esp)), allow(dead_code))]
impl OwnerSlot {
    pub(crate) const fn new() -> Self {
        Self {
            state: Mutex::new(SlotState::Free),
        }
    }

    fn lock(&self) -> MutexGuard<'_, SlotState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn acquire(&'static self) -> Result<Ownership, Error> {
        let mut state = self.lock();
        match *state {
            SlotState::Free => {
                *state = SlotState::Owned;
                Ok(Ownership { slot: self })
            }
            SlotState::Owned => Err(Error::new(
                ErrorKind::Lifecycle,
                Some("take"),
                "the BLE host already has an owner",
            )),
            SlotState::Poisoned => Err(Error::new(
                ErrorKind::Lifecycle,
                Some("take"),
                "the BLE host could not be shut down cleanly and cannot be used again until restart",
            )),
        }
    }
}

/// Proof of ownership. Dropping it frees the slot unless it was poisoned.
struct Ownership {
    slot: &'static OwnerSlot,
}

impl Ownership {
    fn poison(self) {
        *self.slot.lock() = SlotState::Poisoned;
        std::mem::forget(self);
    }
}

impl Drop for Ownership {
    fn drop(&mut self) {
        let mut state = self.slot.lock();
        if *state == SlotState::Owned {
            *state = SlotState::Free;
        }
    }
}

#[derive(Default)]
struct HostState {
    synced: bool,
    last_reset: Option<i32>,
}

/// Host sync and reset notifications from the native callbacks.
#[derive(Default)]
struct HostEvents {
    state: Mutex<HostState>,
    changed: Condvar,
}

impl HostEvents {
    fn lock(&self) -> MutexGuard<'_, HostState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Wait until the host is synchronized, or return the last reset reason
    /// once `timeout` has passed.
    fn wait_synced(&self, timeout: Duration) -> Result<(), Option<i32>> {
        let deadline = Instant::now().checked_add(timeout);
        let mut state = self.lock();
        loop {
            if state.synced {
                return Ok(());
            }
            state = match deadline {
                None => self
                    .changed
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(state.last_reset);
                    }
                    self.changed
                        .wait_timeout(state, remaining)
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .0
                }
            };
        }
    }
}

impl EventSink for HostEvents {
    fn on_event(&self, event: NativeEvent) {
        let mut state = self.lock();
        match event {
            NativeEvent::HostSynced => state.synced = true,
            NativeEvent::HostReset { reason } => {
                state.synced = false;
                state.last_reset = Some(reason);
            }
            // Connection events are handled by later features.
            NativeEvent::Gap(_) => return,
        }
        drop(state);
        self.changed.notify_all();
    }
}

/// Application definitions and callback state, boxed before native code is
/// involved so their addresses stay fixed while the owner moves.
struct Core {
    // Registered with NimBLE by a later ticket.
    #[cfg_attr(not(test), allow(dead_code))]
    server: GattServer,
    dispatcher: Arc<EventDispatcher>,
    events: Arc<HostEvents>,
}

/// The startup stages completed so far, which cleanup undoes in reverse.
#[derive(Clone, Copy, Debug, Default)]
struct Progress {
    initialized: bool,
    callbacks: bool,
    started: bool,
}

/// A configuring owner over any backend.
pub(crate) struct Configured<B: Backend> {
    backend: B,
    ownership: Ownership,
    sync_timeout: Duration,
}

/// Acquire `slot` for an owner that drives `backend`.
#[cfg_attr(not(any(test, argyle_nimble_esp)), allow(dead_code))]
pub(crate) fn take<B: Backend>(
    backend: B,
    slot: &'static OwnerSlot,
) -> Result<Configured<B>, Error> {
    Ok(Configured {
        backend,
        ownership: slot.acquire()?,
        sync_timeout: DEFAULT_SYNC_TIMEOUT,
    })
}

impl<B: Backend> Configured<B> {
    pub(crate) fn set_sync_timeout(&mut self, timeout: Duration) {
        self.sync_timeout = timeout;
    }

    pub(crate) fn start(self, server: GattServer) -> Result<Started<B>, StartError> {
        let events = Arc::new(HostEvents::default());
        let dispatcher = Arc::new(EventDispatcher::new());
        dispatcher
            .attach(events.clone())
            .expect("a new dispatcher has no sink");
        let mut started = Started {
            backend: self.backend,
            ownership: Some(self.ownership),
            core: Some(Box::new(Core {
                server,
                dispatcher,
                events,
            })),
            progress: Progress::default(),
            address_type: 0,
        };

        if let Err(error) = started.backend.host_init() {
            return Err(started.fail(StartStage::HostInit, error.into(), None));
        }
        started.progress.initialized = true;

        let dispatcher = started.core().dispatcher.clone();
        if let Err(error) = started.backend.install_callbacks(dispatcher) {
            return Err(started.fail(StartStage::InstallCallbacks, error.into(), None));
        }
        started.progress.callbacks = true;

        if let Err(error) = started.backend.host_start() {
            return Err(started.fail(StartStage::HostStart, error.into(), None));
        }
        started.progress.started = true;

        if let Err(last_reset) = started.core().events.wait_synced(self.sync_timeout) {
            let error = Error::new(
                ErrorKind::Timeout,
                Some("host synchronization"),
                "the host did not synchronize with the controller within the sync timeout",
            );
            return Err(started.fail(StartStage::Synchronization, error, last_reset));
        }

        match started.backend.infer_address_type() {
            Ok(address_type) => started.address_type = address_type,
            Err(error) => {
                return Err(started.fail(StartStage::AddressInference, error.into(), None));
            }
        }
        Ok(started)
    }
}

impl<B: Backend> fmt::Debug for Configured<B> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Configured")
            .field("sync_timeout", &self.sync_timeout)
            .finish_non_exhaustive()
    }
}

/// A running owner over any backend. Dropping it shuts the host down.
pub(crate) struct Started<B: Backend> {
    backend: B,
    ownership: Option<Ownership>,
    core: Option<Box<Core>>,
    progress: Progress,
    // Used by advertising in a later ticket.
    #[cfg_attr(not(test), allow(dead_code))]
    address_type: u8,
}

impl<B: Backend> Started<B> {
    fn core(&self) -> &Core {
        self.core.as_deref().expect("storage exists until shutdown")
    }

    fn fail(mut self, stage: StartStage, cause: Error, last_host_reset: Option<i32>) -> StartError {
        let cleanup = self.shutdown();
        StartError {
            stage,
            cause,
            cleanup,
            last_host_reset,
        }
    }

    /// Undo the completed stages in reverse and release ownership, or
    /// poison the host and keep the storage alive if that cannot be done.
    /// Later calls do nothing.
    pub(crate) fn shutdown(&mut self) -> Cleanup {
        let Some(ownership) = self.ownership.take() else {
            return Cleanup::Released;
        };
        let core = self.core.take().expect("storage exists until shutdown");
        // On the host task, stopping the host would wait for itself.
        let mut clean = !core.dispatcher.is_delivering_on_current_thread();
        if clean && self.progress.started {
            clean = self.backend.host_stop().is_ok();
        }
        if clean && self.progress.initialized {
            clean = self.backend.host_deinit().is_ok();
        }
        if clean && self.progress.callbacks {
            clean = self.backend.remove_callbacks().is_ok();
        }
        if clean {
            drop(core);
            drop(ownership);
            Cleanup::Released
        } else {
            // Native code may still reach the storage or the callbacks.
            Box::leak(core);
            ownership.poison();
            Cleanup::Poisoned
        }
    }
}

impl<B: Backend> fmt::Debug for Started<B> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Started")
            .field("progress", &self.progress)
            .field("owned", &self.ownership.is_some())
            .finish_non_exhaustive()
    }
}

impl<B: Backend> Drop for Started<B> {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The access policy for the GATT server.
///
/// Phase 1 supports only open access: any connected client may use every
/// declared capability, with no pairing or encryption. The choice must be
/// made explicitly when starting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Access {
    /// No pairing, bonding, or encryption is required.
    Open,
}

/// The startup stage at which [`Ble::start`] failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum StartStage {
    /// Initializing the NimBLE port and controller.
    HostInit,
    /// Installing the host sync and reset callbacks.
    InstallCallbacks,
    /// Starting the host task.
    HostStart,
    /// Waiting for the host to synchronize with the controller.
    Synchronization,
    /// Choosing the own-address type.
    AddressInference,
}

impl fmt::Display for StartStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::HostInit => "host initialization",
            Self::InstallCallbacks => "callback installation",
            Self::HostStart => "host start",
            Self::Synchronization => "host synchronization",
            Self::AddressInference => "address inference",
        })
    }
}

/// What cleanup achieved after a failed start.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Cleanup {
    /// Every completed stage was undone and ownership released;
    /// [`Ble::take`] may be called again.
    Released,
    /// A cleanup step failed, so the host is poisoned: its storage stays
    /// alive and [`Ble::take`] fails until the device restarts.
    Poisoned,
}

/// A failed [`Ble::start`]: the stage, the underlying error, and the outcome
/// of cleanup.
#[derive(Debug)]
pub struct StartError {
    stage: StartStage,
    cause: Error,
    cleanup: Cleanup,
    last_host_reset: Option<i32>,
}

impl StartError {
    /// The stage that failed.
    pub fn stage(&self) -> StartStage {
        self.stage
    }

    /// The underlying failure, also available as the error source.
    pub fn error(&self) -> &Error {
        &self.cause
    }

    /// Whether ownership was released or the host is poisoned.
    pub fn cleanup(&self) -> Cleanup {
        self.cleanup
    }

    /// The reason of the last host reset seen while waiting for
    /// synchronization, if any: a NimBLE host status, for diagnosis.
    pub fn last_host_reset(&self) -> Option<i32> {
        self.last_host_reset
    }
}

impl fmt::Display for StartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "BLE startup failed at {}: {}",
            self.stage, self.cause
        )?;
        if let Some(reason) = self.last_host_reset {
            write!(formatter, " (last host reset reason {reason})")?;
        }
        formatter.write_str(match self.cleanup {
            Cleanup::Released => "; the host was released",
            Cleanup::Poisoned => "; cleanup failed and the host is poisoned",
        })
    }
}

impl std::error::Error for StartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

#[cfg(argyle_nimble_esp)]
type Platform = crate::backend::esp::EspBackend;
#[cfg(not(argyle_nimble_esp))]
type Platform = crate::backend::unavailable::Unavailable;

#[cfg(argyle_nimble_esp)]
static OWNER: OwnerSlot = OwnerSlot::new();

/// The configuring state of [`Ble`].
pub struct Configuring {
    inner: Configured<Platform>,
}

/// The running state of [`Ble`]. Dropping the owner shuts the host down.
pub struct Running {
    inner: Started<Platform>,
}

/// The exclusive owner of the process's BLE host.
///
/// It is `Send`, so it can move to the thread that manages BLE, and it cannot
/// be cloned. Its storage is heap-allocated, so moving the owner never moves
/// anything native code refers to.
pub struct Ble<S> {
    state: S,
}

impl Ble<Configuring> {
    /// Acquire the process-wide BLE host.
    ///
    /// Fails with an [`ErrorKind::Lifecycle`]
    /// error while another owner exists, after the host was poisoned, and
    /// always in builds without NimBLE.
    pub fn take() -> Result<Self, Error> {
        #[cfg(argyle_nimble_esp)]
        {
            Ok(Self {
                state: Configuring {
                    inner: take(crate::backend::esp::EspBackend, &OWNER)?,
                },
            })
        }
        #[cfg(not(argyle_nimble_esp))]
        {
            Err(Error::new(
                ErrorKind::Lifecycle,
                Some("take"),
                "this build has no NimBLE host; build for an ESP-IDF target",
            ))
        }
    }

    /// Set how long [`start`](Self::start) waits for host synchronization;
    /// the default is [`DEFAULT_SYNC_TIMEOUT`].
    pub fn sync_timeout(mut self, timeout: Duration) -> Self {
        self.state.inner.set_sync_timeout(timeout);
        self
    }

    /// Start the host with `server` and the chosen access policy, returning
    /// the running owner once the host is ready. See the
    /// [module documentation](crate::ble) for the stages and cleanup.
    pub fn start(self, server: GattServer, access: Access) -> Result<Ble<Running>, StartError> {
        let Access::Open = access;
        Ok(Ble {
            state: Running {
                inner: self.state.inner.start(server)?,
            },
        })
    }
}

impl Ble<Running> {
    /// Shut the host down and release ownership, reporting whether cleanup
    /// succeeded. Dropping the owner does the same without a report.
    ///
    /// Fails with an [`ErrorKind::Lifecycle`]
    /// error if a cleanup step failed and the host is now poisoned.
    pub fn shutdown(mut self) -> Result<(), Error> {
        match self.state.inner.shutdown() {
            Cleanup::Released => Ok(()),
            Cleanup::Poisoned => Err(Error::new(
                ErrorKind::Lifecycle,
                Some("shutdown"),
                "the host could not be shut down cleanly and is poisoned",
            )),
        }
    }
}

impl fmt::Debug for Ble<Configuring> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Ble<Configuring>")
            .field("sync_timeout", &self.state.inner.sync_timeout)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for Ble<Running> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Ble<Running>")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::{FakeBackend, NativeCall};
    use crate::backend::native::Operation;
    use crate::gatt::{Characteristic, CharacteristicDef, Readable, Service};
    use crate::{AttError, Uuid};
    use std::sync::Barrier;
    use std::thread;

    fn slot() -> &'static OwnerSlot {
        Box::leak(Box::new(OwnerSlot::new()))
    }

    /// Records the fake host's call log at the moment the server's storage
    /// is freed.
    struct Witness {
        fake: FakeBackend,
        freed_after: Arc<Mutex<Option<Vec<NativeCall>>>>,
    }

    impl Drop for Witness {
        fn drop(&mut self) {
            *self.freed_after.lock().unwrap() = Some(self.fake.calls());
        }
    }

    impl Characteristic for Witness {
        type Value = u8;
        fn uuid(&self) -> Uuid {
            Uuid::Uuid16(0x2a19)
        }
    }

    impl Readable for Witness {
        fn read(&self) -> Result<u8, AttError> {
            Ok(1)
        }
    }

    type FreedAfter = Arc<Mutex<Option<Vec<NativeCall>>>>;

    fn witness_server(fake: &FakeBackend) -> (GattServer, FreedAfter) {
        let freed_after = FreedAfter::default();
        let witness = Witness {
            fake: fake.clone(),
            freed_after: freed_after.clone(),
        };
        let service = Service::primary(Uuid::Uuid16(0x180f))
            .characteristic(CharacteristicDef::new(witness).readable());
        (GattServer::new([service]).unwrap(), freed_after)
    }

    /// Start on another thread, delivering `events` once the host task has
    /// been asked to start.
    fn start_with(
        fake: &FakeBackend,
        configured: Configured<FakeBackend>,
        server: GattServer,
        events: Vec<NativeEvent>,
    ) -> Result<Started<FakeBackend>, StartError> {
        let gate = fake.hold(Operation::HostStart);
        let starting = thread::spawn(move || configured.start(server));
        gate.wait_entered();
        for event in events {
            fake.inject(event);
        }
        gate.release();
        starting.join().unwrap()
    }

    const STARTUP: [NativeCall; 4] = [
        NativeCall::HostInit,
        NativeCall::InstallCallbacks,
        NativeCall::HostStart,
        NativeCall::InferAddress,
    ];

    const TEARDOWN: [NativeCall; 3] = [
        NativeCall::HostStop,
        NativeCall::HostDeinit,
        NativeCall::RemoveCallbacks,
    ];

    #[test]
    fn only_one_owner_exists_at_a_time() {
        let slot = slot();
        let fake = FakeBackend::new();
        let first = take(fake.clone(), slot).unwrap();
        let second = take(fake.clone(), slot).unwrap_err();
        assert_eq!(second.kind(), ErrorKind::Lifecycle);
        assert!(second.to_string().contains("already has an owner"));
        drop(first);
        assert!(take(fake.clone(), slot).is_ok(), "released on drop");
        assert!(fake.calls().is_empty(), "acquisition makes no native calls");
    }

    #[test]
    fn concurrent_acquisition_yields_exactly_one_owner() {
        for _ in 0..20 {
            let slot = slot();
            let barrier = Arc::new(Barrier::new(8));
            let attempts: Vec<_> = (0..8)
                .map(|_| {
                    let barrier = barrier.clone();
                    thread::spawn(move || {
                        barrier.wait();
                        take(FakeBackend::new(), slot)
                    })
                })
                .collect();
            let results: Vec<_> = attempts
                .into_iter()
                .map(|attempt| attempt.join().unwrap())
                .collect();
            assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
            for error in results.iter().filter_map(|result| result.as_ref().err()) {
                assert_eq!(error.kind(), ErrorKind::Lifecycle);
            }
        }
    }

    #[test]
    fn a_running_owner_exists_only_after_sync_and_address_inference() {
        let slot = slot();
        let fake = FakeBackend::new();
        let (server, freed_after) = witness_server(&fake);
        let configured = take(fake.clone(), slot).unwrap();
        let running = start_with(&fake, configured, server, vec![NativeEvent::HostSynced])
            .expect("startup completes");
        assert_eq!(fake.calls(), STARTUP);
        assert!(freed_after.lock().unwrap().is_none(), "storage is alive");
        assert!(
            take(fake.clone(), slot).is_err(),
            "still owned while running"
        );

        drop(running);
        let mut expected = STARTUP.to_vec();
        expected.extend(TEARDOWN);
        assert_eq!(fake.calls(), expected, "no other native call, such as NVS");
        assert_eq!(freed_after.lock().unwrap().as_deref(), Some(&expected[..]));
        assert!(take(fake, slot).is_ok(), "released after shutdown");
    }

    #[test]
    fn resets_before_sync_are_survived_and_recorded() {
        let fake = FakeBackend::new();
        let (server, _) = witness_server(&fake);
        let configured = take(fake.clone(), slot()).unwrap();
        let events = vec![
            NativeEvent::HostReset { reason: 19 },
            NativeEvent::HostSynced,
        ];
        assert!(start_with(&fake, configured, server, events).is_ok());
    }

    /// Run a start that fails and check the stage, the native calls made,
    /// when the storage was freed, and the ownership outcome.
    fn failing_start(
        stage: StartStage,
        setup: impl FnOnce(&FakeBackend),
        events: Vec<NativeEvent>,
        expected: &[NativeCall],
    ) -> (StartError, &'static OwnerSlot) {
        let slot = slot();
        let fake = FakeBackend::new();
        setup(&fake);
        let (server, freed_after) = witness_server(&fake);
        let mut configured = take(fake.clone(), slot).unwrap();
        configured.set_sync_timeout(Duration::from_millis(30));
        let error = if expected.contains(&NativeCall::HostStart) {
            start_with(&fake, configured, server, events).unwrap_err()
        } else {
            configured.start(server).unwrap_err()
        };
        assert_eq!(error.stage(), stage);
        assert_eq!(fake.calls(), expected, "{stage}");
        if error.cleanup() == Cleanup::Released {
            assert_eq!(
                freed_after.lock().unwrap().as_deref(),
                Some(expected),
                "freed only after every undo step"
            );
        }
        (error, slot)
    }

    #[test]
    fn each_failing_stage_undoes_only_what_it_completed() {
        use NativeCall::*;
        // nimble_port_init failed, perhaps because the application already
        // initialized NimBLE: nothing is deinitialized.
        let (error, slot) = failing_start(
            StartStage::HostInit,
            |fake| fake.fail_next(Operation::HostInit, 0x103),
            vec![],
            &[HostInit],
        );
        assert_eq!(error.cleanup(), Cleanup::Released);
        assert_eq!(error.error().kind(), ErrorKind::Backend);
        assert!(error.to_string().contains("ESP-IDF error 259"), "{error}");
        assert!(take(FakeBackend::new(), slot).is_ok());

        let (error, _) = failing_start(
            StartStage::InstallCallbacks,
            |fake| fake.fail_next(Operation::InstallCallbacks, 1),
            vec![],
            &[HostInit, InstallCallbacks, HostDeinit],
        );
        assert_eq!(error.cleanup(), Cleanup::Released);

        let (error, _) = failing_start(
            StartStage::HostStart,
            |fake| fake.fail_next(Operation::HostStart, 1),
            vec![],
            &[
                HostInit,
                InstallCallbacks,
                HostStart,
                HostDeinit,
                RemoveCallbacks,
            ],
        );
        assert_eq!(error.cleanup(), Cleanup::Released);

        let (error, slot) = failing_start(
            StartStage::AddressInference,
            |fake| fake.fail_next(Operation::InferAddress, 6),
            vec![NativeEvent::HostSynced],
            &[
                HostInit,
                InstallCallbacks,
                HostStart,
                InferAddress,
                HostStop,
                HostDeinit,
                RemoveCallbacks,
            ],
        );
        assert_eq!(error.cleanup(), Cleanup::Released);
        assert!(take(FakeBackend::new(), slot).is_ok());
    }

    #[test]
    fn synchronization_has_a_finite_failure_outcome() {
        use NativeCall::*;
        let (error, slot) = failing_start(
            StartStage::Synchronization,
            |_| {},
            vec![NativeEvent::HostReset { reason: 19 }],
            &[
                HostInit,
                InstallCallbacks,
                HostStart,
                HostStop,
                HostDeinit,
                RemoveCallbacks,
            ],
        );
        assert_eq!(error.error().kind(), ErrorKind::Timeout);
        assert_eq!(error.last_host_reset(), Some(19));
        assert_eq!(error.cleanup(), Cleanup::Released);
        let message = error.to_string();
        assert!(message.starts_with("BLE startup failed at host synchronization: timed out"));
        assert!(message.contains("last host reset reason 19"), "{message}");
        assert!(std::error::Error::source(&error).is_some());
        assert!(take(FakeBackend::new(), slot).is_ok());
    }

    #[test]
    fn a_failed_cleanup_poisons_the_host_and_keeps_storage_alive() {
        use NativeCall::*;
        let fake = FakeBackend::new();
        fake.fail_next(Operation::HostStop, 2);
        let (server, freed_after) = witness_server(&fake);
        let slot = slot();
        let mut configured = take(fake.clone(), slot).unwrap();
        configured.set_sync_timeout(Duration::from_millis(10));
        let error = start_with(&fake, configured, server, vec![]).unwrap_err();
        assert_eq!(error.cleanup(), Cleanup::Poisoned);
        assert!(error.to_string().ends_with("the host is poisoned"));
        assert_eq!(
            fake.calls(),
            [HostInit, InstallCallbacks, HostStart, HostStop]
        );
        assert!(
            freed_after.lock().unwrap().is_none(),
            "storage is never freed"
        );
        // The native callbacks still reach live storage.
        assert!(fake.inject(NativeEvent::HostSynced).is_some());
        let error = take(fake, slot).unwrap_err();
        assert!(error
            .to_string()
            .contains("cannot be used again until restart"));
    }

    #[test]
    fn explicit_shutdown_reports_success_and_failure() {
        let fake = FakeBackend::new();
        let (server, _) = witness_server(&fake);
        let slot = slot();
        let mut running = start_with(
            &fake,
            take(fake.clone(), slot).unwrap(),
            server,
            vec![NativeEvent::HostSynced],
        )
        .unwrap();
        assert_eq!(running.shutdown(), Cleanup::Released);
        assert_eq!(
            running.shutdown(),
            Cleanup::Released,
            "a second shutdown does nothing"
        );
        drop(running);
        assert_eq!(fake.calls().len(), STARTUP.len() + TEARDOWN.len());

        let fake = FakeBackend::new();
        let (server, freed_after) = witness_server(&fake);
        let mut running = start_with(
            &fake,
            take(fake.clone(), slot).unwrap(),
            server,
            vec![NativeEvent::HostSynced],
        )
        .unwrap();
        fake.fail_next(Operation::HostDeinit, 3);
        assert_eq!(running.shutdown(), Cleanup::Poisoned);
        assert!(freed_after.lock().unwrap().is_none());
        assert!(take(fake, slot).is_err());
    }

    #[test]
    fn shutdown_inside_a_callback_poisons_without_native_calls() {
        let fake = FakeBackend::new();
        let (server, freed_after) = witness_server(&fake);
        let slot = slot();
        let mut running = start_with(
            &fake,
            take(fake.clone(), slot).unwrap(),
            server,
            vec![NativeEvent::HostSynced],
        )
        .unwrap();
        let dispatcher = running.core().dispatcher.clone();
        let delivery = dispatcher.begin().expect("the sink is attached");
        assert_eq!(running.shutdown(), Cleanup::Poisoned);
        drop(delivery);
        assert_eq!(fake.calls(), STARTUP, "stopping would wait for itself");
        assert!(freed_after.lock().unwrap().is_none());
    }

    #[test]
    fn storage_keeps_its_address_while_the_owner_moves() {
        let fake = FakeBackend::new();
        let (server, _) = witness_server(&fake);
        let running = start_with(
            &fake,
            take(fake.clone(), slot()).unwrap(),
            server,
            vec![NativeEvent::HostSynced],
        )
        .unwrap();
        let addresses = |running: &Started<FakeBackend>| {
            let core = running.core();
            (
                std::ptr::from_ref(core) as usize,
                std::ptr::from_ref(&core.server) as usize,
                Arc::as_ptr(&core.dispatcher) as usize,
            )
        };
        let before = addresses(&running);
        let moved = thread::spawn(move || {
            let boxed = Box::new(running);
            (addresses(&boxed), boxed)
        })
        .join()
        .unwrap();
        assert_eq!(moved.0, before);
        assert_eq!(moved.1.address_type, 0);
    }

    #[test]
    fn host_builds_cannot_take_the_platform_host() {
        let error = Ble::take().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Lifecycle);
        assert!(error.to_string().contains("no NimBLE host"));
        fn assert_owner<T: Send + 'static>() {}
        assert_owner::<Ble<Configuring>>();
        assert_owner::<Ble<Running>>();
        assert_owner::<StartError>();
    }
}

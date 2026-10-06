//! Exclusive ownership of the BLE host: startup, advertising, and the
//! connected client.
//!
//! NimBLE is process-global, and native code keeps references to callback
//! state. [`Ble`] is the single owner of that host, and its type parameter
//! records the lifecycle stage:
//!
//! 1. [`Ble::take`] acquires the process-wide owner as `Ble<Configuring>`.
//!    While one owner exists, further calls fail; the owner cannot be cloned.
//! 2. Configuration methods, [`Ble::sync_timeout`], [`Ble::advertise`], and
//!    [`Ble::connection_handler`], exist only on `Ble<Configuring>`.
//! 3. [`Ble::start`] consumes the configuring owner, takes the frozen
//!    [`GattServer`] and an explicit [`Access`] choice, starts the host, and
//!    returns `Ble<Running>` only once the host is ready and advertising, if
//!    configured, has started.
//! 4. `Ble<Running>` reports the connected client ([`Ble::connection`]) and
//!    can start advertising again ([`Ble::start_advertising`]).
//! 5. Dropping a running owner, or calling [`Ble::shutdown`], stops and
//!    deinitializes the host and releases ownership.
//!
//! Starting twice, configuring a running owner, and querying a configuring
//! one are compile errors, not runtime checks.
//!
//! ```no_run
//! use argyle_nimble::gatt::{Characteristic, CharacteristicDef, GattServer, Readable, Service};
//! use argyle_nimble::{Access, Advertising, AttError, Ble, ConnectionEvent, Uuid};
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
//! let (level, level_updates) = CharacteristicDef::new(Level).readable().notifiable();
//! let server = GattServer::new([Service::primary(Uuid::Uuid16(0x180f)).characteristic(level)])?;
//! let advertising = Advertising::builder()
//!     .name("argyle-demo")
//!     .service(Uuid::Uuid16(0x180f))
//!     .build()?;
//! let level_key = level_updates.key();
//! let ble = Ble::take()?
//!     .sync_timeout(Duration::from_secs(3))
//!     .advertise(advertising)
//!     .connection_handler(move |event| match event {
//!         ConnectionEvent::SubscriptionChanged { endpoint, notify, .. } if endpoint == level_key => {
//!             // Start or stop producing level updates.
//!             let _ = notify;
//!         }
//!         _ => {}
//!     });
//! let running = ble.start(server, Access::Open)?;
//! if let Some(client) = running.connection() {
//!     let _ = (client.mtu(), client.is_subscribed(&level_updates));
//! }
//! // ... later work uses `running`; dropping it shuts the host down.
//! let _server = running.shutdown()?;
//! # Ok(())
//! # }
//! ```
//!
//! # Startup
//!
//! [`Ble::start`] runs these stages in order; each failure is reported as a
//! [`StartError`] naming its [`StartStage`]:
//!
//! 1. Before any native call, the advertising configuration, if any, is
//!    checked against the server; a mismatch fails at
//!    [`StartStage::Advertising`] (see [`Advertising`]).
//! 2. [`StartStage::HostInit`]: `nimble_port_init`, which also initializes
//!    the Bluetooth controller, then the standard GAP and GATT services.
//! 3. [`StartStage::DeviceName`]: with an advertising name, set it as the
//!    GAP Device Name (`ble_svc_gap_device_name_set`).
//! 4. [`StartStage::Registration`]: build NimBLE's GATT tables from the
//!    server and hand them to NimBLE (`ble_gatts_count_cfg`,
//!    `ble_gatts_add_svcs`). This only sizes and records the services.
//! 5. [`StartStage::InstallCallbacks`]: the host sync and reset callbacks.
//! 6. [`StartStage::HostStart`]: the NimBLE host task. When it starts, NimBLE
//!    allocates the attributes and assigns their handles. In ESP-IDF 6.1 an
//!    allocation failure there (for example a server too large for the
//!    configured NimBLE memory) fails an assertion on the host task instead
//!    of returning an error: with assertions enabled the firmware aborts, and
//!    otherwise the host never synchronizes and startup times out.
//! 7. [`StartStage::Synchronization`]: wait until the host reports that it
//!    is synchronized with the controller. Host resets during the wait are
//!    recorded, and NimBLE retries synchronization by itself. If the host
//!    has not synchronized within the sync timeout ([`DEFAULT_SYNC_TIMEOUT`]
//!    unless configured), startup fails with an
//!    [`ErrorKind::Timeout`] error, the last reset
//!    reason, and the host is shut down. Timeouts below
//!    [`MIN_SYNC_TIMEOUT`] are raised to it, and a timeout too large to
//!    represent as a deadline waits without a limit.
//! 8. [`StartStage::AddressInference`]: choose the own-address type to
//!    advertise with, without privacy. Then every characteristic must have
//!    been assigned a handle, or startup fails at
//!    [`StartStage::Registration`].
//! 9. [`StartStage::Advertising`]: with an advertising configuration, send
//!    its payloads and start advertising. If the host resets after the sync
//!    wait, NimBLE refuses the commands until it resynchronizes; startup then
//!    waits for the resynchronization within the sync timeout and tries
//!    again, and fails with an [`ErrorKind::Timeout`] error if it does not
//!    come.
//!
//! The GATT server is moved into heap storage owned by the running owner
//! before any native call, so its address stays fixed however the owner
//! moves. NimBLE's tables point into that storage, and the owner keeps them
//! until NimBLE is deinitialized, including after a failed or partial
//! registration; they are freed before the server. Requests are handled as
//! described in [`gatt`](crate::gatt#request-handling).
//!
//! # Advertising
//!
//! The payloads, their placement, and their validation are described on
//! [`Advertising`]. Advertising is legacy, connectable, undirected, and
//! generally discoverable (`ADV_IND`), with NimBLE's default intervals, all
//! channels, no filter, and no time limit. It uses NimBLE's legacy
//! advertising API, which ESP-IDF 6.1's NimBLE compiles to return
//! `BLE_HS_ENOTSUP` when `CONFIG_BT_NIMBLE_EXT_ADV` is enabled; startup then
//! fails at [`StartStage::Advertising`]. Without
//! [`Ble::advertise`], the host runs without advertising, and no client can
//! connect. The firmware must allow at least two NimBLE connections
//! (`CONFIG_BT_NIMBLE_MAX_CONNECTIONS`, 3 by default); ESP builds fail to
//! compile otherwise, because restarting advertising after a failed
//! connection needs a free connection slot while NimBLE still holds the
//! failed link.
//!
//! The firmware must also disable ESP-IDF's connection re-attempt
//! (`CONFIG_BT_NIMBLE_ENABLE_CONN_REATTEMPT=n`; it is enabled by default on
//! the ESP32-C3 and ESP32-S3), or the build fails in the private C shim.
//! When a client's link fails to establish, that feature frees the link and
//! restarts advertising itself without any GAP event, outside the
//! framework's knowledge. The framework's own restarts
//! ([`AdvertisingBuilder::remain_available`]) cover the same need.
//! Supporting the re-attempt is future work.
//!
//! Advertising stops when a client connects: the controller stops it when it
//! accepts the client. ESP-IDF reports the connection only after reading the
//! client's version and features, so a start between the two (by the
//! application or a restart) would leave it running; the framework then
//! stops it when NimBLE reports the connection. Stopping clears NimBLE's
//! advertising state, so a second client the controller accepted from that
//! advertising just before the stop is refused by NimBLE and left to the
//! controller; that residual race needs a start inside this window, so the
//! stop is issued only after one. Unless
//! [`AdvertisingBuilder::remain_available`] turned it off, it restarts by
//! itself, while no client is connected, after the client disconnects,
//! after a connection attempt fails, after NimBLE ends advertising without a
//! connection, and after the host resynchronizes following a reset (the
//! payloads are sent again each time, because a reset also resets the
//! controller). A restart that fails is reported as
//! [`ConnectionEvent::AdvertisingFailed`] and tried again at the next of
//! those points; [`Ble::start_advertising`] starts it on request. Restarts
//! and requests are always sent to NimBLE, which treats advertising that is
//! already running as success.
//!
//! # The connected client
//!
//! One client is supported. Each connection gets a [`ConnectionId`] that is
//! never reused, even when NimBLE reuses its numeric connection handle, so
//! state and work tied to an earlier connection never apply to a later one.
//! The connection's ATT MTU and the endpoints it subscribed to are tracked
//! from NimBLE's events and cleared when it disconnects or the host resets.
//! ESP-IDF reports a client's connection only after reading its version and
//! features, while ATT already serves it: the MTU is read from NimBLE when
//! the connection is reported, and subscriptions made before then are
//! remembered for that link and reported right after
//! [`ConnectionEvent::Connected`]. NimBLE reports one connection per link
//! and reports each link's end (with the re-attempt disabled), so every
//! connection report is treated as a new link.
//! [`Ble::connection`] returns a snapshot, and a [`ConnectionHandler`]
//! registered with [`Ble::connection_handler`] receives each change as a
//! [`ConnectionEvent`] on the host task.
//!
//! Events that do not belong to the connected client change nothing: a
//! repeated connection report, events for a link that already ended or never
//! connected, subscriptions to attributes without a notify endpoint, and MTU
//! changes on channels other than ATT's. If a second client connects anyway
//! (legacy advertising stops at the first connection, so this needs another
//! route into the controller), the framework asks NimBLE to terminate the
//! new link and reports [`ConnectionEvent::ConnectionRejected`]; the
//! connected client is unaffected. A link whose connection attempt NimBLE
//! reports as failed is terminated too, since ESP-IDF can leave it open.
//!
//! [`Access::Open`] means the framework neither requests nor requires
//! pairing, bonding, or encryption. A client that starts pairing is answered
//! by NimBLE according to its security-manager configuration, which this
//! crate does not change, and bond storage is neither managed nor deleted.
//!
//! # Cleanup and ownership of shared resources
//!
//! On a failed start, on drop, and in [`Ble::shutdown`], the framework undoes
//! only the stages it completed, in reverse: stop advertising if it is
//! active (and never start it again), stop the host task (which also
//! terminates the client's connection), deinitialize the host (which also
//! drops NimBLE's GATT registration), then remove the callbacks and free the
//! GATT tables, the connection handler, and the storage.
//! If `nimble_port_init` fails, nothing is deinitialized, so resources the
//! application owns are left alone. With the on-chip controller enabled
//! (`CONFIG_BT_CONTROLLER_ENABLED`, the configuration Phase 1 targets), a
//! NimBLE stack the application already initialized makes it fail that way;
//! without the controller, ESP-IDF has no such guard, so the application
//! must not initialize NimBLE itself. After a released failure,
//! [`StartError::into_server`] returns the GATT server for another attempt.
//!
//! The framework never initializes, erases, or repairs NVS. If the
//! application's configuration uses NVS (for example PHY calibration data
//! or persisted host state), the application initializes it first and keeps
//! ownership of it.
//!
//! If a cleanup step fails, or cleanup would run on the host task (in any
//! BLE callback, where stopping the host would wait for itself), the host
//! is **poisoned**: the framework keeps its storage alive for the rest of the
//! program so native code can never reach freed memory, and every later
//! [`Ble::take`] fails until the device restarts. The error reports which
//! step failed. A host task that has not started its host by the time a
//! sync wait ends cannot be stopped, which also poisons. A failure to stop
//! advertising does not poison: stopping the host ends advertising anyway.
//!
//! Stopping the host waits for the host task to finish its queued work,
//! including delivering the client's disconnection to the connection
//! handler, and NimBLE also runs some callbacks (such as advertising
//! completion) on the thread that stops it. Do not drop or shut down a
//! running owner while holding a lock that a BLE callback or the connection
//! handler may take, or the shutdown will wait for itself or for the host
//! task.
//!
//! After shutdown, the old host task deletes itself asynchronously through
//! ESP-IDF's single host-task handle. Take and start the host again from a
//! thread whose priority is below the NimBLE host task's (ESP-IDF creates it
//! at `configMAX_PRIORITIES - 4`, normally 21), so the old task finishes
//! before a new one exists.
//!
//! A host reset while running ends the connection and advertising; NimBLE
//! resynchronizes by itself, and the framework reports
//! [`ConnectionEvent::HostReset`] and [`ConnectionEvent::HostSynced`] and
//! restarts advertising as described above. Sending notifications is not
//! implemented yet.
//!
//! # Builds without NimBLE
//!
//! Host builds (not ESP-IDF targets) have the same types, but [`Ble::take`]
//! always fails with an [`ErrorKind::Lifecycle`]
//! error, since there is no NimBLE host to own.

pub(crate) mod advertising;
pub(crate) mod connection;

pub use advertising::{
    Advertising, AdvertisingBuilder, AdvertisingError, LocalName, LEGACY_PAYLOAD_CAPACITY,
    MAX_DEVICE_NAME_LEN,
};
pub use connection::{
    ConnectionEvent, ConnectionHandler, ConnectionId, ConnectionInfo, DisconnectReason,
};

use crate::backend::dispatch::{EventDispatcher, EventSink};
use crate::backend::native::{Backend, NativeEvent};
use crate::gatt::registration::GattPlan;
use crate::gatt::GattServer;
use crate::{Error, ErrorKind};
use connection::Runtime;
use std::fmt;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How long [`Ble::start`] waits for host synchronization unless
/// [`Ble::sync_timeout`] sets another limit.
pub const DEFAULT_SYNC_TIMEOUT: Duration = Duration::from_secs(5);

/// The shortest sync timeout [`Ble::sync_timeout`] accepts; shorter values
/// are raised to it. A host task that has not even started its host when
/// the wait ends cannot be stopped cleanly, which poisons the host.
pub const MIN_SYNC_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SlotState {
    Free,
    Owned,
    Poisoned,
}

/// The process-wide ownership record for one native host.
pub(crate) struct OwnerSlot {
    state: Mutex<SlotState>,
    /// The last connection generation handed out. It outlives each owner, so
    /// connection identities are never reused while the program runs. A
    /// `u64` cannot wrap in practice; it is behind a lock because the
    /// ESP32-C3 and ESP32-S3 have no 64-bit atomics.
    generation: Mutex<u64>,
}

// Host builds have no platform owner; tests use these with the fake backend.
#[cfg_attr(not(any(test, argyle_nimble_esp)), allow(dead_code))]
impl OwnerSlot {
    pub(crate) const fn new() -> Self {
        Self {
            state: Mutex::new(SlotState::Free),
            generation: Mutex::new(0),
        }
    }

    /// A connection generation never handed out before by this slot.
    pub(crate) fn next_generation(&self) -> u64 {
        let mut generation = self
            .generation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *generation += 1;
        *generation
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
    /// How many times the host has synchronized.
    syncs: u64,
    last_reset: Option<i32>,
    /// How many times a waiter has blocked, so tests can deliver events
    /// while startup is waiting.
    #[cfg(test)]
    parks: usize,
}

/// Host sync and reset notifications from the native callbacks, which the
/// startup wait observes.
#[derive(Default)]
pub(crate) struct HostEvents {
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
            #[cfg(test)]
            {
                state.parks += 1;
                self.changed.notify_all();
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

impl HostEvents {
    /// How many times the host has synchronized so far.
    pub(crate) fn syncs(&self) -> u64 {
        self.lock().syncs
    }

    /// Wait until the host has synchronized more than `syncs` times in
    /// total, or until `timeout` (`None` for no limit) passes; return
    /// whether it did.
    pub(crate) fn wait_for_sync_after(&self, syncs: u64, timeout: Option<Duration>) -> bool {
        let deadline = timeout.and_then(|timeout| Instant::now().checked_add(timeout));
        let mut state = self.lock();
        loop {
            if state.syncs > syncs {
                return true;
            }
            #[cfg(test)]
            {
                state.parks += 1;
                self.changed.notify_all();
            }
            state = match deadline {
                None => self
                    .changed
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return false;
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

#[cfg(test)]
impl HostEvents {
    /// Block until a waiter has blocked at least `parks` times; a waiter
    /// that is never woken fails the test after a generous limit.
    fn wait_until_parked(&self, parks: usize) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut state = self.lock();
        while state.parks < parks {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "the waiter was not woken to block again"
            );
            state = self
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }
}

impl EventSink for HostEvents {
    fn on_event(&self, event: NativeEvent) {
        let mut state = self.lock();
        match event {
            NativeEvent::HostSynced => {
                state.synced = true;
                state.syncs += 1;
            }
            NativeEvent::HostReset { reason } => {
                state.synced = false;
                state.last_reset = Some(reason);
            }
            // The connection runtime handles GAP events.
            NativeEvent::Gap(_) => return,
        }
        drop(state);
        self.changed.notify_all();
    }
}

/// Application definitions and callback state, boxed before native code is
/// involved so their addresses stay fixed while the owner moves.
struct Core<B: Backend> {
    server: GattServer,
    dispatcher: Arc<EventDispatcher>,
    /// The dispatcher's sink: connection state, advertising, and the
    /// application's connection handler.
    runtime: Arc<Runtime<B>>,
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
    events: Arc<HostEvents>,
    advertising: Option<Advertising>,
    handler: Option<Box<dyn ConnectionHandler>>,
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
        events: Arc::default(),
        advertising: None,
        handler: None,
    })
}

impl<B: Backend> Configured<B> {
    pub(crate) fn set_sync_timeout(&mut self, timeout: Duration) {
        self.sync_timeout = timeout.max(MIN_SYNC_TIMEOUT);
    }

    pub(crate) fn set_advertising(&mut self, advertising: Advertising) {
        self.advertising = Some(advertising);
    }

    pub(crate) fn set_handler(&mut self, handler: Box<dyn ConnectionHandler>) {
        self.handler = Some(handler);
    }

    pub(crate) fn start(self, server: GattServer) -> Result<Started<B>, StartError> {
        // Checked against the server before any native call.
        let (plan, plan_error) = match self
            .advertising
            .as_ref()
            .map(|advertising| advertising.plan(&server))
        {
            None => (None, None),
            Some(Ok(plan)) => (Some(plan), None),
            Some(Err(error)) => (None, Some(error)),
        };
        let slot = self.ownership.slot;
        let runtime = Arc::new(Runtime::new(
            self.backend.clone(),
            slot,
            self.events,
            self.handler,
            plan,
        ));
        let dispatcher = Arc::new(EventDispatcher::new());
        dispatcher
            .attach(runtime.clone())
            .expect("a new dispatcher has no sink");
        let mut started = Started {
            backend: self.backend,
            ownership: Some(self.ownership),
            core: Some(Box::new(Core {
                server,
                dispatcher,
                runtime,
            })),
            registration: None,
            progress: Progress::default(),
        };
        if let Some(error) = plan_error {
            return Err(started.fail(StartStage::Advertising, error.into(), None));
        }

        if let Err(error) = started.backend.host_init() {
            return Err(started.fail(StartStage::HostInit, error.into(), None));
        }
        started.progress.initialized = true;

        // NimBLE copies the name into the GAP service, which `host_init`
        // initialized.
        let name = started
            .core()
            .runtime
            .advertising_plan()
            .and_then(|plan| plan.device_name.clone());
        if let Some(name) = name {
            if let Err(error) = started.backend.set_device_name(&name) {
                return Err(started.fail(StartStage::DeviceName, error.into(), None));
            }
        }

        // The tables are owned before NimBLE sees them, so even a partial
        // registration leaves NimBLE pointing at live storage.
        let plan = GattPlan::new(&started.core().server);
        started.registration = Some(started.backend.prepare_gatt(&plan));
        let registration = started.registration.as_ref().expect("just prepared");
        if let Err(error) = started.backend.register_gatt(registration) {
            return Err(started.fail(StartStage::Registration, error.into(), None));
        }

        let dispatcher = started.core().dispatcher.clone();
        if let Err(error) = started.backend.install_callbacks(dispatcher) {
            return Err(started.fail(StartStage::InstallCallbacks, error.into(), None));
        }
        started.progress.callbacks = true;

        if let Err(error) = started.backend.host_start() {
            return Err(started.fail(StartStage::HostStart, error.into(), None));
        }
        started.progress.started = true;

        if let Err(last_reset) = started.core().runtime.host().wait_synced(self.sync_timeout) {
            let error = Error::new(
                ErrorKind::Timeout,
                Some("host synchronization"),
                "the host did not synchronize with the controller within the sync timeout",
            );
            return Err(started.fail(StartStage::Synchronization, error, last_reset));
        }

        let address_type = match started.backend.infer_address_type() {
            Ok(address_type) => address_type,
            Err(error) => {
                return Err(started.fail(StartStage::AddressInference, error.into(), None));
            }
        };

        // NimBLE assigned the value handles when the host started. A zero
        // means its attribute allocation failed without stopping the host
        // (ESP-IDF assertions disabled); the database is unusable.
        let registration = started.registration.as_ref().expect("registered");
        let handles = started.backend.value_handles(registration);
        if handles.contains(&0) {
            let error = Error::new(
                ErrorKind::Lifecycle,
                Some("GATT registration"),
                "NimBLE did not assign every attribute handle when the host started",
            );
            return Err(started.fail(StartStage::Registration, error, None));
        }
        let value_handles = plan.endpoints().iter().cloned().zip(handles).collect();
        started.core().runtime.prepare(address_type, value_handles);

        if let Err(error) = started.core().runtime.begin(self.sync_timeout) {
            return Err(started.fail(StartStage::Advertising, error, None));
        }
        Ok(started)
    }
}

impl<B: Backend> fmt::Debug for Configured<B> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Configured")
            .field("sync_timeout", &self.sync_timeout)
            .field("advertising", &self.advertising)
            .field("handler", &self.handler.is_some())
            .finish_non_exhaustive()
    }
}

/// A running owner over any backend. Dropping it shuts the host down.
pub(crate) struct Started<B: Backend> {
    backend: B,
    ownership: Option<Ownership>,
    core: Option<Box<Core<B>>>,
    /// NimBLE's GATT tables. They point into `core` and NimBLE points into
    /// them, so they are freed after deinitialization and before `core`.
    registration: Option<B::Registration>,
    progress: Progress,
}

impl<B: Backend> Started<B> {
    fn core(&self) -> &Core<B> {
        self.core.as_deref().expect("storage exists until shutdown")
    }

    pub(crate) fn connection(&self) -> Option<ConnectionInfo> {
        self.core().runtime.connection()
    }

    pub(crate) fn start_advertising(&self) -> Result<(), Error> {
        self.core().runtime.start_advertising()
    }

    fn fail(mut self, stage: StartStage, cause: Error, last_host_reset: Option<i32>) -> StartError {
        let (cleanup, server, cleanup_error) = match self.shutdown() {
            Ok(server) => (Cleanup::Released, server, None),
            Err(error) => (Cleanup::Poisoned, None, Some(error)),
        };
        StartError {
            inner: Box::new(StartFailure {
                stage,
                cause,
                cleanup,
                cleanup_error,
                last_host_reset,
                server,
            }),
        }
    }

    /// Undo the completed stages in reverse and release ownership, returning
    /// the GATT server, which native code no longer references. If a step
    /// fails, or this runs on the host task, the host is poisoned instead:
    /// the storage stays alive and the error explains why. Later calls do
    /// nothing and return `Ok(None)`.
    pub(crate) fn shutdown(&mut self) -> Result<Option<GattServer>, Error> {
        let Some(ownership) = self.ownership.take() else {
            return Ok(None);
        };
        let core = self.core.take().expect("storage exists until shutdown");
        match self.undo(&core) {
            Ok(()) => {
                // NimBLE no longer references the tables, and they no longer
                // need the server.
                drop(self.registration.take());
                let Core { server, .. } = *core;
                drop(ownership);
                Ok(Some(server))
            }
            Err(cause) => {
                // Native code may still reach the tables, the storage, or the
                // callbacks.
                std::mem::forget(self.registration.take());
                Box::leak(core);
                ownership.poison();
                Err(Error::poisoned(cause))
            }
        }
    }

    fn undo(&mut self, core: &Core<B>) -> Result<(), Error> {
        // On the host task, which runs every native callback, stopping the
        // host would wait for itself.
        if self.backend.is_host_task() || core.dispatcher.is_delivering_on_current_thread() {
            return Err(Error::new(
                ErrorKind::Lifecycle,
                Some("shutdown"),
                "the host cannot be shut down from its own task or a BLE callback",
            ));
        }
        if self.progress.started {
            // No advertising starts after this, and active advertising
            // stops; stopping the host then terminates any connection.
            core.runtime.stop();
            self.backend.host_stop()?;
            self.progress.started = false;
        }
        if self.progress.initialized {
            self.backend.host_deinit()?;
            self.progress.initialized = false;
        }
        if self.progress.callbacks {
            self.backend.remove_callbacks()?;
            self.progress.callbacks = false;
        }
        Ok(())
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
        let _ = self.shutdown();
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
    /// Setting the GAP Device Name from the advertising configuration.
    DeviceName,
    /// Registering the GATT server's services with NimBLE.
    Registration,
    /// Installing the host sync and reset callbacks.
    InstallCallbacks,
    /// Starting the host task.
    HostStart,
    /// Waiting for the host to synchronize with the controller.
    Synchronization,
    /// Choosing the own-address type.
    AddressInference,
    /// Checking the advertising configuration against the GATT server
    /// (before any native call), or starting advertising (after every
    /// other stage).
    Advertising,
}

impl fmt::Display for StartStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::HostInit => "host initialization",
            Self::DeviceName => "device name",
            Self::Registration => "GATT registration",
            Self::InstallCallbacks => "callback installation",
            Self::HostStart => "host start",
            Self::Synchronization => "host synchronization",
            Self::AddressInference => "address inference",
            Self::Advertising => "advertising",
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
    // Boxed to keep `Result<Ble<Running>, StartError>` small.
    inner: Box<StartFailure>,
}

#[derive(Debug)]
struct StartFailure {
    stage: StartStage,
    cause: Error,
    cleanup: Cleanup,
    cleanup_error: Option<Error>,
    last_host_reset: Option<i32>,
    server: Option<GattServer>,
}

impl StartError {
    /// The stage that failed.
    pub fn stage(&self) -> StartStage {
        self.inner.stage
    }

    /// The underlying failure, also available as the error source.
    pub fn error(&self) -> &Error {
        &self.inner.cause
    }

    /// Whether ownership was released or the host is poisoned.
    pub fn cleanup(&self) -> Cleanup {
        self.inner.cleanup
    }

    /// Why cleanup failed, when the host is poisoned.
    pub fn cleanup_error(&self) -> Option<&Error> {
        self.inner.cleanup_error.as_ref()
    }

    /// Recover the GATT server for another attempt. It is returned when
    /// cleanup released the host; a poisoned host keeps it alive instead.
    pub fn into_server(self) -> Option<GattServer> {
        self.inner.server
    }

    /// The reason of the last host reset seen while waiting for
    /// synchronization, if any: a NimBLE host status, for diagnosis.
    pub fn last_host_reset(&self) -> Option<i32> {
        self.inner.last_host_reset
    }
}

impl fmt::Display for StartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "BLE startup failed at {}: {}",
            self.inner.stage, self.inner.cause
        )?;
        if let Some(reason) = self.inner.last_host_reset {
            write!(formatter, " (last host reset reason {reason})")?;
        }
        formatter.write_str(match self.inner.cleanup {
            Cleanup::Released => "; the host was released",
            Cleanup::Poisoned => "; cleanup failed and the host is poisoned",
        })
    }
}

impl std::error::Error for StartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.inner.cause)
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

    /// Advertise with `advertising` once the host is ready, and set its name
    /// as the GAP Device Name. A later call replaces the configuration.
    /// Without one the host starts but does not advertise, so no client can
    /// connect. See the [module documentation](crate::ble#advertising).
    pub fn advertise(mut self, advertising: Advertising) -> Self {
        self.state.inner.set_advertising(advertising);
        self
    }

    /// Deliver connection and host lifecycle events to `handler` while the
    /// host runs. A later call replaces the handler. See
    /// [`ConnectionHandler`] for the threading contract.
    pub fn connection_handler(mut self, handler: impl ConnectionHandler) -> Self {
        self.state.inner.set_handler(Box::new(handler));
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
    /// The connected client, or `None` while no client is connected.
    ///
    /// This is a snapshot: the client may connect, disconnect, or change its
    /// subscriptions or MTU right after it is taken. Compare its
    /// [`ConnectionId`] with later events to tell connections apart. It is
    /// safe to call from a [`ConnectionHandler`].
    pub fn connection(&self) -> Option<ConnectionInfo> {
        self.state.inner.connection()
    }

    /// Start advertising with the configuration given to
    /// [`advertise`](Ble::advertise), for example after a client
    /// disconnected while advertising does not
    /// [remain available](AdvertisingBuilder::remain_available).
    ///
    /// The request is sent to NimBLE even if advertising seems active, and
    /// succeeds if it already is. Fails
    /// with an [`ErrorKind::Lifecycle`] error when no advertising was
    /// configured, while a client is connected (one client is supported),
    /// while the host is resynchronizing after a reset, or while it shuts
    /// down; and with an [`ErrorKind::Backend`] error when NimBLE refuses.
    /// It may be called from a [`ConnectionHandler`].
    pub fn start_advertising(&self) -> Result<(), Error> {
        self.state.inner.start_advertising()
    }

    /// Shut the host down and release ownership, returning the GATT server
    /// for a later start. Dropping the owner does the same without a report.
    ///
    /// Fails with an [`ErrorKind::Lifecycle`] error if the host is now
    /// poisoned; its [`source`](std::error::Error::source) is the cleanup
    /// failure. Call it from an application thread, never from a BLE
    /// callback or the host task.
    pub fn shutdown(mut self) -> Result<GattServer, Error> {
        self.state.inner.shutdown()?.ok_or_else(|| {
            Error::new(
                ErrorKind::Lifecycle,
                Some("shutdown"),
                "the host was already shut down",
            )
        })
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

    fn new_slot() -> &'static OwnerSlot {
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

    const STARTUP: [NativeCall; 6] = [
        NativeCall::HostInit,
        NativeCall::GattCount,
        NativeCall::GattAdd,
        NativeCall::InstallCallbacks,
        NativeCall::HostStart,
        NativeCall::InferAddress,
    ];

    const TEARDOWN: [NativeCall; 4] = [
        NativeCall::HostStop,
        NativeCall::HostDeinit,
        NativeCall::RemoveCallbacks,
        NativeCall::RegistrationFreed,
    ];

    #[test]
    fn only_one_owner_exists_at_a_time() {
        let slot = new_slot();
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
            let slot = new_slot();
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
        let slot = new_slot();
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
    fn startup_waits_for_a_sync_that_arrives_while_it_is_blocked() {
        let fake = FakeBackend::new();
        let (server, _) = witness_server(&fake);
        let mut configured = take(fake.clone(), new_slot()).unwrap();
        // Far longer than the bound below, so only a prompt wake-up passes.
        configured.set_sync_timeout(Duration::from_secs(3600));
        let events = configured.events.clone();
        let starting = thread::spawn(move || configured.start(server));

        events.wait_until_parked(1);
        assert_eq!(
            fake.calls(),
            [
                NativeCall::HostInit,
                NativeCall::GattCount,
                NativeCall::GattAdd,
                NativeCall::InstallCallbacks,
                NativeCall::HostStart
            ],
            "not ready, so no address inference yet"
        );
        // A reset while waiting wakes the waiter, which blocks again.
        fake.inject(NativeEvent::HostReset { reason: 19 });
        events.wait_until_parked(2);
        assert_eq!(fake.calls().len(), 5);
        let synced_at = Instant::now();
        fake.inject(NativeEvent::HostSynced);
        let running = starting.join().unwrap().expect("ready after the sync");
        assert!(
            synced_at.elapsed() < Duration::from_secs(30),
            "the sync wakes the waiter instead of the timeout"
        );
        assert_eq!(fake.calls(), STARTUP);
        drop(running);
    }

    #[test]
    fn a_reset_after_sync_clears_readiness() {
        use NativeCall::*;
        let (error, _) = failing_start(
            StartStage::Synchronization,
            |_| {},
            vec![
                NativeEvent::HostSynced,
                NativeEvent::HostReset { reason: 7 },
            ],
            &[
                HostInit,
                GattCount,
                GattAdd,
                InstallCallbacks,
                HostStart,
                HostStop,
                HostDeinit,
                RemoveCallbacks,
                RegistrationFreed,
            ],
        );
        assert_eq!(error.error().kind(), ErrorKind::Timeout);
        assert_eq!(error.last_host_reset(), Some(7));
    }

    #[test]
    fn resets_before_sync_are_survived_and_recorded() {
        let fake = FakeBackend::new();
        let (server, _) = witness_server(&fake);
        let configured = take(fake.clone(), new_slot()).unwrap();
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
        let slot = new_slot();
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
            assert!(error.cleanup_error().is_none());
            assert!(
                freed_after.lock().unwrap().is_none(),
                "the server is handed back after every undo step, not freed"
            );
            assert!(error.inner.server.is_some());
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
            &[
                HostInit,
                GattCount,
                GattAdd,
                InstallCallbacks,
                HostDeinit,
                RegistrationFreed,
            ],
        );
        assert_eq!(error.cleanup(), Cleanup::Released);

        let (error, _) = failing_start(
            StartStage::HostStart,
            |fake| fake.fail_next(Operation::HostStart, 1),
            vec![],
            &[
                HostInit,
                GattCount,
                GattAdd,
                InstallCallbacks,
                HostStart,
                HostDeinit,
                RemoveCallbacks,
                RegistrationFreed,
            ],
        );
        assert_eq!(error.cleanup(), Cleanup::Released);

        let (error, slot) = failing_start(
            StartStage::AddressInference,
            |fake| fake.fail_next(Operation::InferAddress, 6),
            vec![NativeEvent::HostSynced],
            &[
                HostInit,
                GattCount,
                GattAdd,
                InstallCallbacks,
                HostStart,
                InferAddress,
                HostStop,
                HostDeinit,
                RemoveCallbacks,
                RegistrationFreed,
            ],
        );
        assert_eq!(error.cleanup(), Cleanup::Released);
        assert!(take(FakeBackend::new(), slot).is_ok());
    }

    #[test]
    fn registration_failures_keep_tables_until_deinit() {
        use NativeCall::*;
        // Counting failed: nothing was added, but the tables still outlive
        // deinitialization.
        let (error, slot) = failing_start(
            StartStage::Registration,
            |fake| fake.fail_next(Operation::GattCount, 3),
            vec![],
            &[HostInit, GattCount, HostDeinit, RegistrationFreed],
        );
        assert_eq!(error.cleanup(), Cleanup::Released);
        assert!(error.to_string().contains("ble_gatts_count_cfg"), "{error}");
        assert!(take(FakeBackend::new(), slot).is_ok());

        // Adding failed (it is all-or-nothing); the tables still outlive
        // deinitialization.
        let (error, _) = failing_start(
            StartStage::Registration,
            |fake| fake.fail_next(Operation::GattAdd, 6),
            vec![],
            &[HostInit, GattCount, GattAdd, HostDeinit, RegistrationFreed],
        );
        assert_eq!(error.cleanup(), Cleanup::Released);
        assert!(error.to_string().contains("GATT registration"), "{error}");

        // If deinitialization then fails, the tables are never freed.
        let fake = FakeBackend::new();
        fake.fail_next(Operation::GattAdd, 6);
        fake.fail_next(Operation::HostDeinit, 3);
        let (server, _) = witness_server(&fake);
        let error = take(fake.clone(), new_slot())
            .unwrap()
            .start(server)
            .unwrap_err();
        assert_eq!(error.cleanup(), Cleanup::Poisoned);
        assert_eq!(fake.calls(), [HostInit, GattCount, GattAdd, HostDeinit]);
    }

    #[test]
    fn unassigned_value_handles_fail_registration_after_sync() {
        use NativeCall::*;
        let (error, slot) = failing_start(
            StartStage::Registration,
            |fake| fake.skip_handle_assignment(),
            vec![NativeEvent::HostSynced],
            &[
                HostInit,
                GattCount,
                GattAdd,
                InstallCallbacks,
                HostStart,
                InferAddress,
                HostStop,
                HostDeinit,
                RemoveCallbacks,
                RegistrationFreed,
            ],
        );
        assert_eq!(error.cleanup(), Cleanup::Released);
        assert!(
            error
                .to_string()
                .contains("did not assign every attribute handle"),
            "{error}"
        );
        assert!(take(FakeBackend::new(), slot).is_ok());
    }

    #[test]
    fn value_handles_map_to_their_characteristics_and_endpoints() {
        use crate::gatt::{Characteristic, CharacteristicDef, Readable};

        struct Value(u16);

        impl Characteristic for Value {
            type Value = u8;
            fn uuid(&self) -> Uuid {
                Uuid::Uuid16(self.0)
            }
        }

        impl Readable for Value {
            fn read(&self) -> Result<u8, AttError> {
                Ok(0)
            }
        }

        let (first, first_endpoint) = CharacteristicDef::new(Value(0xff01))
            .readable()
            .notifiable();
        let (third, third_endpoint) = CharacteristicDef::new(Value(0xff03)).notifiable();
        let server = GattServer::new([
            Service::primary(Uuid::Uuid16(0x180f))
                .characteristic(first)
                .characteristic(CharacteristicDef::new(Value(0xff02)).readable()),
            Service::primary(Uuid::Uuid16(0x181c)).characteristic(third),
        ])
        .unwrap();
        let fake = FakeBackend::new();
        let running = start_with(
            &fake,
            take(fake.clone(), new_slot()).unwrap(),
            server,
            vec![NativeEvent::HostSynced],
        )
        .unwrap();
        // The fake assigns handles at host start, as NimBLE does, after the
        // stack's services: service 0x11, then declaration/value (and a CCCD
        // for notify) per characteristic.
        assert_eq!(
            running.core().runtime.value_handles(),
            [
                (Some(first_endpoint.id().clone()), 0x13),
                (None, 0x16),
                (Some(third_endpoint.id().clone()), 0x19),
            ]
        );
        let handle_of = |endpoint: &crate::gatt::NotifyEndpoint<u8>| {
            running
                .core()
                .runtime
                .value_handles()
                .iter()
                .find(|(id, _)| id.as_ref() == Some(endpoint.id()))
                .map(|(_, handle)| *handle)
        };
        assert_eq!(handle_of(&third_endpoint), Some(0x19));
        assert_eq!(handle_of(&first_endpoint), Some(0x13));
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
                GattCount,
                GattAdd,
                InstallCallbacks,
                HostStart,
                HostStop,
                HostDeinit,
                RemoveCallbacks,
                RegistrationFreed,
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
        let slot = new_slot();
        let mut configured = take(fake.clone(), slot).unwrap();
        configured.set_sync_timeout(Duration::from_millis(10));
        let error = start_with(&fake, configured, server, vec![]).unwrap_err();
        assert_eq!(error.cleanup(), Cleanup::Poisoned);
        assert!(error.to_string().ends_with("the host is poisoned"));
        let cleanup_error = error.cleanup_error().expect("the cleanup failure is kept");
        assert_eq!(cleanup_error.kind(), ErrorKind::Lifecycle);
        assert!(
            cleanup_error.to_string().contains("nimble_port_stop"),
            "{cleanup_error}"
        );
        assert!(
            error.inner.server.is_none(),
            "a poisoned host keeps the server alive"
        );
        assert_eq!(
            fake.calls(),
            [
                HostInit,
                GattCount,
                GattAdd,
                InstallCallbacks,
                HostStart,
                HostStop
            ],
            "the tables are never freed"
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

    fn started_owner(
        fake: &FakeBackend,
        slot: &'static OwnerSlot,
    ) -> (Started<FakeBackend>, FreedAfter) {
        let (server, freed_after) = witness_server(fake);
        let configured = take(fake.clone(), slot).unwrap();
        let running = start_with(fake, configured, server, vec![NativeEvent::HostSynced]).unwrap();
        (running, freed_after)
    }

    #[test]
    fn explicit_shutdown_reports_success_and_failure() {
        let fake = FakeBackend::new();
        let slot = new_slot();
        let (mut running, _) = started_owner(&fake, slot);
        assert!(running.shutdown().unwrap().is_some());
        assert!(
            running.shutdown().unwrap().is_none(),
            "a second shutdown does nothing"
        );
        drop(running);
        assert_eq!(fake.calls().len(), STARTUP.len() + TEARDOWN.len());

        let fake = FakeBackend::new();
        let (mut running, freed_after) = started_owner(&fake, slot);
        fake.fail_next(Operation::HostDeinit, 3);
        let error = running.shutdown().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Lifecycle);
        assert!(error.to_string().contains("is poisoned"), "{error}");
        let source = std::error::Error::source(&error).expect("the failed step");
        assert!(
            source.to_string().contains("nimble_port_deinit"),
            "{source}"
        );
        assert!(freed_after.lock().unwrap().is_none());
        assert!(take(fake, slot).is_err());
    }

    #[test]
    fn a_released_failure_returns_the_server_for_a_restart() {
        let fake = FakeBackend::new();
        let slot = new_slot();
        fake.fail_next(Operation::InferAddress, 6);
        let (server, freed_after) = witness_server(&fake);
        let error = start_with(
            &fake,
            take(fake.clone(), slot).unwrap(),
            server,
            vec![NativeEvent::HostSynced],
        )
        .unwrap_err();
        let server = error.into_server().expect("released");
        // The same callback slot and owner slot work again.
        let mut running = start_with(
            &fake,
            take(fake.clone(), slot).unwrap(),
            server,
            vec![NativeEvent::HostSynced],
        )
        .expect("restart");
        assert!(running.shutdown().unwrap().is_some());
        let mut expected = STARTUP.to_vec();
        expected.extend(TEARDOWN);
        assert_eq!(fake.calls()[expected.len()..], expected[..]);
        assert!(take(fake, slot).is_ok());
        assert!(freed_after.lock().unwrap().is_some());
    }

    #[test]
    fn shutdown_inside_a_callback_poisons_without_native_calls() {
        let fake = FakeBackend::new();
        let slot = new_slot();
        let (mut running, freed_after) = started_owner(&fake, slot);
        let dispatcher = running.core().dispatcher.clone();
        let delivery = dispatcher.begin().expect("the sink is attached");
        let error = running.shutdown().unwrap_err();
        assert!(std::error::Error::source(&error)
            .unwrap()
            .to_string()
            .contains("BLE callback"));
        drop(delivery);
        assert_eq!(fake.calls(), STARTUP, "stopping would wait for itself");
        assert!(freed_after.lock().unwrap().is_none());

        // Dropping the owner inside a callback poisons the same way.
        let fake = FakeBackend::new();
        let slot = new_slot();
        let (running, freed_after) = started_owner(&fake, slot);
        let dispatcher = running.core().dispatcher.clone();
        let delivery = dispatcher.begin().expect("the sink is attached");
        drop(running);
        drop(delivery);
        assert_eq!(fake.calls(), STARTUP);
        assert!(freed_after.lock().unwrap().is_none());
        assert!(take(fake, slot).is_err());
    }

    #[test]
    fn shutdown_on_the_host_task_poisons_without_native_calls() {
        let fake = FakeBackend::new();
        let slot = new_slot();
        let (running, freed_after) = started_owner(&fake, slot);
        // Any native callback (such as a future GATT access) runs there,
        // with or without the event dispatcher.
        fake.set_host_thread(thread::current().id());
        drop(running);
        assert_eq!(fake.calls(), STARTUP);
        assert!(freed_after.lock().unwrap().is_none());
        assert!(take(fake, slot).is_err());
    }

    #[test]
    fn events_during_teardown_reach_live_storage() {
        let fake = FakeBackend::new();
        let slot = new_slot();
        let (running, freed_after) = started_owner(&fake, slot);
        let gate = fake.hold(Operation::HostStop);
        let stopping = thread::spawn(move || drop(running));
        gate.wait_entered();
        // A late callback while the host stops is still delivered safely.
        assert_eq!(
            fake.inject(NativeEvent::HostSynced),
            Some(crate::backend::dispatch::Delivery::Delivered)
        );
        assert!(freed_after.lock().unwrap().is_none());
        gate.release();
        stopping.join().unwrap();
        assert!(freed_after.lock().unwrap().is_some());
        assert_eq!(
            fake.inject(NativeEvent::HostSynced),
            None,
            "callbacks removed"
        );
        assert!(take(fake, slot).is_ok());
    }

    #[test]
    fn very_short_sync_timeouts_are_raised_to_the_minimum() {
        let mut configured = take(FakeBackend::new(), new_slot()).unwrap();
        configured.set_sync_timeout(Duration::ZERO);
        assert_eq!(configured.sync_timeout, MIN_SYNC_TIMEOUT);
        configured.set_sync_timeout(Duration::from_secs(9));
        assert_eq!(configured.sync_timeout, Duration::from_secs(9));
        configured.set_sync_timeout(Duration::MAX);
        assert_eq!(configured.sync_timeout, Duration::MAX);
    }

    #[test]
    fn storage_keeps_its_address_while_the_owner_moves() {
        let fake = FakeBackend::new();
        let (server, _) = witness_server(&fake);
        let running = start_with(
            &fake,
            take(fake.clone(), new_slot()).unwrap(),
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
        assert!(moved.1.connection().is_none());
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

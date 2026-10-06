//! Single-client connection state and the runtime that follows NimBLE's host
//! and GAP events. The public types carry the contract; [`Runtime`] is the
//! crate-private event sink behind [`Ble`](crate::Ble).
//!
//! # Locking
//!
//! The runtime has two locks, always taken in this order and never while the
//! application's [`ConnectionHandler`] runs:
//!
//! - `operations` serializes advertising decisions with the native calls that
//!   carry them out, so two threads never both decide to start advertising,
//!   and shutdown never races a restart. It is held across HCI commands.
//!   That cannot deadlock with the host task: NimBLE acknowledges HCI
//!   commands from the transport, not the host task
//!   (`ble_hs_hci_rx_evt` in `ble_hs_hci.c`), and calls GAP callbacks
//!   without its host lock held (`ble_gap_call_event_cb` in `ble_gap.c`).
//! - `state` guards the connection record. It is held only briefly and never
//!   across a native call, since some native calls (such as a notification)
//!   deliver GAP events synchronously on the calling thread.

use super::advertising::AdvertisingPlan;
use super::{HostEvents, OwnerSlot};
use crate::backend::dispatch::EventSink;
use crate::backend::gap::GapEvent;
use crate::backend::native::{Backend, NativeEvent};
use crate::gatt::{EndpointId, EndpointKey, NotifyEndpoint};
use crate::{Error, ErrorKind};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

/// The ATT MTU of a new LE connection before an exchange (Core Specification
/// Vol 3, Part F, 3.2.8). ESP builds check it against the SDK.
pub(crate) const ATT_DEFAULT_MTU: u16 = 23;

/// The LE fixed channel of ATT (Core Specification Vol 3, Part A, 2.1). MTU
/// events for other channels (enhanced ATT or connection-oriented channels)
/// do not change the connection's ATT MTU. ESP builds check it against the
/// SDK.
pub(crate) const ATT_CHANNEL: u16 = 0x0004;

/// NimBLE offsets HCI status codes into host statuses from this base
/// (`BLE_HS_ERR_HCI_BASE`). ESP builds check it against the SDK.
pub(crate) const HCI_STATUS_BASE: i32 = 0x200;

/// The identity of one client connection.
///
/// Each accepted connection gets a new identity, and identities are never
/// reused while the program runs, including after the host is shut down and
/// started again. NimBLE reuses its numeric connection handles, but an
/// identity from an earlier connection never matches a later one, so state
/// and work tied to it cannot carry over to a new client. It cannot be built
/// from a number or native handle.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct ConnectionId {
    generation: u64,
}

impl fmt::Debug for ConnectionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ConnectionId({})", self.generation)
    }
}

/// Why a connection ended, as NimBLE reports it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisconnectReason(i32);

impl DisconnectReason {
    /// The NimBLE host status: an HCI reason offset by `0x200` when the
    /// controller ended the link, or a host status such as a reset reason
    /// when the host did.
    pub fn status(self) -> i32 {
        self.0
    }

    /// The HCI reason code (Core Specification Vol 1, Part F), when the
    /// controller reported one; for example `0x13` when the client ended the
    /// connection, or `0x08` for a supervision timeout.
    pub fn hci_reason(self) -> Option<u8> {
        if (HCI_STATUS_BASE..HCI_STATUS_BASE + 0x100).contains(&self.0) {
            Some((self.0 - HCI_STATUS_BASE) as u8)
        } else {
            None
        }
    }
}

impl fmt::Display for DisconnectReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.hci_reason() {
            Some(reason) => write!(formatter, "HCI reason 0x{reason:02x}"),
            None => write!(formatter, "NimBLE host status {}", self.0),
        }
    }
}

/// A snapshot of the connected client, from
/// [`Ble::connection`](crate::Ble::connection).
#[derive(Clone, Debug)]
pub struct ConnectionInfo {
    id: ConnectionId,
    mtu: u16,
    subscriptions: Vec<EndpointKey>,
}

impl ConnectionInfo {
    /// The connection's identity.
    pub fn id(&self) -> ConnectionId {
        self.id
    }

    /// The ATT MTU: 23 until the client exchanges a larger one.
    pub fn mtu(&self) -> u16 {
        self.mtu
    }

    /// The endpoints the client has enabled notifications for, in the order
    /// it enabled them.
    pub fn subscriptions(&self) -> &[EndpointKey] {
        &self.subscriptions
    }

    /// Whether the client has enabled notifications for `endpoint`.
    pub fn is_subscribed<V>(&self, endpoint: &NotifyEndpoint<V>) -> bool {
        self.subscriptions
            .iter()
            .any(|key| key.id() == endpoint.id())
    }
}

/// A connection or host lifecycle event, delivered to the
/// [`ConnectionHandler`].
///
/// For each [`ConnectionId`], [`Connected`](Self::Connected) comes first and
/// [`Disconnected`](Self::Disconnected) last. Every
/// [`SubscriptionChanged`](Self::SubscriptionChanged) that enables an
/// endpoint is followed by one that disables it before that `Disconnected`:
/// NimBLE reports the subscriptions a connection held as ending when it
/// breaks, and the framework reports any it did not.
#[derive(Debug)]
#[non_exhaustive]
pub enum ConnectionEvent {
    /// A client connected. Advertising stopped when it did.
    Connected {
        /// The new connection.
        connection: ConnectionId,
    },
    /// The client's connection ended. Its subscriptions and MTU are gone,
    /// and its identity no longer matches any connection.
    Disconnected {
        /// The connection that ended.
        connection: ConnectionId,
        /// Why it ended.
        reason: DisconnectReason,
    },
    /// A client's connection attempt failed before it was established; no
    /// connection identity was assigned.
    ConnectionFailed {
        /// NimBLE's status: a host status, or in ESP-IDF's NimBLE an HCI
        /// status from the connection's feature exchange.
        status: i32,
    },
    /// A second client connected while one was already connected. Only one
    /// client is supported, so the framework asked NimBLE to terminate the
    /// new link; the connected client is unaffected. `termination` reports
    /// whether that request failed.
    ConnectionRejected {
        /// The result of requesting the termination.
        termination: Result<(), Error>,
    },
    /// The client enabled or disabled notifications for an endpoint.
    /// Changes to indications, which are not supported, and to the stack's
    /// own characteristics are not reported.
    SubscriptionChanged {
        /// The client's connection.
        connection: ConnectionId,
        /// The endpoint; compare with [`NotifyEndpoint::key`].
        endpoint: EndpointKey,
        /// Whether notifications are now enabled.
        notify: bool,
    },
    /// The client and the host agreed on a new ATT MTU.
    MtuChanged {
        /// The client's connection.
        connection: ConnectionId,
        /// The new ATT MTU.
        mtu: u16,
    },
    /// Advertising should have restarted by itself but could not. It is
    /// tried again on the next disconnection, failed connection, stop of
    /// advertising, or host resynchronization, or when the application calls
    /// [`Ble::start_advertising`](crate::Ble::start_advertising).
    AdvertisingFailed {
        /// Why advertising could not start.
        error: Error,
    },
    /// The host reset, ending any connection (reported first) and
    /// advertising. NimBLE resynchronizes by itself.
    HostReset {
        /// The NimBLE host status that caused the reset.
        reason: i32,
    },
    /// The host resynchronized with the controller after a reset. Advertising
    /// restarts if it remains available.
    HostSynced,
}

/// Receives [`ConnectionEvent`]s from the running host.
///
/// Register one with [`Ble::connection_handler`](crate::Ble::connection_handler).
/// Closures `Fn(ConnectionEvent) + Send + Sync + 'static` implement it.
///
/// - Events are delivered one at a time on the NimBLE host task, in the order
///   NimBLE reports them. Keep the handler short and non-blocking: the host
///   processes no other BLE events while it runs.
/// - The change an event reports is applied before the event is delivered,
///   so [`Ble::connection`](crate::Ble::connection) already reflects it. The
///   events that end a connection are delivered after it is gone.
/// - The framework holds no locks of its own while calling the handler, so
///   it may take application locks and may call the running owner's methods,
///   such as [`Ble::connection`](crate::Ble::connection) and
///   [`Ble::start_advertising`](crate::Ble::start_advertising). It must not
///   shut the host down: from the host task that poisons it.
/// - Disconnections caused by shutting the host down are delivered while
///   [`Ble::shutdown`](crate::Ble::shutdown) waits, so do not shut down while
///   holding a lock the handler takes.
/// - A panic is not caught: native callbacks are `extern "C"` and ESP targets
///   build with `panic=abort`, so it aborts the program.
pub trait ConnectionHandler: Send + Sync + 'static {
    /// Handle one event.
    fn on_event(&self, event: ConnectionEvent);
}

impl<F> ConnectionHandler for F
where
    F: Fn(ConnectionEvent) + Send + Sync + 'static,
{
    fn on_event(&self, event: ConnectionEvent) {
        self(event);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    /// The host is starting; advertising has not been started.
    Starting,
    /// Startup completed; advertising may restart by itself.
    Running,
    /// Shutdown began; advertising never starts again.
    Stopping,
}

/// The connected client.
struct Active {
    handle: u16,
    generation: u64,
    mtu: u16,
    subscriptions: Vec<EndpointId>,
}

impl Active {
    fn id(&self) -> ConnectionId {
        ConnectionId {
            generation: self.generation,
        }
    }

    fn info(&self) -> ConnectionInfo {
        ConnectionInfo {
            id: self.id(),
            mtu: self.mtu,
            subscriptions: self
                .subscriptions
                .iter()
                .cloned()
                .map(EndpointKey::from_id)
                .collect(),
        }
    }

    /// The events that end this connection: each remaining subscription,
    /// then the disconnection.
    fn end(self, reason: DisconnectReason, events: &mut Vec<ConnectionEvent>) {
        let connection = self.id();
        events.extend(self.subscriptions.into_iter().map(|endpoint| {
            ConnectionEvent::SubscriptionChanged {
                connection,
                endpoint: EndpointKey::from_id(endpoint),
                notify: false,
            }
        }));
        events.push(ConnectionEvent::Disconnected { connection, reason });
    }
}

struct State {
    phase: Phase,
    /// Whether advertising is believed active. It is set before a start is
    /// requested and cleared when NimBLE reports a connection or the end of
    /// advertising, so a connection that arrives while a start is still
    /// returning is never mistaken for advertising.
    advertising: bool,
    active: Option<Active>,
    /// Handles of extra links the framework asked NimBLE to terminate, so
    /// their disconnection is recognized.
    rejected: Vec<u16>,
}

/// Values known once the host has started.
struct Prepared {
    address_type: u8,
    /// Characteristic value handles in registration order, with each
    /// characteristic's notify endpoint.
    value_handles: Vec<(Option<EndpointId>, u16)>,
}

/// What handling one GAP event under the state lock decided.
#[derive(Default)]
struct Outcome {
    events: Vec<ConnectionEvent>,
    /// An extra link to terminate.
    terminate: Option<u16>,
    /// Whether advertising may need to restart.
    restart: bool,
}

/// The running host's connection and advertising state, attached to the
/// event dispatcher as its sink.
pub(crate) struct Runtime<B: Backend> {
    backend: B,
    /// The source of connection generations.
    slot: &'static OwnerSlot,
    host: Arc<HostEvents>,
    handler: Option<Box<dyn ConnectionHandler>>,
    advertising: Option<AdvertisingPlan>,
    prepared: OnceLock<Prepared>,
    operations: Mutex<()>,
    state: Mutex<State>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Every update is a single step under the lock, so the state stays
    // consistent even if a host-test handler panicked elsewhere. ESP targets
    // abort on panic.
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lifecycle(message: &'static str) -> Error {
    Error::new(ErrorKind::Lifecycle, Some("start advertising"), message)
}

impl<B: Backend> Runtime<B> {
    pub(crate) fn new(
        backend: B,
        slot: &'static OwnerSlot,
        host: Arc<HostEvents>,
        handler: Option<Box<dyn ConnectionHandler>>,
        advertising: Option<AdvertisingPlan>,
    ) -> Self {
        Self {
            backend,
            slot,
            host,
            handler,
            advertising,
            prepared: OnceLock::new(),
            operations: Mutex::new(()),
            state: Mutex::new(State {
                phase: Phase::Starting,
                advertising: false,
                active: None,
                rejected: Vec::new(),
            }),
        }
    }

    pub(crate) fn host(&self) -> &HostEvents {
        &self.host
    }

    pub(crate) fn advertising_plan(&self) -> Option<&AdvertisingPlan> {
        self.advertising.as_ref()
    }

    /// Record the values the host assigned when it started. Called once,
    /// before [`begin`](Self::begin).
    pub(crate) fn prepare(&self, address_type: u8, value_handles: Vec<(Option<EndpointId>, u16)>) {
        let prepared = Prepared {
            address_type,
            value_handles,
        };
        assert!(
            self.prepared.set(prepared).is_ok(),
            "the runtime is prepared once"
        );
    }

    // Used by notification sending (a later ticket) and by tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn value_handles(&self) -> &[(Option<EndpointId>, u16)] {
        self.prepared
            .get()
            .map_or(&[], |prepared| &prepared.value_handles)
    }

    /// Start the configured advertising, if any, and enter the running
    /// phase. On failure the phase stays `Starting` until [`stop`](Self::stop).
    pub(crate) fn begin(&self) -> Result<(), Error> {
        let operations = lock(&self.operations);
        if let Some(plan) = &self.advertising {
            self.advertise(&operations, plan)?;
        }
        lock(&self.state).phase = Phase::Running;
        Ok(())
    }

    /// Enter the stopping phase, after which advertising never starts, and
    /// stop advertising if it is active. Stopping is best effort: stopping
    /// the host also ends advertising (`ble_hs_stop` preempts every GAP
    /// procedure), and the GAP callback refers to no storage.
    pub(crate) fn stop(&self) {
        let _operations = lock(&self.operations);
        let advertising = {
            let mut state = lock(&self.state);
            state.phase = Phase::Stopping;
            std::mem::replace(&mut state.advertising, false)
        };
        if advertising {
            let _ = self.backend.advertising_stop();
        }
    }

    /// The connected client, if any.
    pub(crate) fn connection(&self) -> Option<ConnectionInfo> {
        lock(&self.state).active.as_ref().map(Active::info)
    }

    /// The native handle of `connection` while it is the connected client;
    /// `None` once it has ended, even if NimBLE reuses its handle.
    // Used by notification sending (a later ticket) and by tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn native_handle(&self, connection: ConnectionId) -> Option<u16> {
        lock(&self.state)
            .active
            .as_ref()
            .filter(|active| active.generation == connection.generation)
            .map(|active| active.handle)
    }

    /// Start advertising at the application's request; see
    /// [`Ble::start_advertising`](crate::Ble::start_advertising).
    pub(crate) fn start_advertising(&self) -> Result<(), Error> {
        let plan = self
            .advertising
            .as_ref()
            .ok_or_else(|| lifecycle("no advertising was configured before the host started"))?;
        let operations = lock(&self.operations);
        {
            let state = lock(&self.state);
            if state.phase != Phase::Running {
                return Err(lifecycle("the host is shutting down"));
            }
            if state.active.is_some() {
                return Err(lifecycle(
                    "a client is connected, and only one client is supported",
                ));
            }
            if state.advertising {
                return Ok(());
            }
        }
        if !self.backend.is_synced() {
            return Err(lifecycle(
                "the host is not synchronized with the controller",
            ));
        }
        self.advertise(&operations, plan)
    }

    /// Restart advertising by itself, if it remains available and nothing
    /// prevents it, returning the failure to report.
    fn restart(&self) -> Option<Error> {
        let plan = self.advertising.as_ref()?;
        if !plan.remain_available {
            return None;
        }
        let operations = lock(&self.operations);
        {
            let state = lock(&self.state);
            if state.phase != Phase::Running || state.active.is_some() || state.advertising {
                return None;
            }
        }
        // During a host reset NimBLE reports the reset's GAP events before
        // the reset itself; it resynchronizes and reports that, which
        // restarts advertising then.
        if !self.backend.is_synced() {
            return None;
        }
        self.advertise(&operations, plan).err()
    }

    /// Send the payloads and start advertising. The caller holds
    /// `operations` and has checked that nothing prevents it.
    fn advertise(
        &self,
        _operations: &MutexGuard<'_, ()>,
        plan: &AdvertisingPlan,
    ) -> Result<(), Error> {
        let address_type = self
            .prepared
            .get()
            .expect("prepared before advertising")
            .address_type;
        lock(&self.state).advertising = true;
        // The payloads are sent every time: resynchronizing after a host
        // reset also resets the controller (`ble_hs_startup_go` sends HCI
        // Reset), which forgets them.
        let result = self
            .backend
            .set_advertising_data(&plan.advertising_data)
            .and_then(|()| self.backend.set_scan_response_data(&plan.scan_response))
            .and_then(|()| self.backend.advertising_start(address_type));
        if result.is_err() {
            // A failed start cannot have accepted a connection.
            lock(&self.state).advertising = false;
        }
        result.map_err(Error::from)
    }

    fn emit(&self, events: Vec<ConnectionEvent>) {
        if let Some(handler) = &self.handler {
            for event in events {
                handler.on_event(event);
            }
        }
    }

    fn restart_and_report(&self) {
        if let Some(error) = self.restart() {
            self.emit(vec![ConnectionEvent::AdvertisingFailed { error }]);
        }
    }

    fn endpoint(&self, attribute: u16) -> Option<EndpointId> {
        self.value_handles()
            .iter()
            .find(|(_, handle)| *handle == attribute)
            .and_then(|(endpoint, _)| endpoint.clone())
    }

    fn on_gap(&self, event: GapEvent) {
        let outcome = {
            let mut state = lock(&self.state);
            self.decide(&mut state, event)
        };
        let mut events = outcome.events;
        if let Some(handle) = outcome.terminate {
            let termination = self.backend.terminate(handle).map_err(Error::from);
            events.push(ConnectionEvent::ConnectionRejected { termination });
        }
        self.emit(events);
        if outcome.restart {
            self.restart_and_report();
        }
    }

    /// Apply one GAP event to the connection record.
    fn decide(&self, state: &mut State, event: GapEvent) -> Outcome {
        let mut outcome = Outcome::default();
        match event {
            GapEvent::Connect {
                connection,
                status: 0,
            } => {
                // Legacy advertising ends when it accepts a connection.
                state.advertising = false;
                match &state.active {
                    None => {
                        let active = Active {
                            handle: connection,
                            generation: self.slot.next_generation(),
                            mtu: ATT_DEFAULT_MTU,
                            subscriptions: Vec::new(),
                        };
                        outcome.events.push(ConnectionEvent::Connected {
                            connection: active.id(),
                        });
                        state.active = Some(active);
                    }
                    // A repeated report of the connected client.
                    Some(active) if active.handle == connection => {}
                    Some(_) => {
                        if !state.rejected.contains(&connection) {
                            state.rejected.push(connection);
                            outcome.terminate = Some(connection);
                        }
                    }
                }
            }
            GapEvent::Connect { status, .. } => {
                state.advertising = false;
                // While a client is connected, a failed second link changes
                // nothing.
                if state.active.is_none() {
                    outcome
                        .events
                        .push(ConnectionEvent::ConnectionFailed { status });
                    outcome.restart = true;
                }
            }
            GapEvent::Disconnect { connection, reason } => {
                if state
                    .active
                    .as_ref()
                    .is_some_and(|active| active.handle == connection)
                {
                    let active = state.active.take().expect("checked above");
                    active.end(DisconnectReason(reason), &mut outcome.events);
                } else if let Some(index) = state
                    .rejected
                    .iter()
                    .position(|handle| *handle == connection)
                {
                    state.rejected.swap_remove(index);
                }
                // A disconnection of an unknown link (such as one whose
                // connection attempt was already reported as failed) is
                // otherwise ignored, but may leave the device idle.
                outcome.restart = state.active.is_none();
            }
            GapEvent::Subscribe {
                connection,
                attribute,
                notify,
                ..
            } => {
                let Some(active) = state
                    .active
                    .as_mut()
                    .filter(|active| active.handle == connection)
                else {
                    return outcome;
                };
                // The stack's own characteristics (such as Service Changed)
                // and unknown attributes have no endpoint.
                let Some(endpoint) = self.endpoint(attribute) else {
                    return outcome;
                };
                let position = active.subscriptions.iter().position(|id| *id == endpoint);
                let changed = match (notify, position) {
                    (true, None) => {
                        active.subscriptions.push(endpoint.clone());
                        true
                    }
                    (false, Some(index)) => {
                        active.subscriptions.remove(index);
                        true
                    }
                    _ => false,
                };
                if changed {
                    outcome.events.push(ConnectionEvent::SubscriptionChanged {
                        connection: active.id(),
                        endpoint: EndpointKey::from_id(endpoint),
                        notify,
                    });
                }
            }
            GapEvent::Mtu {
                connection,
                channel,
                mtu,
            } => {
                if channel != ATT_CHANNEL {
                    return outcome;
                }
                if let Some(active) = state
                    .active
                    .as_mut()
                    .filter(|active| active.handle == connection && active.mtu != mtu)
                {
                    active.mtu = mtu;
                    outcome.events.push(ConnectionEvent::MtuChanged {
                        connection: active.id(),
                        mtu,
                    });
                }
            }
            GapEvent::AdvertisingComplete { .. } => {
                state.advertising = false;
                outcome.restart = state.active.is_none();
            }
            // Connection parameters are not tracked, and notification
            // results belong to notification sending.
            GapEvent::ConnectionUpdate { .. } | GapEvent::NotifyTransmit { .. } => {}
        }
        outcome
    }

    fn on_synced(&self) {
        if lock(&self.state).phase == Phase::Running {
            self.emit(vec![ConnectionEvent::HostSynced]);
        }
        self.restart_and_report();
    }

    fn on_reset(&self, reason: i32) {
        let mut events = Vec::new();
        {
            let mut state = lock(&self.state);
            state.advertising = false;
            state.rejected.clear();
            // NimBLE reports each connection's end before the reset; any it
            // did not report ends here.
            if let Some(active) = state.active.take() {
                active.end(DisconnectReason(reason), &mut events);
            }
            if state.phase == Phase::Running {
                events.push(ConnectionEvent::HostReset { reason });
            }
        }
        self.emit(events);
    }
}

impl<B: Backend> EventSink for Runtime<B> {
    fn on_event(&self, event: NativeEvent) {
        match event {
            NativeEvent::HostSynced => {
                self.host.on_event(NativeEvent::HostSynced);
                self.on_synced();
            }
            NativeEvent::HostReset { reason } => {
                self.host.on_event(NativeEvent::HostReset { reason });
                self.on_reset(reason);
            }
            NativeEvent::Gap(event) => self.on_gap(event),
        }
    }
}

impl<B: Backend> fmt::Debug for Runtime<B> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = lock(&self.state);
        formatter
            .debug_struct("Runtime")
            .field("phase", &state.phase)
            .field("advertising", &state.advertising)
            .field("connected", &state.active.is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::{FakeBackend, NativeCall};
    use crate::backend::gap::SubscribeReason;
    use crate::backend::native::Operation;
    use crate::ble::{take, Configured, Started};
    use crate::gatt::{Characteristic, CharacteristicDef, GattServer, Readable, Service};
    use crate::{Advertising, AttError, Cleanup, StartError, StartStage, Uuid};
    use std::thread;

    /// A comparable record of a delivered event.
    #[derive(Clone, Debug, PartialEq)]
    enum Seen {
        Connected(ConnectionId),
        Disconnected(ConnectionId, i32),
        Failed(i32),
        Rejected(Option<String>),
        Subscription(ConnectionId, EndpointKey, bool),
        Mtu(ConnectionId, u16),
        AdvertisingFailed(String),
        HostReset(i32),
        HostSynced,
    }

    fn describe(error: &Error) -> String {
        match error.backend() {
            Some(backend) => backend.operation().to_owned(),
            None => error.to_string(),
        }
    }

    impl From<ConnectionEvent> for Seen {
        fn from(event: ConnectionEvent) -> Self {
            match event {
                ConnectionEvent::Connected { connection } => Self::Connected(connection),
                ConnectionEvent::Disconnected { connection, reason } => {
                    Self::Disconnected(connection, reason.status())
                }
                ConnectionEvent::ConnectionFailed { status } => Self::Failed(status),
                ConnectionEvent::ConnectionRejected { termination } => {
                    Self::Rejected(termination.err().as_ref().map(describe))
                }
                ConnectionEvent::SubscriptionChanged {
                    connection,
                    endpoint,
                    notify,
                } => Self::Subscription(connection, endpoint, notify),
                ConnectionEvent::MtuChanged { connection, mtu } => Self::Mtu(connection, mtu),
                ConnectionEvent::AdvertisingFailed { error } => {
                    Self::AdvertisingFailed(describe(&error))
                }
                ConnectionEvent::HostReset { reason } => Self::HostReset(reason),
                ConnectionEvent::HostSynced => Self::HostSynced,
            }
        }
    }

    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<Seen>>>);

    impl Recorder {
        fn take(&self) -> Vec<Seen> {
            std::mem::take(&mut *self.0.lock().unwrap())
        }

        fn handler(&self) -> Box<dyn ConnectionHandler> {
            let seen = self.0.clone();
            Box::new(move |event: ConnectionEvent| seen.lock().unwrap().push(event.into()))
        }
    }

    struct Value(Uuid);

    impl Characteristic for Value {
        type Value = u8;
        fn uuid(&self) -> Uuid {
            self.0
        }
    }

    impl Readable for Value {
        fn read(&self) -> Result<u8, AttError> {
            Ok(0)
        }
    }

    const BATTERY: Uuid = Uuid::Uuid16(0x180f);
    const CUSTOM: Uuid = Uuid::Uuid128(0x6e40_0001_b5a3_f393_e0a9_e50e_24dc_ca9e);
    /// A remote-user termination (HCI 0x13) as NimBLE reports it.
    const REMOTE_USER: i32 = 0x213;

    fn slot() -> &'static OwnerSlot {
        Box::leak(Box::new(OwnerSlot::new()))
    }

    fn connect(connection: u16) -> GapEvent {
        GapEvent::Connect {
            connection,
            status: 0,
        }
    }

    fn disconnect(connection: u16) -> GapEvent {
        GapEvent::Disconnect {
            connection,
            reason: REMOTE_USER,
        }
    }

    fn subscribe(connection: u16, attribute: u16, notify: bool) -> GapEvent {
        GapEvent::Subscribe {
            connection,
            attribute,
            reason: SubscribeReason::Write,
            notify,
            indicate: false,
        }
    }

    fn mtu(connection: u16, mtu: u16) -> GapEvent {
        GapEvent::Mtu {
            connection,
            channel: ATT_CHANNEL,
            mtu,
        }
    }

    /// Whether `calls` is exactly one advertising start: both payloads, then
    /// the start.
    fn is_advertising_start(calls: &[NativeCall]) -> bool {
        matches!(
            calls,
            [
                NativeCall::AdvertisingData(_),
                NativeCall::ScanResponseData(_),
                NativeCall::AdvertisingStart { address_type: 0 }
            ]
        )
    }

    struct Fixture {
        fake: FakeBackend,
        slot: &'static OwnerSlot,
        running: Started<FakeBackend>,
        seen: Recorder,
        /// Notify endpoints and their value handles.
        level: (EndpointKey, u16),
        custom: (EndpointKey, u16),
        /// The value handle of a characteristic without notify.
        plain: u16,
    }

    impl Fixture {
        fn runtime(&self) -> &Runtime<FakeBackend> {
            &self.running.core().runtime
        }

        /// Native calls made from now on.
        fn mark(&self) -> usize {
            self.fake.calls().len()
        }

        fn calls_since(&self, mark: usize) -> Vec<NativeCall> {
            self.fake.calls()[mark..].to_vec()
        }

        fn deliver(&self, event: GapEvent) {
            assert!(self.fake.inject_gap(event).is_some());
        }

        fn id(&self) -> ConnectionId {
            self.running.connection().expect("a connected client").id()
        }
    }

    fn server() -> (
        GattServer,
        crate::gatt::NotifyEndpoint<u8>,
        crate::gatt::NotifyEndpoint<u8>,
    ) {
        let (level, level_endpoint) = CharacteristicDef::new(Value(Uuid::Uuid16(0x2a19)))
            .readable()
            .notifiable();
        let (custom, custom_endpoint) =
            CharacteristicDef::new(Value(Uuid::Uuid128(CUSTOM.to_u128() + 1))).notifiable();
        let server = GattServer::new([
            Service::primary(BATTERY)
                .characteristic(level)
                .characteristic(CharacteristicDef::new(Value(Uuid::Uuid16(0x2a29))).readable()),
            Service::primary(CUSTOM).characteristic(custom),
        ])
        .unwrap();
        (server, level_endpoint, custom_endpoint)
    }

    fn demo_advertising() -> crate::AdvertisingBuilder {
        Advertising::builder().name("argyle-demo").service(BATTERY)
    }

    /// Start through the fake host task, delivering the sync once the host
    /// has been asked to start.
    fn start_with(
        fake: &FakeBackend,
        configured: Configured<FakeBackend>,
        server: GattServer,
    ) -> Result<Started<FakeBackend>, StartError> {
        let gate = fake.hold(Operation::HostStart);
        let starting = thread::spawn(move || configured.start(server));
        gate.wait_entered();
        fake.inject(NativeEvent::HostSynced);
        gate.release();
        starting.join().unwrap()
    }

    fn fixture_with(advertising: Option<Advertising>) -> Fixture {
        let fake = FakeBackend::new();
        let slot = slot();
        let seen = Recorder::default();
        let mut configured = take(fake.clone(), slot).unwrap();
        configured.set_handler(seen.handler());
        if let Some(advertising) = advertising {
            configured.set_advertising(advertising);
        }
        let (server, level, custom) = server();
        let running = start_with(&fake, configured, server).expect("startup completes");
        let handles = running.core().runtime.value_handles().to_vec();
        let handle_of = |endpoint: &crate::gatt::NotifyEndpoint<u8>| {
            handles
                .iter()
                .find(|(id, _)| id.as_ref() == Some(endpoint.id()))
                .map(|(_, handle)| *handle)
                .unwrap()
        };
        let plain = handles
            .iter()
            .find(|(id, _)| id.is_none())
            .map(|(_, handle)| *handle)
            .unwrap();
        let fixture = Fixture {
            level: (level.key(), handle_of(&level)),
            custom: (custom.key(), handle_of(&custom)),
            plain,
            fake,
            slot,
            running,
            seen,
        };
        assert!(fixture.seen.take().is_empty(), "startup reports nothing");
        fixture
    }

    fn fixture() -> Fixture {
        fixture_with(Some(demo_advertising().build().unwrap()))
    }

    #[test]
    fn advertising_starts_only_after_sync_address_inference_and_handles() {
        let fake = FakeBackend::new();
        let mut configured = take(fake.clone(), slot()).unwrap();
        configured.set_advertising(demo_advertising().build().unwrap());
        // Far longer than the test, so only the sync below ends the wait.
        configured.set_sync_timeout(std::time::Duration::from_secs(3600));
        let events = configured.events.clone();
        let (server, _, _) = server();
        let starting = thread::spawn(move || configured.start(server));
        events.wait_until_parked(1);
        assert_eq!(
            fake.calls(),
            [
                NativeCall::HostInit,
                NativeCall::SetDeviceName {
                    name: "argyle-demo".into()
                },
                NativeCall::GattCount,
                NativeCall::GattAdd,
                NativeCall::InstallCallbacks,
                NativeCall::HostStart,
            ],
            "nothing is advertised before the host is ready"
        );
        fake.inject(NativeEvent::HostSynced);
        let running = starting.join().unwrap().expect("ready after the sync");
        let calls = fake.calls();
        assert_eq!(calls[6], NativeCall::InferAddress);
        // The 16-bit list names the server's only 16-bit service, so it is
        // a Complete List (0x03); no 128-bit UUID is advertised, so there is
        // no 128-bit list.
        assert_eq!(
            calls[7..],
            [
                NativeCall::AdvertisingData(vec![0x02, 0x01, 0x06, 0x03, 0x03, 0x0f, 0x18]),
                NativeCall::ScanResponseData([&[12, 0x09][..], b"argyle-demo"].concat()),
                NativeCall::AdvertisingStart { address_type: 0 },
            ]
        );
        drop(running);
    }

    #[test]
    fn a_host_that_never_syncs_never_advertises() {
        let fake = FakeBackend::new();
        let mut configured = take(fake.clone(), slot()).unwrap();
        configured.set_advertising(demo_advertising().build().unwrap());
        configured.set_sync_timeout(std::time::Duration::from_millis(30));
        let (server, _, _) = server();
        let gate = fake.hold(Operation::HostStart);
        let starting = thread::spawn(move || configured.start(server));
        gate.wait_entered();
        gate.release();
        let error = starting.join().unwrap().unwrap_err();
        assert_eq!(error.stage(), StartStage::Synchronization);
        assert_eq!(error.cleanup(), Cleanup::Released);
        assert!(
            !fake.calls().iter().any(|call| matches!(
                call,
                NativeCall::AdvertisingStart { .. }
                    | NativeCall::AdvertisingData(_)
                    | NativeCall::AdvertisingStop
            )),
            "{:?}",
            fake.calls()
        );
    }

    #[test]
    fn a_single_client_session_stays_coherent_through_reconnection() {
        let fixture = fixture();
        let (level, level_handle) = fixture.level.clone();
        let (custom, custom_handle) = fixture.custom.clone();

        fixture.deliver(connect(1));
        let first = fixture.id();
        assert_eq!(fixture.seen.take(), [Seen::Connected(first)]);
        let info = fixture.running.connection().unwrap();
        assert_eq!(info.mtu(), ATT_DEFAULT_MTU);
        assert!(info.subscriptions().is_empty());

        fixture.deliver(subscribe(1, level_handle, true));
        fixture.deliver(subscribe(1, level_handle, true));
        fixture.deliver(subscribe(1, custom_handle, true));
        fixture.deliver(mtu(1, 247));
        fixture.deliver(mtu(1, 247));
        fixture.deliver(GapEvent::Mtu {
            connection: 1,
            channel: 0x0040,
            mtu: 100,
        });
        fixture.deliver(subscribe(1, level_handle, false));
        assert_eq!(
            fixture.seen.take(),
            [
                Seen::Subscription(first, level.clone(), true),
                Seen::Subscription(first, custom.clone(), true),
                Seen::Mtu(first, 247),
                Seen::Subscription(first, level.clone(), false),
            ],
            "repeats and other channels change nothing"
        );
        let info = fixture.running.connection().unwrap();
        assert_eq!(info.id(), first);
        assert_eq!(info.mtu(), 247);
        assert_eq!(info.subscriptions(), std::slice::from_ref(&custom));

        // NimBLE reports the remaining subscription ending, then the
        // disconnection; advertising restarts.
        let mark = fixture.mark();
        fixture.deliver(GapEvent::Subscribe {
            connection: 1,
            attribute: custom_handle,
            reason: SubscribeReason::Terminated,
            notify: false,
            indicate: false,
        });
        fixture.deliver(disconnect(1));
        assert_eq!(
            fixture.seen.take(),
            [
                Seen::Subscription(first, custom.clone(), false),
                Seen::Disconnected(first, REMOTE_USER),
            ]
        );
        assert!(fixture.running.connection().is_none());
        assert!(is_advertising_start(&fixture.calls_since(mark)));

        // The same native handle is a new connection with fresh state.
        fixture.deliver(connect(1));
        let second = fixture.id();
        assert_ne!(second, first);
        assert_eq!(fixture.seen.take(), [Seen::Connected(second)]);
        let info = fixture.running.connection().unwrap();
        assert_eq!(info.mtu(), ATT_DEFAULT_MTU);
        assert!(info.subscriptions().is_empty());
        assert_eq!(fixture.runtime().native_handle(first), None);
        assert_eq!(fixture.runtime().native_handle(second), Some(1));
    }

    #[test]
    fn a_disconnection_ends_subscriptions_nimble_did_not_report() {
        let fixture = fixture();
        let (level, level_handle) = fixture.level.clone();
        let (custom, custom_handle) = fixture.custom.clone();
        fixture.deliver(connect(3));
        let id = fixture.id();
        fixture.deliver(subscribe(3, custom_handle, true));
        fixture.deliver(subscribe(3, level_handle, true));
        fixture.seen.take();
        fixture.deliver(disconnect(3));
        assert_eq!(
            fixture.seen.take(),
            [
                Seen::Subscription(id, custom, false),
                Seen::Subscription(id, level, false),
                Seen::Disconnected(id, REMOTE_USER),
            ]
        );
    }

    #[test]
    fn a_reused_handle_never_revives_an_earlier_connection() {
        let fixture = fixture();
        let (level, level_handle) = fixture.level.clone();
        fixture.deliver(connect(0));
        let first = fixture.id();
        fixture.deliver(subscribe(0, level_handle, true));
        fixture.deliver(disconnect(0));
        fixture.seen.take();

        // Late events for the old link, before the handle is reused.
        fixture.deliver(subscribe(0, level_handle, true));
        fixture.deliver(mtu(0, 185));
        fixture.deliver(disconnect(0));
        assert!(fixture.seen.take().is_empty());
        assert!(fixture.running.connection().is_none());

        fixture.deliver(connect(0));
        let second = fixture.id();
        assert_ne!(first, second);
        // Nothing from the first session carries over: an identity kept by
        // queued work does not resolve, and no subscription is inherited.
        assert_eq!(fixture.runtime().native_handle(first), None);
        assert_eq!(fixture.runtime().native_handle(second), Some(0));
        let info = fixture.running.connection().unwrap();
        assert!(info.subscriptions().is_empty());
        assert!(!info.subscriptions().contains(&level));
        assert_eq!(info.mtu(), ATT_DEFAULT_MTU);
        assert_eq!(fixture.seen.take(), [Seen::Connected(second)]);
    }

    #[test]
    fn identities_are_unique_across_restarts_of_the_host() {
        let fixture = fixture();
        fixture.deliver(connect(1));
        let first = fixture.id();
        let Fixture {
            fake,
            slot,
            mut running,
            ..
        } = fixture;
        let server = running.shutdown().unwrap().unwrap();
        let mut configured = take(fake.clone(), slot).unwrap();
        configured.set_advertising(demo_advertising().build().unwrap());
        let running = start_with(&fake, configured, server).unwrap();
        fake.inject_gap(connect(1));
        let second = running.connection().unwrap().id();
        assert_ne!(first, second);
        assert_eq!(running.core().runtime.native_handle(first), None);
    }

    #[test]
    fn a_failed_connection_restarts_advertising() {
        let fixture = fixture();
        let mark = fixture.mark();
        fixture.deliver(GapEvent::Connect {
            connection: 2,
            status: 13,
        });
        assert_eq!(fixture.seen.take(), [Seen::Failed(13)]);
        assert!(fixture.running.connection().is_none());
        assert!(is_advertising_start(&fixture.calls_since(mark)));

        // ESP-IDF can report a failed connection for a link that still
        // exists and later disconnects; that disconnection restarts
        // advertising again if it is not active.
        let mark = fixture.mark();
        fixture.deliver(disconnect(2));
        assert!(fixture.seen.take().is_empty());
        assert!(fixture.calls_since(mark).is_empty(), "already advertising");
    }

    #[test]
    fn advertising_that_does_not_remain_available_restarts_only_on_request() {
        let fixture = fixture_with(Some(
            demo_advertising().remain_available(false).build().unwrap(),
        ));
        fixture.deliver(connect(1));
        let id = fixture.id();
        fixture.seen.take();
        let mark = fixture.mark();
        let error = fixture.running.start_advertising().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Lifecycle);
        assert!(
            error.to_string().contains("a client is connected"),
            "{error}"
        );
        fixture.deliver(disconnect(1));
        fixture.deliver(GapEvent::Connect {
            connection: 1,
            status: 13,
        });
        fixture.deliver(GapEvent::AdvertisingComplete { reason: 0 });
        assert_eq!(
            fixture.seen.take(),
            [Seen::Disconnected(id, REMOTE_USER), Seen::Failed(13)]
        );
        assert!(fixture.calls_since(mark).is_empty(), "no automatic restart");

        fixture.running.start_advertising().unwrap();
        assert!(is_advertising_start(&fixture.calls_since(mark)));
        let mark = fixture.mark();
        fixture.running.start_advertising().unwrap();
        assert!(fixture.calls_since(mark).is_empty(), "already advertising");
    }

    #[test]
    fn restart_failures_are_reported_and_retried_on_the_next_trigger() {
        let fixture = fixture();
        fixture.deliver(connect(1));
        let id = fixture.id();
        fixture.seen.take();
        fixture.fake.fail_next(Operation::AdvertisingStart, 6);
        fixture.deliver(disconnect(1));
        assert_eq!(
            fixture.seen.take(),
            [
                Seen::Disconnected(id, REMOTE_USER),
                Seen::AdvertisingFailed("ble_gap_adv_start".into()),
            ]
        );
        // A payload failure stops before the start and is reported too.
        fixture.fake.fail_next(Operation::ScanResponseData, 3);
        let mark = fixture.mark();
        fixture.deliver(GapEvent::AdvertisingComplete { reason: 0 });
        assert_eq!(
            fixture.seen.take(),
            [Seen::AdvertisingFailed("ble_gap_adv_rsp_set_data".into())]
        );
        assert_eq!(
            fixture.calls_since(mark),
            [
                NativeCall::AdvertisingData(vec![0x02, 0x01, 0x06, 0x03, 0x03, 0x0f, 0x18]),
                NativeCall::ScanResponseData([&[12, 0x09][..], b"argyle-demo"].concat()),
            ]
        );
        let mark = fixture.mark();
        fixture.deliver(disconnect(9));
        assert!(fixture.seen.take().is_empty());
        assert!(is_advertising_start(&fixture.calls_since(mark)));
    }

    #[test]
    fn a_second_client_is_rejected_without_disturbing_the_first() {
        let fixture = fixture();
        let (level, level_handle) = fixture.level.clone();
        fixture.deliver(connect(1));
        let first = fixture.id();
        fixture.deliver(subscribe(1, level_handle, true));
        fixture.seen.take();

        let mark = fixture.mark();
        fixture.deliver(connect(2));
        fixture.deliver(connect(2));
        fixture.deliver(subscribe(2, level_handle, false));
        fixture.deliver(mtu(2, 100));
        assert_eq!(fixture.seen.take(), [Seen::Rejected(None)]);
        assert_eq!(
            fixture.calls_since(mark),
            [NativeCall::Terminate { connection: 2 }],
            "one termination, and no advertising"
        );
        let mark = fixture.mark();
        fixture.deliver(disconnect(2));
        assert!(fixture.seen.take().is_empty());
        assert!(fixture.calls_since(mark).is_empty());
        let info = fixture.running.connection().unwrap();
        assert_eq!(info.id(), first);
        assert_eq!(info.subscriptions(), [level]);
        assert_eq!(info.mtu(), ATT_DEFAULT_MTU);

        // A termination NimBLE refuses is reported.
        fixture.fake.fail_next(Operation::Terminate, 7);
        fixture.deliver(connect(3));
        assert_eq!(
            fixture.seen.take(),
            [Seen::Rejected(Some("ble_gap_terminate".into()))]
        );
        assert_eq!(fixture.id(), first);
    }

    #[test]
    fn duplicate_out_of_order_and_stale_events_change_nothing() {
        let fixture = fixture();
        let (_, level_handle) = fixture.level.clone();
        let mark = fixture.mark();
        // Events for a connection that does not exist.
        fixture.deliver(subscribe(5, level_handle, true));
        fixture.deliver(mtu(5, 200));
        fixture.deliver(disconnect(5));
        fixture.deliver(GapEvent::ConnectionUpdate {
            connection: 5,
            status: 0,
        });
        fixture.deliver(GapEvent::NotifyTransmit {
            connection: 5,
            attribute: level_handle,
            status: 0,
            indication: false,
        });
        assert!(fixture.seen.take().is_empty());
        assert!(fixture.running.connection().is_none());
        assert!(fixture.calls_since(mark).is_empty(), "still advertising");

        fixture.deliver(connect(1));
        let id = fixture.id();
        fixture.deliver(connect(1));
        // Unsubscribing what was never subscribed, a characteristic without
        // notify, an unknown attribute, and indications only.
        fixture.deliver(subscribe(1, level_handle, false));
        fixture.deliver(subscribe(1, fixture.plain, true));
        fixture.deliver(subscribe(1, 0x0003, true));
        fixture.deliver(GapEvent::Subscribe {
            connection: 1,
            attribute: level_handle,
            reason: SubscribeReason::Write,
            notify: false,
            indicate: true,
        });
        fixture.deliver(mtu(1, ATT_DEFAULT_MTU));
        assert_eq!(fixture.seen.take(), [Seen::Connected(id)]);
        assert!(fixture
            .running
            .connection()
            .unwrap()
            .subscriptions()
            .is_empty());

        fixture.deliver(disconnect(1));
        fixture.deliver(disconnect(1));
        fixture.deliver(subscribe(1, level_handle, true));
        assert_eq!(fixture.seen.take(), [Seen::Disconnected(id, REMOTE_USER)]);
        assert!(fixture.running.connection().is_none());
    }

    #[test]
    fn a_host_reset_clears_state_and_resynchronization_restarts_advertising() {
        let fixture = fixture();
        let (level, level_handle) = fixture.level.clone();
        fixture.deliver(connect(1));
        let id = fixture.id();
        fixture.deliver(subscribe(1, level_handle, true));
        fixture.seen.take();

        // NimBLE marks the host unsynchronized, reports the reset's GAP
        // events, then the reset; none of them restarts advertising.
        let mark = fixture.mark();
        fixture.fake.set_synced(false);
        fixture.deliver(GapEvent::Subscribe {
            connection: 1,
            attribute: level_handle,
            reason: SubscribeReason::Terminated,
            notify: false,
            indicate: false,
        });
        fixture.deliver(GapEvent::Disconnect {
            connection: 1,
            reason: 19,
        });
        fixture.deliver(GapEvent::AdvertisingComplete { reason: 19 });
        fixture.fake.inject(NativeEvent::HostReset { reason: 19 });
        assert_eq!(
            fixture.seen.take(),
            [
                Seen::Subscription(id, level, false),
                Seen::Disconnected(id, 19),
                Seen::HostReset(19),
            ]
        );
        assert!(fixture.calls_since(mark).is_empty());
        let error = fixture.running.start_advertising().unwrap_err();
        assert!(error.to_string().contains("not synchronized"), "{error}");

        fixture.fake.inject(NativeEvent::HostSynced);
        assert_eq!(fixture.seen.take(), [Seen::HostSynced]);
        assert!(is_advertising_start(&fixture.calls_since(mark)));
    }

    #[test]
    fn a_reset_ends_a_connection_nimble_did_not_report_ending() {
        let fixture = fixture();
        fixture.deliver(connect(4));
        let id = fixture.id();
        fixture.seen.take();
        fixture.fake.inject(NativeEvent::HostReset { reason: 7 });
        assert_eq!(
            fixture.seen.take(),
            [Seen::Disconnected(id, 7), Seen::HostReset(7)]
        );
        assert!(fixture.running.connection().is_none());
        assert_eq!(fixture.runtime().native_handle(id), None);
    }

    #[test]
    fn a_failed_initial_advertising_start_fails_startup_and_releases_the_host() {
        let fake = FakeBackend::new();
        let slot = slot();
        fake.fail_next(Operation::AdvertisingStart, 6);
        let mut configured = take(fake.clone(), slot).unwrap();
        configured.set_advertising(demo_advertising().build().unwrap());
        let (server, _, _) = server();
        let error = start_with(&fake, configured, server).unwrap_err();
        assert_eq!(error.stage(), StartStage::Advertising);
        assert_eq!(error.error().kind(), ErrorKind::Backend);
        assert_eq!(error.cleanup(), Cleanup::Released);
        let calls = fake.calls();
        let start = calls
            .iter()
            .position(|call| matches!(call, NativeCall::AdvertisingStart { .. }))
            .unwrap();
        assert_eq!(
            calls[start + 1..],
            [
                NativeCall::HostStop,
                NativeCall::HostDeinit,
                NativeCall::RemoveCallbacks,
                NativeCall::RegistrationFreed,
            ],
            "nothing is advertising, so nothing is stopped"
        );
        assert!(error.into_server().is_some());
        assert!(take(fake, slot).is_ok());
    }

    #[test]
    fn a_rejected_device_name_fails_at_its_stage() {
        let fake = FakeBackend::new();
        fake.fail_next(Operation::DeviceName, 3);
        let mut configured = take(fake.clone(), slot()).unwrap();
        configured.set_advertising(demo_advertising().build().unwrap());
        let (server, _, _) = server();
        let error = configured.start(server).unwrap_err();
        assert_eq!(error.stage(), StartStage::DeviceName);
        assert_eq!(error.cleanup(), Cleanup::Released);
        assert!(
            error.to_string().contains("ble_svc_gap_device_name_set"),
            "{error}"
        );
        assert_eq!(
            fake.calls(),
            [
                NativeCall::HostInit,
                NativeCall::SetDeviceName {
                    name: "argyle-demo".into()
                },
                NativeCall::HostDeinit,
            ]
        );
    }

    #[test]
    fn advertising_a_service_the_server_lacks_fails_before_any_native_call() {
        let fake = FakeBackend::new();
        let slot = slot();
        let mut configured = take(fake.clone(), slot).unwrap();
        configured.set_advertising(
            Advertising::builder()
                .service(Uuid::Uuid16(0x181c))
                .build()
                .unwrap(),
        );
        let (server, _, _) = server();
        let error = configured.start(server).unwrap_err();
        assert_eq!(error.stage(), StartStage::Advertising);
        assert_eq!(error.error().kind(), ErrorKind::Advertising);
        assert_eq!(
            error.error().advertising(),
            Some(&crate::AdvertisingError::UnknownService(Uuid::Uuid16(
                0x181c
            )))
        );
        assert_eq!(error.cleanup(), Cleanup::Released);
        assert!(fake.calls().is_empty());
        assert!(error.into_server().is_some());
        assert!(take(fake, slot).is_ok());
    }

    #[test]
    fn without_advertising_the_host_runs_without_advertising() {
        let fixture = fixture_with(None);
        let calls = fixture.fake.calls();
        assert!(!calls.iter().any(|call| matches!(
            call,
            NativeCall::SetDeviceName { .. } | NativeCall::AdvertisingStart { .. }
        )));
        let error = fixture.running.start_advertising().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Lifecycle);
        assert!(error.to_string().contains("no advertising"), "{error}");
        let Fixture {
            fake, mut running, ..
        } = fixture;
        let mark = fake.calls().len();
        running.shutdown().unwrap();
        assert_eq!(
            fake.calls()[mark..],
            [
                NativeCall::HostStop,
                NativeCall::HostDeinit,
                NativeCall::RemoveCallbacks,
                NativeCall::RegistrationFreed,
            ]
        );
    }

    #[test]
    fn start_advertising_reports_native_failures() {
        let fixture = fixture_with(Some(
            demo_advertising().remain_available(false).build().unwrap(),
        ));
        fixture.deliver(connect(1));
        fixture.deliver(disconnect(1));
        fixture.fake.fail_next(Operation::AdvertisingData, 3);
        let error = fixture.running.start_advertising().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Backend);
        assert_eq!(
            error.backend().map(crate::BackendError::operation),
            Some("ble_gap_adv_set_data")
        );
        // Nothing was left claiming that advertising is active.
        let mark = fixture.mark();
        fixture.running.start_advertising().unwrap();
        assert!(is_advertising_start(&fixture.calls_since(mark)));
    }

    #[test]
    fn shutdown_stops_advertising_first_and_never_restarts_it() {
        let connected = fixture();
        connected.deliver(connect(1));
        let id = connected.id();
        connected.seen.take();
        let Fixture {
            fake,
            running,
            seen,
            slot,
            ..
        } = connected;
        let mark = fake.calls().len();
        let gate = fake.hold(Operation::HostStop);
        let stopping = thread::spawn(move || drop(running));
        gate.wait_entered();
        // While the host stops, it ends the connection and advertising; the
        // handler hears of the connection, and nothing restarts.
        fake.inject_gap(disconnect(1));
        fake.inject_gap(GapEvent::AdvertisingComplete { reason: 30 });
        gate.release();
        stopping.join().unwrap();
        assert_eq!(seen.take(), [Seen::Disconnected(id, REMOTE_USER)]);
        assert_eq!(
            fake.calls()[mark..],
            [
                NativeCall::HostStop,
                NativeCall::HostDeinit,
                NativeCall::RemoveCallbacks,
                NativeCall::RegistrationFreed,
            ],
            "not advertising while connected, so nothing to stop"
        );
        assert!(take(fake, slot).is_ok());

        // An advertising host stops advertising before the host.
        let Fixture { fake, running, .. } = fixture();
        let mark = fake.calls().len();
        drop(running);
        assert_eq!(
            fake.calls()[mark..],
            [
                NativeCall::AdvertisingStop,
                NativeCall::HostStop,
                NativeCall::HostDeinit,
                NativeCall::RemoveCallbacks,
                NativeCall::RegistrationFreed,
            ]
        );
    }

    #[test]
    fn a_failed_advertising_stop_does_not_prevent_shutdown() {
        let Fixture {
            fake,
            mut running,
            slot,
            ..
        } = fixture();
        fake.fail_next(Operation::AdvertisingStop, 3);
        assert!(running.shutdown().unwrap().is_some());
        assert!(take(fake, slot).is_ok());
    }

    #[test]
    fn handlers_can_reenter_the_runtime_because_no_framework_lock_is_held() {
        type Shared = Arc<OnceLock<Arc<Runtime<FakeBackend>>>>;
        let fake = FakeBackend::new();
        let cell: Shared = Arc::default();
        let results = Arc::new(Mutex::new(Vec::new()));
        let mut configured = take(fake.clone(), slot()).unwrap();
        configured.set_advertising(demo_advertising().remain_available(false).build().unwrap());
        let (handler_cell, handler_results) = (cell.clone(), results.clone());
        configured.set_handler(Box::new(move |event: ConnectionEvent| {
            let runtime = handler_cell.get().expect("set after startup");
            let connected = runtime.connection().map(|info| info.id());
            let restarted = match event {
                ConnectionEvent::Disconnected { .. } => Some(runtime.start_advertising().is_ok()),
                _ => None,
            };
            handler_results.lock().unwrap().push((connected, restarted));
        }));
        let (server, _, _) = server();
        let running = start_with(&fake, configured, server).unwrap();
        assert!(cell.set(running.core().runtime.clone()).is_ok());
        let mark = fake.calls().len();
        fake.inject_gap(connect(1));
        let id = running.connection().unwrap().id();
        fake.inject_gap(disconnect(1));
        assert_eq!(
            *results.lock().unwrap(),
            [(Some(id), None), (None, Some(true))],
            "the snapshot already reflects each event"
        );
        assert!(is_advertising_start(&fake.calls()[mark..]));
    }

    #[test]
    fn a_connection_during_an_advertising_start_is_not_mistaken_for_advertising() {
        let fixture = fixture_with(Some(
            demo_advertising().remain_available(false).build().unwrap(),
        ));
        fixture.deliver(connect(1));
        fixture.deliver(disconnect(1));
        fixture.seen.take();
        let runtime = fixture.running.core().runtime.clone();
        let gate = fixture.fake.hold(Operation::AdvertisingStart);
        let starting = thread::spawn(move || runtime.start_advertising());
        gate.wait_entered();
        // The controller accepted a client before the start call returned.
        fixture.deliver(connect(2));
        gate.release();
        starting.join().unwrap().unwrap();
        let id = fixture.id();
        assert_eq!(fixture.seen.take(), [Seen::Connected(id)]);
        // Advertising ended with that connection, so it can start again
        // after it.
        fixture.deliver(disconnect(2));
        let mark = fixture.mark();
        fixture.running.start_advertising().unwrap();
        assert!(is_advertising_start(&fixture.calls_since(mark)));
    }

    #[test]
    fn concurrent_start_requests_start_advertising_once() {
        for _ in 0..20 {
            let fixture = fixture();
            fixture.deliver(connect(1));
            fixture.fake.fail_next(Operation::AdvertisingStart, 6);
            fixture.deliver(disconnect(1));
            fixture.seen.take();
            let mark = fixture.mark();
            let runtime = fixture.running.core().runtime.clone();
            let gate = fixture.fake.hold(Operation::AdvertisingStart);
            let application = thread::spawn(move || runtime.start_advertising());
            gate.wait_entered();
            // A restart trigger on the host task while the application's
            // start is in progress waits for it, then finds advertising
            // active.
            let fake = fixture.fake.clone();
            let host = thread::spawn(move || fake.inject_gap(disconnect(9)));
            gate.release();
            application.join().unwrap().unwrap();
            host.join().unwrap();
            let starts = fixture
                .calls_since(mark)
                .iter()
                .filter(|call| matches!(call, NativeCall::AdvertisingStart { .. }))
                .count();
            assert_eq!(starts, 1);
            assert!(fixture.seen.take().is_empty());
        }
    }

    #[test]
    fn disconnect_reasons_expose_hci_codes() {
        let remote = DisconnectReason(REMOTE_USER);
        assert_eq!(remote.status(), 0x213);
        assert_eq!(remote.hci_reason(), Some(0x13));
        assert_eq!(remote.to_string(), "HCI reason 0x13");
        let host = DisconnectReason(19);
        assert_eq!(host.hci_reason(), None);
        assert_eq!(host.to_string(), "NimBLE host status 19");
        assert_eq!(DisconnectReason(0x2ff).hci_reason(), Some(0xff));
        assert_eq!(DisconnectReason(0x300).hci_reason(), None);
        assert_eq!(
            format!("{:?}", ConnectionId { generation: 4 }),
            "ConnectionId(4)"
        );
    }
}

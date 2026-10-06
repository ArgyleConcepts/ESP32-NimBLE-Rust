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
//!   carry them out, so two starts never interleave their payloads, shutdown
//!   never races a restart, and a connection report never passes a start in
//!   progress. It is held across HCI commands.
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
use crate::backend::native::{Backend, NativeError, NativeEvent, NativeResult};
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

/// NimBLE's `BLE_HS_EAGAIN`, the status of a peripheral connection that broke
/// before it was reported. ESP builds check it against the SDK.
pub(crate) const HOST_EAGAIN: i32 = 1;

/// NimBLE's `BLE_HS_ENOTCONN`: the host knows no such link. It is also the
/// disconnection status reported for a client whose handle NimBLE reported
/// again for a new link. ESP builds check it against the SDK.
pub(crate) const HOST_ENOTCONN: i32 = 7;

/// The HCI Unknown Connection Identifier status (Core Specification Vol 1,
/// Part F, 1.3): the controller knows no such link.
const HCI_UNKNOWN_CONNECTION: i32 = 0x02;

/// The most links whose subscriptions are remembered before NimBLE reports
/// their connection: as many as NimBLE holds at once, so a live link is
/// never forgotten. Entries of ended links are dropped as they end; should
/// more remain anyway, the oldest is dropped.
fn pending_links<B: Backend>() -> usize {
    B::MAX_LINKS.max(1)
}

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
    /// A client connected. Advertising has stopped: the controller stopped
    /// it when it accepted the client, and if advertising was started again
    /// before NimBLE reported the connection, the framework stopped it
    /// before delivering this event.
    Connected {
        /// The new connection.
        connection: ConnectionId,
    },
    /// The client's connection ended. Its subscriptions and MTU are gone,
    /// and its identity no longer matches any connection.
    Disconnected {
        /// The connection that ended.
        connection: ConnectionId,
        /// Why it ended. `BLE_HS_ENOTCONN` (status 7) when NimBLE reported a
        /// new link with the same handle without reporting this one's end;
        /// NimBLE is not expected to, so this is defensive.
        reason: DisconnectReason,
    },
    /// A client's connection attempt failed before it was established; no
    /// connection identity was assigned. For a failed feature exchange,
    /// which ESP-IDF reports for a link it leaves open, the framework
    /// terminates the link and also reports
    /// [`ConnectionRejected`](Self::ConnectionRejected), unless the link is
    /// already gone. A link reported with `BLE_HS_EAGAIN` is being freed by
    /// NimBLE and is not terminated. ESP-IDF can report such a link failed
    /// a second time, after freeing it; that report is not delivered.
    ConnectionFailed {
        /// Why it failed, classified like a disconnection: ESP-IDF 6.1
        /// reports either `BLE_HS_EAGAIN` (the link broke before it was
        /// reported; [`DisconnectReason::status`] is 1) or the raw HCI status
        /// of the connection's feature exchange, which the framework offsets
        /// into the HCI range so [`DisconnectReason::hci_reason`] returns it.
        /// A raw HCI status of `0x01` would be indistinguishable from
        /// `BLE_HS_EAGAIN` and is classified as it; the feature exchange is
        /// not expected to report it.
        reason: DisconnectReason,
    },
    /// The framework asked NimBLE to terminate a link it does not serve: a
    /// second client that connected while one was already connected (only
    /// one client is supported), or a link whose connection attempt NimBLE
    /// reported as failed but left open. The connected client is
    /// unaffected. `termination` reports whether NimBLE refused the request;
    /// a link it already freed counts as terminated.
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
    /// Advertising should have restarted by itself but could not, or could
    /// not be stopped when a client connected. A restart is tried again on
    /// the next disconnection, failed connection, stop of advertising, or
    /// host resynchronization, or when the application calls
    /// [`Ble::start_advertising`](crate::Ble::start_advertising).
    AdvertisingFailed {
        /// Why advertising could not start.
        error: Error,
    },
    /// The host reset, ending any connection (reported first) and
    /// advertising. NimBLE resynchronizes by itself. A reset while the host
    /// is still starting is not reported; startup waits for the
    /// resynchronization instead.
    HostReset {
        /// The NimBLE host status that caused the reset.
        reason: i32,
    },
    /// The host resynchronized with the controller after a reported
    /// [`HostReset`](Self::HostReset). Advertising restarts if it remains
    /// available.
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
    active: Option<Active>,
    /// Subscriptions of links NimBLE has not reported as connected yet.
    /// ESP-IDF reports a peripheral connection only after reading the
    /// client's version and features (`ble_gap_rx_conn_complete` and
    /// `ble_gap_rx_rd_rem_sup_feat_complete` in `ble_gap.c`), while ATT
    /// already serves the client. Entries are applied when the link is
    /// reported connected and dropped when it is reported failed or
    /// disconnected (a rejected or failed link that is terminated is
    /// reported disconnected: `ble_gap_conn_broken`), when NimBLE reports the subscriptions ending (it
    /// does before freeing a link), and on a host reset, so they never reach
    /// a later link that reuses the handle. NimBLE reports one connection
    /// per link and each link's end, which the framework relies on with
    /// ESP-IDF's connection re-attempt disabled.
    pending: Vec<(u16, Vec<EndpointId>)>,
    /// Whether a host reset was reported and its resynchronization not yet.
    reset_reported: bool,
    /// Whether a host reset was reported (in any phase) and the following
    /// synchronization not yet. Only a resynchronization restarts
    /// advertising: the first synchronization is startup's, which starts it.
    resynchronizing: bool,
}

/// Values known once the host has started.
struct Prepared {
    address_type: u8,
    /// Characteristic value handles in registration order, with each
    /// characteristic's notify endpoint.
    value_handles: Vec<(Option<EndpointId>, u16)>,
}

/// Why the framework terminates a link.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Unserved {
    /// A second client.
    Extra,
    /// A link whose connection attempt NimBLE reported as failed.
    Failed,
}

/// What handling one GAP event under the state lock decided.
#[derive(Default)]
struct Outcome {
    events: Vec<ConnectionEvent>,
    /// A link to terminate.
    terminate: Option<(u16, Unserved)>,
    /// Whether advertising may need to restart.
    restart: bool,
    /// Whether a client is connected after a connection report while the
    /// host is not stopping, so advertising may need to stop.
    stop_advertising: bool,
}

/// What `operations` guards besides the order of advertising operations.
#[derive(Default)]
struct Operations {
    /// Advertising procedures started whose end NimBLE has not reported.
    ///
    /// Only one procedure runs at a time. Each ends in exactly one of these
    /// ways, each accounted once: the controller accepts a client, which
    /// NimBLE reports later as a connection, successful or failed (with
    /// ESP-IDF's connection re-attempt disabled; a duplicate failure report
    /// is recognized and ignored, see `on_gap`); NimBLE reports the end of
    /// advertising (for example `BLE_HS_EPREEMPTED` from
    /// `ble_gap_preempt_done`, which NimBLE's own address-rotation and
    /// privacy code can trigger); the framework stops it (a stop reports
    /// whether a procedure was active); or the host resets, ending all of
    /// them. Starts that find a procedure already running start nothing and
    /// are not counted.
    ///
    /// So when a connection is reported and its procedure accounted for, a
    /// remaining count means either a procedure that is still running or
    /// procedures that ended with other clients the controller accepted but
    /// NimBLE has not reported yet. With a client connected, the framework
    /// then stops advertising; if nothing was running, the stop changes
    /// nothing and the count stays for those later reports. With no
    /// remaining count, nothing can be running, and no stop is sent: a stop
    /// clears NimBLE's advertising state, and NimBLE then refuses a client
    /// the controller accepted from that advertising
    /// (`ble_gap_accept_slave_conn`), leaving a link only the controller
    /// knows. That residual race needs a start while a connection is
    /// unreported.
    running: u32,
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
    operations: Mutex<Operations>,
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

/// Classify a failed connection's status; see
/// [`ConnectionEvent::ConnectionFailed`]. ESP-IDF 6.1 reports a peripheral
/// connection failure with `BLE_HS_EAGAIN` (`ble_gap_conn_broken`) or with
/// the raw HCI status of the feature exchange
/// (`ble_gap_rx_rd_rem_sup_feat_complete`); a raw `0x01` is taken for
/// `BLE_HS_EAGAIN`, as the two cannot be told apart.
fn failure_reason(status: i32) -> DisconnectReason {
    if status != HOST_EAGAIN && (1..0x100).contains(&status) {
        DisconnectReason(HCI_STATUS_BASE + status)
    } else {
        DisconnectReason(status)
    }
}

/// Whether a termination failed only because the link is already gone.
fn already_gone(error: &NativeError) -> bool {
    matches!(
        error,
        NativeError::Status { code, .. }
            if *code == HOST_ENOTCONN || *code == HCI_STATUS_BASE + HCI_UNKNOWN_CONNECTION
    )
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
            operations: Mutex::new(Operations::default()),
            state: Mutex::new(State {
                phase: Phase::Starting,
                active: None,
                pending: Vec::new(),
                reset_reported: false,
                resynchronizing: false,
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
    ///
    /// The host may reset after startup saw it synchronized. Until it
    /// resynchronizes, NimBLE refuses HCI commands (`BLE_HS_ENOTSYNCED`
    /// from `ble_hs_hci_cmd_send_buf` in `ble_hs_hci_cmd.c`: from every task
    /// while the sync state is bad, from other tasks while it is bringing
    /// the controller up), and `ble_gap_adv_start` fails earlier with
    /// `BLE_HS_ENOADDR` once `ble_hs_id_reset` has cleared the identity
    /// addresses. So a start that fails while the host is not synchronized,
    /// or after it resynchronized meanwhile, waits for the next
    /// synchronization, within `timeout` in total, and is tried again, as
    /// the synchronization stage tolerates resets.
    pub(crate) fn begin(&self, timeout: std::time::Duration) -> Result<(), Error> {
        let deadline = std::time::Instant::now().checked_add(timeout);
        loop {
            let mut operations = lock(&self.operations);
            let syncs = self.host.syncs();
            let result = if self.advertising.is_some() {
                self.advertise(&mut operations)
            } else {
                Ok(())
            };
            match result {
                Ok(()) => {
                    lock(&self.state).phase = Phase::Running;
                    return Ok(());
                }
                // NimBLE is not synchronized, or was not while this attempt
                // ran: it marks itself synchronized before it reports the
                // synchronization (`ble_hs_sync`), so the host's own flag,
                // false from a reset report until the sync report, and the
                // sync count cover the rest of a resynchronization.
                Err(_)
                    if !self.backend.is_synced()
                        || !self.host.synced()
                        || self.host.syncs() != syncs =>
                {
                    // Wait without `operations`: the host task takes it while
                    // it reports the reset.
                    drop(operations);
                    let remaining = deadline.map(|deadline| {
                        deadline.saturating_duration_since(std::time::Instant::now())
                    });
                    if !self.host.wait_for_sync_after(syncs, remaining) {
                        return Err(Error::new(
                            ErrorKind::Timeout,
                            Some("advertising"),
                            "the host reset and did not resynchronize within the sync timeout",
                        ));
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Enter the stopping phase, after which advertising never starts, and
    /// stop advertising. Stopping is best effort and skipped while the host
    /// is not synchronized: stopping the host also ends advertising
    /// (`ble_hs_stop` preempts every GAP procedure), and the GAP callback
    /// refers to no storage.
    pub(crate) fn stop(&self) {
        let _operations = lock(&self.operations);
        lock(&self.state).phase = Phase::Stopping;
        if self.advertising.is_some() && self.backend.is_synced() {
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
    /// [`Ble::start_advertising`](crate::Ble::start_advertising). The request
    /// is always sent to NimBLE, which treats a running advertising procedure
    /// as success, so it also recovers advertising that NimBLE stopped
    /// without telling the framework.
    pub(crate) fn start_advertising(&self) -> Result<(), Error> {
        if self.advertising.is_none() {
            return Err(lifecycle(
                "no advertising was configured before the host started",
            ));
        }
        let mut operations = lock(&self.operations);
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
        }
        if !self.backend.is_synced() {
            return Err(lifecycle(
                "the host is not synchronized with the controller",
            ));
        }
        self.advertise(&mut operations).map_err(Error::from)
    }

    /// Restart advertising by itself, if it remains available and nothing
    /// prevents it, returning the failure to report. Like an application
    /// request, it is sent even if advertising may be running.
    fn restart(&self) -> Option<Error> {
        if !self.advertising.as_ref()?.remain_available {
            return None;
        }
        let mut operations = lock(&self.operations);
        {
            let state = lock(&self.state);
            if state.phase != Phase::Running || state.active.is_some() {
                return None;
            }
        }
        // During a host reset NimBLE reports the reset's GAP events before
        // the reset itself; it resynchronizes and reports that, which
        // restarts advertising then.
        if !self.backend.is_synced() {
            return None;
        }
        self.advertise(&mut operations).err().map(Error::from)
    }

    /// Send the payloads and start advertising. The caller holds
    /// `operations` and has checked that nothing prevents it.
    fn advertise(&self, operations: &mut MutexGuard<'_, Operations>) -> NativeResult<()> {
        let Some(plan) = &self.advertising else {
            return Ok(());
        };
        let address_type = self
            .prepared
            .get()
            .expect("prepared before advertising")
            .address_type;
        // The payloads are sent every time: resynchronizing after a host
        // reset also resets the controller (`ble_hs_startup_go` sends HCI
        // Reset), which forgets them.
        self.backend
            .set_advertising_data(&plan.advertising_data)
            .and_then(|()| self.backend.set_scan_response_data(&plan.scan_response))
            .and_then(|()| self.backend.advertising_start(address_type))
            .map(|started| {
                if started {
                    operations.running += 1;
                }
            })
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
        // The MTU query happens before the state lock is taken; only the host
        // task changes the connection record, and it is here, so the answer
        // still holds when the lock is taken.
        let mtu = match event {
            GapEvent::Connect { connection, status } if status != HOST_EAGAIN => {
                self.backend.mtu(connection)
            }
            _ => None,
        };
        // ESP-IDF 6.1 can report a peripheral connection twice: when the link
        // breaks before it is reported, `ble_gap_conn_broken` reports it
        // failed (BLE_HS_EAGAIN) and frees it, and the controller's answer to
        // the feature request NimBLE sent for that link can still arrive;
        // `ble_gap_rx_rd_rem_sup_feat_complete` then reports it failed again
        // with the raw HCI status, for a link it no longer holds. Every other
        // report is for a link NimBLE still holds: it creates the link before
        // requesting features, a failed feature exchange leaves it open, and
        // `ble_gap_conn_broken` reports before freeing it. So a failure
        // report for a link NimBLE does not hold is that duplicate, and
        // changes nothing.
        if let GapEvent::Connect { status, .. } = event {
            if status != 0 && status != HOST_EAGAIN && mtu.is_none() {
                return;
            }
        }
        // A connection report or the end of advertising is handled under
        // `operations`: a start in progress finishes first, so the record and
        // the procedure count below include it, and starts that follow see
        // the client.
        let operations = match event {
            GapEvent::Connect { .. } | GapEvent::AdvertisingComplete { .. } => {
                Some(lock(&self.operations))
            }
            _ => None,
        };
        let outcome = {
            let mut state = lock(&self.state);
            let mut outcome = self.decide(&mut state, event, mtu);
            if let GapEvent::Connect { .. } = event {
                outcome.stop_advertising = state.active.is_some() && state.phase != Phase::Stopping;
            }
            outcome
        };
        let mut events = Vec::new();
        if let Some(mut operations) = operations {
            // The reported connection, or the end of advertising, ended one
            // procedure. ESP-IDF reports a connection only after reading the
            // client's version and features, but the controller ended
            // advertising when it accepted the client; a start in between
            // (by the application or a restart) is still running.
            operations.running = operations.running.saturating_sub(1);
            // While the host is not synchronized (a reset is under way), the
            // stop would be refused, and the reset ends advertising anyway.
            if outcome.stop_advertising && operations.running > 0 && self.backend.is_synced() {
                match self.backend.advertising_stop() {
                    Ok(true) => operations.running -= 1,
                    Ok(false) => {}
                    Err(error) => events.push(ConnectionEvent::AdvertisingFailed {
                        error: error.into(),
                    }),
                }
            }
        }
        if let Some((handle, reason)) = outcome.terminate {
            let termination = match self.backend.terminate(handle) {
                // NimBLE reports the link's end like any disconnection.
                Ok(()) => Some(Ok(())),
                // A failed connection attempt whose link is already gone
                // needs no termination and is not reported as one.
                Err(error) if already_gone(&error) => (reason == Unserved::Extra).then_some(Ok(())),
                // The link may stay open; a later link that reuses the
                // handle is handled afresh.
                Err(error) => Some(Err(Error::from(error))),
            };
            if let Some(termination) = termination {
                events.push(ConnectionEvent::ConnectionRejected { termination });
            }
        }
        let mut all = outcome.events;
        all.extend(events);
        self.emit(all);
        if outcome.restart {
            self.restart_and_report();
        }
    }

    /// Remove and return the pending subscriptions of `handle`.
    fn take_pending(state: &mut State, handle: u16) -> Vec<EndpointId> {
        match state.pending.iter().position(|(link, _)| *link == handle) {
            Some(index) => state.pending.remove(index).1,
            None => Vec::new(),
        }
    }

    /// Forget everything about `handle` before NimBLE reports a new link
    /// with it: NimBLE reports one connection per link, so an earlier link
    /// with that handle has ended. A connected client still recorded on it
    /// ends with status `BLE_HS_ENOTCONN` (NimBLE reports every link's end,
    /// so this is defensive).
    fn forget_link(state: &mut State, handle: u16, events: &mut Vec<ConnectionEvent>) {
        if let Some(active) = state.active.take_if(|active| active.handle == handle) {
            active.end(DisconnectReason(HOST_ENOTCONN), events);
        }
    }

    /// Apply one GAP event to the connection record. `mtu` is NimBLE's ATT
    /// MTU of a newly reported link.
    fn decide(&self, state: &mut State, event: GapEvent, mtu: Option<u16>) -> Outcome {
        let mut outcome = Outcome::default();
        match event {
            GapEvent::Connect {
                connection,
                status: 0,
            } => {
                Self::forget_link(state, connection, &mut outcome.events);
                // A second client's early subscriptions stay until NimBLE
                // reports its link ending.
                if state.active.is_some() {
                    outcome.terminate = Some((connection, Unserved::Extra));
                    return outcome;
                }
                let pending = Self::take_pending(state, connection);
                let active = Active {
                    handle: connection,
                    generation: self.slot.next_generation(),
                    mtu: mtu.unwrap_or(ATT_DEFAULT_MTU),
                    subscriptions: pending,
                };
                let id = active.id();
                outcome
                    .events
                    .push(ConnectionEvent::Connected { connection: id });
                // Changes that happened before NimBLE reported the
                // connection follow it.
                if active.mtu != ATT_DEFAULT_MTU {
                    outcome.events.push(ConnectionEvent::MtuChanged {
                        connection: id,
                        mtu: active.mtu,
                    });
                }
                outcome
                    .events
                    .extend(active.subscriptions.iter().map(|endpoint| {
                        ConnectionEvent::SubscriptionChanged {
                            connection: id,
                            endpoint: EndpointKey::from_id(endpoint.clone()),
                            notify: true,
                        }
                    }));
                state.active = Some(active);
            }
            GapEvent::Connect { connection, status } => {
                Self::forget_link(state, connection, &mut outcome.events);
                Self::take_pending(state, connection);
                // BLE_HS_EAGAIN comes only from `ble_gap_conn_broken`, which
                // frees the link right after; terminating it would send an
                // HCI command for a dead link (possibly to a controller being
                // reset). A failed feature exchange reports a raw HCI status
                // and leaves the link open, so it is terminated.
                if status != HOST_EAGAIN {
                    outcome.terminate = Some((connection, Unserved::Failed));
                }
                if state.active.is_none() {
                    outcome.events.push(ConnectionEvent::ConnectionFailed {
                        reason: failure_reason(status),
                    });
                    outcome.restart = true;
                }
            }
            GapEvent::Disconnect { connection, reason } => {
                Self::take_pending(state, connection);
                if let Some(active) = state.active.take_if(|active| active.handle == connection) {
                    active.end(DisconnectReason(reason), &mut outcome.events);
                }
                // The disconnection of another link (a terminated second
                // client, or one whose connection attempt was reported as
                // failed) is otherwise ignored, but may leave the device idle.
                outcome.restart = state.active.is_none();
            }
            GapEvent::Subscribe {
                connection,
                attribute,
                notify,
                ..
            } => {
                // The stack's own characteristics (such as Service Changed)
                // and unknown attributes have no endpoint.
                let Some(endpoint) = self.endpoint(attribute) else {
                    return outcome;
                };
                if let Some(active) = state
                    .active
                    .as_mut()
                    .filter(|active| active.handle == connection)
                {
                    if update(&mut active.subscriptions, &endpoint, notify) {
                        outcome.events.push(ConnectionEvent::SubscriptionChanged {
                            connection: active.id(),
                            endpoint: EndpointKey::from_id(endpoint),
                            notify,
                        });
                    }
                } else {
                    let position = state
                        .pending
                        .iter()
                        .position(|(link, _)| *link == connection);
                    let index = match position {
                        Some(index) => index,
                        None if notify => {
                            if state.pending.len() >= pending_links::<B>() {
                                state.pending.remove(0);
                            }
                            state.pending.push((connection, Vec::new()));
                            state.pending.len() - 1
                        }
                        None => return outcome,
                    };
                    update(&mut state.pending[index].1, &endpoint, notify);
                    if state.pending[index].1.is_empty() {
                        state.pending.remove(index);
                    }
                }
            }
            GapEvent::Mtu {
                connection,
                channel,
                mtu,
            } => {
                // An exchange before NimBLE reports the connection is read
                // from NimBLE when it does.
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
                outcome.restart = state.active.is_none();
            }
            // Connection parameters are not tracked, and notification
            // results belong to notification sending.
            GapEvent::ConnectionUpdate { .. } | GapEvent::NotifyTransmit { .. } => {}
        }
        outcome
    }

    fn on_synced(&self) {
        let (reported, resynchronized) = {
            let mut state = lock(&self.state);
            let reported =
                state.phase == Phase::Running && std::mem::take(&mut state.reset_reported);
            (reported, std::mem::take(&mut state.resynchronizing))
        };
        if reported {
            self.emit(vec![ConnectionEvent::HostSynced]);
        }
        if resynchronized {
            self.restart_and_report();
        }
    }

    fn on_reset(&self, reason: i32) {
        // The reset also resets the controller, ending advertising.
        lock(&self.operations).running = 0;
        let mut events = Vec::new();
        {
            let mut state = lock(&self.state);
            state.pending.clear();
            // NimBLE reports each connection's end before the reset; any it
            // did not report ends here.
            if let Some(active) = state.active.take() {
                active.end(DisconnectReason(reason), &mut events);
            }
            state.resynchronizing = true;
            if state.phase == Phase::Running {
                state.reset_reported = true;
                events.push(ConnectionEvent::HostReset { reason });
            }
        }
        self.emit(events);
    }
}

/// Enable or disable `endpoint` in `subscriptions`, returning whether that
/// changed anything.
fn update(subscriptions: &mut Vec<EndpointId>, endpoint: &EndpointId, notify: bool) -> bool {
    let position = subscriptions.iter().position(|id| id == endpoint);
    match (notify, position) {
        (true, None) => {
            subscriptions.push(endpoint.clone());
            true
        }
        (false, Some(index)) => {
            subscriptions.remove(index);
            true
        }
        _ => false,
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
                ConnectionEvent::ConnectionFailed { reason } => Self::Failed(reason.status()),
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
            &without_queries(calls)[..],
            [
                NativeCall::AdvertisingData(_),
                NativeCall::ScanResponseData(_),
                NativeCall::AdvertisingStart { address_type: 0 }
            ]
        )
    }

    /// `calls` without MTU queries, which only read NimBLE's link state.
    fn without_queries(calls: &[NativeCall]) -> Vec<NativeCall> {
        calls
            .iter()
            .filter(|call| !matches!(call, NativeCall::Mtu { .. }))
            .cloned()
            .collect()
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
    fn a_link_that_broke_before_it_was_reported_is_not_terminated() {
        let fixture = fixture();
        // `ble_gap_conn_broken` reports BLE_HS_EAGAIN while it still holds
        // the dead link (the fake models that) and frees it right after; a
        // termination would send an HCI command for a dead link.
        fixture.fake.set_mtu(2, ATT_DEFAULT_MTU);
        let mark = fixture.mark();
        fixture.deliver(GapEvent::Connect {
            connection: 2,
            status: HOST_EAGAIN,
        });
        assert_eq!(fixture.seen.take(), [Seen::Failed(HOST_EAGAIN)]);
        assert!(fixture.running.connection().is_none());
        assert!(is_advertising_start(&fixture.calls_since(mark)));
        assert_eq!(fixture.fake.mtu(2), None, "freed after the report");
    }

    #[test]
    fn a_failed_connection_left_open_is_terminated_and_tracked() {
        let fixture = fixture();
        // ESP-IDF reports a failed feature exchange (raw HCI status 0x3b)
        // for a link it leaves open.
        fixture.fake.set_mtu(2, ATT_DEFAULT_MTU);
        let mark = fixture.mark();
        fixture.deliver(GapEvent::Connect {
            connection: 2,
            status: 0x3b,
        });
        assert_eq!(
            fixture.seen.take(),
            [Seen::Failed(0x23b), Seen::Rejected(None)],
            "the HCI status is offset into the HCI range"
        );
        let calls = without_queries(&fixture.calls_since(mark));
        assert_eq!(calls[0], NativeCall::Terminate { connection: 2 });
        assert!(is_advertising_start(&calls[1..]));
        // Its events are ignored until NimBLE reports it ending, which is not
        // reported to the application.
        let (_, level_handle) = fixture.level.clone();
        fixture.deliver(subscribe(2, level_handle, true));
        fixture.deliver(mtu(2, 100));
        fixture.deliver(disconnect(2));
        assert!(fixture.seen.take().is_empty());
        assert!(fixture.running.connection().is_none());
        // Afterwards the handle is a new link like any other.
        fixture.deliver(connect(2));
        let id = fixture.id();
        assert_eq!(fixture.seen.take(), [Seen::Connected(id)]);
        assert!(fixture
            .running
            .connection()
            .unwrap()
            .subscriptions()
            .is_empty());
    }

    #[test]
    fn a_failed_connection_already_gone_from_the_controller_is_not_reported_rejected() {
        let fixture = fixture();
        // The host still holds the link, but the controller dropped it, so
        // the termination fails with HCI Unknown Connection Identifier.
        fixture.fake.set_mtu(2, ATT_DEFAULT_MTU);
        fixture.fake.drop_controller_link(2);
        let mark = fixture.mark();
        fixture.deliver(GapEvent::Connect {
            connection: 2,
            status: 0x3b,
        });
        assert_eq!(fixture.seen.take(), [Seen::Failed(0x23b)]);
        let calls = without_queries(&fixture.calls_since(mark));
        assert_eq!(calls[0], NativeCall::Terminate { connection: 2 });
        assert!(is_advertising_start(&calls[1..]));
        // Not tracked: a new link with that handle is served.
        fixture.deliver(connect(2));
        let id = fixture.id();
        assert_eq!(fixture.seen.take(), [Seen::Connected(id)]);
    }

    #[test]
    fn a_second_client_already_gone_from_the_controller_counts_as_rejected() {
        let fixture = fixture();
        fixture.deliver(connect(1));
        let id = fixture.id();
        fixture.seen.take();
        fixture.fake.set_mtu(2, ATT_DEFAULT_MTU);
        fixture.fake.drop_controller_link(2);
        fixture.deliver(connect(2));
        assert_eq!(fixture.seen.take(), [Seen::Rejected(None)]);
        assert_eq!(fixture.id(), id);
    }

    #[test]
    fn a_repeated_termination_counts_as_terminated() {
        let fixture = fixture();
        fixture.deliver(connect(1));
        fixture.deliver(connect(2));
        fixture.seen.take();
        // NimBLE reports BLE_HS_EALREADY for a link already being
        // terminated; the ESP backend (and the fake) report success.
        assert_eq!(fixture.fake.terminate(2), Ok(()));
    }

    #[test]
    fn connection_failure_statuses_are_classified() {
        assert_eq!(failure_reason(HOST_EAGAIN).status(), HOST_EAGAIN);
        assert_eq!(failure_reason(HOST_EAGAIN).hci_reason(), None);
        assert_eq!(failure_reason(0x3e).status(), 0x23e);
        assert_eq!(failure_reason(0x3e).hci_reason(), Some(0x3e));
        assert_eq!(failure_reason(0xff).hci_reason(), Some(0xff));
        // Statuses already in a host range are kept.
        assert_eq!(failure_reason(0x213).hci_reason(), Some(0x13));
        assert_eq!(failure_reason(-5).status(), -5);
    }

    #[test]
    fn a_failed_second_link_while_connected_changes_nothing_but_is_terminated() {
        let fixture = fixture();
        let (level, level_handle) = fixture.level.clone();
        fixture.deliver(connect(1));
        let id = fixture.id();
        fixture.deliver(subscribe(1, level_handle, true));
        fixture.seen.take();
        fixture.fake.set_mtu(2, ATT_DEFAULT_MTU);
        let mark = fixture.mark();
        fixture.deliver(GapEvent::Connect {
            connection: 2,
            status: 0x3b,
        });
        assert_eq!(fixture.seen.take(), [Seen::Rejected(None)]);
        assert_eq!(
            without_queries(&fixture.calls_since(mark)),
            [NativeCall::Terminate { connection: 2 }],
            "no advertising while connected"
        );
        let info = fixture.running.connection().unwrap();
        assert_eq!(info.id(), id);
        assert_eq!(info.subscriptions(), std::slice::from_ref(&level));
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
            status: HOST_EAGAIN,
        });
        fixture.deliver(GapEvent::AdvertisingComplete { reason: 0 });
        assert_eq!(
            fixture.seen.take(),
            [
                Seen::Disconnected(id, REMOTE_USER),
                Seen::Failed(HOST_EAGAIN)
            ]
        );
        assert!(
            without_queries(&fixture.calls_since(mark)).is_empty(),
            "no automatic restart"
        );
        assert!(!fixture.fake.is_advertising());

        let mark = fixture.mark();
        fixture.running.start_advertising().unwrap();
        assert!(is_advertising_start(&fixture.calls_since(mark)));
        assert!(fixture.fake.is_advertising());
        // A request while advertising is sent again; NimBLE treats it as
        // success.
        let mark = fixture.mark();
        fixture.running.start_advertising().unwrap();
        assert!(is_advertising_start(&fixture.calls_since(mark)));
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
        fixture.deliver(subscribe(2, level_handle, false));
        fixture.deliver(mtu(2, 100));
        assert_eq!(fixture.seen.take(), [Seen::Rejected(None)]);
        assert_eq!(
            without_queries(&fixture.calls_since(mark)),
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

        // A termination NimBLE refuses is reported, and the link is not
        // tracked: a later link reusing its handle is rejected afresh
        // rather than ignored.
        fixture.fake.fail_next(Operation::Terminate, 6);
        fixture.deliver(connect(3));
        assert_eq!(
            fixture.seen.take(),
            [Seen::Rejected(Some("ble_gap_terminate".into()))]
        );
        assert_eq!(fixture.id(), first);
        let mark = fixture.mark();
        fixture.deliver(connect(3));
        assert_eq!(fixture.seen.take(), [Seen::Rejected(None)]);
        assert_eq!(
            without_queries(&fixture.calls_since(mark)),
            [NativeCall::Terminate { connection: 3 }]
        );
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
        // The stale disconnection only re-sends advertising, which NimBLE
        // treats as success.
        assert!(is_advertising_start(&fixture.calls_since(mark)));
        assert!(fixture.fake.is_advertising());

        fixture.deliver(connect(1));
        let id = fixture.id();
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
                NativeCall::AdvertisingStop,
                NativeCall::HostStop,
                NativeCall::HostDeinit,
                NativeCall::RemoveCallbacks,
                NativeCall::RegistrationFreed,
            ],
            "advertising is stopped (NimBLE reports nothing to stop) before the host"
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
                NativeCall::AdvertisingStop,
                NativeCall::HostStop,
                NativeCall::HostDeinit,
                NativeCall::RemoveCallbacks,
                NativeCall::RegistrationFreed,
            ],
            "advertising is stopped before the host and never restarted"
        );
        assert!(take(fake, slot).is_ok());

        // A host that is not synchronized is not asked to stop advertising.
        let Fixture { fake, running, .. } = fixture();
        fake.set_synced(false);
        let mark = fake.calls().len();
        drop(running);
        assert_eq!(fake.calls()[mark], NativeCall::HostStop);
    }

    #[test]
    fn start_advertising_fails_while_the_host_shuts_down() {
        type Shared = Arc<OnceLock<Arc<Runtime<FakeBackend>>>>;
        let fake = FakeBackend::new();
        let cell: Shared = Arc::default();
        let results = Arc::new(Mutex::new(Vec::new()));
        let mut configured = take(fake.clone(), slot()).unwrap();
        configured.set_advertising(demo_advertising().build().unwrap());
        let (handler_cell, handler_results) = (cell.clone(), results.clone());
        configured.set_handler(Box::new(move |event: ConnectionEvent| {
            if let ConnectionEvent::Disconnected { .. } = event {
                let runtime = handler_cell.get().expect("set after startup");
                let result = runtime
                    .start_advertising()
                    .map_err(|error| error.to_string());
                handler_results.lock().unwrap().push(result);
            }
        }));
        let (server, _, _) = server();
        let running = start_with(&fake, configured, server).unwrap();
        assert!(cell.set(running.core().runtime.clone()).is_ok());
        fake.inject_gap(connect(1));
        let gate = fake.hold(Operation::HostStop);
        let stopping = thread::spawn(move || drop(running));
        gate.wait_entered();
        let mark = fake.calls().len();
        fake.inject_gap(disconnect(1));
        gate.release();
        stopping.join().unwrap();
        let results = results.lock().unwrap();
        assert_eq!(results.len(), 1);
        let error = results[0].as_ref().unwrap_err();
        assert!(error.contains("shutting down"), "{error}");
        assert!(!fake.calls()[mark..]
            .iter()
            .any(|call| matches!(call, NativeCall::AdvertisingStart { .. })));
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
        fake.inject_gap(connect(1));
        let id = running.connection().unwrap().id();
        let mark = fake.calls().len();
        fake.inject_gap(disconnect(1));
        assert_eq!(
            *results.lock().unwrap(),
            [(Some(id), None), (None, Some(true))],
            "the snapshot already reflects each event"
        );
        assert!(is_advertising_start(&fake.calls()[mark..]));
    }

    /// The controller accepts a client (NimBLE creates the link and ends
    /// advertising) and an application start re-enables advertising before
    /// NimBLE reports the connection. Returns the fixture with that start
    /// held inside `ble_gap_adv_start` and its thread.
    fn start_before_connection_report(
        fixture: &Fixture,
    ) -> (
        Arc<crate::backend::fake::Gate>,
        thread::JoinHandle<Result<(), Error>>,
    ) {
        fixture.fake.create_link(2);
        assert!(!fixture.fake.is_advertising());
        assert!(fixture.running.connection().is_none());
        let runtime = fixture.running.core().runtime.clone();
        let gate = fixture.fake.hold(Operation::AdvertisingStart);
        let starting = thread::spawn(move || runtime.start_advertising());
        gate.wait_entered();
        (gate, starting)
    }

    #[test]
    fn advertising_started_before_the_connection_report_is_stopped() {
        let fixture = fixture_with(Some(
            demo_advertising().remain_available(false).build().unwrap(),
        ));
        let (gate, starting) = start_before_connection_report(&fixture);
        // The report arrives while the start is still in progress: it waits
        // for the start under `operations`, so the client is not recorded
        // (which a start would then race) before the start finishes. The
        // observations are collected first and the gate released before any
        // assertion, so a failure fails instead of leaving threads blocked.
        let fake = fixture.fake.clone();
        let reporting = thread::spawn(move || fake.inject_gap(connect(2)));
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
        let mut recorded_early = false;
        while std::time::Instant::now() < deadline && !recorded_early {
            recorded_early = fixture.running.connection().is_some();
            thread::yield_now();
        }
        gate.release();
        starting.join().unwrap().unwrap();
        reporting.join().unwrap();
        assert!(!recorded_early, "the report passed a start in progress");
        let id = fixture.id();
        assert_eq!(fixture.seen.take(), [Seen::Connected(id)]);
        assert!(!fixture.fake.is_advertising(), "stopped for the client");
        assert_eq!(
            fixture.fake.calls().last(),
            Some(&NativeCall::AdvertisingStop)
        );
        // Later requests see the client and change nothing.
        assert!(fixture.running.start_advertising().is_err());
        assert!(!fixture.fake.is_advertising());
    }

    #[test]
    fn a_report_after_a_single_start_does_not_stop_advertising() {
        // The controller ended the only advertising procedure when it
        // accepted the client; a stop would only clear NimBLE's state and
        // could make it refuse another accepted connection.
        let fixture = fixture();
        fixture.fake.create_link(1);
        let mark = fixture.mark();
        fixture.deliver(connect(1));
        assert!(!without_queries(&fixture.calls_since(mark)).contains(&NativeCall::AdvertisingStop));
        // After the client disconnects, the restart is again the only one.
        fixture.deliver(disconnect(1));
        fixture.fake.create_link(2);
        let mark = fixture.mark();
        fixture.deliver(connect(2));
        assert!(!without_queries(&fixture.calls_since(mark)).contains(&NativeCall::AdvertisingStop));
    }

    /// Whether advertising was stopped after `mark`.
    fn stopped_since(fixture: &Fixture, mark: usize) -> bool {
        fixture
            .calls_since(mark)
            .contains(&NativeCall::AdvertisingStop)
    }

    #[test]
    fn the_end_of_advertising_accounts_for_its_procedure() {
        // Advertising ends (NimBLE reports it), the restart starts another
        // procedure, and the controller accepts a client from it: nothing
        // is running when the client is reported.
        let fixture = fixture();
        fixture.deliver(GapEvent::AdvertisingComplete { reason: 30 });
        assert!(fixture.fake.is_advertising(), "restarted");
        fixture.fake.create_link(1);
        let mark = fixture.mark();
        fixture.deliver(connect(1));
        assert!(!stopped_since(&fixture, mark));
    }

    #[test]
    fn a_failed_connection_accounts_for_its_procedure() {
        // A client is accepted and its connection fails; the restart's
        // procedure is the one the next client is accepted from.
        let fixture = fixture();
        fixture.fake.create_link(1);
        fixture.deliver(GapEvent::Connect {
            connection: 1,
            status: HOST_EAGAIN,
        });
        assert!(fixture.fake.is_advertising(), "restarted");
        fixture.fake.create_link(2);
        let mark = fixture.mark();
        fixture.deliver(connect(2));
        assert!(!stopped_since(&fixture, mark));
    }

    #[test]
    fn starts_that_start_nothing_are_not_counted() {
        // A start refused by NimBLE, and one that finds advertising already
        // running, start no procedure.
        let fixture = fixture();
        fixture.running.start_advertising().unwrap();
        fixture.fake.create_link(1);
        fixture.fake.fail_next(Operation::AdvertisingStart, 6);
        assert!(fixture.running.start_advertising().is_err());
        let mark = fixture.mark();
        fixture.deliver(connect(1));
        assert!(!stopped_since(&fixture, mark));
    }

    #[test]
    fn a_report_for_another_link_does_not_hide_a_running_procedure() {
        // The controller accepts A, the application starts advertising again,
        // the controller accepts B, A's connection fails (and the restart
        // starts advertising), then B is reported: advertising is running
        // while B is connected, so it is stopped.
        let fixture = fixture_with(Some(demo_advertising().build().unwrap()));
        fixture.fake.create_link(1);
        fixture.running.start_advertising().unwrap();
        fixture.fake.create_link(2);
        fixture.deliver(GapEvent::Connect {
            connection: 1,
            status: HOST_EAGAIN,
        });
        assert!(fixture.fake.is_advertising());
        let mark = fixture.mark();
        fixture.deliver(connect(2));
        assert!(stopped_since(&fixture, mark));
        assert!(!fixture.fake.is_advertising());
        assert!(fixture.running.connection().is_some());
    }

    #[test]
    fn a_reset_ends_the_advertising_a_report_would_stop() {
        // Startup advertised once; the reset ended that advertising, and the
        // resynchronization's restart is the only start since, so the next
        // client needs no stop.
        let fixture = fixture();
        fixture.fake.set_synced(false);
        fixture.fake.inject(NativeEvent::HostReset { reason: 19 });
        fixture.fake.inject(NativeEvent::HostSynced);
        assert!(fixture.fake.is_advertising());
        fixture.fake.create_link(1);
        let mark = fixture.mark();
        fixture.deliver(connect(1));
        assert!(!without_queries(&fixture.calls_since(mark)).contains(&NativeCall::AdvertisingStop));
    }

    #[test]
    fn a_stop_accounts_for_the_procedure_it_ended() {
        // The window start was stopped for client 2; after it disconnects,
        // the restart is the only procedure the next client is accepted from.
        let fixture = fixture_with(Some(demo_advertising().build().unwrap()));
        let (gate, starting) = start_before_connection_report(&fixture);
        gate.release();
        starting.join().unwrap().unwrap();
        let mark = fixture.mark();
        fixture.deliver(connect(2));
        assert!(stopped_since(&fixture, mark));
        fixture.deliver(disconnect(2));
        assert!(fixture.fake.is_advertising(), "restarted");
        fixture.fake.create_link(3);
        let mark = fixture.mark();
        fixture.deliver(connect(3));
        assert!(!stopped_since(&fixture, mark));
    }

    #[test]
    fn running_advertising_is_kept_when_no_client_is_connected() {
        // A start between the controller accepting a client and NimBLE
        // reporting its connection as failed keeps advertising for the next.
        let fixture = fixture_with(Some(demo_advertising().build().unwrap()));
        let (gate, starting) = start_before_connection_report(&fixture);
        gate.release();
        starting.join().unwrap().unwrap();
        let mark = fixture.mark();
        fixture.deliver(GapEvent::Connect {
            connection: 2,
            status: HOST_EAGAIN,
        });
        assert!(!stopped_since(&fixture, mark));
        assert!(fixture.fake.is_advertising());
    }

    #[test]
    fn a_repeated_failure_report_for_a_freed_link_is_ignored() {
        // The controller accepts client 1, whose link breaks before NimBLE
        // reports it (reported failed with BLE_HS_EAGAIN and freed); the
        // controller's late answer to the feature request reports it failed
        // again. Client 2 is then accepted from the restart's procedure, the
        // application starts advertising again, and client 2 is reported:
        // that start is still running and must be stopped.
        let fixture = fixture();
        fixture.fake.create_link(1);
        fixture.deliver(GapEvent::Connect {
            connection: 1,
            status: HOST_EAGAIN,
        });
        assert!(fixture.fake.is_advertising(), "restarted");
        let mark = fixture.mark();
        assert!(fixture.fake.late_feature_failure(1, 0x08).is_some());
        assert_eq!(
            fixture.seen.take(),
            [Seen::Failed(HOST_EAGAIN)],
            "one failure per attempt"
        );
        assert!(
            without_queries(&fixture.calls_since(mark)).is_empty(),
            "no termination or restart"
        );
        fixture.fake.create_link(2);
        fixture.running.start_advertising().unwrap();
        let mark = fixture.mark();
        fixture.deliver(connect(2));
        assert!(stopped_since(&fixture, mark));
        assert!(!fixture.fake.is_advertising());
    }

    #[test]
    fn the_end_of_advertising_does_not_hide_a_restart_before_a_report() {
        // The controller accepts A, the application starts advertising
        // again, NimBLE ends that advertising (for example preempted), the
        // restart starts it again, and then A is reported: the restart is
        // still running.
        let fixture = fixture();
        fixture.fake.create_link(1);
        fixture.running.start_advertising().unwrap();
        fixture.deliver(GapEvent::AdvertisingComplete { reason: 30 });
        assert!(fixture.fake.is_advertising(), "restarted");
        let mark = fixture.mark();
        fixture.deliver(connect(1));
        assert!(stopped_since(&fixture, mark));
        assert!(!fixture.fake.is_advertising());
    }

    #[test]
    fn a_stop_that_found_nothing_keeps_the_count_for_later_reports() {
        // P1 is ended by accepting A, P2 by accepting B. A's report stops
        // nothing (nothing runs) and keeps P2 counted. A disconnects before B
        // is reported, so the restart runs P3; B's report must stop it.
        let fixture = fixture();
        fixture.fake.create_link(1);
        fixture.running.start_advertising().unwrap();
        fixture.fake.create_link(2);
        fixture.deliver(connect(1));
        assert!(!fixture.fake.is_advertising());
        fixture.deliver(disconnect(1));
        assert!(fixture.fake.is_advertising(), "restarted");
        let mark = fixture.mark();
        fixture.deliver(connect(2));
        assert!(stopped_since(&fixture, mark));
        assert!(!fixture.fake.is_advertising());
    }

    #[test]
    fn a_report_during_a_reset_does_not_try_to_stop_advertising() {
        let fixture = fixture_with(Some(
            demo_advertising().remain_available(false).build().unwrap(),
        ));
        let (gate, starting) = start_before_connection_report(&fixture);
        gate.release();
        starting.join().unwrap().unwrap();
        // NimBLE marks itself unsynchronized at the start of a reset, then
        // reports what the reset ends.
        fixture.fake.set_synced(false);
        let mark = fixture.mark();
        fixture.deliver(connect(2));
        assert!(!stopped_since(&fixture, mark));
        let id = fixture.id();
        assert_eq!(fixture.seen.take(), [Seen::Connected(id)]);
    }

    #[test]
    fn a_failed_stop_for_a_new_client_is_reported() {
        let fixture = fixture_with(Some(
            demo_advertising().remain_available(false).build().unwrap(),
        ));
        let (gate, starting) = start_before_connection_report(&fixture);
        gate.release();
        starting.join().unwrap().unwrap();
        fixture.fake.fail_next(Operation::AdvertisingStop, 3);
        fixture.deliver(connect(2));
        let id = fixture.id();
        assert_eq!(
            fixture.seen.take(),
            [
                Seen::Connected(id),
                Seen::AdvertisingFailed("ble_gap_adv_stop".into())
            ]
        );
    }

    #[test]
    fn a_client_reported_during_shutdown_does_not_stop_advertising() {
        let fixture = fixture_with(Some(
            demo_advertising().remain_available(false).build().unwrap(),
        ));
        let (gate, starting) = start_before_connection_report(&fixture);
        gate.release();
        starting.join().unwrap().unwrap();
        let Fixture {
            fake,
            running,
            seen,
            ..
        } = fixture;
        let held = fake.hold(Operation::HostStop);
        let stopping = thread::spawn(move || drop(running));
        held.wait_entered();
        // NimBLE refuses commands while it stops (BLE_HS_EDISABLED); a
        // report now must not try to stop advertising or report a failure.
        fake.fail_next(Operation::AdvertisingStop, 30);
        let mark = fake.calls().len();
        fake.inject_gap(connect(2));
        assert!(!fake.calls()[mark..].contains(&NativeCall::AdvertisingStop));
        held.release();
        stopping.join().unwrap();
        assert!(matches!(seen.take()[..], [Seen::Connected(_)]));
    }

    #[test]
    fn concurrent_start_requests_never_interleave() {
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
            // start is in progress waits for it, then sends its own.
            let fake = fixture.fake.clone();
            let host = thread::spawn(move || fake.inject_gap(disconnect(9)));
            gate.release();
            application.join().unwrap().unwrap();
            host.join().unwrap();
            let calls = fixture.calls_since(mark);
            assert_eq!(calls.len(), 6, "{calls:?}");
            assert!(is_advertising_start(&calls[..3]), "{calls:?}");
            assert!(is_advertising_start(&calls[3..]), "{calls:?}");
            assert!(fixture.seen.take().is_empty());
            assert!(fixture.fake.is_advertising());
        }
    }

    #[test]
    fn events_before_the_late_connection_report_follow_it() {
        let fixture = fixture();
        let (level, level_handle) = fixture.level.clone();
        let (custom, custom_handle) = fixture.custom.clone();
        // ESP-IDF serves ATT before it reports the peripheral connection.
        fixture.fake.set_mtu(1, 247);
        fixture.deliver(mtu(1, 247));
        fixture.deliver(subscribe(1, level_handle, true));
        fixture.deliver(subscribe(1, custom_handle, true));
        fixture.deliver(subscribe(1, custom_handle, false));
        assert!(fixture.seen.take().is_empty());
        assert!(fixture.running.connection().is_none());
        fixture.deliver(connect(1));
        let id = fixture.id();
        assert_eq!(
            fixture.seen.take(),
            [
                Seen::Connected(id),
                Seen::Mtu(id, 247),
                Seen::Subscription(id, level.clone(), true),
            ]
        );
        let info = fixture.running.connection().unwrap();
        assert_eq!(info.mtu(), 247);
        assert_eq!(info.subscriptions(), std::slice::from_ref(&level));
        // Later changes are reported as usual.
        fixture.deliver(subscribe(1, custom_handle, true));
        assert_eq!(fixture.seen.take(), [Seen::Subscription(id, custom, true)]);
    }

    #[test]
    fn early_subscriptions_never_reach_another_link() {
        let fixture = fixture();
        let (_, level_handle) = fixture.level.clone();
        let early = |handle: u16| fixture.deliver(subscribe(handle, level_handle, true));
        let fresh = |handle: u16| {
            fixture.deliver(connect(handle));
            let info = fixture.running.connection().unwrap();
            assert!(info.subscriptions().is_empty(), "{handle}");
            fixture.deliver(disconnect(handle));
            fixture.seen.take();
        };
        // Dropped with a failed connection, a disconnection, a host reset,
        // and NimBLE's own report that the subscriptions ended.
        early(1);
        fixture.deliver(GapEvent::Connect {
            connection: 1,
            status: HOST_EAGAIN,
        });
        fresh(1);
        early(2);
        fixture.deliver(disconnect(2));
        fresh(2);
        early(3);
        fixture.fake.inject(NativeEvent::HostReset { reason: 19 });
        fixture.fake.inject(NativeEvent::HostSynced);
        fresh(3);
        early(4);
        fixture.deliver(GapEvent::Subscribe {
            connection: 4,
            attribute: level_handle,
            reason: SubscribeReason::Terminated,
            notify: false,
            indicate: false,
        });
        fresh(4);
        // And with a rejected second client, whose link NimBLE reports
        // ending.
        fixture.deliver(connect(9));
        early(5);
        fixture.deliver(connect(5));
        fixture.deliver(disconnect(5));
        fixture.deliver(disconnect(9));
        fresh(5);
    }

    #[test]
    fn early_subscriptions_are_kept_for_every_link_nimble_can_hold() {
        let fixture = fixture();
        let (level, level_handle) = fixture.level.clone();
        let (custom, custom_handle) = fixture.custom.clone();
        // As many unreported links as NimBLE holds (three in the fake),
        // with interleaved subscriptions.
        assert_eq!(FakeBackend::MAX_LINKS, 3);
        fixture.deliver(subscribe(10, level_handle, true));
        fixture.deliver(subscribe(11, custom_handle, true));
        fixture.deliver(subscribe(12, level_handle, true));
        fixture.deliver(subscribe(10, custom_handle, true));
        fixture.deliver(subscribe(12, custom_handle, true));
        fixture.deliver(subscribe(12, level_handle, false));
        let expected = [
            (10, vec![level.clone(), custom.clone()]),
            (11, vec![custom.clone()]),
            (12, vec![custom.clone()]),
        ];
        for (handle, subscriptions) in expected {
            fixture.deliver(connect(handle));
            let info = fixture.running.connection().unwrap();
            assert_eq!(info.subscriptions(), &subscriptions[..], "{handle}");
            fixture.deliver(disconnect(handle));
        }
        // Beyond that, the oldest link's are dropped.
        for handle in 20..24 {
            fixture.deliver(subscribe(handle, level_handle, true));
        }
        fixture.deliver(connect(20));
        assert!(fixture
            .running
            .connection()
            .unwrap()
            .subscriptions()
            .is_empty());
        fixture.deliver(disconnect(20));
        fixture.deliver(connect(21));
        assert_eq!(
            fixture.running.connection().unwrap().subscriptions(),
            std::slice::from_ref(&level)
        );
    }

    #[test]
    fn host_synced_is_reported_only_after_a_reported_reset() {
        let fixture = fixture();
        // A sync without a reset (possible on a second core) is not
        // reported and changes nothing.
        let mark = fixture.mark();
        fixture.fake.inject(NativeEvent::HostSynced);
        assert!(fixture.seen.take().is_empty());
        assert!(fixture.calls_since(mark).is_empty());
        fixture.fake.inject(NativeEvent::HostReset { reason: 19 });
        fixture.fake.inject(NativeEvent::HostSynced);
        fixture.fake.inject(NativeEvent::HostSynced);
        assert_eq!(fixture.seen.take(), [Seen::HostReset(19), Seen::HostSynced]);

        // A reset while the host starts is not reported, nor is the sync that
        // completes startup.
        let fake = FakeBackend::new();
        let seen = Recorder::default();
        let mut configured = take(fake.clone(), slot()).unwrap();
        configured.set_handler(seen.handler());
        let (server, _, _) = server();
        let gate = fake.hold(Operation::HostStart);
        let starting = thread::spawn(move || configured.start(server));
        gate.wait_entered();
        fake.inject(NativeEvent::HostReset { reason: 19 });
        fake.inject(NativeEvent::HostSynced);
        gate.release();
        let running = starting.join().unwrap().unwrap();
        fake.inject(NativeEvent::HostSynced);
        assert!(seen.take().is_empty());
        drop(running);
    }

    #[test]
    fn every_connection_report_is_a_new_link() {
        let fixture = fixture();
        let (level, level_handle) = fixture.level.clone();
        // NimBLE reports one connection per link, so a second report with
        // the client's handle means its link ended unreported (defensive):
        // that connection ends, with its subscriptions, and a new one begins.
        fixture.deliver(connect(1));
        let first = fixture.id();
        fixture.deliver(subscribe(1, level_handle, true));
        fixture.seen.take();
        fixture.deliver(connect(1));
        let second = fixture.id();
        assert_ne!(first, second);
        assert_eq!(
            fixture.seen.take(),
            [
                Seen::Subscription(first, level, false),
                Seen::Disconnected(first, HOST_ENOTCONN),
                Seen::Connected(second),
            ]
        );
        assert!(fixture
            .running
            .connection()
            .unwrap()
            .subscriptions()
            .is_empty());
        assert_eq!(fixture.runtime().native_handle(first), None);

        // A failed report for the client's handle ends it too.
        fixture.deliver(GapEvent::Connect {
            connection: 1,
            status: HOST_EAGAIN,
        });
        assert_eq!(
            fixture.seen.take(),
            [
                Seen::Disconnected(second, HOST_ENOTCONN),
                Seen::Failed(HOST_EAGAIN)
            ]
        );
        assert!(fixture.running.connection().is_none());

        // A report for a handle being terminated is a new link, not ignored.
        fixture.deliver(connect(1));
        let third = fixture.id();
        fixture.deliver(connect(2));
        fixture.seen.take();
        fixture.deliver(disconnect(1));
        fixture.seen.take();
        fixture.deliver(connect(2));
        let fourth = fixture.id();
        assert_ne!(third, fourth);
        assert_eq!(fixture.seen.take(), [Seen::Connected(fourth)]);
        // And a failed report for it is reported as failed.
        fixture.deliver(connect(3));
        fixture.seen.take();
        fixture.deliver(disconnect(2));
        fixture.seen.take();
        fixture.deliver(GapEvent::Connect {
            connection: 3,
            status: HOST_EAGAIN,
        });
        assert_eq!(fixture.seen.take(), [Seen::Failed(HOST_EAGAIN)]);
    }

    #[test]
    fn a_reset_while_startup_advertises_leaves_the_host_advertising() {
        // The host resets and resynchronizes (which resets the controller,
        // clearing advertising) while startup's advertising start is in
        // progress. Whether the start is refused (not synchronized) and
        // retried after the resynchronization, or succeeds and is followed by
        // the resynchronization's restart, the host ends up advertising.
        let fake = FakeBackend::new();
        let mut configured = take(fake.clone(), slot()).unwrap();
        configured.set_advertising(demo_advertising().build().unwrap());
        let (gatt, _, _) = server();
        let started = fake.hold(Operation::HostStart);
        let advertising = fake.hold(Operation::AdvertisingStart);
        let starting = thread::spawn(move || configured.start(gatt));
        started.wait_entered();
        fake.inject(NativeEvent::HostSynced);
        started.release();
        advertising.wait_entered();
        let resync = {
            let fake = fake.clone();
            thread::spawn(move || {
                fake.inject(NativeEvent::HostReset { reason: 19 });
                fake.inject(NativeEvent::HostSynced);
            })
        };
        advertising.release();
        let running = starting.join().unwrap().expect("startup completes");
        resync.join().unwrap();
        assert!(fake.is_advertising());
        drop(running);
    }

    type Starting = thread::JoinHandle<Result<Started<FakeBackend>, StartError>>;

    /// Start until startup's advertising call `held` is entered, then begin
    /// a host reset on another thread (NimBLE marks the host unsynchronized
    /// first, then reports the reset, which waits for startup's call) and
    /// let the call proceed.
    fn reset_while_startup_advertises(
        held: Operation,
        sync_timeout: std::time::Duration,
    ) -> (
        FakeBackend,
        Arc<HostEvents>,
        Starting,
        thread::JoinHandle<()>,
    ) {
        let fake = FakeBackend::new();
        let mut configured = take(fake.clone(), slot()).unwrap();
        configured.set_advertising(demo_advertising().build().unwrap());
        configured.set_sync_timeout(sync_timeout);
        let events = configured.events.clone();
        let (gatt, _, _) = server();
        let started = fake.hold(Operation::HostStart);
        let call = fake.hold(held);
        let starting = thread::spawn(move || configured.start(gatt));
        started.wait_entered();
        fake.inject(NativeEvent::HostSynced);
        started.release();
        call.wait_entered();
        let reset = {
            let fake = fake.clone();
            thread::spawn(move || {
                fake.inject(NativeEvent::HostReset { reason: 19 });
            })
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while fake.is_synced() {
            assert!(
                std::time::Instant::now() < deadline,
                "the reset did not begin"
            );
            thread::yield_now();
        }
        call.release();
        (fake, events, starting, reset)
    }

    fn startup_resumes_after_resynchronization(held: Operation) {
        let (fake, events, starting, reset) =
            reset_while_startup_advertises(held, std::time::Duration::from_secs(3600));
        // NimBLE refused the call; startup waits for the resynchronization.
        events.wait_until_parked(1);
        reset.join().unwrap();
        assert!(!fake.is_advertising());
        fake.inject(NativeEvent::HostSynced);
        let running = starting.join().unwrap().expect("startup completes");
        let calls = fake.calls();
        let data: Vec<_> = calls
            .iter()
            .enumerate()
            .filter(|(_, call)| matches!(call, NativeCall::AdvertisingData(_)))
            .map(|(index, _)| index)
            .collect();
        // Tried again; the resynchronization's own restart may also send
        // the payloads again, which NimBLE treats as success.
        assert!(data.len() >= 2, "tried again: {calls:?}");
        assert!(is_advertising_start(&calls[data[1]..data[1] + 3]));
        assert!(fake.is_advertising());
        drop(running);
    }

    #[test]
    fn startup_waits_for_a_resynchronization_that_refuses_advertising_commands() {
        // The reset refuses the HCI command (BLE_HS_ENOTSYNCED).
        startup_resumes_after_resynchronization(Operation::AdvertisingData);
    }

    #[test]
    fn startup_waits_for_a_resynchronization_that_clears_the_address() {
        // The payloads were sent; the reset cleared the identity address, so
        // the start fails with BLE_HS_ENOADDR before any HCI command.
        startup_resumes_after_resynchronization(Operation::AdvertisingStart);
    }

    #[test]
    fn startup_fails_if_the_host_does_not_resynchronize_in_time() {
        let (fake, _, starting, reset) = reset_while_startup_advertises(
            Operation::AdvertisingData,
            std::time::Duration::from_millis(100),
        );
        let error = starting.join().unwrap().unwrap_err();
        reset.join().unwrap();
        assert_eq!(error.stage(), StartStage::Advertising);
        assert_eq!(error.error().kind(), ErrorKind::Timeout);
        assert_eq!(error.last_host_reset(), Some(19));
        assert_eq!(error.cleanup(), Cleanup::Released);
        assert!(!fake.is_advertising());
    }

    /// Start until address inference is held, report a reset there (so the
    /// sync flag of the host events is false), and return what is needed to
    /// continue.
    fn reset_during_inference(
        fake: &FakeBackend,
    ) -> (Arc<HostEvents>, Starting, Arc<crate::backend::fake::Gate>) {
        let mut configured = take(fake.clone(), slot()).unwrap();
        configured.set_advertising(demo_advertising().build().unwrap());
        configured.set_sync_timeout(std::time::Duration::from_secs(3600));
        let events = configured.events.clone();
        let (gatt, _, _) = server();
        let started = fake.hold(Operation::HostStart);
        let inferring = fake.hold(Operation::InferAddress);
        let starting = thread::spawn(move || configured.start(gatt));
        started.wait_entered();
        fake.inject(NativeEvent::HostSynced);
        started.release();
        inferring.wait_entered();
        fake.inject(NativeEvent::HostReset { reason: 19 });
        (events, starting, inferring)
    }

    #[test]
    fn startup_retries_when_nimble_is_synchronized_but_has_not_reported_it() {
        // NimBLE marks itself synchronized before it reports the
        // synchronization; an attempt refused during bring-up is retried
        // once the report arrives.
        let fake = FakeBackend::new();
        let (events, starting, inferring) = reset_during_inference(&fake);
        fake.set_synced(true);
        fake.fail_next(Operation::AdvertisingStart, 22);
        inferring.release();
        events.wait_until_parked(1);
        fake.inject(NativeEvent::HostSynced);
        let running = starting.join().unwrap().expect("startup completes");
        assert!(fake.is_advertising());
        drop(running);
    }

    #[test]
    fn startup_retries_when_the_host_resynchronized_during_the_attempt() {
        // The resynchronization is reported while the refused attempt is
        // still returning.
        let fake = FakeBackend::new();
        let (events, starting, inferring) = reset_during_inference(&fake);
        fake.fail_next(Operation::AdvertisingData, 22);
        let data = fake.hold(Operation::AdvertisingData);
        inferring.release();
        data.wait_entered();
        let before = events.syncs();
        let syncing = {
            let fake = fake.clone();
            thread::spawn(move || fake.inject(NativeEvent::HostSynced))
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while events.syncs() == before {
            assert!(std::time::Instant::now() < deadline, "no sync report");
            thread::yield_now();
        }
        data.release();
        let running = starting.join().unwrap().expect("startup completes");
        syncing.join().unwrap();
        let tries = fake
            .calls()
            .iter()
            .filter(|call| matches!(call, NativeCall::AdvertisingData(_)))
            .count();
        assert!(tries >= 2, "tried again");
        assert!(fake.is_advertising());
        drop(running);
    }

    #[test]
    fn a_failure_while_synchronized_fails_startup_without_waiting() {
        let fake = FakeBackend::new();
        fake.fail_next(Operation::AdvertisingStart, 3);
        let mut configured = take(fake.clone(), slot()).unwrap();
        configured.set_advertising(demo_advertising().build().unwrap());
        configured.set_sync_timeout(std::time::Duration::from_secs(3600));
        let (gatt, _, _) = server();
        let error = start_with(&fake, configured, gatt).unwrap_err();
        assert_eq!(error.stage(), StartStage::Advertising);
        assert_eq!(error.error().kind(), ErrorKind::Backend);
        assert_eq!(error.last_host_reset(), None);
    }

    #[test]
    fn a_reset_before_startup_advertises_needs_one_start() {
        // A reset and resynchronization after the sync wait but before
        // startup advertises: startup advertises on the resynchronized host.
        let fake = FakeBackend::new();
        let mut configured = take(fake.clone(), slot()).unwrap();
        configured.set_advertising(demo_advertising().build().unwrap());
        let (gatt, _, _) = server();
        let started = fake.hold(Operation::HostStart);
        let inferring = fake.hold(Operation::InferAddress);
        let starting = thread::spawn(move || configured.start(gatt));
        started.wait_entered();
        fake.inject(NativeEvent::HostSynced);
        started.release();
        inferring.wait_entered();
        fake.inject(NativeEvent::HostReset { reason: 19 });
        fake.inject(NativeEvent::HostSynced);
        inferring.release();
        let running = starting.join().unwrap().expect("startup completes");
        assert!(fake.is_advertising());
        let starts = fake
            .calls()
            .iter()
            .filter(|call| matches!(call, NativeCall::AdvertisingStart { .. }))
            .count();
        assert_eq!(starts, 1);
        drop(running);
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

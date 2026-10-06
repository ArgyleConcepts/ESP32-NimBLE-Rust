//! Advertising and connection handling are configured before the host starts
//! and observed while it runs. Host builds have no NimBLE, so `take` fails
//! here at runtime with a lifecycle error.

use argyle_nimble::gatt::{Characteristic, CharacteristicDef, GattServer, NotifyEndpoint, Readable, Service};
use argyle_nimble::{
    Access, Advertising, AttError, Ble, ConnectionEvent, ConnectionHandler, ConnectionId,
    ConnectionInfo, DisconnectReason, ErrorKind, LocalName, Running, Uuid,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

struct Level;

impl Characteristic for Level {
    type Value = u8;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0x2a19)
    }
}

impl Readable for Level {
    fn read(&self) -> Result<u8, AttError> {
        Ok(50)
    }
}

/// A handler type with shared, thread-safe state.
struct Tracker {
    subscribed: AtomicBool,
    last: Mutex<Option<ConnectionId>>,
    level: NotifyEndpoint<u8>,
}

impl ConnectionHandler for Tracker {
    fn on_event(&self, event: ConnectionEvent) {
        match event {
            ConnectionEvent::Connected { connection } => {
                *self.last.lock().unwrap() = Some(connection);
            }
            ConnectionEvent::Disconnected { connection, reason } => {
                let _: (ConnectionId, DisconnectReason) = (connection, reason);
                let _: (i32, Option<u8>) = (reason.status(), reason.hci_reason());
                self.subscribed.store(false, Ordering::Relaxed);
            }
            ConnectionEvent::SubscriptionChanged { endpoint, notify, .. } => {
                if endpoint == self.level.key() {
                    self.subscribed.store(notify, Ordering::Relaxed);
                }
            }
            ConnectionEvent::AdvertisingFailed { error } => {
                let _ = error.kind();
            }
            // The enum is non-exhaustive.
            _ => {}
        }
    }
}

fn observe(running: &Ble<Running>, level: &NotifyEndpoint<u8>) -> Option<(ConnectionId, u16, bool)> {
    let info: ConnectionInfo = running.connection()?;
    Some((info.id(), info.mtu(), info.is_subscribed(level)))
}

fn main() {
    let (level, endpoint) = CharacteristicDef::new(Level).readable().notifiable();
    let server = GattServer::new([Service::primary(Uuid::Uuid16(0x180f)).characteristic(level)]).unwrap();
    let advertising = Advertising::builder()
        .name("argyle-demo")
        .service(Uuid::Uuid16(0x180f))
        .remain_available(false)
        .build()
        .unwrap();
    assert_eq!(advertising.local_name(), Some(LocalName::Complete("argyle-demo")));

    let tracker = Tracker {
        subscribed: AtomicBool::new(false),
        last: Mutex::new(None),
        level: endpoint.clone(),
    };
    let error = Ble::take().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Lifecycle);
    if let Ok(ble) = Ble::take() {
        let running = ble
            .advertise(advertising.clone())
            .connection_handler(tracker)
            .start(server, Access::Open)
            .unwrap();
        let _ = observe(&running, &endpoint);
        running.start_advertising().unwrap();
    }

    // Closures are handlers too.
    let count = Arc::new(Mutex::new(0_u32));
    let handler = move |_: ConnectionEvent| *count.lock().unwrap() += 1;
    fn handler_bound<H: ConnectionHandler>(_: &H) {}
    handler_bound(&handler);

    // Snapshots and identities move between threads.
    fn movable<T: Send + Sync + 'static>() {}
    movable::<ConnectionInfo>();
    movable::<ConnectionId>();
    movable::<Advertising>();
}

//! The intended order compiles: take, configure, start with an explicit
//! access choice, then shut down. Host builds have no NimBLE, so `take` fails
//! here at runtime with a lifecycle error.

use argyle_nimble::gatt::{GattServer, Service};
use argyle_nimble::{Access, Ble, Cleanup, ErrorKind, Running, StartStage, Uuid};
use std::time::Duration;

fn run(server: GattServer) -> Result<Ble<Running>, Box<dyn std::error::Error>> {
    let ble = Ble::take()?.sync_timeout(Duration::from_secs(2));
    match ble.start(server, Access::Open) {
        Ok(running) => Ok(running),
        Err(error) => {
            let _: (StartStage, Cleanup, Option<i32>) =
                (error.stage(), error.cleanup(), error.last_host_reset());
            Err(error.into())
        }
    }
}

fn main() {
    let server = GattServer::new([Service::primary(Uuid::Uuid16(0x1801))]).unwrap();
    let error = Ble::take().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Lifecycle);
    assert!(run(server).is_err());

    // The owner moves to another thread.
    fn movable<T: Send + 'static>(_: Option<T>) {}
    movable::<Ble<Running>>(None);
}

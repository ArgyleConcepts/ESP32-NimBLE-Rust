//! Realistic shared application state: characteristics share an `Arc` with
//! the application, and the frozen server moves to another owner thread.

use argyle_nimble::gatt::{
    Characteristic, CharacteristicDef, GattServer, NotifyEndpoint, Readable, Service, Writable,
};
use argyle_nimble::{AttError, Uuid};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Thermostat {
    target: Mutex<i16>,
    reads: AtomicU32,
}

struct Target(Arc<Thermostat>);

impl Characteristic for Target {
    type Value = i16;
    fn uuid(&self) -> Uuid {
        Uuid::Uuid16(0x2b01)
    }
}

impl Readable for Target {
    fn read(&self) -> Result<i16, AttError> {
        self.0.reads.fetch_add(1, Ordering::Relaxed);
        Ok(*self.0.target.lock().map_err(|_| AttError::UNLIKELY)?)
    }
}

impl Writable for Target {
    fn write(&self, value: i16) -> Result<(), AttError> {
        if !(500..=3000).contains(&value) {
            return Err(AttError::OUT_OF_RANGE);
        }
        *self.0.target.lock().map_err(|_| AttError::UNLIKELY)? = value;
        Ok(())
    }
}

fn main() {
    let thermostat = Arc::new(Thermostat::default());
    let (target, updates): (_, NotifyEndpoint<i16>) =
        CharacteristicDef::new(Target(Arc::clone(&thermostat)))
            .readable()
            .writable()
            .notifiable();
    let server = GattServer::new([Service::primary(Uuid::Uuid16(0x181a)).characteristic(target)])
        .expect("valid definition");

    // The application keeps its state and the endpoint; an owner thread takes
    // the server.
    let owner = std::thread::spawn(move || drop(server));
    *thermostat.target.lock().unwrap() = 2100;
    let _copy = updates;
    owner.join().unwrap();
}

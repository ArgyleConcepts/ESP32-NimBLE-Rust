//! Advertising and the connection handler are configured before the host
//! starts.

use argyle_nimble::{Advertising, Ble, ConnectionEvent, Running};

fn advertise_later(running: Ble<Running>, advertising: Advertising) {
    let _ = running.advertise(advertising);
}

fn handle_later(running: Ble<Running>) {
    let _ = running.connection_handler(|_: ConnectionEvent| {});
}

fn main() {}

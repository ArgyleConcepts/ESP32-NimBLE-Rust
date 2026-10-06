//! A connection identity comes only from the running host; it cannot be
//! built from a number or native handle.

use argyle_nimble::ConnectionId;

fn main() {
    let _ = ConnectionId { generation: 1 };
}

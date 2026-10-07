//! A codec writes only through the writer's bounded methods; it cannot
//! reach the output buffer to add bytes beyond the capacity.

use argyle_nimble::codec::{Encode, EncodeError, ValueWriter};

struct Oversized;

impl Encode for Oversized {
    fn encode(&self, writer: &mut ValueWriter<'_>) -> Result<(), EncodeError> {
        writer.output.extend_from_slice(&[0; 1024]);
        Ok(())
    }
}

fn main() {}

//! The backend of builds without NimBLE, such as host builds.
//!
//! It has no values, so no code path can reach a native operation; the public
//! owner reports that the build has no NimBLE host before needing one.

use super::dispatch::EventDispatcher;
use super::native::{Backend, NativeResult};
use std::sync::Arc;

/// An uninhabited backend.
pub(crate) enum Unavailable {}

impl Backend for Unavailable {
    type Mbuf = Unavailable;

    fn host_init(&self) -> NativeResult<()> {
        match *self {}
    }
    fn host_deinit(&self) -> NativeResult<()> {
        match *self {}
    }
    fn install_callbacks(&self, _: Arc<EventDispatcher>) -> NativeResult<()> {
        match *self {}
    }
    fn remove_callbacks(&self) -> NativeResult<()> {
        match *self {}
    }
    fn host_start(&self) -> NativeResult<()> {
        match *self {}
    }
    fn host_stop(&self) -> NativeResult<()> {
        match *self {}
    }
    fn mbuf_from_flat(&self, _: &[u8]) -> NativeResult<Unavailable> {
        match *self {}
    }
    fn mbuf_len(&self, _: &Unavailable) -> usize {
        match *self {}
    }
    fn mbuf_append(&self, _: &mut Unavailable, _: &[u8]) -> NativeResult<()> {
        match *self {}
    }
    fn mbuf_copy(&self, _: &Unavailable, _: usize, _: &mut [u8]) -> NativeResult<()> {
        match *self {}
    }
    fn mbuf_free(&self, _: Unavailable) -> NativeResult<()> {
        match *self {}
    }
    fn notify(&self, _: u16, _: u16, _: Unavailable) -> NativeResult<()> {
        match *self {}
    }
    fn terminate(&self, _: u16) -> NativeResult<()> {
        match *self {}
    }
    fn advertising_stop(&self) -> NativeResult<()> {
        match *self {}
    }
    fn mtu(&self, _: u16) -> Option<u16> {
        match *self {}
    }
    fn infer_address_type(&self) -> NativeResult<u8> {
        match *self {}
    }
    fn is_host_task(&self) -> bool {
        match *self {}
    }
}

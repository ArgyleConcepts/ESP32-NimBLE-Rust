//! The backend of builds without NimBLE, such as host builds.
//!
//! It has no values, so no code path can reach a native operation; the public
//! owner reports that the build has no NimBLE host before needing one.

use super::dispatch::EventDispatcher;
use super::native::{Backend, NativeResult};
use crate::ble::advertising::AdvertisingFields;
use crate::gatt::registration::GattPlan;
use std::ffi::CStr;
use std::sync::Arc;

/// An uninhabited backend.
#[derive(Clone)]
pub(crate) enum Unavailable {}

impl Backend for Unavailable {
    type Mbuf = Unavailable;
    type Registration = Unavailable;
    type AdvertisingFields = Unavailable;

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
    fn set_device_name(&self, _: &CStr) -> NativeResult<()> {
        match *self {}
    }
    fn prepare_advertising_fields(&self, _: &AdvertisingFields) -> Unavailable {
        match *self {}
    }
    fn set_advertising_fields(&self, _: &Unavailable) -> NativeResult<()> {
        match *self {}
    }
    fn set_scan_response_data(&self, _: &[u8]) -> NativeResult<()> {
        match *self {}
    }
    fn advertising_start(&self, _: u8) -> NativeResult<()> {
        match *self {}
    }
    fn advertising_stop(&self) -> NativeResult<()> {
        match *self {}
    }
    fn is_synced(&self) -> bool {
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
    fn prepare_gatt(&self, _: &GattPlan) -> Unavailable {
        match *self {}
    }
    fn register_gatt(&self, _: &Unavailable) -> NativeResult<()> {
        match *self {}
    }
    fn value_handles(&self, _: &Unavailable) -> Vec<u16> {
        match *self {}
    }
}

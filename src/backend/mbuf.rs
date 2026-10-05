//! Owned native buffers.
//!
//! [`OwnedMbuf`] holds one native buffer chain and releases it exactly once:
//! explicitly with [`OwnedMbuf::free`], by transferring it to the SDK with
//! [`OwnedMbuf::notify`], or on drop. Both consuming methods take `self`, so
//! safe code cannot free or transfer the same buffer twice.
//!
//! [`OwnedMbuf::append`] also takes `self`: NimBLE does not roll back a failed
//! append, so the chain may hold a partial payload. A failed append releases
//! the buffer instead of letting a corrupted payload be sent.

use super::native::{Backend, NativeError, NativeResult, Operation};

pub(crate) struct OwnedMbuf<'b, B: Backend> {
    backend: &'b B,
    raw: Option<B::Mbuf>,
}

impl<'b, B: Backend> OwnedMbuf<'b, B> {
    /// Allocate a buffer holding a copy of `data`.
    pub(crate) fn from_slice(backend: &'b B, data: &[u8]) -> NativeResult<Self> {
        let raw = backend.mbuf_from_flat(data)?;
        Ok(Self {
            backend,
            raw: Some(raw),
        })
    }

    fn raw(&self) -> &B::Mbuf {
        self.raw
            .as_ref()
            .expect("an OwnedMbuf holds its buffer until it is consumed")
    }

    pub(crate) fn len(&self) -> usize {
        self.backend.mbuf_len(self.raw())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Append `data` and return the extended buffer. On failure the buffer
    /// is released (the SDK may have appended part of `data`) and the error is
    /// returned. A chain longer than the SDK's `u16` packet length is rejected
    /// before any native call, because NimBLE's length field would wrap.
    pub(crate) fn append(mut self, data: &[u8]) -> NativeResult<Self> {
        let total = self.len().checked_add(data.len());
        if total.is_none_or(|total| total > usize::from(u16::MAX)) {
            return Err(NativeError::InvalidLength {
                operation: Operation::MbufAppend,
                length: data.len(),
            });
        }
        let raw = self
            .raw
            .as_mut()
            .expect("an OwnedMbuf holds its buffer until it is consumed");
        self.backend.mbuf_append(raw, data)?;
        Ok(self)
    }

    /// Copy `destination.len()` bytes starting at `offset`.
    pub(crate) fn copy_to(&self, offset: usize, destination: &mut [u8]) -> NativeResult<()> {
        let end = offset.checked_add(destination.len());
        if end.is_none_or(|end| end > self.len()) {
            return Err(NativeError::OutOfRange {
                operation: Operation::MbufCopy,
                offset,
                length: destination.len(),
            });
        }
        if destination.is_empty() {
            return Ok(());
        }
        self.backend.mbuf_copy(self.raw(), offset, destination)
    }

    /// Copy the whole chain into a new vector.
    pub(crate) fn to_vec(&self) -> NativeResult<Vec<u8>> {
        let mut bytes = vec![0; self.len()];
        self.copy_to(0, &mut bytes)?;
        Ok(bytes)
    }

    /// Free the chain now and report the native result.
    pub(crate) fn free(mut self) -> NativeResult<()> {
        let raw = self.raw.take().expect("an OwnedMbuf is consumed only once");
        self.backend.mbuf_free(raw)
    }

    /// Transfer the chain to the SDK as a notification payload. The SDK owns
    /// the buffer afterwards, including when it reports an error.
    pub(crate) fn notify(mut self, connection: u16, attribute: u16) -> NativeResult<()> {
        let raw = self.raw.take().expect("an OwnedMbuf is consumed only once");
        self.backend.notify(connection, attribute, raw)
    }
}

impl<B: Backend> Drop for OwnedMbuf<'_, B> {
    fn drop(&mut self) {
        if let Some(raw) = self.raw.take() {
            // Drop cannot report a failure; callers that need the native
            // result use `free`. The buffer is released either way.
            let _ = self.backend.mbuf_free(raw);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::{FakeBackend, MbufState, NativeCall};

    #[test]
    fn a_buffer_round_trips_and_is_freed_on_drop_in_call_order() {
        let fake = FakeBackend::new();
        {
            let mbuf = OwnedMbuf::from_slice(&fake, b"head")
                .unwrap()
                .append(b"-tail")
                .unwrap();
            assert_eq!(mbuf.len(), 9);
            assert_eq!(mbuf.to_vec().unwrap(), b"head-tail");
            let mut middle = [0; 3];
            mbuf.copy_to(3, &mut middle).unwrap();
            assert_eq!(&middle, b"d-t");
        }
        assert_eq!(
            fake.calls(),
            [
                NativeCall::MbufFromFlat { id: 1, length: 4 },
                NativeCall::MbufLen { id: 1 },
                NativeCall::MbufAppend { id: 1, length: 5 },
                NativeCall::MbufLen { id: 1 },
                NativeCall::MbufLen { id: 1 },
                NativeCall::MbufLen { id: 1 },
                NativeCall::MbufCopy {
                    id: 1,
                    offset: 0,
                    length: 9
                },
                NativeCall::MbufLen { id: 1 },
                NativeCall::MbufCopy {
                    id: 1,
                    offset: 3,
                    length: 3
                },
                NativeCall::MbufFree { id: 1 },
            ]
        );
        assert_eq!(fake.mbuf_state(1), Some(MbufState::Freed));
        fake.assert_balanced().unwrap();
    }

    #[test]
    fn an_empty_payload_is_a_valid_empty_buffer() {
        let fake = FakeBackend::new();
        let mbuf = OwnedMbuf::from_slice(&fake, &[]).unwrap();
        assert!(mbuf.is_empty());
        assert_eq!(mbuf.to_vec().unwrap(), Vec::<u8>::new());
        let mbuf = mbuf.append(b"z").unwrap();
        assert!(!mbuf.is_empty());
        drop(mbuf);
        fake.assert_balanced().unwrap();
    }

    #[test]
    fn notify_transfers_ownership_once_and_skips_the_drop_free() {
        let fake = FakeBackend::new();
        let mbuf = OwnedMbuf::from_slice(&fake, b"value").unwrap();
        mbuf.notify(2, 17).unwrap();
        assert_eq!(fake.mbuf_state(1), Some(MbufState::Transferred));
        assert_eq!(fake.notifications(), [(2, 17, b"value".to_vec())]);
        assert!(!fake.calls().contains(&NativeCall::MbufFree { id: 1 }));
        fake.assert_balanced().unwrap();
    }

    #[test]
    fn a_failed_notify_still_transfers_ownership_to_the_sdk() {
        let fake = FakeBackend::new();
        fake.fail_next(Operation::Notify, 6);
        let mbuf = OwnedMbuf::from_slice(&fake, b"lost").unwrap();
        let error = mbuf.notify(2, 17).unwrap_err();
        assert_eq!(
            error,
            NativeError::Status {
                operation: Operation::Notify,
                code: 6
            }
        );
        assert_eq!(error.operation(), Operation::Notify);
        assert_eq!(error.to_string(), "Notify failed with native status 6");
        assert_eq!(fake.mbuf_state(1), Some(MbufState::Transferred));
        assert!(fake.notifications().is_empty());
        fake.assert_balanced().unwrap();
    }

    #[test]
    fn allocation_failure_allocates_nothing() {
        let fake = FakeBackend::new();
        fake.fail_next(Operation::MbufFromFlat, -1);
        assert_eq!(
            OwnedMbuf::from_slice(&fake, b"x").err(),
            Some(NativeError::OutOfMemory {
                operation: Operation::MbufFromFlat
            })
        );
        assert_eq!(fake.mbuf_state(1), None);
        fake.assert_balanced().unwrap();
    }

    #[test]
    fn a_failed_append_releases_the_partially_appended_buffer() {
        let fake = FakeBackend::new();
        fake.fail_next(Operation::MbufAppend, 3);
        let mbuf = OwnedMbuf::from_slice(&fake, b"keep").unwrap();
        assert_eq!(
            mbuf.append(b"more").err(),
            Some(NativeError::Status {
                operation: Operation::MbufAppend,
                code: 3
            })
        );
        // Like NimBLE, the fake kept part of the payload; the buffer was
        // released rather than left available to send.
        assert_eq!(fake.mbuf_data(1), Some(b"keepmo".to_vec()));
        assert_eq!(fake.mbuf_state(1), Some(MbufState::Freed));
        fake.assert_balanced().unwrap();
    }

    #[test]
    fn a_chain_longer_than_the_native_length_is_rejected_without_a_native_call() {
        let fake = FakeBackend::new();
        let full = vec![0; usize::from(u16::MAX)];
        let mbuf = OwnedMbuf::from_slice(&fake, &full).unwrap();
        assert_eq!(
            mbuf.append(b"x").err(),
            Some(NativeError::InvalidLength {
                operation: Operation::MbufAppend,
                length: 1
            })
        );
        assert!(!fake
            .calls()
            .iter()
            .any(|call| matches!(call, NativeCall::MbufAppend { .. })));
        assert_eq!(fake.mbuf_state(1), Some(MbufState::Freed));
        fake.assert_balanced().unwrap();
    }

    #[test]
    fn explicit_free_reports_a_native_failure_and_releases_once() {
        let fake = FakeBackend::new();
        fake.fail_next(Operation::MbufFree, 9);
        let mbuf = OwnedMbuf::from_slice(&fake, b"x").unwrap();
        assert_eq!(
            mbuf.free(),
            Err(NativeError::Status {
                operation: Operation::MbufFree,
                code: 9
            })
        );
        let frees = fake
            .calls()
            .into_iter()
            .filter(|call| matches!(call, NativeCall::MbufFree { .. }))
            .count();
        assert_eq!(frees, 1);
        fake.assert_balanced().unwrap();
    }

    #[test]
    fn out_of_range_copies_and_oversized_payloads_are_rejected_before_native_calls() {
        let fake = FakeBackend::new();
        let mbuf = OwnedMbuf::from_slice(&fake, b"abc").unwrap();
        let mut destination = [0; 2];
        assert_eq!(
            mbuf.copy_to(2, &mut destination),
            Err(NativeError::OutOfRange {
                operation: Operation::MbufCopy,
                offset: 2,
                length: 2
            })
        );
        assert_eq!(
            mbuf.copy_to(usize::MAX, &mut destination).map(|_| ()),
            Err(NativeError::OutOfRange {
                operation: Operation::MbufCopy,
                offset: usize::MAX,
                length: 2
            })
        );
        assert!(mbuf.copy_to(3, &mut []).is_ok());
        assert!(!fake
            .calls()
            .iter()
            .any(|call| matches!(call, NativeCall::MbufCopy { .. })));
        drop(mbuf);

        let oversized = vec![0; usize::from(u16::MAX) + 1];
        assert_eq!(
            OwnedMbuf::from_slice(&fake, &oversized).err(),
            Some(NativeError::InvalidLength {
                operation: Operation::MbufFromFlat,
                length: oversized.len()
            })
        );
        fake.assert_balanced().unwrap();
    }
}

//! Delivery of native callbacks to the framework.
//!
//! Native callbacks run on the NimBLE host task. They translate the borrowed
//! SDK data into a [`NativeEvent`] and call [`EventDispatcher::deliver`], which
//! forwards it to the attached sink. [`EventDispatcher::detach`] removes the
//! sink and waits until every in-flight delivery has returned, so callback
//! storage owned by the sink is never used after shutdown. The ESP backend and
//! the test fake both use this type unchanged.

use super::native::NativeEvent;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, ThreadId};

/// Receiver for native events. Implementations must not block indefinitely:
/// they run on the NimBLE host task.
pub(crate) trait EventSink: Send + Sync {
    fn on_event(&self, event: NativeEvent);
}

/// Result of offering an event to the dispatcher.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Delivery {
    Delivered,
    /// No sink was attached, or it was being detached.
    Dropped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DispatchError {
    /// A sink is already attached.
    AlreadyAttached,
    /// `detach` was called from inside a delivery, which would wait forever
    /// for itself to finish.
    DetachFromCallback,
}

struct DispatchState {
    sink: Option<Arc<dyn EventSink>>,
    /// Threads currently inside `sink.on_event`.
    delivering: Vec<ThreadId>,
    detaching: bool,
}

pub(crate) struct EventDispatcher {
    state: Mutex<DispatchState>,
    changed: Condvar,
}

impl EventDispatcher {
    pub(crate) const fn new() -> Self {
        Self {
            state: Mutex::new(DispatchState {
                sink: None,
                delivering: Vec::new(),
                detaching: false,
            }),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, DispatchState> {
        // A panic while holding this lock aborts on ESP targets; in host tests
        // the state is still consistent because every update is a single step.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn attach(&self, sink: Arc<dyn EventSink>) -> Result<(), DispatchError> {
        let mut state = self.lock();
        if state.sink.is_some() || state.detaching {
            return Err(DispatchError::AlreadyAttached);
        }
        state.sink = Some(sink);
        Ok(())
    }

    pub(crate) fn is_attached(&self) -> bool {
        self.lock().sink.is_some()
    }

    /// Forward one event. The sink is called without holding the lock, so a
    /// sink may deliver nested events or query the dispatcher.
    pub(crate) fn deliver(&self, event: NativeEvent) -> Delivery {
        let current = thread::current().id();
        let sink = {
            let mut state = self.lock();
            match state.sink.clone() {
                Some(sink) if !state.detaching => {
                    state.delivering.push(current);
                    sink
                }
                _ => return Delivery::Dropped,
            }
        };
        sink.on_event(event);
        let mut state = self.lock();
        if let Some(position) = state.delivering.iter().position(|id| *id == current) {
            state.delivering.swap_remove(position);
        }
        drop(state);
        self.changed.notify_all();
        Delivery::Delivered
    }

    /// Remove the sink and wait for in-flight deliveries to return. New
    /// deliveries are dropped as soon as detaching starts.
    pub(crate) fn detach(&self) -> Result<Option<Arc<dyn EventSink>>, DispatchError> {
        let current = thread::current().id();
        let mut state = self.lock();
        if state.delivering.contains(&current) {
            return Err(DispatchError::DetachFromCallback);
        }
        state.detaching = true;
        self.changed.notify_all();
        while !state.delivering.is_empty() {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        let sink = state.sink.take();
        state.detaching = false;
        drop(state);
        self.changed.notify_all();
        Ok(sink)
    }

    /// Test coordination: block until another thread has started `detach`.
    #[cfg(test)]
    pub(crate) fn wait_until_detaching(&self) {
        let mut state = self.lock();
        while !state.detaching {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::Gate;
    use std::sync::mpsc;
    use std::sync::Barrier;

    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<NativeEvent>>,
    }

    impl EventSink for Recorder {
        fn on_event(&self, event: NativeEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    impl Recorder {
        fn events(&self) -> Vec<NativeEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    /// Blocks inside the callback until its gate is released.
    struct Blocking {
        gate: Arc<Gate>,
        inner: Recorder,
    }

    impl EventSink for Blocking {
        fn on_event(&self, event: NativeEvent) {
            self.gate.pass();
            self.inner.on_event(event);
        }
    }

    #[test]
    fn events_are_dropped_until_a_sink_is_attached_then_delivered_in_order() {
        let dispatcher = EventDispatcher::new();
        assert_eq!(
            dispatcher.deliver(NativeEvent::HostSynced),
            Delivery::Dropped
        );
        let recorder = Arc::new(Recorder::default());
        dispatcher.attach(recorder.clone()).unwrap();
        assert_eq!(
            dispatcher.deliver(NativeEvent::HostReset { reason: 7 }),
            Delivery::Delivered
        );
        assert_eq!(
            dispatcher.deliver(NativeEvent::HostSynced),
            Delivery::Delivered
        );
        assert_eq!(
            recorder.events(),
            [
                NativeEvent::HostReset { reason: 7 },
                NativeEvent::HostSynced
            ]
        );
    }

    #[test]
    fn a_second_sink_is_rejected_until_the_first_is_detached() {
        let dispatcher = EventDispatcher::new();
        dispatcher.attach(Arc::new(Recorder::default())).unwrap();
        assert_eq!(
            dispatcher.attach(Arc::new(Recorder::default())),
            Err(DispatchError::AlreadyAttached)
        );
        assert!(dispatcher.detach().unwrap().is_some());
        assert!(!dispatcher.is_attached());
        assert_eq!(
            dispatcher.deliver(NativeEvent::HostSynced),
            Delivery::Dropped
        );
        dispatcher.attach(Arc::new(Recorder::default())).unwrap();
        assert!(dispatcher.is_attached());
    }

    #[test]
    fn detach_waits_for_an_in_flight_delivery_and_drops_later_events() {
        let dispatcher = Arc::new(EventDispatcher::new());
        let gate = Arc::new(Gate::new());
        let sink = Arc::new(Blocking {
            gate: gate.clone(),
            inner: Recorder::default(),
        });
        dispatcher.attach(sink.clone()).unwrap();

        let delivering = {
            let dispatcher = dispatcher.clone();
            thread::spawn(move || dispatcher.deliver(NativeEvent::HostSynced))
        };
        gate.wait_entered();

        let (detached_tx, detached_rx) = mpsc::channel();
        let detaching = {
            let dispatcher = dispatcher.clone();
            thread::spawn(move || {
                let sink = dispatcher.detach().unwrap();
                detached_tx.send(()).unwrap();
                sink.is_some()
            })
        };
        dispatcher.wait_until_detaching();
        // Detach has started but cannot finish while the sink is running, and
        // events offered meanwhile are dropped rather than queued.
        assert!(detached_rx.try_recv().is_err());
        assert_eq!(
            dispatcher.deliver(NativeEvent::HostReset { reason: 1 }),
            Delivery::Dropped
        );

        gate.release();
        assert_eq!(delivering.join().unwrap(), Delivery::Delivered);
        assert!(detaching.join().unwrap());
        detached_rx.recv().unwrap();
        assert_eq!(sink.inner.events(), [NativeEvent::HostSynced]);
        assert_eq!(
            dispatcher.deliver(NativeEvent::HostSynced),
            Delivery::Dropped
        );
    }

    #[test]
    fn detaching_from_inside_a_callback_fails_instead_of_deadlocking() {
        struct SelfDetach {
            dispatcher: Arc<EventDispatcher>,
            result: Mutex<Option<Result<bool, DispatchError>>>,
        }
        impl EventSink for SelfDetach {
            fn on_event(&self, _event: NativeEvent) {
                let result = self.dispatcher.detach().map(|sink| sink.is_some());
                *self.result.lock().unwrap() = Some(result);
            }
        }
        let dispatcher = Arc::new(EventDispatcher::new());
        let sink = Arc::new(SelfDetach {
            dispatcher: dispatcher.clone(),
            result: Mutex::new(None),
        });
        dispatcher.attach(sink.clone()).unwrap();
        assert_eq!(
            dispatcher.deliver(NativeEvent::HostSynced),
            Delivery::Delivered
        );
        assert_eq!(
            *sink.result.lock().unwrap(),
            Some(Err(DispatchError::DetachFromCallback))
        );
        assert!(dispatcher.is_attached());
        assert!(dispatcher.detach().unwrap().is_some());
    }

    #[test]
    fn overlapping_deliveries_are_all_counted_across_repeated_runs() {
        const THREADS: usize = 8;
        const EVENTS: usize = 25;
        for _ in 0..20 {
            let dispatcher = Arc::new(EventDispatcher::new());
            let recorder = Arc::new(Recorder::default());
            dispatcher.attach(recorder.clone()).unwrap();
            let start = Arc::new(Barrier::new(THREADS));
            let workers = (0..THREADS)
                .map(|worker| {
                    let dispatcher = dispatcher.clone();
                    let start = start.clone();
                    thread::spawn(move || {
                        start.wait();
                        for event in 0..EVENTS {
                            let reason = i32::try_from(worker * EVENTS + event).unwrap();
                            assert_eq!(
                                dispatcher.deliver(NativeEvent::HostReset { reason }),
                                Delivery::Delivered
                            );
                        }
                    })
                })
                .collect::<Vec<_>>();
            for worker in workers {
                worker.join().unwrap();
            }
            let mut reasons = recorder
                .events()
                .into_iter()
                .map(|event| match event {
                    NativeEvent::HostReset { reason } => reason,
                    other => panic!("unexpected event {other:?}"),
                })
                .collect::<Vec<_>>();
            reasons.sort_unstable();
            let expected = (0..i32::try_from(THREADS * EVENTS).unwrap()).collect::<Vec<_>>();
            assert_eq!(reasons, expected);
            assert!(dispatcher.detach().unwrap().is_some());
        }
    }
}

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
    /// Incremented each time a detach completes, so a detach that waited for
    /// another one does not remove a sink attached in between.
    epoch: u64,
    /// Detaches waiting for another detach to finish (test coordination).
    waiting_detaches: usize,
}

pub(crate) struct EventDispatcher {
    state: Mutex<DispatchState>,
    changed: Condvar,
}

/// A registered delivery. Registration happens in [`EventDispatcher::begin`],
/// so a detach that starts afterwards waits for it. Dropping the value
/// deregisters it, including when the sink panics.
pub(crate) struct ActiveDelivery<'d> {
    dispatcher: &'d EventDispatcher,
    sink: Arc<dyn EventSink>,
    thread: ThreadId,
}

impl ActiveDelivery<'_> {
    pub(crate) fn deliver(self, event: NativeEvent) {
        self.sink.on_event(event);
    }
}

impl Drop for ActiveDelivery<'_> {
    fn drop(&mut self) {
        let mut state = self.dispatcher.lock();
        if let Some(position) = state.delivering.iter().position(|id| *id == self.thread) {
            state.delivering.swap_remove(position);
        }
        drop(state);
        self.dispatcher.changed.notify_all();
    }
}

impl EventDispatcher {
    pub(crate) const fn new() -> Self {
        Self {
            state: Mutex::new(DispatchState {
                sink: None,
                delivering: Vec::new(),
                detaching: false,
                epoch: 0,
                waiting_detaches: 0,
            }),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, DispatchState> {
        // Every update below is a single step under the lock, and delivery
        // registration is undone by a drop guard, so the state stays
        // consistent even if a host-test sink panicked while another thread
        // held the lock. ESP targets abort on panic.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn wait<'s>(&self, state: MutexGuard<'s, DispatchState>) -> MutexGuard<'s, DispatchState> {
        self.changed
            .wait(state)
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

    /// Whether the calling thread is inside a delivery from this dispatcher.
    pub(crate) fn is_delivering_on_current_thread(&self) -> bool {
        let current = thread::current().id();
        self.lock().delivering.contains(&current)
    }

    /// Register a delivery to the attached sink, or return `None` when no
    /// sink is attached or a detach has started. Backends call this while
    /// still holding their own callback-slot lock, so removing callbacks and
    /// then detaching always waits for every delivery that saw the slot.
    pub(crate) fn begin(&self) -> Option<ActiveDelivery<'_>> {
        let thread = thread::current().id();
        let mut state = self.lock();
        if state.detaching {
            return None;
        }
        let sink = state.sink.clone()?;
        state.delivering.push(thread);
        Some(ActiveDelivery {
            dispatcher: self,
            sink,
            thread,
        })
    }

    /// Forward one event. The sink is called without holding the lock, so a
    /// sink may deliver nested events or query the dispatcher.
    pub(crate) fn deliver(&self, event: NativeEvent) -> Delivery {
        match self.begin() {
            Some(active) => {
                active.deliver(event);
                Delivery::Delivered
            }
            None => Delivery::Dropped,
        }
    }

    /// Remove the sink and wait for in-flight deliveries to return. New
    /// deliveries are dropped as soon as detaching starts. A detach that finds
    /// another detach in progress waits for it and then returns `None`.
    pub(crate) fn detach(&self) -> Result<Option<Arc<dyn EventSink>>, DispatchError> {
        let current = thread::current().id();
        let mut state = self.lock();
        if state.delivering.contains(&current) {
            return Err(DispatchError::DetachFromCallback);
        }
        if state.detaching {
            let epoch = state.epoch;
            state.waiting_detaches += 1;
            self.changed.notify_all();
            while state.epoch == epoch {
                state = self.wait(state);
            }
            state.waiting_detaches -= 1;
            return Ok(None);
        }
        state.detaching = true;
        self.changed.notify_all();
        while !state.delivering.is_empty() {
            state = self.wait(state);
        }
        let sink = state.sink.take();
        state.detaching = false;
        state.epoch += 1;
        drop(state);
        self.changed.notify_all();
        Ok(sink)
    }

    /// Test coordination: block until a second detach is waiting.
    #[cfg(test)]
    pub(crate) fn wait_until_detach_waiting(&self) {
        let mut state = self.lock();
        while state.waiting_detaches == 0 {
            state = self.wait(state);
        }
    }

    /// Test coordination: block until another thread has started `detach`.
    #[cfg(test)]
    pub(crate) fn wait_until_detaching(&self) {
        let mut state = self.lock();
        while !state.detaching {
            state = self.wait(state);
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

    #[test]
    fn a_panicking_sink_does_not_leave_detach_waiting_forever() {
        struct Panicking;
        impl EventSink for Panicking {
            fn on_event(&self, _event: NativeEvent) {
                panic!("sink failure");
            }
        }
        let dispatcher = Arc::new(EventDispatcher::new());
        dispatcher.attach(Arc::new(Panicking)).unwrap();
        let delivering = {
            let dispatcher = dispatcher.clone();
            thread::spawn(move || dispatcher.deliver(NativeEvent::HostSynced))
        };
        assert!(delivering.join().is_err(), "the sink panic propagates");
        // The drop guard deregistered the panicked delivery, so detach returns.
        assert!(!dispatcher.is_delivering_on_current_thread());
        assert!(dispatcher.detach().unwrap().is_some());
    }

    #[test]
    fn two_deliveries_can_be_inside_the_sink_at_once() {
        // NimBLE delivers from its host task and, for notify transmit events,
        // from the notifying thread, so deliveries must not serialize. Each
        // event waits inside the sink until the other has also arrived.
        struct Rendezvous(Barrier);
        impl EventSink for Rendezvous {
            fn on_event(&self, _event: NativeEvent) {
                self.0.wait();
            }
        }
        let dispatcher = Arc::new(EventDispatcher::new());
        dispatcher
            .attach(Arc::new(Rendezvous(Barrier::new(2))))
            .unwrap();
        let workers = (0..2)
            .map(|_| {
                let dispatcher = dispatcher.clone();
                thread::spawn(move || dispatcher.deliver(NativeEvent::HostSynced))
            })
            .collect::<Vec<_>>();
        for worker in workers {
            assert_eq!(worker.join().unwrap(), Delivery::Delivered);
        }
        assert!(dispatcher.detach().unwrap().is_some());
    }

    #[test]
    fn a_waiting_second_detach_does_not_remove_a_newly_attached_sink() {
        let dispatcher = Arc::new(EventDispatcher::new());
        let gate = Arc::new(Gate::new());
        dispatcher
            .attach(Arc::new(Blocking {
                gate: gate.clone(),
                inner: Recorder::default(),
            }))
            .unwrap();
        let delivering = {
            let dispatcher = dispatcher.clone();
            thread::spawn(move || dispatcher.deliver(NativeEvent::HostSynced))
        };
        gate.wait_entered();
        let first = {
            let dispatcher = dispatcher.clone();
            thread::spawn(move || dispatcher.detach().unwrap().is_some())
        };
        dispatcher.wait_until_detaching();
        let second = {
            let dispatcher = dispatcher.clone();
            thread::spawn(move || dispatcher.detach().unwrap().is_some())
        };
        dispatcher.wait_until_detach_waiting();
        gate.release();
        assert_eq!(delivering.join().unwrap(), Delivery::Delivered);
        assert!(first.join().unwrap());
        // Attach a replacement while the second detach may still be waking.
        let replacement = Arc::new(Recorder::default());
        dispatcher.attach(replacement.clone()).unwrap();
        assert!(!second.join().unwrap(), "the waiting detach took a sink");
        assert!(dispatcher.is_attached(), "the replacement sink was removed");
        assert_eq!(
            dispatcher.deliver(NativeEvent::HostSynced),
            Delivery::Delivered
        );
        assert_eq!(replacement.events(), [NativeEvent::HostSynced]);
    }
}

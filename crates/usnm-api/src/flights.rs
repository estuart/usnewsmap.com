//! Response computations in flight (06 §6.5).
//!
//! A response the in-process cache doesn't hold is computed by a task of
//! its own, not by the request that asked for it, so a slow search keeps
//! going after its visitor stops waiting (the visitor gets `202 Accepted`
//! and asks again). Identical requests share one computation: the first
//! registers it here under its cache key, and the others wait on the same
//! result. When the computation ends, its task stores a successful body in
//! the in-process cache and only then removes the entry, so a request finds
//! either the computation or its result, never neither. Errors, timeouts
//! included, are handed to whoever is waiting and never cached.
//!
//! A search takes one of `compute_concurrency` slots for as long as it
//! runs, after its persistent-cache read misses, which bounds how many run
//! at once. When every slot is taken it joins a first-come, first-served
//! queue of at most `search_queue` searches and keeps its place there for
//! as long as someone keeps asking about it; a `202` says how many searches
//! are ahead of it. A search that finds the queue full is refused at once
//! as busy. A computation that nobody has waited on for `abandon_after`
//! (15 s) is cancelled, so a visitor who changes the search or leaves frees
//! its slot or its place in the queue. The warm-up has a slot of its own.
//! All of this is per replica (06 §6.6): a request that reaches another
//! replica starts or queues the search there afresh.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures::future::{BoxFuture, Shared};
use tokio::sync::{oneshot, Semaphore};
use tokio::time::Instant;

use crate::error::ApiError;

/// The result of one computation, shared by everyone waiting on it.
pub(crate) type Flight = Shared<BoxFuture<'static, Result<Arc<Vec<u8>>, ApiError>>>;

/// A computation in flight and who is waiting on it.
#[derive(Clone)]
pub(crate) struct Entry {
    pub(crate) flight: Flight,
    pub(crate) interest: Arc<Interest>,
    pub(crate) spot: Arc<Spot>,
}

/// Where a search computation is: waiting in the queue for a slot (with
/// its ticket), or not.
#[derive(Default)]
pub(crate) struct Spot(Mutex<Option<u64>>);

impl Spot {
    fn ticket(&self) -> MutexGuard<'_, Option<u64>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// How many requests wait on a computation, and since when none has.
pub(crate) struct Interest(Mutex<Waiting>);

struct Waiting {
    waiters: usize,
    last_left: Instant,
    /// Abandoned: no request may wait on it any more.
    closed: bool,
}

impl Interest {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(Waiting {
            waiters: 0,
            last_left: Instant::now(),
            closed: false,
        })))
    }

    fn lock(&self) -> MutexGuard<'_, Waiting> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Count a request as waiting until the guard drops; `None` if the
    /// computation has been abandoned (the request should ask again).
    pub(crate) fn watch(self: &Arc<Self>) -> Option<Watching> {
        let mut w = self.lock();
        if w.closed {
            return None;
        }
        w.waiters += 1;
        Some(Watching(self.clone()))
    }

    /// If nobody has waited on the computation for `after`, close it to new
    /// waiters and say so. One lock covers the check and the close, so a
    /// request can't start waiting on a computation that is being cancelled.
    pub(crate) fn abandon_if_idle(&self, after: Duration) -> bool {
        let mut w = self.lock();
        if w.waiters == 0 && w.last_left.elapsed() >= after {
            w.closed = true;
        }
        w.closed
    }
}

/// One request waiting on a computation (see [`Interest::watch`]).
pub(crate) struct Watching(Arc<Interest>);

impl Drop for Watching {
    fn drop(&mut self) {
        let mut w = self.0.lock();
        w.waiters -= 1;
        w.last_left = Instant::now();
    }
}

pub struct Flights {
    running: Mutex<HashMap<String, Entry>>,
    /// Slots for visitors' search computations and the queue for them,
    /// under one lock, so the queue alone decides who gets a freed slot.
    slots: Mutex<Slots>,
    /// How many slots `slots` has in all.
    search_slots: usize,
    /// The warm-up's own slot, so visitors' searches can't starve it (it
    /// runs one query at a time anyway).
    pub(crate) warm_up_slot: Arc<Semaphore>,
    next_ticket: AtomicU64,
    max_queue: usize,
}

struct Slots {
    free: usize,
    /// The searches waiting for a slot, by ticket (lower is earlier), each
    /// with the channel its slot is handed over on.
    waiting: BTreeMap<u64, oneshot::Sender<()>>,
}

impl Slots {
    /// Hand a freed slot to the earliest waiting search, or put it back.
    fn release(&mut self) {
        match self.waiting.pop_first() {
            // If the send fails, that search is being cancelled: its
            // `Place`, dropped with it, finds its ticket gone and the slot
            // not taken, and hands the slot on. Handing it on here as well
            // would free it twice.
            Some((_, wake)) => {
                let _ = wake.send(());
            }
            None => self.free += 1,
        }
    }
}

impl Flights {
    pub(crate) fn new(search_slots: usize, max_queue: usize) -> Self {
        let search_slots = search_slots.max(1);
        Self {
            running: Mutex::new(HashMap::new()),
            slots: Mutex::new(Slots {
                free: search_slots,
                waiting: BTreeMap::new(),
            }),
            search_slots,
            warm_up_slot: Arc::new(Semaphore::new(1)),
            next_ticket: AtomicU64::new(0),
            max_queue,
        }
    }

    /// The slots, even if a thread panicked while holding them: every
    /// change to them leaves them consistent.
    fn slots(&self) -> MutexGuard<'_, Slots> {
        self.slots.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A slot for a visitor's search: at once if one is free and nobody is
    /// waiting, else after the searches queued before it, holding `spot`'s
    /// place in the queue meanwhile (cancelling the wait gives the place
    /// up). Busy when the queue is full.
    pub(crate) async fn slot<'a>(&'a self, spot: &Spot) -> Result<SearchSlot<'a>, ApiError> {
        let (ticket, handed) = {
            let mut slots = self.slots();
            if slots.free > 0 && slots.waiting.is_empty() {
                slots.free -= 1;
                return Ok(SearchSlot(self));
            }
            if slots.waiting.len() >= self.max_queue {
                return Err(ApiError::Busy);
            }
            let ticket = self.next_ticket.fetch_add(1, Ordering::SeqCst);
            let (wake, handed) = oneshot::channel();
            slots.waiting.insert(ticket, wake);
            *spot.ticket() = Some(ticket);
            (ticket, handed)
        };
        let mut place = Place {
            flights: self,
            spot,
            ticket,
            taken: false,
        };
        // The sender is dropped only after a send or with the place itself.
        handed.await.map_err(|e| ApiError::Backend(e.to_string()))?;
        place.taken = true;
        Ok(SearchSlot(self))
    }

    /// How many searches are ahead of this one in the queue for a slot;
    /// `None` once it has left the queue (or never joined it).
    pub(crate) fn ahead(&self, spot: &Spot) -> Option<usize> {
        let ticket = (*spot.ticket())?;
        Some(self.slots().waiting.range(..ticket).count())
    }

    /// The map, even if a thread panicked while holding it: every change
    /// to it is a single insert or remove, so it is never half-updated.
    pub(crate) fn lock(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        self.running.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The computation running for `key`, if any.
    pub(crate) fn get(&self, key: &str) -> Option<Entry> {
        self.lock().get(key).cloned()
    }

    /// Computations running now.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Search slots free now.
    pub fn free_slots(&self) -> usize {
        self.slots().free
    }

    /// Searches waiting for a slot now.
    pub fn queued(&self) -> usize {
        self.slots().waiting.len()
    }

    /// Visitors' searches running or waiting for a slot now. The warm-up's
    /// own slot isn't one of these, so its searches don't count.
    pub fn visitor_searches(&self) -> usize {
        let slots = self.slots();
        self.search_slots.saturating_sub(slots.free) + slots.waiting.len()
    }
}

/// A visitor's search slot, handed on to the next search in the queue (or
/// freed) when dropped.
pub(crate) struct SearchSlot<'a>(&'a Flights);

impl Drop for SearchSlot<'_> {
    fn drop(&mut self) {
        self.0.slots().release();
    }
}

/// A search's place in the queue, given up when it gets its slot or stops
/// waiting. A slot handed to a search that stopped waiting before taking it
/// goes on to the next.
struct Place<'a> {
    flights: &'a Flights,
    spot: &'a Spot,
    ticket: u64,
    taken: bool,
}

impl Drop for Place<'_> {
    fn drop(&mut self) {
        let mut slots = self.flights.slots();
        if slots.waiting.remove(&self.ticket).is_none() && !self.taken {
            slots.release();
        }
        *self.spot.ticket() = None;
    }
}

/// Removes a computation's entry when its task ends, however it ends (a
/// panic included), so a key can never be stuck on a dead computation.
pub(crate) struct Landing {
    pub(crate) state: Arc<crate::AppState>,
    pub(crate) key: String,
}

impl Drop for Landing {
    fn drop(&mut self) {
        self.state.flights.lock().remove(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Waits until `n` searches are queued.
    async fn queued(flights: &Flights, n: usize) {
        while flights.queued() < n {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn slots_go_to_waiting_searches_in_ticket_order() {
        let flights = Arc::new(Flights::new(1, 64));
        let held = flights.slot(&Spot::default()).await.unwrap();
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut waiting = Vec::new();
        for i in 0..32 {
            let (mine, order) = (flights.clone(), order.clone());
            waiting.push(tokio::spawn(async move {
                let spot = Spot::default();
                let slot = mine.slot(&spot).await.unwrap();
                order.lock().unwrap().push(i);
                // Hold it a moment, so the next one really waits for it.
                tokio::task::yield_now().await;
                drop(slot);
            }));
            queued(&flights, i + 1).await;
        }
        drop(held);
        for w in waiting {
            w.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), (0..32).collect::<Vec<_>>());
        assert_eq!(flights.free_slots(), 1);
        assert_eq!(flights.queued(), 0);
    }

    #[tokio::test]
    async fn a_new_search_never_takes_a_slot_ahead_of_one_waiting() {
        let flights = Arc::new(Flights::new(1, 8));
        let held = flights.slot(&Spot::default()).await.unwrap();
        let first = tokio::spawn({
            let flights = flights.clone();
            async move {
                let spot = Spot::default();
                let _slot = flights.slot(&spot).await.unwrap();
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
        queued(&flights, 1).await;
        drop(held);
        // The freed slot went straight to the waiting search.
        assert_eq!(flights.free_slots(), 0);
        let spot = Spot::default();
        let late = flights.slot(&spot);
        tokio::pin!(late);
        assert!(futures::poll!(late.as_mut()).is_pending());
        assert_eq!(flights.ahead(&spot), Some(0));
        first.await.unwrap();
        drop(late.await.unwrap());
        assert_eq!(flights.free_slots(), 1);
    }

    #[tokio::test]
    async fn a_slot_handed_to_a_search_that_stopped_waiting_goes_on() {
        let flights = Flights::new(1, 8);
        let held = flights.slot(&Spot::default()).await.unwrap();
        let (gone, next) = (Spot::default(), Spot::default());
        let mut gone_wait = Box::pin(flights.slot(&gone));
        let mut next_wait = Box::pin(flights.slot(&next));
        assert!(futures::poll!(gone_wait.as_mut()).is_pending());
        assert!(futures::poll!(next_wait.as_mut()).is_pending());
        assert_eq!(flights.ahead(&next), Some(1));
        // The slot is handed to `gone`, which is cancelled before it takes it.
        drop(held);
        drop(gone_wait);
        assert_eq!(flights.ahead(&gone), None);
        let slot = next_wait.await.unwrap();
        assert_eq!(flights.queued(), 0);
        drop(slot);
        assert_eq!(flights.free_slots(), 1);
    }

    #[test]
    fn a_slot_whose_handover_fails_is_freed_once() {
        let flights = Flights::new(1, 8);
        let mut slots = flights.slots();
        slots.free = 0;
        let (wake, handed) = oneshot::channel();
        slots.waiting.insert(7, wake);
        drop(slots);
        let spot = Spot::default();
        *spot.ticket() = Some(7);
        let place = Place {
            flights: &flights,
            spot: &spot,
            ticket: 7,
            taken: false,
        };
        // The waiting search is cancelled: its receiver goes first, then
        // the slot is released (the send fails), then its place goes.
        drop(handed);
        drop(SearchSlot(&flights));
        assert_eq!(flights.free_slots(), 0, "the failed send freed nothing");
        drop(place);
        assert_eq!(flights.free_slots(), 1, "the place freed it, once");
    }

    #[tokio::test]
    async fn visitor_searches_count_the_running_and_the_queued_not_the_warm_up() {
        let flights = Flights::new(2, 8);
        assert_eq!(flights.visitor_searches(), 0);
        let _warm_up = flights.warm_up_slot.clone().acquire_owned().await.unwrap();
        assert_eq!(flights.visitor_searches(), 0, "the warm-up's slot");
        let first = flights.slot(&Spot::default()).await.unwrap();
        assert_eq!(flights.visitor_searches(), 1);
        let second = flights.slot(&Spot::default()).await.unwrap();
        let spot = Spot::default();
        let wait = flights.slot(&spot);
        tokio::pin!(wait);
        assert!(futures::poll!(wait.as_mut()).is_pending());
        assert_eq!(flights.visitor_searches(), 3, "two running, one queued");
        drop(first);
        let third = wait.await.unwrap();
        assert_eq!(flights.visitor_searches(), 2);
        drop((second, third));
        assert_eq!(flights.visitor_searches(), 0);
    }

    #[tokio::test]
    async fn a_full_queue_is_busy() {
        let flights = Flights::new(1, 1);
        let _held = flights.slot(&Spot::default()).await.unwrap();
        let spot = Spot::default();
        let wait = flights.slot(&spot);
        tokio::pin!(wait);
        assert!(futures::poll!(wait.as_mut()).is_pending());
        assert!(matches!(
            flights.slot(&Spot::default()).await,
            Err(ApiError::Busy)
        ));
    }
}

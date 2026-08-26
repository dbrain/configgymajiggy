use crate::config::Config;
use crate::pin::{Namespace, Pin};
use dashmap::{DashMap, mapref::entry::Entry};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tokio::sync::Notify;

/// The README documents this retry budget as a user-facing contract, so it is a
/// named constant rather than a literal in a loop header.
pub const ALLOCATION_ATTEMPTS: usize = 10;

pub type Payload = HashMap<String, Value>;

/// A structured key: with no delimiter to join on, `("a", "b:c")` and
/// `("a:b", "c")` cannot collapse onto the same entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PinKey {
    pub namespace: Namespace,
    pub pin: Pin,
}

impl PinKey {
    pub fn new(namespace: Namespace, pin: Pin) -> Self {
        Self { namespace, pin }
    }
}

struct PinItem {
    /// Monotonic, so an NTP step can neither resurrect an expired entry nor
    /// expire a live one.
    touched_at: Instant,
    result: Option<Payload>,
    /// Woken when a payload lands, so a long poll returns immediately instead of
    /// waiting out its timeout.
    arrival: Arc<Notify>,
}

impl PinItem {
    fn empty() -> Self {
        Self {
            touched_at: Instant::now(),
            result: None,
            arrival: Arc::new(Notify::new()),
        }
    }
}

/// Per-namespace bookkeeping: the quota counter, and the failed-guess counter
/// that rate limits enumeration. Held in a separate map from the pins so no
/// code path ever holds a guard on both.
#[derive(Debug)]
struct NamespaceState {
    live_pins: usize,
    misses: u32,
    window_started: Instant,
}

impl Default for NamespaceState {
    fn default() -> Self {
        Self {
            live_pins: 0,
            misses: 0,
            window_started: Instant::now(),
        }
    }
}

impl NamespaceState {
    /// Misses age out as a whole window rather than decaying individually, which
    /// keeps the state to one counter and one timestamp.
    fn roll_window(&mut self, window: std::time::Duration) {
        if self.window_started.elapsed() >= window {
            self.misses = 0;
            self.window_started = Instant::now();
        }
    }

    fn is_idle(&self, window: std::time::Duration) -> bool {
        self.live_pins == 0 && self.window_started.elapsed() >= window
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Poll {
    Delivered(Payload),
    Pending,
    Unknown,
    Throttled,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Deposit {
    Accepted,
    AlreadyPopulated,
    Unknown,
    Throttled,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Allocation {
    Allocated(Pin),
    /// The service as a whole is full.
    AtCapacity,
    /// This namespace is at its own quota, so one tenant cannot starve the rest.
    NamespaceFull,
}

/// Owns the map and every lock taken against it. Keeping all locking in one
/// place is what makes the atomicity of each transition auditable.
#[derive(Clone)]
pub struct PinStore {
    pins: Arc<DashMap<PinKey, PinItem>>,
    namespaces: Arc<DashMap<Namespace, NamespaceState>>,
    config: Arc<Config>,
    started: Instant,
    /// Millis-since-`started` of the last completed sweep, so readiness can tell
    /// whether the cleanup task is still alive.
    last_sweep_ms: Arc<AtomicU64>,
}

impl PinStore {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            pins: Arc::new(DashMap::new()),
            namespaces: Arc::new(DashMap::new()),
            config,
            started: Instant::now(),
            last_sweep_ms: Arc::new(AtomicU64::new(0)),
        }
    }

    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// False once a sweep is overdue, which is what a dead cleanup task looks
    /// like from the outside. Liveness stays up; only readiness drops.
    pub fn is_ready(&self) -> bool {
        let since_sweep = self
            .elapsed_ms()
            .saturating_sub(self.last_sweep_ms.load(Ordering::Relaxed));
        u128::from(since_sweep) <= self.config.sweep_deadline().as_millis()
    }

    /// Records a miss and reports whether this namespace has now spent its
    /// budget of wrong guesses.
    ///
    /// Only ever called once a lookup has already missed, so a caller holding a
    /// real pin is never throttled - a guesser can burn the budget without
    /// locking out the namespace's legitimate user.
    fn record_miss(&self, namespace: &Namespace) -> bool {
        // Fail closed if the bookkeeping table is full, so filling it cannot buy
        // unthrottled guessing.
        if self.namespaces.len() >= self.config.max_entries
            && !self.namespaces.contains_key(namespace)
        {
            return true;
        }

        let mut state = self.namespaces.entry(namespace.clone()).or_default();
        state.roll_window(self.config.probe_window);
        state.misses = state.misses.saturating_add(1);
        state.misses > self.config.max_probe_misses
    }

    fn release_pin(&self, namespace: &Namespace) {
        if let Some(mut state) = self.namespaces.get_mut(namespace) {
            state.live_pins = state.live_pins.saturating_sub(1);
        }
    }

    /// The handle a long poll waits on. Cloned out so the caller never holds a
    /// map guard across an await.
    pub fn arrival(&self, key: &PinKey) -> Option<Arc<Notify>> {
        self.pins.get(key).map(|item| Arc::clone(&item.arrival))
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn len(&self) -> usize {
        self.pins.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pins.is_empty()
    }

    pub fn allocate(&self, namespace: &Namespace) -> Allocation {
        if self.pins.len() >= self.config.max_entries {
            return Allocation::AtCapacity;
        }

        // Claim the quota slot first and drop the guard before touching `pins`,
        // so the two maps are never locked at the same time.
        {
            let mut state = self.namespaces.entry(namespace.clone()).or_default();
            if state.live_pins >= self.config.max_pins_per_namespace {
                return Allocation::NamespaceFull;
            }
            state.live_pins += 1;
        }

        for _ in 0..ALLOCATION_ATTEMPTS {
            let pin = Pin::generate(self.config.pin_length);
            let key = PinKey::new(namespace.clone(), pin.clone());

            if let Entry::Vacant(slot) = self.pins.entry(key) {
                slot.insert(PinItem::empty());
                return Allocation::Allocated(pin);
            }
        }

        self.release_pin(namespace);
        Allocation::AtCapacity
    }

    /// Reads, refreshes and (on delivery) removes under a single shard write
    /// lock, so concurrent pollers cannot both receive the same payload and a
    /// write landing mid-poll cannot be lost.
    pub fn poll(&self, key: &PinKey) -> Poll {
        let mut outcome = Poll::Unknown;

        self.pins.remove_if_mut(key, |_, item| {
            item.touched_at = Instant::now();
            if let Some(payload) = item.result.take() {
                outcome = Poll::Delivered(payload);
                true
            } else {
                outcome = Poll::Pending;
                false
            }
        });

        // Guards are released by here, so touching the namespace map is safe.
        match &outcome {
            Poll::Delivered(_) => self.release_pin(&key.namespace),
            Poll::Unknown if self.record_miss(&key.namespace) => return Poll::Throttled,
            _ => {}
        }

        outcome
    }

    pub fn deposit(&self, key: &PinKey, payload: Payload) -> Deposit {
        let outcome = match self.pins.get_mut(key) {
            None => Deposit::Unknown,
            Some(item) if item.result.is_some() => Deposit::AlreadyPopulated,
            Some(mut item) => {
                item.result = Some(payload);
                item.touched_at = Instant::now();
                item.arrival.notify_waiters();
                Deposit::Accepted
            }
        };

        if outcome == Deposit::Unknown && self.record_miss(&key.namespace) {
            return Deposit::Throttled;
        }
        outcome
    }

    /// Returns how many entries were evicted. Nothing but the freshness
    /// comparison happens inside the closure: `retain` holds a shard write lock
    /// while it runs, and logging there would block a runtime worker under it.
    pub fn sweep(&self) -> usize {
        self.last_sweep_ms
            .store(self.elapsed_ms(), Ordering::Relaxed);

        // Shorter uptime than the TTL means nothing can be stale yet.
        let Some(cutoff) = Instant::now().checked_sub(self.config.stale_age) else {
            return 0;
        };

        // Counting into a local map keeps the shard lock held for a comparison
        // and an integer bump, nothing more.
        let mut evicted: HashMap<Namespace, usize> = HashMap::new();
        self.pins.retain(|key, item| {
            let fresh = item.touched_at > cutoff;
            if !fresh {
                *evicted.entry(key.namespace.clone()).or_default() += 1;
            }
            fresh
        });

        let total = evicted.values().sum();
        for (namespace, count) in evicted {
            if let Some(mut state) = self.namespaces.get_mut(&namespace) {
                state.live_pins = state.live_pins.saturating_sub(count);
            }
        }

        // Namespace bookkeeping is itself unbounded unless idle entries go too.
        let window = self.config.probe_window;
        self.namespaces.retain(|_, state| !state.is_idle(window));

        total
    }

    #[cfg(test)]
    fn age(&self, key: &PinKey, by: std::time::Duration) {
        let mut item = self.pins.get_mut(key).expect("key must exist");
        item.touched_at = item.touched_at.checked_sub(by).expect("age within range");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    fn store() -> PinStore {
        PinStore::new(Arc::new(Config::default()))
    }

    fn namespace(raw: &str) -> Namespace {
        Namespace::parse(raw).expect("valid namespace")
    }

    fn payload(marker: &str) -> Payload {
        HashMap::from([("marker".to_string(), json!(marker))])
    }

    fn allocated(store: &PinStore, ns: &Namespace) -> PinKey {
        match store.allocate(ns) {
            Allocation::Allocated(pin) => PinKey::new(ns.clone(), pin),
            other => panic!("allocation should succeed: {other:?}"),
        }
    }

    #[test]
    fn allocation_is_unique_and_tracked() {
        let store = store();
        let ns = namespace("alloc");

        let keys: Vec<_> = (0..64).map(|_| allocated(&store, &ns)).collect();
        let distinct: std::collections::HashSet<_> = keys.iter().collect();

        assert_eq!(distinct.len(), keys.len(), "allocated pins must be unique");
        assert_eq!(store.len(), keys.len());
    }

    #[test]
    fn allocation_refuses_past_capacity() {
        let store = PinStore::new(Arc::new(Config {
            max_entries: 3,
            ..Config::default()
        }));
        let ns = namespace("cap");

        for _ in 0..3 {
            assert!(matches!(store.allocate(&ns), Allocation::Allocated(_)));
        }
        assert_eq!(store.allocate(&ns), Allocation::AtCapacity);
        assert_eq!(store.len(), 3);
    }

    #[test]
    fn poll_reports_unknown_without_allocating() {
        let store = store();
        let key = PinKey::new(namespace("probe"), Pin::parse("ZZZZ", 4).unwrap());

        assert_eq!(store.poll(&key), Poll::Unknown);
        assert!(store.is_empty(), "probing must not allocate");
    }

    #[test]
    fn payload_is_delivered_once_then_the_slot_is_gone() {
        let store = store();
        let key = allocated(&store, &namespace("once"));

        assert_eq!(store.poll(&key), Poll::Pending);
        assert_eq!(store.deposit(&key, payload("first")), Deposit::Accepted);
        assert_eq!(store.poll(&key), Poll::Delivered(payload("first")));
        assert_eq!(store.poll(&key), Poll::Unknown);
    }

    #[test]
    fn deposit_refuses_to_clobber_an_undelivered_payload() {
        let store = store();
        let key = allocated(&store, &namespace("clobber"));

        assert_eq!(store.deposit(&key, payload("first")), Deposit::Accepted);
        assert_eq!(
            store.deposit(&key, payload("second")),
            Deposit::AlreadyPopulated
        );
        assert_eq!(store.poll(&key), Poll::Delivered(payload("first")));
    }

    #[test]
    fn deposit_to_an_unknown_pin_is_reported() {
        let store = store();
        let key = PinKey::new(namespace("nope"), Pin::parse("ZZZZ", 4).unwrap());

        assert_eq!(store.deposit(&key, payload("x")), Deposit::Unknown);
        assert!(store.is_empty());
    }

    #[test]
    fn namespaces_are_isolated_even_when_they_share_a_pin() {
        let store = store();
        let pin = Pin::parse("ABCD", 4).unwrap();
        let left = PinKey::new(namespace("left"), pin.clone());
        let right = PinKey::new(namespace("right"), pin);

        store.allocate(&namespace("left"));
        assert_eq!(store.deposit(&left, payload("left")), Deposit::Unknown);

        store.pins.insert(left.clone(), PinItem::empty());
        assert_eq!(store.deposit(&left, payload("left")), Deposit::Accepted);
        assert_eq!(store.deposit(&right, payload("right")), Deposit::Unknown);
        assert_eq!(store.poll(&right), Poll::Unknown);
        assert_eq!(store.poll(&left), Poll::Delivered(payload("left")));
    }

    #[test]
    fn sweep_evicts_only_stale_entries() {
        let store = store();
        let ns = namespace("sweep");
        let stale = allocated(&store, &ns);
        let fresh = allocated(&store, &ns);

        store.age(&stale, Config::default().stale_age + Duration::from_secs(1));

        assert_eq!(store.sweep(), 1);
        assert_eq!(store.poll(&stale), Poll::Unknown);
        assert_eq!(store.poll(&fresh), Poll::Pending);
    }

    #[test]
    fn polling_refreshes_the_expiry_clock() {
        let store = store();
        let key = allocated(&store, &namespace("refresh"));

        // Nearly stale, then polled: the poll must reset the clock so a client
        // that keeps polling does not have its pin swept out from under it.
        store.age(
            &key,
            Config::default()
                .stale_age
                .checked_sub(Duration::from_secs(1))
                .unwrap(),
        );
        assert_eq!(store.poll(&key), Poll::Pending);

        store.age(&key, Duration::from_secs(2));
        assert_eq!(store.sweep(), 0, "a polled pin must not be swept");
        assert_eq!(store.poll(&key), Poll::Pending);
    }

    #[test]
    fn deposit_refreshes_the_expiry_clock() {
        let store = store();
        let key = allocated(&store, &namespace("refresh-put"));

        store.age(
            &key,
            Config::default()
                .stale_age
                .checked_sub(Duration::from_secs(1))
                .unwrap(),
        );
        assert_eq!(store.deposit(&key, payload("x")), Deposit::Accepted);

        store.age(&key, Duration::from_secs(2));
        assert_eq!(store.sweep(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_payload_is_handed_to_exactly_one_concurrent_poller() {
        let store = store();
        let key = allocated(&store, &namespace("once"));
        store.deposit(&key, payload("only"));

        let racers: Vec<_> = (0..32)
            .map(|_| {
                let (store, key) = (store.clone(), key.clone());
                tokio::spawn(async move { store.poll(&key) })
            })
            .collect();

        let mut delivered = 0;
        for racer in racers {
            if matches!(racer.await.unwrap(), Poll::Delivered(_)) {
                delivered += 1;
            }
        }

        assert_eq!(delivered, 1, "the result must reach exactly one poller");
        assert!(store.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_allocation_never_hands_out_a_duplicate() {
        let store = store();
        let ns = namespace("concurrent");

        let racers: Vec<_> = (0..64)
            .map(|_| {
                let (store, ns) = (store.clone(), ns.clone());
                tokio::spawn(async move { store.allocate(&ns) })
            })
            .collect();

        let mut pins = Vec::new();
        for racer in racers {
            match racer.await.unwrap() {
                Allocation::Allocated(pin) => pins.push(pin),
                other => panic!("capacity should not bind here: {other:?}"),
            }
        }

        let distinct: std::collections::HashSet<_> = pins.iter().collect();
        assert_eq!(distinct.len(), pins.len(), "allocations collided");
        assert_eq!(store.len(), pins.len());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_write_racing_a_poll_is_never_lost() {
        // poll() refreshes and takes under one lock; a deposit landing mid-poll
        // must either be seen by that poll or still be there for the next one.
        for _ in 0..200 {
            let store = store();
            let key = allocated(&store, &namespace("race"));

            let writer = tokio::spawn({
                let (store, key) = (store.clone(), key.clone());
                async move { store.deposit(&key, payload("racy")) }
            });
            let reader = tokio::spawn({
                let (store, key) = (store.clone(), key.clone());
                async move { store.poll(&key) }
            });

            assert_eq!(writer.await.unwrap(), Deposit::Accepted);
            if reader.await.unwrap() != Poll::Delivered(payload("racy")) {
                assert_eq!(
                    store.poll(&key),
                    Poll::Delivered(payload("racy")),
                    "the payload was dropped by a concurrent poll"
                );
            }
        }
    }

    #[test]
    fn quota_is_returned_when_a_payload_is_delivered() {
        let store = PinStore::new(Arc::new(Config {
            max_pins_per_namespace: 2,
            ..Config::default()
        }));
        let ns = namespace("quota");

        let first = allocated(&store, &ns);
        let second = allocated(&store, &ns);
        assert_eq!(store.allocate(&ns), Allocation::NamespaceFull);

        // Consuming a pin must hand its quota slot back, or a busy namespace
        // slowly locks itself out.
        store.deposit(&first, payload("x"));
        assert!(matches!(store.poll(&first), Poll::Delivered(_)));
        assert!(matches!(store.allocate(&ns), Allocation::Allocated(_)));

        assert_eq!(store.poll(&second), Poll::Pending);
    }

    #[test]
    fn quota_is_returned_when_pins_are_swept() {
        let store = PinStore::new(Arc::new(Config {
            max_pins_per_namespace: 2,
            ..Config::default()
        }));
        let ns = namespace("swept-quota");

        let first = allocated(&store, &ns);
        let second = allocated(&store, &ns);
        for key in [&first, &second] {
            store.age(key, Config::default().stale_age + Duration::from_secs(1));
        }

        assert_eq!(store.sweep(), 2);
        assert!(matches!(store.allocate(&ns), Allocation::Allocated(_)));
        assert!(matches!(store.allocate(&ns), Allocation::Allocated(_)));
        assert_eq!(store.allocate(&ns), Allocation::NamespaceFull);
    }

    #[test]
    fn quotas_are_independent_across_namespaces() {
        let store = PinStore::new(Arc::new(Config {
            max_pins_per_namespace: 1,
            ..Config::default()
        }));

        assert!(matches!(
            store.allocate(&namespace("greedy")),
            Allocation::Allocated(_)
        ));
        assert_eq!(
            store.allocate(&namespace("greedy")),
            Allocation::NamespaceFull
        );
        assert!(matches!(
            store.allocate(&namespace("polite")),
            Allocation::Allocated(_)
        ));
    }

    #[test]
    fn wrong_guesses_throttle_then_recover_after_the_window() {
        let store = PinStore::new(Arc::new(Config {
            max_probe_misses: 3,
            probe_window: Duration::from_millis(80),
            ..Config::default()
        }));
        let ns = namespace("guessy");
        let key = PinKey::new(ns.clone(), Pin::parse("ZZZZ", 4).unwrap());

        for _ in 0..3 {
            assert_eq!(store.poll(&key), Poll::Unknown);
        }
        assert_eq!(store.poll(&key), Poll::Throttled);
        assert_eq!(store.deposit(&key, payload("x")), Deposit::Throttled);
        assert_eq!(store.len(), 0, "guessing must never allocate");

        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(store.poll(&key), Poll::Unknown, "window should have rolled");
    }

    #[test]
    fn throttling_never_blocks_a_caller_holding_a_real_pin() {
        let store = PinStore::new(Arc::new(Config {
            max_probe_misses: 1,
            ..Config::default()
        }));
        let ns = namespace("mixed");
        let real = allocated(&store, &ns);
        let bogus = PinKey::new(ns, Pin::parse("ZZZZ", 4).unwrap());

        assert_eq!(store.poll(&bogus), Poll::Unknown);
        assert_eq!(store.poll(&bogus), Poll::Throttled);

        // The throttle is only consulted once a lookup has already missed, so a
        // guesser cannot lock the namespace's legitimate user out.
        assert_eq!(store.poll(&real), Poll::Pending);
        assert_eq!(store.deposit(&real, payload("x")), Deposit::Accepted);
        assert!(matches!(store.poll(&real), Poll::Delivered(_)));
    }

    #[test]
    fn readiness_tracks_the_sweeper() {
        let store = PinStore::new(Arc::new(Config {
            cleanup_interval: Duration::from_millis(10),
            ..Config::default()
        }));
        assert!(store.is_ready());

        std::thread::sleep(Duration::from_millis(120));
        assert!(!store.is_ready(), "an overdue sweep must fail readiness");

        store.sweep();
        assert!(store.is_ready(), "a completed sweep restores readiness");
    }

    #[test]
    fn sweep_prunes_idle_namespace_bookkeeping() {
        let store = PinStore::new(Arc::new(Config {
            probe_window: Duration::from_millis(10),
            ..Config::default()
        }));
        let key = PinKey::new(namespace("ghost"), Pin::parse("ZZZZ", 4).unwrap());

        assert_eq!(store.poll(&key), Poll::Unknown);
        assert_eq!(store.namespaces.len(), 1);

        std::thread::sleep(Duration::from_millis(30));
        store.sweep();
        assert_eq!(
            store.namespaces.len(),
            0,
            "idle namespace state must not accumulate"
        );
    }

    #[test]
    fn sweep_is_a_noop_when_uptime_is_shorter_than_the_ttl() {
        let store = PinStore::new(Arc::new(Config {
            stale_age: Duration::from_secs(60 * 60 * 24 * 365 * 100),
            ..Config::default()
        }));
        allocated(&store, &namespace("young"));

        assert_eq!(store.sweep(), 0, "must not panic on Instant underflow");
        assert_eq!(store.len(), 1);
    }
}

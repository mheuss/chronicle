//! Frontmost-app metadata provider with a short-TTL cache.
//!
//! `get_frontmost_app` performs a `CGWindowListCopyWindowInfo` sweep on
//! every call. The screenshot pipeline invokes it for every frame, which
//! is wasteful at 0.5 fps across two displays. `CachingAppMetadataProvider`
//! memoizes the result for 250 ms (~1/8 of the default 2 s capture
//! interval), keeping the hot path light while still picking up app
//! switches quickly.

#![allow(dead_code)]

use std::sync::Mutex;
use std::time::{Duration, Instant};

use chronicle_capture::AppMetadata;

use crate::pipeline::sinks::AppMetadataProvider;

/// Pluggable clock so tests can advance time deterministically.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Instant;
}

/// Real monotonic clock used in production.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Cached metadata provider. `F` is the underlying lookup; in production
/// this is `chronicle_capture::get_frontmost_app`.
pub struct CachingAppMetadataProvider<F, C = SystemClock>
where
    F: Fn() -> AppMetadata + Send + Sync,
    C: Clock,
{
    lookup: F,
    cache: Mutex<Option<(Instant, AppMetadata)>>,
    ttl: Duration,
    clock: C,
}

impl<F, C> CachingAppMetadataProvider<F, C>
where
    F: Fn() -> AppMetadata + Send + Sync,
    C: Clock,
{
    pub fn new(lookup: F, ttl: Duration, clock: C) -> Self {
        Self {
            lookup,
            cache: Mutex::new(None),
            ttl,
            clock,
        }
    }
}

impl<F> CachingAppMetadataProvider<F, SystemClock>
where
    F: Fn() -> AppMetadata + Send + Sync,
{
    pub fn with_default_clock(lookup: F, ttl: Duration) -> Self {
        Self::new(lookup, ttl, SystemClock)
    }
}

impl<F, C> AppMetadataProvider for CachingAppMetadataProvider<F, C>
where
    F: Fn() -> AppMetadata + Send + Sync,
    C: Clock,
{
    fn frontmost(&self) -> AppMetadata {
        // Hold the lock only for cache reads/writes, never across the
        // underlying lookup. That prevents a panic in `(self.lookup)()`
        // from poisoning the mutex and bricking every later call, and
        // it keeps concurrent misses from serializing on the lookup.
        // The trade-off is a benign race: two concurrent misses can
        // both run the lookup and whichever writes last wins. At the
        // pipeline's 0.5 fps that is fine.
        let now = self.clock.now();
        {
            let guard = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((at, ref meta)) = *guard
                && now.duration_since(at) < self.ttl
            {
                return meta.clone();
            }
        }
        let fresh = (self.lookup)();
        let mut guard = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some((now, fresh.clone()));
        fresh
    }
}

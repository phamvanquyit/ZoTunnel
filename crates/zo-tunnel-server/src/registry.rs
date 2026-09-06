//! Client registry — tracks connected tunnel clients.

use crate::server::YamuxHandle;
use dashmap::DashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use zo_tunnel_protocol::{SESSION_STALE_SECS, TAKEOVER_AFTER_MISSED_HEARTBEATS};

/// Handle to a connected client's yamux session.
pub struct ClientEntry {
    pub client_id: String,
    pub session_id: u64,
    pub handle: YamuxHandle,
    pub connected_at: Instant,
    pub last_activity: Arc<Mutex<Instant>>,
    pub missed_heartbeats: Arc<AtomicU32>,
    pub metrics: ClientMetrics,
    pub supports_heartbeat: bool,
}

impl ClientEntry {
    pub fn touch(&self) {
        if let Ok(mut ts) = self.last_activity.lock() {
            *ts = Instant::now();
        }
    }

    /// Healthy duplicates stay registered. Takeover is allowed when:
    /// - the yamux driver is gone, or
    /// - two consecutive heartbeats have failed (not a single jitter miss), or
    /// - there has been no activity for `SESSION_STALE_SECS` (driver stuck /
    ///   legacy client with no heartbeat probes).
    ///
    /// A poisoned `last_activity` lock is treated as dead so reconnect can
    /// recover rather than wedging the name forever.
    pub fn is_alive(&self) -> bool {
        if self.handle.is_closed() {
            return false;
        }
        if self.supports_heartbeat
            && self.missed_heartbeats.load(Ordering::Relaxed) >= TAKEOVER_AFTER_MISSED_HEARTBEATS
        {
            return false;
        }
        self.last_activity
            .lock()
            .map(|ts| ts.elapsed() < Duration::from_secs(SESSION_STALE_SECS))
            .unwrap_or(false)
    }
}

/// Per-client traffic metrics.
pub struct ClientMetrics {
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
    pub requests: AtomicU64,
    pub active_streams: AtomicU64,
}

impl ClientMetrics {
    pub fn new() -> Self {
        Self {
            bytes_in: AtomicU64::new(0),
            bytes_out: AtomicU64::new(0),
            requests: AtomicU64::new(0),
            active_streams: AtomicU64::new(0),
        }
    }
}

/// Serializable client info for the dashboard API.
#[derive(serde::Serialize, Clone)]
pub struct ClientInfo {
    pub client_id: String,
    pub connected_at_secs: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub total_requests: u64,
    pub active_streams: u64,
}

/// Thread-safe client registry.
pub struct Registry {
    clients: DashMap<String, Arc<ClientEntry>>,
    next_session: AtomicU64,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            clients: DashMap::new(),
            next_session: AtomicU64::new(1),
        }
    }

    /// Register a client. Replaces a stale session with the same id.
    /// Returns `(entry, replaced)`. Errors if the id is reserved or a healthy
    /// session is already registered.
    pub fn register(
        &self,
        client_id: String,
        handle: YamuxHandle,
        supports_heartbeat: bool,
        last_activity: Arc<Mutex<Instant>>,
        missed_heartbeats: Arc<AtomicU32>,
    ) -> anyhow::Result<(Arc<ClientEntry>, bool)> {
        use dashmap::mapref::entry::Entry;

        if crate::config::RESERVED_SUBDOMAINS.contains(&client_id.as_str()) {
            anyhow::bail!(
                "Client ID '{}' is reserved (conflicts with system subdomain)",
                client_id
            );
        }

        let session_id = self.next_session.fetch_add(1, Ordering::Relaxed);
        let entry = Arc::new(ClientEntry {
            client_id: client_id.clone(),
            session_id,
            handle,
            connected_at: Instant::now(),
            last_activity,
            missed_heartbeats,
            metrics: ClientMetrics::new(),
            supports_heartbeat,
        });

        match self.clients.entry(client_id.clone()) {
            Entry::Occupied(mut occupied) => {
                if occupied.get().is_alive() {
                    anyhow::bail!("Client '{}' already registered", client_id);
                }
                let old = occupied.insert(entry.clone());
                old.handle.abort();
                Ok((entry, true))
            }
            Entry::Vacant(vacant) => {
                vacant.insert(entry.clone());
                Ok((entry, false))
            }
        }
    }

    /// Unregister a session. No-op if a newer session already replaced it.
    pub fn unregister(&self, client_id: &str, session_id: u64) -> bool {
        self.clients
            .remove_if(client_id, |_, e| e.session_id == session_id)
            .is_some()
    }

    /// Get a client entry by ID.
    pub fn get(&self, client_id: &str) -> Option<Arc<ClientEntry>> {
        self.clients.get(client_id).map(|e| e.value().clone())
    }

    /// List all connected clients.
    pub fn list(&self) -> Vec<ClientInfo> {
        self.clients
            .iter()
            .map(|entry| {
                let e = entry.value();
                ClientInfo {
                    client_id: e.client_id.clone(),
                    connected_at_secs: e.connected_at.elapsed().as_secs(),
                    bytes_in: e.metrics.bytes_in.load(Ordering::Relaxed),
                    bytes_out: e.metrics.bytes_out.load(Ordering::Relaxed),
                    total_requests: e.metrics.requests.load(Ordering::Relaxed),
                    active_streams: e.metrics.active_streams.load(Ordering::Relaxed),
                }
            })
            .collect()
    }

    /// Count connected clients.
    pub fn count(&self) -> usize {
        self.clients.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::YamuxHandle;

    fn live_handle() -> (
        YamuxHandle,
        tokio::sync::mpsc::Receiver<crate::server::YamuxCmd>,
    ) {
        YamuxHandle::mock()
    }

    fn dead_handle() -> YamuxHandle {
        YamuxHandle::mock_closed()
    }

    fn activity(age_secs: u64, missed: u32) -> (Arc<Mutex<Instant>>, Arc<AtomicU32>) {
        (
            Arc::new(Mutex::new(Instant::now() - Duration::from_secs(age_secs))),
            Arc::new(AtomicU32::new(missed)),
        )
    }

    #[tokio::test]
    async fn register_fresh_session() {
        let reg = Registry::new();
        let (handle, _rx) = live_handle();
        let (last, missed) = activity(0, 0);
        let (entry, replaced) = reg
            .register("app".into(), handle, true, last, missed)
            .expect("register");
        assert!(!replaced);
        assert_eq!(reg.count(), 1);
        assert!(reg.unregister("app", entry.session_id));
        assert_eq!(reg.count(), 0);
    }

    #[tokio::test]
    async fn reject_healthy_duplicate() {
        let reg = Registry::new();
        let (h1, _rx) = live_handle();
        let (last, missed) = activity(0, 0);
        let (first, _) = reg
            .register("app".into(), h1, true, last, missed)
            .expect("first");
        let (h2, _rx2) = live_handle();
        let (last2, missed2) = activity(0, 0);
        match reg.register("app".into(), h2, true, last2, missed2) {
            Ok(_) => panic!("expected duplicate to fail"),
            Err(e) => assert!(e.to_string().contains("already registered")),
        }
        assert_eq!(reg.get("app").unwrap().session_id, first.session_id);
    }

    #[tokio::test]
    async fn one_missed_heartbeat_does_not_takeover() {
        let reg = Registry::new();
        let (h1, _rx) = live_handle();
        let (last, missed) = activity(25, 1);
        let (old, _) = reg
            .register("app".into(), h1, true, last, missed)
            .expect("old");
        let (h2, _rx2) = live_handle();
        let (last2, missed2) = activity(0, 0);
        match reg.register("app".into(), h2, true, last2, missed2) {
            Ok(_) => panic!("single miss must not evict a live session"),
            Err(e) => assert!(e.to_string().contains("already registered")),
        }
        assert_eq!(reg.get("app").unwrap().session_id, old.session_id);
    }

    #[tokio::test]
    async fn takeover_after_two_missed_heartbeats() {
        let reg = Registry::new();
        let (h1, _rx) = live_handle();
        let (last, missed) = activity(20, TAKEOVER_AFTER_MISSED_HEARTBEATS);
        let (old, _) = reg
            .register("app".into(), h1, true, last, missed)
            .expect("old");
        let (h2, _rx2) = live_handle();
        let (last2, missed2) = activity(0, 0);
        let (new, replaced) = reg
            .register("app".into(), h2, true, last2, missed2)
            .expect("takeover");
        assert!(replaced);
        assert_ne!(old.session_id, new.session_id);
        assert!(!reg.unregister("app", old.session_id));
        assert!(reg.unregister("app", new.session_id));
    }

    #[tokio::test]
    async fn takeover_after_wall_clock_stale() {
        let reg = Registry::new();
        let (h1, _rx) = live_handle();
        let (stale, missed) = activity(SESSION_STALE_SECS + 1, 0);
        let (old, _) = reg
            .register("app".into(), h1, true, stale, missed)
            .expect("old");
        let (h2, _rx2) = live_handle();
        let (last2, missed2) = activity(0, 0);
        let (new, replaced) = reg
            .register("app".into(), h2, true, last2, missed2)
            .expect("takeover");
        assert!(replaced);
        assert_ne!(old.session_id, new.session_id);
    }

    #[tokio::test]
    async fn takeover_closed_driver_even_if_recent() {
        let reg = Registry::new();
        let h1 = dead_handle();
        let (last, missed) = activity(0, 0);
        let (old, _) = reg
            .register("app".into(), h1, true, last, missed)
            .expect("old");
        let (h2, _rx) = live_handle();
        let (last2, missed2) = activity(0, 0);
        let (new, replaced) = reg
            .register("app".into(), h2, true, last2, missed2)
            .expect("takeover");
        assert!(replaced);
        assert_ne!(old.session_id, new.session_id);
    }

    #[tokio::test]
    async fn legacy_client_stays_until_idle_or_closed() {
        let reg = Registry::new();
        let (h1, _rx) = live_handle();
        let (last, missed) = activity(25, 0);
        let (old, _) = reg
            .register("app".into(), h1, false, last, missed)
            .expect("old");
        let (h2, _rx2) = live_handle();
        let (last2, missed2) = activity(0, 0);
        match reg.register("app".into(), h2, false, last2, missed2) {
            Ok(_) => panic!("idle-but-fresh legacy client must not be kicked"),
            Err(e) => assert!(e.to_string().contains("already registered")),
        }
        assert_eq!(reg.get("app").unwrap().session_id, old.session_id);
    }

    #[tokio::test]
    async fn legacy_client_takeover_after_idle() {
        let reg = Registry::new();
        let (h1, _rx) = live_handle();
        let (stale, missed) = activity(SESSION_STALE_SECS + 1, 0);
        let (old, _) = reg
            .register("app".into(), h1, false, stale, missed)
            .expect("old");
        let (h2, _rx2) = live_handle();
        let (last2, missed2) = activity(0, 0);
        let (new, replaced) = reg
            .register("app".into(), h2, false, last2, missed2)
            .expect("takeover");
        assert!(replaced);
        assert_ne!(old.session_id, new.session_id);
    }

    #[tokio::test]
    async fn reject_reserved_subdomain() {
        let reg = Registry::new();
        let (handle, _rx) = live_handle();
        let (last, missed) = activity(0, 0);
        match reg.register("dashboard".into(), handle, true, last, missed) {
            Ok(_) => panic!("expected reserved id to fail"),
            Err(e) => assert!(e.to_string().contains("reserved")),
        }
    }
}

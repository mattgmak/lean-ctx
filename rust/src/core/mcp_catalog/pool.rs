//! Persistent downstream MCP session pool (#1078).
//!
//! Without pooling, every `ctx_tools` list/call reopened a connection — spawning
//! a fresh child process and re-running the MCP `initialize` handshake — so each
//! tool call paid the full spawn+handshake latency. This pool keeps one live
//! [`ClientService`] per distinct wiring and reuses it across calls:
//!
//! - **keyed** by the resolved transport (same command/args/env/caps/url → same
//!   session), so two different servers never share a child,
//! - **idle-evicted**: a session unused for `IDLE_TTL` is dropped on the next
//!   access (closing the child's stdin → the server exits), swept opportunistically
//!   on every [`acquire`],
//! - **liveness-checked**: [`acquire`] also drops any session whose transport has
//!   closed (the child exited/crashed) *before* handing one out, so a request is
//!   never sent into a dead pipe — and callers never have to blindly re-send a
//!   request to recover (which could double-execute a non-idempotent tool).
//!
//! The map lock is a `std::sync::Mutex` held only for short, await-free critical
//! sections (the slow `open()` runs outside the lock), which keeps [`clear`]
//! callable from the synchronous config/catalog paths.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::client::{self, ClientService};
use super::config::ResolvedTransport;

/// Drop a pooled session after this long without use, so an idle child process
/// does not linger. The next call for that wiring transparently reopens.
const IDLE_TTL: Duration = Duration::from_mins(5);

struct Entry {
    service: Arc<ClientService>,
    last_used: Instant,
}

fn pool() -> &'static Mutex<HashMap<u64, Entry>> {
    static POOL: OnceLock<Mutex<HashMap<u64, Entry>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Stable identity for a resolved transport: same wiring → same key → same
/// pooled session. Derived from the transport's `Debug` form so every field
/// (command/args/env/binary pin/capabilities, or url/headers) is captured.
#[must_use]
pub fn key(transport: &ResolvedTransport) -> u64 {
    let mut h = DefaultHasher::new();
    format!("{transport:?}").hash(&mut h);
    h.finish()
}

/// A live session for `transport`, reusing a pooled one when present + fresh, or
/// opening (and caching) a new one. Sweeps idle sessions on the way in.
pub async fn acquire(
    transport: &ResolvedTransport,
    timeout: Duration,
) -> Result<Arc<ClientService>, String> {
    let k = key(transport);
    {
        let mut map = lock();
        let now = Instant::now();
        // Sweep on the way in: drop sessions that are idle-expired *or* whose
        // transport has closed (child exited/crashed). Dropping an Entry releases
        // its `ClientService`, which tears down the child. A dead session is thus
        // never handed out, so callers never send into a dead pipe.
        map.retain(|_, e| now.duration_since(e.last_used) < IDLE_TTL && !e.service.is_closed());
        if let Some(entry) = map.get_mut(&k) {
            entry.last_used = now;
            return Ok(entry.service.clone());
        }
    }

    // Open outside the lock: a slow connect must not block other servers, and we
    // never hold a std Mutex across an await.
    let service = Arc::new(client::open(transport, timeout).await?);
    let mut map = lock();
    // A racing first-call may have inserted already; last writer wins and the
    // loser's child is closed when its Arc drops.
    map.insert(
        k,
        Entry {
            service: service.clone(),
            last_used: Instant::now(),
        },
    );
    Ok(service)
}

/// Drop the pooled session for `key` (e.g. after a transport-level failure), so
/// the next [`acquire`] reopens a fresh one.
pub fn evict(key: u64) {
    lock().remove(&key);
}

/// Drop every pooled session (closing all children). Called when the gateway
/// wiring changes (install/remove/revoke → [`super::catalog::invalidate`]).
pub fn clear() {
    lock().clear();
}

/// Number of live pooled sessions (test/diagnostic helper).
#[must_use]
pub fn len() -> usize {
    lock().len()
}

fn lock() -> std::sync::MutexGuard<'static, HashMap<u64, Entry>> {
    // A poisoned lock only means a previous holder panicked mid-map-op; the map
    // is still structurally valid, so recover rather than propagate the panic.
    pool()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn stdio(cmd: &str) -> ResolvedTransport {
        ResolvedTransport::Stdio {
            command: cmd.into(),
            args: vec![],
            env: BTreeMap::new(),
            binary_sha256: String::new(),
            capabilities: None,
        }
    }

    #[test]
    fn key_is_stable_and_wiring_sensitive() {
        assert_eq!(key(&stdio("a")), key(&stdio("a")), "same wiring → same key");
        assert_ne!(
            key(&stdio("a")),
            key(&stdio("b")),
            "different command → different key"
        );
    }

    #[test]
    fn evict_and_clear_are_safe_when_empty() {
        clear();
        evict(key(&stdio("never-pooled")));
        clear();
        assert_eq!(len(), 0);
    }
}

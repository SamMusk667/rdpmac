//! Who is connected, as the control socket reports it. IronRDP's connection handler sees the
//! client's address and the end of each connection; a wrapper around the credential validator
//! adds the user once the credentials passed.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use ironrdp_server::{
    ConnectionHandler, ConnectionInfo, CredentialDecision, CredentialValidationError, CredentialValidator,
    Credentials, PostConnectionAction, ServerError,
};
use rdpmac_auth::bare_username;
use rdpmac_session::unlock::Unlocker;
use serde::Serialize;
use tracing::warn;

/// Seconds since the Unix epoch.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize)]
pub struct Connection {
    pub peer: SocketAddr,
    pub since: u64,
    /// Set once the credentials passed.
    pub user: Option<String>,
    /// Came back with its auto-reconnect cookie after its connection dropped.
    pub reconnected: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Ended {
    pub peer: SocketAddr,
    pub user: Option<String>,
    pub ended: u64,
    pub seconds: u64,
    pub error: Option<String>,
}

#[derive(Default)]
struct State {
    current: Option<Connection>,
    last: Option<Ended>,
}

#[derive(Default)]
pub struct Tracker {
    state: Mutex<State>,
}

impl Tracker {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn current(&self) -> Option<Connection> {
        self.lock().current.clone()
    }

    pub fn last(&self) -> Option<Ended> {
        self.lock().last.clone()
    }

    fn accepted(&self, peer: SocketAddr) {
        self.lock().current = Some(Connection {
            peer,
            since: now(),
            user: None,
            reconnected: false,
        });
    }

    fn authenticated(&self, user: &str) {
        if let Some(connection) = self.lock().current.as_mut() {
            connection.user = Some(user.to_owned());
        }
    }

    /// The connection is set up. One whose credentials were not checked came back with its
    /// auto-reconnect cookie, which only the client of the session before it holds.
    fn established(&self) {
        let mut state = self.lock();
        let previous = state.last.as_ref().and_then(|last| last.user.clone());
        if let Some(connection) = state.current.as_mut() {
            if connection.user.is_none() {
                connection.user = previous;
                connection.reconnected = true;
            }
        }
    }

    fn ended(&self, peer: SocketAddr, duration: Duration, error: Option<&ServerError>) {
        let mut state = self.lock();
        let user = state.current.take().and_then(|c| c.user);
        state.last = Some(Ended {
            peer,
            user,
            ended: now(),
            seconds: duration.as_secs(),
            // The error with its causes.
            error: error.map(|e| e.report().to_string()),
        });
    }
}

/// Hands IronRDP's connection events to a [`Tracker`], and drops a password held for the lock
/// screen that its session never used.
pub struct Connections(pub Arc<Tracker>, pub Option<Arc<Unlocker>>);

impl ConnectionHandler for Connections {
    fn on_accept(&mut self, peer: SocketAddr) -> bool {
        self.0.accepted(peer);
        if let Some(unlocker) = &self.1 {
            unlocker.forget();
        }
        // macOS drops posted events silently, so injection itself never reports this.
        if !screenio_core::session_info().can_inject {
            warn!(
                %peer,
                "accessibility permission is missing: macOS drops this client's clicks and keys; allow rdpmacd \
                 under System Settings > Privacy & Security > Accessibility, then restart the server"
            );
        }
        true
    }

    fn on_connection_info(&mut self, _info: &ConnectionInfo) {
        self.0.established();
    }

    fn on_disconnected(
        &mut self,
        peer: SocketAddr,
        duration: Duration,
        error: Option<&ServerError>,
    ) -> PostConnectionAction {
        self.0.ended(peer, duration, error);
        if let Some(unlocker) = &self.1 {
            unlocker.forget();
        }
        PostConnectionAction::Continue
    }
}

/// Records who logged in on the tracker.
pub struct Recorded {
    inner: Arc<dyn CredentialValidator>,
    tracker: Arc<Tracker>,
}

impl Recorded {
    pub fn new(inner: Arc<dyn CredentialValidator>, tracker: Arc<Tracker>) -> Self {
        Self { inner, tracker }
    }
}

#[async_trait]
impl CredentialValidator for Recorded {
    async fn validate(&self, credentials: &Credentials) -> Result<CredentialDecision, CredentialValidationError> {
        let decision = self.inner.validate(credentials).await?;
        if decision == CredentialDecision::Accept {
            self.tracker.authenticated(bare_username(&credentials.username));
        }
        Ok(decision)
    }
}

#[cfg(test)]
mod tests {
    use ironrdp_server::ServerErrorExt as _;

    use super::*;

    #[test]
    fn a_connection_moves_to_last_when_it_ends() {
        let tracker = Arc::new(Tracker::default());
        let mut handler = Connections(tracker.clone(), None);
        let peer: SocketAddr = "192.0.2.7:50000".parse().expect("address");
        assert!(handler.on_accept(peer));
        tracker.authenticated("alice");
        let current = tracker.current().expect("connected");
        assert_eq!((current.peer, current.user.as_deref()), (peer, Some("alice")));

        let error = ServerError::io("reading", std::io::Error::other("reset"));
        handler.on_disconnected(peer, Duration::from_secs(42), Some(&error));
        assert!(tracker.current().is_none());
        let last = tracker.last().expect("ended");
        assert_eq!((last.user.as_deref(), last.seconds), (Some("alice"), 42));
        assert_eq!(last.error.as_deref(), Some("[reading] I/O error, caused by: reset"));
    }

    #[test]
    fn a_reconnection_keeps_the_user_of_the_connection_it_replaces() {
        let tracker = Arc::new(Tracker::default());
        let mut handler = Connections(tracker.clone(), None);
        let peer: SocketAddr = "192.0.2.7:50000".parse().expect("address");
        assert!(handler.on_accept(peer));
        tracker.authenticated("alice");
        tracker.established();
        assert!(!tracker.current().expect("connected").reconnected, "signed in");
        handler.on_disconnected(peer, Duration::from_secs(600), None);

        // Back from a new address with its cookie: no credentials were checked.
        let back: SocketAddr = "198.51.100.4:50123".parse().expect("address");
        assert!(handler.on_accept(back));
        tracker.established();
        let current = tracker.current().expect("connected");
        assert_eq!((current.user.as_deref(), current.reconnected), (Some("alice"), true));
    }
}

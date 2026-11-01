//! Connection session state management

use std::sync::Arc;

use uuid::Uuid;

use crate::identity::UserEndpoint;

/// Encapsulated session state for a client connection
pub struct Session {
    session_id: Uuid,
    current_group: Option<String>,
    current_article: Option<u64>,
    authenticated: bool,
    username: Option<String>,
    is_tls: bool,
    in_stream_mode: bool,
    allow_auth_insecure: bool,
    allow_anonymous_posting: bool,
    is_admin: bool,
    peer_endpoint: Option<String>,
    bindings: Option<Arc<dyn UserEndpoint>>,
}

impl Session {
    pub fn new(is_tls: bool, allow_auth_insecure: bool, allow_anonymous_posting: bool) -> Self {
        Self {
            session_id: Uuid::new_v4(),
            current_group: None,
            current_article: None,
            authenticated: false,
            username: None,
            is_tls,
            in_stream_mode: false,
            allow_auth_insecure,
            allow_anonymous_posting,
            is_admin: false,
            peer_endpoint: None,
            bindings: None,
        }
    }

    /// Remember the iroh peer and where to check key-bound accounts.
    pub fn set_peer(&mut self, endpoint_id: String, bindings: Arc<dyn UserEndpoint>) {
        self.peer_endpoint = Some(endpoint_id);
        self.bindings = Some(bindings);
    }

    pub fn peer_endpoint(&self) -> Option<&str> {
        self.peer_endpoint.as_deref()
    }

    pub fn bindings(&self) -> Option<&Arc<dyn UserEndpoint>> {
        self.bindings.as_ref()
    }

    /// Get the unique session identifier for this connection
    pub fn session_id(&self) -> Uuid {
        self.session_id
    }

    // Group management
    pub fn select_group(&mut self, group: String, first_article: Option<u64>) {
        self.current_group = Some(group);
        self.current_article = first_article;
    }

    pub fn current_group(&self) -> Option<&str> {
        self.current_group.as_deref()
    }

    pub fn leave_group(&mut self) {
        self.current_group = None;
        self.current_article = None;
    }

    // Article navigation
    pub fn current_article(&self) -> Option<u64> {
        self.current_article
    }

    pub fn set_current_article(&mut self, num: u64) {
        self.current_article = Some(num);
    }

    // Authentication
    /// Set the pending username for AUTHINFO USER/PASS flow.
    /// Called when USER is received but before PASS is verified.
    pub fn set_pending_username(&mut self, username: String) {
        self.username = Some(username);
    }

    /// Get the pending username set by AUTHINFO USER.
    pub fn pending_username(&self) -> Option<&str> {
        self.username.as_deref()
    }

    pub fn authenticate(&mut self, username: String) {
        self.authenticated = true;
        self.username = Some(username);
    }

    /// Authenticate user with admin status
    pub fn authenticate_with_admin(&mut self, username: String, is_admin: bool) {
        self.authenticated = true;
        self.username = Some(username);
        self.is_admin = is_admin;
    }

    /// Mark the session as authenticated (username should already be set).
    pub fn confirm_authentication(&mut self) {
        self.authenticated = true;
    }

    pub fn is_authenticated(&self) -> bool {
        self.authenticated
    }

    pub fn username(&self) -> Option<&str> {
        self.username.as_deref()
    }

    // Authentication permissions
    /// Check if authentication is allowed on this connection.
    /// Returns true if TLS or if insecure auth is explicitly allowed.
    pub fn can_authenticate(&self) -> bool {
        self.is_tls || self.allow_auth_insecure
    }

    // Posting permissions
    /// Check if the session can currently post articles.
    /// Requires either authentication or anonymous posting to be enabled.
    pub fn can_post(&self) -> bool {
        self.authenticated || self.allow_anonymous_posting
    }

    pub fn is_tls(&self) -> bool {
        self.is_tls
    }

    // Stream mode
    pub fn enter_stream_mode(&mut self) {
        self.in_stream_mode = true;
    }

    pub fn is_stream_mode(&self) -> bool {
        self.in_stream_mode
    }

    // Admin status
    /// Check if the authenticated user is an admin
    pub fn is_admin(&self) -> bool {
        self.is_admin
    }

    /// Set admin status (called after authentication verifies admin status)
    pub fn set_admin(&mut self, is_admin: bool) {
        self.is_admin = is_admin;
    }
}

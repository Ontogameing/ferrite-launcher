//! In-memory account acceptance and cancellable authentication attempts.

use super::{Account, complete_login, start_login};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, Sender, TryRecvError},
};

/// Session credentials and the current attempt. Neither is persisted or logged.
#[derive(Default)]
pub struct Session {
    account: Option<Account>,
    attempt: Option<LoginAttempt>,
}

struct LoginAttempt {
    events: Receiver<AuthEvent>,
    cancel: Arc<AtomicBool>,
}

impl Drop for LoginAttempt {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

enum AuthEvent {
    Progress(String),
    Device {
        user_code: String,
        verification_uri: String,
    },
    Account(Result<Account, String>),
}

/// Public progress only; credentials are accepted inside [`Session::poll`].
pub enum SessionUpdate {
    Progress(String),
    Device {
        user_code: String,
        verification_uri: String,
    },
    /// The attempt ended. Clear any previously displayed device instructions.
    Finished {
        status: String,
    },
}

/// Blocking protocol work, scheduled by the caller on its chosen worker thread.
pub struct LoginWorker {
    client_id: String,
    sender: Sender<AuthEvent>,
    cancel: Arc<AtomicBool>,
}

impl LoginWorker {
    /// Runs the existing device-code protocol and reports into its originating attempt.
    pub fn run(self) {
        let result = start_login(&self.client_id).and_then(|code| {
            if self.cancel.load(Ordering::Relaxed) {
                return Err("Sign-in cancelled.".into());
            }
            self.sender
                .send(AuthEvent::Device {
                    user_code: code.user_code.clone(),
                    verification_uri: code.verification_uri.clone(),
                })
                .map_err(|_| "Sign-in cancelled.".to_owned())?;
            complete_login(&self.client_id, code, &self.cancel, |message| {
                let _ = self.sender.send(AuthEvent::Progress(message.into()));
            })
        });
        if !self.cancel.load(Ordering::Relaxed) {
            let _ = self.sender.send(AuthEvent::Account(result));
        }
    }
}

impl Session {
    /// Creates a fresh attempt; returns `None` while an attempt is pending.
    pub fn begin_login(&mut self, client_id: &str) -> Result<Option<LoginWorker>, String> {
        if self.is_pending() {
            return Ok(None);
        }
        let client_id = client_id.trim().to_owned();
        if client_id.is_empty() {
            return Err("Enter your Microsoft public-client application ID first.".into());
        }
        let (sender, events) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        self.attempt = Some(LoginAttempt {
            events,
            cancel: Arc::clone(&cancel),
        });
        Ok(Some(LoginWorker {
            client_id,
            sender,
            cancel,
        }))
    }

    pub fn account(&self) -> Option<&Account> {
        self.account.as_ref()
    }

    pub fn is_pending(&self) -> bool {
        self.attempt.is_some()
    }

    /// Drops queued results and signals cancellation, retaining accepted credentials.
    pub fn cancel(&mut self) {
        self.attempt = None;
    }

    /// Invalidates the attempt and forgets credentials, including queued credentials.
    pub fn sign_out(&mut self) {
        self.cancel();
        self.account = None;
    }

    /// Accepts one queued event without blocking. Call until `None` to drain progress.
    pub fn poll(&mut self) -> Option<SessionUpdate> {
        let event = match self.attempt.as_ref()?.events.try_recv() {
            Ok(event) => event,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => {
                self.cancel();
                return Some(SessionUpdate::Finished {
                    status: "Sign-in worker stopped unexpectedly. Try again.".into(),
                });
            }
        };
        Some(match event {
            AuthEvent::Progress(message) => SessionUpdate::Progress(message),
            AuthEvent::Device {
                user_code,
                verification_uri,
            } => SessionUpdate::Device {
                user_code,
                verification_uri,
            },
            AuthEvent::Account(result) => {
                self.cancel();
                let status = match result {
                    Ok(account) if !account.is_expired() => {
                        let status = format!("Signed in as {} (this session only).", account.name);
                        self.account = Some(account);
                        status
                    }
                    Ok(_) => "Session expired. Please sign in again.".into(),
                    Err(error) => format!("Sign-in failed: {error}"),
                };
                SessionUpdate::Finished { status }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn account(expired: bool) -> Account {
        Account {
            name: "TestPlayer".into(),
            uuid: "0123456789abcdef0123456789abcdef".into(),
            access_token: "synthetic-token".into(),
            xuid: "123".into(),
            client_id: "test-client".into(),
            expires: if expired {
                Instant::now()
            } else {
                Instant::now() + Duration::from_secs(3600)
            },
        }
    }

    fn attempt(session: &mut Session) -> (mpsc::Sender<AuthEvent>, Arc<AtomicBool>) {
        let worker = session.begin_login("test-client").unwrap().unwrap();
        (worker.sender, worker.cancel)
    }

    #[test]
    fn begin_login_validates_and_preserves_pending_attempt() {
        let mut session = Session::default();
        assert!(session.begin_login("  ").is_err());
        assert!(!session.is_pending());
        let worker = session.begin_login("  test-client  ").unwrap().unwrap();
        assert_eq!(worker.client_id, "test-client");
        assert!(session.begin_login("").unwrap().is_none());
        assert!(!worker.cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn auth_events_are_polled_on_other_pages_and_errors_clear_device() {
        let mut session = Session::default();
        let (sender, cancel) = attempt(&mut session);
        sender
            .send(AuthEvent::Device {
                user_code: "TEST-CODE".into(),
                verification_uri: "https://microsoft.com/link".into(),
            })
            .unwrap();
        sender
            .send(AuthEvent::Progress("Waiting for Microsoft".into()))
            .unwrap();
        assert!(
            matches!(session.poll(), Some(SessionUpdate::Device { user_code, .. }) if user_code == "TEST-CODE")
        );
        assert!(
            matches!(session.poll(), Some(SessionUpdate::Progress(message)) if message == "Waiting for Microsoft")
        );
        sender
            .send(AuthEvent::Account(Err("Access denied".into())))
            .unwrap();
        assert!(
            matches!(session.poll(), Some(SessionUpdate::Finished { status }) if status == "Sign-in failed: Access denied")
        );
        assert!(!session.is_pending());
        assert!(cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn cancel_and_sign_out_discard_queued_and_late_results() {
        for sign_out in [false, true] {
            let mut session = Session::default();
            let (sender, cancel) = attempt(&mut session);
            sender
                .send(AuthEvent::Progress("Stale progress".into()))
                .unwrap();
            sender.send(AuthEvent::Account(Ok(account(false)))).unwrap();
            if sign_out {
                session.sign_out();
            } else {
                session.cancel();
            }
            assert!(cancel.load(Ordering::Relaxed));
            assert!(sender.send(AuthEvent::Account(Ok(account(false)))).is_err());
            let (_new_sender, _) = attempt(&mut session);
            assert!(session.poll().is_none());
            assert!(session.account().is_none());
        }
    }

    #[test]
    fn dropped_app_cancels_worker_and_disconnect_is_reported() {
        let mut session = Session::default();
        let (sender, cancel) = attempt(&mut session);
        drop(sender);
        assert!(
            matches!(session.poll(), Some(SessionUpdate::Finished { status }) if status.contains("stopped unexpectedly"))
        );
        assert!(cancel.load(Ordering::Relaxed));
        let (sender, cancel) = attempt(&mut session);
        drop(session);
        assert!(cancel.load(Ordering::Relaxed));
        assert!(sender.send(AuthEvent::Progress("Late".into())).is_err());
    }

    #[test]
    fn expired_and_post_completion_accounts_are_not_accepted() {
        let mut session = Session::default();
        let (sender, cancel) = attempt(&mut session);
        sender.send(AuthEvent::Account(Ok(account(true)))).unwrap();
        sender.send(AuthEvent::Account(Ok(account(false)))).unwrap();
        assert!(
            matches!(session.poll(), Some(SessionUpdate::Finished { status }) if status == "Session expired. Please sign in again.")
        );
        assert!(cancel.load(Ordering::Relaxed));
        assert!(session.poll().is_none());
        assert!(session.account().is_none());
        assert!(sender.send(AuthEvent::Account(Ok(account(false)))).is_err());
    }

    #[test]
    fn accepted_account_survives_cancel_and_failed_sign_in_until_sign_out() {
        let mut session = Session::default();
        let (sender, _) = attempt(&mut session);
        sender.send(AuthEvent::Account(Ok(account(false)))).unwrap();
        assert!(
            matches!(session.poll(), Some(SessionUpdate::Finished { status }) if status == "Signed in as TestPlayer (this session only).")
        );
        assert_eq!(session.account().unwrap().access_token, "synthetic-token");
        let (_sender, _) = attempt(&mut session);
        session.cancel();
        assert!(session.account().is_some());
        for result in [Err("Denied".into()), Ok(account(true))] {
            let (sender, _) = attempt(&mut session);
            sender.send(AuthEvent::Account(result)).unwrap();
            assert!(matches!(
                session.poll(),
                Some(SessionUpdate::Finished { .. })
            ));
            assert!(!session.account().unwrap().is_expired());
        }
        let (sender, _) = attempt(&mut session);
        sender.send(AuthEvent::Account(Ok(account(false)))).unwrap();
        session.sign_out();
        assert!(session.poll().is_none());
        assert!(session.account().is_none());
    }
}

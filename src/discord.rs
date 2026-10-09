//! Best-effort Discord Rich Presence over the local Discord IPC transport.
//!
//! [`DiscordPresence`] owns one connected IPC client. Constructing it connects and
//! publishes an initial activity; disabling presence clears that activity.
//! Dropping the value clears presence and closes the connection, which makes storing
//! it in an `Option` a convenient way to tie the IPC lifecycle to an enable setting.
//!
//! Discord integration is non-essential launcher functionality, so failures are
//! reported to standard error rather than propagated after construction. No Discord
//! user token is requested or stored; the numeric ID passed to the client identifies
//! the Discord application whose Rich Presence assets are referenced.

use discord_rich_presence::{DiscordIpc, DiscordIpcClient, activity};

/// An active Discord IPC connection owned by the launcher.
///
/// The value is not cheaply clonable: there should be one owner responsible for
/// updates and cleanup. Its [`Drop`] implementation performs best-effort teardown.
pub struct DiscordPresence {
    client: DiscordIpcClient,
}

impl DiscordPresence {
    /// Connects to a running Discord client and publishes the launcher's default activity.
    ///
    /// Returns `None` if the local IPC connection cannot be established, after writing
    /// a diagnostic to standard error. Failure to publish the initial activity is also
    /// logged, but the live connection is still returned for cleanup.
    pub fn new() -> Option<Self> {
        let mut client = DiscordIpcClient::new("1548778248069054584");

        if let Err(err) = client.connect() {
            eprintln!("Could not connect to Discord: {err}");
            return None;
        }

        let activity = activity::Activity::new()
            .state("FERRITE LAUNCHER OMG!!!")
            .details("OMG THIS GUY IS USING FERRITE LAUNCHER")
            .assets(
                activity::Assets::new()
                    .large_image("pixil-frame-0_4_")
                    .large_text("Ferrite Launcher"),
            );

        if let Err(err) = client.set_activity(activity) {
            eprintln!("Could not set Discord activity: {err}");
        }

        Some(Self { client })
    }

    /// Requests removal of the current activity without closing the IPC connection.
    ///
    /// This is best-effort and intentionally ignores errors. Dropping the value later
    /// repeats the clear before closing, which is safe and keeps cleanup centralized.
    pub fn clear(&mut self) {
        let _ = self.client.clear_activity();
    }
}

impl Drop for DiscordPresence {
    /// Clears stale presence before closing the owned local IPC connection.
    ///
    /// Destructors cannot report useful recovery errors here, so both operations are
    /// best-effort. Explicitly calling [`Self::clear`] is not required before drop.
    fn drop(&mut self) {
        let _ = self.client.clear_activity();
        let _ = self.client.close();
    }
}

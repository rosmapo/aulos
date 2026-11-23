//! Inhibit suspend / idle while music plays (Lollypop's `power-management`
//! setting, `inhibitor.py`).
//!
//! Linux first asks the Wayland compositor directly, through the
//! `idle-inhibit` protocol (what `swayidle` and the other idle managers of
//! wlroots compositors honour, and what the portal of those desktops does not
//! cover). Without Wayland, or without the protocol, it uses the XDG desktop
//! portal (`org.freedesktop.portal.Inhibit` through `ashpd`), which works
//! both sandboxed and natively. Not every desktop ships a portal backend for
//! it, so on failure it falls back to a systemd-logind `sleep:idle` "block"
//! inhibitor (a file descriptor held open for as long as the inhibition
//! should last). Other targets (the web build) have no mechanism, so
//! [`acquire`] returns `None`.
//!
//! The held inhibition is represented by an [`InhibitToken`]; dropping every
//! clone of it releases it, and [`InhibitToken::release`] additionally closes
//! a portal request explicitly.

use crate::player::PlaybackState;
use std::sync::Arc;

/// Human-readable reason shown by the desktop in its "inhibited by" lists.
pub const REASON: &str = "Playing music";

/// Whether an inhibition should be held right now: the user enabled it, and
/// audio is audibly playing through the local engine. (Paused/stopped
/// playback, or a remote MPD server playing elsewhere, must not keep this
/// machine awake.)
pub fn should_inhibit(enabled: bool, state: PlaybackState, local_backend: bool) -> bool {
    enabled && local_backend && state == PlaybackState::Playing
}

/// A held suspend/idle inhibition. Cheap to clone (shared handle).
#[derive(Clone)]
pub struct InhibitToken(Arc<backend::Inhibitor>);

impl std::fmt::Debug for InhibitToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("InhibitToken").field(&self.0.kind()).finish()
    }
}

impl InhibitToken {
    /// Release the inhibition now. Safe to call while other clones exist:
    /// a portal request is closed explicitly; any other lock is released
    /// once the last clone is dropped.
    pub async fn release(self) {
        self.0.release().await;
    }
}

/// Take a suspend+idle inhibition. `None` when no mechanism is available
/// (logged at `warn`; playback is unaffected).
pub async fn acquire(reason: &str) -> Option<InhibitToken> {
    backend::acquire(reason)
        .await
        .map(|inhibitor| InhibitToken(Arc::new(inhibitor)))
}

#[cfg(target_os = "linux")]
mod backend {
    use std::time::Duration;

    /// How long the portal gets to answer before falling back to logind.
    const PORTAL_TIMEOUT: Duration = Duration::from_secs(4);

    pub enum Inhibitor {
        /// The compositor's `idle-inhibit` protocol.
        Wayland(Box<super::wayland::WaylandInhibit>),
        Portal(Box<ashpd::desktop::Request<()>>),
        /// Held only for its `Drop` (closes the logind inhibitor fd).
        Logind(#[allow(dead_code)] zbus::zvariant::OwnedFd),
    }

    impl Inhibitor {
        pub fn kind(&self) -> &'static str {
            match self {
                Self::Wayland(_) => "wayland",
                Self::Portal(_) => "portal",
                Self::Logind(_) => "logind",
            }
        }

        pub async fn release(&self) {
            if let Self::Portal(request) = self
                && let Err(e) = request.close().await
            {
                tracing::debug!("closing portal inhibit request failed: {e}");
            }
        }
    }

    pub async fn acquire(reason: &str) -> Option<Inhibitor> {
        match crate::task::spawn_blocking(super::wayland::WaylandInhibit::acquire).await {
            Ok(Ok(inhibit)) => {
                tracing::debug!("inhibiting idle via the Wayland idle-inhibit protocol");
                return Some(Inhibitor::Wayland(Box::new(inhibit)));
            }
            Ok(Err(e)) => tracing::debug!("Wayland idle-inhibit unavailable ({e}); trying portal"),
            Err(e) => tracing::debug!("Wayland idle-inhibit task failed ({e}); trying portal"),
        }
        match crate::task::timeout(PORTAL_TIMEOUT, acquire_portal(reason)).await {
            Ok(Ok(request)) => {
                tracing::debug!("inhibiting suspend/idle via the desktop portal");
                return Some(Inhibitor::Portal(Box::new(request)));
            }
            Ok(Err(e)) => tracing::debug!("portal inhibit unavailable ({e}); trying logind"),
            Err(_) => tracing::debug!("portal inhibit timed out; trying logind"),
        }
        match acquire_logind(reason).await {
            Ok(fd) => {
                tracing::debug!("inhibiting suspend/idle via logind");
                Some(Inhibitor::Logind(fd))
            }
            Err(e) => {
                tracing::warn!("could not inhibit suspend/idle: {e}");
                None
            }
        }
    }

    async fn acquire_portal(reason: &str) -> Result<ashpd::desktop::Request<()>, ashpd::Error> {
        use ashpd::desktop::inhibit::{InhibitFlags, InhibitOptions, InhibitProxy};
        let proxy = InhibitProxy::new().await?;
        proxy
            .inhibit(
                None,
                InhibitFlags::Suspend | InhibitFlags::Idle,
                InhibitOptions::default().set_reason(reason),
            )
            .await
    }

    async fn acquire_logind(reason: &str) -> zbus::Result<zbus::zvariant::OwnedFd> {
        let conn = zbus::Connection::system().await?;
        let reply = conn
            .call_method(
                Some("org.freedesktop.login1"),
                "/org/freedesktop/login1",
                Some("org.freedesktop.login1.Manager"),
                "Inhibit",
                &("sleep:idle", "Aulos", reason, "block"),
            )
            .await?;
        reply.body().deserialize::<zbus::zvariant::OwnedFd>()
    }
}

/// An inhibitor held through the Wayland `idle-inhibit` protocol: a surface of
/// our own, with no role (never shown), that the compositor is told to treat
/// as inhibiting idle (the way `sway-audio-idle-inhibit` does it). Dropping
/// it destroys the inhibitor and releases the lock.
#[cfg(target_os = "linux")]
mod wayland {
    use std::sync::Mutex;
    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::wl_compositor::WlCompositor;
    use wayland_client::protocol::wl_registry::WlRegistry;
    use wayland_client::protocol::wl_surface::WlSurface;
    use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
    use wayland_protocols::wp::idle_inhibit::zv1::client::zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1;
    use wayland_protocols::wp::idle_inhibit::zv1::client::zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1;

    /// Dispatch state; the compositor sends none of the events we could
    /// care about.
    pub struct State;

    macro_rules! ignore_events {
        ($($proxy:ty => $data:ty),* $(,)?) => {$(
            impl Dispatch<$proxy, $data> for State {
                fn event(
                    _: &mut Self,
                    _: &$proxy,
                    _: <$proxy as Proxy>::Event,
                    _: &$data,
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                }
            }
        )*};
    }

    ignore_events! {
        WlRegistry => GlobalListContents,
        WlCompositor => (),
        WlSurface => (),
        ZwpIdleInhibitManagerV1 => (),
        ZwpIdleInhibitorV1 => (),
    }

    pub struct WaylandInhibit {
        connection: Connection,
        /// Keeps the objects' event queue alive.
        _queue: Mutex<EventQueue<State>>,
        inhibitor: ZwpIdleInhibitorV1,
        surface: WlSurface,
    }

    impl WaylandInhibit {
        /// Connects to the compositor named by the environment and takes
        /// the inhibitor. Blocking, but quick: one round trip.
        pub fn acquire() -> Result<Self, String> {
            let connection = Connection::connect_to_env().map_err(|e| e.to_string())?;
            let (globals, mut queue) =
                registry_queue_init::<State>(&connection).map_err(|e| e.to_string())?;
            let qh = queue.handle();
            let compositor: WlCompositor = globals
                .bind(&qh, 1..=6, ())
                .map_err(|e| format!("no wl_compositor: {e}"))?;
            let manager: ZwpIdleInhibitManagerV1 = globals
                .bind(&qh, 1..=1, ())
                .map_err(|e| format!("no idle-inhibit protocol: {e}"))?;
            let surface = compositor.create_surface(&qh, ());
            let inhibitor = manager.create_inhibitor(&surface, &qh, ());
            // Send the requests and wait until the compositor took them (a
            // protocol error would surface here).
            queue
                .roundtrip(&mut State)
                .map_err(|e| format!("compositor refused the inhibitor: {e}"))?;
            Ok(Self {
                connection,
                _queue: Mutex::new(queue),
                inhibitor,
                surface,
            })
        }
    }

    impl Drop for WaylandInhibit {
        fn drop(&mut self) {
            self.inhibitor.destroy();
            self.surface.destroy();
            let _ = self.connection.flush();
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod backend {
    /// No inhibition mechanism on this target: never constructed.
    pub enum Inhibitor {}

    impl Inhibitor {
        pub fn kind(&self) -> &'static str {
            match *self {}
        }

        pub async fn release(&self) {
            match *self {}
        }
    }

    pub async fn acquire(_reason: &str) -> Option<Inhibitor> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inhibits_only_while_audibly_playing_locally() {
        use PlaybackState::*;
        assert!(should_inhibit(true, Playing, true));
        assert!(!should_inhibit(true, Paused, true));
        assert!(!should_inhibit(true, Stopped, true));
        assert!(!should_inhibit(false, Playing, true), "setting off");
        assert!(!should_inhibit(true, Playing, false), "remote MPD playback");
    }
}

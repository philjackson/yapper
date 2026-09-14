//! Pausing whatever is playing while you dictate.
//!
//! Anything coming out of the speakers goes into the microphone and ends up in
//! the transcript, so music is paused for the duration and put back afterwards.
//!
//! Players are found over MPRIS on the session bus. GTK already has a D-Bus
//! connection, so this needs nothing that is not already linked.

use gtk::gio;
use gtk::glib::Variant;
use gtk::glib::VariantTy;
use gtk::prelude::*;

const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";
/// Short: a wedged player must not hold up the start of a recording.
const TIMEOUT_MS: i32 = 500;

/// The players we paused, so only those get started again. Resuming everything
/// would start things the user had deliberately paused themselves.
#[derive(Default)]
pub struct Paused {
    names: Vec<String>,
}

/// Pause every player that is currently playing.
pub fn pause_playing() -> Paused {
    let Some(bus) = session_bus() else {
        return Paused::default();
    };

    let names = players(&bus)
        .into_iter()
        .filter(|name| is_playing(&bus, name))
        .filter(|name| call(&bus, name, "Pause").is_some())
        .collect();

    Paused { names }
}

/// Start the players we paused earlier.
pub fn resume(paused: Paused) {
    if paused.names.is_empty() {
        return;
    }
    let Some(bus) = session_bus() else {
        return;
    };
    for name in &paused.names {
        call(&bus, name, "Play");
    }
}

fn session_bus() -> Option<gio::DBusConnection> {
    gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)
        .inspect_err(|err| eprintln!("yapper: no session bus, not pausing players: {err}"))
        .ok()
}

/// Every MPRIS name currently on the bus.
fn players(bus: &gio::DBusConnection) -> Vec<String> {
    let reply = bus
        .call_sync(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "ListNames",
            None,
            Some(VariantTy::new("(as)").unwrap()),
            gio::DBusCallFlags::NONE,
            TIMEOUT_MS,
            gio::Cancellable::NONE,
        )
        .ok();

    reply
        .and_then(|reply| reply.child_value(0).get::<Vec<String>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|name| name.starts_with(MPRIS_PREFIX))
        .collect()
}

fn is_playing(bus: &gio::DBusConnection, name: &str) -> bool {
    let reply = bus.call_sync(
        Some(name),
        MPRIS_PATH,
        "org.freedesktop.DBus.Properties",
        "Get",
        Some(&(PLAYER_INTERFACE, "PlaybackStatus").to_variant()),
        Some(VariantTy::new("(v)").unwrap()),
        gio::DBusCallFlags::NONE,
        TIMEOUT_MS,
        gio::Cancellable::NONE,
    );

    // A player that will not say is one we leave alone.
    reply
        .ok()
        .and_then(|reply| reply.child_value(0).as_variant())
        .and_then(|status| status.get::<String>())
        .is_some_and(|status| status == "Playing")
}

fn call(bus: &gio::DBusConnection, name: &str, method: &str) -> Option<Variant> {
    bus.call_sync(
        Some(name),
        MPRIS_PATH,
        PLAYER_INTERFACE,
        method,
        None,
        None,
        gio::DBusCallFlags::NONE,
        TIMEOUT_MS,
        gio::Cancellable::NONE,
    )
    .inspect_err(|err| eprintln!("yapper: {method} on {name} failed: {err}"))
    .ok()
}

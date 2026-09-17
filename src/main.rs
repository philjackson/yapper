//! yapper: push-to-talk speech-to-text for Wayland, transcribed on-device.

mod audio;
mod cli;
mod config;
mod history;
mod models;
mod output;
mod picker;
mod players;
mod replace;
mod preferences;
mod stage;
mod transcribe;
mod ui;

use adw::prelude::*;
use gtk::glib;
use gtk::glib::variant::ToVariant;

const APP_ID: &str = "dev.yapper.Yapper";
/// Quick capture gets its own id so a compositor rule can target it, and so it
/// doesn't fold into an already-running normal window.
const QUICK_APP_ID: &str = "dev.yapper.Yapper.Quick";

fn main() -> glib::ExitCode {
    let options = match cli::parse(std::env::args().skip(1)) {
        cli::Invocation::Run(options) => options,
        cli::Invocation::Help => {
            print!("{}", cli::USAGE);
            return glib::ExitCode::SUCCESS;
        }
        cli::Invocation::Version => {
            println!("yapper {}", env!("CARGO_PKG_VERSION"));
            return glib::ExitCode::SUCCESS;
        }
        cli::Invocation::Invalid(message) => {
            eprintln!("yapper: {message}\n\n{}", cli::USAGE);
            return glib::ExitCode::FAILURE;
        }
    };

    // Quick capture hands the clipboard off and exits, so it needs a clipboard
    // that outlives the process. GTK's own does not.
    if options.quick && options.copy_output && !output::has_wl_copy() {
        eprintln!(
            "yapper: --copy needs wl-clipboard installed — GTK's own clipboard \
             is dropped when the process exits"
        );
        return glib::ExitCode::FAILURE;
    }

    if options.stop_daemon {
        return stop_daemon();
    }

    let config = match config::Config::load() {
        Ok(config) => config,
        Err(err) => {
            eprintln!(
                "yapper: {err:#}\nfalling back to defaults; fix {} to change settings",
                config::config_path().display()
            );
            config::Config::default()
        }
    };

    let app = adw::Application::builder()
        .application_id(if options.quick { QUICK_APP_ID } else { APP_ID })
        .build();

    // So --stop-daemon has something to ask for over D-Bus.
    let quit = gtk::gio::SimpleAction::new("quit", None);
    quit.connect_activate({
        let app = app.clone();
        move |_, _| app.quit()
    });
    app.add_action(&quit);

    // And so a later --quick can say how it wants the transcript delivered.
    // Plain activation carries no arguments, which is why this is an action.
    let capture = gtk::gio::SimpleAction::new(
        "capture",
        Some(glib::VariantTy::new("(bb)").expect("a valid variant type")),
    );
    capture.connect_activate(|_, how| {
        let (typed, copied) = how
            .and_then(|how| how.get::<(bool, bool)>())
            .unwrap_or((false, false));
        ui::start_capture(typed, copied);
    });
    app.add_action(&capture);

    app.connect_activate(move |app| ui::build(app, config.clone(), options));

    // A quick capture whose process is already running asks it to do the work,
    // passing on how the transcript should be delivered. Without this the flag
    // would be quietly ignored by whichever invocation happened to start first.
    if options.quick {
        let how = (options.type_output, options.copy_output).to_variant();
        match hand_to_running(&app, "capture", Some(&how)) {
            Ok(true) => return glib::ExitCode::SUCCESS,
            Ok(false) => {}
            Err(code) => return code,
        }
    }

    // Arguments are parsed above, and GTK would otherwise try to open them as files.
    app.run_with_args::<&str>(&[])
}

/// Ask the resident quick capture process to exit, without putting anything on
/// screen.
///
/// Registering against the same application id says whether one is running:
/// if it is, we are the remote end and can activate its quit action; if not,
/// we briefly become the primary instance ourselves and there was nothing to
/// stop.
fn stop_daemon() -> glib::ExitCode {
    let app = gtk::gio::Application::new(Some(QUICK_APP_ID), gtk::gio::ApplicationFlags::empty());
    match hand_to_running(&app, "quit", None) {
        Ok(true) => println!("yapper: quick capture stopped"),
        Ok(false) => println!("yapper: no quick capture process running"),
        Err(code) => return code,
    }
    glib::ExitCode::SUCCESS
}

/// Register on the session bus and, if another instance already owns the id,
/// hand it `action`. Returns whether there was one to hand it to; the error
/// is the exit code to leave with when the bus cannot be reached at all.
fn hand_to_running(
    app: &impl IsA<gtk::gio::Application>,
    action: &str,
    parameter: Option<&glib::Variant>,
) -> Result<bool, glib::ExitCode> {
    if let Err(err) = app.register(gtk::gio::Cancellable::NONE) {
        eprintln!("yapper: cannot reach the session bus: {err}");
        return Err(glib::ExitCode::FAILURE);
    }
    if !app.is_remote() {
        return Ok(false);
    }
    let app: &gtk::gio::Application = app.as_ref();
    app.activate_action(action, parameter);
    // The message is queued on the bus; leaving now could drop it.
    if let Some(bus) = app.dbus_connection() {
        let _ = bus.flush_sync(gtk::gio::Cancellable::NONE);
    }
    Ok(true)
}

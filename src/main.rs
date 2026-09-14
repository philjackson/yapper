//! yapper: push-to-talk speech-to-text for Wayland, transcribed on-device.

mod audio;
mod cli;
mod config;
mod history;
mod output;
mod players;
mod preferences;
mod stage;
mod transcribe;
mod ui;

use adw::prelude::*;
use gtk::glib;

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
    if options.quick && !output::has_wl_copy() {
        eprintln!(
            "yapper: quick capture needs wl-clipboard installed — GTK's own \
             clipboard is dropped when the process exits"
        );
        return glib::ExitCode::FAILURE;
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
    app.connect_activate(move |app| ui::build(app, config.clone(), options));
    // Arguments are parsed above, and GTK would otherwise try to open them as files.
    app.run_with_args::<&str>(&[])
}

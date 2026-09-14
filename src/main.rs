//! yapper: push-to-talk speech-to-text for Wayland, transcribed on-device.

mod audio;
mod config;
mod output;
mod stage;
mod transcribe;
mod ui;

use adw::prelude::*;
use gtk::glib;

const APP_ID: &str = "dev.yapper.Yapper";

fn main() -> glib::ExitCode {
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

    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(move |app| ui::build(app, config.clone()));
    // Nothing on the command line yet, and GTK would otherwise try to open files.
    app.run_with_args::<&str>(&[])
}

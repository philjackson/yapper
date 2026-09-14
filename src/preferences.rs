//! The preferences dialog: everything in `config.toml`, with a widget each.
//!
//! Changes apply and save as you make them, which is the GNOME convention and
//! saves having an OK button that can be forgotten. Everything takes effect on
//! the next transcription except the model, which is loaded once at startup.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use crate::config::{Config, models_dir};

/// Where the ggml models live. The same place `scripts/fetch-model.sh` pulls
/// from, so the two never disagree.
pub const MODELS_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/tree/main";

/// Called after every change, with the updated config.
pub type OnChange = Rc<dyn Fn(&Config)>;

pub fn present(parent: &impl IsA<gtk::Widget>, config: &Config, on_change: OnChange) {
    let config = Rc::new(RefCell::new(config.clone()));

    let dialog = adw::PreferencesDialog::builder()
        .title("Preferences")
        .content_width(520)
        .build();
    let page = adw::PreferencesPage::new();

    page.add(&transcription_group(parent, &config, &on_change));
    page.add(&output_group(&config, &on_change));
    page.add(&window_group(&config, &on_change));

    dialog.add(&page);
    dialog.present(Some(parent));
}

fn transcription_group(
    parent: &impl IsA<gtk::Widget>,
    config: &Rc<RefCell<Config>>,
    on_change: &OnChange,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Transcription")
        .description("The model is loaded once, so a new one takes effect next time yapper starts")
        .build();

    // --- model -------------------------------------------------------------
    let model_row = adw::ActionRow::builder()
        .title("Model")
        .subtitle(config.borrow().model_path.display().to_string())
        .subtitle_lines(2)
        .build();
    let choose = gtk::Button::builder()
        .label("Choose\u{2026}")
        .valign(gtk::Align::Center)
        .build();
    choose.connect_clicked({
        let parent = parent.as_ref().clone();
        let config = Rc::clone(config);
        let on_change = Rc::clone(on_change);
        let model_row = model_row.clone();
        move |_| {
            let filter = gtk::FileFilter::new();
            filter.set_name(Some("Whisper models"));
            filter.add_pattern("*.bin");

            let dialog = gtk::FileDialog::builder()
                .title("Choose a Whisper model")
                .default_filter(&filter)
                .initial_folder(&gtk::gio::File::for_path(models_dir()))
                .build();

            let window = parent.root().and_downcast::<gtk::Window>();
            let config = Rc::clone(&config);
            let on_change = Rc::clone(&on_change);
            let model_row = model_row.clone();
            dialog.open(window.as_ref(), gtk::gio::Cancellable::NONE, move |result| {
                // Cancelling is a normal outcome, not an error worth reporting.
                let Some(path) = result.ok().and_then(|file| file.path()) else {
                    return;
                };
                model_row.set_subtitle(&path.display().to_string());
                config.borrow_mut().model_path = path;
                save(&config, &on_change);
            });
        }
    });
    model_row.add_suffix(&choose);
    model_row.set_activatable_widget(Some(&choose));
    group.add(&model_row);

    // --- where to get one --------------------------------------------------
    let get_models = adw::ActionRow::builder()
        .title("Get more models")
        .subtitle("tiny 75 MB · base 148 MB · small 488 MB · medium 1.5 GB · large 3.1 GB")
        .subtitle_lines(2)
        .build();
    let browse = gtk::Button::builder()
        .icon_name("web-browser-symbolic")
        .tooltip_text(MODELS_URL)
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    browse.connect_clicked({
        let parent = parent.as_ref().clone();
        move |_| {
            let window = parent.root().and_downcast::<gtk::Window>();
            gtk::UriLauncher::new(MODELS_URL).launch(
                window.as_ref(),
                gtk::gio::Cancellable::NONE,
                |result| {
                    if let Err(err) = result {
                        eprintln!("yapper: could not open {MODELS_URL}: {err}");
                    }
                },
            );
        }
    });
    get_models.add_suffix(&browse);
    get_models.set_activatable_widget(Some(&browse));
    group.add(&get_models);

    // --- language ----------------------------------------------------------
    let language = adw::EntryRow::builder()
        .title("Language")
        .text(&config.borrow().language)
        .build();
    language.set_tooltip_text(Some(
        "An ISO code such as en or de, or auto to detect it from the speech",
    ));
    language.connect_changed({
        let config = Rc::clone(config);
        let on_change = Rc::clone(on_change);
        move |row| {
            let text = row.text().trim().to_string();
            // An empty box means "auto" rather than a broken setting.
            config.borrow_mut().language = if text.is_empty() {
                "auto".to_string()
            } else {
                text
            };
            save(&config, &on_change);
        }
    });
    group.add(&language);

    // --- translate ---------------------------------------------------------
    let translate = adw::SwitchRow::builder()
        .title("Translate to English")
        .subtitle("Transcribe speech in other languages as English")
        .active(config.borrow().translate)
        .build();
    translate.connect_active_notify({
        let config = Rc::clone(config);
        let on_change = Rc::clone(on_change);
        move |row| {
            config.borrow_mut().translate = row.is_active();
            save(&config, &on_change);
        }
    });
    group.add(&translate);

    // --- threads -----------------------------------------------------------
    let threads = adw::SpinRow::builder()
        .title("Threads")
        .subtitle("0 picks a sensible number from the CPU count")
        .adjustment(&gtk::Adjustment::new(
            config.borrow().threads as f64,
            0.0,
            64.0,
            1.0,
            4.0,
            0.0,
        ))
        .build();
    threads.connect_value_notify({
        let config = Rc::clone(config);
        let on_change = Rc::clone(on_change);
        move |row| {
            config.borrow_mut().threads = row.value() as u32;
            save(&config, &on_change);
        }
    });
    group.add(&threads);

    group
}

fn output_group(config: &Rc<RefCell<Config>>, on_change: &OnChange) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("When a transcript arrives")
        .build();

    let copy = adw::SwitchRow::builder()
        .title("Copy to the clipboard")
        .active(config.borrow().copy_to_clipboard)
        .build();
    copy.connect_active_notify({
        let config = Rc::clone(config);
        let on_change = Rc::clone(on_change);
        move |row| {
            config.borrow_mut().copy_to_clipboard = row.is_active();
            save(&config, &on_change);
        }
    });
    group.add(&copy);

    let typing = crate::output::typing_backend();
    let type_row = adw::SwitchRow::builder()
        .title("Type into the focused window")
        .subtitle(match typing {
            Some(tool) => format!("Using {tool}"),
            None => "Needs wtype or ydotool installed".to_string(),
        })
        .active(config.borrow().type_on_finish && typing.is_some())
        .sensitive(typing.is_some())
        .build();
    type_row.connect_active_notify({
        let config = Rc::clone(config);
        let on_change = Rc::clone(on_change);
        move |row| {
            config.borrow_mut().type_on_finish = row.is_active();
            save(&config, &on_change);
        }
    });
    group.add(&type_row);

    group
}

fn window_group(config: &Rc<RefCell<Config>>, on_change: &OnChange) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Window").build();

    let preview = adw::SwitchRow::builder()
        .title("Live transcript")
        .subtitle("Show words under the button while you talk, at the cost of re-transcribing twice a second")
        .active(config.borrow().live_preview)
        .build();
    preview.connect_active_notify({
        let config = Rc::clone(config);
        let on_change = Rc::clone(on_change);
        move |row| {
            config.borrow_mut().live_preview = row.is_active();
            save(&config, &on_change);
        }
    });
    group.add(&preview);

    let history = adw::SpinRow::builder()
        .title("Recordings to keep")
        .subtitle("Older transcripts fall off the end of the list")
        .adjustment(&gtk::Adjustment::new(
            config.borrow().history_limit as f64,
            1.0,
            10_000.0,
            10.0,
            100.0,
            0.0,
        ))
        .build();
    history.connect_value_notify({
        let config = Rc::clone(config);
        let on_change = Rc::clone(on_change);
        move |row| {
            config.borrow_mut().history_limit = row.value() as usize;
            save(&config, &on_change);
        }
    });
    group.add(&history);

    group
}

/// Write the file and tell the window. A failed write is worth saying out loud
/// but not worth losing the change the user just made in the running session.
fn save(config: &Rc<RefCell<Config>>, on_change: &OnChange) {
    let config = config.borrow();
    if let Err(err) = config.save() {
        eprintln!("yapper: could not save preferences: {err:#}");
    }
    on_change(&config);
}

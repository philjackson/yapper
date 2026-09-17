//! The preferences dialog: everything in `config.toml`, with a widget each.
//!
//! Changes apply and save as you make them, which is the GNOME convention and
//! saves having an OK button that can be forgotten. Everything takes effect on
//! the next transcription except the model, which is loaded once at startup.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;

use crate::config::{Config, MODELS_URL, models_dir};

/// Called after every change, with the updated config.
pub type OnChange = Rc<dyn Fn(&Config)>;

/// The dialog's copy of the config and the hook that tells the window about
/// edits. Every row changes a setting the same way: mutate, save, notify.
struct Prefs {
    config: RefCell<Config>,
    on_change: OnChange,
}

impl Prefs {
    fn get<T>(&self, read: impl FnOnce(&Config) -> T) -> T {
        read(&self.config.borrow())
    }

    /// Apply an edit, write the file and tell the window. A failed write is
    /// worth saying out loud but not worth losing the change the user just
    /// made in the running session.
    fn update(&self, edit: impl FnOnce(&mut Config)) {
        let mut config = self.config.borrow_mut();
        edit(&mut config);
        if let Err(err) = config.save() {
            eprintln!("yapper: could not save preferences: {err:#}");
        }
        (self.on_change)(&config);
    }
}

pub fn present(parent: &impl IsA<gtk::Widget>, config: &Config, on_change: OnChange) {
    let prefs = Rc::new(Prefs {
        config: RefCell::new(config.clone()),
        on_change,
    });

    let dialog = adw::PreferencesDialog::builder()
        .title("Preferences")
        .content_width(640)
        .build();
    let page = adw::PreferencesPage::new();

    page.add(&transcription_group(parent, &prefs));
    page.add(&vocabulary_group(&prefs));
    page.add(&output_group(&prefs));
    page.add(&window_group(&prefs));

    dialog.add(&page);
    dialog.present(Some(parent));
}

/// Open the page the models come from, in the browser.
pub fn open_models_page(parent: Option<&gtk::Window>) {
    gtk::UriLauncher::new(MODELS_URL).launch(parent, gtk::gio::Cancellable::NONE, |result| {
        if let Err(err) = result {
            eprintln!("yapper: could not open {MODELS_URL}: {err}");
        }
    });
}

fn transcription_group(parent: &impl IsA<gtk::Widget>, prefs: &Rc<Prefs>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Transcription")
        .description("The model is loaded once, so a new one takes effect next time yapper starts")
        .build();
    let window = parent.root().and_downcast::<gtk::Window>();

    // --- model -------------------------------------------------------------
    let model_row = adw::ActionRow::builder()
        .title("Model")
        .subtitle(prefs.get(|c| c.model_path.display().to_string()))
        .subtitle_lines(2)
        .build();
    let choose = gtk::Button::builder()
        .label("Choose\u{2026}")
        .valign(gtk::Align::Center)
        .build();
    choose.connect_clicked({
        let window = window.clone();
        let prefs = Rc::clone(prefs);
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

            let prefs = Rc::clone(&prefs);
            let model_row = model_row.clone();
            dialog.open(window.as_ref(), gtk::gio::Cancellable::NONE, move |result| {
                // Cancelling is a normal outcome, not an error worth reporting.
                let Some(path) = result.ok().and_then(|file| file.path()) else {
                    return;
                };
                model_row.set_subtitle(&path.display().to_string());
                prefs.update(|c| c.model_path = path);
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
    browse.connect_clicked(move |_| open_models_page(window.as_ref()));
    get_models.add_suffix(&browse);
    get_models.set_activatable_widget(Some(&browse));
    group.add(&get_models);

    // --- language ----------------------------------------------------------
    let language = entry_row(
        prefs,
        "Language",
        "An ISO code such as en or de, or auto to detect it from the speech",
        |c| c.language.clone(),
        |c, text| {
            let text = text.trim();
            // An empty box means "auto" rather than a broken setting.
            c.language = if text.is_empty() { "auto" } else { text }.to_string();
        },
    );
    group.add(&language);

    group.add(&switch_row(
        prefs,
        "Translate to English",
        Some("Transcribe speech in other languages as English"),
        |c| c.translate,
        |c, on| c.translate = on,
    ));

    group.add(&spin_row(
        prefs,
        "Threads",
        "0 picks a sensible number from the CPU count",
        SpinRange {
            lower: 0.0,
            upper: 64.0,
            step: 1.0,
            page: 4.0,
            digits: 0,
        },
        |c| c.threads as f64,
        |c, value| c.threads = value as u32,
    ));

    group
}

/// The whole point of this group is the description: an empty box with the
/// word "Vocabulary" over it tells nobody what to type in it.
fn vocabulary_group(prefs: &Rc<Prefs>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Vocabulary")
        .description(
            "Names and jargon the model keeps getting wrong. It reads these \
             before your speech and leans towards them, so write them as you \
             would say them, separated by commas.\n\n\
             For example: Siobhan, Niamh, Loughborough, Sainsbury's\n\n\
             Keep it to a line or two. A long list crowds out the audio and \
             the model starts hearing your vocabulary instead of you.",
        )
        .build();

    group.add(&entry_row(
        prefs,
        "Words to expect",
        "Passed to Whisper as context for every transcription. Leave empty for none.",
        |c| c.initial_prompt.clone(),
        |c, text| c.initial_prompt = text.to_string(),
    ));

    group
}

/// A live level meter with the silence threshold drawn across it.
///
/// A threshold is impossible to set as a bare number — 0.004 means nothing
/// until you can see where your own voice falls against it. Speak, watch the
/// bar, and put the line under it.
fn add_microphone_test(group: &adw::PreferencesGroup, prefs: &Rc<Prefs>) {
    let level = Rc::new(Cell::new(0.0f32));
    let recorder: Rc<RefCell<Option<crate::audio::Recorder>>> = Rc::new(RefCell::new(None));

    let meter = gtk::DrawingArea::builder()
        .content_height(22)
        .hexpand(true)
        .margin_top(14)
        .margin_bottom(14)
        .margin_start(16)
        .margin_end(16)
        .build();

    meter.set_draw_func({
        let prefs = Rc::clone(prefs);
        let level = Rc::clone(&level);
        move |_, cr, width, height| {
            let (width, height) = (width as f64, height as f64);
            let threshold = prefs.get(|c| c.silence_threshold);

            // Track.
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.08);
            crate::stage::pill(cr, 0.0, 0.0, width, height);
            let _ = cr.fill();

            // Everything else is clipped to the track, so neither the level nor
            // the threshold mark can spill past its rounded ends.
            let _ = cr.save();
            crate::stage::pill(cr, 0.0, 0.0, width, height);
            cr.clip();

            // Square-rooted, because speech and room tone are orders of
            // magnitude apart and a linear bar would pin one end or the other.
            let filled = scale(level.get()) * width;
            if filled > 0.5 {
                if level.get() >= threshold {
                    cr.set_source_rgba(0.18, 0.76, 0.49, 0.95);
                } else {
                    cr.set_source_rgba(1.0, 1.0, 1.0, 0.30);
                }
                cr.rectangle(0.0, 0.0, filled, height);
                let _ = cr.fill();
            }

            let mark = scale(threshold) * width;
            cr.set_source_rgba(0.93, 0.28, 0.31, 0.95);
            cr.set_line_width(2.0);
            cr.move_to(mark, 0.0);
            cr.line_to(mark, height);
            let _ = cr.stroke();

            let _ = cr.restore();
        }
    });

    // The meter is meaningless without saying which mark is which.
    let caption = gtk::Label::builder()
        .label("The bar is what the microphone hears. The red line is the threshold — it turns green above it, which is what counts as speech.")
        .css_classes(["caption", "dim-label"])
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .justify(gtk::Justification::Center)
        .margin_bottom(14)
        .margin_start(16)
        .margin_end(16)
        .build();

    let test = gtk::ToggleButton::builder()
        .label("Test")
        .valign(gtk::Align::Center)
        .build();

    let row = adw::ActionRow::builder()
        .title("Test the microphone")
        .subtitle("Speak, and put the line just under where your voice sits")
        .build();
    row.add_suffix(&test);
    row.set_activatable_widget(Some(&test));

    test.connect_toggled({
        let prefs = Rc::clone(prefs);
        let recorder = Rc::clone(&recorder);
        let level = Rc::clone(&level);
        let meter = meter.clone();
        move |button| {
            if !button.is_active() {
                recorder.borrow_mut().take();
                level.set(0.0);
                meter.queue_draw();
                return;
            }

            let (device, threshold) = prefs.get(|c| (c.input_device.clone(), c.silence_threshold));
            match crate::audio::Recorder::monitor(&device, threshold) {
                Ok(open) => *recorder.borrow_mut() = Some(open),
                Err(err) => {
                    eprintln!("yapper: cannot open the microphone: {err:#}");
                    button.set_active(false);
                    return;
                }
            }

            // Feed the meter for as long as the test runs, then stop.
            gtk::glib::timeout_add_local(std::time::Duration::from_millis(50), {
                let recorder = Rc::clone(&recorder);
                let level = Rc::clone(&level);
                let meter = meter.downgrade();
                move || {
                    let (Some(meter), Some(rms)) = (
                        meter.upgrade(),
                        recorder.borrow().as_ref().map(|open| open.current_rms()),
                    ) else {
                        return gtk::glib::ControlFlow::Break;
                    };
                    level.set(rms);
                    meter.queue_draw();
                    gtk::glib::ControlFlow::Continue
                }
            });
        }
    });

    let threshold = spin_row(
        prefs,
        "Silence threshold",
        "Below this counts as silence",
        SpinRange {
            lower: 0.001,
            upper: 0.100,
            step: 0.001,
            page: 0.010,
            digits: 3,
        },
        |c| c.silence_threshold as f64,
        |c, value| c.silence_threshold = value as f32,
    );
    threshold.connect_value_notify({
        let meter = meter.clone();
        move |_| meter.queue_draw()
    });

    // The meter and its caption go inside a row of their own. Adding a bare
    // widget to a preferences group drops it below the boxed list rather than
    // in it, which is why the gauge appeared to be floating outside the frame.
    let gauge = gtk::Box::new(gtk::Orientation::Vertical, 0);
    gauge.append(&meter);
    gauge.append(&caption);

    let gauge_row = adw::PreferencesRow::builder()
        .activatable(false)
        .selectable(false)
        .child(&gauge)
        .build();

    group.add(&row);
    group.add(&gauge_row);
    group.add(&threshold);
}

/// Compresses the range so room tone and speech are both visible.
fn scale(rms: f32) -> f64 {
    ((rms as f64) / 0.25).sqrt().clamp(0.0, 1.0)
}

/// The microphone picker. "System default" comes first and is what most people
/// want; the rest are the actual capture devices, not ALSA's plugin zoo.
fn microphone_row(prefs: &Rc<Prefs>) -> adw::ComboRow {
    let devices = crate::audio::input_devices();
    let chosen = prefs.get(|c| c.input_device.clone());

    let mut ids: Vec<String> = vec![String::new()];
    let names = gtk::StringList::new(&["System default"]);
    for device in &devices {
        ids.push(device.id.clone());
        names.append(&device.name);
    }

    // A microphone that has been unplugged since it was chosen still shows,
    // so the setting does not silently look like it was never made.
    if !chosen.is_empty() && !ids.contains(&chosen) {
        ids.push(chosen.clone());
        names.append(&format!("{chosen} (not connected)"));
    }

    let selected = ids.iter().position(|id| *id == chosen).unwrap_or(0);
    let row = adw::ComboRow::builder()
        .title("Microphone")
        .subtitle("Which input to record from")
        .model(&names)
        .selected(selected as u32)
        .build();

    row.connect_selected_notify({
        let prefs = Rc::clone(prefs);
        move |row| {
            if let Some(id) = ids.get(row.selected() as usize) {
                prefs.update(|c| c.input_device = id.clone());
            }
        }
    });
    row
}

fn output_group(prefs: &Rc<Prefs>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("When a transcript arrives")
        .build();

    group.add(&switch_row(
        prefs,
        "Copy to the clipboard",
        None,
        |c| c.copy_to_clipboard,
        |c, on| c.copy_to_clipboard = on,
    ));

    let can_type = crate::output::can_type();
    let type_row = switch_row(
        prefs,
        "Type into the focused window",
        Some(if can_type {
            "Using wtype"
        } else {
            "Needs wtype installed"
        }),
        |c| c.type_on_finish && crate::output::can_type(),
        |c, on| c.type_on_finish = on,
    );
    type_row.set_sensitive(can_type);
    group.add(&type_row);

    group
}

fn window_group(prefs: &Rc<Prefs>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Recording").build();

    group.add(&switch_row(
        prefs,
        "Live transcript",
        Some("Show words under the button while you talk, at the cost of re-transcribing twice a second"),
        |c| c.live_preview,
        |c, on| c.live_preview = on,
    ));

    group.add(&microphone_row(prefs));
    add_microphone_test(&group, prefs);

    group.add(&switch_row(
        prefs,
        "Pause media while recording",
        Some("Anything out of the speakers ends up in the transcript"),
        |c| c.pause_players,
        |c, on| c.pause_players = on,
    ));

    group.add(&spin_row(
        prefs,
        "Stop after silence",
        "Seconds of quiet that end a recording. 0 waits for you to stop it",
        SpinRange {
            lower: 0.0,
            upper: 30.0,
            step: 0.5,
            page: 1.0,
            digits: 1,
        },
        |c| c.silence_timeout as f64,
        |c, value| c.silence_timeout = value as f32,
    ));

    group.add(&spin_row(
        prefs,
        "Recordings to keep",
        "Older transcripts fall off the end of the list",
        SpinRange {
            lower: 1.0,
            upper: 10_000.0,
            step: 10.0,
            page: 100.0,
            digits: 0,
        },
        |c| c.history_limit as f64,
        |c, value| c.history_limit = value as usize,
    ));

    group
}

// --- one row per setting -----------------------------------------------------
//
// Each helper reads its initial value from the config and writes every change
// straight back through `Prefs::update`, so a new setting is one call here
// rather than a hand-written closure.

fn switch_row(
    prefs: &Rc<Prefs>,
    title: &str,
    subtitle: Option<&str>,
    read: fn(&Config) -> bool,
    write: fn(&mut Config, bool),
) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder()
        .title(title)
        .active(prefs.get(read))
        .build();
    if let Some(subtitle) = subtitle {
        row.set_subtitle(subtitle);
    }
    row.connect_active_notify({
        let prefs = Rc::clone(prefs);
        move |row| prefs.update(|c| write(c, row.is_active()))
    });
    row
}

struct SpinRange {
    lower: f64,
    upper: f64,
    step: f64,
    page: f64,
    digits: u32,
}

fn spin_row(
    prefs: &Rc<Prefs>,
    title: &str,
    subtitle: &str,
    range: SpinRange,
    read: fn(&Config) -> f64,
    write: fn(&mut Config, f64),
) -> adw::SpinRow {
    let row = adw::SpinRow::builder()
        .title(title)
        .subtitle(subtitle)
        .adjustment(&gtk::Adjustment::new(
            prefs.get(read),
            range.lower,
            range.upper,
            range.step,
            range.page,
            0.0,
        ))
        .digits(range.digits)
        .build();
    row.connect_value_notify({
        let prefs = Rc::clone(prefs);
        move |row| prefs.update(|c| write(c, row.value()))
    });
    row
}

fn entry_row(
    prefs: &Rc<Prefs>,
    title: &str,
    tooltip: &str,
    read: fn(&Config) -> String,
    write: fn(&mut Config, &str),
) -> adw::EntryRow {
    let row = adw::EntryRow::builder()
        .title(title)
        .text(prefs.get(read))
        .tooltip_text(tooltip)
        .build();
    row.connect_changed({
        let prefs = Rc::clone(prefs);
        move |row| prefs.update(|c| write(c, &row.text()))
    });
    row
}

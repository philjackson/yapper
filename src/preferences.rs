//! The preferences dialog: everything in `config.toml`, with a widget each.
//!
//! Changes apply and save as you make them, which is the GNOME convention and
//! saves having an OK button that can be forgotten. Everything takes effect on
//! the next transcription, the model included — picking one in the [picker]
//! loads it there and then.
//!
//! [picker]: crate::picker

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;

use crate::config::Config;
use crate::models::Engine;
use crate::picker;

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

    let (transcription, rows) = transcription_group(&prefs);

    // What a model can be told varies by engine, so the rows follow whichever
    // one is chosen rather than offering settings it would ignore.
    let reflect: Rc<dyn Fn(&Config)> = Rc::new({
        let rows = rows.clone();
        move |config| rows.reflect(config)
    });
    reflect(&prefs.config.borrow());

    rows.choose.connect_clicked({
        let parent = parent.as_ref().clone();
        let prefs = Rc::clone(&prefs);
        let reflect = Rc::clone(&reflect);
        move |_| {
            let prefs = Rc::clone(&prefs);
            let reflect = Rc::clone(&reflect);
            let config = prefs.config.borrow().clone();
            picker::present(
                &parent,
                &config,
                Rc::new(move |change| {
                    prefs.update(|config| match change {
                        picker::Change::Model(model) => config.model = model.id.to_string(),
                        picker::Change::Language {
                            model,
                            hears,
                            writes,
                        } => config.set_language(model, hears, writes),
                    });
                    reflect(&prefs.config.borrow());
                }),
            );
        }
    });

    page.add(&transcription);
    page.add(&output_group(&prefs));
    page.add(&window_group(&prefs));

    dialog.add(&page);
    dialog.present(Some(parent));
}

/// The rows whose meaning depends on the model in use. Widget handles, so
/// cloning one into a closure is free.
#[derive(Clone)]
struct Rows {
    model: adw::ActionRow,
    choose: gtk::Button,
}

impl Rows {
    /// Say which model is in use and what it has been told to listen for. The
    /// language lives with the model, in the picker, because which codes mean
    /// anything is the model's own business.
    fn reflect(&self, config: &Config) {
        self.model.set_subtitle(&match config.model() {
            None => format!("{} — not a model yapper knows about", config.model),
            Some(model) if !crate::models::is_installed(model) => {
                format!("{} — not downloaded yet", model.name)
            }
            Some(model) => format!("{} · {}", model.name, spoken(config, model)),
        });
    }
}

/// What a model is listening for, in a few words: the language, or the pair
/// when it is writing a different one, or how it decides for itself.
fn spoken(config: &Config, model: &crate::models::Model) -> String {
    if !model.engine.takes_language() {
        return match model.engine {
            Engine::Moonshine => "English".to_string(),
            _ => "detects the language itself".to_string(),
        };
    }
    let (hears, writes) = config.language(model);
    let heard = model.engine.language_name(hears);
    if hears == writes {
        if hears == "auto" {
            "detects the language itself".to_string()
        } else {
            heard.to_string()
        }
    } else {
        format!("{heard} → {}", model.engine.language_name(writes))
    }
}

fn transcription_group(prefs: &Rc<Prefs>) -> (adw::PreferencesGroup, Rows) {
    let group = adw::PreferencesGroup::builder()
        .title("Transcription")
        .description("Choosing a model downloads it if it isn't here, and loads it straight away")
        .build();

    // --- model -------------------------------------------------------------
    // One row, and everything about models happens behind it: the catalogue,
    // the downloads, and whatever is already on the disk.
    let model = adw::ActionRow::builder()
        .title("Model")
        .subtitle_lines(2)
        .build();
    let choose = gtk::Button::builder()
        .label("Choose\u{2026}")
        .valign(gtk::Align::Center)
        .build();
    model.add_suffix(&choose);
    model.set_activatable_widget(Some(&choose));
    group.add(&model);

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

    (group, Rows { model, choose })
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

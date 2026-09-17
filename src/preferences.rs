//! The preferences dialog: everything in `config.toml`, with a widget each.
//!
//! Changes apply and save as you make them, which is the GNOME convention and
//! saves having an OK button that can be forgotten. Everything takes effect on
//! the next transcription except the model, which is loaded once at startup.

use std::cell::{Cell, RefCell};
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
        .content_width(640)
        .build();
    let page = adw::PreferencesPage::new();

    page.add(&transcription_group(parent, &config, &on_change));
    page.add(&vocabulary_group(&config, &on_change));
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

/// The whole point of this group is the description: an empty box with the
/// word "Vocabulary" over it tells nobody what to type in it.
fn vocabulary_group(config: &Rc<RefCell<Config>>, on_change: &OnChange) -> adw::PreferencesGroup {
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

    let prompt = adw::EntryRow::builder()
        .title("Words to expect")
        .text(&config.borrow().initial_prompt)
        .build();
    prompt.set_tooltip_text(Some(
        "Passed to Whisper as context for every transcription. Leave empty for none.",
    ));
    prompt.connect_changed({
        let config = Rc::clone(config);
        let on_change = Rc::clone(on_change);
        move |row| {
            config.borrow_mut().initial_prompt = row.text().to_string();
            save(&config, &on_change);
        }
    });
    group.add(&prompt);

    group
}

/// A live level meter with the silence threshold drawn across it.
///
/// A threshold is impossible to set as a bare number — 0.004 means nothing
/// until you can see where your own voice falls against it. Speak, watch the
/// bar, and put the line under it.
fn add_microphone_test(group: &adw::PreferencesGroup, config: &Rc<RefCell<Config>>, on_change: &OnChange) {
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
        let config = Rc::clone(config);
        let level = Rc::clone(&level);
        move |_, cr, width, height| {
            let (width, height) = (width as f64, height as f64);
            let radius = height / 2.0;
            let threshold = config.borrow().silence_threshold;

            // Track.
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.08);
            rounded(cr, 0.0, 0.0, width, height, radius);
            let _ = cr.fill();

            // Everything else is clipped to the track, so neither the level nor
            // the threshold mark can spill past its rounded ends.
            let _ = cr.save();
            rounded(cr, 0.0, 0.0, width, height, radius);
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
        let config = Rc::clone(config);
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

            let (device, threshold) = {
                let config = config.borrow();
                (config.input_device.clone(), config.silence_threshold)
            };
            match crate::audio::Recorder::monitor(Some(&device), threshold) {
                Ok(open) => *recorder.borrow_mut() = Some(open),
                Err(err) => {
                    eprintln!("yapper: cannot open the microphone: {err:#}");
                    button.set_active(false);
                }
            }
        }
    });

    // Only runs while something is being monitored.
    gtk::glib::timeout_add_local(std::time::Duration::from_millis(50), {
        let recorder = Rc::clone(&recorder);
        let level = Rc::clone(&level);
        let meter = meter.downgrade();
        move || {
            let Some(meter) = meter.upgrade() else {
                return gtk::glib::ControlFlow::Break;
            };
            if let Some(open) = recorder.borrow().as_ref() {
                level.set(open.current_rms());
                meter.queue_draw();
            }
            gtk::glib::ControlFlow::Continue
        }
    });

    let threshold = adw::SpinRow::builder()
        .title("Silence threshold")
        .subtitle("Below this counts as silence")
        .adjustment(&gtk::Adjustment::new(
            config.borrow().silence_threshold as f64,
            0.001,
            0.100,
            0.001,
            0.010,
            0.0,
        ))
        .digits(3)
        .build();
    threshold.connect_value_notify({
        let config = Rc::clone(config);
        let on_change = Rc::clone(on_change);
        let meter = meter.clone();
        move |row| {
            config.borrow_mut().silence_threshold = row.value() as f32;
            save(&config, &on_change);
            meter.queue_draw();
        }
    });

    // Added straight to the group: a plain box nested inside one does not
    // render as a row.
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

fn rounded(cr: &gtk::cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::{FRAC_PI_2, PI};
    let r = r.min(w / 2.0).min(h / 2.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, FRAC_PI_2, PI);
    cr.arc(x + r, y + r, r, PI, 3.0 * FRAC_PI_2);
    cr.close_path();
}

/// The microphone picker. "System default" comes first and is what most people
/// want; the rest are the actual capture devices, not ALSA's plugin zoo.
fn microphone_row(config: &Rc<RefCell<Config>>, on_change: &OnChange) -> adw::ComboRow {
    let devices = crate::audio::input_devices();
    let chosen = config.borrow().input_device.clone();

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
        let config = Rc::clone(config);
        let on_change = Rc::clone(on_change);
        move |row| {
            let Some(id) = ids.get(row.selected() as usize) else {
                return;
            };
            config.borrow_mut().input_device = id.clone();
            save(&config, &on_change);
        }
    });
    row
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

    let can_type = crate::output::can_type();
    let type_row = adw::SwitchRow::builder()
        .title("Type into the focused window")
        .subtitle(if can_type {
            "Using wtype"
        } else {
            "Needs wtype installed"
        })
        .active(config.borrow().type_on_finish && can_type)
        .sensitive(can_type)
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
    let group = adw::PreferencesGroup::builder().title("Recording").build();

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

    group.add(&microphone_row(config, on_change));
    add_microphone_test(&group, config, on_change);

    let pause = adw::SwitchRow::builder()
        .title("Pause media while recording")
        .subtitle("Anything out of the speakers ends up in the transcript")
        .active(config.borrow().pause_players)
        .build();
    pause.connect_active_notify({
        let config = Rc::clone(config);
        let on_change = Rc::clone(on_change);
        move |row| {
            config.borrow_mut().pause_players = row.is_active();
            save(&config, &on_change);
        }
    });
    group.add(&pause);

    let silence = adw::SpinRow::builder()
        .title("Stop after silence")
        .subtitle("Seconds of quiet that end a recording. 0 waits for you to stop it")
        .adjustment(&gtk::Adjustment::new(
            config.borrow().silence_timeout as f64,
            0.0,
            30.0,
            0.5,
            1.0,
            0.0,
        ))
        .digits(1)
        .build();
    silence.connect_value_notify({
        let config = Rc::clone(config);
        let on_change = Rc::clone(on_change);
        move |row| {
            config.borrow_mut().silence_timeout = row.value() as f32;
            save(&config, &on_change);
        }
    });
    group.add(&silence);

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

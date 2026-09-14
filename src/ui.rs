//! The window: a status line, a level meter, one big record button, and the
//! transcript. Everything runs on the GTK main thread; recording and inference
//! happen elsewhere and report back over channels.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;

use crate::audio::Recorder;
use crate::config::Config;
use crate::output;
use crate::transcribe::{self, Event, Request};

const METER_BARS: usize = 48;
const TICK: Duration = Duration::from_millis(33);

#[derive(Clone, PartialEq)]
enum State {
    /// Waiting for the model to finish loading.
    Loading,
    Idle,
    Recording,
    Working,
    /// Unrecoverable: no model, no microphone, and so on.
    Broken(String),
}

struct App {
    config: Config,
    state: RefCell<State>,
    recorder: RefCell<Option<Recorder>>,
    levels: RefCell<VecDeque<f32>>,
    requests: async_channel::Sender<Request>,

    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    status_dot: gtk::Label,
    status_label: gtk::Label,
    meter: gtk::DrawingArea,
    record_button: gtk::Button,
    transcript: gtk::TextView,
    copy_button: gtk::Button,
    type_button: gtk::Button,
}

pub fn build(app: &adw::Application, config: Config) {
    load_css();

    let worker = transcribe::spawn(&config);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("yapper")
        .default_width(460)
        .default_height(560)
        .build();

    let status_dot = gtk::Label::builder()
        .label("\u{25cf}")
        .css_classes(["status-dot"])
        .build();
    let status_label = gtk::Label::builder()
        .label("Loading model\u{2026}")
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();

    let status_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    status_row.append(&status_dot);
    status_row.append(&status_label);

    let meter = gtk::DrawingArea::builder()
        .content_height(56)
        .hexpand(true)
        .build();

    let record_button = gtk::Button::builder()
        .label("Record")
        .sensitive(false)
        .css_classes(["suggested-action", "pill", "record-button"])
        .halign(gtk::Align::Center)
        .build();

    let transcript = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .left_margin(8)
        .right_margin(8)
        .top_margin(8)
        .bottom_margin(8)
        .css_classes(["transcript"])
        .build();
    let transcript_scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .has_frame(true)
        .child(&transcript)
        .build();

    let copy_button = gtk::Button::builder().label("Copy").sensitive(false).build();
    let type_button = gtk::Button::builder()
        .label("Type")
        .sensitive(false)
        .tooltip_text(match output::typing_backend() {
            Some(tool) => format!("Type into the focused window using {tool}"),
            None => "Install wtype or ydotool to type into the focused window".to_string(),
        })
        .build();
    let clear_button = gtk::Button::builder().label("Clear").build();

    let actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .halign(gtk::Align::End)
        .build();
    actions.append(&clear_button);
    actions.append(&type_button);
    actions.append(&copy_button);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    content.append(&status_row);
    content.append(&meter);
    content.append(&record_button);
    content.append(&transcript_scroll);
    content.append(&actions);

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&content));

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("yapper", "Ctrl+Space to talk")));

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&toasts));
    window.set_content(Some(&toolbar));

    let app_state = Rc::new(App {
        config,
        state: RefCell::new(State::Loading),
        recorder: RefCell::new(None),
        levels: RefCell::new(VecDeque::from(vec![0.0; METER_BARS])),
        requests: worker.requests,
        window: window.clone(),
        toasts,
        status_dot,
        status_label,
        meter: meter.clone(),
        record_button: record_button.clone(),
        transcript: transcript.clone(),
        copy_button: copy_button.clone(),
        type_button: type_button.clone(),
    });

    app_state.refresh_status();

    record_button.connect_clicked({
        let app_state = Rc::clone(&app_state);
        move |_| app_state.toggle()
    });

    copy_button.connect_clicked({
        let app_state = Rc::clone(&app_state);
        move |_| {
            let text = app_state.transcript_text();
            match output::copy(&text) {
                Ok(()) => app_state.toast("Copied to clipboard"),
                Err(err) => app_state.toast(&format!("Copy failed: {err}")),
            }
        }
    });

    type_button.connect_clicked({
        let app_state = Rc::clone(&app_state);
        move |_| {
            let text = app_state.transcript_text();
            if let Err(err) = output::type_text(&text) {
                app_state.toast(&format!("{err}"));
            }
        }
    });

    clear_button.connect_clicked({
        let app_state = Rc::clone(&app_state);
        move |_| app_state.set_transcript("")
    });

    meter.set_draw_func({
        let app_state = Rc::clone(&app_state);
        move |area, cr, width, height| app_state.draw_meter(area, cr, width, height)
    });

    // Drives the level meter and the recording timer.
    glib::timeout_add_local(TICK, {
        let app_state = Rc::clone(&app_state);
        move || {
            app_state.tick();
            glib::ControlFlow::Continue
        }
    });

    // Worker events arrive here without blocking the main loop.
    glib::spawn_future_local({
        let app_state = Rc::clone(&app_state);
        let events = worker.events;
        async move {
            while let Ok(event) = events.recv().await {
                app_state.handle_event(event);
            }
        }
    });

    install_shortcuts(&window, &app_state);
    install_signal_handler();

    window.present();
}

impl App {
    fn toggle(self: &Rc<Self>) {
        let state = self.state.borrow().clone();
        match state {
            State::Idle => self.start_recording(),
            State::Recording => self.stop_recording(),
            _ => {}
        }
    }

    fn start_recording(self: &Rc<Self>) {
        match Recorder::start() {
            Ok(recorder) => {
                *self.recorder.borrow_mut() = Some(recorder);
                self.set_state(State::Recording);
            }
            Err(err) => {
                self.toast(&format!("Microphone unavailable: {err}"));
                self.set_state(State::Broken(format!("{err:#}")));
            }
        }
    }

    fn stop_recording(self: &Rc<Self>) {
        let Some(recorder) = self.recorder.borrow_mut().take() else {
            return;
        };
        let samples = recorder.finish();
        self.levels.borrow_mut().iter_mut().for_each(|v| *v = 0.0);
        self.meter.queue_draw();

        if samples.is_empty() {
            self.set_state(State::Idle);
            self.toast("Nothing was recorded");
            return;
        }

        self.set_state(State::Working);
        if self.requests.send_blocking(Request::Transcribe(samples)).is_err() {
            self.set_state(State::Broken("the transcription worker stopped".into()));
        }
    }

    fn handle_event(self: &Rc<Self>, event: Event) {
        match event {
            Event::ModelReady => self.set_state(State::Idle),
            Event::ModelFailed(err) => {
                self.set_transcript(&err);
                self.set_state(State::Broken("model failed to load".into()));
            }
            Event::Transcribing => self.set_state(State::Working),
            Event::Done(text) => {
                self.set_state(State::Idle);
                if text.is_empty() {
                    self.toast("No speech detected");
                    return;
                }
                self.append_transcript(&text);
                if self.config.copy_to_clipboard
                    && let Err(err) = output::copy(&text)
                {
                    self.toast(&format!("Copy failed: {err}"));
                }
                if self.config.type_on_finish
                    && let Err(err) = output::type_text(&text)
                {
                    self.toast(&format!("{err}"));
                }
            }
            Event::Failed(err) => {
                self.set_state(State::Idle);
                self.toast(&format!("Transcription failed: {err}"));
            }
        }
    }

    fn tick(self: &Rc<Self>) {
        if TOGGLE_REQUESTED.swap(false, Ordering::Relaxed) {
            self.window.present();
            self.toggle();
        }

        let recording = matches!(*self.state.borrow(), State::Recording);

        let level = if recording {
            self.recorder
                .borrow()
                .as_ref()
                .map(|r| r.take_peak())
                .unwrap_or(0.0)
        } else {
            // Let the bars sink back down rather than snapping to flat.
            self.levels.borrow().back().copied().unwrap_or(0.0) * 0.85
        };

        {
            let mut levels = self.levels.borrow_mut();
            levels.push_back(level);
            while levels.len() > METER_BARS {
                levels.pop_front();
            }
        }
        self.meter.queue_draw();

        if recording && let Some(recorder) = self.recorder.borrow().as_ref() {
            let secs = recorder.duration_secs();
            self.status_label
                .set_label(&format!("Recording  {:01}:{:02}", secs as u32 / 60, secs as u32 % 60));
        }
    }

    fn draw_meter(&self, area: &gtk::DrawingArea, cr: &gtk::cairo::Context, width: i32, height: i32) {
        let color = area.color();
        let recording = matches!(*self.state.borrow(), State::Recording);
        let alpha = if recording { 0.95 } else { 0.3 };

        let levels = self.levels.borrow();
        let bars = levels.len().max(1);
        let slot = width as f64 / bars as f64;
        let bar_width = (slot * 0.55).max(1.0);
        let mid = height as f64 / 2.0;

        if recording {
            cr.set_source_rgba(0.88, 0.11, 0.14, alpha);
        } else {
            cr.set_source_rgba(
                color.red() as f64,
                color.green() as f64,
                color.blue() as f64,
                alpha,
            );
        }

        for (i, level) in levels.iter().enumerate() {
            // sqrt spreads quiet speech over more of the meter than raw amplitude.
            let scaled = level.sqrt().clamp(0.0, 1.0) as f64;
            let bar_height = (scaled * (height as f64 - 6.0)).max(2.0);
            let x = i as f64 * slot + (slot - bar_width) / 2.0;
            rounded_bar(cr, x, mid - bar_height / 2.0, bar_width, bar_height);
        }
        let _ = cr.fill();
    }

    fn set_state(self: &Rc<Self>, state: State) {
        *self.state.borrow_mut() = state;
        self.refresh_status();
    }

    fn refresh_status(self: &Rc<Self>) {
        let state = self.state.borrow().clone();
        let (dot_class, status, button_label, can_record) = match &state {
            State::Loading => ("busy", "Loading model\u{2026}".to_string(), "Record", false),
            State::Idle => ("ready", "Ready".to_string(), "Record", true),
            State::Recording => ("recording", "Recording  0:00".to_string(), "Stop", true),
            State::Working => ("busy", "Transcribing\u{2026}".to_string(), "Record", false),
            State::Broken(err) => ("error", err.clone(), "Record", false),
        };

        for class in ["ready", "busy", "recording", "error"] {
            self.status_dot.remove_css_class(class);
        }
        self.status_dot.add_css_class(dot_class);
        self.status_label.set_label(&status);
        self.record_button.set_label(button_label);
        self.record_button.set_sensitive(can_record);

        if matches!(state, State::Recording) {
            self.record_button.remove_css_class("suggested-action");
            self.record_button.add_css_class("destructive-action");
        } else {
            self.record_button.remove_css_class("destructive-action");
            self.record_button.add_css_class("suggested-action");
        }
    }

    fn transcript_text(&self) -> String {
        let buffer = self.transcript.buffer();
        buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .to_string()
    }

    fn set_transcript(self: &Rc<Self>, text: &str) {
        self.transcript.buffer().set_text(text);
        self.update_action_buttons();
    }

    fn append_transcript(self: &Rc<Self>, text: &str) {
        if !self.config.append_transcripts {
            self.set_transcript(text);
            return;
        }
        let buffer = self.transcript.buffer();
        let mut end = buffer.end_iter();
        if buffer.char_count() > 0 {
            buffer.insert(&mut end, "\n\n");
        }
        buffer.insert(&mut end, text);
        self.update_action_buttons();
        // Keep the newest text in view.
        self.transcript
            .scroll_to_mark(&buffer.create_mark(None, &buffer.end_iter(), false), 0.0, false, 0.0, 1.0);
    }

    fn update_action_buttons(&self) {
        let has_text = self.transcript.buffer().char_count() > 0;
        self.copy_button.set_sensitive(has_text);
        self.type_button
            .set_sensitive(has_text && output::typing_backend().is_some());
    }

    fn toast(&self, message: &str) {
        self.toasts.add_toast(adw::Toast::new(message));
    }
}

fn rounded_bar(cr: &gtk::cairo::Context, x: f64, y: f64, width: f64, height: f64) {
    use std::f64::consts::{FRAC_PI_2, PI};

    let radius = (width / 2.0).min(height / 2.0);
    cr.new_sub_path();
    cr.arc(x + width - radius, y + radius, radius, -FRAC_PI_2, 0.0);
    cr.arc(x + width - radius, y + height - radius, radius, 0.0, FRAC_PI_2);
    cr.arc(x + radius, y + height - radius, radius, FRAC_PI_2, PI);
    cr.arc(x + radius, y + radius, radius, PI, 3.0 * FRAC_PI_2);
    cr.close_path();
}

fn install_shortcuts(window: &adw::ApplicationWindow, app_state: &Rc<App>) {
    let controller = gtk::ShortcutController::new();
    controller.set_scope(gtk::ShortcutScope::Global);

    let toggle = gtk::CallbackAction::new({
        let app_state = Rc::clone(app_state);
        move |_, _| {
            app_state.toggle();
            glib::Propagation::Stop
        }
    });
    controller.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string("<Control>space"),
        Some(toggle),
    ));

    let stop = gtk::CallbackAction::new({
        let app_state = Rc::clone(app_state);
        move |_, _| {
            if matches!(*app_state.state.borrow(), State::Recording) {
                app_state.stop_recording();
            }
            glib::Propagation::Proceed
        }
    });
    controller.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string("Escape"),
        Some(stop),
    ));

    window.add_controller(controller);
}

/// `pkill -USR1 yapper` toggles recording, so a compositor keybind can drive it
/// without the window being focused.
///
/// The handler itself only flips a flag — anything more would not be
/// async-signal-safe. The UI tick picks the flag up a frame later.
static TOGGLE_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigusr1(_signal: libc::c_int) {
    TOGGLE_REQUESTED.store(true, Ordering::Relaxed);
}

fn install_signal_handler() {
    unsafe {
        libc::signal(libc::SIGUSR1, on_sigusr1 as *const () as libc::sighandler_t);
    }
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(
        "
        .status-dot { font-size: 11px; }
        .status-dot.ready { color: #2ec27e; }
        .status-dot.busy { color: #f5c211; }
        .status-dot.recording { color: #e01b24; }
        .status-dot.error { color: #e01b24; }
        .record-button { min-width: 150px; padding: 10px 24px; }
        .transcript { font-size: 1.05em; }
        ",
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

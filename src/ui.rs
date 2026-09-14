//! The window: a round record button with a row of bars dancing behind it, the
//! current status underneath, and the transcript below that.
//!
//! Everything here runs on the GTK main thread; recording and inference happen
//! elsewhere and report back over channels.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;

use crate::audio::Recorder;
use crate::config::Config;
use crate::output;
use crate::stage::Bars;
use crate::transcribe::{self, Event, Request};

/// How often the SIGUSR1 flag and the recording clock are checked. The
/// animation runs off the frame clock instead, so this can stay lazy.
const POLL: Duration = Duration::from_millis(100);

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
    bars: RefCell<Bars>,
    /// Frame clock timestamp of the previous animation frame, in microseconds.
    last_frame: Cell<i64>,
    first_frame: Cell<i64>,
    /// Whether a tick callback is currently installed. The animation stops
    /// itself once the bars settle, so an idle window costs nothing.
    animating: Cell<bool>,
    requests: async_channel::Sender<Request>,

    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    stage: gtk::DrawingArea,
    mic_button: gtk::Button,
    mic_pages: gtk::Stack,
    status_label: gtk::Label,
    hint_label: gtk::Label,
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
        .default_width(560)
        .default_height(620)
        .build();

    // --- The stage: bars behind, button on top -----------------------------

    let stage = gtk::DrawingArea::builder()
        .content_height(196)
        .content_width(400)
        .hexpand(true)
        .build();

    let mic_icon = gtk::Image::from_icon_name("audio-input-microphone-symbolic");
    mic_icon.set_pixel_size(46);
    let spinner = adw::Spinner::builder().width_request(42).height_request(42).build();

    let mic_pages = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .transition_duration(150)
        .build();
    mic_pages.add_named(&mic_icon, Some("mic"));
    mic_pages.add_named(&spinner, Some("busy"));

    let mic_button = gtk::Button::builder()
        .child(&mic_pages)
        .css_classes(["mic-button"])
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .sensitive(false)
        .tooltip_text("Start recording (Ctrl+Space)")
        .build();

    let overlay = gtk::Overlay::builder().child(&stage).build();
    overlay.add_overlay(&mic_button);
    overlay.set_measure_overlay(&mic_button, true);

    // --- Status and transcript ---------------------------------------------

    let status_label = gtk::Label::builder()
        .label("Loading model\u{2026}")
        .css_classes(["status"])
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let hint_label = gtk::Label::builder()
        .label("Ctrl+Space to talk")
        .css_classes(["hint", "dim-label"])
        .build();

    let transcript = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .left_margin(14)
        .right_margin(14)
        .top_margin(12)
        .bottom_margin(12)
        .css_classes(["transcript"])
        .build();
    let transcript_scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .css_classes(["transcript-frame"])
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
        .spacing(10)
        .margin_top(4)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();
    content.append(&overlay);
    content.append(&status_label);
    content.append(&hint_label);
    content.append(&transcript_scroll);
    content.append(&actions);

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&content));

    let header = adw::HeaderBar::builder()
        .css_classes(["flat"])
        .title_widget(&gtk::Label::builder().label("yapper").css_classes(["title"]).build())
        .build();

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&toasts));
    window.set_content(Some(&toolbar));

    let app_state = Rc::new(App {
        config,
        state: RefCell::new(State::Loading),
        recorder: RefCell::new(None),
        bars: RefCell::new(Bars::new()),
        last_frame: Cell::new(0),
        first_frame: Cell::new(0),
        animating: Cell::new(false),
        requests: worker.requests,
        window: window.clone(),
        toasts,
        stage: stage.clone(),
        mic_button: mic_button.clone(),
        mic_pages,
        status_label,
        hint_label,
        transcript: transcript.clone(),
        copy_button: copy_button.clone(),
        type_button: type_button.clone(),
    });

    app_state.refresh_status();

    mic_button.connect_clicked({
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

    stage.set_draw_func({
        let app_state = Rc::clone(&app_state);
        move |_, cr, width, height| {
            app_state
                .bars
                .borrow()
                .draw(cr, width as f64, height as f64, accent_color());
        }
    });

    // Animation runs off the frame clock, so it matches the display's refresh
    // rate instead of guessing at one.
    app_state.start_animating();

    // Signals and the recording clock don't need a frame-rate check-in.
    glib::timeout_add_local(POLL, {
        let app_state = Rc::clone(&app_state);
        move || {
            app_state.poll();
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
                self.start_animating();
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

        if samples.is_empty() {
            self.set_state(State::Idle);
            self.toast("Nothing was recorded");
            return;
        }

        self.set_state(State::Working);
        if self
            .requests
            .send_blocking(Request::Transcribe(samples))
            .is_err()
        {
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

    /// Install the tick callback, unless one is already running.
    fn start_animating(self: &Rc<Self>) {
        if self.animating.replace(true) {
            return;
        }
        self.stage.add_tick_callback({
            let app_state = Rc::clone(self);
            move |area, clock| app_state.animate(area, clock.frame_time())
        });
    }

    /// One animation frame. `frame_time` is the frame clock's timestamp in
    /// microseconds. Returns `Break` once the bars have settled, which uninstalls
    /// the callback until the next recording.
    fn animate(self: &Rc<Self>, area: &gtk::DrawingArea, frame_time: i64) -> glib::ControlFlow {
        if self.first_frame.get() == 0 {
            self.first_frame.set(frame_time);
            self.last_frame.set(frame_time);
        }
        // Clamp so a stalled frame (a resize, a busy CPU) can't make the bars jump.
        let dt = ((frame_time - self.last_frame.get()) as f32 / 1e6).clamp(0.0, 0.1);
        self.last_frame.set(frame_time);
        let elapsed = (frame_time - self.first_frame.get()) as f32 / 1e6;

        let recording = matches!(*self.state.borrow(), State::Recording);
        let level = if recording {
            self.recorder
                .borrow()
                .as_ref()
                .map(|recorder| recorder.take_peak())
                .unwrap_or(0.0)
        } else {
            0.0
        };

        let settled = {
            let mut bars = self.bars.borrow_mut();
            bars.advance(level, recording, elapsed, dt);
            bars.is_at_rest()
        };
        area.queue_draw();

        if settled && !recording {
            self.animating.set(false);
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    }

    /// Picks up SIGUSR1 and keeps the recording clock honest.
    fn poll(self: &Rc<Self>) {
        if TOGGLE_REQUESTED.swap(false, Ordering::Relaxed) {
            self.window.present();
            self.toggle();
        }

        if matches!(*self.state.borrow(), State::Recording)
            && let Some(recorder) = self.recorder.borrow().as_ref()
        {
            let secs = recorder.duration_secs() as u32;
            self.status_label
                .set_label(&format!("Listening  {}:{:02}", secs / 60, secs % 60));
        }
    }

    fn set_state(self: &Rc<Self>, state: State) {
        *self.state.borrow_mut() = state;
        self.refresh_status();
    }

    fn refresh_status(self: &Rc<Self>) {
        let state = self.state.borrow().clone();
        let (status, hint, tooltip, busy, can_record) = match &state {
            State::Loading => (
                "Loading model\u{2026}".to_string(),
                "This takes a moment on the first run",
                "Waiting for the model",
                true,
                false,
            ),
            State::Idle => (
                "Ready".to_string(),
                "Ctrl+Space to talk",
                "Start recording (Ctrl+Space)",
                false,
                true,
            ),
            State::Recording => (
                "Listening  0:00".to_string(),
                "Ctrl+Space or Escape to stop",
                "Stop recording (Escape)",
                false,
                true,
            ),
            State::Working => (
                "Transcribing\u{2026}".to_string(),
                "Hang on",
                "Transcribing",
                true,
                false,
            ),
            State::Broken(err) => (err.clone(), "See the transcript for details", "Unavailable", false, false),
        };

        self.status_label.set_label(&status);
        self.hint_label.set_label(hint);
        self.mic_button.set_tooltip_text(Some(tooltip));
        self.mic_button.set_sensitive(can_record);
        self.mic_pages
            .set_visible_child_name(if busy { "busy" } else { "mic" });

        let recording = matches!(state, State::Recording);
        set_css_class(&self.mic_button, "recording", recording);
        set_css_class(&self.status_label, "recording", recording);
        set_css_class(&self.status_label, "error", matches!(state, State::Broken(_)));
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
        self.transcript.scroll_to_mark(
            &buffer.create_mark(None, &buffer.end_iter(), false),
            0.0,
            false,
            0.0,
            1.0,
        );
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

fn set_css_class(widget: &impl IsA<gtk::Widget>, class: &str, wanted: bool) {
    if wanted {
        widget.add_css_class(class);
    } else {
        widget.remove_css_class(class);
    }
}

/// The user's chosen accent colour, so the bars match the rest of their desktop.
/// The standalone variant is the one libadwaita adjusts for legibility against
/// the window background, which matters most in dark mode.
fn accent_color() -> gtk::gdk::RGBA {
    let style = adw::StyleManager::default();
    style.accent_color().to_standalone_rgba(style.is_dark())
}

/// `pkill -USR1 yapper` toggles recording, so a compositor keybind can drive it
/// without the window being focused.
///
/// The handler itself only flips a flag — anything more would not be
/// async-signal-safe. The next poll picks the flag up.
static TOGGLE_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigusr1(_signal: libc::c_int) {
    TOGGLE_REQUESTED.store(true, Ordering::Relaxed);
}

fn install_signal_handler() {
    unsafe {
        libc::signal(libc::SIGUSR1, on_sigusr1 as *const () as libc::sighandler_t);
    }
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

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("style.css"));
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

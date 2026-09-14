//! The window: a round record button with a row of bars dancing behind it, the
//! current status underneath, and the list of past transcripts below that.
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
use crate::cli::Options;
use crate::config::Config;
use crate::history::{Entry, History, relative_time};
use crate::output;
use crate::stage::Bars;
use crate::transcribe::{self, Event, Request};

/// How often the SIGUSR1 flag and the recording clock are checked. The
/// animation runs off the frame clock instead, so this can stay lazy.
const POLL: Duration = Duration::from_millis(100);
/// How often "just now" is allowed to become "2 minutes ago".
const RESTAMP: Duration = Duration::from_secs(30);

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
    /// Quick capture: record on open, copy on close, then exit.
    quick: bool,
    /// Set once we've decided to exit, so the close handler stops intervening.
    quitting: Cell<bool>,
    app: adw::Application,
    state: RefCell<State>,
    recorder: RefCell<Option<Recorder>>,
    history: RefCell<History>,
    /// Rows in the same order as the history, so a selected row's index is an
    /// index into the entries.
    rows: RefCell<Vec<adw::ActionRow>>,
    /// Length of the clip currently being transcribed. Inference is serialised,
    /// so one slot is enough to pair a transcript with its recording.
    pending_duration: Cell<f32>,
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
    list: gtk::ListBox,
    list_pages: gtk::Stack,
    empty_page: adw::StatusPage,
    copy_button: gtk::Button,
    type_button: gtk::Button,
    delete_button: gtk::Button,
}

pub fn build(app: &adw::Application, config: Config, options: Options) {
    load_css();

    let worker = transcribe::spawn(&config);
    let history = History::load(config.history_limit);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("yapper")
        .default_width(if options.quick { 460 } else { 560 })
        .default_height(if options.quick { 240 } else { 640 })
        .resizable(!options.quick)
        .build();

    if options.quick {
        float_above_everything(&window);
    }

    // --- The stage: bars behind, button on top -----------------------------

    let stage = gtk::DrawingArea::builder()
        .content_height(if options.quick { 150 } else { 196 })
        .content_width(400)
        .hexpand(true)
        .build();

    let mic_icon = gtk::Image::from_icon_name("audio-input-microphone-symbolic");
    mic_icon.set_pixel_size(46);
    let spinner = adw::Spinner::builder()
        .width_request(42)
        .height_request(42)
        .build();

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

    // --- Status ------------------------------------------------------------

    let status_label = gtk::Label::builder()
        .label("Loading model\u{2026}")
        .css_classes(["status"])
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let hint_label = gtk::Label::builder()
        .label("Ctrl+Space to talk")
        .css_classes(["hint", "dim-label"])
        .build();

    // --- History list ------------------------------------------------------

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .css_classes(["boxed-list"])
        .valign(gtk::Align::Start)
        .margin_top(2)
        .margin_bottom(2)
        .build();

    let list_scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&list)
        .build();

    let empty_page = adw::StatusPage::builder()
        .icon_name("audio-input-microphone-symbolic")
        .title("No recordings yet")
        .description("Press the microphone, or Ctrl+Space, to make one")
        .vexpand(true)
        .css_classes(["compact"])
        .build();

    let list_pages = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .transition_duration(150)
        .vexpand(true)
        .build();
    list_pages.add_named(&empty_page, Some("empty"));
    list_pages.add_named(&list_scroll, Some("list"));

    let delete_button = icon_button("user-trash-symbolic", "Delete the selected recording");
    delete_button.add_css_class("delete");
    let type_button = icon_button(
        "input-keyboard-symbolic",
        &match output::typing_backend() {
            Some(tool) => format!("Type the selected transcript using {tool}"),
            None => "Install wtype or ydotool to type into the focused window".to_string(),
        },
    );
    let copy_button = icon_button("edit-copy-symbolic", "Copy the selected transcript");

    let actions = gtk::CenterBox::builder().margin_top(4).build();
    actions.set_start_widget(Some(&delete_button));
    let right = gtk::Box::builder().spacing(8).build();
    right.append(&type_button);
    right.append(&copy_button);
    actions.set_end_widget(Some(&right));

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .build();
    content.append(&overlay);
    content.append(&status_label);
    content.append(&hint_label);
    if !options.quick {
        content.append(&list_pages);
        content.append(&actions);
    }

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&content));

    if options.quick {
        // No header bar and no window decorations: this is a panel, not a
        // window. The card is drawn by the stylesheet.
        content.add_css_class("quick-card");
        window.set_content(Some(&toasts));
    } else {
        content.set_margin_top(4);
        content.set_margin_bottom(16);
        content.set_margin_start(16);
        content.set_margin_end(16);

        let header = adw::HeaderBar::builder()
            .css_classes(["flat"])
            .title_widget(
                &gtk::Label::builder()
                    .label("yapper")
                    .css_classes(["title"])
                    .build(),
            )
            .build();

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&toasts));
        window.set_content(Some(&toolbar));
    }

    let app_state = Rc::new(App {
        config,
        quick: options.quick,
        quitting: Cell::new(false),
        app: app.clone(),
        state: RefCell::new(State::Loading),
        recorder: RefCell::new(None),
        history: RefCell::new(history),
        rows: RefCell::new(Vec::new()),
        pending_duration: Cell::new(0.0),
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
        list: list.clone(),
        list_pages,
        empty_page,
        copy_button: copy_button.clone(),
        type_button: type_button.clone(),
        delete_button: delete_button.clone(),
    });

    app_state.refresh_status();
    app_state.rebuild_list(None);

    mic_button.connect_clicked({
        let app_state = Rc::clone(&app_state);
        move |_| app_state.toggle()
    });

    copy_button.connect_clicked({
        let app_state = Rc::clone(&app_state);
        move |_| app_state.copy_selected()
    });

    type_button.connect_clicked({
        let app_state = Rc::clone(&app_state);
        move |_| {
            let Some(entry) = app_state.selected_entry() else {
                return;
            };
            if let Err(err) = output::type_text(&entry.text) {
                app_state.toast(&format!("{err}"));
            }
        }
    });

    delete_button.connect_clicked({
        let app_state = Rc::clone(&app_state);
        move |_| app_state.delete_selected()
    });

    list.connect_row_selected({
        let app_state = Rc::clone(&app_state);
        move |_, _| app_state.update_action_buttons()
    });

    // Enter or a double click on a row copies it, which is what you almost
    // always want the history for.
    list.connect_row_activated({
        let app_state = Rc::clone(&app_state);
        move |_, _| app_state.copy_selected()
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

    // Relative timestamps drift out of date on their own.
    glib::timeout_add_local(RESTAMP, {
        let app_state = Rc::clone(&app_state);
        move || {
            app_state.restamp_rows();
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

    window.connect_close_request({
        let app_state = Rc::clone(&app_state);
        move |_| {
            if !app_state.quick || app_state.quitting.get() {
                return glib::Propagation::Proceed;
            }
            // Closing is how you finish a quick capture, so the panel goes away
            // immediately but the process lives until the text is on the
            // clipboard.
            app_state.dismiss();
            glib::Propagation::Stop
        }
    });

    install_shortcuts(&window, &app_state);
    install_signal_handler();

    window.present();

    // Recording doesn't wait for the model: the microphone can open now and the
    // audio queues up behind the load. That's the difference between a keybind
    // that feels instant and one that doesn't.
    if options.quick {
        app_state.start_recording();
    }
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

        self.pending_duration
            .set(samples.len() as f32 / crate::audio::TARGET_RATE as f32);
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
            Event::ModelReady => {
                // In quick capture we're already recording by now, so only the
                // initial wait is what this clears.
                if matches!(*self.state.borrow(), State::Loading) {
                    self.set_state(State::Idle);
                }
                // The list grabs focus while the button is still insensitive,
                // and a focused list selects its first row. Hand focus to the
                // button now that it can take it, so nothing starts selected.
                self.mic_button.grab_focus();
                self.list.unselect_all();
                self.update_action_buttons();
            }
            Event::ModelFailed(err) => {
                eprintln!("yapper: {err}");
                self.empty_page.set_title("Model unavailable");
                self.empty_page.set_description(Some(&err));
                self.set_state(State::Broken("model failed to load".into()));
                if self.quick && !self.window.is_visible() {
                    self.quit();
                }
            }
            Event::Transcribing => self.set_state(State::Working),
            Event::Done(text) => {
                self.set_state(State::Idle);
                if text.is_empty() {
                    self.report("No speech detected");
                    if self.quick {
                        self.quit();
                    }
                    return;
                }
                // Copying is the whole point of quick capture, whatever the
                // config says about the normal window.
                if (self.config.copy_to_clipboard || self.quick)
                    && let Err(err) = output::copy(&text)
                {
                    self.report(&format!("Copy failed: {err}"));
                }
                if self.config.type_on_finish
                    && let Err(err) = output::type_text(&text)
                {
                    self.report(&format!("{err}"));
                }
                self.remember(text);
                if self.quick {
                    self.quit();
                }
            }
            Event::Failed(err) => {
                self.set_state(State::Idle);
                self.report(&format!("Transcription failed: {err}"));
                if self.quick {
                    self.quit();
                }
            }
        }
    }

    /// Add a finished transcript to the history and select it.
    fn remember(self: &Rc<Self>, text: String) {
        let duration = self.pending_duration.replace(0.0);
        if let Err(err) = self.history.borrow_mut().add(text, duration) {
            self.toast(&format!("Could not save the recording: {err}"));
        }
        // The newest entry is always first, so the new row is index 0.
        self.rebuild_list(Some(0));
    }

    fn selected_entry(&self) -> Option<Entry> {
        let index = self.list.selected_row()?.index();
        self.history
            .borrow()
            .entries()
            .get(index as usize)
            .cloned()
    }

    fn copy_selected(self: &Rc<Self>) {
        let Some(entry) = self.selected_entry() else {
            return;
        };
        match output::copy(&entry.text) {
            Ok(()) => self.toast("Copied to clipboard"),
            Err(err) => self.toast(&format!("Copy failed: {err}")),
        }
    }

    fn delete_selected(self: &Rc<Self>) {
        let Some(row) = self.list.selected_row() else {
            return;
        };
        let index = row.index() as usize;
        let Some(entry) = self.history.borrow().entries().get(index).cloned() else {
            return;
        };

        match self.history.borrow_mut().remove(entry.id) {
            Ok(true) => {}
            Ok(false) => return,
            Err(err) => {
                self.toast(&format!("Could not delete the recording: {err}"));
                return;
            }
        }

        // Keep the selection where the eye already is: the row that slid up
        // into the deleted one's place, or the new last row.
        let remaining = self.history.borrow().entries().len();
        let next = if remaining == 0 {
            None
        } else {
            Some(index.min(remaining - 1))
        };
        self.rebuild_list(next);
        self.toast("Recording deleted");
    }

    fn rebuild_list(self: &Rc<Self>, select: Option<usize>) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }

        let rows: Vec<adw::ActionRow> = self
            .history
            .borrow()
            .entries()
            .iter()
            .map(build_row)
            .collect();
        for row in &rows {
            self.list.append(row);
        }
        let empty = rows.is_empty();
        *self.rows.borrow_mut() = rows;

        self.list_pages
            .set_visible_child_name(if empty { "empty" } else { "list" });

        if let Some(index) = select
            && let Some(row) = self.list.row_at_index(index as i32)
        {
            self.list.select_row(Some(&row));
        }
        self.update_action_buttons();
    }

    /// Refresh the "5 minutes ago" subtitles in place, so the selection and the
    /// scroll position survive.
    fn restamp_rows(self: &Rc<Self>) {
        let history = self.history.borrow();
        for (row, entry) in self.rows.borrow().iter().zip(history.entries()) {
            row.set_subtitle(&relative_time(entry.recorded_at()));
        }
    }

    fn update_action_buttons(&self) {
        let selected = self.list.selected_row().is_some();
        self.copy_button.set_sensitive(selected);
        self.delete_button.set_sensitive(selected);
        self.type_button
            .set_sensitive(selected && output::typing_backend().is_some());
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
            State::Broken(err) => (
                err.clone(),
                "See below for details",
                "Unavailable",
                false,
                false,
            ),
        };

        self.status_label.set_label(&status);
        self.hint_label.set_label(if self.quick {
            quick_hint(&state)
        } else {
            hint
        });
        self.mic_button.set_tooltip_text(Some(tooltip));
        self.mic_button.set_sensitive(can_record);
        self.mic_pages
            .set_visible_child_name(if busy { "busy" } else { "mic" });

        let recording = matches!(state, State::Recording);
        set_css_class(&self.mic_button, "recording", recording);
        set_css_class(&self.status_label, "recording", recording);
        set_css_class(
            &self.status_label,
            "error",
            matches!(state, State::Broken(_)),
        );
    }

    fn toast(&self, message: &str) {
        self.toasts.add_toast(adw::Toast::new(message));
    }

    /// Say something to the user. A quick capture panel is usually gone or
    /// going by the time anything goes wrong, so it speaks through stderr.
    fn report(&self, message: &str) {
        if self.quick {
            eprintln!("yapper: {message}");
        } else {
            self.toast(message);
        }
    }

    /// Finish a quick capture: hide the panel at once, but stay alive until any
    /// transcription in flight has landed on the clipboard.
    fn dismiss(self: &Rc<Self>) {
        let state = self.state.borrow().clone();
        match state {
            State::Recording => {
                self.stop_recording();
                self.window.set_visible(false);
            }
            State::Working => self.window.set_visible(false),
            _ => self.quit(),
        }
    }

    fn quit(&self) {
        self.quitting.set(true);
        self.app.quit();
    }
}

/// Put the panel on the overlay layer, centred, with the keyboard — the same
/// treatment a launcher gets, so no compositor rule is needed to float it.
fn float_above_everything(window: &adw::ApplicationWindow) {
    use gtk_layer_shell::{KeyboardMode, Layer, LayerShell};

    window.add_css_class("quick");

    if !gtk_layer_shell::is_supported() {
        eprintln!(
            "yapper: this compositor has no layer-shell, falling back to an              ordinary window — float it with a rule on app id dev.yapper.Yapper.Quick"
        );
        return;
    }

    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    // Exclusive so Escape reaches us rather than whatever is underneath.
    window.set_keyboard_mode(KeyboardMode::Exclusive);
    window.set_namespace(Some("yapper-quick"));
    // No anchors, so the compositor centres the surface.
}

/// Quick capture has one way out, and the normal window's hints don't describe it.
fn quick_hint(state: &State) -> &'static str {
    match state {
        State::Recording => "Escape to stop and copy",
        State::Working => "Copying to the clipboard\u{2026}",
        State::Loading => "Loading model\u{2026}",
        _ => "Escape to close",
    }
}

fn build_row(entry: &Entry) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        // Transcripts are arbitrary text, so they are never markup.
        .use_markup(false)
        .title(entry.summary())
        .subtitle(relative_time(entry.recorded_at()))
        .title_lines(2)
        .subtitle_lines(1)
        .activatable(true)
        .build();

    let duration = gtk::Label::builder()
        .label(entry.duration_label())
        .css_classes(["dim-label", "numeric"])
        .valign(gtk::Align::Center)
        .build();
    row.add_suffix(&duration);
    row
}

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .sensitive(false)
        .css_classes(["circular"])
        .build()
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
            if app_state.quick {
                // Escape is how a quick capture ends: stop, copy, close.
                app_state.dismiss();
                return glib::Propagation::Stop;
            }
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

    let delete = gtk::CallbackAction::new({
        let app_state = Rc::clone(app_state);
        move |_, _| {
            app_state.delete_selected();
            glib::Propagation::Stop
        }
    });
    controller.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string("Delete"),
        Some(delete),
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

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
use crate::history::{Entry, History, mm_ss, relative_time, relative_time_at};
use crate::picker;
use crate::preferences;
use crate::output;
use crate::players;
use crate::stage::Bars;
use crate::transcribe::{self, Event, Worker};

/// How often the SIGUSR1 flag and the recording clock are checked. The
/// animation runs off the frame clock instead, so this can stay lazy.
const POLL: Duration = Duration::from_millis(100);
/// How often "just now" is allowed to become "2 minutes ago".
const RESTAMP: Duration = Duration::from_secs(30);
/// How often the running transcript is refreshed while recording. One preview
/// is in flight at a time, so a slow pass just means fewer updates.
const PREVIEW_EVERY: Duration = Duration::from_millis(500);
/// A model invents words from a fragment, so wait for something to work with.
const PREVIEW_MIN_SECS: f32 = 1.0;
/// Roughly three lines. The tail is what you want to read, not the beginning.
const PREVIEW_CHARS: usize = 150;
/// Gaps between words are silence too. Waiting this long before showing the
/// countdown keeps it from flickering on every breath.
const COUNTDOWN_AFTER: f32 = 0.35;

const LOADING: &str = "Loading model\u{2026}";
const START_TOOLTIP: &str = "Start recording (Ctrl+Space)";
const RECORDING_HINT: &str = "Enter to finish \u{00b7} Escape to discard";

// The first launch builds the window and stays resident; later ones find this
// process over D-Bus and land in `build` again with the model already loaded.
// The hold guard keeps the process alive while the panel is hidden.
thread_local! {
    static RUNNING: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
    static HOLD: RefCell<Option<gtk::gio::ApplicationHoldGuard>> = const { RefCell::new(None) };
}

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
    config: RefCell<Config>,
    /// Quick capture: record on open, copy on close, then exit.
    quick: bool,
    /// Set once we've decided to exit, so the close handler stops intervening.
    quitting: Cell<bool>,
    /// What should happen to this particular capture. Set per capture rather
    /// than per process, because one resident process serves keybinds that
    /// want different things.
    type_this_capture: Cell<bool>,
    copy_this_capture: Cell<bool>,
    state: RefCell<State>,
    recorder: RefCell<Option<Recorder>>,
    history: RefCell<History>,
    /// Rows in the same order as the history, so a selected row's index is an
    /// index into the entries.
    rows: RefCell<Vec<adw::ActionRow>>,
    /// Players we paused for this recording, waiting to be started again.
    paused_players: RefCell<players::Paused>,
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
    /// Replaced when a different model is picked, which is what loads it.
    worker: RefCell<Worker>,
    /// Bumped on every worker restart, so events from the model that was just
    /// replaced can be recognised and dropped.
    generation: Cell<u64>,
    /// One preview at a time: sending more only queues work behind the one
    /// that is already too old.
    preview_pending: Cell<bool>,

    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    stage: gtk::DrawingArea,
    mic_button: gtk::Button,
    mic_pages: gtk::Stack,
    status_label: gtk::Label,
    hint_label: gtk::Label,
    banner: adw::Banner,
    preview_label: gtk::Label,
    list: gtk::ListBox,
    list_pages: gtk::Stack,
    empty_page: adw::StatusPage,
    copy_button: gtk::Button,
    type_button: gtk::Button,
    delete_button: gtk::Button,
}

pub fn build(app: &adw::Application, config: Config, options: Options) {
    if let Some(running) = RUNNING.with(|running| running.borrow().clone()) {
        running.reopen();
        return;
    }

    load_css();

    let worker = transcribe::spawn(&config);
    let events = worker.events.clone();
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
        .build();

    let overlay = gtk::Overlay::builder().child(&stage).build();
    overlay.add_overlay(&mic_button);
    overlay.set_measure_overlay(&mic_button, true);

    // --- Status ------------------------------------------------------------

    // Text for these comes from `refresh_status`, which runs before the window
    // is shown.
    let status_label = gtk::Label::builder()
        .css_classes(["status"])
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let hint_label = gtk::Label::builder()
        .css_classes(["hint", "dim-label"])
        .build();

    // The running transcript. A fixed height keeps the panel from jumping
    // about as the text grows a line.
    let preview_label = gtk::Label::builder()
        .css_classes(["preview"])
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .justify(gtk::Justification::Center)
        .max_width_chars(44)
        .height_request(58)
        .valign(gtk::Align::Start)
        .visible(false)
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
        if output::can_type() {
            "Type the selected transcript into the focused window"
        } else {
            "Install wtype to type into the focused window"
        },
    );
    let copy_button = icon_button("edit-copy-symbolic", "Copy the selected transcript");

    let actions = gtk::CenterBox::builder().margin_top(4).build();
    actions.set_start_widget(Some(&delete_button));
    let right = gtk::Box::builder().spacing(8).build();
    right.append(&type_button);
    right.append(&copy_button);
    actions.set_end_widget(Some(&right));

    // Shown only when something needs doing that yapper cannot do itself.
    // A banner rather than the empty-state page, because the page is hidden as
    // soon as there is any history, and quick capture has no page at all.
    let banner = adw::Banner::builder()
        .button_label("Choose a model")
        .revealed(false)
        .build();

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .build();
    content.append(&banner);
    content.append(&overlay);
    content.append(&status_label);
    content.append(&hint_label);
    content.append(&preview_label);
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

        let menu = gtk::gio::Menu::new();
        menu.append(Some("Models\u{2026}"), Some("win.models"));
        menu.append(Some("Preferences"), Some("win.preferences"));
        header.pack_end(
            &gtk::MenuButton::builder()
                .icon_name("open-menu-symbolic")
                .tooltip_text("Main menu")
                .menu_model(&menu)
                .build(),
        );

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&toasts));
        window.set_content(Some(&toolbar));
    }

    let app_state = Rc::new(App {
        config: RefCell::new(config),
        quick: options.quick,
        quitting: Cell::new(false),
        type_this_capture: Cell::new(options.type_output),
        copy_this_capture: Cell::new(options.copy_output),
        state: RefCell::new(State::Loading),
        recorder: RefCell::new(None),
        history: RefCell::new(history),
        rows: RefCell::new(Vec::new()),
        paused_players: RefCell::new(players::Paused::default()),
        pending_duration: Cell::new(0.0),
        bars: RefCell::new(Bars::new()),
        last_frame: Cell::new(0),
        first_frame: Cell::new(0),
        animating: Cell::new(false),
        preview_pending: Cell::new(false),
        worker: RefCell::new(worker),
        generation: Cell::new(0),
        window: window.clone(),
        toasts,
        stage: stage.clone(),
        mic_button: mic_button.clone(),
        mic_pages,
        status_label,
        hint_label,
        banner,
        preview_label,
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
        move |_| app_state.type_selected()
    });

    delete_button.connect_clicked({
        let app_state = Rc::clone(&app_state);
        move |_| app_state.delete_selected()
    });

    list.connect_row_selected({
        let app_state = Rc::clone(&app_state);
        move |_, _| app_state.update_action_buttons()
    });

    // Clicking a row only selects it. Copying is something you ask for — with
    // Ctrl+C or the button — not something a stray click does to your
    // clipboard.

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

    // The running transcript, while recording.
    glib::timeout_add_local(PREVIEW_EVERY, {
        let app_state = Rc::clone(&app_state);
        move || {
            if app_state.is_recording() {
                app_state.request_preview();
            }
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
    app_state.listen(events);

    // The banner is the way out of having no model at all, so it opens the
    // picker rather than a web page.
    app_state.banner.connect_button_clicked({
        let app_state = Rc::clone(&app_state);
        move |_| app_state.show_models()
    });

    RUNNING.with(|running| *running.borrow_mut() = Some(Rc::clone(&app_state)));

    // Without this the process would end when the panel is hidden, and the next
    // keypress would pay for the model all over again.
    if options.quick {
        HOLD.with(|hold| *hold.borrow_mut() = Some(app.hold()));
    }

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

    if !options.quick {
        let preferences = gtk::gio::SimpleAction::new("preferences", None);
        preferences.connect_activate({
            let app_state = Rc::clone(&app_state);
            move |_, _| app_state.show_preferences()
        });
        window.add_action(&preferences);
        app.set_accels_for_action("win.preferences", &["<Control>comma"]);

        let models = gtk::gio::SimpleAction::new("models", None);
        models.connect_activate({
            let app_state = Rc::clone(&app_state);
            move |_, _| app_state.show_models()
        });
        window.add_action(&models);
    }

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
    fn is_recording(&self) -> bool {
        matches!(*self.state.borrow(), State::Recording)
    }

    fn toggle(self: &Rc<Self>) {
        let state = self.state.borrow().clone();
        match state {
            // Recording needs the microphone, not the model, so a key pressed
            // during the initial load starts capturing rather than being
            // swallowed. The audio queues behind the load.
            State::Idle | State::Loading => self.start_recording(),
            State::Recording => self.stop_recording(),
            _ => {}
        }
    }

    fn start_recording(self: &Rc<Self>) {
        // Quiet the room before opening the microphone rather than after, so
        // the first moment of the recording is not the tail of a song.
        if self.config.borrow().pause_players {
            *self.paused_players.borrow_mut() = players::pause_playing();
        }

        let opened = {
            let config = self.config.borrow();
            Recorder::start(&config.input_device, config.silence_threshold)
        };
        match opened {
            Ok(recorder) => {
                *self.recorder.borrow_mut() = Some(recorder);
                self.show_preview("");
                // Claim the space now rather than letting the window jump when
                // the first preview arrives a second or two later.
                self.preview_label.set_visible(self.config.borrow().live_preview);
                self.set_state(State::Recording);
                self.start_animating();
            }
            Err(err) => {
                // Nothing is going to be recorded, so give the music back.
                self.resume_players();
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
        // The microphone is shut, so the music can come back now rather than
        // waiting for the transcript.
        self.resume_players();

        if samples.is_empty() {
            self.set_state(State::Idle);
            self.toast("Nothing was recorded");
            return;
        }

        self.pending_duration
            .set(samples.len() as f32 / crate::audio::TARGET_RATE as f32);
        // Quick capture is done with the screen the moment you finish; the
        // process stays up only long enough to copy.
        if self.quick {
            self.window.set_visible(false);
        }
        // Any preview still running is now pointless; the worker drops it.
        self.preview_pending.set(false);
        self.set_state(State::Working);
        if self.worker.borrow().transcribe(samples).is_err() {
            self.set_state(State::Broken("the transcription worker stopped".into()));
        }
    }

    /// Ask for a refreshed running transcript, unless one is already being made.
    fn request_preview(self: &Rc<Self>) {
        if !self.config.borrow().live_preview || self.preview_pending.get() {
            return;
        }
        let Some(samples) = self
            .recorder
            .borrow()
            .as_ref()
            .filter(|recorder| recorder.duration_secs() >= PREVIEW_MIN_SECS)
            .map(|recorder| recorder.snapshot())
        else {
            return;
        };
        self.preview_pending.set(true);
        self.worker.borrow().preview(samples);
    }

    /// Show the tail of the running transcript, or hide the label when empty.
    fn show_preview(self: &Rc<Self>, text: &str) {
        let text = tail(text, PREVIEW_CHARS);
        self.preview_label.set_visible(!text.is_empty());
        self.preview_label.set_label(&text);
    }

    /// Throw the recording away: no transcription, no clipboard, no history.
    /// The only way to lose audio on purpose, which is why it is a key of its
    /// own rather than something a stray close does.
    fn cancel_recording(self: &Rc<Self>) {
        let Some(recorder) = self.recorder.borrow_mut().take() else {
            return;
        };
        drop(recorder);
        self.resume_players();
        self.show_preview("");
        self.preview_pending.set(false);
        self.set_state(State::Idle);

        if self.quick {
            self.retire();
        } else {
            self.toast("Recording discarded");
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
                self.banner
                    .set_title("No speech model yet. Choose one and yapper will download it.");
                self.banner.set_revealed(true);
                self.set_state(State::Broken("no speech model".into()));
                if self.quick && !self.window.is_visible() {
                    self.quit();
                }
            }
            Event::Preview(text) => {
                self.preview_pending.set(false);
                // An empty preview means the pass failed or found nothing yet;
                // keep whatever was on screen rather than blinking it away.
                if !text.is_empty() && self.is_recording() {
                    self.show_preview(&text);
                }
            }
            Event::Done(text) => {
                self.set_state(State::Idle);
                if text.is_empty() {
                    self.report("No speech detected");
                } else {
                    self.deliver(&text);
                    self.show_preview("");
                    self.remember(text);
                }
                self.finish_capture();
            }
            Event::Failed(err) => {
                self.set_state(State::Idle);
                self.report(&format!("Transcription failed: {err}"));
                self.finish_capture();
            }
        }
    }

    /// Copy and/or type the transcript. Quick capture does what its flags asked
    /// for and nothing else; the window follows the settings. Asking for
    /// neither is a real answer: keep the transcript, touch nothing.
    fn deliver(&self, text: &str) {
        let (wants_copy, wants_typing) = if self.quick {
            (self.copy_this_capture.get(), self.type_this_capture.get())
        } else {
            let config = self.config.borrow();
            (config.copy_to_clipboard, config.type_on_finish)
        };

        if wants_copy
            && let Err(err) = output::copy(text)
        {
            self.report(&format!("Copy failed: {err}"));
        }
        if wants_typing
            && let Err(err) = output::type_text(text)
        {
            self.report(&format!("{err}"));
        }
    }

    /// The capture is over, one way or another. Quick capture puts the panel
    /// away; the window just stays as it is.
    fn finish_capture(self: &Rc<Self>) {
        if self.quick {
            self.retire();
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

    /// The selected row's position in the list and the entry it shows.
    fn selected_entry(&self) -> Option<(usize, Entry)> {
        let index = self.list.selected_row()?.index() as usize;
        let entry = self.history.borrow().entries().get(index)?.clone();
        Some((index, entry))
    }

    fn copy_selected(self: &Rc<Self>) {
        let Some((_, entry)) = self.selected_entry() else {
            return;
        };
        match output::copy(&entry.text) {
            Ok(()) => self.toast("Copied to clipboard"),
            Err(err) => self.toast(&format!("Copy failed: {err}")),
        }
    }

    fn type_selected(self: &Rc<Self>) {
        let Some((_, entry)) = self.selected_entry() else {
            return;
        };
        if let Err(err) = output::type_text(&entry.text) {
            self.toast(&format!("{err}"));
        }
    }

    fn delete_selected(self: &Rc<Self>) {
        let Some((index, entry)) = self.selected_entry() else {
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
        self.list.remove_all();

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
        let Ok(now) = glib::DateTime::now_local() else {
            return;
        };
        let history = self.history.borrow();
        for (row, entry) in self.rows.borrow().iter().zip(history.entries()) {
            row.set_subtitle(&relative_time_at(entry.recorded_at(), &now));
        }
    }

    fn update_action_buttons(&self) {
        let selected = self.list.selected_row().is_some();
        self.copy_button.set_sensitive(selected);
        self.delete_button.set_sensitive(selected);
        self.type_button
            .set_sensitive(selected && output::can_type());
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

        let recording = self.is_recording();
        let level = if recording {
            self.recorder
                .borrow()
                .as_ref()
                .map(|recorder| recorder.take_peak())
                .unwrap_or(0.0)
        } else {
            0.0
        };

        let countdown = self.countdown();
        let settled = {
            let mut bars = self.bars.borrow_mut();
            bars.advance(level, recording, elapsed, dt);
            bars.set_countdown(countdown);
            bars.is_at_rest()
        };
        area.queue_draw();

        if settled && !recording {
            self.animating.set(false);
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    }

    fn resume_players(self: &Rc<Self>) {
        players::resume(self.paused_players.take());
    }

    /// How much of the silence timeout is left, as a fraction, or `None` when
    /// nothing is counting down.
    fn countdown(&self) -> Option<f32> {
        if !self.is_recording() {
            return None;
        }
        let timeout = self.config.borrow().silence_timeout;
        let silence = self.recorder.borrow().as_ref()?.silence_secs();
        silence_remaining(silence, timeout).map(|left| (left / timeout).clamp(0.0, 1.0))
    }

    /// What the line under the status says while recording.
    fn recording_hint(&self, silence: f32, timeout: f32) -> String {
        match silence_remaining(silence, timeout) {
            Some(left) => format!("Quiet \u{2014} stopping in {left:.1}s"),
            None => RECORDING_HINT.to_string(),
        }
    }

    /// Picks up SIGUSR1 and keeps the recording clock honest.
    fn poll(self: &Rc<Self>) {
        if TOGGLE_REQUESTED.swap(false, Ordering::Relaxed) {
            self.window.present();
            self.toggle();
        }

        if !self.is_recording() {
            return;
        }

        let Some((elapsed, silence)) = self
            .recorder
            .borrow()
            .as_ref()
            .map(|recorder| (recorder.duration_secs(), recorder.silence_secs()))
        else {
            return;
        };

        self.status_label.set_label(&listening(elapsed as u32));

        let timeout = self.config.borrow().silence_timeout;
        self.hint_label
            .set_label(&self.recording_hint(silence, timeout));

        // End the recording once the room has been quiet for long enough. The
        // recorder only counts silence after the first word, so this cannot
        // fire before anything has been said.
        if timeout > 0.0 && silence >= timeout {
            self.stop_recording();
        }
    }

    fn set_state(self: &Rc<Self>, state: State) {
        *self.state.borrow_mut() = state;
        self.refresh_status();
    }

    fn refresh_status(self: &Rc<Self>) {
        let state = self.state.borrow().clone();
        // Quick capture has one way out, and the window's hints don't describe it.
        let quick = self.quick;
        let (status, hint, tooltip, busy, can_record) = match &state {
            State::Loading => (
                LOADING.to_string(),
                if quick { LOADING } else { "Ready to record — the model is still loading" },
                START_TOOLTIP,
                true,
                true,
            ),
            State::Idle => (
                "Ready".to_string(),
                if quick { "Escape to close" } else { "Ctrl+Space to talk" },
                START_TOOLTIP,
                false,
                true,
            ),
            State::Recording => (
                listening(0),
                RECORDING_HINT,
                "Finish recording (Enter)",
                false,
                true,
            ),
            State::Working => (
                "Transcribing\u{2026}".to_string(),
                if quick { "Copying to the clipboard\u{2026}" } else { "Hang on" },
                "Transcribing",
                true,
                false,
            ),
            State::Broken(err) => (
                err.clone(),
                if quick { "Escape to close" } else { "" },
                "Unavailable",
                false,
                false,
            ),
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
        set_css_class(
            &self.status_label,
            "error",
            matches!(state, State::Broken(_)),
        );
    }

    /// Hand worker events to `handle_event` until that worker is replaced.
    ///
    /// A model being swapped leaves the old thread finishing whatever it was
    /// doing; its answers are for a model that is no longer in use, so they are
    /// dropped rather than pasted into the window.
    fn listen(self: &Rc<Self>, events: async_channel::Receiver<Event>) {
        let generation = self.generation.get();
        glib::spawn_future_local({
            let app_state = Rc::clone(self);
            async move {
                while let Ok(event) = events.recv().await {
                    if app_state.generation.get() != generation {
                        break;
                    }
                    app_state.handle_event(event);
                }
            }
        });
    }

    /// Load a different model. The old worker's request channel closes as it is
    /// dropped, which is what ends its thread and frees the model it held.
    fn use_model(self: &Rc<Self>) {
        let worker = transcribe::spawn(&self.config.borrow());
        let events = worker.events.clone();
        self.generation.set(self.generation.get() + 1);
        *self.worker.borrow_mut() = worker;
        self.listen(events);
        // The preview the old worker was running will never be answered now,
        // and the slot it holds would otherwise keep the new model from being
        // asked for one.
        self.preview_pending.set(false);

        self.banner.set_revealed(false);
        // Mid-recording the microphone is still fine and the audio is still
        // wanted; the model will have loaded by the time there is anything to
        // transcribe.
        if !self.is_recording() {
            self.set_state(State::Loading);
        }
    }

    /// The model picker, from the banner or from the menu.
    fn show_models(self: &Rc<Self>) {
        let app_state = Rc::clone(self);
        let config = self.config.borrow().clone();
        picker::present(
            &self.window,
            &config,
            Rc::new(move |change| {
                // A different model has to be loaded; a language it was told
                // is picked up by the worker on the next transcription, which
                // reloads the recognizer only if it has to.
                let reload = match change {
                    picker::Change::Model(model) => {
                        if app_state.config.borrow().model == model.id {
                            return;
                        }
                        app_state.config.borrow_mut().model = model.id.to_string();
                        true
                    }
                    picker::Change::Language {
                        model,
                        hears,
                        writes,
                    } => {
                        app_state
                            .config
                            .borrow_mut()
                            .set_language(model, hears, writes);
                        false
                    }
                };
                if let Err(err) = app_state.config.borrow().save() {
                    eprintln!("yapper: could not save the model choice: {err:#}");
                }
                if reload {
                    app_state.use_model();
                } else {
                    let settings = transcribe::Settings::from_config(&app_state.config.borrow());
                    app_state.worker.borrow().apply(settings);
                }
            }),
        );
    }

    fn show_preferences(self: &Rc<Self>) {
        let app_state = Rc::clone(self);
        preferences::present(
            &self.window,
            &self.config.borrow().clone(),
            Rc::new(move |config| app_state.apply_config(config)),
        );
    }

    /// Take on preferences edited in the dialog. Everything is live, the model
    /// included: a different one is loaded there and then.
    fn apply_config(self: &Rc<Self>, config: &Config) {
        let (previous_limit, previous_model) = {
            let current = self.config.borrow();
            (current.history_limit, current.model.clone())
        };
        *self.config.borrow_mut() = config.clone();
        self.worker
            .borrow()
            .apply(transcribe::Settings::from_config(config));

        if config.history_limit != previous_limit {
            *self.history.borrow_mut() = History::load(config.history_limit);
            self.rebuild_list(None);
        }
        if !config.live_preview {
            self.show_preview("");
        }
        if config.model != previous_model {
            self.use_model();
        }
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

    /// Close a quick capture panel. On a layer surface Escape arrives as a
    /// close rather than a key we can intercept, so closing means the same
    /// thing Escape does: discard. Enter is how a recording is kept.
    ///
    /// A transcription already under way is not thrown away — the panel just
    /// goes, and the process lives until the text reaches the clipboard.
    fn dismiss(self: &Rc<Self>) {
        let state = self.state.borrow().clone();
        match state {
            State::Recording => self.cancel_recording(),
            State::Working => self.window.set_visible(false),
            _ => self.retire(),
        }
    }

    fn quit(&self) {
        self.quitting.set(true);
        if let Some(app) = self.window.application() {
            app.quit();
        }
    }

    /// A later launch, arriving at the instance that is already running.
    fn reopen(self: &Rc<Self>) {
        self.window.present();
        // Opening quick capture means starting a recording; that is the verb.
        if self.quick && *self.state.borrow() == State::Idle {
            self.start_recording();
        }
    }

    /// Put the panel away but stay in memory, ready for the next keypress.
    fn retire(self: &Rc<Self>) {
        self.window.set_visible(false);
        self.show_preview("");
        self.set_state(State::Idle);
    }
}

/// Put the panel on the overlay layer, centred, with the keyboard — the same
/// treatment a launcher gets, so no compositor rule is needed to float it.
fn float_above_everything(window: &adw::ApplicationWindow) {
    use gtk_layer_shell::{KeyboardMode, Layer, LayerShell};

    window.add_css_class("quick");

    if !gtk_layer_shell::is_supported() {
        eprintln!(
            "yapper: this compositor has no layer-shell, falling back to an \
             ordinary window — float it with a rule on app id dev.yapper.Yapper.Quick"
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

/// Seconds of the silence timeout still to run, once the room has been quiet
/// long enough for the countdown to show. `None` when nothing is counting down.
fn silence_remaining(silence: f32, timeout: f32) -> Option<f32> {
    (timeout > 0.0 && silence >= COUNTDOWN_AFTER).then(|| (timeout - silence).max(0.0))
}

/// The status line while recording.
fn listening(secs: u32) -> String {
    format!("Listening  {}", mm_ss(secs))
}

/// The last `max_chars` or so of `text`, cut at a word boundary. The newest
/// words are the ones worth showing, so the start is what gets dropped.
fn tail(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let skip = text.chars().count() - max_chars;
    let cut: String = text.chars().skip(skip).collect();
    // Prefer starting at a word boundary, as long as one is close by.
    let start = cut.find(' ').filter(|at| *at < 24).map_or(0, |at| at + 1);
    format!("\u{2026}{}", &cut[start..])
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

/// Start a capture on the process that is already running, for a launch that
/// could not simply become it.
pub fn start_capture(typed: bool, copied: bool) {
    let Some(running) = RUNNING.with(|running| running.borrow().clone()) else {
        return;
    };
    running.type_this_capture.set(typed);
    running.copy_this_capture.set(copied);
    running.reopen();
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
    use glib::Propagation::{Proceed, Stop};

    let controller = gtk::ShortcutController::new();
    controller.set_scope(gtk::ShortcutScope::Global);

    let bind = |trigger: &str, action: fn(&Rc<App>) -> glib::Propagation| {
        let app_state = Rc::clone(app_state);
        controller.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string(trigger),
            Some(gtk::CallbackAction::new(move |_, _| action(&app_state))),
        ));
    };

    bind("<Control>space", |app| {
        app.toggle();
        Stop
    });

    // Escape throws the recording away; Enter or Space keeps it. Two explicit
    // endings, so neither can happen by accident.
    bind("Escape", |app| {
        if app.quick {
            app.dismiss();
            Stop
        } else if app.is_recording() {
            app.cancel_recording();
            Stop
        } else {
            Proceed
        }
    });

    bind("Return|KP_Enter|space", |app| {
        if app.is_recording() {
            app.stop_recording();
            Stop
        } else {
            // Not recording: leave Enter and Space to the focused widget.
            Proceed
        }
    });

    // Ctrl+C rather than Enter: it works wherever the focus happens to be, and
    // it cannot swallow Enter from a focused button the way a bare Return can.
    bind("<Control>c", |app| {
        app.copy_selected();
        Stop
    });

    bind("Delete", |app| {
        app.delete_selected();
        Stop
    });

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_transcript_is_shown_whole() {
        assert_eq!(tail("hello there", 50), "hello there");
        assert_eq!(tail("  padded  ", 50), "padded");
    }

    #[test]
    fn a_long_transcript_keeps_its_tail() {
        let text = "one two three four five six seven eight nine ten";
        let shown = tail(text, 20);
        assert!(shown.starts_with('\u{2026}'), "{shown}");
        assert!(text.ends_with(shown.trim_start_matches('\u{2026}')), "{shown}");
        // Cut at a word boundary rather than mid-word.
        assert!(!shown.trim_start_matches('\u{2026}').starts_with(' '));
        assert!(shown.chars().count() <= 22, "{shown}");
    }

    #[test]
    fn multibyte_text_is_not_split_mid_character() {
        let text = "café naïve résumé über schön mañana";
        let shown = tail(text, 10);
        assert!(text.ends_with(shown.trim_start_matches('\u{2026}')), "{shown}");
    }
}

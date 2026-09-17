//! The model picker: choose a model, set its language, download or delete one.
//!
//! Every model yapper can run is a row with its size and its languages on it,
//! and picking one is a single click whether or not it is already on the disk.
//!
//! A model's language lives here too, on its own row, because the codes that
//! mean anything are the model's own: Canary has to be told one of its four and
//! cannot detect, SenseVoice knows six and will not load if asked for a
//! seventh, and Parakeet and Moonshine have nothing to be told. One box of ISO
//! codes for all of them could only ever be wrong for most of them.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use crate::config::{Config, models_dir};
use crate::models::{self, Download, Model, Outcome};

/// What the picker asks the window to remember. Both arrive as they happen:
/// there is no OK button to forget.
pub enum Change {
    /// Transcribe with this model from now on.
    Model(&'static Model),
    /// This model should listen for `hears` and write `writes` — the same
    /// unless it is translating.
    Language {
        model: &'static Model,
        hears: &'static str,
        writes: &'static str,
    },
}

pub type OnChange = Rc<dyn Fn(Change)>;

pub fn present(parent: &impl IsA<gtk::Widget>, config: &Config, on_change: OnChange) {
    let dialog = adw::PreferencesDialog::builder()
        .title("Models")
        .content_width(680)
        .content_height(720)
        .build();

    let picker = Rc::new(Picker {
        config: RefCell::new(config.clone()),
        on_change,
        rows: RefCell::new(Vec::new()),
        dialog: dialog.clone(),
    });

    let page = adw::PreferencesPage::new();
    page.add(&group(&picker));
    page.add(&where_group());

    dialog.add(&page);
    dialog.present(Some(parent));
}

struct Picker {
    /// The picker's own copy, so a row can say what its model has been told
    /// without asking the window back.
    config: RefCell<Config>,
    on_change: OnChange,
    rows: RefCell<Vec<Rc<Row>>>,
    dialog: adw::PreferencesDialog,
}

impl Picker {
    fn choose(&self, model: &'static Model) {
        self.config.borrow_mut().model = model.id.to_string();
        (self.on_change)(Change::Model(model));
        self.refresh();
    }

    fn set_language(&self, model: &'static Model, hears: &'static str, writes: &'static str) {
        self.config.borrow_mut().set_language(model, hears, writes);
        (self.on_change)(Change::Language {
            model,
            hears,
            writes,
        });
        self.refresh();
    }

    /// Bring every row back in line with the disk and the settings: the tick on
    /// the chosen one, the right buttons on the rest.
    fn refresh(&self) {
        let config = self.config.borrow();
        for row in self.rows.borrow().iter() {
            row.refresh(&config);
        }
    }

    fn say(&self, message: &str) {
        self.dialog.add_toast(adw::Toast::new(message));
    }
}

/// One model: the tick, the description, its language, and whichever buttons
/// apply.
struct Row {
    model: &'static Model,
    row: adw::ActionRow,
    tick: gtk::Image,
    /// Shows the language the model has been told, and opens the dropdowns
    /// behind it. Absent for the models with nothing to choose.
    language: Option<gtk::MenuButton>,
    /// Hidden on the model in use, where it would do nothing.
    use_it: gtk::Button,
    buttons: gtk::Stack,
    progress: gtk::ProgressBar,
    download: RefCell<Option<Download>>,
}

/// The pages of the button stack, by name.
const GET: &str = "get";
const BUSY: &str = "busy";
const HAVE: &str = "have";

impl Row {
    fn installed(&self) -> bool {
        models::is_installed(self.model)
    }

    fn refresh(&self, config: &Config) {
        let in_use = self.model.id == config.model;
        self.tick.set_visible(in_use);
        self.use_it.set_visible(!in_use);

        if self.download.borrow().is_some() {
            self.buttons.set_visible_child_name(BUSY);
        } else if self.installed() {
            self.buttons.set_visible_child_name(HAVE);
        } else {
            self.buttons.set_visible_child_name(GET);
        }

        self.row.set_subtitle(&describe(self.model));
        if let Some(button) = &self.language {
            button.set_label(&language_label(config, self.model));
        }
        // The one in use is the row you should not have to hunt for.
        if in_use {
            self.row.add_css_class("accent");
        } else {
            self.row.remove_css_class("accent");
        }
    }
}

/// The line under a model's name: who made it, what it costs, what it speaks,
/// and the one thing worth knowing before choosing it.
fn describe(model: &Model) -> String {
    let size = if models::is_installed(model) {
        format!(
            "{} on disk",
            models::human_size(models::size_on_disk(model))
        )
    } else {
        format!("{} to download", models::human_size(model.bytes))
    };
    format!(
        "{} · {size} · {}\n{}",
        model.maker, model.languages, model.note
    )
}

/// What the language button says: the language, the pair when it is writing a
/// different one, or that it is left to decide.
fn language_label(config: &Config, model: &Model) -> String {
    let (hears, writes) = config.language(model);
    if hears == "auto" {
        return "Detect".to_string();
    }
    let heard = model.engine.language_name(hears);
    if hears == writes {
        heard.to_string()
    } else {
        format!("{heard} → {}", model.engine.language_name(writes))
    }
}

fn group(picker: &Rc<Picker>) -> adw::PreferencesGroup {
    // No title: the dialog is already called Models, and saying it twice is
    // two lines of nothing.
    let group = adw::PreferencesGroup::builder()
        .description(
            "All of them run on this machine, through sherpa-onnx, on the CPU, and all of them \
             punctuate and capitalise as they go. Downloading one keeps it until you delete it.",
        )
        .build();

    for model in models::CATALOGUE {
        group.add(&model_row(picker, model));
    }
    group
}

fn model_row(picker: &Rc<Picker>, model: &'static Model) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(model.name)
        .subtitle(describe(model))
        .subtitle_lines(3)
        .activatable(true)
        .build();

    let tick = gtk::Image::builder()
        .icon_name("object-select-symbolic")
        .visible(false)
        .build();
    row.add_prefix(&tick);

    let progress = gtk::ProgressBar::builder()
        .width_request(120)
        .valign(gtk::Align::Center)
        .show_text(true)
        .build();

    let download = gtk::Button::builder()
        .label("Download")
        .valign(gtk::Align::Center)
        .build();
    let use_it = gtk::Button::builder()
        .label("Use")
        .valign(gtk::Align::Center)
        .build();
    let delete = gtk::Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text("Delete the downloaded files")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    let stop = gtk::Button::builder()
        .icon_name("process-stop-symbolic")
        .tooltip_text("Stop downloading")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();

    let busy = gtk::Box::builder().spacing(6).build();
    busy.append(&progress);
    busy.append(&stop);
    let have = gtk::Box::builder().spacing(6).build();
    have.append(&use_it);
    have.append(&delete);

    let buttons = gtk::Stack::builder()
        .valign(gtk::Align::Center)
        .hhomogeneous(false)
        .build();
    buttons.add_named(&download, Some(GET));
    buttons.add_named(&busy, Some(BUSY));
    buttons.add_named(&have, Some(HAVE));

    let language = language_button(picker, model);
    if let Some(button) = &language {
        row.add_suffix(button);
    }
    row.add_suffix(&buttons);

    let state = Rc::new(Row {
        model,
        row: row.clone(),
        tick,
        language,
        use_it: use_it.clone(),
        buttons,
        progress,
        download: RefCell::new(None),
    });

    download.connect_clicked({
        let picker = Rc::clone(picker);
        let state = Rc::clone(&state);
        move |_| start(&picker, &state)
    });
    use_it.connect_clicked({
        let picker = Rc::clone(picker);
        move |_| picker.choose(model)
    });
    delete.connect_clicked({
        let picker = Rc::clone(picker);
        let state = Rc::clone(&state);
        move |_| confirm_delete(&picker, &state)
    });
    stop.connect_clicked({
        let state = Rc::clone(&state);
        move |_| {
            if let Some(download) = state.download.borrow().as_ref() {
                download.cancel();
            }
        }
    });

    // Activating the row does the obvious thing: use it if it is here, fetch it
    // if it is not, and nothing at all while it is on its way.
    row.connect_activated({
        let picker = Rc::clone(picker);
        let state = Rc::clone(&state);
        move |_| {
            if state.download.borrow().is_some() {
                return;
            }
            if state.installed() {
                picker.choose(state.model);
            } else {
                start(&picker, &state);
            }
        }
    });

    state.refresh(&picker.config.borrow());
    picker.rows.borrow_mut().push(state);
    row
}

/// The language control: a button showing the current choice, with the
/// dropdowns in a popover behind it.
///
/// A popover rather than two dropdowns in the row, because Canary needs both a
/// language to listen for and one to write, and four widgets abreast leaves no
/// room for the model's own description.
fn language_button(picker: &Rc<Picker>, model: &'static Model) -> Option<gtk::MenuButton> {
    let codes = model.engine.languages();
    if codes.is_empty() {
        return None;
    }

    let (hears_now, writes_now) = picker.config.borrow().language(model);
    let names: Vec<&str> = codes.iter().map(|(_, name)| *name).collect();
    let index_of = |wanted: &str| {
        codes
            .iter()
            .position(|(code, _)| *code == wanted)
            .unwrap_or(0) as u32
    };

    let hears = gtk::DropDown::from_strings(&names);
    hears.set_selected(index_of(hears_now));

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();

    // Only Canary can write a language it did not hear, so only Canary is
    // asked two questions.
    let writes = model.engine.can_translate().then(|| {
        let writes = gtk::DropDown::from_strings(&names);
        writes.set_selected(index_of(writes_now));
        writes
    });

    let line = |label: &str, dropdown: &gtk::DropDown| {
        let line = gtk::Box::builder().spacing(10).build();
        line.append(
            &gtk::Label::builder()
                .label(label)
                .width_chars(6)
                .xalign(0.0)
                .build(),
        );
        line.append(dropdown);
        line
    };

    if writes.is_some() {
        content.append(&line("Hears", &hears));
    } else {
        content.append(&hears);
    }
    if let Some(writes) = &writes {
        content.append(&line("Writes", writes));
        content.append(
            &gtk::Label::builder()
                .label("Writing a different language is how this model translates")
                .css_classes(["caption", "dim-label"])
                .wrap(true)
                .max_width_chars(28)
                .xalign(0.0)
                .build(),
        );
    }

    // Built before the handlers so choosing a language can dismiss it. Left to
    // itself a popover stays up until you click away, which made picking a
    // language feel like it had not taken.
    let popover = gtk::Popover::builder().child(&content).build();

    // Either dropdown changing sends both values, so the pair is always saved
    // as the model was actually asked for.
    let remember = {
        let picker = Rc::clone(picker);
        let hears = hears.clone();
        let writes = writes.clone();
        let popover = popover.clone();
        move || {
            // GTK answers with an invalid position when nothing is selected,
            // which must not be an index into the list.
            let pick = |dropdown: &gtk::DropDown| {
                codes
                    .get(dropdown.selected() as usize)
                    .map(|(code, _)| *code)
                    .unwrap_or_else(|| models::clamp_language(model.engine, ""))
            };
            let heard = pick(&hears);
            let written = writes.as_ref().map(pick).unwrap_or(heard);
            picker.set_language(model, heard, written);
            // The button underneath now reads the answer, so the popover has
            // nothing left to say. Canary's other dropdown is one click away
            // again, which beats never being sure whether the choice landed.
            popover.popdown();
        }
    };
    hears.connect_selected_notify({
        let remember = remember.clone();
        move |_| remember()
    });
    if let Some(writes) = &writes {
        writes.connect_selected_notify(move |_| remember());
    }

    let button = gtk::MenuButton::builder()
        .label(&language_label(&picker.config.borrow(), model))
        .tooltip_text(if model.engine.can_translate() {
            "The language this model listens for, and the one it writes"
        } else {
            "The language this model listens for"
        })
        .valign(gtk::Align::Center)
        .popover(&popover)
        .build();
    Some(button)
}

/// Start a download, and keep the row's progress bar honest until it ends.
fn start(picker: &Rc<Picker>, state: &Rc<Row>) {
    let model = state.model;
    let download = match models::fetch(model) {
        Ok(download) => download,
        Err(err) => {
            picker.say(&format!("{err:#}"));
            return;
        }
    };
    let finished = download.finished.clone();
    state.progress.set_fraction(0.0);
    state.progress.set_text(Some("0%"));
    *state.download.borrow_mut() = Some(download);
    state.refresh(&picker.config.borrow());

    // The bar reads the staging directory rather than curl's own reporting, so
    // it stays right across the file-by-file fetch and any retries.
    gtk::glib::timeout_add_local(std::time::Duration::from_millis(250), {
        let state = Rc::clone(state);
        move || {
            let borrowed = state.download.borrow();
            let Some(download) = borrowed.as_ref() else {
                return gtk::glib::ControlFlow::Break;
            };
            let fraction = download.fraction();
            state.progress.set_fraction(fraction);
            state
                .progress
                .set_text(Some(&format!("{:.0}%", fraction * 100.0)));
            gtk::glib::ControlFlow::Continue
        }
    });

    gtk::glib::spawn_future_local({
        let picker = Rc::clone(picker);
        let state = Rc::clone(state);
        async move {
            let outcome = finished.recv().await;
            state.download.borrow_mut().take();
            match outcome {
                Ok(Outcome::Done) => {
                    // You asked for it, so you get it: switching now saves a
                    // second click, and switching back is one more.
                    picker.say(&format!("{} is ready", model.name));
                    picker.choose(model);
                }
                Ok(Outcome::Cancelled) => {
                    picker.say(&format!("Stopped downloading {}", model.name));
                    picker.refresh();
                }
                Ok(Outcome::Failed(err)) => {
                    eprintln!("yapper: downloading {}: {err}", model.name);
                    picker.say(&format!("Could not download {}", model.name));
                    picker.refresh();
                }
                // The worker went away without a word, which should not happen
                // but is not worth a crash.
                Err(_) => picker.refresh(),
            }
        }
    });
}

/// Deleting is a click that costs a re-download of up to 670 MB, so it asks.
fn confirm_delete(picker: &Rc<Picker>, state: &Rc<Row>) {
    let model = state.model;
    let in_use = picker.config.borrow().model == model.id;
    let alert = adw::AlertDialog::builder()
        .heading(format!("Delete {}?", model.name))
        .body(if in_use {
            format!(
                "Its {} of files are removed. It is the model in use, so pick another one \
                 afterwards — the one in memory keeps working until yapper restarts.",
                models::human_size(models::size_on_disk(model))
            )
        } else {
            format!(
                "Its {} of files are removed. You can download it again at any time.",
                models::human_size(models::size_on_disk(model))
            )
        })
        .build();
    alert.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
    alert.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    alert.set_default_response(Some("cancel"));
    alert.set_close_response("cancel");

    alert.connect_response(None, {
        let picker = Rc::clone(picker);
        let state = Rc::clone(state);
        move |_, response| {
            if response != "delete" {
                return;
            }
            match models::remove(model) {
                Ok(()) => picker.say(&format!("Deleted {}", model.name)),
                Err(err) => {
                    eprintln!("yapper: {err:#}");
                    picker.say(&format!("Could not delete {}", model.name));
                }
            }
            state.refresh(&picker.config.borrow());
        }
    });
    alert.present(Some(&picker.dialog));
}

/// Where all of this ends up, because a folder full of hundreds of megabytes
/// should say where it is.
fn where_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    let row = adw::ActionRow::builder()
        .title("Models are kept in")
        .subtitle(models_dir().display().to_string())
        .subtitle_lines(2)
        .css_classes(["property"])
        .build();
    group.add(&row);
    group
}

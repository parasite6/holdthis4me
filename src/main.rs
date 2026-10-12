//! holdthis4me: a shelf for files and folders on the top-left of the screen.
//!
//! Touch the top screen border by the corner, or drag something there, and the
//! shelf drops out. Drop files in; later drag them out into any app. The shelf only holds
//! links (src/shelf.rs): nothing is copied and nothing lives in memory.
//!
//! Layer surfaces: an invisible 2 px trigger strip per screen (on the top border), and
//! the shelf, which stays mapped and is slid away, transparent and click-through while
//! hidden.

mod shelf;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gdk, gio, glib, pango};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

const APP_ID: &str = "local.holdthis4me";
const STYLE: &str = include_str!("style.css");

/// The trigger is a 2 px strip on the top screen border itself (never further in, where
/// apps are), exactly as long as the shelf is wide, so touching the border anywhere the
/// shelf will appear opens it at once.
const STRIP_WIDTH: i32 = 2;
/// The shelf's frame margin (style.css .ht4m-frame): the gap to the screen edges.
const FRAME_MARGIN: i32 = 16;
/// Rising back out (style.css .ht4m-frame.away has the matching duration).
const RISE_MS: u64 = 200;
/// After the pointer leaves the shelf it goes away after this long. A drag that leaves
/// (usually: just dropped something) gets longer, so you see the item land.
const LEAVE_MS: u64 = 350;
const AFTER_DROP_MS: u64 = 1200;
/// Opened but never touched (e.g. after the file picker, or --toggle): go away anyway.
const UNTOUCHED_MS: u64 = 2500;
const TOGGLE_MS: u64 = 6000;
/// "Clear" needs a second click within this long.
const CLEAR_ARM_MS: u64 = 2500;

/// The surface's width, including the transparent margin around the frame (room for
/// its shadow, and the gap to the screen edges, like the bar and dock have).
const SHELF_WIDTH: i32 = 372;
/// The surface's height when empty or nearly so; it grows with the items.
const SHELF_MIN_HEIGHT: i32 = 260;

const ICON_SIZE: i32 = 32;
const CLEAR_TIP: &str = "Let go of everything (the files stay where they are)";

fn main() -> glib::ExitCode {
    if std::env::var("WAYLAND_DISPLAY").map_or(true, |v| v.is_empty()) {
        eprintln!("holdthis4me: needs a Wayland session (WAYLAND_DISPLAY is not set)");
        return glib::ExitCode::FAILURE;
    }
    // SAFETY: first thing in main, before GTK or any other thread exists.
    unsafe { std::env::set_var("GDK_BACKEND", "wayland") };

    // `holdthis4me` starts the background process; `holdthis4me --toggle` (a second
    // run) slides the shelf out or back in.
    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();

    let state: Rc<RefCell<Option<Rc<Holder>>>> = Rc::default();
    let slot = state.clone();
    app.connect_startup(move |app| {
        load_css();
        *slot.borrow_mut() = Some(Holder::new(app));
        std::mem::forget(app.hold()); // keep running with the shelf hidden
    });

    app.connect_command_line(move |_, cmd| {
        let holder = state.borrow().clone();
        match cmd.arguments().get(1).and_then(|a| a.to_str()) {
            None => 0.into(),
            Some("--toggle") => {
                if let Some(h) = holder {
                    h.toggle();
                }
                0.into()
            }
            Some(other) => {
                eprintln!("holdthis4me: unknown option {other} (try --toggle)");
                2.into()
            }
        }
    });

    app.run()
}

/// The desktop's colours (ml4w/matugen, regenerated from the wallpaper), the same file
/// the quickshell bar and dock read.
fn theme_path() -> PathBuf {
    glib::user_config_dir().join("ml4w").join("colors").join("colors.json")
}

/// Used when the file is missing or a key isn't in it: the bar's own defaults (Theme.qml).
const THEME_DEFAULTS: [(&str, &str); 7] = [
    ("background", "#1a1110"),
    ("primary", "#ffb4a5"),
    ("on_primary", "#561f13"),
    ("on_background", "#f1dfdb"),
    ("on_surface_variant", "#d8c2bd"),
    ("outline_variant", "#534340"),
    ("shadow", "#000000"),
];

/// `@define-color ht_<key> #rrggbb;` for every colour the stylesheet uses.
fn theme_css(json: Option<&str>) -> String {
    THEME_DEFAULTS
        .iter()
        .map(|(key, fallback)| {
            let value = json.and_then(|j| json_color(j, key)).unwrap_or(fallback);
            format!("@define-color ht_{key} {value};\n")
        })
        .collect()
}

/// The `"#hex"` value of `"key"` in a flat JSON object. Anything that isn't a plain hex
/// colour is ignored (it goes into CSS).
fn json_color<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let at = json.find(&format!("\"{key}\""))? + key.len() + 2;
    let rest = json[at..].trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
    let value = &rest[..rest.find('"')?];
    let hex = value.strip_prefix('#')?;
    (matches!(hex.len(), 6 | 8) && hex.bytes().all(|b| b.is_ascii_hexdigit())).then_some(value)
}

/// Load the stylesheet with the current theme, and again whenever the wallpaper's
/// colours are regenerated.
fn load_css() {
    let provider = gtk::CssProvider::new();
    let apply = {
        let provider = provider.clone();
        move || {
            let json = std::fs::read_to_string(theme_path()).ok();
            provider.load_from_string(&(theme_css(json.as_deref()) + STYLE));
        }
    };
    apply();
    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().expect("no display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    match gio::File::for_path(theme_path()).monitor_file(gio::FileMonitorFlags::WATCH_MOVES, None::<&gio::Cancellable>) {
        Ok(watch) => {
            let debounce: Rc<Timer> = Rc::default();
            watch.connect_changed(move |_, _, _, _| {
                let apply = apply.clone();
                debounce.start(200, apply);
            });
            std::mem::forget(watch); // for the life of the process
        }
        Err(e) => eprintln!("holdthis4me: not watching the theme: {e}"),
    }
}

/// A one-shot timer that can be restarted or cancelled at any time.
#[derive(Default)]
struct Timer(RefCell<Option<glib::SourceId>>);

impl Timer {
    fn start(self: &Rc<Self>, ms: u64, f: impl FnOnce() + 'static) {
        self.cancel();
        let me = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(Duration::from_millis(ms), move || {
            if let Some(t) = me.upgrade() {
                t.0.borrow_mut().take(); // fired: nothing left to cancel
            }
            f();
        });
        *self.0.borrow_mut() = Some(id);
    }

    fn cancel(&self) {
        if let Some(id) = self.0.borrow_mut().take() {
            id.remove();
        }
    }

    fn is_running(&self) -> bool {
        self.0.borrow().is_some()
    }
}

struct Holder {
    app: gtk::Application,
    dir: PathBuf,
    /// Where dropped text and links are saved as small files (see shelf.rs, notes).
    notes: PathBuf,
    shelf: gtk::ApplicationWindow,
    /// What slides: the bordered card (the surface itself stays put).
    frame: gtk::Box,
    /// Holds the per-show start offset of the slide (the shelf's height).
    anim_css: gtk::CssProvider,
    anim_timer: Rc<Timer>,
    list: gtk::Box,
    scroller: gtk::ScrolledWindow,
    empty: gtk::Label,
    count: gtk::Label,
    drag_all: gtk::Box,
    /// Real paths of everything held (what "Drag all" hands out, minus `missing`).
    held: RefCell<Vec<PathBuf>>,
    /// Held targets found to be gone. Filled in by background checks after each reload;
    /// `generation` tells a late check that a newer reload has replaced its row.
    missing: RefCell<HashSet<PathBuf>>,
    generation: Cell<u64>,
    /// Targets whose background check hasn't come back yet. Not checked again until it
    /// does, so a hung mount ties up one worker thread, not one more per opening.
    checking: RefCell<HashSet<PathBuf>>,
    /// The window class of the app the current drag came from (Hyprland's active window
    /// when the drag first reached us); used to label dropped text with its app.
    drag_from: RefCell<Option<String>>,
    /// Window class -> "<icon>\t<name>" of its app, looked up once per class.
    apps_by_class: RefCell<HashMap<String, Option<String>>>,
    /// "Clear" was clicked once and is waiting for the confirming second click.
    clear_armed: Rc<Timer>,
    triggers: RefCell<Vec<gtk::ApplicationWindow>>,
    /// The screen the shelf is on (or was last on).
    monitor: RefCell<Option<gdk::Monitor>>,
    hide_timer: Rc<Timer>,
    /// Out (or on its way out). When it's not, the shelf stays mapped but transparent,
    /// slid away and click-through, so coming out never waits for a new window.
    out: Cell<bool>,
    reload_timer: Rc<Timer>,
    /// One of our items is being dragged out. The shelf must stay mapped until the drag
    /// ends (unmapping the drag source can cancel the drag), and its rows must not be
    /// rebuilt; `stale` remembers that a reload is owed.
    dragging_out: Cell<bool>,
    stale: Cell<bool>,
    _watch: RefCell<Option<gio::FileMonitor>>,
}

impl Holder {
    fn new(app: &gtk::Application) -> Rc<Self> {
        let dir = shelf::default_dir();
        if let Err(e) = shelf::prepare(&dir) {
            // Nothing will be added or removed while this stands (shelf.rs refuses).
            eprintln!("holdthis4me: shelf folder {} unusable: {e}", dir.display());
        }
        let notes = shelf::notes_dir();
        match shelf::prepare(&notes).and_then(|()| shelf::prune_notes(&dir, &notes)) {
            Ok(0) => {}
            Ok(n) => eprintln!("holdthis4me: cleared {n} note(s) no item pointed to"),
            Err(e) => eprintln!("holdthis4me: notes folder {} unusable: {e}", notes.display()),
        }

        let shelf = gtk::ApplicationWindow::builder()
            .application(app)
            .title("holdthis4me")
            .css_classes(["ht4m"])
            .build();
        shelf.init_layer_shell();
        shelf.set_layer(Layer::Overlay);
        shelf.set_keyboard_mode(KeyboardMode::None);
        shelf.set_namespace(Some("holdthis4me"));
        shelf.set_anchor(Edge::Top, true);
        shelf.set_anchor(Edge::Left, true);
        shelf.set_exclusive_zone(-1); // the true corner, over the bar

        let title = gtk::Label::builder().label("hold this").xalign(0.0).css_classes(["ht4m-title"]).build();
        let count = gtk::Label::builder().xalign(0.0).hexpand(true).css_classes(["ht4m-count"]).build();
        let add_files = icon_button("list-add-symbolic", "Add files…");
        let add_folders = icon_button("folder-new-symbolic", "Add folders…");
        let open_shelf = icon_button("folder-open-symbolic", "Open the shelf folder in Files");
        let clear = icon_button("edit-clear-all-symbolic", CLEAR_TIP);
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        header.add_css_class("ht4m-header");
        for w in [title.upcast_ref::<gtk::Widget>(), count.upcast_ref(), add_files.upcast_ref(),
                  add_folders.upcast_ref(), open_shelf.upcast_ref(), clear.upcast_ref()] {
            header.append(w);
        }

        let drag_all = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        drag_all.add_css_class("ht4m-dragall");
        drag_all.set_tooltip_text(Some("Drag from here to drop everything at once"));
        drag_all.append(&gtk::Image::from_icon_name("view-paged-symbolic"));
        drag_all.append(&gtk::Label::new(Some("Drag all")));

        let empty = gtk::Label::builder()
            .label("Drop files, folders, text or links here")
            .wrap(true)
            .justify(gtk::Justification::Center)
            .vexpand(true)
            .css_classes(["ht4m-empty"])
            .build();
        let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .vexpand(true)
            .child(&list)
            .build();

        let card = gtk::Box::new(gtk::Orientation::Vertical, 6);
        card.add_css_class("ht4m-card");
        card.append(&header);
        card.append(&drag_all);
        card.append(&empty);
        card.append(&scroller);
        // Same build as the bar and dock: a gradient frame with the fill inset 2 px.
        let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
        frame.add_css_class("ht4m-frame");
        frame.append(&card);
        shelf.set_child(Some(&frame));
        let anim_css = gtk::CssProvider::new();
        gtk::style_context_add_provider_for_display(
            &gdk::Display::default().expect("no display"),
            &anim_css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
        );

        let h = Rc::new(Holder {
            app: app.clone(),
            dir,
            notes,
            shelf,
            frame,
            anim_css,
            anim_timer: Rc::default(),
            list,
            scroller,
            empty,
            count,
            drag_all,
            held: RefCell::default(),
            missing: RefCell::default(),
            generation: Cell::new(0),
            checking: RefCell::default(),
            clear_armed: Rc::default(),
            drag_from: RefCell::default(),
            apps_by_class: RefCell::default(),
            triggers: RefCell::default(),
            monitor: RefCell::default(),
            hide_timer: Rc::default(),
            out: Cell::new(false),
            reload_timer: Rc::default(),
            dragging_out: Cell::new(false),
            stale: Cell::new(false),
            _watch: RefCell::default(),
        });

        let w = Rc::downgrade(&h);
        add_files.connect_clicked(with(&w, |h| h.pick(false)));
        add_folders.connect_clicked(with(&w, |h| h.pick(true)));
        open_shelf.connect_clicked(with(&w, |h| {
            show_in_files(&h.app, "ShowFolders", &h.dir);
            h.hide();
        }));
        // Two clicks: the first arms it (highlighted), the second within CLEAR_ARM_MS clears.
        clear.connect_clicked(with(&w, |h| h.clear_clicked()));

        h.wire_shelf();
        // GTK may reset the input region when it lays the window out again; re-apply
        // ours after every layout, so the hidden shelf never blocks clicks.
        let w = Rc::downgrade(&h);
        h.shelf.connect_realize(move |shelf| {
            let w = w.clone();
            if let Some(surface) = shelf.surface() {
                surface.connect_layout(move |_, _, _| {
                    if let Some(h) = w.upgrade() {
                        h.set_clickable(h.out.get());
                    }
                });
            }
        });
        h.watch_dir();
        h.watch_monitors();
        h.reload();
        h.toggle_ready();
        h
    }

    /// Pointer and drag tracking on the shelf, drops into it, and the "Drag all" handle.
    fn wire_shelf(self: &Rc<Self>) {
        let w = Rc::downgrade(self);

        let hover = gtk::EventControllerMotion::new();
        hover.connect_enter(move_with(&w, |h| h.hide_timer.cancel()));
        hover.connect_leave(with(&w, |h| h.left(LEAVE_MS)));
        self.shelf.add_controller(hover);

        let drag_over = gtk::DropControllerMotion::new();
        drag_over.connect_enter(move_with(&w, |h| {
            h.hide_timer.cancel();
            h.note_drag_source();
        }));
        drag_over.connect_leave(with(&w, |h| h.left(AFTER_DROP_MS)));
        self.shelf.add_controller(drag_over);

        // On the whole window, including the transparent margin, so a drag at the very
        // screen edge still lands.
        self.shelf.add_controller(self.drop_target());

        let all = gtk::DragSource::new();
        all.set_actions(gdk::DragAction::COPY);
        all.connect_prepare({
            let w = w.clone();
            move |_, _, _| {
                let h = w.upgrade()?;
                let missing = h.missing.borrow();
                let present: Vec<PathBuf> = h.held.borrow().iter().filter(|p| !missing.contains(*p)).cloned().collect();
                (!present.is_empty()).then(|| files_content(&present))
            }
        });
        self.drag_source_events(&all, &self.drag_all.clone().upcast());
        self.drag_all.add_controller(all);
    }

    /// Accepts files and folders, and text or web links (saved as notes). Files come
    /// first: a drag offering both (a file manager) gives files.
    ///
    /// Asks for COPY, but lets MOVE in: Hyprland reports and settles every drag as MOVE,
    /// ignoring what the receiving app asks for, so a COPY-only target takes nothing.
    fn drop_target(self: &Rc<Self>) -> gtk::DropTarget {
        let target = gtk::DropTarget::new(glib::Type::INVALID, gdk::DragAction::COPY | gdk::DragAction::MOVE);
        target.set_types(&[gdk::FileList::static_type(), String::static_type()]);
        // Logged to the journal (`journalctl --user -t holdthis4me`), so a drop that
        // doesn't take can be diagnosed.
        target.connect_accept(|t, drop| {
            let ok = t.formats().is_some_and(|f| drop.formats().match_(&f));
            if !ok {
                eprintln!("holdthis4me: drag refused: offers {} only", drop.formats().to_str());
            }
            ok
        });
        target.connect_enter(|_, _, _| gdk::DragAction::COPY);
        target.connect_motion(|_, _, _| gdk::DragAction::COPY);
        let w = Rc::downgrade(self);
        target.connect_drop(move |t, value, _, _| {
            // Hyprland settles every drag as MOVE whatever we ask for. Taken anyway: we only
            // ever add a link, and file managers don't delete on a MOVE into another app
            // (it's what every drag on Hyprland already is). Logged for the record.
            let settled = t.current_drop().map(|d| d.actions()).unwrap_or(gdk::DragAction::empty());
            eprintln!("holdthis4me: drop, settled as {settled:?}");
            let Some(h) = w.upgrade() else { return false };
            let took = if let Ok(files) = value.get::<gdk::FileList>() {
                // Local files are held as they are; a web address in the list (a link
                // dragged from a browser) becomes a link note.
                let (local, remote): (Vec<gio::File>, Vec<gio::File>) =
                    files.files().into_iter().partition(|f| f.path().is_some());
                let paths: Vec<PathBuf> = local.iter().filter_map(|f| f.path()).collect();
                let links: Vec<shelf::Note> = remote
                    .iter()
                    .map(|f| f.uri().to_string())
                    .filter(|u| shelf::is_link(u))
                    .map(shelf::Note::Link)
                    .collect();
                let a = !paths.is_empty() && h.hold(&paths);
                let b = !links.is_empty() && h.hold_notes(links);
                a || b
            } else if let Ok(text) = value.get::<String>() {
                shelf::classify(&text).is_some_and(|n| h.hold_notes(vec![n]))
            } else {
                false
            };
            if !took {
                eprintln!("holdthis4me: drop of {} not taken", value.type_());
            }
            h.drag_from.take();
            h.reload();
            took
        });
        target
    }

    /// Drag-out bookkeeping shared by item rows and "Drag all": while dragging, the shelf
    /// drops below windows (so they can take the drop) instead of unmapping, then hides
    /// when the drag ends.
    fn drag_source_events(self: &Rc<Self>, source: &gtk::DragSource, icon_of: &gtk::Widget) {
        let w = Rc::downgrade(self);
        let icon_of = icon_of.downgrade();
        source.connect_drag_begin({
            let w = w.clone();
            move |src, _| {
                if let Some(icon) = icon_of.upgrade() {
                    src.set_icon(Some(&gtk::WidgetPaintable::new(Some(&icon)).current_image()), 16, 16);
                }
                if let Some(h) = w.upgrade() {
                    h.dragging_out.set(true);
                    h.hide_timer.cancel();
                    h.shelf.set_layer(Layer::Bottom);
                }
            }
        });
        source.connect_drag_end(move |_, _, _| {
            if let Some(h) = w.upgrade() {
                h.dragging_out.set(false);
                h.hide();
                if h.stale.take() {
                    h.reload();
                }
            }
        });
    }

    /// Changes made outside (links deleted or added in Nautilus) show up while it's open.
    fn watch_dir(self: &Rc<Self>) {
        let file = gio::File::for_path(&self.dir);
        match file.monitor_directory(gio::FileMonitorFlags::NONE, None::<&gio::Cancellable>) {
            Ok(watch) => {
                let w = Rc::downgrade(self);
                watch.connect_changed(move |_, _, _, _| {
                    let Some(h) = w.upgrade() else { return };
                    if h.shown() {
                        let w = Rc::downgrade(&h);
                        h.reload_timer.start(100, move || {
                            if let Some(h) = w.upgrade() {
                                h.reload();
                            }
                        });
                    }
                });
                *self._watch.borrow_mut() = Some(watch);
            }
            Err(e) => eprintln!("holdthis4me: not watching {}: {e}", self.dir.display()),
        }
    }

    /// One trigger strip per screen, rebuilt when screens come and go.
    fn watch_monitors(self: &Rc<Self>) {
        let monitors = gdk::Display::default().expect("no display").monitors();
        let w = Rc::downgrade(self);
        monitors.connect_items_changed(move |_, _, _, _| {
            // Let GDK finish filling in the new monitor's geometry first.
            let w = w.clone();
            glib::idle_add_local_once(move || {
                if let Some(h) = w.upgrade() {
                    h.build_triggers();
                }
            });
        });
        self.build_triggers();
    }

    fn build_triggers(self: &Rc<Self>) {
        for t in self.triggers.borrow_mut().drain(..) {
            t.destroy();
        }
        let monitors = gdk::Display::default().expect("no display").monitors();
        let current: Vec<gdk::Monitor> =
            (0..monitors.n_items()).filter_map(|i| monitors.item(i).and_downcast()).collect();
        let triggers = current
            .iter()
            .map(|m| self.trigger(m))
            .collect();
        *self.triggers.borrow_mut() = triggers;
        // Map the shelf again after the new strip, so it stacks above it, and never on
        // a screen that's gone. (Closes it if it was open.)
        if !self.dragging_out.get() {
            self.out.set(false);
            self.hide_timer.cancel();
            self.anim_timer.cancel();
            self.shelf.set_visible(false);
            // Bound first: a borrow in the `if let` would live through place()'s borrow_mut.
            let current = self.monitor.borrow().clone();
            if let Some(m) = usable_monitor(current) {
                self.place(&m);
            }
        }
    }

    /// The 2 px strip along the top border, as long as the shelf is wide.
    fn trigger(self: &Rc<Self>, monitor: &gdk::Monitor) -> gtk::ApplicationWindow {
        let t = gtk::ApplicationWindow::builder().application(&self.app).css_classes(["ht4m-trigger"]).build();
        t.init_layer_shell();
        // Top, not Overlay: above normal windows, but under fullscreen ones, so a game or
        // video never gets the shelf popping over it (or a surface composited on top).
        t.set_layer(Layer::Top);
        t.set_keyboard_mode(KeyboardMode::None);
        t.set_namespace(Some("holdthis4me-trigger"));
        t.set_anchor(Edge::Top, true);
        t.set_anchor(Edge::Left, true);
        t.set_exclusive_zone(-1);
        t.set_monitor(Some(monitor));
        let (width, height) = (SHELF_WIDTH - FRAME_MARGIN, STRIP_WIDTH);
        // Needs a child of exactly this size: a GTK window without one is 200×200.
        let fill = gtk::Box::new(gtk::Orientation::Vertical, 0);
        fill.set_size_request(width, height);
        t.set_child(Some(&fill));
        t.set_default_size(width, height);

        let w = Rc::downgrade(self);
        // Touching the border opens it, instantly.
        let hover = gtk::EventControllerMotion::new();
        let m = monitor.clone();
        hover.connect_enter(move_with(&w, move |h| {
            if !h.shown() {
                h.show(&m, UNTOUCHED_MS);
            }
        }));
        t.add_controller(hover);

        // Something from another app is being dragged onto the corner or the edge: open
        // right away.
        let drag_over = gtk::DropControllerMotion::new();
        let m = monitor.clone();
        drag_over.connect_enter(move |_, _, _| {
            if let Some(h) = w.upgrade()
                && !h.dragging_out.get()
            {
                h.note_drag_source();
                h.show(&m, UNTOUCHED_MS);
            }
        });
        t.add_controller(drag_over);
        t.add_controller(self.drop_target()); // a drop that lands on the trigger itself

        t.present();
        t
    }

    fn shown(&self) -> bool {
        self.out.get()
    }

    /// Put the (hidden) shelf on `monitor`. Moving a layer surface to another screen
    /// needs a fresh map; on the same screen it stays mapped the whole time.
    fn place(&self, monitor: &gdk::Monitor) {
        if self.shelf.is_visible() && self.monitor.borrow().as_ref() == Some(monitor) {
            return;
        }
        self.shelf.set_visible(false);
        *self.monitor.borrow_mut() = Some(monitor.clone());
        self.shelf.set_monitor(Some(monitor));
        // Hidden, it waits on the Top layer: under fullscreen windows (nothing drawn over a
        // game), but never covered by normal ones. A fully covered surface may stop getting
        // frame callbacks, which would stall the next open.
        self.shelf.set_layer(Layer::Top);
        self.frame.add_css_class("away");
        self.fit();
        self.shelf.present();
        self.set_clickable(false);
    }

    /// Hidden, the surface takes no input at all: clicks and drags go to whatever is
    /// underneath.
    fn set_clickable(&self, clickable: bool) {
        let Some(surface) = self.shelf.surface() else { return };
        // None: the whole surface; an empty region: nothing.
        let nothing = gtk::cairo::Region::create();
        surface.set_input_region((!clickable).then_some(&nothing));
    }

    fn show(self: &Rc<Self>, monitor: &gdk::Monitor, untouched_ms: u64) {
        if !monitor.is_valid() {
            return; // unplugged since the caller looked it up
        }
        let w = Rc::downgrade(self);
        let untouched = move || {
            if let Some(h) = w.upgrade() {
                h.hide();
            }
        };
        self.hide_timer.cancel();
        if self.out.get() && self.monitor.borrow().as_ref() == Some(monitor) {
            self.hide_timer.start(untouched_ms, untouched);
            return;
        }
        self.anim_timer.cancel();
        let fresh = !self.shelf.is_visible() || self.monitor.borrow().as_ref() != Some(monitor);
        self.place(monitor);
        self.shelf.set_layer(Layer::Overlay);
        self.out.set(true);
        self.reload();
        self.set_clickable(true);
        if fresh {
            // Just mapped: drop once its first frame (still above the edge) is drawn.
            let frames = Cell::new(0);
            self.frame.add_tick_callback(move |frame, _| {
                frames.set(frames.get() + 1);
                if frames.get() < 2 {
                    return glib::ControlFlow::Continue;
                }
                frame.remove_css_class("away");
                glib::ControlFlow::Break
            });
        } else {
            self.frame.remove_css_class("away");
        }
        self.hide_timer.start(untouched_ms, untouched);
    }

    /// Rise back up and stop taking input; the surface stays mapped.
    fn hide(&self) {
        self.hide_timer.cancel();
        if self.dragging_out.get() || !self.out.get() {
            return;
        }
        self.out.set(false);
        self.drag_from.take(); // a drag that left without dropping
        self.frame.add_css_class("away");
        self.set_clickable(false);
        if self.clear_armed.is_running() {
            self.clear_armed.cancel();
            let b = self.clear_button();
            b.remove_css_class("armed");
            b.set_tooltip_text(Some(CLEAR_TIP));
        }
        // Once it's out of sight, wait on the Top layer again (see place()).
        let shelf = self.shelf.clone();
        self.anim_timer.start(RISE_MS, move || shelf.set_layer(Layer::Top));
    }

    /// Map the hidden shelf right away on the first screen, so the first open is as
    /// quick as the rest.
    fn toggle_ready(&self) {
        if let Some(m) = usable_monitor(None) {
            self.place(&m);
        }
    }

    /// Hold these paths; true if any were taken (or were already held).
    fn hold(&self, paths: &[PathBuf]) -> bool {
        let mut took = false;
        for (path, result) in paths.iter().zip(shelf::add_all(&self.dir, paths)) {
            match result {
                Ok(_) => took = true,
                Err(e) => eprintln!("holdthis4me: can't hold {}: {e}", path.display()),
            }
        }
        took
    }

    fn clear_clicked(self: &Rc<Self>) {
        let button = self.clear_button();
        if !self.clear_armed.is_running() {
            button.add_css_class("armed");
            button.set_tooltip_text(Some("Click again to let go of everything"));
            let b = button.downgrade();
            self.clear_armed.start(CLEAR_ARM_MS, move || {
                if let Some(b) = b.upgrade() {
                    b.remove_css_class("armed");
                    b.set_tooltip_text(Some(CLEAR_TIP));
                }
            });
            return;
        }
        self.clear_armed.cancel();
        button.remove_css_class("armed");
        button.set_tooltip_text(Some(CLEAR_TIP));
        for item in shelf::list(&self.dir) {
            self.let_go(&item);
        }
        self.reload();
    }

    /// Stop holding an item. For our own notes, their file goes too (it exists only to
    /// be held); for everything else only the link is removed.
    fn let_go(&self, item: &shelf::Item) {
        if let Err(e) = shelf::remove(&self.dir, &item.link) {
            eprintln!("holdthis4me: {}: {e}", item.link.display());
            return;
        }
        if shelf::is_note(&self.notes, &item.target)
            && let Err(e) = shelf::delete_note(&self.notes, &item.target)
        {
            eprintln!("holdthis4me: note {}: {e}", item.target.display());
        }
    }

    /// Save dropped text or links as notes and hold them; true if any were. Text is
    /// labelled with the app it was dragged from, when that's known.
    fn hold_notes(&self, notes: Vec<shelf::Note>) -> bool {
        let source = self.drag_from.borrow().clone().and_then(|class| self.app_for_class(&class));
        let paths: Vec<PathBuf> = notes
            .iter()
            .filter_map(|n| match shelf::write_note(&self.notes, n) {
                Ok(p) => {
                    if let (shelf::Note::Text(_), Some(src)) = (n, &source)
                        && let Err(e) = shelf::set_note_source(&p, src)
                    {
                        eprintln!("holdthis4me: can't label note with its app: {e}");
                    }
                    Some(p)
                }
                Err(e) => {
                    eprintln!("holdthis4me: can't save dropped text: {e}");
                    None
                }
            })
            .collect();
        !paths.is_empty() && self.hold(&paths)
    }

    /// Note which app a drag comes from, the first time it reaches us (the strip or the
    /// shelf). The window you drag from keeps focus during a drag, so it's the active one.
    fn note_drag_source(&self) {
        if self.drag_from.borrow().is_none() && !self.dragging_out.get() {
            *self.drag_from.borrow_mut() = active_window_class();
        }
    }

    /// "<icon>\t<name>" for the installed app with this window class (by its launcher's
    /// StartupWMClass, or a launcher id equal to the class). Cached per class.
    fn app_for_class(&self, class: &str) -> Option<String> {
        if let Some(found) = self.apps_by_class.borrow().get(class) {
            return found.clone();
        }
        let want = class.to_lowercase();
        let all = gio::AppInfo::all();
        let matches = |a: &&gio::AppInfo| {
            let wm = a.downcast_ref::<gio_unix::DesktopAppInfo>().and_then(|d| d.startup_wm_class());
            let id = a.id().map(|i| i.trim_end_matches(".desktop").to_lowercase());
            wm.is_some_and(|w| w.to_lowercase() == want) || id.as_deref() == Some(want.as_str())
        };
        let app = all.iter().filter(|a| a.should_show()).find(matches).or_else(|| all.iter().find(matches));
        let found = app.and_then(|a| {
            let icon = a.icon()?.to_string()?;
            Some(format!("{icon}\t{}", a.display_name()))
        });
        self.apps_by_class.borrow_mut().insert(class.to_owned(), found.clone());
        found
    }

    fn toggle(self: &Rc<Self>) {
        if self.shown() {
            self.hide();
            return;
        }
        // Bound first: a borrow in the `if let` would live through place()'s borrow_mut.
            let current = self.monitor.borrow().clone();
            if let Some(m) = usable_monitor(current) {
            self.show(&m, TOGGLE_MS);
        }
    }

    /// The pointer (or a drag) left the shelf.
    fn left(self: &Rc<Self>, ms: u64) {
        if self.dragging_out.get() || !self.shown() {
            return;
        }
        let w = Rc::downgrade(self);
        self.hide_timer.start(ms, move || {
            if let Some(h) = w.upgrade() {
                h.hide();
            }
        });
    }

    /// "Upload and navigate yourself": the desktop's file chooser.
    fn pick(self: &Rc<Self>, folders: bool) {
        let monitor = self.monitor.borrow().clone();
        self.hide(); // the chooser goes over everything else, but not over the shelf
        let dialog = gtk::FileDialog::builder()
            .title(if folders { "Hold these folders" } else { "Hold these files" })
            .modal(false)
            .build();
        let w = Rc::downgrade(self);
        let done = move |result: Result<gio::ListModel, glib::Error>| {
            let (Some(h), Ok(chosen)) = (w.upgrade(), result) else { return };
            let paths: Vec<PathBuf> = (0..chosen.n_items())
                .filter_map(|i| chosen.item(i).and_downcast::<gio::File>())
                .filter_map(|f| f.path())
                .collect();
            h.hold(&paths);
            // The screen may have gone while the chooser was open.
            if let Some(m) = usable_monitor(monitor.clone()) {
                h.show(&m, UNTOUCHED_MS); // a glimpse of what was added
            }
        };
        if folders {
            dialog.select_multiple_folders(None::<&gtk::Window>, None::<&gio::Cancellable>, done);
        } else {
            dialog.open_multiple(None::<&gtk::Window>, None::<&gio::Cancellable>, done);
        }
    }

    /// Rebuild the rows from the shelf folder and resize to fit.
    fn reload(self: &Rc<Self>) {
        if self.dragging_out.get() {
            self.stale.set(true);
            return;
        }
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        self.generation.set(self.generation.get() + 1);
        self.missing.borrow_mut().clear();
        let items = shelf::list(&self.dir);
        for item in &items {
            self.list.append(&self.row(item));
        }
        let held: Vec<PathBuf> = items.iter().map(|i| i.target.clone()).collect();
        // A refused shelf folder lists nothing and takes nothing: say why, on the shelf.
        self.empty.set_label(&match shelf::usable(&self.dir) {
            Ok(()) => "Drop files, folders, text or links here".to_string(),
            Err(e) => format!("Shelf folder unusable:\n{e}\n{}", tilde(&self.dir)),
        });
        self.empty.set_visible(items.is_empty());
        self.scroller.set_visible(!items.is_empty());
        self.drag_all.set_visible(held.len() > 1);
        self.count.set_label(&match items.len() {
            0 => String::new(),
            n => n.to_string(),
        });
        *self.held.borrow_mut() = held;
        self.fit();
    }

    /// At least SHELF_MIN_HEIGHT, taller as items pile up, scrolling past 80% of the
    /// screen. The slide starts exactly one shelf-height above the edge.
    fn fit(&self) {
        let Some(m) = self.monitor.borrow().clone() else { return };
        let screen = m.geometry().height();
        let max = (screen as f64 * 0.8) as i32;
        self.scroller.set_max_content_height(max);
        let (_, natural, _, _) = self.shelf.child().expect("frame").measure(gtk::Orientation::Vertical, SHELF_WIDTH);
        let height = natural.clamp(SHELF_MIN_HEIGHT, max);
        self.shelf.set_default_size(SHELF_WIDTH, height);
        self.anim_css.load_from_string(&format!(".ht4m-frame.away {{ transform: translateY(-{height}px); }}"));
    }

    /// One held item. It's drawn straight away as present with a generic icon; a
    /// background check then fills in its real icon or thumbnail, or marks it missing.
    /// (Checking on the spot would hang the whole shelf on a dead network mount.)
    fn row(self: &Rc<Self>, item: &shelf::Item) -> gtk::Box {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        row.add_css_class("ht4m-item");
        row.set_tooltip_text(Some(&item.target.to_string_lossy()));

        // Our own notes (dropped text and links) show what they say, not their file name.
        // They're small local files, so reading one here can't hang.
        let note = shelf::is_note(&self.notes, &item.target).then(|| shelf::read_note(&self.notes, &item.target)).flatten();
        // Text shows the icon of the app it was dragged from, when that was recorded.
        let source = matches!(note, Some(shelf::Note::Text(_)))
            .then(|| shelf::note_source(&item.target))
            .flatten()
            .and_then(|s| s.split_once('\t').map(|(i, n)| (i.to_owned(), n.to_owned())));
        let icon = gtk::Image::from_icon_name(match &note {
            Some(shelf::Note::Link(_)) => "insert-link-symbolic",
            _ => "text-x-generic",
        });
        if let Some(gicon) = source.as_ref().and_then(|(i, _)| gio::Icon::for_string(i).ok()) {
            icon.set_from_gicon(&gicon);
        }
        icon.set_pixel_size(ICON_SIZE);
        icon.add_css_class("ht4m-icon");
        let title = match &note {
            Some(shelf::Note::Link(u)) => u.clone(),
            Some(shelf::Note::Text(t)) => t.trim().lines().next().unwrap_or("").chars().take(200).collect(),
            None => item.target.file_name().map_or_else(|| item.target.to_string_lossy(), |n| n.to_string_lossy()).into_owned(),
        };
        let name = gtk::Label::builder()
            .label(title.as_str())
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::Middle)
            .css_classes(["ht4m-name"])
            .build();
        let parent = match &note {
            Some(shelf::Note::Link(_)) => "link".to_owned(),
            Some(shelf::Note::Text(t)) => match &source {
                Some((_, app)) => format!("text from {app} · {} characters", t.chars().count()),
                None => format!("text · {} characters", t.chars().count()),
            },
            None => item.target.parent().map(tilde).unwrap_or_default(),
        };
        let place = gtk::Label::builder()
            .label(parent.as_str())
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::Start)
            .css_classes(["ht4m-place"])
            .build();
        let texts = gtk::Box::new(gtk::Orientation::Vertical, 0);
        texts.set_hexpand(true);
        texts.set_valign(gtk::Align::Center);
        texts.append(&name);
        texts.append(&place);
        row.append(&icon);
        row.append(&texts);

        let w = Rc::downgrade(self);
        let reveal = icon_button("find-location-symbolic", "Show in Files");
        reveal.add_css_class("ht4m-itembtn");
        let target = item.target.clone();
        reveal.connect_clicked(with(&w, move |h| {
            show_in_files(&h.app, "ShowItems", &target);
            h.hide();
        }));
        row.append(&reveal);
        let let_go = icon_button("window-close-symbolic", "Let go (the file stays where it is)");
        let_go.add_css_class("ht4m-itembtn");
        let held = shelf::Item { link: item.link.clone(), target: item.target.clone() };
        let_go.connect_clicked(with(&w, move |h| {
            h.let_go(&held);
            h.reload();
        }));
        row.append(&let_go);

        // Drag out and double-click open, unless the check finds it gone.
        let source = gtk::DragSource::new();
        source.set_actions(gdk::DragAction::COPY);
        let target = item.target.clone();
        let wp = w.clone();
        source.connect_prepare(move |_, _, _| {
            let h = wp.upgrade()?;
            if h.missing.borrow().contains(&target) {
                return None;
            }
            let file = files_content(std::slice::from_ref(&target));
            // A note offers its text (or address) first, so a text box or chat gets the
            // words themselves; apps that only take files still get the file.
            Some(match shelf::read_note(&h.notes, &target) {
                Some(shelf::Note::Link(t) | shelf::Note::Text(t)) => {
                    gdk::ContentProvider::new_union(&[gdk::ContentProvider::for_value(&t.to_value()), file])
                }
                None => file,
            })
        });
        self.drag_source_events(&source, icon.upcast_ref());
        row.add_controller(source);

        let click = gtk::GestureClick::new();
        let target = item.target.clone();
        let wc = w.clone();
        click.connect_pressed(move |_, n, _, _| {
            let Some(h) = wc.upgrade() else { return };
            if n == 2 && !h.missing.borrow().contains(&target) {
                match shelf::read_note(&h.notes, &target) {
                    Some(shelf::Note::Link(url)) => open_uri(&url),
                    _ => open(&target),
                }
                h.hide();
            }
        });
        row.add_controller(click);

        if !self.checking.borrow_mut().insert(item.target.clone()) {
            return row; // an earlier check is still out; its result triggers a fresh reload
        }
        let generation = self.generation.get();
        let target = item.target.clone();
        let (row_w, icon_w, place_w, reveal_w) = (row.downgrade(), icon.downgrade(), place.downgrade(), reveal.downgrade());
        gio::File::for_path(&item.target).query_info_async(
            "standard::icon,thumbnail::path,thumbnail::is-valid",
            gio::FileQueryInfoFlags::NONE,
            glib::Priority::DEFAULT_IDLE,
            None::<&gio::Cancellable>,
            move |result| {
                let Some(h) = w.upgrade() else { return };
                h.checking.borrow_mut().remove(&target);
                if h.generation.get() != generation {
                    // Rebuilt since: this row is gone, and the new one skipped its check
                    // while this one was out. Rebuild once more so it gets one.
                    if h.shown() {
                        let w = Rc::downgrade(&h);
                        h.reload_timer.start(100, move || {
                            if let Some(h) = w.upgrade() {
                                h.reload();
                            }
                        });
                    }
                    return;
                }
                let (Some(row), Some(icon), Some(place), Some(reveal)) =
                    (row_w.upgrade(), icon_w.upgrade(), place_w.upgrade(), reveal_w.upgrade())
                else {
                    return;
                };
                match result {
                    Ok(_) if shelf::is_note(&h.notes, &target) => {} // keeps its link/text icon
                    Ok(info) => set_icon(&icon, &info),
                    Err(e) if e.matches(gio::IOErrorEnum::NotFound) => {
                        h.missing.borrow_mut().insert(target.clone());
                        row.add_css_class("missing");
                        icon.set_icon_name(Some("dialog-question-symbolic"));
                        place.set_label(&format!("missing (was in {parent})"));
                        reveal.set_visible(false);
                        let present = h.held.borrow().iter().filter(|p| !h.missing.borrow().contains(*p)).count();
                        h.drag_all.set_visible(present > 1);
                    }
                    Err(_) => {} // unreadable, not gone: keep the generic icon
                }
            },
        );
        row
    }

    fn clear_button(&self) -> gtk::Button {
        // The header's last child (see new()).
        let card = self.frame.first_child().expect("card");
        let header = card.first_child().expect("header");
        header.last_child().and_downcast::<gtk::Button>().expect("clear button")
    }
}

/// `connect_clicked`-style handler that runs `f` on the holder if it's still alive.
fn with<T>(w: &Weak<Holder>, f: impl Fn(&Rc<Holder>) + 'static) -> impl Fn(&T) + 'static {
    let w = w.clone();
    move |_| {
        if let Some(h) = w.upgrade() {
            f(&h);
        }
    }
}

/// Same for enter handlers, which also get the pointer position.
fn move_with<T>(w: &Weak<Holder>, f: impl Fn(&Rc<Holder>) + 'static) -> impl Fn(&T, f64, f64) + 'static {
    let w = w.clone();
    move |_, _, _| {
        if let Some(h) = w.upgrade() {
            f(&h);
        }
    }
}

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .valign(gtk::Align::Center)
        .css_classes(["ht4m-btn"])
        .build()
}

/// The thumbnail Nautilus already made for it if there is one, else its type's icon.
fn set_icon(image: &gtk::Image, info: &gio::FileInfo) {
    let valid = !info.has_attribute("thumbnail::is-valid") || info.boolean("thumbnail::is-valid");
    match (info.attribute_byte_string("thumbnail::path").filter(|_| valid), info.icon()) {
        (Some(t), _) => image.set_from_file(Some(t.as_str())),
        (None, Some(icon)) => image.set_from_gicon(&icon),
        (None, None) => {}
    }
}

/// The class of Hyprland's active window, straight from its IPC socket (no hyprctl
/// process). None outside Hyprland or if it doesn't answer within 200 ms.
fn active_window_class() -> Option<String> {
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok().filter(|s| !s.is_empty() && !s.contains('/'))?;
    let socket = glib::user_runtime_dir().join("hypr").join(sig).join(".socket.sock");
    let mut stream = UnixStream::connect(socket).ok()?;
    let limit = Some(Duration::from_millis(200));
    stream.set_read_timeout(limit).ok()?;
    stream.set_write_timeout(limit).ok()?;
    stream.write_all(b"j/activewindow").ok()?;
    let mut reply = String::new();
    stream.take(64 * 1024).read_to_string(&mut reply).ok()?;
    json_str(&reply, "class").filter(|c| !c.is_empty())
}

/// The string value of `"key"` in a JSON object (first occurrence), unescaped for the
/// simple escapes window classes could contain.
fn json_str(json: &str, key: &str) -> Option<String> {
    let at = json.find(&format!("\"{key}\""))? + key.len() + 2;
    let rest = json[at..].trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => out.push(chars.next()?),
            c => out.push(c),
        }
    }
    None
}

/// The shelf's screen if it's still connected, else the first one there is.
fn usable_monitor(preferred: Option<gdk::Monitor>) -> Option<gdk::Monitor> {
    preferred.filter(|m| m.is_valid()).or_else(|| {
        let monitors = gdk::Display::default()?.monitors();
        monitors.item(0).and_downcast::<gdk::Monitor>()
    })
}

/// What a drag hands to the app it's dropped on: the real files (as text/uri-list, plus
/// the document portal for sandboxed apps; GTK does both from a FileList).
fn files_content(paths: &[PathBuf]) -> gdk::ContentProvider {
    let files: Vec<gio::File> = paths.iter().map(gio::File::for_path).collect();
    gdk::ContentProvider::for_value(&gdk::FileList::from_array(&files).to_value())
}

/// Open in its default app. Asynchronous: finding the app reads the file, which could
/// hang on a dead mount.
fn open(path: &Path) {
    open_uri(&gio::File::for_path(path).uri());
}

/// Open a file URI or (for a link note) a web address in its default app.
fn open_uri(uri: &str) {
    let ctx = gdk::Display::default().map(|d| d.app_launch_context());
    let what = uri.to_owned();
    gio::AppInfo::launch_default_for_uri_async(uri, ctx.as_ref(), None::<&gio::Cancellable>, move |result| {
        if let Err(e) = result {
            eprintln!("holdthis4me: can't open {what}: {e}");
        }
    });
}

/// Ask the file manager (Nautilus) to show a file selected in its folder (`ShowItems`)
/// or to open a folder (`ShowFolders`).
fn show_in_files(app: &gtk::Application, method: &str, path: &Path) {
    let uri = gio::File::for_path(path).uri().to_string();
    let Some(bus) = app.dbus_connection() else { return };
    let path = path.to_owned();
    bus.call(
        Some("org.freedesktop.FileManager1"),
        "/org/freedesktop/FileManager1",
        "org.freedesktop.FileManager1",
        method,
        Some(&(vec![uri], "").to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        5000,
        None::<&gio::Cancellable>,
        move |result| {
            if let Err(e) = result {
                eprintln!("holdthis4me: file manager didn't answer for {}: {e}", path.display());
            }
        },
    );
}

/// `/home/me/Downloads` → `~/Downloads`.
fn tilde(path: &Path) -> String {
    let home = glib::home_dir();
    match path.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_window_class_from_hyprland_json() {
        let reply = r#"{"address": "0x1", "initialClass": "wrong", "class": "brave-browser", "title": "a \"q\""}"#;
        assert_eq!(json_str(reply, "class").as_deref(), Some("brave-browser"));
        assert_eq!(json_str(reply, "title").as_deref(), Some("a \"q\""));
        assert_eq!(json_str("{}", "class"), None);
    }

    #[test]
    fn theme_reads_colours_and_falls_back() {
        let json = r##"{ "background": "#1a110e", "on_primary": "#561f13", "inverse_primary": "#000000",
            "primary" : "#FFB4A5", "outline_variant": "red; } * { color: blue" }"##;
        let css = theme_css(Some(json));
        assert!(css.contains("@define-color ht_background #1a110e;"));
        assert!(css.contains("@define-color ht_primary #FFB4A5;"));
        assert!(css.contains("@define-color ht_outline_variant #534340;")); // not a colour: default
        assert!(css.contains("@define-color ht_shadow #000000;")); // missing: default
        assert_eq!(theme_css(None).lines().count(), THEME_DEFAULTS.len());
    }
}

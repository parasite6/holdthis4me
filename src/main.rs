//! holdthis4me: a shelf for files and folders on the top-left of the screen.
//!
//! Hover the very top-left corner, or drag something onto the top part of the left screen
//! edge, and the shelf slides out. Drop files in; later drag them out into any app. The
//! shelf only holds links (src/shelf.rs): nothing is copied and nothing lives in memory.
//!
//! Two layer surfaces per screen: an invisible 2 px strip on the left edge that's always
//! there (the trigger), and the shelf itself, mapped only while it's out.

mod shelf;

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gdk, gio, glib, pango};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

const APP_ID: &str = "local.holdthis4me";
const STYLE: &str = include_str!("style.css");

/// The trigger strip: this wide, and this share of the screen height from the top.
const STRIP_WIDTH: i32 = 2;
const STRIP_SHARE: f64 = 0.45;
/// Hovering opens it only in the corner: this square, after resting in it this long, so
/// passing by doesn't pop the shelf out. It's empty space (the bar reserves the top of
/// the screen; windows never go there). Drags on the corner or the strip open it at once.
const CORNER_ZONE: i32 = 48;
const DWELL_MS: u64 = 150;
/// The drop-in/rise-out animation (style.css has the matching durations).
const RISE_MS: u64 = 200;
const FIRST_FRAME_MS: u64 = 30;
/// After the pointer leaves the shelf it goes away after this long. A drag that leaves
/// (usually: just dropped something) gets longer, so you see the item land.
const LEAVE_MS: u64 = 350;
const AFTER_DROP_MS: u64 = 1200;
/// Opened but never touched (e.g. after the file picker, or --toggle): go away anyway.
const UNTOUCHED_MS: u64 = 2500;
const TOGGLE_MS: u64 = 6000;

/// The surface's width, including the transparent margin around the frame (room for
/// its shadow, and the gap to the screen edges, like the bar and dock have).
const SHELF_WIDTH: i32 = 372;

#[derive(Clone, Copy, PartialEq)]
enum Trigger {
    /// The corner square: hover or drag.
    Corner,
    /// The thin strip down the left edge: drags only.
    Edge,
}
const ICON_SIZE: i32 = 32;

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

    fn running(&self) -> bool {
        self.0.borrow().is_some()
    }
}

struct Holder {
    app: gtk::Application,
    dir: PathBuf,
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
    /// Real paths of everything held that still exists (what "Drag all" hands out).
    held: RefCell<Vec<PathBuf>>,
    triggers: RefCell<Vec<gtk::ApplicationWindow>>,
    /// The screen the shelf is on (or was last on).
    monitor: RefCell<Option<gdk::Monitor>>,
    hide_timer: Rc<Timer>,
    dwell: Rc<Timer>,
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
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("holdthis4me: can't create {}: {e}", dir.display());
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
        let clear = icon_button("edit-clear-all-symbolic", "Let go of everything (the files stay where they are)");
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
            .label("Drop files or folders here")
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
            triggers: RefCell::default(),
            monitor: RefCell::default(),
            hide_timer: Rc::default(),
            dwell: Rc::default(),
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
        clear.connect_clicked(with(&w, |h| {
            for item in shelf::list(&h.dir) {
                if let Err(e) = shelf::remove(&h.dir, &item.link) {
                    eprintln!("holdthis4me: {}: {e}", item.link.display());
                }
            }
            h.reload();
        }));

        h.wire_shelf();
        h.watch_dir();
        h.watch_monitors();
        h.reload();
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
        drag_over.connect_enter(move_with(&w, |h| h.hide_timer.cancel()));
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
                let held = h.held.borrow();
                (!held.is_empty()).then(|| files_content(&held))
            }
        });
        self.drag_source_events(&all, &self.drag_all.clone().upcast());
        self.drag_all.add_controller(all);
    }

    /// Accepts files and folders. Only ever COPY: accepting MOVE would tell the app they
    /// came from (e.g. Nautilus) to delete its originals.
    fn drop_target(self: &Rc<Self>) -> gtk::DropTarget {
        let target = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let w = Rc::downgrade(self);
        target.connect_drop(move |_, value, _, _| {
            let (Some(h), Ok(files)) = (w.upgrade(), value.get::<gdk::FileList>()) else { return false };
            let mut took = false;
            for path in files.files().iter().filter_map(|f| f.path()) {
                match shelf::add(&h.dir, &path) {
                    Ok(_) => took = true,
                    Err(e) => eprintln!("holdthis4me: can't hold {}: {e}", path.display()),
                }
            }
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
                    if h.shelf.is_visible() {
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
        if let Some(m) = self.monitor.borrow().as_ref()
            && !current.contains(m)
        {
            self.shelf.set_visible(false);
        }
        let triggers = current
            .iter()
            .flat_map(|m| [self.trigger(m, Trigger::Corner), self.trigger(m, Trigger::Edge)])
            .collect();
        *self.triggers.borrow_mut() = triggers;
    }

    fn trigger(self: &Rc<Self>, monitor: &gdk::Monitor, kind: Trigger) -> gtk::ApplicationWindow {
        let t = gtk::ApplicationWindow::builder().application(&self.app).css_classes(["ht4m-trigger"]).build();
        t.init_layer_shell();
        t.set_layer(Layer::Overlay);
        t.set_keyboard_mode(KeyboardMode::None);
        t.set_namespace(Some("holdthis4me-trigger"));
        t.set_anchor(Edge::Top, true);
        t.set_anchor(Edge::Left, true);
        t.set_exclusive_zone(-1);
        t.set_monitor(Some(monitor));
        let (width, height) = match kind {
            Trigger::Corner => (CORNER_ZONE, CORNER_ZONE),
            Trigger::Edge => (STRIP_WIDTH, strip_height(monitor)),
        };
        // Needs a child of exactly this size: a GTK window without one is 200×200.
        let fill = gtk::Box::new(gtk::Orientation::Vertical, 0);
        fill.set_size_request(width, height);
        t.set_child(Some(&fill));
        t.set_default_size(width, height);

        let w = Rc::downgrade(self);
        if kind == Trigger::Corner {
            // Resting anywhere in the corner square opens it.
            let hover = gtk::EventControllerMotion::new();
            let m = monitor.clone();
            hover.connect_enter(move_with(&w, move |h| {
                if !h.shown() && !h.dwell.running() {
                    let (w, m) = (Rc::downgrade(h), m.clone());
                    h.dwell.start(DWELL_MS, move || {
                        if let Some(h) = w.upgrade() {
                            h.show(&m, UNTOUCHED_MS);
                        }
                    });
                }
            }));
            hover.connect_leave(with(&w, |h| h.dwell.cancel()));
            t.add_controller(hover);
        }

        // Something from another app is being dragged onto the corner or the edge: open
        // right away.
        let drag_over = gtk::DropControllerMotion::new();
        let m = monitor.clone();
        drag_over.connect_enter(move |_, _, _| {
            if let Some(h) = w.upgrade()
                && !h.dragging_out.get()
            {
                h.show(&m, UNTOUCHED_MS);
            }
        });
        t.add_controller(drag_over);
        t.add_controller(self.drop_target()); // a drop that lands on the trigger itself

        t.present();
        t
    }

    /// Out, and not on its way back in.
    fn shown(&self) -> bool {
        self.shelf.is_visible() && !self.frame.has_css_class("away")
    }

    fn show(self: &Rc<Self>, monitor: &gdk::Monitor, untouched_ms: u64) {
        self.dwell.cancel();
        let w = Rc::downgrade(self);
        let untouched = move || {
            if let Some(h) = w.upgrade() {
                h.hide();
            }
        };
        if self.shelf.is_visible() && self.monitor.borrow().as_ref() == Some(monitor) {
            // Already out, or still rising away: drop back down from where it is.
            self.hide_timer.cancel();
            if self.frame.has_css_class("away") {
                self.anim_timer.cancel();
                self.frame.remove_css_class("away");
                self.hide_timer.start(untouched_ms, untouched);
            }
            return;
        }
        self.anim_timer.cancel();
        self.shelf.set_visible(false);
        *self.monitor.borrow_mut() = Some(monitor.clone());
        self.shelf.set_monitor(Some(monitor));
        self.shelf.set_layer(Layer::Overlay);
        self.reload();
        // Start above the screen's top edge, then let the CSS transition drop it in.
        let height = self.shelf.default_size().1;
        self.anim_css.load_from_string(&format!(".ht4m-frame.away {{ transform: translateY(-{height}px); }}"));
        self.frame.add_css_class("away");
        self.shelf.present();
        let frame = self.frame.clone();
        self.anim_timer.start(FIRST_FRAME_MS, move || frame.remove_css_class("away"));
        self.hide_timer.start(untouched_ms, untouched);
    }

    /// Rise back up, then unmap.
    fn hide(&self) {
        self.hide_timer.cancel();
        if self.dragging_out.get() || !self.shown() {
            return;
        }
        self.frame.add_css_class("away");
        let shelf = self.shelf.clone();
        self.anim_timer.start(RISE_MS, move || shelf.set_visible(false));
    }

    fn toggle(self: &Rc<Self>) {
        if self.shown() {
            self.hide();
            return;
        }
        let first = || {
            let monitors = gdk::Display::default()?.monitors();
            monitors.item(0).and_downcast::<gdk::Monitor>()
        };
        let m = self.monitor.borrow().clone().or_else(first);
        if let Some(m) = m {
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
            for path in (0..chosen.n_items())
                .filter_map(|i| chosen.item(i).and_downcast::<gio::File>())
                .filter_map(|f| f.path())
            {
                if let Err(e) = shelf::add(&h.dir, &path) {
                    eprintln!("holdthis4me: can't hold {}: {e}", path.display());
                }
            }
            if let Some(m) = &monitor {
                h.show(m, UNTOUCHED_MS); // a glimpse of what was added
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
        let items = shelf::list(&self.dir);
        for item in &items {
            self.list.append(&self.row(item));
        }
        let held: Vec<PathBuf> = items.iter().filter(|i| i.exists).map(|i| i.target.clone()).collect();
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

    /// Tall enough to cover the trigger strip (so a drag along the edge lands on the
    /// shelf), taller as items pile up, scrolling past 80% of the screen.
    fn fit(&self) {
        let Some(m) = self.monitor.borrow().clone() else { return };
        let screen = m.geometry().height();
        let max = (screen as f64 * 0.8) as i32;
        self.scroller.set_max_content_height(max);
        let (_, natural, _, _) = self.shelf.child().expect("frame").measure(gtk::Orientation::Vertical, SHELF_WIDTH);
        self.shelf.set_default_size(SHELF_WIDTH, natural.clamp(strip_height(&m), max));
    }

    fn row(self: &Rc<Self>, item: &shelf::Item) -> gtk::Box {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        row.add_css_class("ht4m-item");
        row.set_tooltip_text(Some(&item.target.to_string_lossy()));

        let icon = icon_for(&item.target, item.exists);
        let name = gtk::Label::builder()
            .label(item.target.file_name().map_or_else(|| item.target.to_string_lossy(), |n| n.to_string_lossy()))
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::Middle)
            .css_classes(["ht4m-name"])
            .build();
        let parent = item.target.parent().map(tilde).unwrap_or_default();
        let place = gtk::Label::builder()
            .label(if item.exists { parent } else { format!("missing (was in {parent})") })
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
        if item.exists {
            let reveal = icon_button("find-location-symbolic", "Show in Files");
            reveal.add_css_class("ht4m-itembtn");
            let target = item.target.clone();
            reveal.connect_clicked(with(&w, move |h| {
                show_in_files(&h.app, "ShowItems", &target);
                h.hide();
            }));
            row.append(&reveal);
        } else {
            row.add_css_class("missing");
        }
        let let_go = icon_button("window-close-symbolic", "Let go (the file stays where it is)");
        let_go.add_css_class("ht4m-itembtn");
        let link = item.link.clone();
        let_go.connect_clicked(with(&w, move |h| {
            if let Err(e) = shelf::remove(&h.dir, &link) {
                eprintln!("holdthis4me: {}: {e}", link.display());
            }
            h.reload();
        }));
        row.append(&let_go);

        if item.exists {
            let source = gtk::DragSource::new();
            source.set_actions(gdk::DragAction::COPY);
            let target = item.target.clone();
            source.connect_prepare(move |_, _, _| Some(files_content(std::slice::from_ref(&target))));
            self.drag_source_events(&source, icon.upcast_ref());
            row.add_controller(source);

            let click = gtk::GestureClick::new();
            let target = item.target.clone();
            click.connect_pressed(move |_, n, _, _| {
                if n == 2 {
                    open(&target);
                    if let Some(h) = w.upgrade() {
                        h.hide();
                    }
                }
            });
            row.add_controller(click);
        }
        row
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

fn strip_height(monitor: &gdk::Monitor) -> i32 {
    (monitor.geometry().height() as f64 * STRIP_SHARE) as i32
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
fn icon_for(path: &Path, exists: bool) -> gtk::Image {
    let image = if !exists {
        gtk::Image::from_icon_name("dialog-question-symbolic")
    } else {
        let info = gio::File::for_path(path).query_info(
            "standard::icon,thumbnail::path,thumbnail::is-valid",
            gio::FileQueryInfoFlags::NONE,
            None::<&gio::Cancellable>,
        );
        match info {
            Ok(info) => {
                let thumb = info
                    .attribute_byte_string("thumbnail::path")
                    .filter(|_| !info.has_attribute("thumbnail::is-valid") || info.boolean("thumbnail::is-valid"));
                match (thumb, info.icon()) {
                    (Some(t), _) => gtk::Image::from_file(t.as_str()),
                    (None, Some(icon)) => gtk::Image::from_gicon(&icon),
                    (None, None) => gtk::Image::from_icon_name("text-x-generic"),
                }
            }
            Err(_) => gtk::Image::from_icon_name("text-x-generic"),
        }
    };
    image.set_pixel_size(ICON_SIZE);
    image.add_css_class("ht4m-icon");
    image
}

/// What a drag hands to the app it's dropped on: the real files (as text/uri-list, plus
/// the document portal for sandboxed apps; GTK does both from a FileList).
fn files_content(paths: &[PathBuf]) -> gdk::ContentProvider {
    let files: Vec<gio::File> = paths.iter().map(gio::File::for_path).collect();
    gdk::ContentProvider::for_value(&gdk::FileList::from_array(&files).to_value())
}

fn open(path: &Path) {
    let ctx = gdk::Display::default().map(|d| d.app_launch_context());
    let uri = gio::File::for_path(path).uri();
    if let Err(e) = gio::AppInfo::launch_default_for_uri(&uri, ctx.as_ref()) {
        eprintln!("holdthis4me: can't open {}: {e}", path.display());
    }
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

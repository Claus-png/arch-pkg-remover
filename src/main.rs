// Arch Package Remover (APR) — interactive TUI for removing pacman packages.
//
// Build:  cargo build --release
// Run:    sudo ./target/release/apr

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::io;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, Sender};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph,
    },
    Frame, Terminal,
};
use alpm::{Alpm, PackageReason, TransFlag};

// ── Palette ──────────────────────────────────────────────────────────────────

const C_ACCENT:   Color = Color::Cyan;
const C_WARN:     Color = Color::Yellow;
const C_DANGER:   Color = Color::LightRed;
const C_SUCCESS:  Color = Color::Green;
const C_MUTED:    Color = Color::DarkGray;
const C_MARK:     Color = Color::Red;
const C_WHITE:    Color = Color::White;
const C_ORPHAN:   Color = Color::Magenta;
const C_SEL_BG:   Color = Color::Rgb(28, 50, 72);
const C_BAR_BG:   Color = Color::Rgb(15, 15, 22);
const C_MOD_BG:   Color = Color::Rgb(10, 10, 18);

fn st_accent()  -> Style { Style::default().fg(C_ACCENT) }
fn st_muted()   -> Style { Style::default().fg(C_MUTED) }
fn st_danger()  -> Style { Style::default().fg(C_DANGER).add_modifier(Modifier::BOLD) }
fn st_warn()    -> Style { Style::default().fg(C_WARN) }
fn st_bold()    -> Style { Style::default().add_modifier(Modifier::BOLD) }
fn st_mark()    -> Style { Style::default().fg(C_MARK).add_modifier(Modifier::BOLD) }
fn st_success() -> Style { Style::default().fg(C_SUCCESS) }
fn st_orphan()  -> Style { Style::default().fg(C_ORPHAN).add_modifier(Modifier::BOLD) }

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct PkgInfo {
    name:        String,
    version:     String,
    desc:        String,
    size:        i64,
    reason:      PackageReason,
    depends:     Vec<String>,
    optdeps:     Vec<String>,
    required_by: Vec<String>,   // packages that depend on this
}

impl PkgInfo {
    /// An "orphan" is a package installed as a dependency but nothing requires it.
    fn is_orphan(&self) -> bool {
        self.reason == PackageReason::Depend && self.required_by.is_empty()
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Simple,    // -R
    Recursive, // -Rs
    Full,      // -Rns
    Force,     // -Rdd ⚠
}

impl Mode {
    fn label(self) -> &'static str {
        match self { Mode::Simple=>"R", Mode::Recursive=>"Rs", Mode::Full=>"Rns", Mode::Force=>"Rdd ⚠" }
    }
    fn hint(self) -> &'static str {
        match self {
            Mode::Simple    => "Remove only",
            Mode::Recursive => "Remove + unused deps",
            Mode::Full      => "Remove + deps + configs",
            Mode::Force     => "Force (ignore deps!)",
        }
    }
    fn color(self) -> Color {
        match self { Mode::Simple=>C_SUCCESS, Mode::Recursive=>C_ACCENT, Mode::Full=>C_WARN, Mode::Force=>C_DANGER }
    }
    fn flags(self) -> TransFlag {
        match self {
            Mode::Simple    => TransFlag::empty(),
            Mode::Recursive => TransFlag::RECURSE,
            Mode::Full      => TransFlag::RECURSE | TransFlag::NO_SAVE,
            Mode::Force     => TransFlag::NO_DEPS,
        }
    }
    fn next(self) -> Self {
        match self {
            Mode::Simple    => Mode::Recursive,
            Mode::Recursive => Mode::Full,
            Mode::Full      => Mode::Force,
            Mode::Force     => Mode::Simple,
        }
    }
}

// Sort modes for the package list
#[derive(Clone, Copy, PartialEq, Debug)]
enum SortMode {
    Name,         // alphabetical (default)
    Size,         // largest first
    OrphansFirst, // orphans at top, then by name
}

impl SortMode {
    fn next(self) -> Self {
        match self {
            SortMode::Name         => SortMode::Size,
            SortMode::Size         => SortMode::OrphansFirst,
            SortMode::OrphansFirst => SortMode::Name,
        }
    }
    fn label(self) -> &'static str {
        match self {
            SortMode::Name         => "Name",
            SortMode::Size         => "Size↓",
            SortMode::OrphansFirst => "Orphans↑",
        }
    }
}

enum WorkerMsg {
    DryRunOk { targets: Vec<String>, freed: i64, warnings: Vec<String> },
    DryRunErr(String),
    Log(String),
    Progress(u8, String),
    Done(Result<(), String>),
    Refreshed(Vec<PkgInfo>),
}

enum Screen {
    Browse,
    Help,
    Confirm {
        targets:  Vec<String>,
        freed:    i64,
        warnings: Vec<String>,
        input:    String,
        flags:    TransFlag,
    },
    Removing {
        progress:    u8,
        current_pkg: String,
        log:         Vec<String>,
    },
    Finished {
        success: bool,
        message: String,
    },
}

// ── App State ─────────────────────────────────────────────────────────────────

struct App {
    pkgs:         Vec<PkgInfo>,
    filtered:     Vec<usize>,     // indices into pkgs, post-filter+sort
    cursor:       usize,
    marked:       HashSet<usize>, // indices into pkgs
    query:        String,
    mode:         Mode,
    sort_mode:    SortMode,
    show_orphans: bool,           // show only orphan packages
    screen:       Screen,
    searching:    bool,
    rx: Receiver<WorkerMsg>,
    tx: Sender<WorkerMsg>,
}

impl App {
    fn new() -> anyhow::Result<Self> {
        let pkgs = load_packages()?;
        let n = pkgs.len();
        let (tx, rx) = bounded(512);
        let mut app = Self {
            filtered: (0..n).collect(),
            pkgs,
            cursor: 0,
            marked: HashSet::new(),
            query: String::new(),
            mode: Mode::Recursive,
            sort_mode: SortMode::Name,
            show_orphans: false,
            screen: Screen::Browse,
            searching: false,
            rx, tx,
        };
        app.apply_filter();
        Ok(app)
    }

    fn apply_filter(&mut self) {
        let q = self.query.to_lowercase();

        self.filtered = self.pkgs.iter().enumerate()
            .filter(|(_, p)| {
                // orphan filter
                if self.show_orphans && !p.is_orphan() { return false; }
                // text search
                if q.is_empty() { return true; }
                p.name.to_lowercase().contains(&q) ||
                p.desc.to_lowercase().contains(&q) ||
                p.version.to_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect();

        // Apply sort
        match self.sort_mode {
            SortMode::Name => {
                self.filtered.sort_by(|&a, &b| self.pkgs[a].name.cmp(&self.pkgs[b].name));
            }
            SortMode::Size => {
                self.filtered.sort_by(|&a, &b| self.pkgs[b].size.cmp(&self.pkgs[a].size));
            }
            SortMode::OrphansFirst => {
                self.filtered.sort_by(|&a, &b| {
                    let oa = self.pkgs[a].is_orphan();
                    let ob = self.pkgs[b].is_orphan();
                    ob.cmp(&oa).then_with(|| self.pkgs[a].name.cmp(&self.pkgs[b].name))
                });
            }
        }

        self.cursor = self.cursor.min(self.filtered.len().saturating_sub(1));
    }

    fn orphan_count(&self) -> usize {
        self.pkgs.iter().filter(|p| p.is_orphan()).count()
    }

    fn move_cursor(&mut self, delta: i64) {
        if self.filtered.is_empty() { return; }
        let len = self.filtered.len() as i64;
        self.cursor = ((self.cursor as i64 + delta).rem_euclid(len)) as usize;
    }

    fn toggle_mark(&mut self) {
        if let Some(&idx) = self.filtered.get(self.cursor) {
            if self.marked.contains(&idx) { self.marked.remove(&idx); }
            else { self.marked.insert(idx); }
        }
    }

    /// Mark every orphan package and switch to the recursive mode so the
    /// next dry-run/removal cleans them all up in one go.
    fn mark_all_orphans(&mut self) {
        self.marked.clear();
        for (i, p) in self.pkgs.iter().enumerate() {
            if p.is_orphan() {
                self.marked.insert(i);
            }
        }
        if self.mode == Mode::Simple {
            self.mode = Mode::Recursive;
        }
    }

    fn selected_pkg(&self) -> Option<&PkgInfo> {
        self.filtered.get(self.cursor).map(|&i| &self.pkgs[i])
    }

    fn marked_names(&self) -> Vec<String> {
        let mut v: Vec<_> = self.marked.iter()
            .map(|&i| self.pkgs[i].name.clone())
            .collect();
        v.sort();
        v
    }

    fn marked_size(&self) -> i64 {
        self.marked.iter().map(|&i| self.pkgs[i].size).sum()
    }

    fn process_messages(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                WorkerMsg::DryRunOk { targets, freed, warnings } => {
                    self.screen = Screen::Confirm {
                        flags: self.mode.flags(),
                        targets, freed, warnings,
                        input: String::new(),
                    };
                }
                WorkerMsg::DryRunErr(e) => {
                    self.screen = Screen::Finished { success: false, message: e };
                }
                WorkerMsg::Log(l) => {
                    if let Screen::Removing { log, .. } = &mut self.screen {
                        log.push(l);
                        if log.len() > 30 { log.remove(0); }
                    }
                }
                WorkerMsg::Progress(pct, pkg) => {
                    if let Screen::Removing { progress, current_pkg, .. } = &mut self.screen {
                        *progress = pct;
                        *current_pkg = pkg;
                    }
                }
                WorkerMsg::Done(result) => {
                    match result {
                        Ok(()) => {
                            if let Ok(pkgs) = load_packages() {
                                self.pkgs = pkgs;
                                self.marked.clear();
                                self.apply_filter();
                            }
                            self.screen = Screen::Finished {
                                success: true,
                                message: "Packages removed successfully.".into(),
                            };
                        }
                        Err(e) => {
                            self.screen = Screen::Finished { success: false, message: e };
                        }
                    }
                }
                WorkerMsg::Refreshed(pkgs) => {
                    self.pkgs = pkgs;
                    self.marked.retain(|&i| i < self.pkgs.len());
                    self.apply_filter();
                }
            }
        }
    }

    // Real ALPM dry-run — forwards mode flags so we see the transitive diff
    fn begin_dry_run(&mut self) {
        if self.marked.is_empty() { return; }
        let names = self.marked_names();
        let flags = self.mode.flags();
        let tx = self.tx.clone();
        thread::spawn(move || {
            dry_run_alpm(names, flags, tx);
        });
    }

    fn begin_removal(&mut self, targets: Vec<String>, flags: TransFlag) {
        let tx = self.tx.clone();
        thread::spawn(move || { let _ = perform_removal(targets, flags, tx); });
        self.screen = Screen::Removing {
            progress: 0,
            current_pkg: String::new(),
            log: vec!["Initialising transaction…".into()],
        };
    }

    fn begin_refresh(&mut self) {
        let tx = self.tx.clone();
        thread::spawn(move || {
            if let Ok(pkgs) = load_packages() {
                let _ = tx.send(WorkerMsg::Refreshed(pkgs));
            }
        });
    }

    /// Returns true → quit
    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        if code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL) {
            return true;
        }
        match &self.screen {
            Screen::Finished { .. } => { self.screen = Screen::Browse; return false; }
            Screen::Help        { .. } => { self.screen = Screen::Browse; return false; }
            Screen::Removing    { .. } => return false,
            _ => {}
        }
        if matches!(&self.screen, Screen::Confirm { .. }) {
            return self.key_confirm(code);
        }
        if self.searching {
            self.key_search(code);
        } else {
            return self.key_browse(code);
        }
        false
    }

    fn key_search(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc | KeyCode::Enter => { self.searching = false; }
            KeyCode::Char(c)   => { self.query.push(c); self.apply_filter(); }
            KeyCode::Backspace => { self.query.pop();   self.apply_filter(); }
            _ => {}
        }
    }

    fn key_browse(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Char('q')              => return true,
            KeyCode::Char('/')              => { self.searching = true; }
            KeyCode::Char('?')              => { self.screen = Screen::Help; }
            KeyCode::Esc => {
                if self.show_orphans { self.show_orphans = false; self.apply_filter(); }
                else if !self.query.is_empty() { self.query.clear(); self.apply_filter(); }
                self.cursor = self.cursor.min(self.filtered.len().saturating_sub(1));
            }
            KeyCode::Up   | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::PageUp                    => self.move_cursor(-10),
            KeyCode::PageDown                  => self.move_cursor(10),
            KeyCode::Char('g') | KeyCode::Home => { self.cursor = 0; }
            KeyCode::Char('G') | KeyCode::End  => { self.cursor = self.filtered.len().saturating_sub(1); }
            KeyCode::Char(' ')              => self.toggle_mark(),
            KeyCode::Char('a')              => {
                let all_marked = self.filtered.iter().all(|i| self.marked.contains(i));
                for &i in &self.filtered {
                    if all_marked { self.marked.remove(&i); } else { self.marked.insert(i); }
                }
            }
            // Quick action: mark every orphan in one keystroke and switch to a
            // safe removal mode, ready for [d] to dry-run.
            KeyCode::Char('O')              => self.mark_all_orphans(),
            KeyCode::Char('d')              => self.begin_dry_run(),
            KeyCode::Char('r')              => self.begin_refresh(),
            KeyCode::Char('o')              => {
                self.show_orphans = !self.show_orphans;
                self.apply_filter();
            }
            KeyCode::Char('s')              => {
                self.sort_mode = self.sort_mode.next();
                self.apply_filter();
            }
            KeyCode::Char('1')              => self.mode = Mode::Simple,
            KeyCode::Char('2')              => self.mode = Mode::Recursive,
            KeyCode::Char('3')              => self.mode = Mode::Full,
            KeyCode::Char('4')              => self.mode = Mode::Force,
            KeyCode::Tab                    => self.mode = self.mode.next(),
            _ => {}
        }
        false
    }

    fn key_confirm(&mut self, code: KeyCode) -> bool {
        let ready = if let Screen::Confirm { input, .. } = &self.screen {
            code == KeyCode::Enter && input.trim() == "yes"
        } else { false };

        if ready {
            if let Screen::Confirm { targets, flags, .. } =
                std::mem::replace(&mut self.screen, Screen::Browse)
            {
                self.begin_removal(targets, flags);
            }
            return false;
        }

        match code {
            KeyCode::Esc => { self.screen = Screen::Browse; }
            KeyCode::Char(c) => {
                if let Screen::Confirm { input, .. } = &mut self.screen { input.push(c); }
            }
            KeyCode::Backspace => {
                if let Screen::Confirm { input, .. } = &mut self.screen { input.pop(); }
            }
            _ => {}
        }
        false
    }
}

// ── ALPM ──────────────────────────────────────────────────────────────────────

fn load_packages() -> anyhow::Result<Vec<PkgInfo>> {
    let handle = Alpm::new("/", "/var/lib/pacman/")?;
    let db = handle.localdb();

    // Build a reverse-dependency map in one pass (faster than compute_requiredby per pkg)
    let mut rev_deps: HashMap<String, Vec<String>> = HashMap::new();
    for pkg in db.pkgs().iter() {
        let pname = pkg.name().to_string();
        for dep in pkg.depends().iter() {
            rev_deps
                .entry(dep.name().to_string())
                .or_default()
                .push(pname.clone());
        }
    }

    let mut pkgs: Vec<PkgInfo> = db.pkgs().iter().map(|p| {
        let name = p.name().to_string();
        let required_by = rev_deps.get(&name).cloned().unwrap_or_default();
        PkgInfo {
            name,
            version:     p.version().to_string(),
            desc:        p.desc().unwrap_or("").to_string(),
            size:        p.isize(),
            reason:      p.reason(),
            depends:     p.depends().iter().map(|d| d.name().to_string()).collect(),
            optdeps:     p.optdepends().iter().map(|d| d.name().to_string()).collect(),
            required_by,
        }
    }).collect();

    pkgs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(pkgs)
}

// Real ALPM dry-run — uses trans_prepare() to get the actual removal list
// including transitive dependencies that the chosen mode would pull in.
fn dry_run_alpm(names: Vec<String>, flags: TransFlag, tx: Sender<WorkerMsg>) {
    // Lock check
    if std::path::Path::new("/var/lib/pacman/db.lck").exists() {
        let _ = tx.send(WorkerMsg::DryRunErr(
            "Database is locked (/var/lib/pacman/db.lck).\nAnother package manager may be running.".into()
        ));
        return;
    }

    let mut handle = match Alpm::new("/", "/var/lib/pacman/") {
        Ok(h) => h,
        Err(e) => { let _ = tx.send(WorkerMsg::DryRunErr(format!("alpm init: {}", e))); return; }
    };

    if let Err(e) = handle.trans_init(flags) {
        let _ = tx.send(WorkerMsg::DryRunErr(format!("trans_init: {}", e)));
        return;
    }

    // Add all selected packages to the transaction
    let localdb = handle.localdb();
    for name in &names {
        match localdb.pkg(name.as_str()) {
            Ok(pkg) => {
                if let Err(e) = handle.trans_remove_pkg(pkg) {
                    let _ = handle.trans_release();
                    let _ = tx.send(WorkerMsg::DryRunErr(format!("add '{}': {}", name, e)));
                    return;
                }
            }
            Err(_) => {
                let _ = handle.trans_release();
                let _ = tx.send(WorkerMsg::DryRunErr(format!("Package not found: {}", name)));
                return;
            }
        }
    }

    // Prepare resolves transitive dependencies
    if let Err(e_str) = handle.trans_prepare().map_err(|e| format!("{:?}", e)) {
        let _ = handle.trans_release();
        let _ = tx.send(WorkerMsg::DryRunErr(format!("Dependency resolution failed:\n{}", e_str)));
        return;
    }

    // Collect the ACTUAL removal list (may be larger than `names` due to recursive flags)
    let remove_info: Vec<(String, i64)> = handle.trans_remove()
        .iter()
        .map(|p| (p.name().to_string(), p.isize()))
        .collect();

    let _ = handle.trans_release();

    let targets: Vec<String> = remove_info.iter().map(|(n, _)| n.clone()).collect();
    let freed: i64 = remove_info.iter().map(|(_, s)| *s).sum();

    // Critical system package list
    let critical = [
        "glibc", "systemd", "pacman", "linux", "base", "bash", "coreutils",
        "gcc-libs", "glib2", "dbus", "util-linux", "shadow", "pam", "openssl",
        "cryptsetup", "lvm2", "e2fsprogs", "btrfs-progs", "grub", "efibootmgr",
        "sudo", "polkit", "networkmanager", "systemd-libs", "libsystemd",
        "iptables", "nftables", "iproute2", "procps-ng", "psmisc", "less",
        "grep", "gawk", "sed", "findutils", "tar", "gzip", "xz", "zstd",
        "mkinitcpio", "filesystem", "tzdata",
    ];
    let warnings: Vec<String> = targets.iter()
        .filter(|n| {
            critical.contains(&n.as_str()) ||
            (n.starts_with("linux-") && !n.starts_with("linux-headers"))
        })
        .map(|n| format!("{}  is a critical system package!", n))
        .collect();

    let _ = tx.send(WorkerMsg::DryRunOk { targets, freed, warnings });
}

fn perform_removal(
    names: Vec<String>,
    flags: TransFlag,
    tx: Sender<WorkerMsg>,
) -> anyhow::Result<()> {
    if std::path::Path::new("/var/lib/pacman/db.lck").exists() {
        let _ = tx.send(WorkerMsg::Done(Err(
            "Database is locked (/var/lib/pacman/db.lck).\nAnother package manager may be running.".into()
        )));
        return Ok(());
    }

    let mut handle = Alpm::new("/", "/var/lib/pacman/")
        .map_err(|e| anyhow::anyhow!("alpm init: {}", e))?;

    handle.trans_init(flags)
        .map_err(|e| anyhow::anyhow!("trans_init: {}", e))?;

    let localdb = handle.localdb();
    for name in &names {
        match localdb.pkg(name.as_str()) {
            Ok(pkg) => {
                handle.trans_remove_pkg(pkg)
                    .map_err(|e| anyhow::anyhow!("add '{}': {}", name, e))?;
            }
            Err(_) => {
                let _ = handle.trans_release();
                let _ = tx.send(WorkerMsg::Done(Err(format!("Package not found: {}", name))));
                return Ok(());
            }
        }
    }

    let _ = tx.send(WorkerMsg::Log("Resolving dependencies…".into()));
    if let Err(e_str) = handle.trans_prepare().map_err(|e| format!("{:?}", e)) {
        let _ = handle.trans_release();
        let _ = tx.send(WorkerMsg::Done(Err(format!("Prepare failed: {}", e_str))));
        return Ok(());
    }

    let _ = tx.send(WorkerMsg::Log("Committing transaction…".into()));
    let n = names.len();
    for (i, name) in names.iter().enumerate() {
        let pct = ((i as f32 / n as f32) * 90.0) as u8;
        let _ = tx.send(WorkerMsg::Progress(pct, name.clone()));
        let _ = tx.send(WorkerMsg::Log(format!("Removing {}…", name)));
    }

    if let Err(e_str) = handle.trans_commit().map_err(|e| format!("{:?}", e)) {
        let _ = handle.trans_release();
        let _ = tx.send(WorkerMsg::Done(Err(format!("Commit failed: {}", e_str))));
        return Ok(());
    }

    let _ = handle.trans_release();
    let _ = tx.send(WorkerMsg::Progress(100, String::new()));
    let _ = tx.send(WorkerMsg::Log("Done.".into()));
    let _ = tx.send(WorkerMsg::Done(Ok(())));
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn fmt_size(b: i64) -> String {
    if b < 0 { return "–".into(); }
    let f = b as f64;
    if f < 1024.0 * 1024.0          { format!("{:.0} KiB", f / 1024.0) }
    else if f < 1024.0_f64.powi(3)  { format!("{:.1} MiB", f / 1024.0_f64.powi(2)) }
    else                             { format!("{:.2} GiB", f / 1024.0_f64.powi(3)) }
}

fn size_color(b: i64) -> Color {
    if b < 30_000_000       { C_SUCCESS }
    else if b < 500_000_000 { C_WARN }
    else                    { C_DANGER }
}

fn centered_rect(pct_w: u16, pct_h: u16, r: Rect) -> Rect {
    let w = (r.width  * pct_w) / 100;
    let h = (r.height * pct_h) / 100;
    Rect {
        x: r.x + (r.width.saturating_sub(w)) / 2,
        y: r.y + (r.height.saturating_sub(h)) / 2,
        width: w, height: h,
    }
}

fn trunc(s: &str, max: usize) -> String {
    if s.len() <= max { s.to_string() }
    else { format!("{}…", &s[..max.saturating_sub(1)]) }
}

fn hdivider(width: u16) -> Span<'static> {
    Span::styled("─".repeat(width as usize), st_muted())
}

// ── Draw ──────────────────────────────────────────────────────────────────────

fn draw(f: &mut Frame, app: &App) {
    match &app.screen {
        Screen::Browse => draw_main(f, app),
        Screen::Help => {
            draw_main(f, app);
            draw_help(f);
        }
        Screen::Confirm { targets, freed, warnings, input, .. } => {
            draw_main(f, app);
            draw_confirm(f, targets, *freed, warnings, input);
        }
        Screen::Removing { progress, current_pkg, log } => {
            draw_removing(f, *progress, current_pkg, log);
        }
        Screen::Finished { success, message } => {
            draw_main(f, app);
            draw_finished(f, *success, message);
        }
    }
}

fn draw_main(f: &mut Frame, app: &App) {
    let area = f.area();
    let v = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(8),
        Constraint::Length(1),
    ]).split(area);

    draw_header(f, app, v[0]);
    draw_body(f, app, v[1]);
    draw_statusbar(f, app, v[2]);
}

// ── Header ────────────────────────────────────────────────────────────────────

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    // 3-column header: Search | Sort indicator | Mode
    let h = Layout::horizontal([
        Constraint::Min(20),
        Constraint::Length(14),
        Constraint::Length(20),
    ]).split(area);

    // Search box
    let (border_st, title_st) = if app.searching {
        (st_accent(), Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD))
    } else {
        (st_muted(), st_muted())
    };

    let cursor_blink = Span::styled("▌", st_accent());
    let query_spans = if app.query.is_empty() && !app.searching {
        let hint = if app.show_orphans {
            " 🔵 orphans only — /search or Esc"
        } else {
            "  type / to search…"
        };
        vec![Span::styled(hint, st_muted())]
    } else {
        let mut v = vec![
            Span::styled(" ❯ ", Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(app.query.clone(), Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)),
        ];
        if app.searching { v.push(cursor_blink); }
        v
    };

    let mut title_parts = vec![Span::styled(" Search ", title_st)];
    if app.show_orphans {
        title_parts.push(Span::styled(" orphans ", st_orphan()));
    }

    let search = Paragraph::new(Line::from(query_spans))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(if app.show_orphans { Style::default().fg(C_ORPHAN) } else { border_st })
                .title(Line::from(title_parts)),
        );
    f.render_widget(search, h[0]);

    // Sort mode indicator
    let sort_col = match app.sort_mode {
        SortMode::Name         => C_MUTED,
        SortMode::Size         => C_WARN,
        SortMode::OrphansFirst => C_ORPHAN,
    };
    let sort_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(sort_col))
        .title(Span::styled(" Sort ", st_muted()));
    let sort_inner = sort_block.inner(h[1]);
    f.render_widget(sort_block, h[1]);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!(" {} ", app.sort_mode.label()),
                Style::default().fg(sort_col).add_modifier(Modifier::BOLD)),
            Span::styled("[s]", st_muted()),
        ])).alignment(Alignment::Center),
        sort_inner,
    );

    // Mode selector
    let mode_col = app.mode.color();
    let mode_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(mode_col))
        .title(Span::styled(" Mode ", st_muted()));
    let mode_inner = mode_block.inner(h[2]);
    f.render_widget(mode_block, h[2]);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!(" -{} ", app.mode.label()),
                Style::default().fg(mode_col).add_modifier(Modifier::BOLD)),
            Span::styled("1-4", st_muted()),
        ])).alignment(Alignment::Center),
        mode_inner,
    );
}

// ── Body ──────────────────────────────────────────────────────────────────────

fn draw_body(f: &mut Frame, app: &App, area: Rect) {
    let h = Layout::horizontal([
        Constraint::Percentage(46),
        Constraint::Percentage(54),
    ]).split(area);

    draw_pkg_list(f, app, h[0]);
    draw_details(f, app, h[1]);
}

// ── Package List ──────────────────────────────────────────────────────────────

fn draw_pkg_list(f: &mut Frame, app: &App, area: Rect) {
    let marked = app.marked.len();
    let orphan_count = app.orphan_count();

    let title = Line::from(vec![
        Span::styled(" Packages ", st_bold()),
        Span::styled(format!("{}/{} ", app.filtered.len(), app.pkgs.len()), st_muted()),
        if orphan_count > 0 {
            Span::styled(format!(" ◆{} ", orphan_count), st_orphan())
        } else { Span::raw("") },
        if marked > 0 {
            Span::styled(format!(" ✓{} ", marked), st_mark())
        } else { Span::raw("") },
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(st_muted())
        .title(title);

    let inner = block.inner(area);
    f.render_widget(&block, area);

    if inner.height < 3 { return; }

    let col_w = inner.width as usize;
    let name_w  = 24usize.min(col_w.saturating_sub(26));
    let ver_w   = 13usize;
    let size_w  = 10usize;

    let col_header = Line::from(vec![
        Span::styled("     ", st_muted()),
        Span::styled(format!("{:<width$}", "NAME", width = name_w),
            Style::default().fg(C_MUTED).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)),
        Span::styled(format!(" {:<width$}", "VERSION", width = ver_w), st_muted()),
        Span::styled(format!("{:>width$}", "SIZE", width = size_w), st_muted()),
    ]);

    let header_rect = Rect { x: inner.x, y: inner.y, width: inner.width, height: 1 };
    f.render_widget(Paragraph::new(col_header), header_rect);

    let div_rect = Rect { x: inner.x, y: inner.y + 1, width: inner.width, height: 1 };
    f.render_widget(Paragraph::new(hdivider(inner.width)), div_rect);

    let list_rect = Rect {
        x: inner.x, y: inner.y + 2,
        width: inner.width,
        height: inner.height.saturating_sub(2),
    };

    let items: Vec<ListItem> = app.filtered.iter().enumerate().map(|(list_i, &pkg_i)| {
        let p = &app.pkgs[pkg_i];
        let is_marked   = app.marked.contains(&pkg_i);
        let is_selected = list_i == app.cursor;
        let is_orphan   = p.is_orphan();

        // Checkbox / orphan indicator
        let prefix = if is_marked {
            Span::styled(" ✓ ", st_mark())
        } else if is_orphan {
            Span::styled(" ◆ ", st_orphan())
        } else {
            Span::styled(" ○ ", st_muted())
        };

        // Name color
        let name = trunc(&p.name, name_w);
        let name_style = if is_marked {
            Style::default().fg(C_MARK).add_modifier(Modifier::BOLD)
        } else if is_orphan && is_selected {
            Style::default().fg(C_ORPHAN).add_modifier(Modifier::BOLD)
        } else if is_orphan {
            Style::default().fg(C_ORPHAN)
        } else if is_selected {
            Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(C_WHITE)
        };
        let name_span = Span::styled(format!("{:<width$}", name, width = name_w), name_style);

        let ver = trunc(&p.version, ver_w);
        let ver_span = Span::styled(format!(" {:<width$}", ver, width = ver_w), st_muted());

        let size_span = Span::styled(
            format!("{:>width$}", fmt_size(p.size), width = size_w),
            Style::default().fg(size_color(p.size)),
        );

        // Reason dot
        let reason_dot = match p.reason {
            PackageReason::Explicit => Span::styled("·", Style::default().fg(C_SUCCESS)),
            PackageReason::Depend   => Span::styled("·", st_muted()),
        };

        let line = Line::from(vec![prefix, name_span, ver_span, size_span, Span::raw(" "), reason_dot]);
        if is_selected {
            ListItem::new(line).style(Style::default().bg(C_SEL_BG))
        } else {
            ListItem::new(line)
        }
    }).collect();

    let mut state = ListState::default();
    state.select(Some(app.cursor));
    f.render_stateful_widget(List::new(items), list_rect, &mut state);
}

// ── Details Panel ─────────────────────────────────────────────────────────────

fn draw_details(f: &mut Frame, app: &App, area: Rect) {
    let is_marked_cur = app.filtered.get(app.cursor)
        .map(|i| app.marked.contains(i))
        .unwrap_or(false);

    let Some(p) = app.selected_pkg() else {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(st_muted())
            .title(Span::styled(" Details ", st_bold()));
        let inner = block.inner(area);
        f.render_widget(block, area);
        f.render_widget(
            Paragraph::new(Span::styled("  No packages found.", st_muted())),
            inner,
        );
        return;
    };

    let is_orphan = p.is_orphan();

    let title = Line::from(vec![
        Span::styled(" Details ", st_bold()),
        if is_marked_cur { Span::styled(" ✓ marked ", st_mark()) }
        else if is_orphan { Span::styled(" ◆ orphan ", st_orphan()) }
        else { Span::raw("") },
    ]);

    let border_style = if is_marked_cur {
        Style::default().fg(C_MARK)
    } else if is_orphan {
        Style::default().fg(C_ORPHAN)
    } else {
        st_muted()
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .title(title);

    let inner = block.inner(area);
    f.render_widget(&block, area);

    let w = inner.width as usize;
    let mut lines: Vec<Line> = vec![];

    lines.push(Line::from(vec![
        Span::styled(" ◆ ", Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(p.name.clone(), Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)),
    ]));
    lines.push(Line::from(hdivider(inner.width)));
    lines.push(Line::raw(""));

    let field = |label: &'static str, val: String| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("  {:<11} ", label), st_muted()),
            Span::styled(val, Style::default().fg(C_WHITE)),
        ])
    };

    lines.push(field("Version",  p.version.clone()));
    lines.push(field("Size",     fmt_size(p.size)));
    lines.push(field("Reason",   match p.reason {
        PackageReason::Explicit => "Explicit".into(),
        PackageReason::Depend   => {
            if is_orphan { "Dependency (ORPHAN)".into() }
            else { "Dependency".into() }
        }
    }));
    lines.push(Line::raw(""));

    // Description
    if !p.desc.is_empty() {
        let text_w = w.saturating_sub(4);
        let mut buf = String::new();
        for word in p.desc.split_whitespace() {
            if buf.len() + word.len() + 1 > text_w && !buf.is_empty() {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(std::mem::take(&mut buf), st_muted()),
                ]));
            }
            if !buf.is_empty() { buf.push(' '); }
            buf.push_str(word);
        }
        if !buf.is_empty() {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(buf, st_muted()),
            ]));
        }
        lines.push(Line::raw(""));
    }

    // Dependencies
    if !p.depends.is_empty() {
        lines.push(section_header(inner.width, "Dependencies"));
        let chunk_w = w.saturating_sub(4);
        let mut row = String::new();
        for d in &p.depends {
            let token = format!("{}  ", d);
            if row.len() + token.len() > chunk_w && !row.is_empty() {
                lines.push(Line::from(vec![Span::raw("  "), Span::styled(std::mem::take(&mut row), st_muted())]));
            }
            row.push_str(&token);
        }
        if !row.is_empty() {
            lines.push(Line::from(vec![Span::raw("  "), Span::styled(row, st_muted())]));
        }
        lines.push(Line::raw(""));
    }

    // Required By (reverse dependencies)
    if !p.required_by.is_empty() {
        lines.push(section_header(inner.width, "Required By"));
        let chunk_w = w.saturating_sub(4);
        let mut row = String::new();
        for d in &p.required_by {
            let token = format!("{}  ", d);
            if row.len() + token.len() > chunk_w && !row.is_empty() {
                lines.push(Line::from(vec![Span::raw("  "), Span::styled(std::mem::take(&mut row), Style::default().fg(C_ACCENT))]));
            }
            row.push_str(&token);
        }
        if !row.is_empty() {
            lines.push(Line::from(vec![Span::raw("  "), Span::styled(row, Style::default().fg(C_ACCENT))]));
        }
        lines.push(Line::raw(""));
    } else if is_orphan {
        // Highlight clearly that nothing requires this package
        lines.push(section_header_col(inner.width, "Required By", C_ORPHAN));
        lines.push(Line::from(vec![
            Span::styled("  ◆ ", st_orphan()),
            Span::styled("Nothing requires this package (safe to remove)", st_orphan()),
        ]));
        lines.push(Line::raw(""));
    }

    // Optional deps
    if !p.optdeps.is_empty() {
        lines.push(section_header(inner.width, "Optional"));
        for d in p.optdeps.iter().take(4) {
            lines.push(Line::from(vec![
                Span::styled("  + ", st_muted()),
                Span::styled(d.clone(), st_muted()),
            ]));
        }
        if p.optdeps.len() > 4 {
            lines.push(Line::from(Span::styled(
                format!("  …{} more", p.optdeps.len() - 4), st_muted()
            )));
        }
        lines.push(Line::raw(""));
    }

    // Marked banner
    if is_marked_cur {
        lines.push(section_header_col(inner.width, "Queued for removal", C_MARK));
        lines.push(Line::from(vec![
            Span::styled("  ✓ ", st_mark()),
            Span::styled("Will be removed on [d]", st_mark()),
        ]));
    }

    f.render_widget(Paragraph::new(lines), inner);
}

fn section_header(width: u16, label: &'static str) -> Line<'static> {
    section_header_col(width, label, C_ACCENT)
}

fn section_header_col(width: u16, label: &'static str, col: Color) -> Line<'static> {
    let dash = "─".repeat(2);
    Line::from(vec![
        Span::styled(format!("  {} ", dash), st_muted()),
        Span::styled(label.to_string(), Style::default().fg(col)),
        Span::styled(format!(" {}{}",
            dash,
            "─".repeat((width as usize).saturating_sub(label.len() + 9))
        ), st_muted()),
    ])
}

// ── Status Bar ────────────────────────────────────────────────────────────────

fn draw_statusbar(f: &mut Frame, app: &App, area: Rect) {
    let marked = app.marked.len();
    let orphan_count = app.orphan_count();

    let mut spans: Vec<Span> = vec![
        kb("↑↓/jk"), hint(" Nav  "),
        kb("Spc"),    hint(" Mark  "),
        kb("a"),      hint(" All  "),
        kb("O"),      hint(" MarkOrphans  "),
        kb("d"),      hint(" Delete  "),
        kb("/"),      hint(" Search  "),
        kb("o"),      hint(if app.show_orphans { " Orphans✓  " } else { " Orphans  " }),
        kb("s"),      hint(format!(" {} Sort  ", app.sort_mode.label())),
        kb("1-4/Tab"), hint(format!(" {}  ", app.mode.label())),
        kb("r"),      hint(" Refresh  "),
        kb("?"),      hint(" Help  "),
        kb("q"),      hint(" Quit"),
    ];

    if orphan_count > 0 {
        spans.push(Span::raw("    "));
        spans.push(Span::styled(
            format!("◆ {} orphan{}", orphan_count, if orphan_count == 1 { "" } else { "s" }),
            st_orphan(),
        ));
    }

    if marked > 0 {
        spans.push(Span::raw("    "));
        spans.push(Span::styled(
            format!("✓ {}  {}  queued", marked, fmt_size(app.marked_size())),
            st_mark(),
        ));
    }

    let bar = Paragraph::new(Line::from(spans))
        .style(Style::default().bg(C_BAR_BG));
    f.render_widget(bar, area);
}

fn kb(s: impl Into<String>) -> Span<'static> {
    Span::styled(format!(" {}", s.into()), st_accent())
}
fn hint<S: Into<String>>(s: S) -> Span<'static> {
    Span::styled(s.into(), st_muted())
}

// ── Confirm Modal ─────────────────────────────────────────────────────────────

fn draw_confirm(f: &mut Frame, targets: &[String], freed: i64, warnings: &[String], input: &str) {
    let area = centered_rect(64, 72, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(C_WARN))
        .style(Style::default().bg(C_MOD_BG))
        .title(Span::styled(" ⚠  CONFIRM REMOVAL ", Style::default().fg(C_WARN).add_modifier(Modifier::BOLD)));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = vec![];
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled("  The following packages will be ", st_muted()),
        Span::styled("permanently removed:", Style::default().fg(C_DANGER).add_modifier(Modifier::BOLD)),
    ]));
    lines.push(Line::raw(""));

    for name in targets.iter().take(12) {
        lines.push(Line::from(vec![
            Span::styled("  ✗ ", Style::default().fg(C_DANGER)),
            Span::styled(name.clone(), Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)),
        ]));
    }
    if targets.len() > 12 {
        lines.push(Line::from(Span::styled(
            format!("  … and {} more", targets.len() - 12), st_muted()
        )));
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(hdivider(inner.width)));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled("  Total freed:  ", st_muted()),
        Span::styled(fmt_size(freed), Style::default().fg(C_SUCCESS).add_modifier(Modifier::BOLD)),
    ]));

    if !warnings.is_empty() {
        lines.push(Line::raw(""));
        for w in warnings {
            lines.push(Line::from(vec![
                Span::styled("  ⚠  ", st_warn()),
                Span::styled(w.clone(), st_warn()),
            ]));
        }
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(hdivider(inner.width)));
    lines.push(Line::raw(""));

    let confirmed = input.trim() == "yes";
    let input_style = if confirmed {
        Style::default().fg(C_SUCCESS).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(C_ACCENT)
    };
    lines.push(Line::from(vec![
        Span::styled("  Type ", st_muted()),
        Span::styled("yes", Style::default().fg(C_DANGER).add_modifier(Modifier::BOLD)),
        Span::styled(" to confirm: ", st_muted()),
        Span::styled(format!("[ {}▌]", input), input_style),
    ]));

    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled("  Esc", st_accent()),
        Span::styled("  Cancel        ", st_muted()),
        Span::styled("Enter", st_accent()),
        Span::styled("  Confirm", st_muted()),
    ]));

    f.render_widget(Paragraph::new(lines), inner);
}

// ── Removing Screen ───────────────────────────────────────────────────────────

fn draw_removing(f: &mut Frame, progress: u8, current: &str, log: &[String]) {
    let area = f.area();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(st_danger())
        .style(Style::default().bg(C_MOD_BG))
        .title(Span::styled(" ✗  REMOVING PACKAGES ", Style::default().fg(C_DANGER).add_modifier(Modifier::BOLD)));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let v = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(4),
    ]).split(inner);

    f.render_widget(Paragraph::new(Line::from(vec![
        Span::styled("  Removing: ", st_muted()),
        Span::styled(current.to_string(), Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)),
    ])), v[0]);

    let gauge = Gauge::default()
        .gauge_style(Style::default().fg(C_ACCENT).bg(Color::Rgb(30, 30, 30)))
        .percent(progress as u16)
        .label(format!(" {}% ", progress));
    f.render_widget(gauge, v[2]);

    let log_block = Block::default()
        .borders(Borders::TOP)
        .border_style(st_muted())
        .title(Span::styled(" Log ", st_muted()));
    let log_inner = log_block.inner(v[4]);
    f.render_widget(log_block, v[4]);

    let log_items: Vec<Line> = log.iter()
        .rev()
        .take(log_inner.height as usize)
        .rev()
        .map(|l| Line::from(vec![
            Span::styled("  → ", st_accent()),
            Span::styled(l.clone(), st_muted()),
        ]))
        .collect();

    f.render_widget(Paragraph::new(log_items), log_inner);
}

// ── Finished Modal ────────────────────────────────────────────────────────────

fn draw_finished(f: &mut Frame, success: bool, message: &str) {
    let area = centered_rect(54, 28, f.area());
    f.render_widget(Clear, area);

    let (title, col) = if success {
        (" ✓  DONE ", C_SUCCESS)
    } else {
        (" ✗  ERROR ", C_DANGER)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(col))
        .style(Style::default().bg(C_MOD_BG))
        .title(Span::styled(title, Style::default().fg(col).add_modifier(Modifier::BOLD)));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let lines = vec![
        Line::raw(""),
        Line::from(vec![Span::raw("  "), Span::styled(message.to_string(), st_muted())]),
        Line::raw(""),
        Line::from(Span::styled("  Press any key to return.", st_muted())),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

// ── Help Modal ────────────────────────────────────────────────────────────────

fn draw_help(f: &mut Frame) {
    let area = centered_rect(60, 80, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(st_accent())
        .style(Style::default().bg(C_MOD_BG))
        .title(Span::styled(" ?  HELP ", Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let row = |key: &'static str, desc: &'static str| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("  {:<10}", key), Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(desc, Style::default().fg(C_WHITE)),
        ])
    };
    let section = |label: &'static str| -> Line<'static> {
        Line::from(Span::styled(format!("  {}", label), st_bold()))
    };

    let mut lines = vec![Line::raw("")];

    lines.push(section("Navigation"));
    lines.push(row("↑↓ / j k", "Move selection"));
    lines.push(row("PgUp/PgDn", "Move 10 rows"));
    lines.push(row("g / Home", "Jump to top"));
    lines.push(row("G / End", "Jump to bottom"));
    lines.push(row("/", "Search packages"));
    lines.push(row("Esc", "Clear search / orphan filter"));
    lines.push(Line::raw(""));

    lines.push(section("Selection"));
    lines.push(row("Space", "Toggle mark on selected package"));
    lines.push(row("a", "Mark/unmark all visible packages"));
    lines.push(row("O", "Mark all orphans (quick cleanup)"));
    lines.push(Line::raw(""));

    lines.push(section("View"));
    lines.push(row("o", "Toggle orphans-only filter"));
    lines.push(row("s", "Cycle sort mode"));
    lines.push(row("r", "Refresh package list"));
    lines.push(Line::raw(""));

    lines.push(section("Removal"));
    lines.push(row("1-4", "Set mode: R / Rs / Rns / Rdd"));
    lines.push(row("Tab", "Cycle removal mode"));
    lines.push(row("d", "Dry-run & open confirmation"));
    lines.push(Line::raw(""));

    lines.push(section("Other"));
    lines.push(row("?", "Toggle this help screen"));
    lines.push(row("q", "Quit"));
    lines.push(row("Ctrl+C", "Force quit"));
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled("  Press any key to close.", st_muted())));

    f.render_widget(Paragraph::new(lines), inner);
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn main() -> anyhow::Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!();
        eprintln!("  ╔══════════════════════════════════════╗");
        eprintln!("  ║  ✗  Root privileges required.        ║");
        eprintln!("  ║     Run:  sudo apr                   ║");
        eprintln!("  ╚══════════════════════════════════════╝");
        eprintln!();
        std::process::exit(1);
    }

    let mut app = App::new()?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, cursor::Hide)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, cursor::Show)?;
    result
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> anyhow::Result<()> {
    let frame_dur = Duration::from_millis(16); // ~60 fps
    let mut last = Instant::now();

    loop {
        app.process_messages();
        terminal.draw(|f| draw(f, app))?;

        let wait = frame_dur.checked_sub(last.elapsed()).unwrap_or_default();
        if event::poll(wait)? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press {
                    if app.handle_key(k.code, k.modifiers) { break; }
                }
            }
        }

        if last.elapsed() >= frame_dur { last = Instant::now(); }
    }
    Ok(())
}

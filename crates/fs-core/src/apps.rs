//! Handing a path to another application — "Open with ▸", "Open Terminal
//! Here", and the plain "Open" that uses the system's default handler.
//!
//! # Where the list comes from
//!
//! On Windows an installer writes registry keys and the application appears in
//! Explorer's context menu. macOS's equivalent is **Launch Services**: an
//! application declares the document types it handles in its `Info.plist`, and
//! the system registers that when the application is installed. `NSWorkspace`'s
//! `urlsForApplications(toOpen:)` reads exactly that registry — it is what
//! Finder's own "Open With" submenu shows — so our submenu populates itself
//! with no code change when something new is installed.
//!
//! It is also **type-aware**, which a hard-coded list can never be: a `.rs`
//! file offers the two editors that claim it, a folder offers the handful that
//! claim folders, a `.txt` offers everything.
//!
//! # Why that is only half the problem
//!
//! macOS's Mission Control defaults to *"when switching to an application,
//! switch to a Space with open windows for the application"*. So `open -a Foo`
//! **activates** `Foo` and yanks the user to whichever Space `Foo` already has
//! windows on. For a file manager — where the whole point is "act on this
//! folder, *here*" — that is the wrong behavior.
//!
//! The fix is to ask the application for a **new window**, which macOS creates
//! on the Space the user is actually looking at. But Launch Services only says
//! *which* applications can open a file; it never says *how to open a new
//! window* in one. That knowledge cannot be derived, so it lives in
//! [`RECIPES`], a small table keyed by bundle id:
//!
//! | App | Program | Argument |
//! |---|---|---|
//! | Zed | `<bundle>/Contents/MacOS/cli` | `-n` |
//! | VS Code | `<bundle>/Contents/Resources/app/bin/code` | `-n` |
//!
//! An application in the table gets [`LaunchStrategy::NewWindow`] and the
//! guarantee. Everything else gets [`LaunchStrategy::SystemOpen`], which is
//! honest best-effort and may switch Spaces — the same thing Finder does.
//!
//! **Do not "simplify" a recipe to `open -a`.** It looks equivalent, it passes
//! every test that does not involve two Spaces, and it silently reintroduces
//! the bug. [`launch_command`]'s unit tests pin the flags for that reason.
//!
//! # Why the in-bundle CLI and not `zed` on `$PATH`
//!
//! Homebrew's `zed` and `code` are symlinks *into* these bundles, so the two
//! are equivalent from a shell — but a bundled `.app` launched from Finder
//! does not inherit a login shell's `PATH`, so `/opt/homebrew/bin` is simply
//! absent once we ship (plan §7 M8). Addressing the CLI through the bundle
//! Launch Services reported works in both worlds.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// How to open a new window in one known application, keyed by bundle id.
///
/// This table is the *only* thing standing between the user and a Space
/// switch; see the module docs. Adding a row is how a new application earns
/// the guarantee.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NewWindowRecipe {
    pub bundle_id: &'static str,
    /// The CLI inside the bundle, relative to the bundle root.
    pub cli: &'static str,
    /// The argument that means "a new window, please".
    pub flag: &'static str,
}

/// Every application we know how to open a new window in.
pub const RECIPES: &[NewWindowRecipe] = &[
    NewWindowRecipe {
        bundle_id: "dev.zed.Zed",
        cli: "Contents/MacOS/cli",
        flag: "-n",
    },
    NewWindowRecipe {
        bundle_id: "com.microsoft.VSCode",
        cli: "Contents/Resources/app/bin/code",
        flag: "-n",
    },
];

/// How a given application will be launched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchStrategy {
    /// A [`RECIPES`] entry matched and its CLI is present: a new window on the
    /// Space the user is looking at, guaranteed.
    NewWindow {
        program: PathBuf,
        flag: &'static str,
    },
    /// No recipe, or its CLI is missing: hand the path to the system.
    /// Best-effort — this is the path that may switch Spaces.
    SystemOpen,
}

impl LaunchStrategy {
    /// Whether this launch keeps the new window on the current Space.
    pub fn keeps_current_space(&self) -> bool {
        matches!(self, LaunchStrategy::NewWindow { .. })
    }
}

/// One application that can open a given path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalApp {
    /// The bundle's display name, as the menu shows it.
    pub name: String,
    pub bundle_id: String,
    /// The `.app` bundle directory.
    pub bundle: PathBuf,
    pub strategy: LaunchStrategy,
}

impl ExternalApp {
    pub fn label(&self) -> &str {
        &self.name
    }

    /// See [`LaunchStrategy::keeps_current_space`].
    pub fn keeps_current_space(&self) -> bool {
        self.strategy.keeps_current_space()
    }
}

/// Terminals, which Launch Services cannot help with: a terminal is not a
/// document handler, so it never appears in `urlsForApplications(toOpen:)` for
/// a folder. This list stays curated for that reason, not by preference.
///
/// Ordered by preference — the first one installed wins.
pub const TERMINALS: &[(&str, &str)] = &[
    ("com.googlecode.iterm2", "iTerm.app"),
    ("com.mitchellh.ghostty", "Ghostty.app"),
    ("com.github.wez.wezterm", "WezTerm.app"),
    ("com.apple.Terminal", "Terminal.app"),
];

/// A program and its arguments, ready to spawn. Split out from the spawning so
/// the interesting half — *which flags we pass* — is a pure function with unit
/// tests, while the untestable half stays a thin shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

impl LaunchCommand {
    /// The arguments as strings, for tests and diagnostics.
    pub fn arg_strings(&self) -> Vec<String> {
        self.args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }
}

/// The roots to search for the curated terminals, most specific first.
pub fn default_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home).join("Applications"));
    }
    roots.push(PathBuf::from("/Applications"));
    // Terminal.app lives here on every modern macOS.
    roots.push(PathBuf::from("/System/Applications/Utilities"));
    roots
}

/// Pick the launch strategy for a bundle.
///
/// A recipe only applies if its CLI is actually present — a bundle missing its
/// CLI cannot honour the new-window guarantee, so it degrades to
/// [`LaunchStrategy::SystemOpen`] rather than producing a command that would
/// fail to spawn.
pub fn strategy_for(bundle_id: &str, bundle: &Path) -> LaunchStrategy {
    let Some(recipe) = RECIPES.iter().find(|r| r.bundle_id == bundle_id) else {
        return LaunchStrategy::SystemOpen;
    };
    let program = bundle.join(recipe.cli);
    if program.is_file() {
        LaunchStrategy::NewWindow {
            program,
            flag: recipe.flag,
        }
    } else {
        LaunchStrategy::SystemOpen
    }
}

/// Build an [`ExternalApp`] from what Launch Services reported.
pub fn app_from_bundle(name: String, bundle_id: String, bundle: PathBuf) -> ExternalApp {
    let strategy = strategy_for(&bundle_id, &bundle);
    ExternalApp {
        name,
        bundle_id,
        bundle,
        strategy,
    }
}

/// Collapse a Launch Services result into what the menu should show: the
/// default handler first, then the rest, **one row per bundle id**.
///
/// The de-duplication is not hygiene — a real machine reports the same
/// application several times (an auto-updater staging copies under
/// `~/Library/Application Support` is the common case), and three identical
/// "Microsoft Edge" rows is a visibly broken menu.
pub fn dedup_handlers(
    default_first: Option<ExternalApp>,
    rest: Vec<ExternalApp>,
) -> Vec<ExternalApp> {
    let mut seen: Vec<String> = Vec::new();
    let mut out: Vec<ExternalApp> = Vec::new();
    for app in default_first.into_iter().chain(rest) {
        if seen.iter().any(|id| id == &app.bundle_id) {
            continue;
        }
        seen.push(app.bundle_id.clone());
        out.push(app);
    }
    out
}

/// Find the preferred installed terminal under `roots`, if any.
///
/// Takes its roots as an argument so this is testable against a `tempfile`
/// tree on any platform.
pub fn discover_terminal_in(roots: &[PathBuf]) -> Option<ExternalApp> {
    TERMINALS.iter().find_map(|(bundle_id, bundle_name)| {
        roots.iter().find_map(|root| {
            let bundle = root.join(bundle_name);
            bundle.is_dir().then(|| {
                let name = bundle_name.trim_end_matches(".app").to_string();
                app_from_bundle(name, (*bundle_id).to_string(), bundle)
            })
        })
    })
}

/// Build the command that opens `target` in `app`.
///
/// The flags here are load-bearing; see the module docs before changing them.
pub fn launch_command(app: &ExternalApp, target: &Path) -> LaunchCommand {
    match &app.strategy {
        LaunchStrategy::NewWindow { program, flag } => LaunchCommand {
            program: program.clone(),
            args: vec![OsString::from(*flag), target.as_os_str().to_os_string()],
        },
        LaunchStrategy::SystemOpen => LaunchCommand {
            program: PathBuf::from("/usr/bin/open"),
            args: vec![
                OsString::from("-a"),
                app.bundle.as_os_str().to_os_string(),
                target.as_os_str().to_os_string(),
            ],
        },
    }
}

/// Build the command for plain "Open" — hand the path to whatever the system
/// has registered as its default handler.
pub fn open_default_command(target: &Path) -> LaunchCommand {
    LaunchCommand {
        program: PathBuf::from("/usr/bin/open"),
        args: vec![target.as_os_str().to_os_string()],
    }
}

/// The folder a "here" command should act on: the path itself when it is a
/// directory, otherwise its parent.
pub fn enclosing_directory(target: &Path, is_dir: bool) -> Option<PathBuf> {
    if is_dir {
        Some(target.to_path_buf())
    } else {
        target.parent().map(Path::to_path_buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle_with_cli(dir: &Path, name: &str, cli: Option<&str>) -> PathBuf {
        let bundle = dir.join(name);
        std::fs::create_dir_all(&bundle).unwrap();
        if let Some(cli) = cli {
            let path = bundle.join(cli);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        }
        bundle
    }

    fn app(bundle_id: &str, bundle: PathBuf) -> ExternalApp {
        app_from_bundle(bundle_id.to_string(), bundle_id.to_string(), bundle)
    }

    // ---- the recipes, and the Space guarantee they carry ----

    #[test]
    fn a_recipe_app_gets_the_new_window_strategy() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = bundle_with_cli(tmp.path(), "Zed.app", Some("Contents/MacOS/cli"));
        let strategy = strategy_for("dev.zed.Zed", &bundle);
        assert_eq!(
            strategy,
            LaunchStrategy::NewWindow {
                program: bundle.join("Contents/MacOS/cli"),
                flag: "-n",
            }
        );
        assert!(strategy.keeps_current_space());
    }

    /// An application with no recipe still appears in the menu — it just
    /// cannot promise the Space, which is what `keeps_current_space` reports.
    #[test]
    fn an_unknown_app_falls_back_to_system_open() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = bundle_with_cli(tmp.path(), "Whatever.app", None);
        let strategy = strategy_for("com.example.Whatever", &bundle);
        assert_eq!(strategy, LaunchStrategy::SystemOpen);
        assert!(!strategy.keeps_current_space());
    }

    /// A recipe whose CLI is missing must degrade rather than build a command
    /// that cannot spawn.
    #[test]
    fn a_recipe_without_its_cli_degrades_to_system_open() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = bundle_with_cli(tmp.path(), "Zed.app", None);
        assert_eq!(
            strategy_for("dev.zed.Zed", &bundle),
            LaunchStrategy::SystemOpen
        );
    }

    #[test]
    fn every_recipe_names_a_flag_and_a_cli() {
        for recipe in RECIPES {
            assert!(!recipe.flag.is_empty(), "{} has no flag", recipe.bundle_id);
            assert!(!recipe.cli.is_empty(), "{} has no cli", recipe.bundle_id);
        }
    }

    // ---- launch commands ----

    #[test]
    fn a_new_window_launch_uses_the_bundle_cli_and_its_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = bundle_with_cli(tmp.path(), "Zed.app", Some("Contents/MacOS/cli"));
        let app = app("dev.zed.Zed", bundle.clone());
        let cmd = launch_command(&app, Path::new("/tmp/project"));
        assert_eq!(cmd.program, bundle.join("Contents/MacOS/cli"));
        assert_eq!(cmd.arg_strings(), vec!["-n", "/tmp/project"]);
    }

    /// The regression this module exists to prevent: an application with a
    /// recipe must never be launched by *activating* it, because that is what
    /// switches Spaces.
    #[test]
    fn a_recipe_app_is_never_launched_by_activating_the_application() {
        let tmp = tempfile::tempdir().unwrap();
        for recipe in RECIPES {
            let bundle = bundle_with_cli(
                &tmp.path().join(recipe.bundle_id),
                "App.app",
                Some(recipe.cli),
            );
            let app = app(recipe.bundle_id, bundle);
            let cmd = launch_command(&app, Path::new("/tmp/project"));
            assert_ne!(
                cmd.program,
                PathBuf::from("/usr/bin/open"),
                "{} must not be launched with `open`, which switches Spaces",
                recipe.bundle_id
            );
            assert!(cmd.arg_strings().contains(&recipe.flag.to_string()));
        }
    }

    #[test]
    fn a_system_open_launch_goes_through_open_with_the_bundle() {
        let app = ExternalApp {
            name: "Whatever".into(),
            bundle_id: "com.example.Whatever".into(),
            bundle: PathBuf::from("/Applications/Whatever.app"),
            strategy: LaunchStrategy::SystemOpen,
        };
        let cmd = launch_command(&app, Path::new("/tmp/a.txt"));
        assert_eq!(cmd.program, PathBuf::from("/usr/bin/open"));
        assert_eq!(
            cmd.arg_strings(),
            vec!["-a", "/Applications/Whatever.app", "/tmp/a.txt"]
        );
    }

    #[test]
    fn open_default_hands_the_path_to_the_system() {
        let cmd = open_default_command(Path::new("/tmp/a.txt"));
        assert_eq!(cmd.program, PathBuf::from("/usr/bin/open"));
        assert_eq!(cmd.arg_strings(), vec!["/tmp/a.txt"]);
    }

    #[test]
    fn a_path_with_spaces_is_passed_as_one_argument() {
        let app = ExternalApp {
            name: "X".into(),
            bundle_id: "com.example.X".into(),
            bundle: PathBuf::from("/Applications/X.app"),
            strategy: LaunchStrategy::SystemOpen,
        };
        let cmd = launch_command(&app, Path::new("/tmp/two words/a b.txt"));
        assert_eq!(cmd.arg_strings().last().unwrap(), "/tmp/two words/a b.txt");
    }

    // ---- de-duplication and ordering ----

    #[test]
    fn the_default_handler_comes_first() {
        let d = app("com.example.Default", PathBuf::from("/Applications/D.app"));
        let other = app("com.example.Other", PathBuf::from("/Applications/O.app"));
        let out = dedup_handlers(Some(d.clone()), vec![other, d]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].bundle_id, "com.example.Default");
        assert_eq!(out[1].bundle_id, "com.example.Other");
    }

    /// A real machine reports the same application from several staged copies;
    /// three identical rows is a visibly broken menu.
    #[test]
    fn repeated_bundle_ids_collapse_to_one_row() {
        let a = app(
            "com.microsoft.edgemac",
            PathBuf::from("/Applications/Edge.app"),
        );
        let staged = app(
            "com.microsoft.edgemac",
            PathBuf::from("/Users/x/Library/Application Support/staged/Edge.app"),
        );
        let out = dedup_handlers(None, vec![a, staged.clone(), staged]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].bundle, PathBuf::from("/Applications/Edge.app"));
    }

    #[test]
    fn dedup_of_nothing_is_nothing() {
        assert!(dedup_handlers(None, Vec::new()).is_empty());
    }

    // ---- terminals (curated: not document handlers) ----

    #[test]
    fn the_preferred_installed_terminal_wins() {
        let tmp = tempfile::tempdir().unwrap();
        bundle_with_cli(tmp.path(), "Terminal.app", None);
        bundle_with_cli(tmp.path(), "iTerm.app", None);
        let found = discover_terminal_in(&[tmp.path().to_path_buf()]).unwrap();
        assert_eq!(
            found.bundle_id, "com.googlecode.iterm2",
            "iTerm outranks Terminal"
        );
    }

    #[test]
    fn terminal_discovery_finds_nothing_when_none_is_installed() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(discover_terminal_in(&[tmp.path().to_path_buf()]).is_none());
    }

    #[test]
    fn a_terminal_is_launched_through_open_at_the_directory() {
        let tmp = tempfile::tempdir().unwrap();
        bundle_with_cli(tmp.path(), "Terminal.app", None);
        let term = discover_terminal_in(&[tmp.path().to_path_buf()]).unwrap();
        let cmd = launch_command(&term, Path::new("/tmp/here"));
        assert_eq!(cmd.program, PathBuf::from("/usr/bin/open"));
        assert_eq!(cmd.arg_strings().last().unwrap(), "/tmp/here");
    }

    // ---- "here" resolution ----

    #[test]
    fn here_is_the_folder_itself_for_a_directory() {
        assert_eq!(
            enclosing_directory(Path::new("/a/b"), true),
            Some(PathBuf::from("/a/b"))
        );
    }

    #[test]
    fn here_is_the_parent_for_a_file() {
        assert_eq!(
            enclosing_directory(Path::new("/a/b/c.txt"), false),
            Some(PathBuf::from("/a/b"))
        );
    }

    #[test]
    fn here_is_nothing_for_a_parentless_path() {
        assert_eq!(enclosing_directory(Path::new("/"), false), None);
    }
}

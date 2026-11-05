//! Handing a path to another application — "Open with ▸ Zed", "Open Terminal
//! Here", and the plain "Open" that uses the system's default handler.
//!
//! # Why this is not just `open -a`
//!
//! macOS's Mission Control has "when switching to an application, switch to a
//! Space with open windows for the application" on by default. `open -a Foo`
//! **activates** `Foo`, so if `Foo` already has a window on another Space, the
//! user is yanked to that Space and their new window appears somewhere they
//! were not looking. For a file manager — where the whole point is "act on
//! this folder, here" — that is the wrong behavior, and it is the single
//! reason this module exists instead of a one-line `Command::new("open")`.
//!
//! The fix is to ask the target application for a **new window** rather than
//! to activate it, because a new window is created on the Space the user is
//! actually on. Both editors we support expose exactly that through their own
//! CLI:
//!
//! | App | Program | Argument |
//! |---|---|---|
//! | Zed | `<bundle>/Contents/MacOS/cli` | `-n` |
//! | VS Code | `<bundle>/Contents/Resources/app/bin/code` | `-n` |
//! | Terminal | `/usr/bin/open` | `-a <bundle>` (a path always opens a new window) |
//!
//! **Do not "simplify" these to `open -a`.** It looks equivalent, it passes
//! every test that does not involve two Spaces, and it silently reintroduces
//! the bug. [`launch_command`]'s unit tests pin the flags for that reason.
//!
//! # Why the in-bundle CLI and not `zed` on `$PATH`
//!
//! Homebrew's `zed` and `code` are symlinks *into* these bundles, so the two
//! are equivalent from a shell — but a bundled `.app` launched from Finder
//! does not inherit a login shell's `PATH`, so `/opt/homebrew/bin` is simply
//! absent once we ship (plan §7 M8). Addressing the CLI through the bundle we
//! discovered works in both worlds.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// An application this app knows how to hand a path to.
///
/// Deliberately a closed set, not a scan of every installed app: each variant
/// carries knowledge of *how to open a new window in it* (see the module
/// docs), which cannot be derived from a bundle alone. "Open with" over
/// arbitrary applications is the system's job — that is what [`open_default`]
/// is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AppKind {
    Zed,
    VsCode,
    Terminal,
}

impl AppKind {
    /// Menu order, and the order [`discover_in`] returns.
    pub const ALL: [AppKind; 3] = [AppKind::Zed, AppKind::VsCode, AppKind::Terminal];

    /// How the app names itself, for menu labels.
    pub fn label(self) -> &'static str {
        match self {
            AppKind::Zed => "Zed",
            AppKind::VsCode => "Visual Studio Code",
            AppKind::Terminal => "Terminal",
        }
    }

    /// The bundle directory name to look for under each root.
    pub fn bundle_name(self) -> &'static str {
        match self {
            AppKind::Zed => "Zed.app",
            AppKind::VsCode => "Visual Studio Code.app",
            AppKind::Terminal => "Terminal.app",
        }
    }

    /// The CLI inside the bundle that can open a **new window**, relative to
    /// the bundle root. `None` for apps driven through `/usr/bin/open`.
    ///
    /// When this is `Some`, discovery requires it to exist: a bundle whose CLI
    /// is missing cannot honour the new-window guarantee, and offering it
    /// anyway would silently degrade to the Space-switching behavior this
    /// module exists to avoid.
    pub fn new_window_cli(self) -> Option<&'static str> {
        match self {
            AppKind::Zed => Some("Contents/MacOS/cli"),
            AppKind::VsCode => Some("Contents/Resources/app/bin/code"),
            AppKind::Terminal => None,
        }
    }

    /// Whether this app wants a **directory**. Terminal is opened *at* a
    /// folder; the editors happily take either.
    pub fn wants_directory(self) -> bool {
        matches!(self, AppKind::Terminal)
    }
}

/// An [`AppKind`] actually found on this machine, with the bundle it was found
/// at. Only installed apps become one, so the menu lists only what exists —
/// the same rule the sidebar's Locations section follows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalApp {
    pub kind: AppKind,
    /// The `.app` bundle directory.
    pub bundle: PathBuf,
}

impl ExternalApp {
    pub fn label(&self) -> &'static str {
        self.kind.label()
    }
}

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

/// The default roots to search for application bundles, most specific first.
///
/// `~/Applications` comes before `/Applications` so a per-user install wins,
/// which is how macOS itself resolves duplicates.
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

/// Find which of [`AppKind::ALL`] are installed under `roots`.
///
/// Takes its roots as an argument rather than reading the real filesystem so
/// the whole of discovery is testable against a `tempfile` tree, on any
/// platform. Results keep `AppKind::ALL`'s order, so the menu is stable
/// regardless of which roots matched.
pub fn discover_in(roots: &[PathBuf]) -> Vec<ExternalApp> {
    AppKind::ALL
        .iter()
        .filter_map(|&kind| {
            roots.iter().find_map(|root| {
                let bundle = root.join(kind.bundle_name());
                if !bundle.is_dir() {
                    return None;
                }
                // A bundle without its CLI cannot open a new window on this
                // Space — see `new_window_cli`.
                if let Some(cli) = kind.new_window_cli()
                    && !bundle.join(cli).is_file()
                {
                    return None;
                }
                Some(ExternalApp { kind, bundle })
            })
        })
        .collect()
}

/// Build the command that opens `target` in `app` **in a new window on the
/// current Space**.
///
/// The flags here are load-bearing; see the module docs before changing them.
pub fn launch_command(app: &ExternalApp, target: &Path) -> LaunchCommand {
    match app.kind.new_window_cli() {
        // Editors: their own CLI, with the new-window flag.
        Some(cli) => LaunchCommand {
            program: app.bundle.join(cli),
            args: vec![OsString::from("-n"), target.as_os_str().to_os_string()],
        },
        // Terminal: `open -a <bundle> <dir>` already opens a new window at
        // that directory, and takes no new-window flag of its own.
        None => LaunchCommand {
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
///
/// This one deliberately *does* use `/usr/bin/open` with no new-window flag:
/// there is no general way to ask an unknown application for a new window, and
/// matching the system's own behavior is the honest default. The Space-switch
/// caveat in the module docs therefore still applies to this path alone.
pub fn open_default_command(target: &Path) -> LaunchCommand {
    LaunchCommand {
        program: PathBuf::from("/usr/bin/open"),
        args: vec![target.as_os_str().to_os_string()],
    }
}

/// The folder a "here" command should act on: the path itself when it is a
/// directory, otherwise its parent.
///
/// Right-clicking a *file* and asking for a terminal means "a terminal where
/// this file lives", which is what Explorer and Finder both do.
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

    /// Build a fake bundle tree so discovery is testable off a real Mac.
    fn fake_root(dir: &Path, kind: AppKind, with_cli: bool) {
        let bundle = dir.join(kind.bundle_name());
        std::fs::create_dir_all(&bundle).unwrap();
        if with_cli && let Some(cli) = kind.new_window_cli() {
            let cli_path = bundle.join(cli);
            std::fs::create_dir_all(cli_path.parent().unwrap()).unwrap();
            std::fs::write(&cli_path, b"#!/bin/sh\n").unwrap();
        }
    }

    #[test]
    fn discovery_lists_only_what_is_installed() {
        let tmp = tempfile::tempdir().unwrap();
        fake_root(tmp.path(), AppKind::Zed, true);
        // VS Code and Terminal deliberately absent.
        let found = discover_in(&[tmp.path().to_path_buf()]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, AppKind::Zed);
    }

    #[test]
    fn discovery_is_empty_when_nothing_is_installed() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(discover_in(&[tmp.path().to_path_buf()]).is_empty());
    }

    #[test]
    fn discovery_keeps_a_stable_order_regardless_of_root_order() {
        let tmp = tempfile::tempdir().unwrap();
        fake_root(tmp.path(), AppKind::Terminal, true);
        fake_root(tmp.path(), AppKind::VsCode, true);
        fake_root(tmp.path(), AppKind::Zed, true);
        let found = discover_in(&[tmp.path().to_path_buf()]);
        let kinds: Vec<AppKind> = found.iter().map(|a| a.kind).collect();
        assert_eq!(kinds, AppKind::ALL.to_vec());
    }

    /// The guarantee from the module docs: a bundle we cannot open a new
    /// window in is not offered at all, rather than offered and degrading to
    /// a Space switch.
    #[test]
    fn a_bundle_without_its_cli_is_not_offered() {
        let tmp = tempfile::tempdir().unwrap();
        fake_root(tmp.path(), AppKind::Zed, false);
        assert!(discover_in(&[tmp.path().to_path_buf()]).is_empty());
    }

    #[test]
    fn an_earlier_root_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        fake_root(&first, AppKind::Zed, true);
        fake_root(&second, AppKind::Zed, true);
        let found = discover_in(&[first.clone(), second]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].bundle, first.join("Zed.app"));
    }

    // ---- the flags that keep the new window on this Space ----

    #[test]
    fn zed_is_launched_through_its_own_cli_with_the_new_window_flag() {
        let app = ExternalApp {
            kind: AppKind::Zed,
            bundle: PathBuf::from("/Applications/Zed.app"),
        };
        let cmd = launch_command(&app, Path::new("/tmp/project"));
        assert_eq!(
            cmd.program,
            PathBuf::from("/Applications/Zed.app/Contents/MacOS/cli")
        );
        assert_eq!(cmd.arg_strings(), vec!["-n", "/tmp/project"]);
    }

    #[test]
    fn vscode_is_launched_through_its_own_cli_with_the_new_window_flag() {
        let app = ExternalApp {
            kind: AppKind::VsCode,
            bundle: PathBuf::from("/Applications/Visual Studio Code.app"),
        };
        let cmd = launch_command(&app, Path::new("/tmp/project"));
        assert_eq!(
            cmd.program,
            PathBuf::from("/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code")
        );
        assert_eq!(cmd.arg_strings(), vec!["-n", "/tmp/project"]);
    }

    /// The regression this module exists to prevent: neither editor may be
    /// launched by activating the application, because that is what switches
    /// Spaces.
    #[test]
    fn no_editor_is_launched_by_activating_the_application() {
        for kind in [AppKind::Zed, AppKind::VsCode] {
            let app = ExternalApp {
                kind,
                bundle: PathBuf::from("/Applications").join(kind.bundle_name()),
            };
            let cmd = launch_command(&app, Path::new("/tmp/project"));
            assert_ne!(
                cmd.program,
                PathBuf::from("/usr/bin/open"),
                "{} must not be launched with `open`, which switches Spaces",
                kind.label()
            );
            assert!(
                cmd.arg_strings().contains(&"-n".to_string()),
                "{} must be asked for a new window",
                kind.label()
            );
        }
    }

    #[test]
    fn terminal_opens_at_the_directory_through_open() {
        let app = ExternalApp {
            kind: AppKind::Terminal,
            bundle: PathBuf::from("/System/Applications/Utilities/Terminal.app"),
        };
        let cmd = launch_command(&app, Path::new("/tmp/here"));
        assert_eq!(cmd.program, PathBuf::from("/usr/bin/open"));
        assert_eq!(
            cmd.arg_strings(),
            vec![
                "-a",
                "/System/Applications/Utilities/Terminal.app",
                "/tmp/here"
            ]
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
            kind: AppKind::Zed,
            bundle: PathBuf::from("/Applications/Zed.app"),
        };
        let cmd = launch_command(&app, Path::new("/tmp/two words/a b.txt"));
        assert_eq!(cmd.args.len(), 2);
        assert_eq!(cmd.arg_strings()[1], "/tmp/two words/a b.txt");
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

    #[test]
    fn only_terminal_demands_a_directory() {
        assert!(AppKind::Terminal.wants_directory());
        assert!(!AppKind::Zed.wants_directory());
        assert!(!AppKind::VsCode.wants_directory());
    }
}

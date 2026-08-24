//! Extended file attributes for the info panel (ARCHITECTURE.md §6, M5): unix
//! permissions, the multi-selection summary, and the previewable-type gate.
//!
//! Everything here is pure and clock-free — no filesystem access at all — so it
//! is safe to call from the UI thread and asserts exact values in tests on every
//! platform. The parts that *do* need an OS call live behind
//! [`crate::Platform::file_attrs`], which fills in a [`FileAttrs`].

use std::path::Path;
use std::time::SystemTime;

use crate::entry::FileEntry;

/// Which of the three unix permission classes a bit belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PermClass {
    Owner,
    Group,
    Others,
}

/// One permission bit within a [`PermClass`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PermBit {
    Read,
    Write,
    Exec,
}

/// The low 12 bits of `st_mode`: the nine rwx bits plus setuid, setgid and the
/// sticky bit. Deliberately *not* the file type bits — this is a permission
/// value, and the entry's kind is already [`crate::EntryKind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UnixPerms {
    /// Masked to `0o7777` on construction.
    mode: u32,
}

/// Mask covering everything [`UnixPerms`] keeps: `rwxrwxrwx` + setuid/setgid/sticky.
const PERM_MASK: u32 = 0o7777;
/// setuid, setgid and sticky — the bits that push [`UnixPerms::octal`] to four digits.
const SPECIAL_MASK: u32 = 0o7000;

impl UnixPerms {
    /// Take the permission bits out of a raw `st_mode` (file-type bits ignored).
    pub fn from_mode(mode: u32) -> Self {
        Self {
            mode: mode & PERM_MASK,
        }
    }

    /// The permission bits, already masked to `0o7777`.
    pub fn mode(&self) -> u32 {
        self.mode
    }

    /// Octal notation as the info panel shows it: three digits normally
    /// (`"644"`), four when any of setuid/setgid/sticky is set (`"4755"`,
    /// `"1777"`) — a leading zero would read as C source, not as a mode.
    pub fn octal(&self) -> String {
        if self.mode & SPECIAL_MASK == 0 {
            format!("{:03o}", self.mode)
        } else {
            format!("{:04o}", self.mode)
        }
    }

    /// Symbolic notation as `ls -l` writes it, without the leading type
    /// character: nine bytes, e.g. `"rw-r--r--"`.
    ///
    /// The special bits fold into the class they modify, exactly as `ls` does:
    /// setuid/setgid replace the class's `x` with `s` (or `-` with `S` when the
    /// class is not executable), and sticky replaces the others' `x` with `t`
    /// (or `-` with `T`).
    pub fn symbolic(&self) -> String {
        let mut out = String::with_capacity(9);
        for (class, special, set, unset) in [
            (PermClass::Owner, 0o4000, 's', 'S'),
            (PermClass::Group, 0o2000, 's', 'S'),
            (PermClass::Others, 0o1000, 't', 'T'),
        ] {
            out.push(if self.allows(class, PermBit::Read) {
                'r'
            } else {
                '-'
            });
            out.push(if self.allows(class, PermBit::Write) {
                'w'
            } else {
                '-'
            });
            let exec = self.allows(class, PermBit::Exec);
            out.push(match (self.mode & special != 0, exec) {
                (true, true) => set,
                (true, false) => unset,
                (false, true) => 'x',
                (false, false) => '-',
            });
        }
        out
    }

    /// Whether `class` holds `bit`.
    pub fn allows(&self, class: PermClass, bit: PermBit) -> bool {
        let shift = match class {
            PermClass::Owner => 6,
            PermClass::Group => 3,
            PermClass::Others => 0,
        };
        let bit = match bit {
            PermBit::Read => 0o4,
            PermBit::Write => 0o2,
            PermBit::Exec => 0o1,
        };
        self.mode & (bit << shift) != 0
    }
}

/// Attributes that need an OS call beyond [`crate::Vfs::metadata`], as produced
/// by [`crate::Platform::file_attrs`].
///
/// Every field is independently optional: a platform that cannot answer one
/// lookup degrades that field ([`None`], or `false` for the flags) rather than
/// failing the whole call, so the info panel shows what is known instead of
/// nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileAttrs {
    /// Unix permission bits, when the filesystem has them.
    pub perms: Option<UnixPerms>,
    /// Owning user's name, falling back to the uid rendered as a string.
    pub owner: Option<String>,
    /// Owning group's name, falling back to the gid rendered as a string.
    pub group: Option<String>,
    /// macOS user-immutable flag (`UF_IMMUTABLE`) — Finder's "Locked".
    pub locked: bool,
    /// macOS "Date Added" (when the item entered its containing folder);
    /// [`None`] on platforms that do not record it.
    pub added: Option<SystemTime>,
    /// Whether the OS hides this item's extension in its own UI.
    pub extension_hidden: bool,
    /// Localized type description, e.g. `"JPEG image"`.
    pub type_description: Option<String>,
}

/// Summary of a multi-selection, for the info panel's "N items" state
/// (ARCHITECTURE.md §2, "multi-selection summary").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SelectionSummary {
    /// Non-directory entries (files, and symlinks that do not resolve to a directory).
    pub files: usize,
    /// Directory-like entries, per [`FileEntry::is_dir_like`].
    pub dirs: usize,
    /// Summed size of the **files** only. A directory's `size` is its own inode
    /// size, not its content's, so including it would report a number nobody
    /// asked for; recursive folder sizing is a separate, cancellable job.
    pub total_size: u64,
}

impl SelectionSummary {
    /// Total number of selected entries.
    pub fn count(&self) -> usize {
        self.files + self.dirs
    }
}

/// Summarize a selection: how many files, how many folders, and the files' total size.
pub fn summarize<'a>(entries: impl Iterator<Item = &'a FileEntry>) -> SelectionSummary {
    let mut summary = SelectionSummary::default();
    for entry in entries {
        if entry.is_dir_like() {
            summary.dirs += 1;
        } else {
            summary.files += 1;
            summary.total_size = summary.total_size.saturating_add(entry.size);
        }
    }
    summary
}

/// Largest file we will ask the OS to preview, in bytes (64 MiB).
///
/// A preview costs a decode of the whole file, so the ceiling is what keeps the
/// info panel from stalling on a multi-gigabyte disk image or video that
/// QuickLook would happily start chewing on. It is generous enough for the
/// photos, PDFs and documents the panel is actually for. The boundary is
/// inclusive: a file of exactly this size is still previewable.
pub const PREVIEW_SIZE_CEILING: u64 = 64 * 1024 * 1024;

/// Extensions worth asking QuickLook about (lowercase, no dot): images, PDF,
/// plain text / markup / source, audio, video, rich text and office documents.
///
/// An allowlist rather than a denylist on purpose — the long tail of file types
/// is overwhelmingly *not* previewable (`.o`, `.dylib`, `.bin`, every unknown
/// extension), and asking about each one costs an XPC round-trip apiece.
const PREVIEWABLE_EXTENSIONS: &[&str] = &[
    // images
    "png", "jpg", "jpeg", "gif", "bmp", "tif", "tiff", "webp", "heic", "heif", "avif", "svg",
    "icns", "ico", // documents
    "pdf", "rtf", "epub", // plain text and markup
    "txt", "text", "log", "md", "markdown", "csv", "tsv", "json", "yaml", "yml", "toml", "xml",
    "html", "htm", "css", "plist", // source
    "rs", "c", "h", "cc", "cpp", "hpp", "m", "mm", "swift", "py", "rb", "go", "java", "kt", "js",
    "jsx", "ts", "tsx", "sh", "zsh", "bash", "sql", // audio
    "mp3", "m4a", "aac", "wav", "aiff", "aif", "flac", // video
    "mp4", "m4v", "mov", "avi", "mkv", // office / iWork
    "doc", "docx", "xls", "xlsx", "ppt", "pptx", "pages", "numbers", "key",
    // camera raw and the design formats QuickLook renders: exactly the files
    // whose owner cares most about a preview, and (unlike `.o`) ones the OS
    // reliably has a representation for.
    "dng", "cr2", "cr3", "nef", "arw", "raf", "orf", "rw2", "psd", "ai", "eps",
];

/// Whether it is worth asking the OS for a preview of `path` at `size` bytes.
///
/// Pure: it looks at the extension and the size only, and never touches the
/// disk (the UI thread calls this). Extension matching is case-insensitive.
///
/// Directories are excluded by [`is_previewable_entry`], which knows the entry's
/// kind; this path-and-size form cannot tell a folder named `Album.png` from a
/// file, so prefer the entry form whenever a [`FileEntry`] is at hand.
pub fn is_previewable(path: &Path, size: u64) -> bool {
    if size > PREVIEW_SIZE_CEILING {
        return false;
    }
    path.extension()
        .map(|ext| ext.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|ext| PREVIEWABLE_EXTENSIONS.contains(&ext.as_str()))
}

/// [`is_previewable`] for a listing row: directories (and symlinks to
/// directories) are never previewable, whatever their name looks like.
pub fn is_previewable_entry(entry: &FileEntry) -> bool {
    !entry.is_dir_like() && is_previewable(&entry.path, entry.size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{EntryKind, TargetKind};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn entry(name: &str, kind: EntryKind, size: u64) -> FileEntry {
        FileEntry {
            path: Arc::from(PathBuf::from("/root").join(name)),
            name: name.into(),
            kind,
            size,
            modified: SystemTime::UNIX_EPOCH,
            created: None,
            hidden: false,
        }
    }

    fn file(name: &str, size: u64) -> FileEntry {
        entry(name, EntryKind::File, size)
    }

    fn dir(name: &str) -> FileEntry {
        entry(name, EntryKind::Dir, 96)
    }

    #[test]
    fn from_mode_keeps_only_the_permission_bits() {
        // 0o100644 is a regular file, mode 644: the type bits must not survive.
        assert_eq!(UnixPerms::from_mode(0o100_644).mode(), 0o644);
        assert_eq!(UnixPerms::from_mode(0o040_755).mode(), 0o755);
        assert_eq!(UnixPerms::from_mode(0o104_755).mode(), 0o4755);
    }

    #[test]
    fn octal_is_three_digits_unless_a_special_bit_is_set() {
        for (mode, expected) in [
            (0o000, "000"),
            (0o644, "644"),
            (0o755, "755"),
            (0o777, "777"),
            (0o4755, "4755"), // setuid
            (0o2755, "2755"), // setgid
            (0o1777, "1777"), // sticky
            (0o7777, "7777"), // all three
            (0o7000, "7000"), // special bits only
        ] {
            assert_eq!(UnixPerms::from_mode(mode).octal(), expected, "{mode:o}");
        }
    }

    #[test]
    fn symbolic_matches_ls_including_the_special_bits() {
        for (mode, expected) in [
            (0o000, "---------"),
            (0o644, "rw-r--r--"),
            (0o755, "rwxr-xr-x"),
            (0o777, "rwxrwxrwx"),
            (0o600, "rw-------"),
            (0o4755, "rwsr-xr-x"), // setuid over an executable owner
            (0o4655, "rwSr-xr-x"), // setuid without owner execute
            (0o2755, "rwxr-sr-x"), // setgid over an executable group
            (0o2745, "rwxr-Sr-x"), // setgid without group execute
            (0o1777, "rwxrwxrwt"), // sticky over executable others
            (0o1776, "rwxrwxrwT"), // sticky without others execute
            (0o7777, "rwsrwsrwt"),
        ] {
            let symbolic = UnixPerms::from_mode(mode).symbolic();
            assert_eq!(symbolic, expected, "{mode:o}");
            assert_eq!(symbolic.len(), 9, "{mode:o}");
        }
    }

    #[test]
    fn allows_covers_every_class_and_bit() {
        let classes = [PermClass::Owner, PermClass::Group, PermClass::Others];
        let bits = [PermBit::Read, PermBit::Write, PermBit::Exec];

        let none = UnixPerms::from_mode(0o000);
        let all = UnixPerms::from_mode(0o777);
        for class in classes {
            for bit in bits {
                assert!(!none.allows(class, bit), "{class:?}/{bit:?} on 000");
                assert!(all.allows(class, bit), "{class:?}/{bit:?} on 777");
            }
        }

        // 0o642: owner rw-, group r--, others -w-.
        let mixed = UnixPerms::from_mode(0o642);
        let expected = [
            (PermClass::Owner, [true, true, false]),
            (PermClass::Group, [true, false, false]),
            (PermClass::Others, [false, true, false]),
        ];
        for (class, wanted) in expected {
            for (bit, want) in bits.into_iter().zip(wanted) {
                assert_eq!(mixed.allows(class, bit), want, "{class:?}/{bit:?}");
            }
        }

        // Special bits are not permission bits: 0o7000 grants nothing.
        let special_only = UnixPerms::from_mode(0o7000);
        for class in classes {
            for bit in bits {
                assert!(!special_only.allows(class, bit), "{class:?}/{bit:?}");
            }
        }
    }

    #[test]
    fn summarize_counts_files_and_dirs_and_sums_file_sizes_only() {
        let entries = [
            file("a.txt", 100),
            dir("photos"),
            file("b.bin", 1_000),
            entry(
                "link-to-dir",
                EntryKind::Symlink {
                    target_kind: TargetKind::Dir,
                },
                0,
            ),
            entry(
                "link-to-file",
                EntryKind::Symlink {
                    target_kind: TargetKind::File,
                },
                7,
            ),
        ];
        let summary = summarize(entries.iter());
        assert_eq!(
            summary,
            SelectionSummary {
                files: 3, // a.txt, b.bin, link-to-file
                dirs: 2,  // photos, link-to-dir
                total_size: 1_107,
            }
        );
        assert_eq!(summary.count(), 5);
    }

    #[test]
    fn summarize_of_nothing_is_all_zeroes() {
        let summary = summarize(std::iter::empty());
        assert_eq!(summary, SelectionSummary::default());
        assert_eq!(summary.count(), 0);
    }

    #[test]
    fn summarize_saturates_instead_of_overflowing() {
        let entries = [file("huge-a", u64::MAX), file("huge-b", 2)];
        assert_eq!(summarize(entries.iter()).total_size, u64::MAX);
    }

    #[test]
    fn is_previewable_accepts_the_allowlist_case_insensitively() {
        for name in [
            "photo.jpg",
            "photo.JPG",
            "scan.HEIC",
            "report.pdf",
            "notes.md",
            "readme.txt",
            "main.rs",
            "song.mp3",
            "clip.mov",
            "deck.pptx",
            "styles.css",
            // Camera raw and the design formats: the icon grid already shows
            // QuickLook thumbnails for these, so the panel must not be the one
            // place that refuses to preview them.
            "IMG_0001.DNG",
            "shot.cr2",
            "shot.NEF",
            "shot.arw",
            "poster.psd",
            "logo.ai",
            "logo.eps",
        ] {
            assert!(is_previewable(Path::new(name), 1_024), "{name}");
        }
    }

    #[test]
    fn is_previewable_rejects_unlisted_and_extensionless_paths() {
        for name in [
            "object.o",
            "lib.dylib",
            "disk.dmg",
            "archive.zip",
            "Makefile",
            "no-extension",
            ".gitignore", // a leading dot is a name, not an extension
        ] {
            assert!(!is_previewable(Path::new(name), 1_024), "{name}");
        }
    }

    #[test]
    fn is_previewable_ceiling_is_inclusive() {
        let path = Path::new("/root/photo.png");
        assert!(
            is_previewable(path, 0),
            "an empty file is still previewable"
        );
        assert!(is_previewable(path, PREVIEW_SIZE_CEILING - 1));
        assert!(is_previewable(path, PREVIEW_SIZE_CEILING));
        assert!(!is_previewable(path, PREVIEW_SIZE_CEILING + 1));
        assert!(!is_previewable(path, u64::MAX));
    }

    #[test]
    fn is_previewable_entry_excludes_directories_whatever_they_are_called() {
        assert!(is_previewable_entry(&file("photo.png", 10)));
        assert!(!is_previewable_entry(&dir("Album.png")));
        assert!(!is_previewable_entry(&entry(
            "shortcut.png",
            EntryKind::Symlink {
                target_kind: TargetKind::Dir
            },
            0
        )));
        assert!(is_previewable_entry(&entry(
            "shortcut.png",
            EntryKind::Symlink {
                target_kind: TargetKind::File
            },
            0
        )));
        assert!(!is_previewable_entry(&file("photo.png", u64::MAX)));
    }
}

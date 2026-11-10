//! What *sort* of thing a file is, from its name alone.
//!
//! This is the classification behind the theme's `file_colors` (plan §6): a
//! folder is blue, an image is violet, code is green, and so on. It is
//! deliberately **not** a content sniff and **not** a Uniform Type Identifier
//! lookup — both are OS calls, and this runs for every visible row on every
//! repaint. An extension table is wrong occasionally and free always, which
//! is the right trade for a tint.
//!
//! Lives in fs-core rather than the app so it is portable and testable
//! headless, and so the icon grid, the details list and the info panel cannot
//! disagree about what a `.rs` file is.

use std::path::Path;

/// The kinds the theme carries a colour for.
///
/// Mirrors `theme::FileColors`' fields exactly; adding a variant here means
/// adding a colour there, which the exhaustive `match` in
/// `crate::icons::entry_icon_color` makes a compile error rather than a
/// silently grey icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FileKind {
    Folder,
    Image,
    Code,
    Archive,
    Audio,
    Video,
    Document,
    /// Anything the table below does not classify.
    Other,
}

impl FileKind {
    /// Every kind, for exhaustiveness tests.
    pub const ALL: [FileKind; 8] = [
        FileKind::Folder,
        FileKind::Image,
        FileKind::Code,
        FileKind::Archive,
        FileKind::Audio,
        FileKind::Video,
        FileKind::Document,
        FileKind::Other,
    ];
}

/// Classify `path`. `is_dir_like` short-circuits to [`FileKind::Folder`] —
/// a directory named `build.rs` is still a directory.
pub fn classify(path: &Path, is_dir_like: bool) -> FileKind {
    if is_dir_like {
        return FileKind::Folder;
    }
    let Some(ext) = path.extension() else {
        return FileKind::Other;
    };
    classify_extension(&ext.to_string_lossy())
}

/// The extension table, case-insensitive.
///
/// Kept modest on purpose: a tint that is right for the files people actually
/// look at beats an exhaustive list nobody can audit. Unknown extensions get
/// [`FileKind::Other`], which is a real colour, not a fallback to grey text.
pub fn classify_extension(ext: &str) -> FileKind {
    let ext = ext.to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "tiff" | "tif" | "webp" | "heic" | "svg"
        | "ico" | "icns" => FileKind::Image,

        "rs" | "go" | "py" | "js" | "ts" | "tsx" | "jsx" | "c" | "h" | "cpp" | "hpp" | "cs"
        | "java" | "rb" | "php" | "swift" | "kt" | "sh" | "zsh" | "bash" | "sql" | "json"
        | "toml" | "yaml" | "yml" | "xml" | "html" | "css" | "scss" | "lua" | "pl" | "r" | "vb"
        | "ps1" => FileKind::Code,

        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar" | "dmg" | "pkg" | "iso"
        | "jar" => FileKind::Archive,

        "mp3" | "wav" | "flac" | "aac" | "m4a" | "ogg" | "aiff" | "aif" => FileKind::Audio,

        "mp4" | "mov" | "avi" | "mkv" | "webm" | "m4v" | "wmv" | "mpg" | "mpeg" => FileKind::Video,

        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "md" | "rtf" | "odt"
        | "ods" | "odp" | "pages" | "numbers" | "key" | "csv" | "tsv" | "log" => FileKind::Document,

        _ => FileKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn kind(name: &str) -> FileKind {
        classify(&PathBuf::from(name), false)
    }

    #[test]
    fn a_directory_is_a_folder_whatever_it_is_called() {
        assert_eq!(
            classify(&PathBuf::from("/a/build.rs"), true),
            FileKind::Folder,
            "a directory named like a source file is still a directory"
        );
    }

    #[test]
    fn the_table_classifies_each_family() {
        assert_eq!(kind("photo.png"), FileKind::Image);
        assert_eq!(kind("main.rs"), FileKind::Code);
        assert_eq!(kind("backup.tar.gz"), FileKind::Archive);
        assert_eq!(kind("song.flac"), FileKind::Audio);
        assert_eq!(kind("clip.mov"), FileKind::Video);
        assert_eq!(kind("report.pdf"), FileKind::Document);
    }

    #[test]
    fn classification_ignores_case() {
        assert_eq!(kind("PHOTO.PNG"), FileKind::Image);
        assert_eq!(kind("Report.PDF"), FileKind::Document);
    }

    /// An unknown extension is `Other`, which carries its own colour — it is
    /// a classification, not a failure.
    #[test]
    fn an_unknown_extension_is_other() {
        assert_eq!(kind("thing.qqq"), FileKind::Other);
    }

    #[test]
    fn a_file_with_no_extension_is_other() {
        assert_eq!(kind("README"), FileKind::Other);
        assert_eq!(kind("Makefile"), FileKind::Other);
    }

    /// Only the final extension counts, which is what `Path::extension`
    /// gives and what the tint should follow.
    #[test]
    fn only_the_last_extension_counts() {
        assert_eq!(kind("archive.tar.gz"), FileKind::Archive);
        assert_eq!(kind("notes.backup.md"), FileKind::Document);
    }

    #[test]
    fn a_dotfile_has_no_extension_to_classify() {
        // `Path::extension` reports None for a leading-dot name, so these are
        // `Other` rather than being read as an extension.
        assert_eq!(kind(".gitignore"), FileKind::Other);
    }
}

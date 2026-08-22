//! Natural sorting for directory listings (ARCHITECTURE.md §6, `sort.rs`).
//!
//! Case-insensitive natural comparison (digit runs compare numerically, so
//! `file2 < file10`), folders-first grouping, and direction flip.

use std::cmp::Ordering;
use std::iter::Peekable;
use std::str::Chars;

use serde::{Deserialize, Serialize};

use crate::entry::FileEntry;

/// Which column a listing is sorted by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SortKey {
    Name,
    Size,
    DateModified,
}

/// Sort direction; flipping it reverses the key ordering but never the
/// folders-first grouping.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SortDirection {
    Ascending,
    Descending,
}

impl SortDirection {
    pub fn flipped(self) -> Self {
        match self {
            SortDirection::Ascending => SortDirection::Descending,
            SortDirection::Descending => SortDirection::Ascending,
        }
    }
}

/// A complete sort configuration for one directory listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortSpec {
    pub key: SortKey,
    pub direction: SortDirection,
    pub folders_first: bool,
}

impl Default for SortSpec {
    fn default() -> Self {
        Self {
            key: SortKey::Name,
            direction: SortDirection::Ascending,
            folders_first: true,
        }
    }
}

impl SortSpec {
    /// Total order over entries: folders-first partition (unaffected by
    /// direction), then the selected key, then name and path tie-breaks so the
    /// order is deterministic and binary-searchable.
    pub fn compare(&self, a: &FileEntry, b: &FileEntry) -> Ordering {
        if self.folders_first {
            match (a.is_dir_like(), b.is_dir_like()) {
                (true, false) => return Ordering::Less,
                (false, true) => return Ordering::Greater,
                _ => {}
            }
        }
        let ord = match self.key {
            SortKey::Name => natural_cmp(&a.name, &b.name),
            SortKey::Size => a
                .size
                .cmp(&b.size)
                .then_with(|| natural_cmp(&a.name, &b.name)),
            SortKey::DateModified => a
                .modified
                .cmp(&b.modified)
                .then_with(|| natural_cmp(&a.name, &b.name)),
        };
        let ord = ord.then_with(|| a.path.cmp(&b.path));
        match self.direction {
            SortDirection::Ascending => ord,
            SortDirection::Descending => ord.reverse(),
        }
    }
}

/// Case-insensitive natural comparison: runs of ASCII digits compare by
/// numeric value (`file2 < file10`), everything else compares by lowercased
/// characters. Equal-modulo-case/zero-padding strings fall back to raw
/// ordering so the result is a total order (`Equal` only for identical
/// strings).
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let run_a = take_digit_run(&mut ai);
                let run_b = take_digit_run(&mut bi);
                let ord = cmp_digit_runs(&run_a, &run_b);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(x), Some(y)) => {
                let ord = x.to_lowercase().cmp(y.to_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
                ai.next();
                bi.next();
            }
        }
    }
}

fn take_digit_run(chars: &mut Peekable<Chars<'_>>) -> String {
    let mut run = String::new();
    while let Some(c) = chars.peek().copied() {
        if !c.is_ascii_digit() {
            break;
        }
        run.push(c);
        chars.next();
    }
    run
}

/// Compare two digit runs by numeric value without overflow: strip leading
/// zeros, then longer runs are larger, then lexicographic.
fn cmp_digit_runs(a: &str, b: &str) -> Ordering {
    let a = a.trim_start_matches('0');
    let b = b.trim_start_matches('0');
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryKind;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    fn entry(name: &str, kind: EntryKind, size: u64, modified_secs: u64) -> FileEntry {
        FileEntry {
            path: Arc::from(PathBuf::from(format!("/t/{name}"))),
            name: name.into(),
            kind,
            size,
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(modified_secs),
            created: None,
            hidden: name.starts_with('.'),
        }
    }

    fn names(mut entries: Vec<FileEntry>, spec: SortSpec) -> Vec<String> {
        entries.sort_by(|a, b| spec.compare(a, b));
        entries.into_iter().map(|e| e.name.to_string()).collect()
    }

    #[test]
    fn natural_cmp_table() {
        use Ordering::*;
        let cases: &[(&str, &str, Ordering)] = &[
            ("file2", "file10", Less),
            ("file10", "file2", Greater),
            ("file2", "file2", Equal),
            ("file", "file2", Less),
            ("File10", "file2", Greater), // case-insensitive, 10 > 2
            ("apple", "Banana", Less),    // case-insensitive letters
            ("a2b3", "a2b10", Less),      // multiple digit runs
            ("a10b2", "a10b10", Less),
            ("file9", "file010", Less),    // 9 < 10 despite zero padding
            ("日本語2", "日本語10", Less), // digit runs after non-ASCII
            ("", "a", Less),
            ("1000000000000000000000", "2", Greater), // longer than u64
        ];
        for (a, b, expected) in cases {
            assert_eq!(natural_cmp(a, b), *expected, "natural_cmp({a:?}, {b:?})");
        }
    }

    #[test]
    fn natural_cmp_is_a_total_order_on_case_and_padding_ties() {
        // Equal modulo case / zero padding must still order deterministically,
        // and only identical strings are Equal.
        assert_ne!(natural_cmp("File", "file"), Ordering::Equal);
        assert_ne!(natural_cmp("file02", "file2"), Ordering::Equal);
        assert_eq!(natural_cmp("file02", "file2"), "file02".cmp("file2"));
    }

    #[test]
    fn folders_sort_before_files() {
        let entries = vec![
            entry("zeta", EntryKind::Dir, 0, 0),
            entry("alpha.txt", EntryKind::File, 1, 0),
            entry("beta", EntryKind::Dir, 0, 0),
        ];
        assert_eq!(
            names(entries, SortSpec::default()),
            ["beta", "zeta", "alpha.txt"]
        );
    }

    #[test]
    fn direction_flip_reverses_keys_but_keeps_folders_first() {
        let entries = vec![
            entry("b.txt", EntryKind::File, 1, 0),
            entry("dir", EntryKind::Dir, 0, 0),
            entry("a.txt", EntryKind::File, 1, 0),
        ];
        let spec = SortSpec {
            direction: SortDirection::Descending,
            ..SortSpec::default()
        };
        assert_eq!(names(entries, spec), ["dir", "b.txt", "a.txt"]);
    }

    #[test]
    fn sort_by_size_with_name_tiebreak() {
        let entries = vec![
            entry("big.bin", EntryKind::File, 100, 0),
            entry("small2.bin", EntryKind::File, 1, 0),
            entry("small1.bin", EntryKind::File, 1, 0),
        ];
        let spec = SortSpec {
            key: SortKey::Size,
            ..SortSpec::default()
        };
        assert_eq!(
            names(entries, spec),
            ["small1.bin", "small2.bin", "big.bin"]
        );
    }

    #[test]
    fn sort_by_date_modified() {
        let entries = vec![
            entry("new.txt", EntryKind::File, 0, 300),
            entry("old.txt", EntryKind::File, 0, 100),
            entry("mid.txt", EntryKind::File, 0, 200),
        ];
        let spec = SortSpec {
            key: SortKey::DateModified,
            ..SortSpec::default()
        };
        assert_eq!(names(entries, spec), ["old.txt", "mid.txt", "new.txt"]);
    }

    #[test]
    fn direction_flipped_helper() {
        assert_eq!(
            SortDirection::Ascending.flipped(),
            SortDirection::Descending
        );
        assert_eq!(
            SortDirection::Descending.flipped(),
            SortDirection::Ascending
        );
    }
}

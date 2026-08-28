//! M6b's acceptance criterion, as a test: **a file tagged here shows up tagged
//! in Finder, and a file tagged in Finder shows up tagged here.**
//!
//! Round-tripping our own writer through our own reader would prove nothing
//! about Finder, so the macOS half of this file never does that. It writes tags
//! with [`fs_core::MacPlatform`] and reads the resulting extended attribute back
//! with **Apple's own tools** (`xattr` for the raw bytes, `plutil` for the plist
//! structure), then goes the other way: builds a plist with `plutil`, installs
//! it with `xattr -wx`, and asserts `read_tags` decodes it. Both formats Finder
//! and third-party taggers write — binary and XML — are covered.
//!
//! The portable half (stub platform, pure codec) runs everywhere, so this file
//! is meaningful on a Windows or Linux development machine too, per CLAUDE.md.

use std::sync::Arc;

use fs_core::{Platform, Spawner, StubPlatform, Tag, TagColor, TestSpawner};
use futures::executor::block_on;

#[test]
fn stub_platform_round_trips_tags_and_empty_removes_them() {
    let platform = StubPlatform::new();
    let path = std::path::Path::new("/root/report.pdf");
    let other = std::path::Path::new("/root/notes.md");

    assert_eq!(
        block_on(platform.read_tags(path)).unwrap(),
        vec![],
        "a path never written has no tags"
    );

    let tags = vec![Tag::new("Work", TagColor::Blue), Tag::uncolored("Ideas")];
    block_on(platform.write_tags(path, &tags)).unwrap();
    assert_eq!(block_on(platform.read_tags(path)).unwrap(), tags);
    assert_eq!(
        block_on(platform.read_tags(other)).unwrap(),
        vec![],
        "writing one path does not tag another"
    );

    // Known tags = palette + whatever the user actually has.
    let known = block_on(platform.known_tags()).unwrap();
    assert_eq!(&known[..7], &fs_core::standard_tags()[..]);
    let names: Vec<&str> = known.iter().map(|t| t.name.as_ref()).collect();
    assert!(
        names.contains(&"Work") && names.contains(&"Ideas"),
        "{names:?}"
    );

    block_on(platform.write_tags(path, &[])).unwrap();
    assert_eq!(block_on(platform.read_tags(path)).unwrap(), vec![]);
    assert_eq!(
        block_on(platform.known_tags()).unwrap(),
        fs_core::standard_tags(),
        "clearing the last tagged file leaves only the palette"
    );
}

#[cfg(target_os = "macos")]
mod finder_compatibility {
    use super::*;

    use std::process::Command;

    const XATTR: &str = "com.apple.metadata:_kMDItemUserTags";

    fn platform() -> fs_core::MacPlatform {
        let spawner: Arc<dyn Spawner> = Arc::new(TestSpawner::new());
        fs_core::MacPlatform::new(spawner)
    }

    fn run(program: &str, args: &[&str]) -> String {
        let output = Command::new(program)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("running {program}: {e}"));
        assert!(
            output.status.success(),
            "{program} {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// `xattr -px` prints the value as space-separated hex bytes, wrapped.
    fn hex_to_bytes(hex: &str) -> Vec<u8> {
        let digits: Vec<char> = hex.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        assert!(
            digits.len().is_multiple_of(2),
            "odd hex digit count: {hex:?}"
        );
        digits
            .chunks(2)
            .map(|pair| u8::from_str_radix(&pair.iter().collect::<String>(), 16).unwrap())
            .collect()
    }

    fn bytes_to_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// **Out**: what we write is what Finder reads — a binary plist whose
    /// strings are exactly the `Name\ncolorindex` forms, verified by `xattr`
    /// and `plutil`, not by our own reader.
    #[test]
    fn tags_we_write_are_a_binary_plist_apple_tooling_agrees_with() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("report.pdf");
        std::fs::write(&file, b"pdf").unwrap();

        let tags = vec![
            Tag::new("Red", TagColor::Red),
            Tag::new("Wörk", TagColor::None),
            Tag::new("Später", TagColor::Purple),
        ];
        block_on(platform().write_tags(&file, &tags)).unwrap();

        let raw = hex_to_bytes(&run(
            "/usr/bin/xattr",
            &["-px", XATTR, file.to_str().unwrap()],
        ));
        assert!(
            raw.starts_with(b"bplist00"),
            "Finder writes a *binary* plist; we wrote {:?}…",
            String::from_utf8_lossy(&raw[..raw.len().min(16)])
        );

        // Hand the raw bytes to plutil and read the structure back out of it.
        let plist_file = dir.path().join("dumped.plist");
        std::fs::write(&plist_file, &raw).unwrap();
        let xml = run(
            "/usr/bin/plutil",
            &["-convert", "xml1", "-o", "-", plist_file.to_str().unwrap()],
        );
        for expected in ["Red\n6", "Wörk\n0", "Später\n3"] {
            assert!(
                xml.contains(&format!("<string>{expected}</string>")),
                "plutil did not see <string>{expected:?}</string> in:\n{xml}"
            );
        }
        assert_eq!(xml.matches("<string>").count(), 3, "{xml}");
        assert!(
            xml.contains("<array>"),
            "top level must be an array:\n{xml}"
        );
    }

    /// **Out, clearing**: an empty set removes the attribute rather than
    /// leaving an empty array behind, so an untagged file looks untouched to
    /// Finder and to `xattr -l`.
    #[test]
    fn clearing_tags_removes_the_extended_attribute_entirely() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.md");
        std::fs::write(&file, b"# notes").unwrap();
        let platform = platform();

        block_on(platform.write_tags(&file, &[Tag::new("Blue", TagColor::Blue)])).unwrap();
        assert!(run("/usr/bin/xattr", &["-l", file.to_str().unwrap()]).contains(XATTR));

        block_on(platform.write_tags(&file, &[])).unwrap();
        let listed = run("/usr/bin/xattr", &["-l", file.to_str().unwrap()]);
        assert!(
            !listed.contains(XATTR),
            "attribute survived clearing: {listed}"
        );

        // Reading an untagged file is Ok(empty), not an error…
        assert_eq!(block_on(platform.read_tags(&file)).unwrap(), vec![]);
        // …and clearing it again is a no-op, not ENOATTR.
        block_on(platform.write_tags(&file, &[])).unwrap();
    }

    /// **In**: a binary plist built by `plutil` and installed by `xattr -wx` —
    /// byte-for-byte what Finder puts there — decodes to the right tags.
    #[test]
    fn tags_written_by_apple_tooling_are_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("photo.png");
        std::fs::write(&file, b"png").unwrap();

        // Build the plist the way a shell user would, then convert to binary.
        let source = dir.path().join("tags.plist");
        std::fs::write(
            &source,
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <plist version=\"1.0\"><array>\
             <string>Blue\n4</string>\
             <string>Ideas</string>\
             <string>日本語\n7</string>\
             </array></plist>",
        )
        .unwrap();
        run(
            "/usr/bin/plutil",
            &["-convert", "binary1", source.to_str().unwrap()],
        );
        let binary = std::fs::read(&source).unwrap();
        assert!(binary.starts_with(b"bplist00"));
        run(
            "/usr/bin/xattr",
            &["-wx", XATTR, &bytes_to_hex(&binary), file.to_str().unwrap()],
        );

        assert_eq!(
            block_on(platform().read_tags(&file)).unwrap(),
            vec![
                Tag::new("Blue", TagColor::Blue),
                // A bare name with no newline is a real Finder form: no colour.
                Tag::uncolored("Ideas"),
                Tag::new("日本語", TagColor::Orange),
            ]
        );
    }

    /// **In, XML**: some third-party taggers (and anyone using `xattr -w` by
    /// hand) leave an XML plist in place. Foundation sniffs the format, so we
    /// read those too.
    #[test]
    fn an_xml_plist_payload_is_read_as_well_as_a_binary_one() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("legacy.txt");
        std::fs::write(&file, b"legacy").unwrap();

        run(
            "/usr/bin/xattr",
            &[
                "-w",
                XATTR,
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                 <plist version=\"1.0\"><array><string>Green\n2</string></array></plist>",
                file.to_str().unwrap(),
            ],
        );

        assert_eq!(
            block_on(platform().read_tags(&file)).unwrap(),
            vec![Tag::new("Green", TagColor::Green)]
        );
    }

    /// Our own writer and reader must of course also agree — but this is the
    /// *weak* test, kept only to pin the end-to-end path (and the empty and
    /// many-tag edges) now that the two above pin the format itself.
    #[test]
    fn write_then_read_agrees_on_the_whole_palette() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("everything");
        std::fs::write(&file, b"x").unwrap();
        let platform = platform();

        let tags: Vec<Tag> = fs_core::standard_tags();
        block_on(platform.write_tags(&file, &tags)).unwrap();
        assert_eq!(block_on(platform.read_tags(&file)).unwrap(), tags);

        // Directories carry tags too — the sidebar filter depends on it.
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let one = vec![Tag::new("Orange", TagColor::Orange)];
        block_on(platform.write_tags(&sub, &one)).unwrap();
        assert_eq!(block_on(platform.read_tags(&sub)).unwrap(), one);
    }

    /// Failure modes the info panel and the details rows will actually hit.
    #[test]
    fn missing_paths_and_corrupt_payloads_are_reported_not_silently_empty() {
        let dir = tempfile::tempdir().unwrap();
        let platform = platform();

        // Reading a file that is not there is an error, not "no tags".
        let missing = dir.path().join("gone.txt");
        assert!(block_on(platform.read_tags(&missing)).is_err());

        // A payload that is not a plist at all is loud: the next write would
        // overwrite it, so "no tags" would be a lie.
        let junk = dir.path().join("junk.txt");
        std::fs::write(&junk, b"junk").unwrap();
        run(
            "/usr/bin/xattr",
            &["-w", XATTR, "not a plist", junk.to_str().unwrap()],
        );
        let error = block_on(platform.read_tags(&junk)).unwrap_err().to_string();
        assert!(error.contains("junk.txt"), "{error}");
    }

    /// `known_tags` is best-effort, but it must always contain the palette and
    /// must never fail on a normal Mac.
    #[test]
    fn known_tags_contains_the_standard_palette() {
        let known = block_on(platform().known_tags()).unwrap();
        assert_eq!(&known[..7], &fs_core::standard_tags()[..]);
        let mut names: Vec<&str> = known.iter().map(|t| t.name.as_ref()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), known.len(), "no duplicate tag names");
    }

    /// Tags belong to the item the user clicked, so a symlink's tags are its
    /// target's — the documented `options: 0` (follow) choice in `macos.rs`.
    #[test]
    fn tags_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        let link = dir.path().join("link.txt");
        std::fs::write(&target, b"t").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let tags = vec![Tag::new("Yellow", TagColor::Yellow)];
        block_on(platform().write_tags(&link, &tags)).unwrap();
        assert_eq!(block_on(platform().read_tags(&target)).unwrap(), tags);
    }
}

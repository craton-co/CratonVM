use super::{
    attribute_names_for_view, attribute_view_names_for_path, attribute_write_kind,
    jarfs_encode, split_attribute_spec, supported_attribute_view_names, vfs_stat_facts,
    AttributeWrite,
};

/// Write a two-entry zip to a unique temp path and return it.
fn write_probe_zip(tag: &str) -> std::path::PathBuf {
    use std::io::Write;
    let path =
        std::env::temp_dir().join(format!("cratonvm-vfsattr-{tag}-{}.zip", std::process::id()));
    let file = std::fs::File::create(&path).expect("create zip");
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("hello.txt", opts).expect("start_file");
    zip.write_all(b"hello").expect("write entry");
    zip.add_directory("dir", opts).expect("add_directory");
    zip.finish().expect("finish zip");
    path
}

/// An entry inside a mounted archive must be answered FROM the archive.
/// Its path string is the sentinel-delimited jar-FS form, which names no
/// host file, so any code that reaches `stat` with it reports the entry as
/// missing — which is exactly what `Files.readAttributes` on a zip entry
/// did.
#[test]
fn a_jar_entry_is_stat_ed_out_of_the_archive_not_the_host() {
    let zip = write_probe_zip("stat");
    let jar = zip.to_string_lossy().into_owned();

    let file = vfs_stat_facts(&jarfs_encode(&jar, "hello.txt"))
        .expect("a jar-FS path must be recognised")
        .expect("hello.txt is present");
    assert!(file.is_regular, "a zip entry is a regular file");
    assert!(!file.is_dir);
    assert_eq!(file.size, 5, "size comes from the central directory");

    let dir = vfs_stat_facts(&jarfs_encode(&jar, "dir"))
        .expect("recognised")
        .expect("dir is present");
    assert!(dir.is_dir, "an explicit directory entry is a directory");
    assert_eq!(dir.size, 0);

    // A name the archive does not hold is NotFound, not a host lookup.
    assert!(
        vfs_stat_facts(&jarfs_encode(&jar, "nope.txt"))
            .expect("recognised")
            .is_err(),
        "an absent entry must report NotFound"
    );

    // A plain host path is not ours to answer.
    assert!(
        vfs_stat_facts("/tmp").is_none(),
        "an ordinary path must fall through to stat"
    );

    let _ = std::fs::remove_file(&zip);
}

/// A mounted archive offers `basic` and nothing else — HotSpot's zipfs does
/// not answer `posix`/`unix`/`dos` for an entry either, and answering them
/// here would mean answering them out of a host `stat` of a path string that
/// names no file.
#[test]
fn an_archive_entry_offers_only_the_basic_view() {
    let jarfs_path = jarfs_encode("/tmp/whatever.jar", "a/b.txt");
    assert_eq!(attribute_view_names_for_path(&jarfs_path), &["basic"]);
    assert_eq!(
        attribute_view_names_for_path("/tmp/whatever.jar"),
        supported_attribute_view_names(),
        "an ordinary host path keeps the host's view list"
    );
}

/// `Files` treats a spec with no colon as the `basic` view. Getting this
/// wrong turns `readAttributes(p, "size")` into a request for a view named
/// `size`.
#[test]
fn a_spec_without_a_colon_is_the_basic_view() {
    assert_eq!(split_attribute_spec("size"), ("basic", "size"));
    assert_eq!(split_attribute_spec("*"), ("basic", "*"));
    assert_eq!(split_attribute_spec("unix:dev"), ("unix", "dev"));
    assert_eq!(split_attribute_spec("posix:*"), ("posix", "*"));
    // Only the FIRST colon splits; the JDK's own rule.
    assert_eq!(split_attribute_spec("unix:a:b"), ("unix", "a:b"));
}

/// Every view this VM advertises through `supportedFileAttributeViews()`
/// must have a name table, or `readAttributes` answers
/// `UnsupportedOperationException` for a view the same VM just claimed to
/// support. `user` is the documented exception: it is the extended-attribute
/// view, which has no fixed attribute names at all.
#[test]
fn every_advertised_view_except_user_has_a_name_table() {
    for view in supported_attribute_view_names() {
        if *view == "user" || *view == "acl" {
            continue;
        }
        assert!(
            attribute_names_for_view(view).is_some(),
            "view `{view}` is advertised by supportedFileAttributeViews() but \
             readAttributes has no name table for it"
        );
    }
}

/// A wider view must be a superset of `basic`: the JDK's `PosixFileAttributes`
/// and `UnixFileAttributes` extend `BasicFileAttributes`, so
/// `readAttributes(p, "unix:*")` returning fewer keys than
/// `readAttributes(p, "basic:*")` would be a silent regression for every
/// caller that widened its view to get one extra field.
#[test]
fn posix_unix_and_dos_all_contain_the_basic_names() {
    let basic = attribute_names_for_view("basic").expect("basic");
    for view in ["posix", "unix", "dos"] {
        let names = attribute_names_for_view(view).expect(view);
        for b in basic {
            assert!(
                names.contains(b),
                "view `{view}` is missing basic attribute `{b}`"
            );
        }
    }
}

/// `setAttribute` and `readAttributes` must agree about what a view holds.
/// Every name the reader answers gets a verdict from the writer — the
/// default arm makes that trivially true, so what this really pins is WHICH
/// verdict, i.e. that a name added to the reader's table is not silently
/// classified writable (a write that lands somewhere unintended) or
/// silently refused (a name `readAttributes` returns that `setAttribute`
/// claims does not exist).
#[test]
fn the_writable_attributes_are_the_ones_the_jdk_lets_you_write() {
    for name in ["lastModifiedTime", "lastAccessTime", "creationTime"] {
        assert_eq!(attribute_write_kind(name), AttributeWrite::Time, "{name}");
    }
    for name in ["readonly", "hidden", "archive", "system"] {
        assert_eq!(
            attribute_write_kind(name),
            AttributeWrite::DosFlag,
            "{name}"
        );
    }
    assert_eq!(
        attribute_write_kind("permissions"),
        AttributeWrite::Permissions
    );
    assert_eq!(attribute_write_kind("mode"), AttributeWrite::Mode);
    for name in ["owner", "group", "uid", "gid"] {
        assert_eq!(
            attribute_write_kind(name),
            AttributeWrite::Unimplemented,
            "{name}"
        );
    }
    // Read-only in the JDK: the basic attribute view's `setAttribute`
    // throws for every one of these.
    for name in [
        "size",
        "isRegularFile",
        "isDirectory",
        "isSymbolicLink",
        "isOther",
        "fileKey",
        "ino",
        "dev",
        "rdev",
        "nlink",
        "ctime",
    ] {
        assert_eq!(
            attribute_write_kind(name),
            AttributeWrite::NotWritable,
            "{name} must not be writable"
        );
    }

    // And every name any advertised view answers is covered by one of the
    // arms above — no name reaches the writer unclassified.
    for view in supported_attribute_view_names() {
        let Some(names) = attribute_names_for_view(view) else {
            continue;
        };
        for name in names {
            let kind = attribute_write_kind(name);
            assert!(
                matches!(
                    kind,
                    AttributeWrite::Time
                        | AttributeWrite::DosFlag
                        | AttributeWrite::Permissions
                        | AttributeWrite::Mode
                        | AttributeWrite::Unimplemented
                        | AttributeWrite::NotWritable
                ),
                "`{view}:{name}` has no write verdict"
            );
        }
    }
}

/// The `unix` view is the one `sun.jvmstat.PlatformSupportImpl` reads during
/// container detection, and that read is what made
/// `com.sun.tools.attach.VirtualMachine.list()` throw `InternalError`.
#[test]
fn the_unix_view_carries_the_stat_fields_jvmstat_asks_for() {
    let unix = attribute_names_for_view("unix").expect("unix");
    for name in ["dev", "ino", "mode", "nlink", "uid", "gid", "rdev", "ctime"] {
        assert!(unix.contains(&name), "unix view is missing `{name}`");
    }
}

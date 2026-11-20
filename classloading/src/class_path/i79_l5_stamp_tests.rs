use super::*;

fn scratch(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("cratonvm_i79_l5_{tag}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// i71: the stamp a class-path read returns is the metadata of the file
/// it read (its own handle's), not a later `stat` of the path; and the
/// mapped and the copied reads agree on it.
#[test]
fn i79_the_class_path_read_stamps_the_file_it_read() {
    let dir = scratch("read_stamp");
    let path = dir.join("a.jar");
    fs::write(&path, b"PK\x05\x06 not really a jar, only bytes").unwrap();
    let (backing, stamp) = read_archive_for_classpath_stamped(&path).unwrap();
    let fd_meta = fs::File::open(&path).unwrap().metadata().unwrap();
    assert_eq!(stamp, OuterArchiveStamp::of(&fd_meta));
    assert_eq!(stamp.map(|s| s.len), Some(backing.len() as u64));
    let (bytes, copied) = read_file_for_classpath_stamped(&path).unwrap();
    assert_eq!(copied, stamp);
    assert_eq!(&bytes[..], &backing[..]);
    let _ = fs::remove_dir_all(&dir);
}

/// i68: a jar replaced through a rename with a file of the SAME length and
/// the SAME modification time has another stamp where the platform has a
/// file identity (Unix: device and inode); an in-place rewrite keeps it.
#[test]
fn i79_a_same_length_same_mtime_rename_changes_the_stamp_on_unix() {
    let dir = scratch("rename_stamp");
    let path = dir.join("app.jar");
    fs::write(&path, b"AAAAAAAAAAAAAAAA").unwrap();
    let mtime = fs::metadata(&path).unwrap().modified().unwrap();
    let before = OuterArchiveStamp::read(&path).unwrap();

    let next = dir.join("app.jar.next");
    fs::write(&next, b"BBBBBBBBBBBBBBBB").unwrap();
    fs::File::options()
        .write(true)
        .open(&next)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    fs::rename(&next, &path).unwrap();
    let after = OuterArchiveStamp::read(&path).unwrap();
    assert_eq!(
        (after.len, after.modified),
        (before.len, before.modified),
        "length and mtime kept"
    );
    if cfg!(unix) {
        assert_ne!(after, before, "the inode tells the files apart");
    } else {
        assert_eq!(after, before, "no stable file identity here");
    }

    // In place, same length, mtime restored: the same inode, the same stamp.
    fs::write(&path, b"CCCCCCCCCCCCCCCC").unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    assert_eq!(OuterArchiveStamp::read(&path).unwrap(), after);
    let _ = fs::remove_dir_all(&dir);
}

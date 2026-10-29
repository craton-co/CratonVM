// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// T1.8.2 follow-up — production-code panic gate. The class-path
// loader is on the startup-critical path; a panic here would
// terminate the entire VM before user code even starts. We gate the
// lint on `not(test)` so the test module (which legitimately uses
// `.unwrap()`/`.expect()` for fixture construction) is unaffected.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used,))]

use crate::loader_flags;
use cratonvm_reader::SharedBytes;
use cratonvm_types::error::ClassFileError;
use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tracing::debug;
use zip::ZipArchive;

/// Percent-encode a filesystem path for embedding in a `file:`/`jar:file:` URL,
/// the way `File.toURI()` (`sun.net.www.ParseUtil.encodePath`) does.
///
/// Without this a directory literally named `custom#root` came back from
/// `ClassLoader.getResource` as `file:/.../custom#root/scanned/`, where the raw
/// `#` is a URL *fragment* delimiter — every consumer that re-parses the URL
/// silently drops everything after it (HotSpot keeps `custom%23root`; see
/// `core.io.support.PathMatchingResourcePatternResolverTests.encodedHashtagInPath`).
/// `%` is escaped first so the escapes added here are not double-encoded.
/// Mirrors `native-builtins::classloader::file_url_spec`, which already did
/// this for the `URLClassLoader.getURLs()` / manifest side only.
pub(crate) fn encode_path_for_url(p: &str) -> String {
    p.replace('%', "%25")
        .replace(' ', "%20")
        .replace('#', "%23")
        .replace('?', "%3F")
}

/// Immutable backing for an archive. On-disk archives are mapped once and all
/// `ZipArchive` cursors plus stored class entries share that mapping. Nested
/// archives and test fixtures retain an owned reference-counted buffer.
#[derive(Clone)]
enum ArchiveBacking {
    Owned(Arc<[u8]>),
    Mapped(Arc<memmap2::Mmap>),
    /// Phase 5 boot image (WP2): mapped at a brokered fixed address during a
    /// capture run (`cratonvm_types::image_layout::map_file_readonly`).
    Fixed(Arc<cratonvm_types::image_layout::FileMapping>),
    Shared(SharedBytes),
}

impl ArchiveBacking {
    fn from_vec(bytes: Vec<u8>) -> Self {
        Self::Owned(Arc::from(bytes))
    }
}

impl AsRef<[u8]> for ArchiveBacking {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Owned(bytes) => bytes,
            Self::Mapped(bytes) => bytes,
            Self::Fixed(bytes) => bytes,
            Self::Shared(bytes) => bytes,
        }
    }
}

impl std::ops::Deref for ArchiveBacking {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_ref()
    }
}

type SharedArchive = ZipArchive<Cursor<ArchiveBacking>>;

/// How a [`ClassPathEntry::JmodFile`] serves its entries.
///
/// zr z8/fm (2026-10-13): `ZipArchive::new` reads every entry's local header
/// at open, which faults in ~every page of a mapped JMOD (~47 MB for the 70
/// JMODs of a Hello world's boot class path). [`JmodArchive::Central`] reads
/// only the central directory and one local header per entry actually served
/// ([`crate::jmod_index`]). `CRATONVM_JMOD_LAZY_INDEX=0` (the kill switch)
/// keeps [`JmodArchive::Zip`] for every JMOD, which is also what a JMOD the
/// index does not model gets.
enum JmodArchive {
    Zip(Mutex<SharedArchive>),
    Central(crate::jmod_index::JmodCentralIndex),
}

/// The 22 bytes of an archive with no entries: what [`JarArchive::zip`] falls
/// back to for a JAR whose central directory parsed but whose `ZipArchive`
/// open fails (a damaged local header the index never reads).
const EMPTY_ZIP_ARCHIVE: [u8; 22] = [
    0x50, 0x4b, 0x05, 0x06, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

/// How a [`ClassPathEntry::JarFile`] reads its entries (zt/fp footprint round,
/// `CRATONVM_JAR_LAZY_INDEX`, default ON, `=0` kill).
///
/// `ZipArchive::new` reads every entry's local header at open (see
/// [`JmodArchive`]), which faults in about one page per entry of a mapped jar:
/// a class path of a hundred dependency jars made most of their bytes resident
/// before the first class loaded. A jar the central-directory index models
/// ([`crate::jmod_index`]) is opened from its end record and central directory
/// only; a local header is read when its entry is served.
///
/// The by-name readers (class and resource bytes, existence, listing, the
/// multi-release version set; [`ArchiveRead`]) answer from the index. Anything
/// else (signed-jar verification, the nested-archive rows, a lent parse) goes
/// through [`Self::zip`] / `Deref`, which builds the `ZipArchive` on first
/// use: the old full open, paid only by the jars that need it.
struct JarArchive {
    backing: ArchiveBacking,
    central: Option<crate::jmod_index::JmodCentralIndex>,
    zip: OnceLock<Mutex<SharedArchive>>,
}

impl JarArchive {
    /// A jar opened the old way.
    fn zipped(archive: SharedArchive, backing: ArchiveBacking) -> Self {
        Self {
            backing,
            central: None,
            zip: OnceLock::from(Mutex::new(archive)),
        }
    }

    /// A jar served from its central-directory index; the `ZipArchive` is
    /// built only when something asks for it.
    fn central(index: crate::jmod_index::JmodCentralIndex, backing: ArchiveBacking) -> Self {
        Self {
            backing,
            central: Some(index),
            zip: OnceLock::new(),
        }
    }

    /// The `ZipArchive`, opened now if this jar has only the index so far.
    #[allow(clippy::expect_used)]
    fn zip(&self) -> &Mutex<SharedArchive> {
        self.zip.get_or_init(|| {
            let archive = ZipArchive::new(Cursor::new(self.backing.clone())).unwrap_or_else(|_| {
                ZipArchive::new(Cursor::new(ArchiveBacking::from_vec(
                    EMPTY_ZIP_ARCHIVE.to_vec(),
                )))
                .expect("an archive with no entries opens")
            });
            Mutex::new(archive)
        })
    }

    /// Has the full `ZipArchive` been opened (tests: a plain class load must
    /// not have)?
    #[cfg(test)]
    fn zip_is_open(&self) -> bool {
        self.zip.get().is_some()
    }

    /// `META-INF/MANIFEST.MF`, read from the index when there is one.
    fn read_manifest(&self) -> ManifestInfo {
        match &self.central {
            Some(index) => {
                let data =
                    ClassPath::central_find_vec(index, &self.backing, "META-INF/MANIFEST.MF");
                data.map_or_else(ManifestInfo::default, |data| ManifestInfo::parse(&data))
            }
            None => ClassPath::read_manifest(&mut *self.zip().lock()),
        }
    }

    /// [`plain_view_of`] over this jar: from the index when there is one (one
    /// record per name, counted before duplicates were folded), else from the
    /// `ZipArchive`'s own directory.
    fn plain_view(&self, entry_index: &FxHashSet<String>) -> bool {
        match &self.central {
            Some(index) => {
                entry_index.len() == index.len()
                    && index.raw_records() == index.len()
                    && nested_archive_view_is_plain(entry_index)
            }
            None => {
                let guard = self.zip().lock();
                plain_view_of(entry_index, guard.len(), shared_archive_records(&guard))
            }
        }
    }

    /// The FILE entry `name` with the central directory's CRC-32 `crc32` and
    /// size `size`: [`NestedArchiveEntries::read_checked_entry`] over the index
    /// when there is one (CRC-checked, of exactly the directory's size).
    fn read_checked(&self, name: &str, crc32: u32, size: u64) -> Option<Vec<u8>> {
        match &self.central {
            Some(index) => {
                if name.ends_with('/') {
                    return None;
                }
                let loc = index.locate(self.backing.as_ref(), name)?;
                if loc.crc32 != crc32 || loc.uncompressed != size {
                    return None;
                }
                if size > MAX_UNCOMPRESSED_ENTRY_BYTES {
                    return None;
                }
                let data = ClassPath::jmod_inflate_checked(self.backing.as_ref(), &loc)?;
                (data.len() as u64 == size).then_some(data)
            }
            None => NestedArchiveEntries::read_checked_entry(
                &mut *self.zip().lock(),
                name,
                Some((crc32, size)),
            ),
        }
    }

    /// Does the index show that this jar holds no signer block
    /// (`META-INF/*.RSA|.DSA|.EC` under a safe name)? `false` when there is no
    /// index to ask.
    fn lacks_signer_blocks(&self) -> bool {
        let Some(index) = &self.central else {
            return false;
        };
        !index.names().iter_all().any(|name| {
            if !is_safe_entry_name(name) {
                return false;
            }
            let upper = name.to_ascii_uppercase();
            upper.starts_with("META-INF/")
                && (upper.ends_with(".RSA") || upper.ends_with(".DSA") || upper.ends_with(".EC"))
        })
    }
}

impl std::ops::Deref for JarArchive {
    type Target = Mutex<SharedArchive>;

    fn deref(&self) -> &Mutex<SharedArchive> {
        self.zip()
    }
}

/// The by-name reads a class path asks of an archive, over a `ZipArchive`
/// ([`Mutex<SharedArchive>`]) or a [`JarArchive`], so the multi-release and
/// indexed lookups serve both.
trait ArchiveRead {
    /// The entry's bytes, inflated and capped.
    fn read_vec(&self, name: &str) -> Option<Vec<u8>>;
    /// The entry's bytes as shared storage: a stored entry is a range view of
    /// `backing`.
    fn read_shared(&self, backing: &ArchiveBacking, name: &str) -> Option<SharedBytes>;
    /// Is there an entry called `name`?
    fn has_entry(&self, name: &str) -> bool;
    /// Every entry name, in the archive's order.
    fn each_name(&self, f: &mut dyn FnMut(&str));
}

impl ArchiveRead for Mutex<SharedArchive> {
    fn read_vec(&self, name: &str) -> Option<Vec<u8>> {
        ClassPath::find_in_archive(self, name)
    }

    fn read_shared(&self, backing: &ArchiveBacking, name: &str) -> Option<SharedBytes> {
        ClassPath::find_shared_in_archive(self, backing, name)
    }

    fn has_entry(&self, name: &str) -> bool {
        ClassPath::archive_has_entry(self, name)
    }

    fn each_name(&self, f: &mut dyn FnMut(&str)) {
        let mut guard = self.lock();
        for i in 0..guard.len() {
            if let Ok(entry) = guard.by_index_raw(i) {
                f(entry.name());
            }
        }
    }
}

impl ArchiveRead for JarArchive {
    fn read_vec(&self, name: &str) -> Option<Vec<u8>> {
        match &self.central {
            Some(index) => ClassPath::central_find_vec(index, &self.backing, name),
            None => ClassPath::find_in_archive(self.zip(), name),
        }
    }

    fn read_shared(&self, backing: &ArchiveBacking, name: &str) -> Option<SharedBytes> {
        match &self.central {
            Some(index) => ClassPath::central_find_shared(index, backing, name),
            None => ClassPath::find_shared_in_archive(self.zip(), backing, name),
        }
    }

    fn has_entry(&self, name: &str) -> bool {
        match &self.central {
            Some(index) => index.contains(name),
            None => ClassPath::archive_has_entry(self.zip(), name),
        }
    }

    fn each_name(&self, f: &mut dyn FnMut(&str)) {
        match &self.central {
            Some(index) => index.names().iter_all().for_each(f),
            None => self.zip().each_name(f),
        }
    }
}

/// The outer archive's length and modification time when a
/// [`NestedArchiveEntries`] was opened: a later ask whose file no longer
/// matches is refused (the caller runs the Java body, which reads the jar as
/// it now is).
///
/// Since interpreter round i3 wave 79 (lane L5) also the file's identity
/// where the platform has a stable one ([`FileIdentity`]: device and inode on
/// Unix, empty elsewhere), as the JDK's `ZipFile$Source.Key` keys on
/// `lastModifiedTime` + `fileKey`: a jar replaced through a new file (most
/// tools write a temporary file and rename it) is told from the old one even
/// when the length and modification time match -- a coarse-mtime filesystem,
/// `cp -p` / `rsync -t`, or a reproducible build copied over another
/// (`docs/feature-designs/class-resolution-and-caching.md`).
/// An in-place rewrite of the same length within one mtime tick keeps the
/// inode and stays invisible to any `stat`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct OuterArchiveStamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
    identity: FileIdentity,
}

impl OuterArchiveStamp {
    fn read(path: &Path) -> Option<Self> {
        let meta = fs::metadata(path).ok()?;
        Self::of(&meta)
    }
}

/// The length and modification time of a file on disk, as the nested-jar
/// rows compare it, for a caller that must remember a REFUSAL against the
/// file it refused (interpreter round i2 wave 68, lane L5,
/// `CRATONVM_NESTED_JAR_REFUSAL_STAMP`: `native-builtins`
/// `classloader::memoised_row` retries a refused walk-index or bytes-index
/// row once the outer file's stamp differs from the one read BEFORE the
/// refused build). Opaque: only equality is meaningful.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OuterFileStamp(OuterArchiveStamp);

impl OuterFileStamp {
    /// The stamp of the regular file at `path` (one `stat`), or `None` when
    /// there is no such file (absent, a directory, unreadable metadata).
    pub fn read(path: &Path) -> Option<Self> {
        OuterArchiveStamp::read(path).map(Self)
    }
}

/// The entry names of one nested jar ([`ClassPath::nested_archive_entry_index`])
/// with the outer archive's length and modification time when they were
/// read, so a caller that memoises the index can tell that the fat jar on
/// disk was rewritten since (interpreter round i2 wave 65, lane L5; the
/// walk index of `native-builtins` `classloader::nested_jar_entry_index`,
/// `CRATONVM_NESTED_JAR_WALK_INDEX_STAMP`). The stamp is read BEFORE the
/// archive, so a rewrite racing the read leaves a stamp that no longer
/// matches, never a fresh stamp over stale names.
pub struct NestedEntryNameIndex {
    outer: PathBuf,
    stamp: OuterArchiveStamp,
    names: FxHashSet<String>,
}

impl NestedEntryNameIndex {
    /// Is `name` in the index ([`ClassPath::nested_archive_entry_names`]'s
    /// set: the exact entries plus every versioned entry's unversioned name)?
    pub fn contains(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    /// The number of names in the index.
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Is the index empty?
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Is the outer archive on disk still the file the names were read from
    /// (same length and modification time)? One `stat`.
    pub fn outer_unchanged(&self) -> bool {
        OuterArchiveStamp::read(&self.outer) == Some(self.stamp)
    }
}

/// One outer fat jar opened ONCE -- its stamp, its bytes and its parsed
/// central directory -- so every nested jar inside it is indexed
/// ([`ClassPath::nested_archive_entry_index_in`]) or opened
/// ([`ClassPath::open_nested_archive_in`]) without mapping the file and
/// parsing its whole central directory again (interpreter round i2 wave 66,
/// lane L5, `CRATONVM_NESTED_JAR_SHARED_OUTER`;
/// `i65-L5-proposal-one-outer-archive-per-fat-jar-per-vm`).
///
/// The stamp is read BEFORE the bytes, as [`NestedEntryNameIndex`]'s is, so
/// a rewrite racing the open leaves a stamp that no longer matches. A holder
/// must ask [`Self::unchanged`] before it reuses the value for a new nested
/// jar: what it derives carries this stamp, never a fresher one.
///
/// The bytes are read into memory on every host (interpreter round i4 wave
/// 99 / i5 wave 100, lane L5): a mapping turns a program's in-place
/// truncation of the fat jar into `SIGBUS` on Unix, and on Windows a live
/// mapping blocks deleting or replacing the file while a per-VM holder
/// outlives every Java `JarFile` over it.
pub struct OuterArchive {
    path: PathBuf,
    stamp: OuterArchiveStamp,
    backing: ArchiveBacking,
    archive: Mutex<SharedArchive>,
}

impl OuterArchive {
    /// Open the archive at `path`, or `None` when it is not a readable
    /// archive file.
    pub fn open(path: &Path) -> Option<Self> {
        let stamp = OuterArchiveStamp::read(path)?;
        // Read into memory on every platform, never mapped: an outer fat jar
        // a running program may truncate or rewrite in place, where a mapping
        // turns the next read past the new end into `SIGBUS` on Unix
        // (interpreter round i4 wave 99, lane L5;
        // `i99-orch-truncated-jar-sigbus-on-linux`). Windows already read it.
        let backing = ArchiveBacking::from_vec(read_file_for_classpath(path).ok()?);
        let archive = ZipArchive::new(Cursor::new(backing.clone())).ok()?;
        Some(Self {
            path: path.to_path_buf(),
            stamp,
            backing,
            archive: Mutex::new(archive),
        })
    }

    /// The path this was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The number of entries in the outer central directory.
    pub fn entry_count(&self) -> usize {
        self.archive.lock().len()
    }

    /// Is the file on disk still the one this was opened from (same length
    /// and modification time)? One `stat`.
    pub fn unchanged(&self) -> bool {
        OuterArchiveStamp::read(&self.path) == Some(self.stamp)
    }

    /// The bytes of the entry `nested_path`: a range of the outer bytes for
    /// a stored entry (no copy), inflated once for a deflated one. `None` for
    /// an empty, unsafe or absent name, or an entry that does not read.
    fn nested_backing(&self, nested_path: &str) -> Option<ArchiveBacking> {
        if nested_path.is_empty() || !is_safe_entry_name(nested_path) {
            return None;
        }
        let mut guard = self.archive.lock();
        Some(ArchiveBacking::Shared(
            ClassPath::find_shared_in_archive_locked(&mut guard, &self.backing, nested_path)?,
        ))
    }
}

impl OuterArchiveStamp {
    /// The stamp of an already-open file's metadata (`None` for a
    /// non-file), the same fields [`Self::read`] compares.
    fn of(meta: &fs::Metadata) -> Option<Self> {
        meta.is_file().then(|| Self {
            len: meta.len(),
            modified: meta.modified().ok(),
            identity: FileIdentity::of(meta),
        })
    }
}

/// The reader an [`OuterArchiveIndex`]'s parsed archive reads through: a
/// file handle that exists only for the duration of ONE ask
/// ([`OuterArchiveIndex::read_nested`]) and is `None` between asks, so the
/// index holds no open handle and no mapping. A read or seek while closed is
/// an `io::Error` (the `zip` reader then fails the entry, the caller runs the
/// Java body).
#[derive(Clone, Default)]
struct OuterFileSlot(Arc<Mutex<Option<fs::File>>>);

impl OuterFileSlot {
    fn closed() -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "outer archive is not open",
        )
    }
}

impl Read for OuterFileSlot {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.0.lock().as_mut() {
            Some(file) => file.read(buf),
            None => Err(Self::closed()),
        }
    }
}

impl std::io::Seek for OuterFileSlot {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        match self.0.lock().as_mut() {
            Some(file) => std::io::Seek::seek(file, pos),
            None => Err(Self::closed()),
        }
    }
}

/// The open outer file a [`NestedWindow`] reads through -- present only for
/// the duration of ONE ask, as [`OuterFileSlot`]'s -- and a small read-ahead
/// buffer, cleared whenever the file is closed so no bytes are held between
/// asks (interpreter round i2 wave 68, lane L5,
/// `CRATONVM_NESTED_JAR_BYTES_DIRECTORY`).
#[derive(Default)]
struct WindowFile {
    file: Option<fs::File>,
    /// Bytes of the file starting at the absolute offset `buf_at`.
    buf: Vec<u8>,
    buf_at: u64,
}

impl WindowFile {
    fn open(&mut self, file: fs::File) {
        self.buf.clear();
        self.file = Some(file);
    }

    /// Take the file out (closing the window) and drop the read-ahead.
    fn close(&mut self) -> Option<fs::File> {
        self.buf = Vec::new();
        self.buf_at = 0;
        self.file.take()
    }
}

/// The read-ahead of a [`NestedWindow`]: one positioned read of this many
/// bytes serves a local header and, for a class of ordinary size, its data.
const NESTED_WINDOW_READ_AHEAD: usize = 16 * 1024;

/// A `Read + Seek` view of the byte range `[start, start + len)` of the
/// outer file -- a nested jar STORED in its fat jar -- through a shared
/// [`WindowFile`]. A seek only moves the position (no system call); a read
/// is served from the read-ahead when it covers the position, read directly
/// when it is at least [`NESTED_WINDOW_READ_AHEAD`] long, and refills the
/// read-ahead otherwise. A read while the file is closed is an `io::Error`
/// (the `zip` reader then fails the entry, the caller runs the Java body);
/// a short file is an early end of stream (a CRC or length mismatch for the
/// `zip` reader, the same outcome).
struct NestedWindow {
    state: Arc<Mutex<WindowFile>>,
    start: u64,
    len: u64,
    pos: u64,
}

impl Read for NestedWindow {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        use std::io::Seek as _;
        if out.is_empty() || self.pos >= self.len {
            return Ok(0);
        }
        let want =
            usize::try_from((self.len - self.pos).min(out.len() as u64)).unwrap_or(out.len());
        let at = self.start + self.pos;
        let mut guard = self.state.lock();
        let WindowFile { file, buf, buf_at } = &mut *guard;
        let Some(file) = file.as_mut() else {
            return Err(OuterFileSlot::closed());
        };
        if at >= *buf_at && at < *buf_at + buf.len() as u64 {
            let off = (at - *buf_at) as usize;
            let n = want.min(buf.len() - off);
            out[..n].copy_from_slice(&buf[off..off + n]);
            self.pos += n as u64;
            return Ok(n);
        }
        file.seek(std::io::SeekFrom::Start(at))?;
        if want >= NESTED_WINDOW_READ_AHEAD {
            let n = file.read(&mut out[..want])?;
            self.pos += n as u64;
            return Ok(n);
        }
        let fill = usize::try_from((self.len - self.pos).min(NESTED_WINDOW_READ_AHEAD as u64))
            .unwrap_or(NESTED_WINDOW_READ_AHEAD);
        buf.clear();
        buf.resize(fill, 0);
        let mut got = 0;
        while got < fill {
            match file.read(&mut buf[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    buf.clear();
                    return Err(e);
                }
            }
        }
        buf.truncate(got);
        *buf_at = at;
        let n = want.min(got);
        out[..n].copy_from_slice(&buf[..n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl std::io::Seek for NestedWindow {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        let to = match pos {
            std::io::SeekFrom::Start(n) => Some(n),
            std::io::SeekFrom::End(d) => self.len.checked_add_signed(d),
            std::io::SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        match to {
            Some(to) => {
                self.pos = to;
                Ok(to)
            }
            None => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek before the start of the nested jar",
            )),
        }
    }
}

/// [`OuterArchiveIndex::open_nested_window`]'s answer.
enum NestedWindowRead {
    /// A stored nested jar: its parsed directory over a closed window.
    Window {
        state: Arc<Mutex<WindowFile>>,
        archive: ZipArchive<NestedWindow>,
        /// The central-directory records walked while the window was open
        /// ([`count_central_records`]; wave 79, L5), for the plain view's
        /// duplicate-name check.
        records: Option<usize>,
    },
    /// Not a plain stored entry (deflated): no window.
    NotStored,
}

/// One outer fat jar's parsed central directory and stamp, WITHOUT its bytes
/// (interpreter round i2 wave 67, lane L5, `CRATONVM_NESTED_JAR_OUTER_INDEX`;
/// `i66-L5-proposal-an-outer-index-without-the-outer-bytes`): the successor
/// of [`OuterArchive`] that holds no mapping (Linux/macOS) and no resident
/// copy (Windows) across asks.
///
/// Each nested jar is read at the time of the ask
/// ([`ClassPath::nested_archive_entry_index_in_index`],
/// [`ClassPath::open_nested_archive_in_index`]): the file is opened, its
/// metadata compared with the stamp, the ONE nested entry read with a
/// bounded seek + read (its local header, then exactly its compressed size;
/// inflated when deflated, CRC-checked by the `zip` reader either way) into
/// an owned buffer, the metadata compared again, and the file closed. A
/// truncation racing the read is a short read -- an `io::Error` or a CRC /
/// length mismatch -- answering `None` (the body runs), never a signal; a
/// file deleted or replaced between asks is simply not found or refused by
/// the stamp, as HotSpot's `ZipFile` reopen would see it.
///
/// The stamp is read BEFORE the central directory, as [`OuterArchive`]'s is.
pub struct OuterArchiveIndex {
    path: PathBuf,
    stamp: OuterArchiveStamp,
    slot: OuterFileSlot,
    archive: Mutex<ZipArchive<OuterFileSlot>>,
}

impl OuterArchiveIndex {
    /// Parse the central directory of the archive at `path` (the file is
    /// open only while it is parsed), or `None` when it is not a readable
    /// archive file or changed while it was parsed.
    pub fn open(path: &Path) -> Option<Self> {
        let stamp = OuterArchiveStamp::read(path)?;
        let file = fs::File::open(path).ok()?;
        if OuterArchiveStamp::of(&file.metadata().ok()?) != Some(stamp) {
            return None;
        }
        let slot = OuterFileSlot::default();
        *slot.0.lock() = Some(file);
        let parsed = ZipArchive::new(slot.clone());
        // Closed again before anything else: nothing is held across asks.
        let file = slot.0.lock().take();
        let after = file.as_ref().and_then(|f| f.metadata().ok());
        drop(file);
        let archive = parsed.ok()?;
        if after.as_ref().and_then(OuterArchiveStamp::of) != Some(stamp) {
            return None;
        }
        Some(Self {
            path: path.to_path_buf(),
            stamp,
            slot,
            archive: Mutex::new(archive),
        })
    }

    /// The path this was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The number of entries in the outer central directory.
    pub fn entry_count(&self) -> usize {
        self.archive.lock().len()
    }

    /// Is the file on disk still the one this was parsed from (same length
    /// and modification time)? One `stat`.
    pub fn unchanged(&self) -> bool {
        OuterArchiveStamp::read(&self.path) == Some(self.stamp)
    }

    /// Does this hold an open handle to the file? Never between asks (the
    /// property the type exists for; for tests and the page's measurement).
    pub fn holds_open_file(&self) -> bool {
        self.slot.0.lock().is_some()
    }

    /// The uncompressed bytes of the FILE entry `nested_path`, read now
    /// (see the type). `None` for an empty, unsafe, absent, directory or
    /// encrypted name, an entry over [`MAX_UNCOMPRESSED_ENTRY_BYTES`], a file
    /// whose stamp no longer matches (before or after the read), or a read
    /// that fails or comes back short or with a bad CRC.
    fn read_nested(&self, nested_path: &str) -> Option<Vec<u8>> {
        if nested_path.is_empty() || !is_safe_entry_name(nested_path) {
            return None;
        }
        let mut guard = self.archive.lock();
        if guard.index_for_name(nested_path).is_none() {
            return None;
        }
        let file = fs::File::open(&self.path).ok()?;
        if OuterArchiveStamp::of(&file.metadata().ok()?) != Some(self.stamp) {
            return None;
        }
        *self.slot.0.lock() = Some(file);
        let read = Self::read_entry_now(&mut guard, nested_path);
        // Closed before the archive lock is released: the next ask opens
        // the file afresh.
        let file = self.slot.0.lock().take();
        drop(guard);
        let after = file.as_ref().and_then(|f| f.metadata().ok());
        drop(file);
        if after.as_ref().and_then(OuterArchiveStamp::of) != Some(self.stamp) {
            return None;
        }
        read
    }

    /// The nested jar `nested_path` as a byte range of the outer file, with
    /// its own central directory parsed now through that range and NOTHING of
    /// its bytes kept (interpreter round i2 wave 68, lane L5,
    /// `CRATONVM_NESTED_JAR_BYTES_DIRECTORY`). The file is opened, its stamp
    /// compared, the outer entry's local header read (for its data offset),
    /// the nested jar's END record and central directory read through a
    /// [`NestedWindow`], the stamp compared again, and the file closed.
    ///
    /// [`NestedWindowRead::NotStored`] for a nested jar that is not a plain
    /// STORED entry (deflated: its entries are no ranges of the outer file;
    /// the caller keeps [`Self::read_nested`]'s copy). `None` for an empty,
    /// unsafe, absent, directory or encrypted name, a size over
    /// [`MAX_UNCOMPRESSED_ENTRY_BYTES`], a range past the end of the file, a
    /// nested directory that does not parse, or a file whose stamp no longer
    /// matches (before or after).
    fn open_nested_window(&self, nested_path: &str) -> Option<NestedWindowRead> {
        if nested_path.is_empty() || !is_safe_entry_name(nested_path) {
            return None;
        }
        let mut guard = self.archive.lock();
        guard.index_for_name(nested_path)?;
        let file = fs::File::open(&self.path).ok()?;
        if OuterArchiveStamp::of(&file.metadata().ok()?) != Some(self.stamp) {
            return None;
        }
        *self.slot.0.lock() = Some(file);
        let range = Self::stored_range_now(&mut guard, nested_path);
        let file = self.slot.0.lock().take();
        drop(guard);
        let file = file?;
        let (start, len) = match range? {
            Some(range) => range,
            None => return Some(NestedWindowRead::NotStored),
        };
        if start.checked_add(len)? > self.stamp.len {
            return None;
        }
        let state = Arc::new(Mutex::new(WindowFile::default()));
        state.lock().open(file);
        let window = NestedWindow {
            state: Arc::clone(&state),
            start,
            len,
            pos: 0,
        };
        let parsed = ZipArchive::new(window);
        // The records the directory really has (the `zip` map keeps one per
        // NAME), walked while the window is open (wave 79, L5).
        let records = parsed.as_ref().ok().and_then(|archive| {
            let mut probe = NestedWindow {
                state: Arc::clone(&state),
                start,
                len,
                pos: 0,
            };
            count_central_records(&mut probe, archive.central_directory_start(), archive.len())
        });
        let file = state.lock().close();
        let after = file.as_ref().and_then(|f| f.metadata().ok());
        drop(file);
        if after.as_ref().and_then(OuterArchiveStamp::of) != Some(self.stamp) {
            return None;
        }
        Some(NestedWindowRead::Window {
            state,
            archive: parsed.ok()?,
            records,
        })
    }

    /// [`Self::open_nested_window`]'s outer-entry probe, with the file
    /// already in the slot: `Some(Some((data offset, size)))` for a stored
    /// file entry, `Some(None)` for any other method, `None` when it does not
    /// read.
    fn stored_range_now(
        archive: &mut ZipArchive<OuterFileSlot>,
        nested_path: &str,
    ) -> Option<Option<(u64, u64)>> {
        let entry = archive.by_name(nested_path).ok()?;
        if entry.is_dir() || entry.encrypted() {
            return None;
        }
        let size = entry.size();
        if size > MAX_UNCOMPRESSED_ENTRY_BYTES {
            return None;
        }
        if entry.compression() != zip::CompressionMethod::Stored || entry.compressed_size() != size
        {
            return Some(None);
        }
        Some(Some((entry.data_start(), size)))
    }

    /// [`Self::read_nested`]'s read, with the file already in the slot.
    fn read_entry_now(
        archive: &mut ZipArchive<OuterFileSlot>,
        nested_path: &str,
    ) -> Option<Vec<u8>> {
        let mut entry = archive.by_name(nested_path).ok()?;
        if entry.is_dir() || entry.encrypted() {
            return None;
        }
        let size = entry.size();
        if size > MAX_UNCOMPRESSED_ENTRY_BYTES {
            return None;
        }
        let data = read_entry_capped(&mut entry, size).ok()?;
        (data.len() as u64 == size).then_some(data)
    }
}

/// Which file an open handle reads, beyond its stamp: the device and inode
/// on Unix, so a handle whose path now names ANOTHER file (replaced by a
/// rename, or deleted and recreated) is told from one whose file is still
/// there even when the two files have the same length and modification
/// time. Empty elsewhere ([`OUTER_HANDLE_REUSE_SUPPORTED`] is `false` there,
/// so no handle is ever compared by it).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct FileIdentity {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
}

impl FileIdentity {
    fn of(meta: &fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            Self {
                dev: meta.dev(),
                ino: meta.ino(),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = meta;
            Self {}
        }
    }
}

/// Does [`OuterHandlePool`] keep handles at all on this host? Unix only: a
/// handle kept there is compared with the path by device and inode before
/// every reuse ([`FileIdentity`]). On Windows the standard library exposes
/// no stable file identity (`MetadataExt::file_index` is unstable, and a
/// creation time is inherited by a file recreated under the same name within
/// 15 s -- file-system tunnelling), so a handle to a file replaced with one
/// of the same length and modification time could not be told from the
/// current file; there the pool is inert and every read opens and closes the
/// file as before (which also means no handle of the VM's ever delays a
/// delete or a replace there).
const OUTER_HANDLE_REUSE_SUPPORTED: bool = cfg!(unix);

/// How long an idle kept handle stays open: the end of an ask burst
/// ([`OuterHandlePool`]).
pub const OUTER_HANDLE_IDLE: std::time::Duration = std::time::Duration::from_millis(50);

/// At most this many fat jars have a kept handle at once (one handle each),
/// and at most this many recent closes are remembered.
const OUTER_HANDLE_POOL_LIMIT: usize = 4;

/// The open outer fat-jar handles ONE VM keeps for the length of an ask burst
/// (interpreter round i2 wave 69, lane L5,
/// `CRATONVM_NESTED_JAR_OUTER_HANDLE_REUSE`;
/// `i68-L5-proposal-keep-one-outer-handle-open-per-ask-burst`), so a burst of
/// class loads served from the directory-only rows of
/// `CRATONVM_NESTED_JAR_BYTES_DIRECTORY`
/// ([`NestedArchiveEntries::entry_bytes_pooled`]) does not `open` + `close`
/// the fat jar once per class.
///
/// The burst: a read that opened the fat jar afresh and finds that it closed
/// the same path less than [`OUTER_HANDLE_IDLE`] ago keeps its handle instead
/// of closing it ("parks" it); every later read of that path takes the
/// parked handle and parks it again. A handle idle for [`OUTER_HANDLE_IDLE`]
/// is closed by a small reaper thread, spawned when a handle is parked while
/// none runs and gone as soon as nothing is parked, so the VM holds no handle
/// longer than about [`OUTER_HANDLE_IDLE`] after the last read of a burst --
/// whether or not anything is asked afterwards -- and holds nothing while
/// idle. An isolated read (no close of the path within the interval) opens
/// and closes as before, with no thread.
///
/// Never a mapping, never a byte buffer (the read-ahead is dropped with
/// every read), never a handle across a stamp change: a handle is parked
/// only after a read whose after-read `fstat` matched the row's stamp, and
/// it is taken only for a row of that stamp, after a `stat` of the PATH finds
/// the same length, modification time, device and inode (a file replaced by a
/// rename or deleted is not read through the old handle; the read then opens
/// the path as before, and is refused or reads the new file exactly as
/// without the pool). A truncation through the same file is seen by that
/// `stat` and by the after-read `fstat`. On Windows the pool keeps nothing
/// ([`OUTER_HANDLE_REUSE_SUPPORTED`]).
///
/// Dropping the pool (the VM's registry going away) closes every kept handle;
/// the reaper holds only a weak reference.
pub struct OuterHandlePool {
    state: Arc<Mutex<OuterHandleState>>,
    opens: std::sync::atomic::AtomicU64,
    reuses: std::sync::atomic::AtomicU64,
}

struct OuterHandleState {
    idle: std::time::Duration,
    parked: Vec<ParkedOuterHandle>,
    /// Paths closed by a read that did not park, and when.
    recent: Vec<(PathBuf, std::time::Instant)>,
    /// Is a reaper thread running (it clears this, under the lock, when it
    /// finds nothing parked and exits)?
    reaper: bool,
    /// Reaper threads spawned so far (tests and the page's measurement).
    reapers_spawned: u64,
}

struct ParkedOuterHandle {
    path: PathBuf,
    stamp: OuterArchiveStamp,
    identity: FileIdentity,
    file: fs::File,
    last_used: std::time::Instant,
}

impl Default for OuterHandlePool {
    fn default() -> Self {
        Self::new()
    }
}

impl OuterHandlePool {
    /// A pool whose bursts end after [`OUTER_HANDLE_IDLE`] of idleness.
    pub fn new() -> Self {
        Self::with_idle(OUTER_HANDLE_IDLE)
    }

    /// A pool whose bursts end after `idle` of idleness.
    pub fn with_idle(idle: std::time::Duration) -> Self {
        Self {
            state: Arc::new(Mutex::new(OuterHandleState {
                idle,
                parked: Vec::new(),
                recent: Vec::new(),
                reaper: false,
                reapers_spawned: 0,
            })),
            opens: std::sync::atomic::AtomicU64::new(0),
            reuses: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// How many handles are kept open now.
    pub fn held(&self) -> usize {
        self.state.lock().parked.len()
    }

    /// Reads through the pool that opened the fat jar afresh.
    pub fn opens(&self) -> u64 {
        self.opens.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Reads through the pool that reused a kept handle.
    pub fn reuses(&self) -> u64 {
        self.reuses.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Reaper threads spawned so far (one per burst at most).
    pub fn reapers_spawned(&self) -> u64 {
        self.state.lock().reapers_spawned
    }

    /// Is a reaper thread running now?
    pub fn reaper_running(&self) -> bool {
        self.state.lock().reaper
    }

    /// Does the pool keep a handle of `path` for a row of `stamp` now (a lock
    /// and a scan of at most [`OUTER_HANDLE_POOL_LIMIT`] entries, no system
    /// call)? Always `false` where the pool keeps nothing
    /// ([`OUTER_HANDLE_REUSE_SUPPORTED`]). The handle may still be refused by
    /// the next [`Self::take`] (idle past the interval, or the path's `stat`
    /// no longer matching). Interpreter round i3 wave 71, lane L5
    /// (`CRATONVM_NESTED_JAR_POOLED_STAT`; see
    /// [`NestedArchiveEntries::pooled_read_checks_outer`]).
    fn holds(&self, path: &Path, stamp: OuterArchiveStamp) -> bool {
        OUTER_HANDLE_REUSE_SUPPORTED
            && self
                .state
                .lock()
                .parked
                .iter()
                .any(|h| h.stamp == stamp && h.path == path)
    }

    /// Close every kept handle now.
    pub fn close_all(&self) {
        let parked = std::mem::take(&mut self.state.lock().parked);
        drop(parked);
    }

    /// The kept handle of `path` for a row of `stamp`, when the file at
    /// `path` is still the one it reads (one `stat`; see the type), else
    /// `None` (any kept handle of `path` is closed).
    fn take(&self, path: &Path, stamp: OuterArchiveStamp) -> Option<fs::File> {
        if !OUTER_HANDLE_REUSE_SUPPORTED {
            return None;
        }
        let (handle, idle) = {
            let mut state = self.state.lock();
            let at = state.parked.iter().position(|h| h.path == path)?;
            (state.parked.swap_remove(at), state.idle)
        };
        if handle.stamp != stamp || handle.last_used.elapsed() >= idle {
            return None;
        }
        let meta = fs::metadata(path).ok()?;
        if OuterArchiveStamp::of(&meta) != Some(stamp) || FileIdentity::of(&meta) != handle.identity
        {
            return None;
        }
        self.reuses
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(handle.file)
    }

    /// Count a read that opened the fat jar afresh.
    fn note_open(&self) {
        self.opens
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// After a clean read of `path` (its after-read `fstat` `meta` matched
    /// the row's `stamp`): keep `file` when the read is part of a burst
    /// (`reused`, or `path` was closed within the interval), else close it
    /// and remember the close.
    fn give_back(
        &self,
        path: &Path,
        stamp: OuterArchiveStamp,
        meta: &fs::Metadata,
        file: fs::File,
        reused: bool,
    ) {
        if !OUTER_HANDLE_REUSE_SUPPORTED {
            return;
        }
        let now = std::time::Instant::now();
        let mut closed = Some(file);
        let spawn = {
            let mut state = self.state.lock();
            let idle = state.idle;
            state
                .recent
                .retain(|(_, at)| now.duration_since(*at) < idle);
            let burst = reused || state.recent.iter().any(|(p, _)| p == path);
            if !burst {
                if state.recent.len() >= OUTER_HANDLE_POOL_LIMIT {
                    state.recent.remove(0);
                }
                state.recent.push((path.to_path_buf(), now));
                false
            } else if state.parked.len() >= OUTER_HANDLE_POOL_LIMIT
                || state.parked.iter().any(|h| h.path == path)
            {
                // Full, or another thread parked one for this path: close.
                false
            } else if let Some(file) = closed.take() {
                state.parked.push(ParkedOuterHandle {
                    path: path.to_path_buf(),
                    stamp,
                    identity: FileIdentity::of(meta),
                    file,
                    last_used: now,
                });
                let spawn = !state.reaper;
                if spawn {
                    state.reaper = true;
                    state.reapers_spawned += 1;
                }
                spawn
            } else {
                false
            }
        };
        // Closed outside the lock.
        drop(closed);
        if spawn && !self.spawn_reaper() {
            // No thread, no kept handle: close everything now.
            let parked = {
                let mut state = self.state.lock();
                state.reaper = false;
                std::mem::take(&mut state.parked)
            };
            drop(parked);
        }
    }

    fn spawn_reaper(&self) -> bool {
        // Phase 5 fast boot (WP3): a boot image's capture run starts no OS
        // thread before its snapshot point; the caller's no-thread fallback
        // closes the parked handles at once, and a later park spawns it.
        if cratonvm_types::image_layout::capturing() {
            return false;
        }
        let state = Arc::downgrade(&self.state);
        std::thread::Builder::new()
            .name("cratonvm-outer-handle-reaper".to_string())
            .spawn(move || reap_outer_handles(&state))
            .is_ok()
    }
}

impl Drop for OuterHandlePool {
    fn drop(&mut self) {
        self.close_all();
    }
}

/// The reaper of an [`OuterHandlePool`]: closes each kept handle once it has
/// been idle for the pool's interval, sleeping until the next one is due,
/// and exits (clearing `reaper` under the lock) when nothing is kept or the
/// pool is gone.
fn reap_outer_handles(state: &std::sync::Weak<Mutex<OuterHandleState>>) {
    loop {
        let wait = {
            let Some(state) = state.upgrade() else {
                return;
            };
            let mut guard = state.lock();
            let now = std::time::Instant::now();
            let idle = guard.idle;
            let mut expired = Vec::new();
            let mut i = 0;
            while i < guard.parked.len() {
                if now.duration_since(guard.parked[i].last_used) >= idle {
                    expired.push(guard.parked.swap_remove(i));
                } else {
                    i += 1;
                }
            }
            let next = guard.parked.iter().map(|h| h.last_used + idle).min();
            if next.is_none() {
                guard.reaper = false;
            }
            drop(guard);
            // Closed outside the lock.
            drop(expired);
            match next {
                Some(due) => due.saturating_duration_since(now),
                None => return,
            }
        };
        std::thread::sleep(wait.max(std::time::Duration::from_millis(1)));
    }
}

/// A nested jar opened for its entries' bytes
/// ([`ClassPath::open_nested_archive`]; interpreter round i2 wave 63, lane L5,
/// `CRATONVM_NESTED_JAR_BYTES_INDEX`).
///
/// It answers an entry only through its PLAIN view: a jar with no
/// `META-INF/versions/` entry (a Multi-Release jar's versioned view may serve
/// another entry's bytes under the name) and no signature file (a signed
/// jar's stream verifies what it reads), and no duplicated entry name. For
/// any other jar [`Self::entry_bytes`] answers `None` and the caller keeps
/// the Java body.
pub struct NestedArchiveEntries {
    outer: PathBuf,
    stamp: OuterArchiveStamp,
    archive: NestedArchiveReader,
    entry_index: FxHashSet<String>,
    plain_view: bool,
}

impl NestedArchiveEntries {
    /// Does the jar have the plain view (see the type)?
    pub fn plain_view(&self) -> bool {
        self.plain_view
    }

    /// Is `name` an exact entry of the nested jar's central directory?
    pub fn has_entry(&self, name: &str) -> bool {
        self.entry_index.contains(name)
    }

    /// Is the outer archive on disk still the file this was opened from
    /// (same length and modification time)?
    pub fn outer_unchanged(&self) -> bool {
        OuterArchiveStamp::read(&self.outer) == Some(self.stamp)
    }

    /// Will the next pooled read of this row
    /// ([`Self::entry_bytes_pooled`] with `pool`) itself establish that the
    /// outer file is still the one the row was built from, so a caller may
    /// skip its own [`Self::outer_unchanged`] `stat` on the hit (interpreter
    /// round i3 wave 71, lane L5, `CRATONVM_NESTED_JAR_POOLED_STAT`;
    /// `i69-L5-proposal-one-stat-per-pooled-read`)? `true` only for a
    /// directory-only row (`CRATONVM_NESTED_JAR_BYTES_DIRECTORY`) whose outer
    /// path has a kept handle of the row's stamp in `pool`: that read `stat`s
    /// the path against the stamp and the handle's identity before using the
    /// handle ([`OuterHandlePool::take`]), opens the path afresh and compares
    /// its `fstat` with the stamp when that fails, and compares the handle's
    /// `fstat` with the stamp after the read -- answering `None` (the body
    /// runs) for a changed file, never stale bytes. The row itself is then
    /// rebuilt on the next ask, whose pool no longer holds the handle (a
    /// failed read does not park it). No system call.
    pub fn pooled_read_checks_outer(&self, pool: &OuterHandlePool) -> bool {
        matches!(self.archive, NestedArchiveReader::Window { .. })
            && pool.holds(&self.outer, self.stamp)
    }

    /// The bytes of the FILE entry `name` exactly as a `JarFile` stream over
    /// the nested jar yields them, or `None` when that is not certain: no
    /// plain view, `name` empty or a directory (`.../`), `name` absent, or
    /// `name + "/"` present too (a reader matching either could pick the
    /// directory), or an entry that does not read cleanly (the `zip` reader
    /// checks the CRC at the end of every entry, stored or deflated; the
    /// length must equal the central directory's `size`).
    pub fn entry_bytes(&self, name: &str) -> Option<Vec<u8>> {
        self.entry_bytes_checked(name, None, None, &mut false)
    }

    /// [`Self::entry_bytes`] that, for a directory-only row
    /// (`CRATONVM_NESTED_JAR_BYTES_DIRECTORY`), reads the outer file through
    /// `pool`'s kept handle when the read is part of a burst
    /// (`CRATONVM_NESTED_JAR_OUTER_HANDLE_REUSE`; interpreter round i2 wave
    /// 69, lane L5; see [`OuterHandlePool`]). The same answer as
    /// [`Self::entry_bytes`] for every row and every file state; `pool` is
    /// not consulted for a row that holds its bytes.
    pub fn entry_bytes_pooled(
        &self,
        name: &str,
        pool: Option<&OuterHandlePool>,
    ) -> Option<Vec<u8>> {
        self.entry_bytes_pooled_noting(name, pool).0
    }

    /// [`Self::entry_bytes_pooled`], and whether the read went through a
    /// kept handle (the LOADER census's `nested-jar-outer-handle-reused=`;
    /// interpreter round i2 wave 69, lane L5).
    pub fn entry_bytes_pooled_noting(
        &self,
        name: &str,
        pool: Option<&OuterHandlePool>,
    ) -> (Option<Vec<u8>>, bool) {
        let mut reused = false;
        let bytes = self.entry_bytes_checked(name, None, pool, &mut reused);
        (bytes, reused)
    }

    /// [`Self::entry_bytes`], and only when the entry's central-directory
    /// CRC-32 and uncompressed size are `crc32` / `size` -- what a Java
    /// `ZipFile` that read the same file's central directory recorded in its
    /// `ZipEntry` (`crc`, `size`). A mismatch means the two readers are not
    /// looking at the same bytes (the file was replaced between their opens),
    /// and answers `None`. The jar-loader bytes index (`native-builtins`
    /// `classloader::jar_loader_resource_bytes`,
    /// `CRATONVM_JAR_LOADER_BYTES_INDEX`; interpreter round i2 wave 64, lane
    /// L5).
    pub fn entry_bytes_matching(&self, name: &str, crc32: u32, size: u64) -> Option<Vec<u8>> {
        self.entry_bytes_checked(name, Some((crc32, size)), None, &mut false)
    }

    fn entry_bytes_checked(
        &self,
        name: &str,
        expect: Option<(u32, u64)>,
        pool: Option<&OuterHandlePool>,
        reused_out: &mut bool,
    ) -> Option<Vec<u8>> {
        if !self.plain_view
            || name.is_empty()
            || name.ends_with('/')
            || !self.entry_index.contains(name)
            || self.entry_index.contains(&format!("{name}/"))
        {
            return None;
        }
        match &self.archive {
            NestedArchiveReader::Held(archive) => {
                let mut guard = archive.lock();
                Self::read_checked_entry(&mut *guard, name, expect)
            }
            NestedArchiveReader::Window { state, archive } => {
                // The outer file is opened for this one entry and closed
                // again; the stamp is compared on the open handle before and
                // after the read, as `OuterArchiveIndex::read_nested` does.
                // With a `pool` (wave 69, `CRATONVM_NESTED_JAR_OUTER_HANDLE_REUSE`)
                // a read inside a burst takes the kept handle instead of
                // opening (its path re-checked by `stat`, see `OuterHandlePool`)
                // and a clean read gives it back instead of closing it.
                let mut guard = archive.lock();
                let kept = pool.and_then(|pool| pool.take(&self.outer, self.stamp));
                let reused = kept.is_some();
                *reused_out = reused;
                let file = match kept {
                    Some(file) => file,
                    None => {
                        let file = fs::File::open(&self.outer).ok()?;
                        if OuterArchiveStamp::of(&file.metadata().ok()?) != Some(self.stamp) {
                            return None;
                        }
                        if let Some(pool) = pool {
                            pool.note_open();
                        }
                        file
                    }
                };
                state.lock().open(file);
                let read = Self::read_checked_entry(&mut *guard, name, expect);
                let file = state.lock().close();
                drop(guard);
                let file = file?;
                let after = file.metadata().ok();
                let unchanged = after.as_ref().and_then(OuterArchiveStamp::of) == Some(self.stamp);
                match (pool, after) {
                    (Some(pool), Some(meta)) if unchanged && read.is_some() => {
                        pool.give_back(&self.outer, self.stamp, &meta, file, reused);
                    }
                    _ => drop(file),
                }
                if !unchanged {
                    return None;
                }
                read
            }
        }
    }

    /// [`Self::entry_bytes_checked`]'s read out of either reader: the FILE
    /// entry `name`, with `expect`'s CRC-32 and size when given, CRC-checked
    /// by the `zip` reader and of exactly the central directory's size.
    fn read_checked_entry<R: Read + std::io::Seek>(
        archive: &mut ZipArchive<R>,
        name: &str,
        expect: Option<(u32, u64)>,
    ) -> Option<Vec<u8>> {
        let mut entry = archive.by_name(name).ok()?;
        if entry.is_dir() {
            return None;
        }
        let size = entry.size();
        if let Some((crc32, expected_size)) = expect {
            if entry.crc32() != crc32 || size != expected_size {
                return None;
            }
        }
        if size > MAX_UNCOMPRESSED_ENTRY_BYTES {
            return None;
        }
        let data = read_entry_capped(&mut entry, size).ok()?;
        (data.len() as u64 == size).then_some(data)
    }

    /// Does this row hold the nested jar's bytes (a copy or a mapping range),
    /// as every row did before wave 68? `false` for a row built by
    /// [`ClassPath::open_nested_archive_directory_in_index`] over a stored
    /// nested jar, which holds only its directory
    /// (`CRATONVM_NESTED_JAR_BYTES_DIRECTORY`).
    pub fn holds_nested_bytes(&self) -> bool {
        matches!(self.archive, NestedArchiveReader::Held(_))
    }
}

/// Where a [`NestedArchiveEntries`] reads its entries from.
enum NestedArchiveReader {
    /// The nested jar's bytes, held (an owned copy, or a range of a mapping).
    Held(Mutex<SharedArchive>),
    /// Only the nested jar's parsed directory, over a [`NestedWindow`] of the
    /// outer file that is open only during a read (interpreter round i2 wave
    /// 68, lane L5, `CRATONVM_NESTED_JAR_BYTES_DIRECTORY`).
    Window {
        state: Arc<Mutex<WindowFile>>,
        archive: Mutex<ZipArchive<NestedWindow>>,
    },
}

/// The central-directory file headers (`PK\x01\x02`) of an archive whose
/// directory starts at `start` in `reader`, walked by their own name / extra /
/// comment lengths, counting at most `limit + 1` (enough to tell `limit`
/// from more). `None` when the walk cannot read a header it started.
///
/// Why (interpreter round i3 wave 79, lane L5;
/// `docs/feature-designs/class-resolution-and-caching.md`):
/// the `zip` crate keeps its directory in a map keyed by NAME
/// (`zip-2.4.2` `SharedBuilder::build`, `IndexMap::insert`), so a second
/// record of one name replaces the first and `ZipArchive::len()` counts
/// DISTINCT names -- the plain view's `entry_index.len() == archive.len()`
/// could never see a duplicate. The `zip` reader reads exactly the EOCD's
/// record count from `central_directory_start()`, so the records walked
/// from there are the records it read.
fn count_central_records<R: Read + std::io::Seek>(
    reader: &mut R,
    start: u64,
    limit: usize,
) -> Option<usize> {
    const CENTRAL_HEADER: [u8; 4] = [0x50, 0x4b, 0x01, 0x02];
    reader.seek(std::io::SeekFrom::Start(start)).ok()?;
    let mut header = [0u8; 46];
    let mut count = 0usize;
    while count <= limit {
        if reader.read_exact(&mut header[..4]).is_err() || header[..4] != CENTRAL_HEADER {
            break;
        }
        reader.read_exact(&mut header[4..]).ok()?;
        let field = |at: usize| i64::from(u16::from_le_bytes([header[at], header[at + 1]]));
        reader
            .seek(std::io::SeekFrom::Current(
                field(28) + field(30) + field(32),
            ))
            .ok()?;
        count += 1;
    }
    Some(count)
}

/// [`count_central_records`] for an archive over in-memory bytes (a clone of
/// its reader: an `Arc` and a cursor, no copy).
fn shared_archive_records(archive: &SharedArchive) -> Option<usize> {
    let mut reader = archive.clone().into_inner();
    count_central_records(
        &mut reader,
        archive.central_directory_start(),
        archive.len(),
    )
}

/// The jar-loader rows' PLAIN view: one central-directory record per name
/// (`records`, from [`count_central_records`], equal to the distinct names
/// the `zip` map and `entry_index` hold) and [`nested_archive_view_is_plain`].
/// A duplicated name makes "which entry" a question the two readers could
/// answer differently, so no plain view then.
fn plain_view_of(
    entry_index: &FxHashSet<String>,
    archive_len: usize,
    records: Option<usize>,
) -> bool {
    entry_index.len() == archive_len
        && records == Some(archive_len)
        && nested_archive_view_is_plain(entry_index)
}

/// No `META-INF/versions/` entry and no signature-related file
/// (`META-INF/*.SF`, `*.RSA`, `*.DSA`, `*.EC`, `SIG-*`), compared
/// case-insensitively as `JarFile` does.
fn nested_archive_view_is_plain(names: &FxHashSet<String>) -> bool {
    const META_INF: &[u8] = b"META-INF/";
    const VERSIONS: &[u8] = b"versions/";
    !names.iter().any(|name| {
        let bytes = name.as_bytes();
        if bytes.len() < META_INF.len() || !bytes[..META_INF.len()].eq_ignore_ascii_case(META_INF) {
            return false;
        }
        let rest = &bytes[META_INF.len()..];
        if rest.len() >= VERSIONS.len() && rest[..VERSIONS.len()].eq_ignore_ascii_case(VERSIONS) {
            return true;
        }
        if rest.contains(&b'/') {
            return false;
        }
        let upper = rest.to_ascii_uppercase();
        upper.ends_with(b".SF")
            || upper.ends_with(b".RSA")
            || upper.ends_with(b".DSA")
            || upper.ends_with(b".EC")
            || upper.starts_with(b"SIG-")
    })
}

#[derive(Clone)]
struct ArchiveSlice {
    backing: ArchiveBacking,
    start: usize,
    end: usize,
}

impl AsRef<[u8]> for ArchiveSlice {
    fn as_ref(&self) -> &[u8] {
        &self.backing.as_ref()[self.start..self.end]
    }
}

/// Diagnostic-only (TLD/JAR-scan slowness investigation, 2026-07-21): time
/// every `find_resource`/`find_all_resource_urls` call and print a running
/// summary every 2000 calls when `CRATONVM_DBG_RESOURCE_TIMING=1`. Not a
/// permanent instrumentation point — remove before shipping the real fix.
fn diag_resource_call_wrapper<T>(label: &'static str, f: impl FnOnce() -> T) -> T {
    use std::sync::atomic::{AtomicU64, Ordering};
    static ENABLED: OnceLock<bool> = OnceLock::new();
    let enabled = *ENABLED.get_or_init(|| loader_flags().dbg_resource_timing);
    if !enabled {
        return f();
    }
    static CALLS: AtomicU64 = AtomicU64::new(0);
    static NANOS: AtomicU64 = AtomicU64::new(0);
    let t0 = std::time::Instant::now();
    let result = f();
    let elapsed = t0.elapsed().as_nanos() as u64;
    let calls = CALLS.fetch_add(1, Ordering::Relaxed) + 1;
    let total_nanos = NANOS.fetch_add(elapsed, Ordering::Relaxed) + elapsed;
    if calls % 2000 == 0 || elapsed > 5_000_000 {
        eprintln!(
            "[RES-DIAG] {label} call#{calls} this_call={:?} total_calls={calls} total_time={:?}",
            std::time::Duration::from_nanos(elapsed),
            std::time::Duration::from_nanos(total_nanos),
        );
    }
    result
}

/// Read a classpath file into owned bytes without memory-mapping it.
///
/// Classpath entries are ordinary files controlled by launchers, build tools,
/// and sometimes deployment systems. A concurrent truncate of a memory-mapped
/// JAR can terminate the process on Unix when the mapping is touched. Keeping
/// the read on `File::read_to_end` avoids that signal-level failure mode; if
/// the file changes during the read, callers get an `InvalidData` error and the
/// classpath entry is skipped/fails closed just like a corrupt archive.
fn read_file_for_classpath(path: &Path) -> std::io::Result<Vec<u8>> {
    read_file_for_classpath_stamped(path).map(|(bytes, _)| bytes)
}

/// [`read_file_for_classpath`], with the stamp of the file it actually read
/// (its open handle's metadata, once the before / after reads agree), never
/// a second `stat` of the path that a rename in between could answer for
/// another file (interpreter round i3 wave 79, lane L5;
/// `docs/feature-designs/class-resolution-and-caching.md`).
fn read_file_for_classpath_stamped(
    path: &Path,
) -> std::io::Result<(Vec<u8>, Option<OuterArchiveStamp>)> {
    let mut file = std::fs::File::open(path)?;
    let before = file.metadata()?;
    let mut buf = Vec::with_capacity(safe_with_capacity(before.len()));
    file.read_to_end(&mut buf)?;
    let after = file.metadata()?;
    if classpath_file_metadata_changed(&before, &after) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "classpath file changed while being read: {}",
                path.display()
            ),
        ));
    }
    Ok((buf, OuterArchiveStamp::of(&before)))
}

/// Map an on-disk archive once, falling back to owned bytes when mapping is
/// unavailable. Classpath archives are immutable for the lifetime of a class
/// loader by JVM convention. `CRATONVM_DISABLE_JAR_MMAP=1` retains the copied
/// path for deployment systems that replace/truncate archives in place.
fn read_archive_for_classpath(path: &Path) -> std::io::Result<ArchiveBacking> {
    read_archive_for_classpath_stamped(path).map(|(backing, _)| backing)
}

/// [`read_archive_for_classpath`], with the stamp of the file it actually
/// mapped or read ([`read_file_for_classpath_stamped`]): what
/// `ClassPathEntry::JarFile::stamp` keeps (interpreter round i3 wave 79,
/// lane L5).
fn read_archive_for_classpath_stamped(
    path: &Path,
) -> std::io::Result<(ArchiveBacking, Option<OuterArchiveStamp>)> {
    let from_vec = |(bytes, stamp): (Vec<u8>, Option<OuterArchiveStamp>)| {
        (ArchiveBacking::from_vec(bytes), stamp)
    };
    if loader_flags().disable_jar_mmap {
        return read_file_for_classpath_stamped(path).map(from_vec);
    }
    let file = std::fs::File::open(path)?;
    let before = file.metadata()?;
    if before.len() == 0 {
        return Ok((
            ArchiveBacking::from_vec(Vec::new()),
            OuterArchiveStamp::of(&before),
        ));
    }
    // SAFETY: the mapping is read-only, its owner is held by every archive
    // cursor/slice, and classpath archives are immutable after loader creation.
    // Deployments that cannot provide immutability use the opt-out above.
    // Phase 5 boot image (WP2): a capture run maps at a brokered fixed
    // address and records the file; `None` in every other mode.
    let mapped = if let Some(fixed) = cratonvm_types::image_layout::map_file_readonly(
        cratonvm_types::image_layout::SegmentKind::ClassPathArchive,
        &file,
        path,
    ) {
        ArchiveBacking::Fixed(Arc::new(fixed))
    } else {
        match unsafe { memmap2::MmapOptions::new().map(&file) } {
            Ok(mapped) => ArchiveBacking::Mapped(Arc::new(mapped)),
            Err(_) => return read_file_for_classpath_stamped(path).map(from_vec),
        }
    };
    let after = file.metadata()?;
    if classpath_file_metadata_changed(&before, &after) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "classpath archive changed while being mapped: {}",
                path.display()
            ),
        ));
    }
    Ok((mapped, OuterArchiveStamp::of(&before)))
}

/// [`read_archive_for_classpath_stamped`] for an archive a running program
/// put on the class path ([`ClassPath::add_path`]: a `--compatible`
/// `URLClassLoader`'s jar, or the outer jar of its `<jar>!/<prefix>/` spec).
/// On Windows it is read into memory, never mapped: a live mapping makes
/// Windows refuse to truncate, rewrite, replace or delete the file
/// (`ERROR_USER_MAPPED_FILE`, or `AccessDeniedException` on a rename over
/// it), and the mapping outlives the loader's `close()` in every class whose
/// bytes are a slice of it, where HotSpot's `JarFile` holds nothing once
/// closed. The same choice `OuterArchive::open` and `open_jar_archive` make
/// there. Interpreter round i4 wave 96, lane L5 (`L5W64JarLoaderBytesIndex`,
/// `L5W65`-`L5W69` nested-jar probes and `L5W79NestedJarSameStampRename`,
/// `--compatible` on Windows).
///
/// Every platform since interpreter round i4 wave 99, lane L5
/// (`docs/feature-designs/class-resolution-and-caching.md`):
/// on Linux and macOS the mapping survived the program's truncation of the
/// jar, and the next read of a page past the new end was `SIGBUS`
/// (`L5W67NestedJarOuterTruncate`'s stress phase, `--compatible`), where the
/// JDK's `ZipFile` reads with positioned reads into its own buffers and
/// throws. The copy keeps the stamp the wave-97 re-check compares.
fn read_archive_for_dynamic_classpath_stamped(
    path: &Path,
) -> std::io::Result<(ArchiveBacking, Option<OuterArchiveStamp>)> {
    read_file_for_classpath_stamped(path)
        .map(|(bytes, stamp)| (ArchiveBacking::from_vec(bytes), stamp))
}

/// [`read_archive_for_dynamic_classpath_stamped`] when `dynamic` (an archive
/// of a loader a running program created: [`ClassPath::add_path`],
/// [`ClassPath::for_url_loader`], and the manifest `Class-Path:` jars they
/// expand), else [`read_archive_for_classpath_stamped`] (the VM's own launch
/// class path). Interpreter round i4 wave 97, lane L5.
fn read_archive_for_classpath_as(
    path: &Path,
    dynamic: bool,
) -> std::io::Result<(ArchiveBacking, Option<OuterArchiveStamp>)> {
    if dynamic {
        read_archive_for_dynamic_classpath_stamped(path)
    } else {
        read_archive_for_classpath_stamped(path)
    }
}

/// The bytes of the entry `nested_path` of the archive at `outer`, read
/// through the file (no mapping), or `None` when the archive or the entry
/// cannot be read cleanly (the `zip` reader checks each entry's CRC) or the
/// file changed while it was read. For [`ClassPath::open_nested_archive`]
/// without `map`.
/// The archive at `path` parsed through buffered positioned reads of the file
/// (its central directory only; entries are read when asked), never mapped,
/// or `None` when it is not a readable archive. For one-shot reads of a jar a
/// running program may truncate or rewrite in place: a truncation racing the
/// read is an `io::Error` or a CRC mismatch, as with the JDK's `ZipFile`,
/// never `SIGBUS`. Interpreter round i5 wave 100, lane L5
/// (`interpreter-i99-orch-truncated-jar-sigbus-on-linux-FIXED-20261015.md`).
fn open_archive_unmapped(path: &Path) -> Option<ZipArchive<std::io::BufReader<fs::File>>> {
    let file = fs::File::open(path).ok()?;
    ZipArchive::new(std::io::BufReader::new(file)).ok()
}

fn read_nested_entry_from_file(outer: &Path, nested_path: &str) -> Option<Vec<u8>> {
    let file = std::fs::File::open(outer).ok()?;
    let before = file.metadata().ok()?;
    let mut archive = ZipArchive::new(std::io::BufReader::new(file)).ok()?;
    let bytes = {
        let mut entry = archive.by_name(nested_path).ok()?;
        let size = entry.size();
        if entry.is_dir() || size > MAX_UNCOMPRESSED_ENTRY_BYTES {
            return None;
        }
        let bytes = read_entry_capped(&mut entry, size).ok()?;
        (bytes.len() as u64 == size).then_some(bytes)?
    };
    let after = archive.into_inner().into_inner().metadata().ok()?;
    (!classpath_file_metadata_changed(&before, &after)).then_some(bytes)
}

fn classpath_file_metadata_changed(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    if before.len() != after.len() {
        return true;
    }
    match (before.modified(), after.modified()) {
        (Ok(before), Ok(after)) => before != after,
        _ => false,
    }
}

/// Round 7 audit fix (HIGH #4): bound the canonicalize cache so a
/// long-running JVM that probes many distinct paths (Spring-style
/// directory scans, fileless classloaders, agent-discovered classes)
/// can't grow it without limit. Picked to mirror the analogous round-3/4
/// cap on `class_bytes_cache`. Classpath roots stay constant; resolved-
/// file entries are usually well under this cap, so eviction only kicks
/// in for pathological probe sets.
const CANONICALIZE_CACHE_CAP: usize = 1024;

/// Cached verdict for the `CRATONVM_DBG_GETRESOURCES` env var.
///
/// `find_all_resource_urls` is called once per `ClassPath` instance
/// (bootstrap / extension / application) for every `getResources`
/// probe — a hot path on Spring-style classpath scans. The env var
/// can't change after process start, so `cratonvm_types::flags::runtime_var` (which locks
/// the libc environ and allocates a `String`) is read exactly once
/// here and the boolean verdict reused on every subsequent call.
static DBG_GETRESOURCES: OnceLock<bool> = OnceLock::new();

/// `true` when `CRATONVM_DBG_GETRESOURCES` is set to a non-empty,
/// non-`"0"` value. Computed once and cached for the process lifetime.
fn dbg_getresources() -> bool {
    loader_flags().dbg_getresources
}

/// Cached verdict for the `CRATONVM_HARDEN_MANIFEST_CLASSPATH` env var.
///
/// VULN (low) — manifest `Class-Path` filesystem reach: per the JAR
/// spec, a JAR's `MANIFEST.MF` `Class-Path:` attribute can name
/// *arbitrary* paths (`../../etc`, `file:/etc`, absolute drive roots)
/// and those become additional classpath roots. HotSpot honours this
/// verbatim — the manifest is part of the trusted application bundle,
/// so a relative/absolute escape is the author's prerogative, and
/// matching that behaviour is the default here so legitimate launchers
/// (Surefire booter JARs, Spring Boot, fat JARs that point at a sibling
/// `lib/`) keep working.
///
/// When set to a non-empty, non-`"0"` value this flag opts into a
/// hardened policy: manifest `Class-Path` roots are restricted to
/// descendants of the JAR's own directory (`jar_dir`), so a hostile or
/// tampered manifest cannot pull in `file:/etc` or `..\..` outside the
/// app bundle. Resolution still happens; out-of-tree roots are dropped
/// (with a debug log) rather than silently honoured. Off by default to
/// preserve HotSpot parity. Read once at process start (the env can't
/// change mid-run), mirroring [`dbg_getresources`].
static HARDEN_MANIFEST_CLASSPATH: OnceLock<bool> = OnceLock::new();

/// `true` when `CRATONVM_HARDEN_MANIFEST_CLASSPATH` is set to a
/// non-empty, non-`"0"` value. Computed once and cached for the process
/// lifetime. See [`HARDEN_MANIFEST_CLASSPATH`].
fn harden_manifest_classpath() -> bool {
    loader_flags().harden_manifest_classpath
}

/// Bounded FIFO cache for [`fs::canonicalize`] results.
///
/// Round 7 audit fix (HIGH #4): the previous unbounded `HashMap` had
/// no eviction and never invalidated entries; a long-running process
/// that probes many distinct paths (Spring scanning,
/// agent-instrumented classes, hidden classes) could grow it without
/// bound. The FIFO tracker keeps `paths` and `order` in sync so that
/// when the map hits `CANONICALIZE_CACHE_CAP` entries, the
/// oldest-inserted path is evicted. Mirrors the same pattern used by
/// `class_bytes_cache_fifo` in `class_manager.rs`.
struct CanonicalizeCache {
    paths: HashMap<PathBuf, PathBuf>,
    /// Insertion-order tracker for FIFO eviction. The front is the
    /// oldest entry; the back is the most recent. Kept in sync with
    /// `paths`: every insert pushes to the back; every eviction pops
    /// from the front. Bounded by `CANONICALIZE_CACHE_CAP`.
    order: VecDeque<PathBuf>,
}

impl CanonicalizeCache {
    fn new() -> Self {
        Self {
            paths: HashMap::new(),
            order: VecDeque::with_capacity(CANONICALIZE_CACHE_CAP),
        }
    }

    /// Insert `(key, value)` honouring the FIFO cap. If `key` already
    /// existed the value is overwritten in place and the FIFO position
    /// is left unchanged (avoiding a linear `VecDeque` scan on the
    /// hot insert path — duplicate inserts are race-resolution writes
    /// that pick identical values, see callers).
    fn insert(&mut self, key: PathBuf, value: PathBuf) {
        if self.paths.contains_key(&key) {
            self.paths.insert(key, value);
            return;
        }
        if self.paths.len() >= CANONICALIZE_CACHE_CAP {
            if let Some(oldest) = self.order.pop_front() {
                self.paths.remove(&oldest);
            }
        }
        self.order.push_back(key.clone());
        self.paths.insert(key, value);
    }

    /// Drop every memoized canonicalization. Used when a classpath root is
    /// retracted, since a cached hit may name a file the classpath can no
    /// longer serve.
    fn clear(&mut self) {
        self.paths.clear();
        self.order.clear();
    }
}

/// Does `entry` come from the filesystem root `root`?
///
/// Every variant carries the path it was built from: the archive/directory
/// itself for the flat forms, and the OUTER jar for the two nested forms — which
/// is the right answer for retraction, because one `add_path` of a fat JAR
/// appends one entry per nested archive inside it.
fn entry_derives_from(entry: &ClassPathEntry, root: &Path) -> bool {
    let source = match entry {
        ClassPathEntry::Directory(path)
        | ClassPathEntry::JarFile { path, .. }
        | ClassPathEntry::JmodFile { path, .. }
        | ClassPathEntry::JImageFile { path, .. } => path,
        ClassPathEntry::NestedDirectory { parent_jar, .. }
        | ClassPathEntry::NestedJar { parent_jar, .. } => parent_jar,
    };
    if source == root {
        return true;
    }
    // A spec and the recorded entry can spell the same file differently
    // (relative vs absolute, `/` vs `\`), so fall back to a canonical compare.
    match (source.canonicalize(), root.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Represents the classpath used to find `.class` files.
///
/// Supports directory entries, flat JAR entries, and Spring Boot fat JAR
/// entries (nested JARs inside `BOOT-INF/lib/` and classes from
/// `BOOT-INF/classes/`).
pub struct ClassPath {
    entries: Vec<ClassPathEntry>,
    /// Round 5 audit fix (MED): per-`ClassPath` cache of
    /// [`fs::canonicalize`] results. Each `find_class` probe over a
    /// `Directory` entry previously canonicalized BOTH the classpath
    /// root and the resolved file on every call — that's two syscalls
    /// per probe even when neither path has changed since the last
    /// lookup. The cache is consulted before falling through to the
    /// `fs` call; populated lazily on first miss. Use `Mutex` (not
    /// `RwLock`) because the populate path only writes; reads of an
    /// already-cached entry are sub-ms even under contention.
    ///
    /// Round 7 audit fix (HIGH #4): capped at
    /// `CANONICALIZE_CACHE_CAP` with FIFO eviction (see
    /// [`CanonicalizeCache`]). Long-running processes that probe many
    /// distinct paths can no longer grow this map without bound.
    canonicalize_cache: Mutex<CanonicalizeCache>,
    /// Memoized `fs::canonicalize` for **classpath roots** only — see
    /// [`ClassPath::canonicalize_root`] for why these cannot share the
    /// bounded FIFO above. Unbounded by construction, because its key set is
    /// exactly the entry list: one insert per `Directory`/`JarFile` root the
    /// scan has touched, never a caller-supplied probe path.
    root_canonical_cache: Mutex<HashMap<PathBuf, PathBuf>>,
    /// Specs handed to [`ClassPath::add_path`], with a use count.
    ///
    /// `URLClassLoader.close()` must stop a closed loader serving new classes,
    /// which means retracting the roots that loader added — and until this
    /// existed there was no way to name them, so `close()` could only ever be a
    /// no-op. The count is what makes retraction safe when two loaders were
    /// handed the same JAR: the last one to close is the one that removes it.
    dynamic_specs: HashMap<String, u32>,
    /// `entries.len()` immediately before the FIRST [`ClassPath::add_path`].
    ///
    /// Retraction only ever considers entries at or past this index, so a JAR
    /// that is both a startup classpath root and a dynamically added one keeps
    /// its startup entry when the dynamic loader closes.
    static_len: Option<usize>,
    /// Relative paths present under each `Directory` entry, so a MISS on that
    /// entry costs a hash probe instead of a `File::exists` syscall.
    ///
    /// # Why a directory needed one when a JAR already had one
    ///
    /// A `JarFile` entry carries `entry_index` for exactly this reason — "hot
    /// class/resource lookup reject[s] misses without taking the ZipArchive
    /// lock". `Directory` had no equivalent, so [`ClassPath::find_class`] did
    /// a real `dir.join(rel).exists()` for every directory entry on every
    /// lookup. That is one syscall per directory per class, and a class is
    /// found in at most one of them, so the whole cost is misses.
    ///
    /// Measured on the netty suite's classpath (308 entries: 197 JARs, **111
    /// directories** — one `target/classes` and one `target/test-classes` per
    /// reactor module) loading netty's slf4j/logback first-touch, 1102 class
    /// definitions, Windows host:
    ///
    /// | classpath | first `DefaultThreadFactory.newThread` |
    /// |---|---|
    /// | all 308 entries | 2195 ms |
    /// | the 4 entries actually needed | 188 ms |
    ///
    /// 111 x 1102 = 122k stats at ~15-20 us each is the whole difference.
    /// Linux hides it — the same probe costs 483 ms there, faster than HotSpot
    /// — because a `stat` on a warm dentry cache is ~1 us. It is the same
    /// shape as the `JarFile` accessor defect written up in
    /// `a_stat_per_accessor_call_hid_behind_an_o1_cache`, one layer out.
    ///
    /// # Staleness
    ///
    /// The index is built once per directory, on first use, and never
    /// invalidated — so a class written into a classpath directory *after*
    /// that point is absent from it. That is why an index MISS is not the end
    /// of the lookup: `find_class` falls back to the real `exists()` probe for
    /// directory entries when no entry claimed the class, which is precisely
    /// the pre-index behaviour. Anything the old code could find, this finds;
    /// the index only removes syscalls from the path that succeeds.
    dir_index: Mutex<HashMap<PathBuf, Option<Arc<FxHashSet<Box<str>>>>>>,
    /// Whether this class path emulates web and Spring Boot archives: a fat
    /// JAR's `BOOT-INF/classes/` and `BOOT-INF/lib/*.jar` become entries of
    /// their own (see [`ClassPath::extract_fat_jar_entries`]), and a class
    /// lookup in any archive also tries `WEB-INF/classes/` and
    /// `BOOT-INF/classes/`.
    ///
    /// HotSpot does neither. On its class path a fat JAR is its root, and the
    /// application inside it is Spring Boot's `LaunchedClassLoader`'s, reached
    /// through `jar:nested:` URLs and Spring's own handler. `--jdk-only`
    /// application class paths turn this off
    /// ([`ClassPath::with_fat_jar_emulation`]); every other `ClassPath` keeps
    /// it. See
    /// `fatjar-classes-load-from-the-vms-flattened-class-path-not-launchedclassloader-FIXED-20260924.md`.
    fat_jar_emulation: bool,
    /// SBR5 `clload`: the per-class-path facts [`ClassPath::fast_define_ok`] answers from,
    /// built once on first use (a class path whose entry list is still the startup one).
    fast_define_gate: OnceLock<FastDefineGate>,
    /// [`ClassPath::jar_entry_bytes_matching`]'s memo of which entry (index
    /// into `entries`) is the `JarFile` for an asked path (`None`: no entry
    /// is), so the canonicalisation of a path spelled differently from its
    /// class path element runs once per path, not once per class
    /// (interpreter round i3 wave 71, lane L5,
    /// `CRATONVM_JAR_LOADER_CLASS_PATH_DIRECT`). Cleared whenever the entry
    /// list changes ([`ClassPath::add_path`], [`ClassPath::remove_path`]),
    /// so a remembered index always names the entry it was found as; bounded
    /// by [`JAR_ENTRY_SLOT_CACHE_CAP`] (the keys are callers' paths).
    jar_entry_slots: Mutex<HashMap<PathBuf, Option<usize>>>,
    /// zs s4/rp, `CRATONVM_BOOT_IMAGE_CLASS_BYTES` (default ON, `=0` kill):
    /// whether a class lookup that a JDK's own `jmods/<module>.jmod` can serve
    /// answers the bytes of the same JDK's runtime image instead, and whether a
    /// lookup nothing on the path serves falls back to that image. See
    /// [`RuntimeImageClasses`].
    runtime_image_on: bool,
    /// The runtime image beside the first JDK `jmods/` directory on this path,
    /// opened on the first lookup that needs it (`None`: there is none, or it
    /// cannot be mapped). Reset whenever the entry list changes.
    runtime_image: OnceLock<Option<RuntimeImageClasses>>,
    /// [`ClassPath::for_url_loader`] only (empty otherwise): each archive
    /// (or directory) the loader's paths name, and each jar their manifests'
    /// `Class-Path:` added, with the file's stamp from BEFORE it was read
    /// (`None`: no such regular file then). Read by
    /// [`ClassPath::url_loader_archives_unchanged`]. Interpreter round i4
    /// wave 97, lane L5.
    url_loader_stamps: Vec<(PathBuf, Option<OuterArchiveStamp>)>,
    /// The highest `META-INF/versions/<N>/` a multi-release jar serves from
    /// (`JarFile.runtimeVersion()`): the BOOTED class library's feature version,
    /// set per VM by [`Self::set_multi_release_feature`] (`vm_init` from the
    /// image's `release` file; the `URLClassLoader` class paths from the
    /// native registry's `jdk_feature`). [`JVM_FEATURE_VERSION`] until set, and
    /// never above it. Interpreter round i5 wave 100, lane L5
    /// (`i100-L5-multi-release-jars-read-at-version-25-whatever-jdk-boots`).
    multi_release_feature: std::sync::atomic::AtomicU32,
    /// [`Self::set_multi_release_feature`] ran at least once (wave 103: lets a
    /// cached class path skip re-deriving the value on every ask).
    multi_release_feature_set: std::sync::atomic::AtomicBool,
    /// The `Error("Circular loading of URL stream handler providers
    /// detected")`s JDK 25's lazy application `URLClassPath` throws while it
    /// opens this class path, not yet raised: set by
    /// [`Self::apply_url_class_path_model`], taken one at a time by
    /// [`Self::take_provider_circularity_error`] (the `--compatible`
    /// application loader's miss raises it). Interpreter round i5 wave 108,
    /// lane L5 (probe `tools/probes/interp/L5/L5W107ClassPathDriveToken`).
    provider_circularity_errors: std::sync::atomic::AtomicUsize,
    /// [`Self::apply_url_class_path_model`] found a manifest `Class-Path`
    /// token whose `new URL(base, token)` runs `URL.lookupViaProviders`
    /// (a scheme other than `file`, `jar` and `jrt` with no cached handler),
    /// so the JDK's lazy expansion of this class path depends on the order
    /// in which lookups open it. Wave 108, lane L5.
    provider_lookup_class_path: bool,
}

/// zs s4/rp: the JDK runtime image (`<java.home>/lib/modules`) beside the
/// `<java.home>/jmods/` directory a boot class path was built from.
///
/// HotSpot never reads a JMOD: it runs the classes of `lib/modules`, which
/// `jlink` built from the JMODs and then changed. On JDK 25 the image's
/// `java.base` differs from `java.base.jmod` in exactly the jlink plugins'
/// output (measured, 25.0.3: 7358 classes byte-identical, 5 differ, 16 exist
/// only in the image):
///
/// - `GenerateJLIClassesPlugin` regenerates `DirectMethodHandle$Holder`,
///   `DelegatingMethodHandle$Holder`, `Invokers$Holder` and
///   `LambdaForm$Holder` as top-level classes full of pregenerated
///   `LambdaForm` methods (the JMOD's are empty member classes, so
///   `getEnclosingClass`, `getSimpleName`, `getNestHost` and
///   `getDeclaredClasses` of their outers answered differently), and adds the
///   `BoundMethodHandle$Species_*` classes (`Class.forName` threw
///   `ClassNotFoundException` for them);
/// - `SystemModulesPlugin` rewrites `SystemModulesMap` and adds
///   `SystemModules$*`.
///
/// So a boot class the image holds is served from the image, and a class only
/// the image holds is served at all. The boot class path itself is unchanged
/// (the JMODs stay its entries, for the reasons on `discover_boot_classpath`):
/// module descriptors (`module-info`) and every resource are still the JMODs'.
struct RuntimeImageClasses {
    /// The `jmods` directory whose JMODs the image stands for.
    jmods_dir: PathBuf,
    reader: cratonvm_reader::JImageReader,
    /// Package (internal form) -> module, from the class names of the JMODs in
    /// `jmods_dir`, built on the first lookup no entry served: which module of
    /// the image to ask for a class only the image holds.
    package_modules: OnceLock<FxHashMap<Box<str>, Box<str>>>,
}

/// The most paths [`ClassPath::jar_entry_bytes_matching`] remembers; the
/// memo is cleared when it is full.
const JAR_ENTRY_SLOT_CACHE_CAP: usize = 4096;

/// Which class-path classes the application loader may define natively (the per-class
/// work of `BuiltinClassLoader.findClassOnClassPathOrNull`) without the JDK's
/// `defineOrCheckPackage` / `Resource` / signer steps being able to observe a difference.
/// See [`ClassPath::fast_define_ok`].
struct FastDefineGate {
    /// Some entry makes the JDK's own lookup order or semantics differ from this class path's
    /// (a manifest `Class-Path`, a nested / jmod / jimage entry): nothing is fast.
    disabled: bool,
    /// Package (internal form, `""` unnamed) -> whether EVERY entry holding classes of it is `entry_ok`.
    /// A package split over several entries is fine when none of them is sealed, signed or carries
    /// per-package manifest sections: the JDK's per-class check then never throws and the Package is
    /// the first-loaded class's jar's in both paths.
    packages: rustc_hash::FxHashMap<Box<str>, bool>,
    /// Every class name (internal form, a `META-INF/versions/<n>/` prefix stripped for an `n`
    /// in `9..=`[`ClassPath::multi_release_feature`] only) any jar or directory entry held
    /// when the gate was built.
    names: rustc_hash::FxHashSet<Box<str>>,
    /// The `Directory` roots, re-checked on disk at lookup time (a directory can gain files).
    dirs: Vec<PathBuf>,
    /// Per entry: an unsigned jar whose manifest has no `Sealed` attribute anywhere (per-package sections
    /// are fine: the VM's package definition reads them, `Sbr5ClloadSection`), or a directory.
    entry_ok: Vec<bool>,
}

/// The URL schemes JDK 25's `java.base` has a stream handler for
/// (`sun.net.www.protocol.<scheme>.Handler`): a manifest `Class-Path` token
/// with any other scheme makes `new URL(base, token)` throw (see
/// [`ManifestInfo::class_path_drops_jar`]).
const URL_HANDLER_PROTOCOLS: [&str; 8] = ["file", "ftp", "http", "https", "jar", "jmod", "jrt", "mailto"];

/// The scheme `java.net.URL(URL context, String spec)` reads from a manifest
/// `Class-Path` token, lower-cased, or `None` for a relative token: the
/// characters before the first `:` that comes before any `/` (after an
/// optional `url:` prefix, not for a `#` reference), when they form a valid
/// protocol name (a letter, then letters, digits, `.`, `+`, `-`). `C:/x.jar`
/// and `C:\x.jar` are the scheme `c`; `1:x.jar` and `lib/a:b.jar` have none.
/// Interpreter round i5 wave 107, lane L5.
fn manifest_token_protocol(token: &str) -> Option<String> {
    let mut spec = token.trim_matches(|c: char| c <= ' ');
    if spec.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("url:")) {
        spec = &spec[4..];
    }
    if spec.starts_with('#') {
        return None;
    }
    let colon = spec.find([':', '/'])?;
    if spec.as_bytes()[colon] != b':' {
        return None;
    }
    let scheme = &spec[..colon];
    let mut chars = scheme.chars();
    let first = chars.next()?;
    if !first.is_alphabetic()
        || !chars.all(|c| c.is_alphanumeric() || c == '.' || c == '+' || c == '-')
    {
        return None;
    }
    Some(scheme.to_lowercase())
}

/// One manifest `Class-Path` token ([`ManifestInfo::class_path_tokens`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ClassPathToken {
    /// A relative or `file:` token: the path it names (a `dir/*` spelling is
    /// expanded as the class path's own expansion does).
    File(String),
    /// A token with another scheme, lower-cased (`c` for `C:/x.jar`).
    Scheme(String),
}

/// What [`simulate_url_class_path`] found.
#[derive(Debug, Default, PartialEq, Eq)]
struct UrlClassPathModel {
    /// The loaders JDK 25's `URLClassPath` ends with, in its order.
    order: Vec<PathBuf>,
    /// How many `Error("Circular loading of URL stream handler providers
    /// detected")`s opening it throws.
    circularity_errors: usize,
    /// How many `URL.lookupViaProviders` calls opening it makes.
    provider_lookups: usize,
}

/// JDK 25's lazy expansion of the APPLICATION `URLClassPath` (`ClassLoaders`'
/// `new URLClassPath(java.class.path)`), run to the end. Interpreter round i5
/// wave 108, lane L5 (probe `tools/probes/interp/L5/L5W107ClassPathDriveToken`).
///
/// `URLClassPath.getLoader(int)` polls the unopened URLs in order, skips one
/// that already has a loader (`lmap`), opens the jar and reads its manifest
/// `Class-Path` (`JarLoader.parseClassPath`, every token through
/// `new URL(base, token)`), pushes the tokens' URLs IN FRONT of the unopened
/// ones and only then adds the loader. A token whose scheme is not `file`,
/// `jar` or `jrt` and whose handler is not cached yet makes
/// `URL.getURLStreamHandler` call `URL.lookupViaProviders`, a
/// `ServiceLoader` of `URLStreamHandlerProvider` over the SYSTEM loader,
/// whose `getResources` walks this same `URLClassPath` (the monitor is
/// re-entrant), so the nested walk opens every remaining unopened URL:
///
/// * a jar opened inside that nested walk that itself needs a provider
///   lookup finds the thread already looking up (`ThreadTracker`) and throws
///   `Error("Circular loading of URL stream handler providers detected")`;
///   it is not an `IOException`, so it escapes the nested `getLoader`, the
///   outer jar's `parseClassPath`, and the lookup that started it: both jars
///   are lost (polled, never added) and so are their `Class-Path` URLs;
/// * otherwise the nested walk adds every remaining loader, and the outer
///   jar is then dropped (an unknown scheme: `MalformedURLException:
///   unknown protocol`, swallowed by `getLoader`) or kept AFTER all of them
///   (a known scheme, `http:`: its handler is now cached and
///   `tryResolveFile` ignores the token), followed by its own `Class-Path`.
///
/// Approximations, stated: no `URLStreamHandlerProvider` on the class path
/// and no `java.protocol.handler.pkgs`, no handler cached before the
/// expansion except `file`, `jar` and `jrt`, a directory is a loader whatever
/// its URL spelling, and the eager walk's order of lookups does not matter
/// (the result is the state after the class path is fully open).
/// `tokens_of` answers a path's tokens (empty for a directory), or `None`
/// when the JDK could not open it (`getLoader` skips the URL).
fn simulate_url_class_path(
    top: &[PathBuf],
    tokens_of: &mut dyn FnMut(&Path) -> Option<Vec<ClassPathToken>>,
) -> UrlClassPathModel {
    /// The circularity `Error`, unwinding a nested walk.
    struct Circular;
    struct Walk<'a> {
        tokens_of: &'a mut dyn FnMut(&Path) -> Option<Vec<ClassPathToken>>,
        unopened: VecDeque<PathBuf>,
        opened: FxHashSet<PathBuf>,
        cached: FxHashSet<String>,
        model: UrlClassPathModel,
    }
    impl Walk<'_> {
        /// `getLoader(url)` + `loader.getClassPath()`: `Ok(Some(urls))` adds
        /// the loader, `Ok(None)` is the swallowed `IOException`.
        fn open(&mut self, path: &Path, nested: bool) -> Result<Option<Vec<PathBuf>>, Circular> {
            let Some(tokens) = (self.tokens_of)(path) else {
                return Ok(None);
            };
            let mut urls = Vec::new();
            for token in tokens {
                match token {
                    ClassPathToken::File(spec) => urls.extend(
                        ClassPath::expand_classpath_wildcard(&spec)
                            .into_iter()
                            .map(PathBuf::from),
                    ),
                    ClassPathToken::Scheme(scheme) => {
                        // `jrt` is not overrideable; `jar` skips the providers.
                        if scheme == "jar" || scheme == "jrt" || self.cached.contains(&scheme) {
                            continue;
                        }
                        if nested {
                            return Err(Circular);
                        }
                        self.model.provider_lookups += 1;
                        self.drain()?;
                        if !URL_HANDLER_PROTOCOLS.contains(&scheme.as_str()) {
                            return Ok(None);
                        }
                        self.cached.insert(scheme);
                    }
                }
            }
            Ok(Some(urls))
        }

        fn add(&mut self, path: PathBuf, urls: Vec<PathBuf>) {
            for url in urls.into_iter().rev() {
                self.unopened.push_front(url);
            }
            self.opened.insert(path.clone());
            self.model.order.push(path);
        }

        /// The provider lookup's nested walk over what is left unopened.
        fn drain(&mut self) -> Result<(), Circular> {
            while let Some(path) = self.unopened.pop_front() {
                if self.opened.contains(&path) {
                    continue;
                }
                if let Some(urls) = self.open(&path, true)? {
                    self.add(path, urls);
                }
            }
            Ok(())
        }
    }
    let mut walk = Walk {
        tokens_of,
        unopened: top.iter().cloned().collect(),
        opened: FxHashSet::default(),
        cached: FxHashSet::default(),
        model: UrlClassPathModel::default(),
    };
    while let Some(path) = walk.unopened.pop_front() {
        if walk.opened.contains(&path) {
            continue;
        }
        match walk.open(&path, false) {
            Ok(Some(urls)) => walk.add(path, urls),
            Ok(None) => {}
            Err(Circular) => walk.model.circularity_errors += 1,
        }
    }
    walk.model
}

/// SBR5 `clload`: is the main-section `Class-Path` of this manifest text (CR already removed) absent or made
/// only of tokens both the JDK's `URLClassPath` and this class path expand the same way: a plain relative
/// path, or an absolute `file:/...` URL (not `file://host`), with no percent-escape, backslash, `..`, query or
/// fragment and no other scheme or drive spelling? The JDK pushes a jar's `Class-Path` URLs in front of its
/// unopened ones (depth first, duplicates skipped by URL, missing ones ignored); `ClassPath::load_jar_data_at_depth`
/// expands depth first right after the jar, skipping a source already present, so for these tokens the entry
/// order is the JDK's. Anything else turns the fast path off.
fn manifest_class_path_is_simple(text: &str) -> bool {
    fn simple_token(t: &str) -> bool {
        if t.contains('%')
            || t.contains('\\')
            || t.contains('?')
            || t.contains('#')
            || t.contains("..")
        {
            return false;
        }
        match t.find(':') {
            None => true,
            Some(_) => t.starts_with("file:/") && !t.starts_with("file://"),
        }
    }
    let mut value: Option<String> = None;
    let mut cur: Option<String> = None;
    let mut flush = |cur: Option<String>, value: &mut Option<String>| {
        if let Some(c) = cur {
            if c.len() >= 11 && c[..11].eq_ignore_ascii_case("class-path:") {
                *value = Some(c[11..].to_string());
            }
        }
    };
    for line in text.split('\n') {
        if line.is_empty() {
            break;
        }
        if let Some(rest) = line.strip_prefix(' ') {
            if let Some(c) = cur.as_mut() {
                c.push_str(rest);
            }
            continue;
        }
        flush(cur.take(), &mut value);
        cur = Some(line.to_string());
    }
    flush(cur.take(), &mut value);
    match value {
        None => true,
        Some(v) => v.split_whitespace().all(simple_token),
    }
}

/// Above this many files, a directory is left UNINDEXED and every lookup
/// stats it, as before. A classpath root with a million files under it is not
/// a normal `target/classes`, and building a set that size on the first class
/// load would trade a latency spike for the throughput win.
const DIR_INDEX_MAX_ENTRIES: usize = 200_000;

/// Companion bound to [`DIR_INDEX_MAX_ENTRIES`] on the other axis. The walk
/// follows symlinks, so a directory cycle is reachable and the file cap alone
/// would not stop it — a cycle of empty directories adds no files.
const DIR_INDEX_MAX_DIRS: usize = 50_000;

/// Per-archive memoized signing state for a signed JAR.
///
/// Security fix (V3, unsigned-entry attack — JAR spec §"Signature
/// Validation"): the archive-level signer *chain* alone is NOT a
/// licence to stamp every `.class` in the JAR with the signer's
/// certificates. The JAR spec only treats an entry as signed when the
/// **manifest commits to it** with a per-entry `<alg>-Digest` that
/// matches the entry's bytes. An entry that is present in the archive
/// but absent from the manifest (or present without a digest) is
/// **unsigned** — even inside a signed JAR — and an injected/swapped
/// `.class` must NOT inherit the signer's identity.
///
/// We therefore memoize, alongside the verified `chain`, the set of
/// entry names the manifest committed to (with the digest algorithm and
/// expected value the signer authenticated). `find_class_code_source_info`
/// consults `signed_entries` and re-hashes the specific class bytes
/// before attaching `chain`; a class that is not in this map, or whose
/// bytes don't match, is reported with an empty cert list (unsigned),
/// matching HotSpot's per-entry `CodeSigner` semantics.
struct JarSignerInfo {
    /// Verified archive-level signer cert chain (leaf first), the union
    /// of every signer block that passed full verification. Empty for an
    /// unsigned JAR or one that failed verification.
    chain: Vec<Vec<u8>>,
    /// Entry name (JAR-internal path, e.g. `com/example/Foo.class`) →
    /// the `(algorithm, expected-digest)` the manifest committed to for
    /// that entry, restricted to manifests bound to a verified signer.
    /// Only entries appearing here are eligible to inherit `chain`.
    signed_entries: HashMap<String, (crate::jar_signer::DigestAlg, Vec<u8>)>,
}

enum ClassPathEntry {
    Directory(PathBuf),
    /// A JAR file read into memory. The `Mutex` provides interior mutability
    /// since `ZipArchive::by_name` requires `&mut self`, and future-proofs
    /// for multi-threaded access (Phase 5).
    JarFile {
        path: PathBuf,
        archive: JarArchive,
        backing: ArchiveBacking,
        /// Exact central-directory entry names in this archive. This lets hot
        /// class/resource lookup reject misses without taking the ZipArchive
        /// lock or asking zip::ZipArchive::by_name to hash/probe its index.
        entry_index: FxHashSet<String>,
        /// True if the JAR declares `Multi-Release: true` in its manifest (JEP 238).
        multi_release: bool,
        /// Audit-fix #7: cached set of `META-INF/versions/<N>/` directory
        /// numbers present in this archive. Built lazily on first
        /// multi-release lookup so subsequent lookups skip
        /// `by_name` probes for absent versions. `None` means the
        /// cache hasn't been built yet.
        ///
        /// PERF: wrapped in `Arc` so `ensure_versions_cache` can hand
        /// callers a cheap reference-count bump instead of deep-cloning
        /// the whole `BTreeSet` on every multi-release class lookup (the
        /// previous `cache.clone()` heap-allocated a fresh tree per
        /// lookup). The set is immutable once built, so sharing it is
        /// behavior-preserving.
        versions_cache: Mutex<Option<Arc<BTreeSet<u32>>>>,
        /// Report P1 (perf): memoized verified signer cert chain (leaf+chain
        /// DER) for this archive. `extract_jar_signer_blocks` is a pure
        /// function of the archive — the `CodeSource` certificates are
        /// identical for every class in the JAR — but it was re-run on every
        /// class lookup: full central-directory rescan, re-read of every
        /// `*.RSA`/`*.SF`, PKCS#7 parse, RSA/ECDSA/DSA verify, trust-chain
        /// walk, and (post V1 fix) a MANIFEST.MF + per-entry digest re-hash.
        /// Populated once on the first signed lookup and cloned thereafter;
        /// an empty chain (unsigned, or failed verification) is also cached so
        /// the rescan is skipped for unsigned JARs too. Security is unchanged
        /// — the full verification still runs, exactly once.
        ///
        /// V3: now also carries the manifest-committed `signed_entries` map so
        /// the signer chain is attached **per class** (only to entries the
        /// signer committed to), not blanket to every class in the archive.
        signer_cache: OnceLock<JarSignerInfo>,
        /// The archive file's length and modification time when it was
        /// loaded (read when the entry is built, kept only when the length
        /// equals the backing's), or `None` (no such file, or another
        /// length). Interpreter round i2 wave 69, lane L5: what
        /// [`ClassPath::outer_archive_for`] compares before it lends this
        /// entry's parse to the nested-jar indexes.
        stamp: Option<OuterArchiveStamp>,
        /// Does this archive have the jar-loader rows' PLAIN view (no
        /// `META-INF/versions/` entry, no signature file, no duplicated
        /// name; [`nested_archive_view_is_plain`])? Computed on the first
        /// [`ClassPath::jar_entry_bytes_matching`] (interpreter round i3
        /// wave 71, lane L5, `CRATONVM_JAR_LOADER_CLASS_PATH_DIRECT`), from
        /// the same index and directory a lent row would compute it from.
        plain_view: OnceLock<bool>,
    },
    /// A virtual directory inside a fat JAR (e.g. `BOOT-INF/classes/`).
    /// Entries are stored as a map from relative path to byte content.
    NestedDirectory {
        /// The outer JAR path (for debug/logging).
        parent_jar: PathBuf,
        /// Prefix inside the outer JAR (e.g. `BOOT-INF/classes/`).
        prefix: String,
        /// Cached entries: relative_path (without prefix) → bytes.
        entries_cache: HashMap<String, SharedBytes>,
    },
    /// A nested JAR extracted from inside a fat JAR (e.g. `BOOT-INF/lib/dep.jar`).
    NestedJar {
        /// The outer JAR path (for debug/logging).
        parent_jar: PathBuf,
        /// Path inside the outer JAR (e.g. `BOOT-INF/lib/spring-core-6.1.0.jar`).
        nested_path: String,
        /// The extracted nested archive.
        archive: Mutex<SharedArchive>,
        backing: ArchiveBacking,
        /// Exact central-directory entry names in this nested archive.
        entry_index: FxHashSet<String>,
        /// Report P1 (perf): memoized verified signer cert chain for this
        /// nested archive — see the matching field on `JarFile`. Populated
        /// once on the first signed lookup, cloned thereafter.
        ///
        /// V3: carries the manifest-committed `signed_entries` map for
        /// per-class cert attachment (see [`JarSignerInfo`]).
        signer_cache: OnceLock<JarSignerInfo>,
    },
    /// A JDK 9+ JMOD file (ZIP with 4-byte `JM\x01\x00` prefix).
    ///
    /// PERF (2026-07-26 boot-classpath-lazy): this variant used to carry a
    /// `classes_cache: HashMap<String, SharedBytes>` holding the **inflated
    /// bytes of every `classes/` entry**, built eagerly by `load_jmod`. On a
    /// stock JDK 25 `discover_boot_classpath` puts all 70 `.jmod` files on the
    /// boot classpath, so that cost 27,962 full deflate passes and ~136 MB of
    /// resident inflated bytes *before the first Java class loaded* — measured
    /// at 15-21 s even with optimised native zlib, and far worse in a debug
    /// build. See `boot-classpath-lazy.md`.
    ///
    /// The eager cache existed to stop existence probes paying a deflate each
    /// (`find_in_archive(..).is_some()` inflated the entry just to answer a
    /// yes/no). That is now solved the way the JAR path already solves it: a
    /// decompression-free name index built from the central directory
    /// (compare `JarFile::entry_index` / `build_archive_entry_index`), with
    /// bytes inflated only for entries actually requested. Existence checks
    /// are a hash-set probe; a real hit is one deflate of one entry.
    JmodFile {
        path: PathBuf,
        /// Names of every entry under `classes/`, with the `classes/` prefix
        /// stripped (e.g. `java/lang/Object.class`). Built once during
        /// `load_jmod` from the central directory — no entry is inflated to
        /// populate it. Membership here means "this JMOD can serve that
        /// name"; the bytes come from [`ClassPath::jmod_class_bytes`].
        class_entry_index: crate::jmod_index::JmodClassNames,
        /// The full set of entry names in the JMOD (including non-class entries)
        /// kept for `list_jmod_classes` and resource lookups.
        all_entry_names: crate::jmod_index::JmodAllNames,
        /// The archive, used for every byte-serving lookup (classes via
        /// [`ClassPath::jmod_class_bytes`], other entries directly): the
        /// central-directory index by default, a `ZipArchive` under
        /// `CRATONVM_JMOD_LAZY_INDEX=0` or for a JMOD that index does not model.
        archive: JmodArchive,
        backing: ArchiveBacking,
    },
    /// A JDK 9+ `lib/modules` jimage file (NEW-5): one binary blob holding
    /// every class and resource in the boot layer, accessed through the
    /// [`cratonvm_reader::JImageReader`] perfect-hash index.
    ///
    /// The lookup strategy:
    ///   1. `class_to_module`: pre-built index from internal class name
    ///      (e.g. `java/lang/String`) to its owning module (e.g. `java.base`),
    ///      by package since zs s6/rs ([`JImageClassIndex`]). Populated once
    ///      during load by iterating the jimage entries.
    ///   2. `find_class` consults this map and then calls
    ///      `JImageReader::find_class(module, name)` to fetch bytes via the
    ///      perfect-hash path — O(1) amortized.
    ///   3. `find_resource` tries every known module for a matching resource,
    ///      stopping on the first hit (matches JDK `ModuleReader` semantics).
    JImageFile {
        path: PathBuf,
        reader: cratonvm_reader::JImageReader,
        /// Internal class name (no `.class` suffix) to its owning module
        /// name. Built once during load by walking every entry; used by
        /// `find_class` for O(1) module resolution. Per package by default,
        /// per class under `CRATONVM_JIMAGE_PACKAGE_INDEX=0`
        /// ([`JImageClassIndex`]).
        class_to_module: JImageClassIndex,
        /// Map from resource path (with `.class` suffix for classes, or the
        /// raw resource path for non-class files) to the list of modules
        /// that contain it. Only non-class resources appear here — classes
        /// use `class_to_module` instead.
        resource_to_modules: FxHashMap<String, Vec<String>>,
        /// Every module name that contributes at least one entry. Used by
        /// `scan_module_infos` and by the module-layer builder.
        module_names: Vec<String>,
    },
}

/// zs s6/rs: a jimage's class -> module index
/// (`ClassPathEntry::JImageFile::class_to_module`).
///
/// A Hello world's boot walks all ~30 000 entries of `lib/modules`. The
/// per-class map kept ~27 000 `String` -> `String` pairs for the life of the
/// VM, about 5 MB with its `reserve(resource_count)` (65 536 buckets), every
/// page of it written. HotSpot keeps no such table: libjimage answers a class
/// by package (`JImagePackageToModule`) and a perfect-hash probe. The
/// [`Self::Packages`] arm does the same with the ~900 packages of the image.
///
/// Answers are the per-class map's:
/// - [`Self::candidate_module`] may name a module for a class the image does
///   not hold (its package is the image's). Every caller reads the class
///   through the image right after (`find_class`, `find_resource`), which
///   answers "absent" exactly where the map missed.
/// - [`Self::contains`] confirms with the image's own lookup
///   ([`cratonvm_reader::JImageReader::has_class`]: the same perfect hash and
///   full-path compare as a read).
/// - A package in two modules (no JDK image has one; jlink refuses it) makes
///   the load keep the per-class map for that image, so last-entry-wins
///   stays the rule there.
///
/// `module-info` is never a class of the index, in either arm.
#[derive(Debug)]
pub(crate) enum JImageClassIndex {
    /// The per-class map, as before: `CRATONVM_JIMAGE_PACKAGE_INDEX=0`, or an
    /// image with a split package.
    Classes(FxHashMap<String, String>),
    /// Package (`java/lang`; `""` for the unnamed package) -> index into
    /// `modules`.
    Packages {
        modules: Box<[Box<str>]>,
        packages: FxHashMap<Box<str>, u32>,
        /// Class entries the walk filed (`module-info` excluded).
        classes: usize,
    },
}

/// The package of an internal class name: everything before the last `/`.
fn jimage_package_of(class_name: &str) -> &str {
    class_name.rsplit_once('/').map_or("", |(p, _)| p)
}

impl JImageClassIndex {
    /// The module that would hold `class_name`, unconfirmed (see the type
    /// docs); `None` when no module can.
    pub(crate) fn candidate_module(&self, class_name: &str) -> Option<&str> {
        match self {
            Self::Classes(map) => map.get(class_name).map(String::as_str),
            Self::Packages {
                modules, packages, ..
            } => {
                // "`module-info` is never a class of the index, in either
                // arm": the walk files no `module-info` base in any package,
                // so neither may the package arm route one (p5-cp; the
                // per-class map never held `<pkg>/module-info`).
                if class_name == "module-info" || class_name.ends_with("/module-info") {
                    return None;
                }
                let id = *packages.get(jimage_package_of(class_name))?;
                modules.get(id as usize).map(|m| &**m)
            }
        }
    }

    /// Whether the image holds the class `class_name` (not `module-info`).
    pub(crate) fn contains(
        &self,
        reader: &cratonvm_reader::JImageReader,
        class_name: &str,
    ) -> bool {
        match self {
            Self::Classes(map) => map.contains_key(class_name),
            Self::Packages { .. } => self
                .candidate_module(class_name)
                .is_some_and(|m| matches!(reader.has_class(m, class_name), Ok(true))),
        }
    }

    /// How many classes the index holds.
    pub(crate) fn class_count(&self) -> usize {
        match self {
            Self::Classes(map) => map.len(),
            Self::Packages { classes, .. } => *classes,
        }
    }

    /// Populate package-to-module mappings from the jimage package index into the module registry.
    pub(crate) fn populate_registry_packages(&self, registry: &mut crate::module::ModuleRegistry) {
        if let Self::Packages { modules, packages, .. } = self {
            let arcs: Vec<std::sync::Arc<str>> = modules.iter().map(|m| std::sync::Arc::from(&**m)).collect();
            for (pkg, &mod_idx) in packages {
                if let Some(arc) = arcs.get(mod_idx as usize) {
                    registry.add_package_to_module_arc(pkg, arc.clone());
                }
            }
        }
    }

    /// Every class name the index holds (no `module-info`). The per-package
    /// arm walks the image again: a cold path (the application class path
    /// listing), not a boot one.
    pub(crate) fn class_names(&self, reader: &cratonvm_reader::JImageReader) -> Vec<String> {
        match self {
            Self::Classes(map) => map.keys().cloned().collect(),
            Self::Packages { .. } => {
                let mut out = Vec::new();
                let _ = reader.for_each_entry_parts(|module, parent, base, extension| {
                    if let Some((_, class_name)) =
                        jimage_class_entry(module, parent, base, extension)
                    {
                        out.push(class_name);
                    }
                });
                out
            }
        }
    }
}

/// `(module, class_name)` of a jimage entry that `load_jimage` files as a
/// class (`.class`, not `module-info`), from its path parts; `None` for
/// any other entry. The same split as `load_jimage`'s walk.
fn jimage_class_entry(
    module: &str,
    parent: &str,
    base: &str,
    extension: &str,
) -> Option<(String, String)> {
    let mut full = String::new();
    if !module.is_empty() {
        full.push('/');
        full.push_str(module);
    }
    if !parent.is_empty() {
        full.push('/');
        full.push_str(parent);
    }
    if !base.is_empty() {
        full.push('/');
        full.push_str(base);
    }
    if !extension.is_empty() {
        full.push('.');
        full.push_str(extension);
    }
    let without_lead = full.strip_prefix('/')?;
    let (m, r) = without_lead.split_once('/')?;
    if m.is_empty() || r.is_empty() {
        return None;
    }
    let class_name = r.strip_suffix(".class")?;
    // `load_jimage`'s walk files no `module-info` base of an entry with a
    // module, in any package (`base != "module-info"`); neither does this
    // split. A `<pkg>/module-info` here put a class in `class_names` that the
    // index never held (`zs6rs_jimage_package_index_answers_as_the_class_map`
    // failed on that before p5-cp).
    if (!module.is_empty() && base == "module-info") || class_name == "module-info" {
        return None;
    }
    Some((m.to_string(), class_name.to_string()))
}

/// Builds a [`JImageClassIndex`] during `load_jimage`'s walk.
pub(crate) struct JImageIndexBuilder {
    /// `Some` in the per-class arm (the kill switch).
    classes: Option<FxHashMap<String, String>>,
    modules: Vec<Box<str>>,
    /// zs s9/rs2b (rv6 X3): module name -> index into `modules`. The walk is
    /// in the image's hash-slot order, so consecutive entries rarely share a
    /// module, and a linear search per entry was ~65 compares.
    module_ids: FxHashMap<Box<str>, u32>,
    packages: FxHashMap<Box<str>, u32>,
    /// p5-cp: a package's string-table offset -> its entry in `packages`
    /// ([`Self::add_at`]), so a class of a package seen before is filed
    /// without reading or hashing the package name. Always agrees with
    /// `packages` (an entry there never changes once inserted).
    package_at: FxHashMap<u64, u32>,
    /// The last module's index, so a run of same-module entries looks it up
    /// once.
    last: Option<u32>,
    filed: usize,
    split: bool,
    /// Entries whose module was found through `module_ids` (neither the
    /// package's known module nor `last`): the walk statistic the real-image
    /// test prints.
    module_lookups: usize,
}

impl JImageIndexBuilder {
    /// `by_package` is `CRATONVM_JIMAGE_PACKAGE_INDEX`; `resource_count` is
    /// the per-class arm's reserve, as before.
    pub(crate) fn new(by_package: bool, resource_count: usize) -> Self {
        let classes = (!by_package).then(|| {
            let mut m = FxHashMap::default();
            m.reserve(resource_count);
            m
        });
        Self {
            classes,
            modules: Vec::new(),
            module_ids: FxHashMap::default(),
            packages: FxHashMap::default(),
            package_at: FxHashMap::default(),
            last: None,
            filed: 0,
            split: false,
            module_lookups: 0,
        }
    }

    /// File the class components `parent` (package) and `base` (simple name) of `module`.
    pub(crate) fn add_parts(&mut self, parent: &str, base: &str, module: &str) {
        if let Some(map) = self.classes.as_mut() {
            let mut class_name = String::with_capacity(parent.len() + base.len() + 1);
            if !parent.is_empty() {
                class_name.push_str(parent);
                class_name.push('/');
            }
            class_name.push_str(base);
            map.insert(class_name, module.to_string());
            return;
        }
        if self.split {
            return;
        }
        let package = parent;
        let known = self.packages.get(package).copied();
        let id = self.module_id(known, module);
        self.filed += 1;
        match known {
            Some(have) if have != id => self.split = true,
            Some(_) => {}
            None => {
                self.packages.insert(package.into(), id);
            }
        }
    }

    /// The index of `module` for a class whose package is `known` (its
    /// package's module so far, if any). The package's own module is the
    /// usual answer (one compare); then the previous entry's; then the name
    /// map.
    fn module_id(&mut self, known: Option<u32>, module: &str) -> u32 {
        match (known, self.last) {
            (Some(id), _) if &*self.modules[id as usize] == module => id,
            (_, Some(id)) if &*self.modules[id as usize] == module => id,
            _ => {
                self.module_lookups += 1;
                let id = match self.module_ids.get(module) {
                    Some(&i) => i,
                    None => {
                        let i = self.modules.len() as u32;
                        self.modules.push(module.into());
                        self.module_ids.insert(module.into(), i);
                        i
                    }
                };
                self.last = Some(id);
                id
            }
        }
    }

    /// [`Self::add_parts`] for the class whose package (parent) and simple
    /// name (base) are the strings at `parent_off` / `base_off` of `reader`
    /// (p5-cp, `load_jimage`'s walk): the per-package arm reads the package
    /// name only the first time its offset is seen. The same index as
    /// `add_parts(reader.string_at(parent_off)?, reader.string_at(base_off)?,
    /// module)`.
    pub(crate) fn add_at(
        &mut self,
        reader: &cratonvm_reader::JImageReader,
        parent_off: u64,
        base_off: u64,
        module: &str,
    ) -> Result<(), cratonvm_reader::JImageError> {
        if self.classes.is_some() {
            let parent = reader.string_at(parent_off)?;
            let base = reader.string_at(base_off)?;
            self.add_parts(parent, base, module);
            return Ok(());
        }
        if self.split {
            return Ok(());
        }
        let (known, package) = match self.package_at.get(&parent_off) {
            Some(&id) => (Some(id), None),
            None => {
                let package = reader.string_at(parent_off)?;
                (self.packages.get(package).copied(), Some(package))
            }
        };
        let id = self.module_id(known, module);
        self.filed += 1;
        match (known, package) {
            (Some(have), _) if have != id => self.split = true,
            (Some(have), Some(_)) => {
                self.package_at.insert(parent_off, have);
            }
            (Some(_), None) => {}
            (None, package) => {
                let package = package.unwrap_or_default();
                self.packages.insert(package.into(), id);
                self.package_at.insert(parent_off, id);
            }
        }
        Ok(())
    }

    /// File the class `class_name` (not `module-info`) of `module`.
    pub(crate) fn add(&mut self, class_name: &str, module: &str) {
        if let Some(map) = self.classes.as_mut() {
            map.insert(class_name.to_string(), module.to_string());
            return;
        }
        if self.split {
            return;
        }
        let package = jimage_package_of(class_name);
        let known = self.packages.get(package).copied();
        let id = self.module_id(known, module);
        self.filed += 1;
        match known {
            Some(have) if have != id => self.split = true,
            Some(_) => {}
            None => {
                self.packages.insert(package.into(), id);
            }
        }
    }

    /// The index. A split package rebuilds the per-class map with a second
    /// walk (never on a JDK image).
    pub(crate) fn finish(
        self,
        reader: &cratonvm_reader::JImageReader,
    ) -> Result<JImageClassIndex, cratonvm_reader::JImageError> {
        if let Some(map) = self.classes {
            return Ok(JImageClassIndex::Classes(map));
        }
        if self.split {
            let mut map: FxHashMap<String, String> = FxHashMap::default();
            reader.for_each_entry_parts(|module, parent, base, extension| {
                if let Some((m, c)) = jimage_class_entry(module, parent, base, extension) {
                    map.insert(c, m);
                }
            })?;
            return Ok(JImageClassIndex::Classes(map));
        }
        let mut packages = self.packages;
        packages.shrink_to_fit();
        Ok(JImageClassIndex::Packages {
            modules: self.modules.into_boxed_slice(),
            packages,
            classes: self.filed,
        })
    }
}

impl std::fmt::Debug for ClassPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let paths: Vec<String> = self
            .entries
            .iter()
            .map(|e| match e {
                ClassPathEntry::Directory(p) => format!("dir:{}", p.display()),
                ClassPathEntry::JarFile { path, .. } => format!("jar:{}", path.display()),
                ClassPathEntry::NestedDirectory {
                    parent_jar, prefix, ..
                } => format!("nested-dir:{}!/{}", parent_jar.display(), prefix),
                ClassPathEntry::NestedJar {
                    parent_jar,
                    nested_path,
                    ..
                } => format!("nested-jar:{}!/{}", parent_jar.display(), nested_path),
                ClassPathEntry::JmodFile { path, .. } => format!("jmod:{}", path.display()),
                ClassPathEntry::JImageFile { path, .. } => {
                    format!("jimage:{}", path.display())
                }
            })
            .collect();
        f.debug_struct("ClassPath")
            .field("entries", &paths)
            .finish()
    }
}

impl std::fmt::Debug for ClassPathEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClassPathEntry::Directory(p) => write!(f, "Directory({})", p.display()),
            ClassPathEntry::JarFile { path, .. } => write!(f, "JarFile({})", path.display()),
            ClassPathEntry::NestedDirectory {
                parent_jar, prefix, ..
            } => write!(f, "NestedDirectory({}!/{})", parent_jar.display(), prefix),
            ClassPathEntry::NestedJar {
                parent_jar,
                nested_path,
                ..
            } => write!(f, "NestedJar({}!/{})", parent_jar.display(), nested_path),
            ClassPathEntry::JmodFile { path, .. } => write!(f, "JmodFile({})", path.display()),
            ClassPathEntry::JImageFile { path, .. } => {
                write!(f, "JImageFile({})", path.display())
            }
        }
    }
}

/// Do two entries read from the same place?
///
/// This is the identity HotSpot's `URLClassPath` dedupes on: it refuses to
/// push a URL already on its path, so `java -cp dir;dir` answers
/// `getResources` with ONE URL, not two (measured, JDK 25).
fn same_classpath_source(a: &ClassPathEntry, b: &ClassPathEntry) -> bool {
    match (a, b) {
        (ClassPathEntry::Directory(x), ClassPathEntry::Directory(y)) => same_classpath_file(x, y),
        (ClassPathEntry::JarFile { path: x, .. }, ClassPathEntry::JarFile { path: y, .. })
        | (ClassPathEntry::JmodFile { path: x, .. }, ClassPathEntry::JmodFile { path: y, .. })
        | (
            ClassPathEntry::JImageFile { path: x, .. },
            ClassPathEntry::JImageFile { path: y, .. },
        ) => same_classpath_file(x, y),
        (
            ClassPathEntry::NestedDirectory {
                parent_jar: xj,
                prefix: xp,
                ..
            },
            ClassPathEntry::NestedDirectory {
                parent_jar: yj,
                prefix: yp,
                ..
            },
        ) => xj == yj && xp == yp,
        (
            ClassPathEntry::NestedJar {
                parent_jar: xj,
                nested_path: xn,
                ..
            },
            ClassPathEntry::NestedJar {
                parent_jar: yj,
                nested_path: yn,
                ..
            },
        ) => xj == yj && xn == yn,
        _ => false,
    }
}

/// Do two spellings of a class path element name one file? `URLClassPath(String
/// cp, ...)` maps every `java.class.path` element through
/// `ParseUtil.fileToEncodedURL(new File(s).getCanonicalFile())` and keeps one
/// loader per URL (`lmap`), so `a.jar`, `./a.jar`, `lib/../a.jar` and the
/// absolute spelling of one jar (or directory) are ONE element on HotSpot, in
/// both modes; `--compatible` kept each spelling as an entry (every resource
/// of the jar answered twice by `getResources`, a `ServiceLoader` file read
/// twice). `--jdk-only` absolutizes the elements before they reach here
/// (`vm_init::jdk_only_app_class_path_entry`). Equal spellings answer at once;
/// two whose last components differ (ASCII case aside on Windows) are two
/// files; the rest compare lexically, each made absolute against the working
/// directory (`std::path::absolute`, no disk access) with its `.` and `..`
/// segments folded ([`url_path_normalized`]), ASCII case aside on Windows.
/// Not compared: two spellings that meet only through a symbolic link
/// (`getCanonicalFile` resolves links; asking the file system for every pair
/// of same-named entries -- every module's `build/classes/java/main` -- would
/// cost a system call per pair at each class path build). Interpreter round
/// i5 wave 109, lane L5 (page
/// `i108-L5-compatible-class-path-element-spelled-twice-is-two-entries`, probe
/// `L5W109ClassPathSpelledTwice`).
fn same_classpath_file(x: &Path, y: &Path) -> bool {
    if x == y {
        return true;
    }
    if let (Some(xn), Some(yn)) = (x.file_name(), y.file_name()) {
        #[cfg(windows)]
        let same_name = xn.to_string_lossy().eq_ignore_ascii_case(&yn.to_string_lossy());
        #[cfg(not(windows))]
        let same_name = xn == yn;
        if !same_name {
            return false;
        }
    }
    let identity = |p: &Path| std::path::absolute(p).ok().map(url_path_normalized);
    match (identity(x), identity(y)) {
        #[cfg(windows)]
        (Some(a), Some(b)) => a
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy()),
        #[cfg(not(windows))]
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// A manifest `Class-Path` path as the URL JDK 25's `new URL(base, token)`
/// makes of it names it: `URLStreamHandler.parseURL` drops `.` segments and
/// folds every `name/..` pair, keeping a `..` with nothing left to fold, all
/// lexically (the file system is not asked, so a `sub/..` resolves even when
/// `sub` is missing or a link). `URLClassPath` then keys its loaders by that
/// URL (`lmap`), so `dep.jar`, `./dep.jar` and `sub/../dep.jar` are ONE
/// loader there; here they were two entries for one jar (duplicate
/// `getResources` URLs, spelled with the `..`) because entries compare by
/// path. Unchanged when the path has no `.` or `..` component. Interpreter
/// round i5 wave 108, lane L5 (probe `L5W108ClassPathDotSegments`).
fn url_path_normalized(path: PathBuf) -> PathBuf {
    use std::path::Component;
    // `components()` hides an inner `.` (it compares equal already), but the
    // spelling is what resource URLs and code sources are built from.
    if !path
        .as_os_str()
        .to_string_lossy()
        .split(['/', std::path::MAIN_SEPARATOR])
        .any(|segment| segment == "." || segment == "..")
    {
        return path;
    }
    let mut out = PathBuf::new();
    // Trailing `Normal` components a `..` may fold.
    let mut foldable = 0usize;
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir if foldable > 0 => {
                out.pop();
                foldable -= 1;
            }
            Component::ParentDir => out.push(".."),
            Component::Normal(name) => {
                out.push(name);
                foldable += 1;
            }
            other => {
                out.push(other.as_os_str());
                foldable = 0;
            }
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Append `entry` unless this `ClassPath` already reads from that source.
///
/// One physical source can reach a single `ClassPath` twice. The case that
/// mattered: `--jar <pathing-jar>` expands the launch jar's manifest
/// `Class-Path` in `vm-cli`, hands the result to `ClassPath::new`, and
/// `load_jar_data_at_depth` then expands that same manifest again when it
/// opens the launch jar. Jars survived it — that function returns early for an
/// archive already published — but directories were pushed unconditionally, so
/// every directory on a pathing jar's manifest was enumerated TWICE. Spring
/// Boot's `@WithPackageResources` copies each `getResources` hit into a fresh
/// temp root, so the second copy of the same directory failed with
/// `FileAlreadyExistsException` and took the whole SSL/PEM/JKS test cluster
/// with it.
fn push_classpath_entry(entries: &mut Vec<ClassPathEntry>, entry: ClassPathEntry) {
    if entries
        .iter()
        .any(|existing| same_classpath_source(existing, &entry))
    {
        return;
    }
    entries.push(entry);
}

/// Parsed contents of `META-INF/MANIFEST.MF` relevant to JAR loading.
#[derive(Debug, Default)]
pub struct ManifestInfo {
    /// `Main-Class` header (the launcher class).
    pub main_class: Option<String>,
    /// `Start-Class` header (Spring Boot: the actual application class).
    pub start_class: Option<String>,
    /// `Spring-Boot-Classes` header (default: `BOOT-INF/classes/`).
    pub boot_classes: Option<String>,
    /// `Spring-Boot-Lib` header (default: `BOOT-INF/lib/`).
    pub boot_lib: Option<String>,
    /// `Class-Path` header (space-separated relative paths to additional JARs).
    pub class_path: Option<String>,
    /// `Multi-Release: true` header (JEP 238, Java 9+).
    pub multi_release: bool,
    /// T19.H10: full attribute map from the manifest's main section.
    /// Keys are stored verbatim (case-preserving) so callers like
    /// `Class.getPackage().getImplementationVersion()` can pull arbitrary
    /// `Implementation-*` / `Specification-*` headers without losing
    /// whitespace or casing. Only the main section is captured —
    /// per-entry sections (rare; used by signed JARs) are not parsed.
    pub attributes: std::collections::HashMap<String, String>,
}

impl ManifestInfo {
    /// Fold the JAR manifest **main section** into logical lines per the JAR
    /// specification: any physical line beginning with a single leading space
    /// continues the previous line (the line break and continuation marker space
    /// are removed; the remainder is appended verbatim).
    ///
    /// This matters for long `Class-Path` headers (Surefire booter jars split
    /// across many 72-byte lines). The previous `replace("\\n ", "")` approach
    /// incorrectly deleted the continuation marker without appending the
    /// continuation text, corrupting classpath entries.
    fn fold_main_section_lines(text: &str) -> Vec<String> {
        let text = text.replace('\r', "");
        let mut folded: Vec<String> = Vec::new();
        let mut buf = String::new();
        for line in text.split('\n') {
            if line.is_empty() {
                if !buf.is_empty() {
                    folded.push(std::mem::take(&mut buf));
                }
                break;
            }
            if line.starts_with(' ') && !buf.is_empty() {
                buf.push_str(&line[1..]);
            } else {
                if !buf.is_empty() {
                    folded.push(std::mem::take(&mut buf));
                }
                buf.push_str(line);
            }
        }
        if !buf.is_empty() {
            folded.push(buf);
        }
        folded
    }

    /// Decode a single JAR-manifest `Class-Path:` token into a filesystem
    /// path.
    ///
    /// **SECURITY (VULN, low) — trusted-manifest filesystem reach.** This
    /// deliberately honours `file:` URLs, absolute paths, Windows
    /// drive-absolute paths, and `..` escapes, joining only genuinely
    /// relative tokens to `jar_dir`. That means a JAR's manifest can add
    /// *arbitrary* filesystem roots as classpath entries — e.g.
    /// `Class-Path: file:/etc` or `Class-Path: ../../secret` — exactly as
    /// HotSpot's `URLClassPath` does. The manifest is part of the trusted
    /// application bundle (it ships inside the JAR the user chose to run),
    /// so this is the spec-mandated behaviour and is REQUIRED by real
    /// launchers (Surefire booter JARs emit absolute `file:/C:/...` tokens;
    /// fat JARs point at sibling `lib/` dirs). We must NOT break it by
    /// default.
    ///
    /// Defence in depth: every path produced here is still subject to the
    /// downstream canonicalize/`is_safe_*` checks at *read* time, so a
    /// decoded `..` cannot escape the resolved root of whatever entry it
    /// becomes — this function only chooses the roots, it does not grant
    /// raw file access.
    ///
    /// For environments that want to forbid out-of-bundle reach, the
    /// caller [`ManifestInfo::resolve_class_path`] gates roots behind
    /// [`harden_manifest_classpath`] (`CRATONVM_HARDEN_MANIFEST_CLASSPATH`),
    /// restricting resolved roots to descendants of `jar_dir`. Decoding
    /// itself is left permissive so the caller can make that policy
    /// decision against the canonical form.
    fn decode_manifest_classpath_entry(entry: &str, jar_dir: &Path) -> PathBuf {
        let raw = entry.trim();
        // JAR manifests may carry file URLs (e.g. surefire booter jars emit
        // `file:/C:/...` entries). Treat those as absolute paths instead of
        // joining them to the launcher jar directory.
        // `file:` URI paths are percent-decoded by `decode_percent_path`
        // (a full RFC 3986 `%XX` decoder), replacing the prior no-op
        // `replace('%', "%")` + hardcoded `%20/%5B/%5D/%7B/%7D` subset.
        // Downstream reads are canonicalize-checked, so a decoded
        // `..`/separator cannot escape the intended root.
        let from_file_uri = |rest: &str| -> PathBuf {
            let mut out = decode_percent_path(rest);
            if let Some(stripped) = out.strip_prefix("///") {
                out = stripped.to_string();
            } else if let Some(stripped) = out.strip_prefix("//") {
                // file://<host>/... . For local drive paths this is typically
                // file:///C:/...; for malformed host forms preserve the slash.
                out = stripped.to_string();
            } else if let Some(stripped) = out.strip_prefix('/') {
                // file:/C:/... (single slash before drive letter) -> C:/...
                if stripped.as_bytes().get(1) == Some(&b':') {
                    out = stripped.to_string();
                }
            }
            url_path_normalized(PathBuf::from(out))
        };

        if let Some(rest) = raw.strip_prefix("file:") {
            return from_file_uri(rest);
        }

        if raw.len() >= 2 && raw.as_bytes()[1] == b':' {
            // Windows drive-absolute path like C:/foo or C:\foo.
            return PathBuf::from(raw);
        }
        // Every other token is a URL relative to the jar's own (JDK 25
        // `URLClassPath.JarLoader.parseClassPath` -> `new URL(base, path)`),
        // and the jar it names is opened through `ParseUtil.decode` of the
        // URL's path (`FileURLMapper`), so `lib%20dir/dep.jar` names the file
        // `lib dir/dep.jar` there, absolute or relative. These were joined
        // verbatim, so such a jar was never found (interpreter round i5 wave
        // 106, lane L5; probe `L5W106ClassPathPercentEncoded`).
        let decoded = if raw.contains('%') {
            decode_percent_path(raw)
        } else {
            raw.to_string()
        };
        let p = Path::new(&decoded);
        if p.is_absolute() {
            return url_path_normalized(p.to_path_buf());
        }
        url_path_normalized(jar_dir.join(decoded))
    }

    /// Parse a `MANIFEST.MF` file's bytes.
    pub fn parse(data: &[u8]) -> Self {
        let text = String::from_utf8_lossy(data);
        let mut info = ManifestInfo::default();
        // MANIFEST.MF continuation lines (leading space after a newline) are
        // folded into logical lines before key/value parsing — see
        // [`Self::fold_main_section_lines`].
        let joined_lines = Self::fold_main_section_lines(&text);
        // Per the JAR spec the file is split into sections by blank lines;
        // only the *main* section (the leading section before the first
        // blank line) carries the manifest-wide attributes that
        // `Package.getImplementationVersion()` etc. consult. Per-entry
        // sections after the first blank are skipped.
        for line in joined_lines.iter() {
            if line.is_empty() {
                break;
            }
            if let Some((key, value)) = line.split_once(':') {
                let key = key.trim();
                let value = value.trim();
                // Cap most attributes to 8 KiB to keep a malicious manifest
                // from blowing out memory, but allow a larger window for
                // Class-Path: build tools (Surefire booter JARs in
                // particular) routinely emit 10s of KB when test classpaths
                // include many dependencies.
                // Attribute names are case-insensitive (`java.util.jar.
                // Attributes.Name` compares ignoring ASCII case, and
                // `JarFile.checkForSpecialAttributes` canonicalises the
                // `Class-Path` / `Multi-Release` bytes it searches for), so a
                // `class-path:` or `MULTI-RELEASE:` header is honoured on
                // HotSpot (interpreter round i4 wave 98, lane L5;
                // `tools/probes/interp/L5/L5W98ManifestHeaderCase.java`).
                let lower_key = key.to_ascii_lowercase();
                let max_len = if lower_key == "class-path" {
                    256 * 1024
                } else {
                    8 * 1024
                };
                if value.len() > max_len {
                    continue;
                }
                info.attributes.insert(key.to_string(), value.to_string());
                match lower_key.as_str() {
                    "main-class" => info.main_class = Some(value.to_string()),
                    "start-class" => info.start_class = Some(value.to_string()),
                    "spring-boot-classes" => info.boot_classes = Some(value.to_string()),
                    "spring-boot-lib" => info.boot_lib = Some(value.to_string()),
                    "class-path" => info.class_path = Some(value.to_string()),
                    "multi-release" => info.multi_release = value.eq_ignore_ascii_case("true"),
                    _ => {}
                }
            }
        }
        info
    }

    /// Does JDK 25's `URLClassPath.JarLoader.parseClassPath` throw for this
    /// manifest's `Class-Path`, so that `URLClassPath.getLoader` skips the
    /// WHOLE jar (its own classes and resources, and its `Class-Path`)?
    /// `new URL(fileBase, token)` raises `MalformedURLException("unknown
    /// protocol: c")` for a token whose scheme has no URL handler -- a Windows
    /// drive spelling such as `C:/x.jar` or `C:\x.jar` is the scheme `c` --
    /// and the `IOException` is swallowed by `getLoader` with the loader
    /// unadded. Interpreter round i5 wave 107, lane L5 (probe
    /// `tools/probes/interp/L5/L5W107ClassPathDriveToken`).
    pub fn class_path_drops_jar(&self) -> bool {
        self.class_path.as_deref().is_some_and(|cp| {
            cp.split_whitespace().any(|token| {
                manifest_token_protocol(token)
                    .is_some_and(|protocol| !URL_HANDLER_PROTOCOLS.contains(&protocol.as_str()))
            })
        })
    }

    /// Resolve `Class-Path` manifest entries relative to a JAR file's parent directory.
    ///
    /// Per the JAR specification, `Class-Path` entries are space-separated URIs
    /// resolved relative to the JAR's location (not the current working directory).
    ///
    /// A token with a scheme other than `file` is not an entry (JDK 25
    /// `JarLoader.tryResolveFile`: "If the protocol for the resolved URL isn't
    /// file:, it should be ignored"); a scheme with no handler at all drops
    /// the whole jar instead, which the caller decides with
    /// [`Self::class_path_drops_jar`]. Before wave 107 (lane L5) a drive
    /// spelling (`C:/x.jar`) was honoured as a path and an `http:` token was
    /// joined to the jar's directory.
    pub fn resolve_class_path(&self, jar_path: &Path) -> Vec<String> {
        let jar_dir = jar_path.parent().unwrap_or(Path::new("."));
        match &self.class_path {
            Some(cp) => cp
                .split_whitespace()
                .filter(|entry| manifest_token_protocol(entry).is_none_or(|p| p == "file"))
                .filter_map(|entry| {
                    let resolved = Self::decode_manifest_classpath_entry(entry, jar_dir);
                    // VULN (low) hardening opt-in: when
                    // `CRATONVM_HARDEN_MANIFEST_CLASSPATH` is set, drop any
                    // manifest `Class-Path` root that escapes the JAR's own
                    // directory (absolute roots, `file:/etc`, `..` escapes).
                    // Default-off preserves HotSpot's trusted-manifest
                    // behaviour; see `decode_manifest_classpath_entry`.
                    if harden_manifest_classpath() && !path_is_within(&resolved, jar_dir) {
                        debug!(
                            "manifest Class-Path entry {:?} resolves outside the \
                             JAR directory {:?}; dropped \
                             (CRATONVM_HARDEN_MANIFEST_CLASSPATH)",
                            entry, jar_dir
                        );
                        return None;
                    }
                    if Self::token_is_slashless_directory(entry, &resolved) {
                        debug!(
                            "manifest Class-Path entry {:?} names a directory without a \
                             trailing '/'; ignored (JDK 25 opens it as a jar and fails)",
                            entry
                        );
                        return None;
                    }
                    Some(resolved.to_string_lossy().into_owned())
                })
                .collect(),
            None => Vec::new(),
        }
    }

    /// Does `token` name an existing directory without a trailing `/`? JDK
    /// 25's `URLClassPath.getLoader(URL)` makes a `FileLoader` only for a
    /// URL whose file ends with `/`; any other URL is a `JarLoader`, whose
    /// constructor opens the directory as a jar, fails, and `getLoader`
    /// skips the URL. A trailing `.` or `..` segment counts as a slash
    /// (`URLStreamHandler.parseURL` folds `dir/.` to `dir/`), and on Windows
    /// so does a backslash (the `file:` handler reads it as `/`). CratonVM used every
    /// such directory as a class path root. Interpreter round i5 wave 108,
    /// lane L5 (probe `L5W108ClassPathDirectoryNoSlash`).
    fn token_is_slashless_directory(token: &str, resolved: &Path) -> bool {
        let token = if cfg!(windows) {
            token.replace('\\', "/")
        } else {
            token.to_string()
        };
        let slash = token.ends_with('/')
            || token.ends_with("/.")
            || token.ends_with("/..")
            || token == "."
            || token == "..";
        !slash && resolved.is_dir()
    }

    /// The main-section `Class-Path` tokens in order, as JDK 25's
    /// `JarLoader.parseClassPath` meets them: a relative or `file:` token is
    /// the path [`Self::resolve_class_path`] resolves it to (dropped under
    /// `CRATONVM_HARDEN_MANIFEST_CLASSPATH` as there), any other token its
    /// lower-cased scheme ([`manifest_token_protocol`]). Interpreter round i5
    /// wave 108, lane L5 ([`simulate_url_class_path`]).
    fn class_path_tokens(&self, jar_path: &Path) -> Vec<ClassPathToken> {
        let jar_dir = jar_path.parent().unwrap_or(Path::new("."));
        let Some(cp) = self.class_path.as_deref() else {
            return Vec::new();
        };
        cp.split_whitespace()
            .filter_map(|token| match manifest_token_protocol(token) {
                Some(protocol) if protocol != "file" => Some(ClassPathToken::Scheme(protocol)),
                _ => {
                    let resolved = Self::decode_manifest_classpath_entry(token, jar_dir);
                    if harden_manifest_classpath() && !path_is_within(&resolved, jar_dir) {
                        return None;
                    }
                    if Self::token_is_slashless_directory(token, &resolved) {
                        return None;
                    }
                    Some(ClassPathToken::File(resolved.to_string_lossy().into_owned()))
                }
            })
            .collect()
    }

    /// Returns true if this JAR appears to be a Spring Boot fat JAR.
    pub fn is_spring_boot(&self) -> bool {
        self.start_class.is_some() || self.boot_classes.is_some() || self.boot_lib.is_some()
    }
}

/// Maximum Java version supported by this VM. Multi-release JARs check
/// `META-INF/versions/{N}/` entries descending from this version down to 8
/// ([`versioned_lookup_range`]).
const MULTI_RELEASE_MAX_VERSION: u32 = 25;

/// The `META-INF/versions/<n>/` numbers JDK 25's `JarFile.getVersionedEntry`
/// probes for entry `name` in a multi-release jar read at version `max`: every
/// present `n` in `8..=max` (it walks `versions.previousSetBit(..)` while
/// `version >= BASE_VERSION_FEATURE`, which is 8), and none at all when `max`
/// is 8 or less (`BASE_VERSION_FEATURE < versionFeature` fails: base entries
/// only) or when `name` itself starts with `META-INF/` (never versioned).
/// Interpreter round i5 wave 104, lane L5: the lookups stopped at 9 before,
/// so a `META-INF/versions/8/` entry HotSpot 25 serves was not served.
#[inline]
fn versioned_lookup_range(name: &str, max: u32) -> std::ops::Range<u32> {
    const BASE_VERSION_FEATURE: u32 = 8;
    if max <= BASE_VERSION_FEATURE || name.starts_with("META-INF/") {
        0..0
    } else {
        BASE_VERSION_FEATURE..max + 1
    }
}

/// Compile-time JVM feature version this build implements. `Runtime.version()
/// .feature()` returns this value to user code. Used to bound the
/// multi-release JAR shadow-search range so an attacker can't smuggle a
/// `META-INF/versions/<future-N>/java/lang/String.class` past the JVM by
/// targeting a version we haven't been compiled to honor.
const JVM_FEATURE_VERSION: u32 = MULTI_RELEASE_MAX_VERSION;

/// Audit-fix #2: zip-bomb cap. Maximum uncompressed bytes we will
/// `Vec::with_capacity` for a single ZIP entry. The declared `size` field
/// in the central directory is attacker-controlled and can be up to
/// `u64::MAX`; without this clamp a malicious JAR triggers a multi-GiB
/// allocation before any decompression even starts. 512 MiB is well
/// above any legitimate single class file or resource and matches the
/// upper bound JDK 21+'s `ZipInputStream` uses internally.
pub(crate) const MAX_UNCOMPRESSED_ENTRY_BYTES: u64 = 512 * 1024 * 1024;

/// Directory prefix under which a JMOD stores its class files and the
/// resources that ship inside the module (`classes/java/lang/Object.class`,
/// `classes/META-INF/services/...`). A JMOD's other top-level directories
/// (`lib/`, `bin/`, `conf/`, `include/`, `legal/`, `man/`) are build-time
/// artefacts that the runtime reads from `$JAVA_HOME`, not from the module,
/// which is why only this subtree is indexed for class lookup.
const JMOD_CLASSES_PREFIX: &str = "classes/";

/// Clamp a ZIP entry's declared `size()` to [`MAX_UNCOMPRESSED_ENTRY_BYTES`]
/// for `Vec::with_capacity`. Returns the clamped capacity as `usize`.
#[inline]
fn safe_with_capacity(declared_size: u64) -> usize {
    declared_size.min(MAX_UNCOMPRESSED_ENTRY_BYTES) as usize
}

/// V2 (decompression-bomb DoS): read a ZIP entry's *streaming* inflate
/// output, bounding the **actual** inflated size — not just the declared
/// pre-allocation [`safe_with_capacity`] guards.
///
/// The `zip` reader streams deflate output up to the entry's declared
/// uncompressed size, which is attacker-controlled; a highly compressible
/// payload (~1000:1) lets a tiny compressed entry inflate to GBs and
/// `read_to_end` grows the buffer to that full size regardless of the
/// 512 MiB capacity clamp. Here we read with `Read::take(cap + 1)` and
/// reject (with an `io::Error`) the instant the running inflated total
/// exceeds [`MAX_UNCOMPRESSED_ENTRY_BYTES`], so the buffer can never grow
/// past the cap by more than one byte before the abort.
///
/// `declared_size` is the central-directory `size()` used only to seed the
/// initial `Vec` capacity (already clamped via [`safe_with_capacity`]); the
/// cap, not the declared size, is the authoritative bound on what we read.
fn read_entry_capped<R: Read>(reader: &mut R, declared_size: u64) -> std::io::Result<Vec<u8>> {
    let cap = MAX_UNCOMPRESSED_ENTRY_BYTES;
    let mut data = Vec::with_capacity(safe_with_capacity(declared_size));
    // Read at most `cap + 1` bytes: hitting `cap + 1` proves the real
    // inflated stream exceeds the cap (a lying-small `size()` bomb).
    let read = reader.take(cap + 1).read_to_end(&mut data)?;
    if read as u64 > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "zip entry inflated size exceeds MAX_UNCOMPRESSED_ENTRY_BYTES (decompression bomb)",
        ));
    }
    Ok(data)
}

/// Audit-fix #3: reject zip-slip-style entry names so attacker-controlled
/// ZIP entries cannot poison in-memory resource caches. Validates against
/// path-escape, absolute paths, NUL bytes, Windows drive letters, and
/// the alternate Windows separator. Returns `false` for any name we
/// refuse to cache.
pub(crate) fn is_safe_entry_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    if name.contains('\0') {
        return false;
    }
    if name.starts_with('/') || name.starts_with('\\') {
        return false;
    }
    if name.contains(':') {
        return false; // Windows drive letters: `C:\...`
    }
    if name.contains('\\') {
        // ZIP spec mandates `/` as separator; backslash is suspicious
        // and on Windows would create an alternate path interpretation.
        return false;
    }
    // Reject any path component equal to `..` (path escape) or `.`
    // (no-op components that obscure traversal).
    for component in name.split('/') {
        if component == ".." || component == "." {
            return false;
        }
    }
    true
}

/// VULN (low) hardening helper: returns `true` when `candidate` resolves
/// to a location at or beneath `base`, using a purely *lexical*
/// normalization (no filesystem access — `resolve_class_path` is a pure
/// resolver and the paths may not exist yet).
///
/// Both paths are first lexically normalized: `.` components are dropped
/// and `..` components pop the previous normal component (a leading or
/// un-poppable `..` is preserved, which keeps the candidate firmly
/// *outside* any relative base). The candidate is then required to share
/// `base`'s full component prefix. A `base` that normalizes to empty
/// (e.g. `"."`) admits any relative candidate but still rejects absolute
/// ones and ones that climb above the current directory.
///
/// This is intentionally conservative: it only *adds* a restriction when
/// `CRATONVM_HARDEN_MANIFEST_CLASSPATH` is set, so a false negative
/// (dropping a borderline path) is preferable to admitting an escape.
fn path_is_within(candidate: &Path, base: &Path) -> bool {
    use std::path::Component;

    // Lexically normalize a path into a component vector. Returns `None`
    // if a `..` would climb above the root the path is anchored to, which
    // for our purposes means "treat as outside" (the caller drops it).
    fn normalize(p: &Path) -> Vec<Component<'_>> {
        let mut stack: Vec<Component<'_>> = Vec::new();
        for comp in p.components() {
            match comp {
                Component::CurDir => {}
                Component::ParentDir => {
                    match stack.last() {
                        // Pop a real directory component.
                        Some(Component::Normal(_)) => {
                            stack.pop();
                        }
                        // A `..` at/above an anchor (root/prefix) or after
                        // another preserved `..` is kept verbatim so the
                        // prefix check below can reject the escape.
                        _ => stack.push(comp),
                    }
                }
                other => stack.push(other),
            }
        }
        stack
    }

    let base_norm = normalize(base);
    let cand_norm = normalize(candidate);

    // An absolute candidate against a relative base (or vice-versa) can
    // never be "within": their root/prefix components won't match, so the
    // prefix check below handles it. The candidate must be at least as
    // long as the base and agree on every base component.
    if cand_norm.len() < base_norm.len() {
        return false;
    }
    // Special case: a base that normalizes to empty (e.g. `"."`, used when
    // the JAR has no parent dir) means "the JAR's own relative location".
    // A relative candidate is admissible, but an *absolute* one (a
    // root/prefix first component, e.g. manifest `file:/etc`) escapes it
    // and must be rejected.
    if base_norm.is_empty() {
        if let Some(first) = cand_norm.first() {
            if matches!(first, Component::Prefix(_) | Component::RootDir) {
                return false;
            }
        }
    }
    base_norm
        .iter()
        .zip(cand_norm.iter())
        .all(|(b, c)| b == c)
        // A normalized candidate that still contains a leading `..` escaped
        // above its anchor is never "within" a base lacking that same `..`.
        && !cand_norm
            .iter()
            .skip(base_norm.len())
            .any(|c| matches!(c, Component::ParentDir))
}

/// Percent-decode an RFC 3986 `file:` URI path.
///
/// Each `%XX` escape (two upper- or lower-case hex digits) is decoded to the
/// corresponding byte; the resulting byte sequence is then interpreted as
/// UTF-8 (lossily, so a malformed/non-UTF-8 manifest cannot panic us). Any
/// `%` not followed by two valid hex digits is preserved verbatim, so a
/// literal `%` in a path is left intact rather than dropped.
///
/// This replaces the earlier `replace('%', "%")` no-op plus a hardcoded
/// `%20/%5B/%5D/%7B/%7D` subset, which mangled any other escape (e.g.
/// `%2520`, accented characters, `%28`/`%29`). Decoding is intentionally
/// unconditional over the whole string; the surrounding `from_file_uri`
/// logic still strips the leading `file:` slashes afterward, and downstream
/// filesystem reads are canonicalize-vs-root checked, so a decoded separator
/// or `..` cannot be used to escape the intended classpath root.
pub(crate) fn decode_percent_path(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            // Need two hex digits following the '%'.
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Reject resource names that could escape the classpath root or be
/// reinterpreted as a host path. This mirrors the input filter applied by
/// [`ClassPath::find_class`] so that `getResource`/`getResourceAsStream`
/// and the `find_all_resource_*` enumeration paths reject the same hostile
/// inputs: `..` traversal, NUL bytes, leading `/`, any Windows separator
/// `\\`, Windows drive letters (`:`), and `./` / `.\\`
/// current-dir references. The caller is expected to have already stripped
/// any leading `/` (HotSpot strips one leading slash from resource names);
/// a *remaining* leading `/` after that strip is still rejected here.
///
/// The downstream canonicalize-vs-root check in each resource path remains
/// the authoritative backstop; this is a cheap pre-filter that keeps the
/// resource and class entry points symmetric.
pub(crate) fn is_safe_resource_name(name: &str) -> bool {
    !(name.contains("..")
        || name.starts_with('/')
        || name.starts_with('\\')
        || name.contains('\\')
        || name.contains('\0')
        || name.contains(':') // Windows drive letters (C:)
        || name.contains("./") // current-dir references
        || name.contains(".\\")) // Windows current-dir references
}

fn is_safe_class_name(name: &str) -> bool {
    !(name.contains("..")
        || name.starts_with('/')
        || name.starts_with('\\')
        || name.contains('\\')
        || name.contains('\0')
        || name.contains(':') // Windows drive letters (C:)
        || name.contains("./")) // current-dir references
}

fn is_directory_resolvable_resource_name(name: &str) -> bool {
    // HotSpot normalizes "." and ".." when a URLClassLoader probes a directory
    // classpath root. Let directory lookups reach the canonical root check below,
    // but keep archive/JMOD/JRT lookups on the stricter literal resource filter.
    !(name.starts_with('/')
        || name.starts_with('\\')
        || name.contains('\\')
        || name.contains('\0')
        || name.contains(':'))
}

fn simple_resource_glob(name: &str) -> Option<(&str, &str)> {
    if !name.contains('*') && !name.contains('?') {
        return None;
    }
    let split = name.rfind('/').map(|idx| idx + 1).unwrap_or(0);
    let (prefix, pattern) = name.split_at(split);
    if prefix.contains('*')
        || prefix.contains('?')
        || (!prefix.is_empty() && !is_safe_resource_name(prefix))
        || pattern.is_empty()
    {
        return None;
    }
    Some((prefix, pattern))
}

fn glob_segment_matches(pattern: &str, candidate: &str) -> bool {
    let pattern = pattern.as_bytes();
    let candidate = candidate.as_bytes();
    let (mut pi, mut ci) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut star_match = 0usize;

    while ci < candidate.len() {
        if pi < pattern.len() && (pattern[pi] == b'?' || pattern[pi] == candidate[ci]) {
            pi += 1;
            ci += 1;
        } else if pi < pattern.len() && pattern[pi] == b'*' {
            star = Some(pi);
            pi += 1;
            star_match = ci;
        } else if let Some(star_idx) = star {
            pi = star_idx + 1;
            star_match += 1;
            ci = star_match;
        } else {
            return false;
        }
    }

    while pi < pattern.len() && pattern[pi] == b'*' {
        pi += 1;
    }
    pi == pattern.len()
}

/// Match one candidate against an **already-parsed** glob.
///
/// Split out of [`resource_name_matches_simple_glob`] so a caller that tests
/// many candidates against one glob parses the glob once instead of once per
/// candidate. See [`ClassPath::matching_resource_entry_names`].
#[inline]
fn resource_name_matches_parsed_glob(prefix: &str, pattern: &str, candidate: &str) -> bool {
    let Some(tail) = candidate.strip_prefix(prefix) else {
        return false;
    };
    !tail.is_empty() && !tail.contains('/') && glob_segment_matches(pattern, tail)
}

fn resource_name_matches_simple_glob(glob: &str, candidate: &str) -> bool {
    let Some((prefix, pattern)) = simple_resource_glob(glob) else {
        return false;
    };
    resource_name_matches_parsed_glob(prefix, pattern, candidate)
}

/// Parse a `<jar-path>!/<prefix>/` specification of the form produced by
/// stripping `file:`/`jar:` off a `jar:file:/.../foo.jar!/some/dir/` URL.
/// Returns `Some((jar_path, prefix_with_trailing_slash))` if the input
/// matches; `None` otherwise. The prefix is guaranteed to end with `/`,
/// matching the convention used by [`ClassPathEntry::NestedDirectory`].
///
/// Examples:
///   "C:/dacapo.jar!/harness/" -> Some(("C:/dacapo.jar", "harness/"))
///   "/lib/foo.jar!/META-INF/"  -> Some(("/lib/foo.jar", "META-INF/"))
///   "/apps/app.war!/WEB-INF/classes/" -> Some(("/apps/app.war", "WEB-INF/classes/"))
///   "C:/x.jar"                 -> None  (no `!/` separator)
///   "C:/x.jar!/"               -> None  (empty prefix → equivalent to root)
fn parse_jar_subdir_spec(spec: &str) -> Option<(String, String)> {
    let idx = spec.find("!/")?;
    let jar_part = &spec[..idx];
    let prefix_part = &spec[idx + 2..];
    // Spring Boot's `JarUrl.create(file, "BOOT-INF/classes/")` always marks
    // the root of the nested location with a SECOND, trailing `!/` after the
    // prefix itself (`jar:nested:<jar>/!BOOT-INF/classes/!/`), which
    // `extract_url_path` passes through unchanged. Strip that trailing
    // marker so the prefix used to match zip entries is the real one
    // (`BOOT-INF/classes/`) rather than the literal, unmatchable
    // `BOOT-INF/classes/!/`.
    let prefix_part = prefix_part.strip_suffix("!/").unwrap_or(prefix_part);
    if jar_part.is_empty() || prefix_part.is_empty() {
        return None;
    }
    // The outer file's extension is deliberately not part of the grammar.
    // `URLClassLoader` treats a `jar:` URL as an archive independently of its
    // name, and application servers routinely use `.war`, `.ear`, and `.par`
    // files. The caller verifies that the named file exists and that it is a
    // readable ZIP before it contributes an entry, so accepting the syntactic
    // `!/` form here cannot turn an arbitrary resource miss into a classpath
    // entry.
    let prefix = if prefix_part.ends_with('/') {
        prefix_part.to_string()
    } else {
        format!("{prefix_part}/")
    };
    // Defence-in-depth: prefix must be a sane forward-slash-only path.
    if prefix.contains('\\') || prefix.contains('\0') || prefix.contains("..") {
        return None;
    }
    Some((jar_part.to_string(), prefix))
}

impl ClassPath {
    /// Round 5 audit fix (MED): cached [`fs::canonicalize`] wrapper.
    ///
    /// The classpath-traversal symlink check (audit-fix #5) canonicalizes
    /// both the directory root AND the resolved class file on every probe.
    /// On a Spring app with 15k classes × 3 directory entries that's
    /// ~90k canonicalize syscalls per cold start. The cache makes
    /// repeated lookups of the same `PathBuf` zero-syscall.
    ///
    /// Errors are NOT cached — a transient `ENOENT` should not be
    /// remembered as a fail-closed verdict for the rest of the process.
    /// Canonicalize a **classpath root** — the `dir` of a
    /// [`ClassPathEntry::Directory`] or the `path` of a
    /// [`ClassPathEntry::JarFile`] — through an unbounded memo.
    ///
    /// This exists because the bounded [`Self::canonicalize_cached`] FIFO
    /// cannot serve a scan whose working set IS the classpath. Every
    /// `getResources` probe walks every entry and canonicalizes each root it
    /// touches; once the classpath is larger than [`CANONICALIZE_CACHE_CAP`]
    /// that cyclic access pattern evicts each root exactly before it is next
    /// needed, so the hit rate collapses to zero and every probe re-runs
    /// `realpath` on every root. Measured on the Quarkus full-reactor harness
    /// (4230 entries, 1676 of them directories): 6.79M `readlink` calls for a
    /// single test class — half of a 110 s run — against 66.5K on HotSpot for
    /// the same work.
    ///
    /// The cap was added (Round 7 audit, HIGH #4) to stop an unbounded map
    /// growing on arbitrary *probe* paths, and that concern is untouched:
    /// probe paths still go through [`Self::canonicalize_cached`]. Roots are a
    /// different population — the key set here is bounded by `self.entries`,
    /// i.e. by the classpath the launcher was already given — so this map
    /// cannot grow past a size the process has already paid for. It is
    /// cleared alongside the bounded cache whenever a root is retracted.
    ///
    /// Errors are NOT cached, for the same reason as `canonicalize_cached`.
    fn canonicalize_root(&self, path: &Path) -> std::io::Result<PathBuf> {
        {
            let guard = self.root_canonical_cache.lock();
            if let Some(canon) = guard.get(path) {
                return Ok(canon.clone());
            }
        }
        let canon = fs::canonicalize(path)?;
        self.root_canonical_cache
            .lock()
            .insert(path.to_path_buf(), canon.clone());
        Ok(canon)
    }

    fn canonicalize_cached(&self, path: &Path) -> std::io::Result<PathBuf> {
        {
            let guard = self.canonicalize_cache.lock();
            if let Some(canon) = guard.paths.get(path) {
                return Ok(canon.clone());
            }
        }
        let canon = fs::canonicalize(path)?;
        // Insert under the lock; tolerate the race where another thread
        // inserted the same entry between our read and write — they
        // produce identical values so either wins. Round 7 audit fix
        // (HIGH #4): inserts now go through the bounded
        // [`CanonicalizeCache::insert`] which enforces the FIFO cap.
        self.canonicalize_cache
            .lock()
            .insert(path.to_path_buf(), canon.clone());
        Ok(canon)
    }

    fn matching_directory_resource_paths(dir: &Path, name: &str) -> Vec<PathBuf> {
        let Some((prefix, pattern)) = simple_resource_glob(name) else {
            return Vec::new();
        };
        let base = dir.join(prefix);
        let mut matches = Vec::new();
        let Ok(entries) = fs::read_dir(base) else {
            return matches;
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(file_name) = file_name.to_str() else {
                continue;
            };
            if glob_segment_matches(pattern, file_name) {
                matches.push(entry.path());
            }
        }
        matches.sort();
        matches
    }

    /// Every entry name matching `glob`, sorted.
    ///
    /// PERF (2026-07-26 classpath-scan audit): the glob is parsed **once**.
    /// This used to call `resource_name_matches_simple_glob` per candidate,
    /// which re-ran `simple_resource_glob` — two `contains` scans, an `rfind`,
    /// and an `is_safe_resource_name` prefix check that is itself six more
    /// substring scans — for every name in the archive, on a loop-invariant
    /// string. The caller is `find_all_resource_urls` on the glob path, which
    /// visits *every* entry name of *every* archive on the classpath; the
    /// hottest reachable caller is `ClassLoader.getDefinedPackage`
    /// (`Class.getPackage()` → `"<pkg>/*.class"`), so this ran O(total entry
    /// names across all jars) times per `getPackage()` call.
    ///
    /// An unparseable glob previously made the per-candidate predicate return
    /// `false` for everything, yielding an empty vec; the early return here is
    /// the same answer without the walk.
    fn matching_resource_entry_names<'a, I>(names: I, glob: &str) -> Vec<String>
    where
        I: IntoIterator,
        I::Item: AsRef<str> + 'a,
    {
        let Some((prefix, pattern)) = simple_resource_glob(glob) else {
            return Vec::new();
        };
        let mut matches: Vec<String> = names
            .into_iter()
            .filter(|name| resource_name_matches_parsed_glob(prefix, pattern, name.as_ref()))
            .map(|name| name.as_ref().to_string())
            .collect();
        matches.sort();
        matches
    }

    fn checked_directory_resource_canonical(
        &self,
        dir: &Path,
        full_path: &Path,
        name: &str,
    ) -> Option<PathBuf> {
        let canon_dir = match self.canonicalize_root(dir) {
            Ok(p) => p,
            Err(e) => {
                debug!(
                    "Refusing to read resource {name}: cannot canonicalize classpath root {}: {e}",
                    dir.display()
                );
                return None;
            }
        };
        let canon_path = match self.canonicalize_cached(full_path) {
            Ok(p) => p,
            Err(e) => {
                debug!(
                    "Refusing to read resource {name}: cannot canonicalize resolved path {}: {e}",
                    full_path.display()
                );
                return None;
            }
        };
        if !canon_path.starts_with(&canon_dir) {
            debug!(
                "Resource path traversal blocked: {} escapes {}",
                canon_path.display(),
                canon_dir.display()
            );
            return None;
        }
        Some(canon_path)
    }

    fn directory_resource_url_from_canonical(canon_path: &Path) -> String {
        let p = canon_path.to_string_lossy().replace('\\', "/");
        let p = p.strip_prefix("//?/").unwrap_or(&p);
        let p = p.trim_start_matches('/');
        if canon_path.is_dir() && !p.ends_with('/') {
            format!("file:/{}/", encode_path_for_url(p))
        } else {
            format!("file:/{}", encode_path_for_url(p))
        }
    }

    /// Audit-fix #7: build (once) the set of `META-INF/versions/<N>/`
    /// numbers actually present in `archive`. Subsequent multi-release
    /// lookups iterate only over this set, skipping the per-class
    /// `by_name` probes the previous implementation paid for every
    /// absent version.
    fn ensure_versions_cache(
        archive: &(impl ArchiveRead + ?Sized),
        versions_cache: &Mutex<Option<Arc<BTreeSet<u32>>>>,
    ) -> Arc<BTreeSet<u32>> {
        // PERF: hand back a shared `Arc` so the fast path (every
        // multi-release class lookup after the first) is an atomic
        // reference-count bump, not a heap-allocating deep `BTreeSet`
        // clone. The cached set is immutable after construction, so all
        // callers can safely share it; they only iterate over it.
        {
            let guard = versions_cache.lock();
            if let Some(cache) = guard.as_ref() {
                return Arc::clone(cache);
            }
        }
        // Build by scanning the central directory once.
        let mut set: BTreeSet<u32> = BTreeSet::new();
        archive.each_name(&mut |name| {
            if let Some(rest) = name.strip_prefix("META-INF/versions/") {
                if let Some(slash_idx) = rest.find('/') {
                    let ver_str = &rest[..slash_idx];
                    if let Ok(ver) = ver_str.parse::<u32>() {
                        set.insert(ver);
                    }
                }
            }
        });
        // PERF: store the set behind an `Arc` once; the build path no
        // longer pays an extra `set.clone()` — the `Arc` and its return
        // value share the single heap allocation.
        let shared = Arc::new(set);
        let mut guard = versions_cache.lock();
        // Tolerate the benign race where another thread built the cache
        // first; either set is identical (pure function of the archive),
        // so adopt whichever is already published to keep all callers on
        // one shared allocation.
        if let Some(existing) = guard.as_ref() {
            return Arc::clone(existing);
        }
        *guard = Some(Arc::clone(&shared));
        shared
    }

    /// Look up an entry in a multi-release JAR archive.
    ///
    /// Audit-fix #4 (multi-release shadow attack): the version-search
    /// upper bound is now [`JVM_FEATURE_VERSION`] (the actual feature
    /// version this VM implements), NOT a fixed-25 constant. A JAR
    /// that ships a `META-INF/versions/99/java/lang/String.class` can
    /// no longer shadow the base entry when the running JVM doesn't
    /// claim version 99.
    ///
    /// Audit-fix #7 (perf): we consult the cached per-archive
    /// versions set (built lazily on first lookup) and only probe
    /// versions actually present in the archive, eliminating the
    /// per-class 17 `by_name` round-trips.
    fn find_in_multi_release_archive(
        archive: &(impl ArchiveRead + ?Sized),
        versions_cache: &Mutex<Option<Arc<BTreeSet<u32>>>>,
        entry_index: Option<&FxHashSet<String>>,
        name: &str,
        max: u32,
    ) -> Option<Vec<u8>> {
        let present = Self::ensure_versions_cache(archive, versions_cache);
        // Search descending so the highest-supported version wins,
        // but only over versions that ACTUALLY exist in this archive
        // AND that are ≤ the booted library's feature version (`max`,
        // [`ClassPath::multi_release_feature`]).
        for &ver in present.range(versioned_lookup_range(name, max)).rev() {
            let versioned = format!("META-INF/versions/{ver}/{name}");
            if entry_index.is_some_and(|idx| !idx.contains(&versioned)) {
                continue;
            }
            if let Some(data) = archive.read_vec(&versioned) {
                return Some(data);
            }
        }
        // Fall back to base entry.
        if entry_index.is_some_and(|idx| !idx.contains(name)) {
            return None;
        }
        archive.read_vec(name)
    }

    fn find_shared_in_multi_release_archive(
        archive: &(impl ArchiveRead + ?Sized),
        backing: &ArchiveBacking,
        versions_cache: &Mutex<Option<Arc<BTreeSet<u32>>>>,
        entry_index: &FxHashSet<String>,
        name: &str,
        max: u32,
    ) -> Option<SharedBytes> {
        let present = Self::ensure_versions_cache(archive, versions_cache);
        for &ver in present.range(versioned_lookup_range(name, max)).rev() {
            let versioned = format!("META-INF/versions/{ver}/{name}");
            if entry_index.contains(&versioned) {
                return archive.read_shared(backing, &versioned);
            }
        }
        if !entry_index.contains(name) {
            return None;
        }
        archive.read_shared(backing, name)
    }

    /// Return the physical entry selected for a multi-release lookup.
    ///
    /// Resource URLs must name this physical entry, not the logical base name:
    /// callers such as `URL.openStream()` re-open the URL and therefore cannot
    /// recover a version choice made only while probing the archive.  Returning
    /// `p/res.txt` for an effective `META-INF/versions/17/p/res.txt` made a
    /// multi-release class load use the right bytes while its corresponding
    /// resource URL exposed the base bytes.
    fn multi_release_entry_name(
        archive: &(impl ArchiveRead + ?Sized),
        versions_cache: &Mutex<Option<Arc<BTreeSet<u32>>>>,
        entry_index: &FxHashSet<String>,
        name: &str,
        max: u32,
    ) -> Option<String> {
        let present = Self::ensure_versions_cache(archive, versions_cache);
        for &ver in present.range(versioned_lookup_range(name, max)).rev() {
            let versioned = format!("META-INF/versions/{ver}/{name}");
            if entry_index.contains(&versioned) {
                return Some(versioned);
            }
        }
        entry_index.contains(name).then(|| name.to_string())
    }

    fn build_archive_entry_index(archive: &mut SharedArchive) -> FxHashSet<String> {
        let mut index = FxHashSet::default();
        for i in 0..archive.len() {
            if let Ok(entry) = archive.by_index_raw(i) {
                index.insert(entry.name().to_string());
            }
        }
        index
    }

    #[inline]
    fn find_in_indexed_archive(
        archive: &(impl ArchiveRead + ?Sized),
        entry_index: &FxHashSet<String>,
        name: &str,
    ) -> Option<Vec<u8>> {
        if !entry_index.contains(name) {
            return None;
        }
        archive.read_vec(name)
    }

    #[inline]
    fn find_shared_in_indexed_archive(
        archive: &(impl ArchiveRead + ?Sized),
        backing: &ArchiveBacking,
        entry_index: &FxHashSet<String>,
        name: &str,
    ) -> Option<SharedBytes> {
        if !entry_index.contains(name) {
            return None;
        }
        archive.read_shared(backing, name)
    }

    /// Create a classpath from a list of path strings.
    ///
    /// Each entry can be a directory or an archive file. Non-existent paths and
    /// invalid archive files are silently skipped with a debug log message.
    ///
    /// JAR files are automatically scanned for Spring Boot fat JAR structure:
    /// if `BOOT-INF/classes/` or `BOOT-INF/lib/` are detected (via MANIFEST.MF
    /// or directory probing), nested entries are extracted and added to the
    /// classpath.
    ///
    /// **Wildcard expansion (Java CLI parity).** An entry ending in `/*` or
    /// `\*` (or that is exactly `*`) is expanded to every `*.jar` / `*.JAR`
    /// directly inside the parent directory. This matches HotSpot's
    /// `-cp lib/*` syntax used by Elasticsearch, Cassandra, and most
    /// hand-rolled launchers. Without expansion, the literal entry
    /// `lib/*` is treated as a non-existent path and silently dropped —
    /// producing an empty classpath and breaking `ServiceLoader`
    /// (`META-INF/services/...`) discovery for every dependency.
    pub fn new(paths: &[String]) -> Self {
        Self::with_fat_jar_emulation(paths, true)
    }

    /// [`ClassPath::new`], choosing whether web and Spring Boot archives are
    /// emulated (see the `fat_jar_emulation` field). `false` is HotSpot's
    /// class path: an archive contributes its root, plus whatever its manifest
    /// `Class-Path:` names, and nothing else.
    pub fn with_fat_jar_emulation(paths: &[String], fat_jar_emulation: bool) -> Self {
        Self::with_options(paths, fat_jar_emulation, false)
    }

    /// [`ClassPath::new`] for the jars of a `URLClassLoader` a running
    /// program created (`native-builtins` `cached_class_path_for_paths`,
    /// which keeps it in a per-VM LRU long after the loader is closed): its
    /// archives, and the ones their manifests' `Class-Path:` name, are opened
    /// as [`ClassPath::add_path`] opens them
    /// ([`read_archive_for_dynamic_classpath_stamped`]: read into memory on
    /// Windows, never mapped), so the program can still truncate, rewrite,
    /// rename over or delete them as on HotSpot. Interpreter round i4 wave
    /// 97, lane L5 (`--compatible` on Windows: `L5W64JarLoaderBytesIndex`,
    /// `L5W65`-`L5W69`, `L5W79NestedJarSameStampRename`).
    pub fn for_url_loader(paths: &[String]) -> Self {
        Self::with_options(paths, true, true)
    }

    /// [`ClassPath::with_fat_jar_emulation`]; `dynamic`: see
    /// [`ClassPath::for_url_loader`].
    fn with_options(paths: &[String], fat_jar_emulation: bool, dynamic: bool) -> Self {
        let __diag_start = loader_flags().dbg_classpath.then(std::time::Instant::now);
        let __diag_npaths = paths.len();
        // Stamped BEFORE the archives are read, so a rewrite racing the read
        // leaves a stamp that no longer matches, never a fresh stamp over
        // stale bytes.
        let mut url_loader_stamps: Vec<(PathBuf, Option<OuterArchiveStamp>)> = Vec::new();
        if dynamic {
            for raw in paths {
                let file = parse_jar_subdir_spec(raw)
                    .map(|(archive, _)| archive)
                    .unwrap_or_else(|| raw.clone());
                let file = PathBuf::from(file);
                if !url_loader_stamps.iter().any(|(seen, _)| *seen == file) {
                    let stamp = OuterArchiveStamp::read(&file);
                    url_loader_stamps.push((file, stamp));
                }
            }
        }
        let mut entries = Vec::new();
        for raw in paths {
            Self::process_classpath_token(raw, &mut entries, 0, fat_jar_emulation, dynamic);
        }
        if dynamic {
            // The jars a manifest `Class-Path:` added: the stamp of the
            // handle that read them when kept, else the file's now.
            for entry in &entries {
                if let ClassPathEntry::JarFile { path, stamp, .. } = entry {
                    if !url_loader_stamps.iter().any(|(seen, _)| seen == path) {
                        let stamp = (*stamp).or_else(|| OuterArchiveStamp::read(path));
                        url_loader_stamps.push((path.clone(), stamp));
                    }
                }
            }
        }
        if let Some(t0) = __diag_start {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            eprintln!(
                "[CP] ClassPath::new call#{n} npaths={__diag_npaths} nentries={} elapsed={:?}",
                entries.len(),
                t0.elapsed()
            );
        }
        Self {
            entries,
            canonicalize_cache: Mutex::new(CanonicalizeCache::new()),
            root_canonical_cache: Mutex::new(HashMap::new()),
            dynamic_specs: HashMap::new(),
            static_len: None,
            dir_index: Mutex::new(HashMap::new()),
            fat_jar_emulation,
            fast_define_gate: OnceLock::new(),
            jar_entry_slots: Mutex::new(HashMap::new()),
            runtime_image_on: loader_flags().boot_image_class_bytes,
            runtime_image: OnceLock::new(),
            url_loader_stamps,
            multi_release_feature: std::sync::atomic::AtomicU32::new(JVM_FEATURE_VERSION),
            multi_release_feature_set: std::sync::atomic::AtomicBool::new(false),
            provider_circularity_errors: std::sync::atomic::AtomicUsize::new(0),
            provider_lookup_class_path: false,
        }
    }

    /// [`ClassPath::runtime_image_on`] for this path (tests: both arms of
    /// `CRATONVM_BOOT_IMAGE_CLASS_BYTES` in one process).
    #[cfg(test)]
    fn set_runtime_image_classes(&mut self, on: bool) {
        self.runtime_image_on = on;
        self.runtime_image = OnceLock::new();
    }

    /// The runtime image beside the first JDK `jmods/` directory on this path
    /// ([`RuntimeImageClasses`]), mapped on first use; `None` when the switch
    /// is off or there is no such image.
    fn runtime_image(&self) -> Option<&RuntimeImageClasses> {
        if !self.runtime_image_on {
            return None;
        }
        self.runtime_image
            .get_or_init(|| {
                let jmods_dir = self.entries.iter().find_map(|entry| match entry {
                    ClassPathEntry::JmodFile { path, .. } => {
                        let dir = path.parent()?;
                        (dir.file_name()? == "jmods").then(|| dir.to_path_buf())
                    }
                    _ => None,
                })?;
                let modules = jmods_dir.parent()?.join("lib").join("modules");
                if !modules.is_file() {
                    return None;
                }
                // Mapped only: a reader over a copy would put the whole image
                // (~146 MB) in memory for a handful of classes.
                let file = std::fs::File::open(&modules).ok()?;
                // SAFETY: a read-only private mapping of the runtime image, a
                // file the JDK installs once and never rewrites while a VM
                // runs (HotSpot maps the same file the same way).
                // Phase 5 boot image (WP2): brokered fixed address in a capture run.
                let shared: Arc<dyn AsRef<[u8]> + Send + Sync> =
                    match cratonvm_types::image_layout::map_file_readonly(
                        cratonvm_types::image_layout::SegmentKind::RuntimeImage,
                        &file,
                        &modules,
                    ) {
                        Some(fixed) => Arc::new(fixed),
                        None => Arc::new(unsafe { memmap2::MmapOptions::new().map(&file) }.ok()?),
                    };
                let reader = cratonvm_reader::JImageReader::from_shared(shared).ok()?;
                debug!("Boot classes from runtime image {}", modules.display());
                Some(RuntimeImageClasses {
                    jmods_dir,
                    reader,
                    package_modules: OnceLock::new(),
                })
            })
            .as_ref()
    }

    /// The runtime image's bytes of `class_name`, which the JDK JMOD at
    /// `jmod` also holds; `None` (serve the JMOD's) when `jmod` is not one of
    /// the image's JMODs, for a module descriptor, or when the image does not
    /// have the class or cannot serve it.
    fn runtime_image_class_for_jmod(&self, jmod: &Path, class_name: &str) -> Option<SharedBytes> {
        let image = self.runtime_image()?;
        if jmod.parent()? != image.jmods_dir.as_path()
            || class_name == "module-info"
            || class_name.ends_with("/module-info")
        {
            return None;
        }
        let module = jmod.file_stem()?.to_str()?;
        let bytes = image.reader.find_class_bytes(module, class_name).ok()??;
        debug!("Found class {class_name} in the runtime image (module {module})");
        Some(SharedBytes::from(bytes))
    }

    /// A class no entry of this path holds, from the runtime image: the module
    /// is the one whose JMOD holds the class's package (a class only the image
    /// holds is a jlink plugin's, in a package of its module).
    fn runtime_image_only_class(&self, class_name: &str) -> Option<SharedBytes> {
        let image = self.runtime_image()?;
        let (package, _) = class_name.rsplit_once('/')?;
        let modules = image.package_modules.get_or_init(|| {
            let mut map: FxHashMap<Box<str>, Box<str>> = FxHashMap::default();
            for entry in &self.entries {
                let ClassPathEntry::JmodFile {
                    path,
                    class_entry_index,
                    ..
                } = entry
                else {
                    continue;
                };
                if path.parent() != Some(image.jmods_dir.as_path()) {
                    continue;
                }
                let Some(module) = path.file_stem().and_then(|m| m.to_str()) else {
                    continue;
                };
                for name in class_entry_index {
                    let Some(class) = name.strip_suffix(".class") else {
                        continue;
                    };
                    if let Some((pkg, _)) = class.rsplit_once('/') {
                        if !map.contains_key(pkg) {
                            map.insert(pkg.into(), module.into());
                        }
                    }
                }
            }
            map
        });
        let module = modules.get(package)?;
        let bytes = image.reader.find_class_bytes(module, class_name).ok()??;
        debug!("Found class {class_name} only in the runtime image (module {module})");
        Some(SharedBytes::from(bytes))
    }

    /// Make this (the APPLICATION) class path the one JDK 25's lazy
    /// `URLClassPath` over `top` (the `java.class.path` elements, as handed
    /// to [`ClassPath::new`]) ends with when opening it runs a URL handler
    /// provider lookup ([`simulate_url_class_path`]): its loaders in that
    /// order, the jars it loses to `Error("Circular loading of URL stream
    /// handler providers detected")` gone, and that many errors left for
    /// [`Self::take_provider_circularity_error`]. Answers whether anything
    /// was applied; a class path with no such token, a runtime-extended one,
    /// or one with nested, jmod or jimage entries is left as it is.
    /// Interpreter round i5 wave 108, lane L5 (probe
    /// `tools/probes/interp/L5/L5W107ClassPathDriveToken`).
    pub fn apply_url_class_path_model(&mut self, top: &[String]) -> bool {
        if self.static_len.is_some()
            || self.entries.iter().any(|entry| {
                !matches!(
                    entry,
                    ClassPathEntry::Directory(_) | ClassPathEntry::JarFile { .. }
                )
            })
        {
            return false;
        }
        let top: Vec<PathBuf> = top
            .iter()
            .flat_map(|raw| Self::expand_classpath_wildcard(raw))
            .map(PathBuf::from)
            .collect();
        let model = {
            let entries = &self.entries;
            let by_path: FxHashMap<&Path, usize> = entries
                .iter()
                .enumerate()
                .filter_map(|(i, entry)| match entry {
                    ClassPathEntry::Directory(p) | ClassPathEntry::JarFile { path: p, .. } => {
                        Some((p.as_path(), i))
                    }
                    _ => None,
                })
                .collect();
            let mut tokens_of = |path: &Path| -> Option<Vec<ClassPathToken>> {
                match by_path.get(path).map(|&i| &entries[i]) {
                    Some(ClassPathEntry::JarFile { archive, .. }) => {
                        let manifest = archive.read_manifest();
                        Some(manifest.class_path_tokens(path))
                    }
                    Some(_) => Some(Vec::new()),
                    // Not an entry: a jar the eager walk dropped (an unknown
                    // scheme), or one it could not open.
                    None if path.is_dir() => Some(Vec::new()),
                    None if path.is_file() => {
                        Self::read_jar_manifest(path).map(|m| m.class_path_tokens(path))
                    }
                    None => None,
                }
            };
            simulate_url_class_path(&top, &mut tokens_of)
        };
        self.provider_lookup_class_path = model.provider_lookups > 0;
        if model.provider_lookups == 0 {
            return false;
        }
        let mut slots: Vec<Option<ClassPathEntry>> = std::mem::take(&mut self.entries)
            .into_iter()
            .map(Some)
            .collect();
        let mut reordered = Vec::with_capacity(model.order.len());
        for path in &model.order {
            let slot = slots.iter_mut().find(|slot| match slot.as_ref() {
                Some(ClassPathEntry::Directory(p)) | Some(ClassPathEntry::JarFile { path: p, .. }) => {
                    p == path
                }
                _ => false,
            });
            if let Some(entry) = slot.and_then(Option::take) {
                reordered.push(entry);
            }
        }
        for lost in slots.into_iter().flatten() {
            debug!(
                "class path entry {lost:?} lost to a URL handler provider lookup \
                 (JDK 25's URLClassPath never adds it)"
            );
        }
        self.entries = reordered;
        self.fast_define_gate = OnceLock::new();
        self.jar_entry_slots.lock().clear();
        self.runtime_image = OnceLock::new();
        self.provider_circularity_errors.store(
            model.circularity_errors,
            std::sync::atomic::Ordering::SeqCst,
        );
        true
    }

    /// Take one of the circularity `Error`s [`Self::apply_url_class_path_model`]
    /// left: `true` once per error (wave 108, lane L5).
    pub fn take_provider_circularity_error(&self) -> bool {
        self.provider_circularity_errors
            .try_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |n| n.checked_sub(1),
            )
            .is_ok()
    }

    /// Did [`Self::apply_url_class_path_model`] meet a token that runs a URL
    /// handler provider lookup (wave 108, lane L5)?
    pub fn provider_lookup_class_path(&self) -> bool {
        self.provider_lookup_class_path
    }

    /// Serve multi-release jars at the booted class library's feature
    /// version `feature` (HotSpot's `JarFile.runtimeVersion()`), clamped to
    /// `8..=`[`JVM_FEATURE_VERSION`] (8, `JarFile.BASE_VERSION_FEATURE`, serves
    /// no versioned entry: `-Djdk.util.jar.version` below 9, or
    /// `-Djdk.util.jar.enableMultiRelease=false`; see
    /// [`Self::jar_runtime_version`]). Call before the first lookup (a later
    /// call changes later lookups only). Interpreter round i5 wave 100, lane
    /// L5; floor 8 since wave 103.
    pub fn set_multi_release_feature(&self, feature: u32) {
        self.multi_release_feature.store(
            feature.clamp(8, JVM_FEATURE_VERSION),
            std::sync::atomic::Ordering::Relaxed,
        );
        self.multi_release_feature_set
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Has [`Self::set_multi_release_feature`] run on this class path?
    #[inline]
    pub fn multi_release_feature_is_set(&self) -> bool {
        self.multi_release_feature_set
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// JDK 25 `JarFile.<clinit>`'s `RUNTIME_VERSION` for a library of feature
    /// `feature`, given the values of `jdk.util.jar.version` and
    /// `jdk.util.jar.enableMultiRelease`: a parseable `jdk.util.jar.version`
    /// above `feature` keeps `feature`, otherwise it is `max(value, 8)`; and
    /// `enableMultiRelease=false` turns versioned lookups off (8, base entries
    /// only). An unparseable version (HotSpot: `NumberFormatException` in
    /// `JarFile.<clinit>`) keeps `feature`. Interpreter round i5 wave 103, lane
    /// L5 (`i100-L5-multi-release-jars-read-at-version-25-whatever-jdk-boots`).
    pub fn jar_runtime_version(
        feature: u32,
        jar_version: Option<&str>,
        enable_multi_release: Option<&str>,
    ) -> u32 {
        const BASE_VERSION_FEATURE: i64 = 8;
        if enable_multi_release == Some("false") {
            return BASE_VERSION_FEATURE as u32;
        }
        match jar_version.and_then(|v| v.parse::<i32>().ok()) {
            Some(v) if i64::from(v) <= i64::from(feature) => {
                i64::from(v).max(BASE_VERSION_FEATURE) as u32
            }
            _ => feature,
        }
    }

    /// The version [`Self::set_multi_release_feature`] set.
    #[inline]
    pub fn multi_release_feature(&self) -> u32 {
        self.multi_release_feature
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// For a [`ClassPath::for_url_loader`] value: is every archive it read
    /// still the file it read (one `stat` each; a directory, or a path that
    /// named nothing, must still be no regular file)? `false` once one was
    /// rewritten, replaced, created or deleted since: a NEW loader over the
    /// same URLs then opens them afresh, as HotSpot's does, instead of being
    /// answered from this copy (`native-builtins`
    /// `classloader::class_path_from_cache_for`). Always `true` for any other
    /// class path. Interpreter round i4 wave 97, lane L5
    /// (`L5W64JarLoaderBytesIndex` round 2, `L5W65NestedJarWalkRewrite`).
    pub fn url_loader_archives_unchanged(&self) -> bool {
        self.url_loader_stamps
            .iter()
            .all(|(path, stamp)| OuterArchiveStamp::read(path) == *stamp)
    }

    /// SBR5 `clload`: may the application loader define `internal` (slash form) from this class
    /// path WITHOUT running the JDK's `findClassOnClassPathOrNull` bytecode?
    ///
    /// `true` only when everything that bytecode does beyond "find the entry, read it, define it"
    /// is provably a no-op for this class:
    /// * the entry list is the startup one (no `add_path`: the JDK's `URLClassPath` is then the
    ///   same list, in the same order, and no loader reads a different `ClassPath`);
    /// * no entry is a nested jar / jmod / jimage and no jar's manifest has `Class-Path`
    ///   (the JDK appends those lazily, in its own order);
    /// * EVERY entry holding classes of the class's package is a directory or an unsigned jar
    ///   whose manifest has no `Sealed` attribute (per-package sections without it are read alike by both paths) (a sealed jar, a per-package
    ///   `Sealed` section and signers are the only inputs of the JDK's package/signer checks that
    ///   can throw or differ; with none of them among the entries a split package cannot raise a
    ///   sealing violation, and the `Package` is the first-loaded class's jar's in both paths);
    /// * the name is not in `java.*` (`preDefineClass` refuses it).
    ///
    /// Multi-release jars are handled: a `META-INF/versions/<n>/` name counts as its base package,
    /// and the entry lookup the native define then runs is this class path's own
    /// (`find_class`, which searches `META-INF/versions/25` down to 8 as the JDK does).
    pub fn fast_define_ok(&self, internal: &str) -> bool {
        // A class path whose opening runs a URL handler provider lookup
        // (`apply_url_class_path_model`): the JDK's first lookup that walks
        // the `URLClassPath` past what it has opened is the one that meets
        // the lookup (and its circularity `Error`), so every lookup must be
        // the JDK's (interpreter round i5 wave 108, lane L5).
        if self.static_len.is_some()
            || self.provider_lookup_class_path
            || internal.starts_with("java/")
            || !is_safe_class_name(internal)
        {
            return false;
        }
        let gate = self
            .fast_define_gate
            .get_or_init(|| self.build_fast_define_gate());
        if gate.disabled {
            return false;
        }
        let pkg = internal.rsplit_once('/').map_or("", |(p, _)| p);
        gate.packages.get(pkg).copied().unwrap_or(false)
    }

    /// SBR5 `clload`: is `internal` provably on NO entry of this class path, so the JDK's
    /// `URLClassPath` lookup (one `getEntry` per jar, versioned entries included) can only answer
    /// "not found"? Same preconditions as [`Self::fast_define_ok`] for the class path itself (the
    /// startup entry list, no `Class-Path`, no nested / jmod / jimage entry), the name in no jar
    /// index (a multi-release `META-INF/versions/<n>/` name counts as its base name), and the file
    /// absent from every directory root on disk right now.
    pub fn fast_absent_ok(&self, internal: &str) -> bool {
        // A class path whose opening runs a URL handler provider lookup
        // (`apply_url_class_path_model`): the JDK's first lookup that walks
        // the `URLClassPath` past what it has opened is the one that meets
        // the lookup (and its circularity `Error`), so every lookup must be
        // the JDK's (interpreter round i5 wave 108, lane L5).
        if self.static_len.is_some()
            || self.provider_lookup_class_path
            || internal.starts_with("java/")
            || !is_safe_class_name(internal)
        {
            return false;
        }
        let gate = self
            .fast_define_gate
            .get_or_init(|| self.build_fast_define_gate());
        if gate.disabled || gate.names.contains(internal) {
            return false;
        }
        let rel = format!("{internal}.class");
        !gate
            .dirs
            .iter()
            .any(|dir| dir.join(Path::new(&rel)).exists())
    }

    fn build_fast_define_gate(&self) -> FastDefineGate {
        let feature = self.multi_release_feature();
        let mut gate = FastDefineGate {
            disabled: false,
            packages: rustc_hash::FxHashMap::default(),
            entry_ok: Vec::with_capacity(self.entries.len()),
            names: rustc_hash::FxHashSet::default(),
            dirs: Vec::new(),
        };
        // package -> the entries holding classes of it (ascending, no repeats)
        let mut pkg_entries: rustc_hash::FxHashMap<Box<str>, Vec<usize>> =
            rustc_hash::FxHashMap::default();
        let mut note = |gate: &mut FastDefineGate, name: &str, idx: usize| {
            gate.names.insert(name.into());
            let pkg = name.rsplit_once('/').map_or("", |(p, _)| p);
            match pkg_entries.get_mut(pkg) {
                Some(v) => {
                    if v.last() != Some(&idx) {
                        v.push(idx);
                    }
                }
                None => {
                    pkg_entries.insert(pkg.into(), vec![idx]);
                }
            }
        };
        for (idx, entry) in self.entries.iter().enumerate() {
            match entry {
                ClassPathEntry::Directory(dir) => {
                    gate.dirs.push(dir.clone());
                    let mut names = Vec::new();
                    Self::walk_dir_classes(dir, dir, &mut names);
                    for cn in &names {
                        note(&mut gate, cn, idx);
                    }
                    gate.entry_ok.push(true);
                }
                ClassPathEntry::JarFile {
                    archive,
                    entry_index,
                    multi_release,
                    ..
                } => {
                    let mut ok = true;
                    for name in entry_index {
                        let upper = name.to_ascii_uppercase();
                        if upper.starts_with("META-INF/")
                            && ["SF", "RSA", "DSA", "EC"]
                                .iter()
                                .any(|ext| upper.ends_with(&format!(".{ext}")))
                            && !upper["META-INF/".len()..].contains('/')
                        {
                            ok = false;
                        }
                        let Some(base) = name.strip_suffix(".class") else {
                            continue;
                        };
                        let base = if let Some(rest) = base.strip_prefix("META-INF/versions/") {
                            if !*multi_release {
                                continue;
                            }
                            // Only the versions `JarFile.getEntry` searches at this class
                            // path's feature version (`feature` down to 8, wave 104) can serve
                            // a class; a class only under a higher (or non-numeric) version
                            // directory is absent, as on HotSpot (interpreter round i5 wave 103,
                            // lane L5,
                            // `i100-L5-multi-release-jars-read-at-version-25-whatever-jdk-boots`).
                            match rest.split_once('/') {
                                Some((ver, tail))
                                    if !ver.starts_with('0')
                                        && ver.bytes().all(|b| b.is_ascii_digit())
                                        && ver.parse::<u32>().is_ok_and(|v| {
                                            versioned_lookup_range(tail, feature).contains(&v)
                                        }) =>
                                {
                                    tail
                                }
                                _ => continue,
                            }
                        } else if base.starts_with("META-INF/") {
                            continue;
                        } else {
                            base
                        };
                        if base == "module-info" || base.ends_with("/module-info") {
                            continue;
                        }
                        note(&mut gate, base, idx);
                    }
                    // Manifest: a plain main section only, no `Class-Path`, no `Sealed`.
                    let text = archive.read_vec("META-INF/MANIFEST.MF");
                    if let Some(data) = text {
                        let text = String::from_utf8_lossy(&data).replace('\r', "");
                        for line in text.split('\n') {
                            let lower = line.to_ascii_lowercase();
                            if lower.starts_with("sealed:") {
                                ok = false;
                            }
                        }
                        if !manifest_class_path_is_simple(&text) {
                            gate.disabled = true;
                        }
                    }
                    gate.entry_ok.push(ok);
                }
                // A nested archive, a jmod, a jimage, a virtual directory: the JDK does not see
                // these as the startup `-cp` roots; keep the Java path for everything.
                _ => {
                    gate.disabled = true;
                    gate.entry_ok.push(false);
                }
            }
        }
        for (pkg, idxs) in pkg_entries {
            let all_ok = idxs
                .iter()
                .all(|i| gate.entry_ok.get(*i).copied().unwrap_or(false));
            gate.packages.insert(pkg, all_ok);
        }
        gate
    }

    /// Expand Java launcher classpath wildcards into their concrete JAR paths.
    ///
    /// This is also used when publishing `java.class.path`: HotSpot expands
    /// `lib/*` before exposing that property, and in-process javac consumes the
    /// property directly rather than consulting CratonVM's class loader.
    pub fn expand_classpath_entries(paths: &[String]) -> Vec<String> {
        paths
            .iter()
            .flat_map(|raw| Self::expand_classpath_wildcard(raw))
            .collect()
    }

    /// Expand a single classpath token, honouring Java's `dir/*` wildcard.
    ///
    /// HotSpot's `java -cp lib/*` expands to every JAR (`*.jar` / `*.JAR`)
    /// directly inside `lib/` — non-recursive, ignoring sub-directories.
    /// Non-wildcard entries pass through unchanged. The expanded order is
    /// sorted so behaviour is deterministic across runs and platforms;
    /// `ServiceLoader` provider ordering is observable for SPIs that
    /// register the same interface in multiple jars, so we want
    /// byte-identical lists across runs.
    ///
    /// This is the entry point used by both [`ClassPath::new`] and
    /// [`ClassPath::add_path`] so dynamic loaders that synthesise
    /// `someLib/*` strings benefit equally.
    fn expand_classpath_wildcard(raw: &str) -> Vec<String> {
        // Accept `dir/*`, `dir\*`, and the bare token `*` (current dir).
        // Patterns with an embedded `*` mid-path are NOT supported, in
        // line with HotSpot — pass them through literally and let the
        // load fail with a "skipping non-existent classpath entry"
        // debug line.
        let (dir_part, matched) = if raw == "*" {
            (".".to_string(), true)
        } else if let Some(parent) = raw.strip_suffix("/*") {
            (parent.to_string(), true)
        } else if let Some(parent) = raw.strip_suffix("\\*") {
            (parent.to_string(), true)
        } else {
            (String::new(), false)
        };
        if !matched {
            return vec![raw.to_string()];
        }
        if dir_part.contains('*') {
            debug!("Classpath wildcard with embedded '*' not supported: {raw}");
            return vec![raw.to_string()];
        }

        let dir = PathBuf::from(if dir_part.is_empty() {
            "."
        } else {
            dir_part.as_str()
        });
        let read = match fs::read_dir(&dir) {
            Ok(r) => r,
            Err(e) => {
                debug!(
                    "Classpath wildcard {raw}: cannot read directory {}: {e}",
                    dir.display()
                );
                // Yield zero entries — matches `java -cp missing/*`,
                // which silently expands to the empty set rather than
                // erroring.
                return Vec::new();
            }
        };
        let mut jars: Vec<String> = Vec::new();
        for ent in read.flatten() {
            let p = ent.path();
            if !p.is_file() {
                continue;
            }
            let is_jar = p
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("jar"));
            if !is_jar {
                continue;
            }
            jars.push(p.to_string_lossy().into_owned());
        }
        jars.sort();
        if jars.is_empty() {
            debug!(
                "Classpath wildcard {raw}: directory {} contains no JARs",
                dir.display()
            );
        } else {
            debug!(
                "Classpath wildcard {raw} -> {} JAR(s) under {}",
                jars.len(),
                dir.display()
            );
        }
        jars
    }

    /// Heuristic: does `path` look like a JDK 9+ jimage file?
    ///
    /// The standard location is `$JAVA_HOME/lib/modules` — a file named
    /// `modules` (no extension) under a `lib` directory. We accept any
    /// file named `modules` that exists and is larger than the jimage
    /// header; the real magic-number check happens inside
    /// [`JImageReader::open`] so callers still get a clean error if the
    /// file is not actually a jimage.
    fn is_likely_jimage(path: &Path) -> bool {
        if !path.exists() {
            return false;
        }
        if path.extension().is_some() {
            return false; // anything with an extension is not `modules`
        }
        match path.file_name().and_then(|n| n.to_str()) {
            Some("modules") => {}
            _ => return false,
        }
        match fs::metadata(path) {
            Ok(meta) => meta.is_file() && meta.len() >= 28, // header size
            Err(_) => false,
        }
    }

    /// Resolve a single classpath token (wildcard, jar-subdir spec,
    /// directory, jmod, jimage, or plain jar/file) into `entries`. Shared by
    /// [`ClassPath::new`]'s top-level path list (`depth=0`) and by a plain
    /// jar's own manifest `Class-Path:` expansion (`depth=parent_depth+1`,
    /// see [`ClassPath::load_jar_data_at_depth`]).
    fn process_classpath_token(
        raw: &str,
        entries: &mut Vec<ClassPathEntry>,
        depth: u32,
        fat_jar_emulation: bool,
        dynamic: bool,
    ) {
        for p in Self::expand_classpath_wildcard(raw) {
            // A URLClassLoader rooted at an archive subdirectory hands us
            // `<archive>!/<prefix>/`. Handle it before interpreting the
            // token as a filesystem path so `.war`/`.ear` archives retain
            // their internal root for both class and resource lookup.
            if let Some((archive, prefix)) = parse_jar_subdir_spec(&p) {
                let path = PathBuf::from(archive);
                if path.exists() {
                    match read_archive_for_classpath_as(&path, dynamic).map(|(data, _)| data) {
                        Ok(data) => {
                            if let Some(entry) = Self::build_nested_jar_from_jar(
                                &path,
                                data.clone(),
                                prefix.trim_end_matches('/'),
                            )
                            .or_else(|| Self::build_nested_directory_from_jar(&path, data, &prefix))
                            {
                                push_classpath_entry(entries, entry);
                            }
                        }
                        Err(e) => {
                            debug!(
                                "Failed to read nested classpath archive {}: {e}",
                                path.display()
                            );
                        }
                    }
                } else {
                    debug!(
                        "Skipping missing nested classpath archive: {}",
                        path.display()
                    );
                }
                continue;
            }
            let path = PathBuf::from(&p);
            if path.is_dir() {
                push_classpath_entry(entries, ClassPathEntry::Directory(path));
            } else if path.extension().is_some_and(|ext| ext == "jmod") && path.exists() {
                match Self::load_jmod(&path) {
                    Ok(entry) => push_classpath_entry(entries, entry),
                    Err(e) => debug!("Failed to read JMOD {}: {e}", path.display()),
                }
            } else if Self::is_likely_jimage(&path) {
                // NEW-5: the JDK 9+ runtime image at `$JAVA_HOME/lib/modules`
                // is a single jimage blob. Detection is by file name
                // (`modules` under any directory) plus a magic-number
                // verification inside `load_jimage`. The explicit file
                // name check lets users write `-cp /path/to/lib/modules`
                // without having to pass a special flag.
                match Self::load_jimage(&path) {
                    Ok(entry) => push_classpath_entry(entries, entry),
                    Err(e) => debug!("Failed to read jimage {}: {e}", path.display()),
                }
            } else if path.is_file() {
                // A URLClassLoader treats every existing file URL as an
                // archive candidate, regardless of its suffix. Hibernate's
                // packaged-bootstrap tests use `.par`/`.war`/`.ear` ZIPs;
                // accepting only `.jar` here made the per-loader resolver
                // silently lose their resources while `add_path` accepted
                // the same URLs. `load_jar_data` fails closed for ordinary
                // non-archive files, so the broader admission is safe.
                match read_archive_for_classpath_as(&path, dynamic) {
                    Ok((data, stamp)) => Self::load_jar_data_at_depth(
                        &path,
                        data,
                        stamp,
                        entries,
                        depth,
                        fat_jar_emulation,
                        dynamic,
                    ),
                    Err(e) => debug!("Failed to read classpath archive {}: {e}", path.display()),
                }
            } else {
                debug!("Skipping non-existent classpath entry: {p}");
            }
        }
    }

    /// Load a JAR from raw bytes, auto-detecting fat JAR structure.
    fn load_jar_data(path: &Path, data: Vec<u8>, entries: &mut Vec<ClassPathEntry>) {
        // No stamp: bytes handed in are never lent.
        Self::load_jar_data_at_depth(
            path,
            ArchiveBacking::from_vec(data),
            None,
            entries,
            0,
            true,
            false,
        );
    }

    /// Compatibility wrapper for legacy callers that still supply a manifest
    /// expansion set. `load_jar_data_at_depth` now owns recursive manifest
    /// processing and its depth guard, so the set is no longer needed here.
    fn load_jar_data_with_manifest_class_path(
        path: &Path,
        data: Vec<u8>,
        entries: &mut Vec<ClassPathEntry>,
        _expanded_manifest_jars: &mut FxHashSet<PathBuf>,
    ) {
        Self::load_jar_data_at_depth(
            path,
            ArchiveBacking::from_vec(data),
            None,
            entries,
            0,
            true,
            false,
        );
    }

    /// Manifest `Class-Path:` expansion recursion cap — guards against a
    /// cyclic chain (jar A's Class-Path names jar B, whose Class-Path names
    /// A again) recursing forever. Real classpaths are never nested this
    /// deep in practice.
    const MAX_CLASS_PATH_MANIFEST_DEPTH: u32 = 16;

    /// [`load_jar_data`], plus honouring a plain (non-fat) JAR's own
    /// manifest `Class-Path:` attribute.
    ///
    /// Per the JAR spec, `Class-Path:` is honoured for ANY jar a
    /// `URLClassLoader`/launcher opens, not just the process's initial
    /// classpath — but until this fix, `vm-cli`'s one-time `--jar <path>`
    /// bootstrap handling (`resolve_class_path`, `vm-cli/src/main.rs`) was
    /// the ONLY caller that expanded it. A "pathing JAR" (a jar containing
    /// no classes, only a manifest `Class-Path:` pointing at the real
    /// dependency jars/dirs — used by e.g. the spring-boot-suite-runner to
    /// dodge Windows' command-line length limit) loaded through this
    /// general-purpose `ClassPath::new` path — the one every ad hoc
    /// `URLClassLoader` (`ModifiedClassPathClassLoader`, custom test
    /// classloaders, etc.) is built from — therefore silently resolved to
    /// an unusable, effectively empty classpath: the pathing jar itself
    /// has no class entries, and its `Class-Path:` was never followed.
    fn load_jar_data_at_depth(
        path: &Path,
        data: ArchiveBacking,
        stamp: Option<OuterArchiveStamp>,
        entries: &mut Vec<ClassPathEntry>,
        depth: u32,
        fat_jar_emulation: bool,
        dynamic: bool,
    ) {
        Self::load_jar_data_at_depth_with(
            path,
            data,
            stamp,
            entries,
            depth,
            fat_jar_emulation,
            dynamic,
            loader_flags().jar_lazy_index,
        );
    }

    /// [`Self::load_jar_data_at_depth`] with the `CRATONVM_JAR_LAZY_INDEX`
    /// decision made by the caller (`lazy_index`), so a test can open one jar
    /// both ways.
    #[allow(clippy::too_many_arguments)]
    fn load_jar_data_at_depth_with(
        path: &Path,
        data: ArchiveBacking,
        stamp: Option<OuterArchiveStamp>,
        entries: &mut Vec<ClassPathEntry>,
        depth: u32,
        fat_jar_emulation: bool,
        dynamic: bool,
        lazy_index: bool,
    ) {
        // Manifest Class-Path chains can form cycles. Avoid mapping and
        // indexing an archive a second time once any entry from that archive
        // has already been published into this ClassPath.
        // One file spelled two ways is one archive (`same_classpath_file`,
        // interpreter round i5 wave 109, lane L5).
        if entries.iter().any(|entry| match entry {
            ClassPathEntry::JarFile { path: loaded, .. } => same_classpath_file(loaded, path),
            ClassPathEntry::NestedDirectory {
                parent_jar: loaded, ..
            }
            | ClassPathEntry::NestedJar {
                parent_jar: loaded, ..
            } => same_classpath_file(loaded, path),
            _ => false,
        }) {
            return;
        }
        let backing = data.clone();
        // The file's stamp for `ClassPath::outer_archive_for` (wave 69): since
        // wave 79 (L5) the metadata of the file whose bytes these are, from
        // the handle that read them (`read_archive_for_classpath_stamped`),
        // not a second `stat` of the path after the read -- a same-length
        // replacement by rename in between left the OLD bytes under the NEW
        // file's stamp, and every lend then served them as current (i71 page).
        // `None` (bytes handed in) is never lent. The length filter stays as
        // a guard.
        let stamp = stamp.filter(|stamp| stamp.len == backing.len() as u64);
        // zt/fp (`CRATONVM_JAR_LAZY_INDEX`): a plain jar the central-directory
        // index models is opened from its directory alone. A jar it does not
        // model, and a fat jar (which extracts through the `ZipArchive`), take
        // the old open below.
        if lazy_index {
            if let Some(index) = crate::jmod_index::JmodCentralIndex::parse(backing.as_ref()) {
                let archive = JarArchive::central(index, backing.clone());
                if Self::push_central_jar(
                    path,
                    archive,
                    &backing,
                    stamp,
                    entries,
                    depth,
                    fat_jar_emulation,
                    dynamic,
                ) {
                    return;
                }
            }
        }
        let cursor = Cursor::new(data);
        match ZipArchive::new(cursor) {
            Ok(mut archive) => {
                // Check for MANIFEST.MF to detect Spring Boot fat JAR
                let manifest = Self::read_manifest(&mut archive);
                // A `Class-Path` token with an unknown URL scheme (a drive
                // spelling `C:/x.jar`) makes JDK 25's `JarLoader.getClassPath`
                // throw, and `URLClassPath.getLoader` then skips the whole jar:
                // none of its classes, resources or `Class-Path` jars are seen
                // (interpreter round i5 wave 107, lane L5).
                if manifest.class_path_drops_jar() {
                    debug!(
                        "JAR {} skipped: its manifest Class-Path has a token with an \
                         unknown URL protocol (HotSpot's MalformedURLException)",
                        path.display()
                    );
                    return;
                }
                let manifest_class_path = manifest.resolve_class_path(path);
                // Without emulation a Spring Boot or web archive is an ordinary
                // JAR, as on HotSpot: its root below, and its manifest `Class-Path:`.
                let is_fat_jar = fat_jar_emulation
                    && (manifest.is_spring_boot() || Self::probe_fat_jar_structure(&mut archive));

                if is_fat_jar {
                    debug!(
                        "Detected fat JAR: {} (Start-Class: {:?})",
                        path.display(),
                        manifest.start_class
                    );
                    Self::extract_fat_jar_entries(path, &mut archive, &backing, &manifest, entries);
                    // Also add the outer JAR itself (for classes at the root,
                    // e.g. the Spring Boot launcher classes in org/springframework/boot/loader/).
                    //
                    // T1.8.2 follow-up (MED #32): we previously `.unwrap()`'d
                    // the re-open. The first `ZipArchive::new` succeeded on
                    // the same byte buffer, so in practice this can only
                    // fail if the underlying `Vec<u8>` is somehow corrupted
                    // between the two opens — but a panic here would kill
                    // the VM before any user code runs. Fat-JAR entries
                    // we already pushed remain valid; we just skip the
                    // root-classes entry and log.
                    let mr = manifest.multi_release;
                    let data = archive.into_inner().into_inner();
                    let root_backing = data.clone();
                    match ZipArchive::new(Cursor::new(data)) {
                        Ok(mut reloaded) => {
                            let entry_index = Self::build_archive_entry_index(&mut reloaded);
                            entries.push(ClassPathEntry::JarFile {
                                path: path.to_path_buf(),
                                archive: JarArchive::zipped(reloaded, root_backing.clone()),
                                backing: root_backing,
                                entry_index,
                                multi_release: mr,
                                versions_cache: Mutex::new(None),
                                signer_cache: OnceLock::new(),
                                stamp,
                                plain_view: OnceLock::new(),
                            });
                        }
                        Err(e) => {
                            debug!(
                                "Failed to re-open fat JAR {} for root-class entry: {e}",
                                path.display()
                            );
                        }
                    }
                } else {
                    let mr = manifest.multi_release;
                    debug!("Loaded JAR: {}", path.display());
                    let entry_index = Self::build_archive_entry_index(&mut archive);
                    entries.push(ClassPathEntry::JarFile {
                        path: path.to_path_buf(),
                        archive: JarArchive::zipped(archive, backing.clone()),
                        backing,
                        entry_index,
                        multi_release: mr,
                        versions_cache: Mutex::new(None),
                        signer_cache: OnceLock::new(),
                        stamp,
                        plain_view: OnceLock::new(),
                    });
                    if depth < Self::MAX_CLASS_PATH_MANIFEST_DEPTH {
                        for token in manifest.resolve_class_path(path) {
                            // A jar a running program put on a loader names
                            // its `Class-Path:` jars for that loader too: open
                            // them the same way (wave 97, L5).
                            Self::process_classpath_token(
                                &token,
                                entries,
                                depth + 1,
                                fat_jar_emulation,
                                dynamic,
                            );
                        }
                    }
                }
            }
            Err(e) => {
                debug!("Failed to open JAR {}: {e}", path.display());
            }
        }
    }

    /// The plain-jar half of [`Self::load_jar_data_at_depth_with`] over a
    /// central-directory index: the same manifest checks, entry index and
    /// `Class-Path:` walk as the `ZipArchive` open, from the index. Returns
    /// `false` (nothing pushed) for a fat jar, which the caller opens the old
    /// way; `true` when the jar was pushed or skipped.
    #[allow(clippy::too_many_arguments)]
    fn push_central_jar(
        path: &Path,
        archive: JarArchive,
        backing: &ArchiveBacking,
        stamp: Option<OuterArchiveStamp>,
        entries: &mut Vec<ClassPathEntry>,
        depth: u32,
        fat_jar_emulation: bool,
        dynamic: bool,
    ) -> bool {
        let manifest = archive.read_manifest();
        if manifest.class_path_drops_jar() {
            debug!(
                "JAR {} skipped: its manifest Class-Path has a token with an \
                 unknown URL protocol (HotSpot's MalformedURLException)",
                path.display()
            );
            return true;
        }
        let Some(index) = archive.central.as_ref() else {
            return false;
        };
        let is_fat_jar = fat_jar_emulation
            && (manifest.is_spring_boot()
                || index.names().iter_all().take(100).any(|name| {
                    name.starts_with("BOOT-INF/") || name.starts_with("WEB-INF/classes/")
                }));
        if is_fat_jar {
            return false;
        }
        debug!("Loaded JAR: {} (central directory only)", path.display());
        let entry_index: FxHashSet<String> =
            index.names().iter_all().map(str::to_string).collect();
        entries.push(ClassPathEntry::JarFile {
            path: path.to_path_buf(),
            archive,
            backing: backing.clone(),
            entry_index,
            multi_release: manifest.multi_release,
            versions_cache: Mutex::new(None),
            signer_cache: OnceLock::new(),
            stamp,
            plain_view: OnceLock::new(),
        });
        if depth < Self::MAX_CLASS_PATH_MANIFEST_DEPTH {
            for token in manifest.resolve_class_path(path) {
                Self::process_classpath_token(&token, entries, depth + 1, fat_jar_emulation, dynamic);
            }
        }
        true
    }

    /// Add one classpath token, including wildcard expansion and any manifest
    /// dependencies discovered while opening archives.
    fn add_classpath_entry(
        raw: &str,
        entries: &mut Vec<ClassPathEntry>,
        expanded_manifest_jars: &mut FxHashSet<PathBuf>,
    ) {
        for p in Self::expand_classpath_wildcard(raw) {
            // A URLClassLoader rooted at an archive subdirectory hands us
            // `<archive>!/<prefix>/`. Handle it before interpreting the token
            // as a filesystem path so `.war`/`.ear` archives retain their
            // internal root for both class and resource lookup.
            if let Some((archive, prefix)) = parse_jar_subdir_spec(&p) {
                let path = PathBuf::from(archive);
                if path.exists() {
                    match read_archive_for_classpath(&path) {
                        Ok(data) => {
                            if let Some(entry) = Self::build_nested_jar_from_jar(
                                &path,
                                data.clone(),
                                prefix.trim_end_matches('/'),
                            )
                            .or_else(|| Self::build_nested_directory_from_jar(&path, data, &prefix))
                            {
                                push_classpath_entry(entries, entry);
                            }
                        }
                        Err(e) => {
                            debug!(
                                "Failed to read nested classpath archive {}: {e}",
                                path.display()
                            );
                        }
                    }
                } else {
                    debug!(
                        "Skipping missing nested classpath archive: {}",
                        path.display()
                    );
                }
                continue;
            }

            let path = PathBuf::from(&p);
            if path.is_dir() {
                push_classpath_entry(entries, ClassPathEntry::Directory(path));
            } else if path.extension().is_some_and(|ext| ext == "jmod") && path.exists() {
                match Self::load_jmod(&path) {
                    Ok(entry) => push_classpath_entry(entries, entry),
                    Err(e) => debug!("Failed to read JMOD {}: {e}", path.display()),
                }
            } else if Self::is_likely_jimage(&path) {
                match Self::load_jimage(&path) {
                    Ok(entry) => push_classpath_entry(entries, entry),
                    Err(e) => debug!("Failed to read jimage {}: {e}", path.display()),
                }
            } else if path.is_file() {
                // URLClassLoader accepts every file URL as an archive
                // candidate. `load_jar_data_with_manifest_class_path` fails
                // closed for ordinary non-archive files.
                match read_archive_for_classpath_stamped(&path) {
                    Ok((data, stamp)) => {
                        Self::load_jar_data_at_depth(&path, data, stamp, entries, 0, true, false)
                    }
                    Err(e) => debug!("Failed to read classpath archive {}: {e}", path.display()),
                }
            } else {
                debug!("Skipping non-existent classpath entry: {p}");
            }
        }
    }

    /// Read `META-INF/MANIFEST.MF` from an archive.
    fn read_manifest<R: Read + std::io::Seek>(archive: &mut ZipArchive<R>) -> ManifestInfo {
        let result = archive
            .by_name("META-INF/MANIFEST.MF")
            .and_then(|mut entry| {
                // Audit-fix #2 (capacity) + V2 (streaming bound): clamp the
                // attacker-controlled declared size AND bound the actual
                // inflate via `read_entry_capped`. The `?` converts the
                // `io::Error` overflow into `ZipError` so the closure stays
                // in the `ZipResult` the `and_then` expects.
                let size = entry.size();
                Ok(read_entry_capped(&mut entry, size)?)
            });
        match result {
            Ok(data) => ManifestInfo::parse(&data),
            Err(_) => ManifestInfo::default(),
        }
    }

    /// Probe for fat JAR structure by checking if any entry starts with
    /// `BOOT-INF/` or `WEB-INF/classes/`.
    fn probe_fat_jar_structure(archive: &mut SharedArchive) -> bool {
        for i in 0..archive.len().min(100) {
            if let Ok(entry) = archive.by_index_raw(i) {
                let name = entry.name().to_string();
                if name.starts_with("BOOT-INF/") || name.starts_with("WEB-INF/classes/") {
                    return true;
                }
            }
        }
        false
    }

    /// Extract nested entries from a Spring Boot fat JAR.
    ///
    /// 1. All `.class` files under `BOOT-INF/classes/` (or custom path from
    ///    MANIFEST) become a `NestedDirectory` entry.
    /// 2. Each `.jar` file under `BOOT-INF/lib/` (or custom path) is extracted
    ///    into memory and added as a `NestedJar` entry.
    fn extract_fat_jar_entries(
        path: &Path,
        archive: &mut SharedArchive,
        backing: &ArchiveBacking,
        manifest: &ManifestInfo,
        entries: &mut Vec<ClassPathEntry>,
    ) {
        let classes_prefix = manifest
            .boot_classes
            .as_deref()
            .unwrap_or("BOOT-INF/classes/");
        let lib_prefix = manifest.boot_lib.as_deref().unwrap_or("BOOT-INF/lib/");

        // Ensure prefixes end with /
        let classes_prefix = if classes_prefix.ends_with('/') {
            classes_prefix.to_string()
        } else {
            format!("{classes_prefix}/")
        };
        let lib_prefix = if lib_prefix.ends_with('/') {
            lib_prefix.to_string()
        } else {
            format!("{lib_prefix}/")
        };

        // Also handle WEB-INF for WAR files
        let war_classes_prefix = "WEB-INF/classes/";

        // Phase 1: Collect BOOT-INF/classes/ entries into a NestedDirectory
        let mut classes_cache: HashMap<String, SharedBytes> = HashMap::new();
        let mut nested_jar_names: Vec<String> = Vec::new();

        for i in 0..archive.len() {
            let name = match archive.by_index_raw(i) {
                Ok(entry) => entry.name().to_string(),
                Err(_) => continue,
            };

            // Audit-fix #3 (zip-slip): refuse to cache any entry whose
            // name escapes the JAR root, contains NUL bytes, uses
            // absolute paths, or smuggles a Windows drive letter.
            // In-memory caches alone do not write to disk, but a
            // poisoned cache namespace (`../etc/passwd`) lets the
            // attacker collide on lookup keys that legitimate code
            // might subsequently use.
            if !is_safe_entry_name(&name) {
                debug!(
                    "Fat JAR {}: rejecting unsafe entry name {name:?}",
                    path.display()
                );
                continue;
            }

            if name.starts_with(&classes_prefix) && name.len() > classes_prefix.len() {
                let relative = &name[classes_prefix.len()..];
                if !relative.is_empty() && !relative.ends_with('/') && is_safe_entry_name(relative)
                {
                    if let Some(data) = Self::find_shared_in_archive_locked(archive, backing, &name)
                    {
                        classes_cache.insert(relative.to_string(), data);
                    }
                }
            } else if name.starts_with(war_classes_prefix) && name.len() > war_classes_prefix.len()
            {
                let relative = &name[war_classes_prefix.len()..];
                if !relative.is_empty() && !relative.ends_with('/') && is_safe_entry_name(relative)
                {
                    if let Some(data) = Self::find_shared_in_archive_locked(archive, backing, &name)
                    {
                        classes_cache.insert(relative.to_string(), data);
                    }
                }
            } else if name.starts_with(&lib_prefix)
                && name.ends_with(".jar")
                && name.len() > lib_prefix.len()
            {
                nested_jar_names.push(name);
            }
        }

        if !classes_cache.is_empty() {
            debug!(
                "Fat JAR {}: extracted {} files from {}",
                path.display(),
                classes_cache.len(),
                classes_prefix
            );
            entries.push(ClassPathEntry::NestedDirectory {
                parent_jar: path.to_path_buf(),
                prefix: classes_prefix.to_string(),
                entries_cache: classes_cache,
            });
        }

        // Phase 2: Extract each nested JAR from BOOT-INF/lib/
        let mut nested_count = 0;
        for jar_name in &nested_jar_names {
            if let Some(jar_data) = Self::find_shared_in_archive_locked(archive, backing, jar_name)
            {
                let nested_backing = ArchiveBacking::Shared(jar_data);
                let cursor = Cursor::new(nested_backing.clone());
                match ZipArchive::new(cursor) {
                    Ok(mut nested_archive) => {
                        let entry_index = Self::build_archive_entry_index(&mut nested_archive);
                        entries.push(ClassPathEntry::NestedJar {
                            parent_jar: path.to_path_buf(),
                            nested_path: jar_name.clone(),
                            archive: Mutex::new(nested_archive),
                            backing: nested_backing,
                            entry_index,
                            signer_cache: OnceLock::new(),
                        });
                        nested_count += 1;
                    }
                    Err(e) => {
                        debug!(
                            "Fat JAR {}: failed to open nested JAR {}: {e}",
                            path.display(),
                            jar_name
                        );
                    }
                }
            }
        }

        if nested_count > 0 {
            debug!(
                "Fat JAR {}: loaded {} nested JARs from {}",
                path.display(),
                nested_count,
                lib_prefix
            );
        }
    }

    /// Returns true if this classpath has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the number of classpath entries.
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Read the MANIFEST.MF from the first JAR on this classpath that has one.
    ///
    /// Useful for the CLI to determine `Start-Class` from a Spring Boot fat JAR.
    /// Read the manifest from a single JAR file without constructing a full ClassPath.
    ///
    /// Returns `None` if the JAR cannot be read as a zip; a JAR with no
    /// `MANIFEST.MF` answers an EMPTY manifest (`ManifestInfo::default()`),
    /// which [`Self::jar_has_manifest_entry`] tells apart.
    pub fn read_jar_manifest(jar_path: &Path) -> Option<ManifestInfo> {
        // Positioned reads of the central directory and the one manifest
        // entry, never a mapping: the jar is any jar a running program handed
        // a loader (`URLClassLoader.getURLs()`'s manifest expansion), which it
        // may truncate or rewrite in place -- an `io::Error` or a CRC mismatch
        // here, `SIGBUS` through a mapping on Unix (interpreter round i5 wave
        // 100, lane L5; `i99-orch-truncated-jar-sigbus-on-linux`). Nothing but
        // the directory and the manifest is read.
        let mut archive = open_archive_unmapped(jar_path)?;
        let info = Self::read_manifest(&mut archive);
        Some(info)
    }

    /// Whether the jar at `jar_path` has a `META-INF/MANIFEST.MF` entry
    /// (`None`: the jar cannot be read as a zip). [`Self::read_jar_manifest`]
    /// answers an empty manifest for a jar with none, which `java.c` tells
    /// apart: no manifest entry is `Error: Invalid or corrupt jarfile <jar>`
    /// (`JLI_ParseManifest`, case-insensitive entry name), a manifest without
    /// `Main-Class` is `LauncherHelper`'s `no main manifest attribute, in
    /// <jar>` (interpreter round i2 wave 60, lane L5; probe
    /// `L5W60LauncherJarManifest`). Read only on the launcher's failure path.
    pub fn jar_has_manifest_entry(jar_path: &Path) -> Option<bool> {
        // The central directory only, through the file (wave 100, lane L5).
        let archive = open_archive_unmapped(jar_path)?;
        let found = archive
            .file_names()
            .any(|name| name.eq_ignore_ascii_case("META-INF/MANIFEST.MF"));
        Some(found)
    }

    pub fn read_first_manifest(&self) -> Option<ManifestInfo> {
        for entry in &self.entries {
            if let ClassPathEntry::JarFile { archive, .. } = entry {
                let info = archive.read_manifest();
                if info.main_class.is_some() || info.start_class.is_some() {
                    return Some(info);
                }
            }
        }
        None
    }

    /// Add a new path entry at runtime (for URLClassLoader and dynamic class loading).
    ///
    /// If `path` is a directory it is added as a `Directory` entry; if it ends
    /// with `.jar` (or `.zip`) and exists, it is opened as a `JarFile` entry.
    /// Fat JARs are auto-detected and their nested entries are extracted.
    /// Silently skips non-existent paths or unreadable JARs.
    ///
    /// Supports the same `dir/*` wildcard expansion as [`ClassPath::new`] so
    /// app servers / launchers that synthesise `lib/*` strings at runtime
    /// (e.g. JBoss module loader, Maven plugin loaders) get every JAR in
    /// the directory instead of silently dropping the entry.
    pub fn add_path(&mut self, path: &str) {
        if self.static_len.is_none() {
            self.static_len = Some(self.entries.len());
        }
        // A path remembered as on no entry may be one now (wave 71).
        self.jar_entry_slots.get_mut().clear();
        self.runtime_image = OnceLock::new();
        *self.dynamic_specs.entry(path.to_string()).or_insert(0) += 1;
        for expanded in Self::expand_classpath_wildcard(path) {
            // DaCapo-style URL handoff: a URL of the form
            // `jar:file:/<jar>!/<prefix>/` extracted by URLClassLoader will
            // arrive here as `<jar>!/<prefix>/` (the `file:` and `jar:`
            // prefixes are stripped by the URLClassLoader native before
            // `add_path` is called). Treat the outer JAR as the resource
            // root, but virtualised at the prefix — so a `findClass` for
            // `Foo` looks inside the JAR at `<prefix>Foo.class` instead of
            // `Foo.class`. This mirrors the JDK's `URLClassLoader` over a
            // `jar:` URL pointing at a subdirectory inside a JAR, which is
            // exactly what DaCapo 9.12-MR1's Harness does at startup
            // (`cl.getResource("harness/")` → URLClassLoader → loadClass
            // `org.dacapo.harness.TestHarness`).
            if let Some((jar_part, prefix)) = parse_jar_subdir_spec(&expanded) {
                let pb = std::path::PathBuf::from(jar_part);
                if pb.exists() {
                    match read_archive_for_dynamic_classpath_stamped(&pb).map(|(data, _)| data) {
                        Ok(data) => {
                            match Self::build_nested_jar_from_jar(
                                &pb,
                                data.clone(),
                                prefix.trim_end_matches('/'),
                            )
                            .or_else(|| Self::build_nested_directory_from_jar(&pb, data, &prefix))
                            {
                                Some(entry) => {
                                    debug!(
                                        "Dynamic classpath: adding nested-dir {}!/{}",
                                        pb.display(),
                                        prefix
                                    );
                                    push_classpath_entry(&mut self.entries, entry);
                                }
                                None => debug!(
                                    "Dynamic classpath: nested-dir {}!/{} \
                                     yielded no entries (skipping)",
                                    pb.display(),
                                    prefix
                                ),
                            }
                        }
                        Err(e) => debug!("Dynamic classpath: failed to read {}: {e}", pb.display()),
                    }
                    continue;
                }
                debug!(
                    "Dynamic classpath: nested-dir spec {} references missing JAR",
                    expanded
                );
                continue;
            }
            let pb = std::path::PathBuf::from(&expanded);
            if pb.is_dir() {
                debug!("Dynamic classpath: adding directory {expanded}");
                push_classpath_entry(&mut self.entries, ClassPathEntry::Directory(pb));
            } else if pb.extension().is_some_and(|e| e == "jmod") && pb.exists() {
                match Self::load_jmod(&pb) {
                    Ok(entry) => push_classpath_entry(&mut self.entries, entry),
                    Err(e) => debug!("Dynamic classpath: failed to read JMOD {expanded}: {e}"),
                }
            } else if pb.is_file() {
                // Any existing non-directory classpath entry is treated as a
                // JAR/ZIP archive regardless of file extension, matching the
                // real JDK's `URLClassPath` (a `file:` URL that does not end in
                // '/' becomes a `JarLoader`). Hibernate's packaged-archive tests
                // put `.par`/`.war`/`.ear` jars on a `URLClassLoader`; gating
                // archive loading on a `.jar`/`.zip` extension made every class
                // and resource inside them invisible (PackagedEntityManagerTest:
                // orm.xml/cfg.xml not found, entities not discovered). `load_jar_data`
                // fails closed (pushes no entry) when the bytes are not a valid
                // zip, so a stray non-archive file on the classpath is harmless.
                //
                // Use the same stable owned read as startup classpath JARs
                // (read into memory on Windows, wave 96).
                //
                // `CRATONVM_CLASS_PATH_LIVE_JAR_NO_REREAD` (default off;
                // interpreter round i6 wave 115, lane L5, proposal
                // `i100-L5-proposal-loader-area-directions` item 1): a jar this
                // class path already has an entry for is not read again. Its
                // entry is never replaced -- `push_classpath_entry` drops the
                // new one as a duplicate of the same file -- so the read, the
                // central-directory parse and the manifest walk (all under the
                // class-manager WRITE lock, `extend_application_classpath`)
                // only produced what is then thrown away. Every further
                // `URLClassLoader` over a live jar (a test suite's loader per
                // test) paid a whole read of it. What is skipped too: a
                // manifest `Class-Path` jar the jar names NOW and did not name
                // when its entry was made (a jar rewritten in place under a
                // live loader), which the re-read would have appended.
                if loader_flags().class_path_live_jar_no_reread
                    && self.entries.iter().any(|entry| match entry {
                        ClassPathEntry::JarFile { path, .. } => same_classpath_file(path, &pb),
                        _ => false,
                    })
                {
                    debug!("Dynamic classpath: {expanded} already an entry (not re-read)");
                    continue;
                }
                match read_archive_for_dynamic_classpath_stamped(&pb) {
                    Ok((data, stamp)) => {
                        Self::load_jar_data_at_depth(
                            &pb,
                            data,
                            stamp,
                            &mut self.entries,
                            0,
                            self.fat_jar_emulation,
                            true,
                        );
                    }
                    Err(e) => {
                        debug!("Dynamic classpath: failed to read {expanded}: {e}");
                    }
                }
            } else {
                debug!("Dynamic classpath: skipping non-existent entry {expanded}");
            }
        }
    }

    /// Retract a spec previously handed to [`Self::add_path`].
    ///
    /// This is the other half of the `URLClassLoader.close()` contract: a closed
    /// loader must stop serving classes and resources it had not already
    /// loaded. Already-defined classes stay defined, exactly as on HotSpot —
    /// `close()` shuts the loader's `URLClassPath`, it does not unload anything.
    ///
    /// Returns the number of classpath entries actually removed. Zero is the
    /// normal answer for a spec that another live loader still holds (the use
    /// count from [`Self::add_path`] has not reached zero), for one that was
    /// never added, and for one whose files did not resolve to any entry.
    pub fn remove_path(&mut self, path: &str) -> usize {
        match self.dynamic_specs.get_mut(path) {
            Some(count) if *count > 1 => {
                *count -= 1;
                return 0;
            }
            Some(_) => {
                self.dynamic_specs.remove(path);
            }
            None => return 0,
        }
        let Some(static_len) = self.static_len else {
            return 0;
        };
        // Every filesystem root this spec could have contributed, in the same
        // two forms `add_path` accepts: a plain path (possibly wildcarded) and
        // the `<jar>!/<prefix>/` nested form, whose root is the outer JAR.
        let mut roots: Vec<PathBuf> = Vec::new();
        for expanded in Self::expand_classpath_wildcard(path) {
            if let Some((jar_part, _prefix)) = parse_jar_subdir_spec(&expanded) {
                roots.push(PathBuf::from(jar_part));
            } else {
                roots.push(PathBuf::from(&expanded));
            }
        }
        if roots.is_empty() {
            return 0;
        }
        let before = self.entries.len();
        let mut index = 0usize;
        self.entries.retain(|entry| {
            let position = index;
            index += 1;
            // Startup roots are never retractable — see `static_len`.
            if position < static_len {
                return true;
            }
            !roots.iter().any(|root| entry_derives_from(entry, root))
        });
        let removed = before - self.entries.len();
        if removed > 0 {
            // A retracted root may have been the only source of a name that is
            // now absent again, and vice versa for anything memoized while it
            // was present.
            self.canonicalize_cache.lock().clear();
            self.root_canonical_cache.lock().clear();
            // The directory index is keyed on the root path, and a root that
            // comes back (a `URLClassLoader` closed and reopened over the same
            // directory) must not be served from the set we walked last time.
            // Clearing it costs one relazy walk per surviving root; keeping a
            // stale set would let a retracted-then-recreated directory answer
            // from contents that no longer exist.
            self.dir_index.lock().clear();
            // Entry indices past a retracted one moved (wave 71).
            self.jar_entry_slots.get_mut().clear();
            self.runtime_image = OnceLock::new();
            debug!("Dynamic classpath: retracted {removed} entrie(s) for {path}");
        }
        removed
    }

    /// Build a [`ClassPathEntry::NestedJar`] from an archive entry that is
    /// itself a valid ZIP/JAR. This is the classpath form of Spring Boot's
    /// `jar:nested:<outer>/!<inner.jar>!/` URL: the trailing marker denotes
    /// the nested archive root, rather than a directory named `inner.jar`.
    ///
    /// Returning `None` for a non-archive entry lets the caller fall back to
    /// the ordinary nested-directory treatment for `<outer>!/<prefix>/`.
    fn build_nested_jar_from_jar(
        path: &Path,
        data: ArchiveBacking,
        nested_path: &str,
    ) -> Option<ClassPathEntry> {
        if nested_path.is_empty() || !is_safe_entry_name(nested_path) {
            return None;
        }
        let backing = data.clone();
        let mut outer = ZipArchive::new(Cursor::new(data)).ok()?;
        let nested_backing = ArchiveBacking::Shared(Self::find_shared_in_archive_locked(
            &mut outer,
            &backing,
            nested_path,
        )?);
        let mut archive = ZipArchive::new(Cursor::new(nested_backing.clone())).ok()?;
        let entry_index = Self::build_archive_entry_index(&mut archive);
        Some(ClassPathEntry::NestedJar {
            parent_jar: path.to_path_buf(),
            nested_path: nested_path.to_string(),
            archive: Mutex::new(archive),
            backing: nested_backing,
            entry_index,
            signer_cache: OnceLock::new(),
        })
    }

    /// The exact central-directory entry names of the archive stored at
    /// `nested_path` inside the archive at `outer` (Spring Boot's
    /// `jar:nested:<outer>/!<nested_path>!/` URL), or `None` when either
    /// archive cannot be read: the same index a [`ClassPathEntry::NestedJar`]
    /// keeps, without its archive or the manifest `Class-Path:` expansion of
    /// [`Self::new`] (the JDK's generic `URLClassPath$Loader` that serves such
    /// a URL has no local class path).
    ///
    /// For the `URLClassPath$Loader` walk index (`native-builtins`
    /// `classloader::nested_jar_walk_index_says_absent`; interpreter round i2
    /// wave 54, lane L5): a name absent here is a name Spring's
    /// `JarUrlConnection` answers "not found" for.
    pub fn nested_archive_entry_names(
        outer: &Path,
        nested_path: &str,
    ) -> Option<FxHashSet<String>> {
        if nested_path.is_empty() || !is_safe_entry_name(nested_path) {
            return None;
        }
        // The ONE nested entry read through the file, never a mapping of the
        // outer jar: `outer` is any fat jar a running program named, which it
        // may truncate or rewrite in place while this reads it -- a short read
        // here, `SIGBUS` through a mapping (interpreter round i5 wave 100, lane
        // L5; `i99-orch-truncated-jar-sigbus-on-linux`).
        let nested_backing =
            ArchiveBacking::from_vec(read_nested_entry_from_file(outer, nested_path)?);
        Self::nested_entry_names_of(nested_backing)
    }

    /// The walk index's name set of the nested jar whose bytes are
    /// `nested_backing` ([`Self::nested_archive_entry_names`]'s second half,
    /// shared with [`Self::nested_archive_entry_index_in`]).
    fn nested_entry_names_of(nested_backing: ArchiveBacking) -> Option<FxHashSet<String>> {
        let mut archive = ZipArchive::new(Cursor::new(nested_backing)).ok()?;
        let mut names = Self::build_archive_entry_index(&mut archive);
        // A Multi-Release jar serves `META-INF/versions/<N>/<name>` as
        // `<name>` through its versioned view (Spring's `NestedJarFile` opened
        // at the runtime version; spring-core 7 keeps classes ONLY there), so
        // every versioned entry is ALSO present under its unversioned name,
        // whatever `<N>` is: the index may over-answer "present" (the JDK body
        // then runs), never "absent". Interpreter round i2 wave 54, lane L5
        // follow-up (the `--jdk-only` fat jar's
        // `NoClassDefFoundError: ...ClassFileMetadataReaderFactory`).
        let versioned: Vec<String> = names
            .iter()
            .filter_map(|n| {
                let rest = n.strip_prefix("META-INF/versions/")?;
                let (version, name) = rest.split_once('/')?;
                (!version.is_empty()
                    && version.bytes().all(|b| b.is_ascii_digit())
                    && !name.is_empty())
                .then(|| name.to_string())
            })
            .collect();
        names.extend(versioned);
        Some(names)
    }

    /// [`Self::nested_archive_entry_names`] with the outer archive's stamp
    /// (read first), for a memoised walk index that must notice a rewritten
    /// fat jar ([`NestedEntryNameIndex::outer_unchanged`]). `None` exactly
    /// when [`Self::nested_archive_entry_names`] is (a path that is not a
    /// file is no readable archive either). Interpreter round i2 wave 65,
    /// lane L5.
    pub fn nested_archive_entry_index(
        outer: &Path,
        nested_path: &str,
    ) -> Option<NestedEntryNameIndex> {
        let stamp = OuterArchiveStamp::read(outer)?;
        let names = Self::nested_archive_entry_names(outer, nested_path)?;
        Some(NestedEntryNameIndex {
            outer: outer.to_path_buf(),
            stamp,
            names,
        })
    }

    /// [`Self::nested_archive_entry_index`] over an outer archive already
    /// opened ([`OuterArchive`]): no mapping and no outer central-directory
    /// parse of its own, and the index carries `outer`'s stamp (the caller
    /// checks [`OuterArchive::unchanged`] before it hands a held `outer` in).
    /// The same names as [`Self::nested_archive_entry_names`] over the same
    /// file. Interpreter round i2 wave 66, lane L5
    /// (`CRATONVM_NESTED_JAR_SHARED_OUTER`).
    pub fn nested_archive_entry_index_in(
        outer: &OuterArchive,
        nested_path: &str,
    ) -> Option<NestedEntryNameIndex> {
        let names = Self::nested_entry_names_of(outer.nested_backing(nested_path)?)?;
        Some(NestedEntryNameIndex {
            outer: outer.path.clone(),
            stamp: outer.stamp,
            names,
        })
    }

    /// The archive stored at `nested_path` inside the archive at `outer`
    /// (Spring Boot's `jar:nested:<outer>/!<nested_path>!/` URL), opened to
    /// serve its entries' bytes, or `None` when either archive cannot be read.
    /// With `map`, the outer archive is mapped as a class-path archive is
    /// ([`read_archive_for_classpath`]); a stored nested jar (Spring Boot's
    /// layout) is a range of that mapping, a deflated one is inflated once.
    /// Without it only the nested jar is read, through a file read of the
    /// outer one, and nothing stays mapped.
    ///
    /// `map` is the caller's word that `outer` is one of the VM's own class
    /// path archives, which the class path already maps on the same
    /// immutability assumption. A jar an application loader was handed may be
    /// rewritten in place while the VM still holds it: a mapping would turn a
    /// concurrent truncation into `SIGBUS` where HotSpot's `ZipFile` throws.
    /// Interpreter round i2 wave 64, lane L5 follow-up
    /// (`docs/feature-designs/class-resolution-and-caching.md`).
    ///
    /// For the nested-jar bytes index (`native-builtins`
    /// `classloader::nested_jar_resource_get_bytes`,
    /// `CRATONVM_NESTED_JAR_BYTES_INDEX`; interpreter round i2 wave 63, lane
    /// L5): `Resource.getBytes()` of the JDK's generic `URLClassPath$Loader`
    /// answered from here instead of through Spring's Java stream stack.
    pub fn open_nested_archive(
        outer: &Path,
        nested_path: &str,
        map: bool,
    ) -> Option<NestedArchiveEntries> {
        if nested_path.is_empty() || !is_safe_entry_name(nested_path) {
            return None;
        }
        let stamp = OuterArchiveStamp::read(outer)?;
        let nested_backing = if map {
            let backing = read_archive_for_classpath(outer).ok()?;
            let mut outer_archive = ZipArchive::new(Cursor::new(backing.clone())).ok()?;
            ArchiveBacking::Shared(Self::find_shared_in_archive_locked(
                &mut outer_archive,
                &backing,
                nested_path,
            )?)
        } else {
            ArchiveBacking::from_vec(read_nested_entry_from_file(outer, nested_path)?)
        };
        Self::nested_entries_of(outer.to_path_buf(), stamp, nested_backing)
    }

    /// [`Self::open_nested_archive`] over an outer archive already opened
    /// ([`OuterArchive`]): the nested jar is a range of `outer`'s bytes (or
    /// inflated once), with `outer`'s stamp. Interpreter round i2 wave 66,
    /// lane L5 (`CRATONVM_NESTED_JAR_SHARED_OUTER`).
    pub fn open_nested_archive_in(
        outer: &OuterArchive,
        nested_path: &str,
    ) -> Option<NestedArchiveEntries> {
        let nested_backing = outer.nested_backing(nested_path)?;
        Self::nested_entries_of(outer.path.clone(), outer.stamp, nested_backing)
    }

    /// The archive at `path` as THIS class path already holds it, lent as an
    /// [`OuterArchive`] -- the entry's own bytes (its mapping, or its copy)
    /// and a clone of its parsed central directory, so nothing is opened,
    /// mapped, read or parsed -- for the nested-jar walk and bytes indexes
    /// (interpreter round i2 wave 69, lane L5,
    /// `CRATONVM_NESTED_JAR_CLASS_PATH_OUTER`;
    /// `i66-L5-proposal-reuse-the-class-path-parse-of-a-fat-jar`): the
    /// `java -jar app.jar` shape, where the Spring Boot launcher's
    /// `jar:nested:` URLs name the VM's own `-jar` archive.
    ///
    /// `None` -- the caller then opens the file as before -- unless a
    /// [`ClassPathEntry::JarFile`] of this class path has `path` (equal, or
    /// equal once both are canonicalised), it kept its load-time stamp, and
    /// the file at `path` still has that stamp now (one `stat`): a fat jar
    /// rewritten since the class path loaded it is never answered from the
    /// class path's bytes. The lent value carries the load-time stamp, so
    /// every row built from it is re-checked against the file as a row built
    /// from [`OuterArchive::open`] is.
    pub fn outer_archive_for(&self, path: &Path) -> Option<OuterArchive> {
        let mut canonical: Option<Option<PathBuf>> = None;
        for entry in &self.entries {
            let ClassPathEntry::JarFile {
                path: entry_path,
                archive,
                backing,
                stamp,
                ..
            } = entry
            else {
                continue;
            };
            let same = entry_path.as_path() == path || {
                let asked = canonical.get_or_insert_with(|| fs::canonicalize(path).ok());
                asked.is_some() && fs::canonicalize(entry_path).ok().as_ref() == asked.as_ref()
            };
            if !same {
                continue;
            }
            let stamp = (*stamp)?;
            if OuterArchiveStamp::read(path) != Some(stamp) {
                return None;
            }
            let archive = archive.lock().clone();
            return Some(OuterArchive {
                path: path.to_path_buf(),
                stamp,
                backing: backing.clone(),
                archive: Mutex::new(archive),
            });
        }
        None
    }

    /// [`Self::nested_archive_entry_index_in`] over an outer central
    /// directory held WITHOUT the outer bytes ([`OuterArchiveIndex`]): the
    /// nested jar is read now with a bounded positioned read, parsed and
    /// dropped. The same names as [`Self::nested_archive_entry_names`] over
    /// the same file; the index carries `outer`'s stamp. Interpreter round
    /// i2 wave 67, lane L5 (`CRATONVM_NESTED_JAR_OUTER_INDEX`).
    pub fn nested_archive_entry_index_in_index(
        outer: &OuterArchiveIndex,
        nested_path: &str,
    ) -> Option<NestedEntryNameIndex> {
        let bytes = outer.read_nested(nested_path)?;
        let names = Self::nested_entry_names_of(ArchiveBacking::from_vec(bytes))?;
        Some(NestedEntryNameIndex {
            outer: outer.path.clone(),
            stamp: outer.stamp,
            names,
        })
    }

    /// [`Self::open_nested_archive_in`] over an [`OuterArchiveIndex`]: the
    /// nested jar is an owned copy read now (a stored one is copied, a
    /// deflated one inflated), never a range of an outer mapping, with
    /// `outer`'s stamp. Interpreter round i2 wave 67, lane L5
    /// (`CRATONVM_NESTED_JAR_OUTER_INDEX`).
    pub fn open_nested_archive_in_index(
        outer: &OuterArchiveIndex,
        nested_path: &str,
    ) -> Option<NestedArchiveEntries> {
        let bytes = outer.read_nested(nested_path)?;
        Self::nested_entries_of(
            outer.path.clone(),
            outer.stamp,
            ArchiveBacking::from_vec(bytes),
        )
    }

    /// [`Self::open_nested_archive_in_index`] that keeps only the nested
    /// jar's DIRECTORY when the nested jar is stored in the outer one (Spring
    /// Boot's layout): its entries are then byte ranges of the outer file,
    /// and [`NestedArchiveEntries::entry_bytes`] reads each one at the ask
    /// (open, stamp check, one positioned read of the local header and data
    /// through a small read-ahead, CRC check, stamp check, close) instead of
    /// copying it out of a held copy of the nested jar. A deflated nested jar
    /// is opened as [`Self::open_nested_archive_in_index`] opens it (an owned
    /// copy). The same entry index and plain-view answer as that copy would
    /// have; the row carries `outer`'s stamp. Interpreter round i2 wave 68,
    /// lane L5 (`CRATONVM_NESTED_JAR_BYTES_DIRECTORY`;
    /// `i67-L5-proposal-a-bytes-index-row-without-the-nested-bytes`).
    pub fn open_nested_archive_directory_in_index(
        outer: &OuterArchiveIndex,
        nested_path: &str,
    ) -> Option<NestedArchiveEntries> {
        let (state, archive, records) = match outer.open_nested_window(nested_path)? {
            NestedWindowRead::NotStored => {
                return Self::open_nested_archive_in_index(outer, nested_path);
            }
            NestedWindowRead::Window {
                state,
                archive,
                records,
            } => (state, archive, records),
        };
        // The names straight from the parsed directory (no local-header
        // read per entry, which through the window would be one positioned
        // read each); a name whose local header is bad is then indexed but
        // fails its read, answering `None` as the copy's absent name does.
        let entry_index: FxHashSet<String> = archive.file_names().map(str::to_string).collect();
        let plain_view = plain_view_of(&entry_index, archive.len(), records);
        Some(NestedArchiveEntries {
            outer: outer.path.clone(),
            stamp: outer.stamp,
            archive: NestedArchiveReader::Window {
                state,
                archive: Mutex::new(archive),
            },
            entry_index,
            plain_view,
        })
    }

    /// [`Self::open_nested_archive`]'s second half: the nested jar whose
    /// bytes are `nested_backing`, stamped with its outer archive's `stamp`.
    fn nested_entries_of(
        outer: PathBuf,
        stamp: OuterArchiveStamp,
        nested_backing: ArchiveBacking,
    ) -> Option<NestedArchiveEntries> {
        let mut archive = ZipArchive::new(Cursor::new(nested_backing)).ok()?;
        let entry_index = Self::build_archive_entry_index(&mut archive);
        // A duplicated name makes "which entry" a question the two readers
        // could answer differently: no plain view then ([`plain_view_of`]).
        let plain_view = plain_view_of(
            &entry_index,
            archive.len(),
            shared_archive_records(&archive),
        );
        Some(NestedArchiveEntries {
            outer,
            stamp,
            archive: NestedArchiveReader::Held(Mutex::new(archive)),
            entry_index,
            plain_view,
        })
    }

    /// The archive at `path` itself -- a plain jar a JDK
    /// `URLClassPath$JarLoader` opened as a `java.util.jar.JarFile` -- opened
    /// to serve its entries' bytes through the same plain view as
    /// [`Self::open_nested_archive`] (the "outer" stamp is `path`'s own).
    /// With `map` (as for [`Self::open_nested_archive`]: `path` is one of the
    /// VM's own class path archives) it is mapped as a class-path archive is
    /// ([`read_archive_for_classpath`]) on every host but Windows (read into
    /// memory there, see the body), so a stored entry is a copy out of the
    /// mapping and a deflated one one native inflate; without it, or on
    /// Windows, the jar is read into memory. `None` when `path` is not a
    /// readable archive.
    ///
    /// A second mapping of a jar the class path may already map: the
    /// `ClassPathEntry` mapping is not reachable from the native that asks,
    /// and a mapping costs no copy.
    ///
    /// For the jar-loader bytes index (`native-builtins`
    /// `classloader::jar_loader_resource_bytes`,
    /// `CRATONVM_JAR_LOADER_BYTES_INDEX`; interpreter round i2 wave 64, lane
    /// L5): `Resource.getBytes()` of the JDK's `URLClassPath$JarLoader`
    /// answered from here instead of the interpreted `ZipFile` stream loop.
    pub fn open_jar_archive(path: &Path, map: bool) -> Option<NestedArchiveEntries> {
        let stamp = OuterArchiveStamp::read(path)?;
        // Windows: a mapped file cannot be deleted, and this copy outlives
        // the Java `JarFile` (a test that closes its `URLClassLoader` and
        // deletes the jar would fail where HotSpot succeeds), so read it into
        // memory and release the handle instead.
        let backing = if cfg!(windows) || !map {
            ArchiveBacking::from_vec(read_file_for_classpath(path).ok()?)
        } else {
            read_archive_for_classpath(path).ok()?
        };
        let mut archive = ZipArchive::new(Cursor::new(backing)).ok()?;
        let entry_index = Self::build_archive_entry_index(&mut archive);
        let plain_view = plain_view_of(
            &entry_index,
            archive.len(),
            shared_archive_records(&archive),
        );
        Some(NestedArchiveEntries {
            outer: path.to_path_buf(),
            stamp,
            archive: NestedArchiveReader::Held(Mutex::new(archive)),
            entry_index,
            plain_view,
        })
    }

    /// [`Self::open_jar_archive`] over the archive the application class path
    /// already holds, lent by [`Self::outer_archive_for`]: the row reads the
    /// class path entry's own bytes (its mapping, or its copy) through a
    /// clone of its parsed central directory, so nothing is opened, mapped,
    /// read or parsed -- the second mapping and central-directory parse of a
    /// `-cp` jar that `open_jar_archive(path, true)` makes is not made
    /// (interpreter round i2 wave 70, lane L5,
    /// `CRATONVM_JAR_LOADER_CLASS_PATH_ARCHIVE`;
    /// `i69-L5-proposal-lend-the-class-path-archive-to-the-jar-loader-bytes-index`).
    ///
    /// The row has the same entry index and plain view as
    /// `open_jar_archive` builds over the same bytes (the index is rebuilt
    /// from the lent directory, in memory: no I/O), and carries the lent
    /// value's stamp -- the class path's LOAD-TIME stamp, which
    /// `outer_archive_for` compared with the file now -- so a row's
    /// [`NestedArchiveEntries::outer_unchanged`] re-check on every hit asks
    /// whether the file is still the one the class path loaded, never a
    /// fresher one. Nothing new is held open or mapped: the row shares the
    /// mapping the class path holds for the VM's life anyway.
    pub fn open_jar_archive_lent(outer: OuterArchive) -> Option<NestedArchiveEntries> {
        let OuterArchive {
            path,
            stamp,
            archive,
            ..
        } = outer;
        let mut archive = archive.into_inner();
        let entry_index = Self::build_archive_entry_index(&mut archive);
        let plain_view = plain_view_of(
            &entry_index,
            archive.len(),
            shared_archive_records(&archive),
        );
        Some(NestedArchiveEntries {
            outer: path,
            stamp,
            archive: NestedArchiveReader::Held(Mutex::new(archive)),
            entry_index,
            plain_view,
        })
    }

    /// The bytes of the FILE entry `name` of the jar at `path`, answered
    /// straight from THIS class path's own [`ClassPathEntry::JarFile`] for
    /// that file -- its bytes, its parsed directory, its entry index, its
    /// load-time stamp -- with no row, no clone of the directory and no copy
    /// of the index (interpreter round i3 wave 71, lane L5,
    /// `CRATONVM_JAR_LOADER_CLASS_PATH_DIRECT`;
    /// `i70-L5-proposal-serve-class-path-jar-bytes-without-a-row`).
    ///
    /// * [`ClassPathJarBytes::NotHeld`](cratonvm_native_api::ClassPathJarBytes::NotHeld)
    ///   unless an entry has `path` (equal, then equal once both are
    ///   canonicalised -- remembered per path, see `jar_entry_slots`), kept
    ///   its load-time stamp, and the file at `path` still has that stamp
    ///   now (one `stat`): the rule of [`Self::outer_archive_for`]. The
    ///   caller then asks as it would without the class path.
    /// * `Body` when the file is held unchanged but the answer is not
    ///   certain, by exactly the rules of a jar-loader row built over the
    ///   same archive ([`Self::open_jar_archive_lent`],
    ///   [`NestedArchiveEntries::entry_bytes_matching`]): no plain view
    ///   (computed once per entry), `name` empty, a directory, absent, or
    ///   shadowed by `name + "/"`, a central-directory CRC-32 or size other
    ///   than `crc32` / `size`, or a read that is not clean.
    /// * `Bytes` otherwise: CRC-checked by the `zip` reader, of exactly the
    ///   central directory's size.
    ///
    /// The read takes the entry's own `ZipArchive` lock, the one the class
    /// path's class lookups take.
    pub fn jar_entry_bytes_matching(
        &self,
        path: &Path,
        name: &str,
        crc32: u32,
        size: u64,
    ) -> cratonvm_native_api::ClassPathJarBytes {
        use cratonvm_native_api::ClassPathJarBytes as Answer;
        let Some(slot) = self.jar_entry_slot(path) else {
            return Answer::NotHeld;
        };
        let Some(ClassPathEntry::JarFile {
            archive,
            entry_index,
            stamp,
            plain_view,
            ..
        }) = self.entries.get(slot)
        else {
            return Answer::NotHeld;
        };
        let Some(stamp) = *stamp else {
            return Answer::NotHeld;
        };
        if OuterArchiveStamp::read(path) != Some(stamp) {
            return Answer::NotHeld;
        }
        // A duplicated name makes "which entry" a question two readers could
        // answer differently: no plain view then (as `open_jar_archive_lent`).
        let plain = *plain_view.get_or_init(|| archive.plain_view(entry_index));
        if !plain
            || name.is_empty()
            || name.ends_with('/')
            || !entry_index.contains(name)
            || entry_index.contains(&format!("{name}/"))
        {
            return Answer::Body;
        }
        match archive.read_checked(name, crc32, size) {
            Some(bytes) => Answer::Bytes(bytes),
            None => Answer::Body,
        }
    }

    /// The index in `entries` of the [`ClassPathEntry::JarFile`] whose path
    /// is `path` -- the first equal one, else the first equal once both are
    /// canonicalised (the root's canonical form memoised by
    /// [`Self::canonicalize_root`]) -- remembered per `path` in
    /// `jar_entry_slots`, the absence too.
    fn jar_entry_slot(&self, path: &Path) -> Option<usize> {
        if let Some(slot) = self.jar_entry_slots.lock().get(path) {
            return *slot;
        }
        let jar_paths = || {
            self.entries
                .iter()
                .enumerate()
                .filter_map(|(i, entry)| match entry {
                    ClassPathEntry::JarFile {
                        path: entry_path, ..
                    } => Some((i, entry_path)),
                    _ => None,
                })
        };
        let slot = jar_paths()
            .find(|(_, entry_path)| entry_path.as_path() == path)
            .or_else(|| {
                let asked = fs::canonicalize(path).ok()?;
                jar_paths().find(|(_, entry_path)| {
                    self.canonicalize_root(entry_path)
                        .is_ok_and(|canon| canon == asked)
                })
            })
            .map(|(i, _)| i);
        let mut guard = self.jar_entry_slots.lock();
        if guard.len() >= JAR_ENTRY_SLOT_CACHE_CAP {
            guard.clear();
        }
        guard.insert(path.to_path_buf(), slot);
        slot
    }

    /// Build a [`ClassPathEntry::NestedDirectory`] by extracting every entry
    /// in `archive` whose name begins with `prefix` (which itself must end
    /// with `/`, mirroring how `BOOT-INF/classes/` is structured). Returns
    /// `None` if the archive cannot be opened or no entries match.
    ///
    /// Used by `add_path` for the DaCapo-style `<jar>!/<prefix>/` URL form.
    fn build_nested_directory_from_jar(
        path: &Path,
        data: ArchiveBacking,
        prefix: &str,
    ) -> Option<ClassPathEntry> {
        let backing = data.clone();
        let cursor = Cursor::new(data);
        let mut archive = match ZipArchive::new(cursor) {
            Ok(a) => a,
            Err(e) => {
                debug!("Failed to open JAR for nested-dir {}: {e}", path.display());
                return None;
            }
        };
        let mut entries_cache: HashMap<String, SharedBytes> = HashMap::new();
        let total = archive.len();
        for i in 0..total {
            let name = match archive.by_index_raw(i) {
                Ok(entry) => entry.name().to_string(),
                Err(_) => continue,
            };
            if !is_safe_entry_name(&name) {
                continue;
            }
            if !name.starts_with(prefix) || name.len() <= prefix.len() {
                continue;
            }
            let relative = &name[prefix.len()..];
            if relative.is_empty() || relative.ends_with('/') {
                // Skip the directory marker itself and any subdirectories.
                continue;
            }
            if !is_safe_entry_name(relative) {
                continue;
            }
            if let Some(bytes) = Self::find_shared_in_archive_locked(&mut archive, &backing, &name)
            {
                entries_cache.insert(relative.to_string(), bytes);
            }
        }
        if entries_cache.is_empty() {
            return None;
        }
        Some(ClassPathEntry::NestedDirectory {
            parent_jar: path.to_path_buf(),
            prefix: prefix.to_string(),
            entries_cache,
        })
    }

    /// Find and read a class file by its binary name (e.g., `java/lang/Object`).
    ///
    /// Class names are validated to prevent path traversal attacks. Names containing
    /// `..` or starting with `/` are rejected.
    pub fn find_class(&self, class_name: &str) -> Result<SharedBytes, ClassFileError> {
        // Reject path traversal attempts, absolute paths, and suspicious patterns.
        // Class binary names use '/' as separator and must not escape the classpath root.
        if !is_safe_class_name(class_name) {
            return Err(ClassFileError::ClassNotFound {
                class_name: class_name.to_string(),
            });
        }

        // PASS 1 consults `dir_index` and never stats a directory that cannot
        // hold this class; PASS 2 is the historical stat-per-directory scan,
        // and runs only when pass 1 found nothing anywhere. See the
        // `dir_index` field doc for the measurement and for why the second
        // pass is what keeps this exactly as correct as the single pass it
        // replaces: a class created after a directory was indexed is invisible
        // to pass 1 and still found by pass 2.
        match self.find_class_pass(class_name, true) {
            Ok(bytes) => return Ok(bytes),
            Err(ClassFileError::ClassNotFound { .. }) => {}
            Err(other) => return Err(other),
        }
        match self.find_class_pass(class_name, false) {
            Err(ClassFileError::ClassNotFound {
                class_name: missing,
            }) => {
                // zs s4/rp: a class only the JDK's runtime image holds.
                match self.runtime_image_only_class(class_name) {
                    Some(bytes) => Ok(bytes),
                    None => Err(ClassFileError::ClassNotFound {
                        class_name: missing,
                    }),
                }
            }
            other => other,
        }
    }

    /// One search of the entry list. `use_dir_index` selects pass 1 (skip a
    /// directory whose index says the class is absent) or pass 2 (probe every
    /// directory with a real `exists()`, as before the index existed).
    fn find_class_pass(
        &self,
        class_name: &str,
        use_dir_index: bool,
    ) -> Result<SharedBytes, ClassFileError> {
        let relative_path = format!("{}.class", class_name);
        // Executable WAR / Spring-Boot WAR support: when a class is requested
        // by its binary name, also probe the common archive-internal class
        // roots used by web/fat-jar containers. Jenkins's WAR is an
        // *executable* WAR with its Main-Class at the archive root
        // (`executable/Main.class`), not under `WEB-INF/classes/`; other
        // WARs put application classes under `WEB-INF/classes/`; Spring
        // Boot fat JARs use `BOOT-INF/classes/`. We try root first (the
        // canonical location for ordinary JARs) and only fall back to
        // the prefixed variants — this is a no-op for plain JARs because
        // those prefixed entries simply don't exist.
        // p5-cp: built on the first archive entry only; a boot class path (the
        // runtime image) never needs them, and the three strings were paid on
        // every lookup.
        let candidates_cell: std::cell::OnceCell<[String; 3]> = std::cell::OnceCell::new();
        let candidates = || {
            candidates_cell.get_or_init(|| {
                [
                    relative_path.clone(),
                    format!("WEB-INF/classes/{}", relative_path),
                    format!("BOOT-INF/classes/{}", relative_path),
                ]
            })
        };
        // The prefixed roots are archive emulation; HotSpot looks only at the
        // entry the class name spells.
        let archive_candidates = || -> &[String] {
            if self.fat_jar_emulation {
                candidates()
            } else {
                &candidates()[..1]
            }
        };

        for entry in &self.entries {
            match entry {
                ClassPathEntry::Directory(dir) => {
                    // The syscall this whole index exists to avoid. `Some(false)`
                    // is a definite absence at index-build time, and pass 2 is
                    // what covers the case where that has since changed.
                    if use_dir_index && self.dir_index_contains(dir, &relative_path) == Some(false)
                    {
                        continue;
                    }
                    let full_path = dir.join(Path::new(&relative_path));
                    if full_path.exists() {
                        if !use_dir_index {
                            // Pass 2 found what pass 1's index denied: the
                            // directory has grown. Drop the stale index so the
                            // next lookup rebuilds rather than falling through
                            // to pass 2 forever.
                            self.dir_index_invalidate(dir);
                        }
                        // Audit-fix #5: symlink-traversal check is now
                        // fail-CLOSED. If we cannot canonicalize either
                        // the classpath root or the resolved file we
                        // refuse to read — silently proceeding allowed
                        // a symlinked `Object.class -> /etc/passwd` to
                        // be served as classfile bytes.
                        //
                        // Round 5 audit fix (MED): routed through
                        // `canonicalize_cached` so repeat probes of the
                        // same paths skip the per-call syscall — the
                        // classpath root is identical across every
                        // probe and resolved-file paths repeat once a
                        // class is reloaded by JVMTI / instrumentation.
                        let canon_dir = self.canonicalize_root(dir).map_err(|e| {
                            debug!(
                                "Refusing to load {class_name}: cannot canonicalize \
                                 classpath root {}: {e}",
                                dir.display()
                            );
                            ClassFileError::IoError {
                                class_name: class_name.to_string(),
                                source: e,
                            }
                        })?;
                        let canon_path = self.canonicalize_cached(&full_path).map_err(|e| {
                            debug!(
                                "Refusing to load {class_name}: cannot canonicalize \
                                 resolved path {}: {e}",
                                full_path.display()
                            );
                            ClassFileError::IoError {
                                class_name: class_name.to_string(),
                                source: e,
                            }
                        })?;
                        if !canon_path.starts_with(&canon_dir) {
                            debug!(
                                "Path traversal blocked: {} escapes {}",
                                canon_path.display(),
                                canon_dir.display()
                            );
                            return Err(ClassFileError::ClassNotFound {
                                class_name: class_name.to_string(),
                            });
                        }
                        debug!("Found class {class_name} at {}", full_path.display());
                        // Use the stable classpath read helper so a class file
                        // that changes mid-read is rejected as invalid input.
                        return read_file_for_classpath(&full_path)
                            .map(SharedBytes::from)
                            .map_err(|e| ClassFileError::IoError {
                                class_name: class_name.to_string(),
                                source: e,
                            });
                    }
                }
                // Pass 2 exists only for the directory staleness window; every
                // archive kind answers from an in-memory index that pass 1
                // already consulted, so repeating them would be pure waste.
                _ if !use_dir_index => continue,
                ClassPathEntry::JarFile {
                    archive,
                    multi_release,
                    versions_cache,
                    entry_index,
                    backing,
                    ..
                } => {
                    for candidate in archive_candidates() {
                        let found = if *multi_release {
                            Self::find_shared_in_multi_release_archive(
                                archive,
                                backing,
                                versions_cache,
                                entry_index,
                                candidate,
                                self.multi_release_feature(),
                            )
                        } else {
                            Self::find_shared_in_indexed_archive(
                                archive,
                                backing,
                                entry_index,
                                candidate,
                            )
                        };
                        if let Some(data) = found {
                            debug!("Found class {class_name} in JAR (entry: {candidate})");
                            return Ok(data);
                        }
                    }
                }
                ClassPathEntry::NestedDirectory { entries_cache, .. } => {
                    if let Some(data) = entries_cache.get(&relative_path) {
                        debug!("Found class {class_name} in nested directory");
                        return Ok(data.clone());
                    }
                }
                ClassPathEntry::NestedJar {
                    archive,
                    entry_index,
                    nested_path,
                    backing,
                    ..
                } => {
                    for candidate in archive_candidates() {
                        if let Some(data) = Self::find_shared_in_indexed_archive(
                            archive,
                            backing,
                            entry_index,
                            candidate,
                        ) {
                            debug!(
                                "Found class {class_name} in nested JAR {nested_path} \
                                 (entry: {candidate})"
                            );
                            return Ok(data);
                        }
                    }
                }
                ClassPathEntry::JmodFile {
                    path,
                    class_entry_index,
                    archive,
                    backing,
                    ..
                } => {
                    // zs s4/rp: the JDK's runtime image is authoritative for
                    // the classes of its own JMODs (`RuntimeImageClasses`).
                    if class_entry_index.contains(&relative_path) {
                        if let Some(data) = self.runtime_image_class_for_jmod(path, class_name) {
                            return Ok(data);
                        }
                    }
                    // O(1) name probe against the central-directory index;
                    // only a hit pays a deflate, and only for this one entry.
                    if let Some(data) =
                        Self::jmod_class_bytes(archive, backing, class_entry_index, &relative_path)
                    {
                        debug!("Found class {class_name} in JMOD {}", path.display());
                        return Ok(data);
                    }
                }
                ClassPathEntry::JImageFile {
                    path,
                    reader,
                    class_to_module,
                    ..
                } => {
                    // Two paths:
                    //   1. Fast path — the class_to_module index was built
                    //      at load time, so one HashMap lookup tells us
                    //      exactly which module owns the class.
                    //   2. Fallback — if the class isn't in the index
                    //      (edge case: entry added after load, or a
                    //      synthesized class), iterate known modules.
                    // The candidate is confirmed by `find_class` itself
                    // (the image's own lookup); `Ok(None)` below is then
                    // also the answer for a class the package index
                    // routed to a module that does not hold it.
                    if let Some(module) = class_to_module.candidate_module(class_name) {
                        // p5-cp: borrowed from the image and copied once,
                        // into the shared buffer (a `Vec` first was a
                        // second copy).
                        match reader.find_class_bytes(module, class_name) {
                            Ok(Some(bytes)) => {
                                debug!(
                                    "Found class {class_name} in jimage {} (module {module})",
                                    path.display()
                                );
                                return Ok(SharedBytes::from(bytes));
                            }
                            Ok(None) => {
                                // Index said module owns it, but the
                                // perfect-hash lookup disagrees — treat as
                                // a broken image and fall through so the
                                // caller sees a clean ClassNotFound.
                            }
                            Err(e) => {
                                return Err(ClassFileError::IoError {
                                    class_name: class_name.to_string(),
                                    source: std::io::Error::new(
                                        std::io::ErrorKind::InvalidData,
                                        format!("jimage: {e}"),
                                    ),
                                });
                            }
                        }
                    }
                }
            }
        }

        Err(ClassFileError::ClassNotFound {
            class_name: class_name.to_string(),
        })
    }

    /// Is `relative_path` present under the `Directory` entry `dir`, according
    /// to [`ClassPath::dir_index`]?
    ///
    /// `Some(false)` is the answer worth having: it lets `find_class` skip a
    /// `stat` for a directory that cannot hold this class. `Some(true)` means
    /// the file was there when the index was built and the caller should go on
    /// to the ordinary probe (which re-checks existence anyway). `None` means
    /// there is no usable index — an unreadable or oversized directory — and
    /// the caller must probe.
    ///
    /// Builds the index on first use, under the map lock. Two threads racing
    /// on the same cold directory can both walk it; the second insert wins and
    /// the sets are equal, so the race costs one redundant walk and nothing
    /// else. Holding the lock across the walk instead would serialise every
    /// other directory's first lookup behind it.
    fn dir_index_contains(&self, dir: &Path, relative_path: &str) -> Option<bool> {
        if let Some(index) = self.dir_index.lock().get(dir) {
            return index.as_ref().map(|set| set.contains(relative_path));
        }
        let built = Self::build_dir_index(dir);
        let answer = built.as_ref().map(|set| set.contains(relative_path));
        self.dir_index
            .lock()
            .insert(dir.to_path_buf(), built.clone());
        answer
    }

    /// Drop the cached index for `dir` so the next lookup rebuilds it.
    ///
    /// Called when the stat-based fallback finds a file the index said was
    /// absent, i.e. the directory has grown since the walk. One rebuild then
    /// serves every later lookup, instead of the fallback running forever.
    fn dir_index_invalidate(&self, dir: &Path) {
        self.dir_index.lock().remove(dir);
    }

    /// Walk `dir` and collect every file's path relative to it, in classpath
    /// form (`/` separators, no leading slash).
    ///
    /// Returns `None` when the directory cannot be read or holds more than
    /// [`DIR_INDEX_MAX_ENTRIES`] files — both mean "no index", and every
    /// lookup falls back to the syscall.
    ///
    /// Symlinks ARE followed, and that is deliberate rather than permissive.
    /// A symlinked entry left out of the index is a pass-1 miss that pass 2
    /// resolves — and pass 2's success invalidates the index, so the NEXT
    /// class under that symlink rebuilds the whole directory. Skipping
    /// symlinks would therefore trade one `stat` per lookup for one full
    /// directory walk per lookup, which is worse than the defect. Following
    /// them costs a real `metadata` call only on the entries that are
    /// symlinks; `file_type` answers the other 99.9% for free, out of the
    /// `FIND_DATA` on Windows and `d_type` on Linux.
    ///
    /// Following symlinks means a directory cycle is reachable, so the walk is
    /// bounded on BOTH axes: [`DIR_INDEX_MAX_ENTRIES`] files and
    /// `DIR_INDEX_MAX_DIRS` directories. Hitting either abandons the index
    /// (`None`), and every lookup on that entry falls back to the syscall —
    /// the pre-index behaviour, never a hang.
    ///
    /// Indexing a symlink target is not a security decision: the read path
    /// canonicalises and fail-closed refuses anything that escapes the root
    /// (see `find_class`), and that check is unchanged. An index entry only
    /// says "worth probing".
    fn build_dir_index(dir: &Path) -> Option<Arc<FxHashSet<Box<str>>>> {
        let mut set: FxHashSet<Box<str>> = FxHashSet::default();
        let mut stack = vec![(dir.to_path_buf(), String::new())];
        let mut dirs_seen = 0usize;
        while let Some((current, prefix)) = stack.pop() {
            dirs_seen += 1;
            if dirs_seen > DIR_INDEX_MAX_DIRS {
                return None;
            }
            let reader = std::fs::read_dir(&current).ok()?;
            for entry in reader.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let mut rel = String::with_capacity(prefix.len() + 1 + name.len());
                rel.push_str(&prefix);
                if !prefix.is_empty() {
                    rel.push('/');
                }
                rel.push_str(&name);
                let Ok(kind) = entry.file_type() else {
                    return None;
                };
                // Only a symlink needs the follow-through `metadata` stat.
                let kind = if kind.is_symlink() {
                    match entry.metadata() {
                        Ok(md) => md.file_type(),
                        // A dangling link is neither; skip it.
                        Err(_) => continue,
                    }
                } else {
                    kind
                };
                if kind.is_dir() {
                    stack.push((entry.path(), rel));
                } else if kind.is_file() {
                    if set.len() >= DIR_INDEX_MAX_ENTRIES {
                        return None;
                    }
                    set.insert(rel.into_boxed_str());
                }
            }
        }
        Some(Arc::new(set))
    }

    /// Find the filesystem path of the classpath entry that holds the given
    /// class.  Returns `Some(path)` where `path` is a `file:`-style path to
    /// the containing JAR or directory — the same thing HotSpot returns in
    /// `ProtectionDomain.getCodeSource().getLocation().getPath()`.
    ///
    /// For a bare directory, returns the directory's absolute path with a
    /// trailing slash.  For JARs (flat or nested), returns the JAR file's
    /// absolute path (not wrapped in `jar:!/`).  For JMOD/jimage modules,
    /// returns `None` — JDK internals aren't user-visible code.
    pub fn find_class_source_path(&self, class_name: &str) -> Option<String> {
        // Audit-fix #6: match `find_class`'s full validation set (NUL
        // bytes, leading slash, backslash, drive letter, dot-dot,
        // relative-dir prefixes). The previous truncated check let
        // `..\\Object` or `C:\Foo` reach the JAR-name lookups below.
        if !is_safe_class_name(class_name) {
            return None;
        }
        // Two passes, for the reason `find_class` has two: this runs once per
        // class DEFINE (the origin census, and `defineClass`'s CodeSource), so
        // its own stat-per-directory scan was a second copy of the same
        // 111-directories x 1102-classes cost. Indexing `find_class` alone took
        // netty's first-touch 2195 ms -> 1299 ms and left 1332 ms of it here.
        self.find_class_source_path_pass(class_name, true)
            .or_else(|| self.find_class_source_path_pass(class_name, false))
    }

    fn find_class_source_path_pass(&self, class_name: &str, use_dir_index: bool) -> Option<String> {
        let relative_path = format!("{}.class", class_name);
        for entry in &self.entries {
            match entry {
                ClassPathEntry::Directory(dir) => {
                    if use_dir_index && self.dir_index_contains(dir, &relative_path) == Some(false)
                    {
                        continue;
                    }
                    let full_path = dir.join(Path::new(&relative_path));
                    if full_path.exists() {
                        if !use_dir_index {
                            self.dir_index_invalidate(dir);
                        }
                        return Some(
                            dir.to_string_lossy()
                                .trim_end_matches(['/', '\\'])
                                .to_string()
                                + "/",
                        );
                    }
                }
                // Pass 2 is only for the directory staleness window.
                _ if !use_dir_index => continue,
                ClassPathEntry::JarFile {
                    archive,
                    multi_release,
                    versions_cache,
                    entry_index,
                    path,
                    ..
                } => {
                    let found = if *multi_release {
                        Self::find_in_multi_release_archive(
                            archive,
                            versions_cache,
                            Some(entry_index),
                            &relative_path,
                            self.multi_release_feature(),
                        )
                        .is_some()
                    } else {
                        Self::find_in_indexed_archive(archive, entry_index, &relative_path)
                            .is_some()
                    };
                    if found {
                        return Some(path.to_string_lossy().into_owned());
                    }
                }
                ClassPathEntry::NestedDirectory {
                    parent_jar,
                    entries_cache,
                    ..
                } => {
                    if entries_cache.contains_key(&relative_path) {
                        return Some(parent_jar.to_string_lossy().into_owned());
                    }
                }
                ClassPathEntry::NestedJar {
                    parent_jar,
                    archive,
                    entry_index,
                    nested_path,
                    ..
                } => {
                    if Self::find_in_indexed_archive(archive, entry_index, &relative_path).is_some()
                    {
                        // Return "$parent_jar!/$nested_path" style (JAR-in-JAR)
                        // so the caller can distinguish nested location
                        // layouts; Quarkus uses the outer jar path for its
                        // appRoot heuristic, so just return the outer JAR
                        // with the nested path appended after an exclamation
                        // (informational only — matches HotSpot's behaviour
                        // for fat JARs).
                        let outer = parent_jar.to_string_lossy();
                        return Some(format!("{outer}!/{nested_path}"));
                    }
                }
                ClassPathEntry::JmodFile {
                    path,
                    class_entry_index,
                    ..
                } => {
                    // Existence only — never inflate for a code-source query.
                    if class_entry_index.contains(&relative_path) {
                        return Some(path.to_string_lossy().into_owned());
                    }
                }
                ClassPathEntry::JImageFile { .. } => {
                    // jimage contains modules — not user-visible code.
                }
            }
        }
        None
    }

    /// Find the code source for a given class: returns the containing JAR /
    /// directory URL and the DER-encoded X.509 certificates extracted from
    /// every **verified** signer in the JAR's `META-INF/*.RSA|.DSA|.EC`
    /// signature blocks (if any).
    ///
    /// The returned URL uses `file:` form for directories and JAR paths,
    /// matching HotSpot's `CodeSource.getLocation()`. The certificate
    /// vector holds the leaf X.509 cert DER for each PKCS#7 signer
    /// whose `messageDigest` authenticated attribute matched the
    /// corresponding `.SF` file's SHA-X digest — see
    /// [`crate::jar_signer::verify_signer_block`].  Signer blocks that
    /// fail verification contribute **no certificates** (so
    /// `Class.getCodeSource().getCertificates()` returns empty rather
    /// than opaque attacker bytes).
    ///
    /// Returns `None` if the class isn't on this classpath or lives in a
    /// JMOD/jimage module (JDK internals have no user-visible code source).
    ///
    /// # TRUST BOUNDARY — what a non-empty certificate vector means
    ///
    /// This is the API a Java caller ultimately sees, via
    /// `Class.getCodeSource().getCertificates()`, and the point at which
    /// application code is most likely to conclude "this class came from a
    /// trusted publisher". A non-empty vector means **all** of the
    /// following held (see `docs/security/signed-jar-trust.md`):
    ///
    ///   * a `META-INF/*.RSA|.DSA|.EC` signer block verified against its
    ///     `.SF` companion and chained to an anchor in the process trust
    ///     store — [`crate::jar_signer::verify_signer_block`];
    ///   * that `.SF` committed to this archive's exact `MANIFEST.MF`;
    ///   * every entry the manifest declares a digest for matched its bytes;
    ///   * **this specific class entry** is one the manifest committed to,
    ///     and the bytes about to be loaded still hash to the signed digest
    ///     (re-checked per class in [`Self::certs_for_signed_class`]).
    ///
    /// It does **not** mean the signer's certificate is unrevoked (no
    /// CRL/OCSP is consulted), nor that name-constraint or policy
    /// processing was performed. And an **empty** vector is not evidence of
    /// tampering: an unsigned JAR, a directory classpath entry, and a host
    /// with no trust anchors configured all produce the same empty result.
    /// Callers must fail closed on empty, never infer a reason from it.
    pub fn find_class_code_source_info(&self, class_name: &str) -> Option<(String, Vec<Vec<u8>>)> {
        // Audit-fix #6: match `find_class` / `find_class_source_path`'s full
        // validation set (NUL bytes, leading slash, backslash, drive letter,
        // dot-dot, relative-dir prefixes). The previous truncated check let
        // `..\\Object` or `C:\Foo` reach the JAR-name lookups below.
        if !is_safe_class_name(class_name) {
            return None;
        }
        // Two passes, same rule as `find_class` / `find_class_source_path`:
        // this is on `defineClass`'s CodeSource path, so its directory scan
        // was a third copy of the per-class stat sweep.
        self.find_class_code_source_info_pass(class_name, true)
            .or_else(|| self.find_class_code_source_info_pass(class_name, false))
    }

    fn find_class_code_source_info_pass(
        &self,
        class_name: &str,
        use_dir_index: bool,
    ) -> Option<(String, Vec<Vec<u8>>)> {
        let relative_path = format!("{}.class", class_name);
        for entry in &self.entries {
            match entry {
                ClassPathEntry::Directory(dir) => {
                    if use_dir_index && self.dir_index_contains(dir, &relative_path) == Some(false)
                    {
                        continue;
                    }
                    let full_path = dir.join(Path::new(&relative_path));
                    if full_path.exists() {
                        if !use_dir_index {
                            self.dir_index_invalidate(dir);
                        }
                        // Directories are never signed.
                        let abs = self
                            .canonicalize_root(dir)
                            .unwrap_or_else(|_| dir.to_path_buf());
                        let p = abs.to_string_lossy().replace('\\', "/");
                        let p = p.strip_prefix("//?/").unwrap_or(&p).to_string();
                        let p = p.trim_start_matches('/').trim_end_matches('/').to_string();
                        return Some((format!("file:/{}/", encode_path_for_url(&p)), Vec::new()));
                    }
                }
                // Pass 2 is only for the directory staleness window.
                _ if !use_dir_index => continue,
                ClassPathEntry::JarFile {
                    archive,
                    multi_release,
                    versions_cache,
                    path,
                    signer_cache,
                    entry_index,
                    ..
                } => {
                    let found = if *multi_release {
                        Self::find_in_multi_release_archive(
                            archive,
                            versions_cache,
                            Some(entry_index),
                            &relative_path,
                            self.multi_release_feature(),
                        )
                        .is_some()
                    } else {
                        Self::find_in_indexed_archive(archive, entry_index, &relative_path)
                            .is_some()
                    };
                    if found {
                        let abs = self
                            .canonicalize_root(path)
                            .unwrap_or_else(|_| path.clone());
                        let p = abs.to_string_lossy().replace('\\', "/");
                        let p = p.strip_prefix("//?/").unwrap_or(&p).to_string();
                        let p = p.trim_start_matches('/').to_string();
                        // Report P1 (perf): verify the signer blocks at most once
                        // per archive, then reuse the cached signing state.
                        let info =
                            signer_cache.get_or_init(|| {
                                if archive.lacks_signer_blocks() {
                                    return JarSignerInfo {
                                        chain: Vec::new(),
                                        signed_entries: HashMap::new(),
                                    };
                                }
                                Self::extract_jar_signer_blocks(archive)
                            });
                        // V3 (unsigned-entry attack): attach the signer chain
                        // ONLY if this exact class entry is committed-to by the
                        // verified manifest and its bytes still match. A class
                        // present in a signed JAR but absent from the manifest
                        // (or whose bytes were swapped post-sign) is unsigned.
                        // For multi-release JARs the *served* entry (possibly
                        // `META-INF/versions/<N>/<class>`) is the one that must
                        // be signed, so resolve it before checking.
                        let mr_cache = if *multi_release {
                            Some(versions_cache)
                        } else {
                            None
                        };
                        let certs =
                            Self::certs_for_signed_class(
                                info,
                                archive,
                                &relative_path,
                                mr_cache,
                                self.multi_release_feature(),
                            );
                        return Some((format!("file:/{}", encode_path_for_url(&p)), certs));
                    }
                }
                ClassPathEntry::NestedDirectory {
                    parent_jar,
                    entries_cache,
                    ..
                } => {
                    if entries_cache.contains_key(&relative_path) {
                        let p = parent_jar.to_string_lossy().replace('\\', "/");
                        let p = p.trim_start_matches('/').to_string();
                        return Some((format!("file:/{}", encode_path_for_url(&p)), Vec::new()));
                    }
                }
                ClassPathEntry::NestedJar {
                    parent_jar,
                    archive,
                    nested_path,
                    signer_cache,
                    entry_index,
                    ..
                } => {
                    if Self::find_in_indexed_archive(archive, entry_index, &relative_path).is_some()
                    {
                        let outer = parent_jar.to_string_lossy().replace('\\', "/");
                        let outer = outer.trim_start_matches('/').to_string();
                        // Report P1 (perf): verify the signer blocks at most once
                        // per nested archive, then reuse the cached signing state.
                        let info =
                            signer_cache.get_or_init(|| Self::extract_jar_signer_blocks(archive));
                        // V3 (unsigned-entry attack): per-class cert attachment —
                        // see the matching `JarFile` arm above. Nested JARs are
                        // not searched multi-release here, so pass `None`.
                        let certs =
                            Self::certs_for_signed_class(
                                info,
                                archive,
                                &relative_path,
                                None,
                                self.multi_release_feature(),
                            );
                        return Some((
                            format!("jar:file:/{}!/{nested_path}", encode_path_for_url(&outer)),
                            certs,
                        ));
                    }
                }
                ClassPathEntry::JmodFile { .. } | ClassPathEntry::JImageFile { .. } => {
                    // JDK internals — no user-visible code source.
                }
            }
        }
        None
    }

    /// Walk a JAR archive for every `META-INF/*.RSA`, `META-INF/*.DSA`,
    /// and `META-INF/*.EC` signature block, parse each as PKCS#7
    /// SignedData, and return the DER bytes of every **verified** leaf
    /// X.509 certificate.  Verification is performed by
    /// [`crate::jar_signer::verify_signer_block`] — it parses the
    /// SignedData container, extracts the embedded cert chain, and
    /// confirms that the signer's `messageDigest` authenticated
    /// attribute equals the SHA-X digest of the matching `*.SF` file.
    ///
    /// **Security note.**  Until this commit, the function returned the
    /// raw signer-block bytes verbatim and the call chain stored them as
    /// `CodeSource.certificates`.  PKCS#7 SignedData blobs are not
    /// X.509 certificates — `Class.getCodeSource().getCertificates()`
    /// was therefore handing attacker-controlled opaque bytes back to
    /// callers (e.g. policy `signedBy` filters) and treating them as
    /// trusted signer identities.  We now decode the structure and
    /// return only the real cert DER on successful integrity check.
    ///
    /// An empty result means either:
    ///   * the JAR is unsigned, or
    ///   * every signer block failed verification (parse error, missing
    ///     `.SF` companion, digest mismatch, ...). In that case
    ///     `Class.getCodeSource().getCertificates()` will be empty /
    ///     null — **never garbage** — which matches what HotSpot does
    ///     for a JAR that fails `jarsigner -verify`.
    ///
    /// # TRUST BOUNDARY
    ///
    /// Public-key verification of the SignerInfo signature and full
    /// certification-path construction to a trust anchor **are** performed
    /// (the older `TODO` here, which said neither was, is obsolete — see
    /// `jar_signer.rs` module docs and `docs/security/signed-jar-trust.md`).
    ///
    /// `chain` carries only the certificates on the *validated path*:
    /// [`crate::jar_signer::verify_signer_block`] narrows the
    /// attacker-supplied CMS `certificates` set to the certs it actually
    /// checked, so an extra certificate appended to a legitimately signed
    /// JAR cannot be surfaced here as one of the signer's.
    ///
    /// What remains unproven for a non-empty `chain`: **revocation** (no
    /// CRL/OCSP), name constraints, and certificate policies. And note the
    /// two-stage gate — a non-empty `chain` is archive-level; it is
    /// [`Self::certs_for_signed_class`] that decides whether any given
    /// class entry is entitled to it.
    fn extract_jar_signer_blocks(archive: &Mutex<SharedArchive>) -> JarSignerInfo {
        let mut guard = archive.lock();
        // Collect every safe META-INF entry name once; we need both the
        // `*.SF` companions (to feed into the integrity check) and the
        // `*.RSA|.DSA|.EC` signer blocks.
        let all_names: Vec<String> = (0..guard.len())
            .filter_map(|i| guard.by_index_raw(i).ok().map(|e| e.name().to_string()))
            .filter(|n| {
                // Audit-fix #3 (zip-slip): refuse to treat traversal /
                // absolute-path entry names as signer blocks. A
                // malicious JAR could otherwise smuggle a signature
                // block under `../META-INF/key.RSA` that gets attached
                // to the wrong CodeSource.
                is_safe_entry_name(n)
            })
            .collect();

        // Partition into signer blocks and a lookup map for `.SF` companions.
        let mut signer_block_names: Vec<String> = Vec::new();
        // Map from uppercase stem (e.g. "META-INF/FOO") to original
        // `.SF` entry name. This lets us pair `META-INF/foo.RSA` with
        // `META-INF/FOO.SF` regardless of letter case.
        let mut sf_by_stem: HashMap<String, String> = HashMap::new();
        for n in &all_names {
            let upper = n.to_ascii_uppercase();
            if !upper.starts_with("META-INF/") {
                continue;
            }
            if upper.ends_with(".RSA") || upper.ends_with(".DSA") || upper.ends_with(".EC") {
                signer_block_names.push(n.clone());
            } else if let Some(stem) = upper.strip_suffix(".SF") {
                sf_by_stem.insert(stem.to_string(), n.clone());
            }
        }

        let mut out: Vec<Vec<u8>> = Vec::new();
        // V3: union of every entry the bound manifest committed to (across
        // all verified signer blocks). Only classes appearing here are
        // eligible to inherit `out` (the signer chain).
        let mut signed_entries: HashMap<String, (crate::jar_signer::DigestAlg, Vec<u8>)> =
            HashMap::new();
        for name in signer_block_names {
            // Read signer block bytes (clamped against zip-bomb sizes,
            // streaming-bounded by `read_entry_capped` — V2).
            let block = match guard.by_name(&name).and_then(|mut e| {
                let size = e.size();
                Ok(read_entry_capped(&mut e, size)?)
            }) {
                Ok(d) if !d.is_empty() => d,
                _ => continue,
            };

            // Locate the matching `.SF`. The stem is everything before
            // the final `.` of the entry name; we look it up case-
            // insensitively to be compatible with archivers that
            // produce mixed-case filenames.
            let upper = name.to_ascii_uppercase();
            let stem_upper = match upper.rsplit_once('.') {
                Some((stem, _ext)) => stem.to_string(),
                None => continue,
            };
            let sf_name = match sf_by_stem.get(&stem_upper) {
                Some(n) => n.clone(),
                None => {
                    debug!(
                        "jar signer: signer block {} has no matching .SF — skipping",
                        name
                    );
                    continue;
                }
            };
            let sf_bytes = match guard.by_name(&sf_name).and_then(|mut e| {
                let size = e.size();
                Ok(read_entry_capped(&mut e, size)?)
            }) {
                Ok(d) => d,
                Err(_) => continue,
            };

            // Verify self-consistency AND chain to trust-store anchor.
            // Task #40 wires the process-wide default trust store into
            // this gate — a JAR whose leaf doesn't chain to any anchor
            // (including every self-signed signer) ends up reported as
            // unsigned, matching the HotSpot "fails verify" behaviour.
            // On failure we silently drop the block.
            let trust_store = crate::jar_signer::default_trust_store();
            if let Some(vs) = crate::jar_signer::verify_signer_block(&block, &sf_bytes, trust_store)
            {
                // V1: the signature/`.SF` check above only binds the `.SF`
                // to the signer. The full jarsigner trust chain is
                // signature -> .SF -> MANIFEST.MF -> per-entry digest ->
                // bytes. Without the last two links, an attacker can swap a
                // signed JAR's class body (leaving MANIFEST/.SF/.RSA intact)
                // and still surface the original signer's certificate.
                // `verify_signed_entries` re-reads MANIFEST.MF and every
                // entry it commits to, computes the named digest, and
                // rejects the whole signer block on any mismatch — so the
                // certs are dropped (CodeSource reported as unsigned),
                // matching HotSpot's "fails verify" behaviour.
                if let Some(entries) = Self::verify_signed_entries(&mut guard, &sf_bytes) {
                    for cert in vs.chain {
                        out.push(cert);
                    }
                    // V3: record exactly which entries this verified signer
                    // committed to, so cert attachment can be gated per class.
                    for (entry_name, alg, expected) in entries {
                        signed_entries.entry(entry_name).or_insert((alg, expected));
                    }
                } else {
                    debug!(
                        "jar signer: signer block {} verified but a manifest \
                         entry digest did not match — dropping certs",
                        name
                    );
                }
            }
        }
        // V3: if no signer block fully verified, `out` is empty; the
        // `signed_entries` map is then irrelevant (no chain to attach) and is
        // returned empty too. An archive with a verified chain but a class
        // absent from `signed_entries` will be reported unsigned per class.
        JarSignerInfo {
            chain: out,
            signed_entries,
        }
    }

    /// V3 (unsigned-entry attack, JAR spec §"Signature Validation"):
    /// decide whether the archive-level signer `chain` may be attached to
    /// the specific class entry `relative_path`.
    ///
    /// The chain is returned **only** when `relative_path` is one of the
    /// entries the verified manifest committed to (recorded in
    /// `info.signed_entries`) AND the class's *current* bytes still hash to
    /// the digest the signer authenticated. Any other case — the entry is
    /// not named in the manifest, has no digest, or its bytes were swapped
    /// after signing — is treated as **unsigned** and yields an empty cert
    /// list, so `Class.getCodeSource().getCertificates()` is empty/null,
    /// matching HotSpot's per-entry `CodeSigner` behaviour.
    ///
    /// Re-hashing here (rather than trusting the one-time
    /// `verify_signed_entries` pass) is cheap — a single SHA over the class
    /// we are about to load anyway — and is robust against any in-memory
    /// cache staleness; it also means the per-archive `info` can be cached
    /// while the decision stays per class.
    ///
    /// `mr_versions` is `Some(versions_cache)` for a `Multi-Release: true`
    /// JAR. In that case the *served* entry may be a
    /// `META-INF/versions/<N>/<class>` override, and it is THAT entry name
    /// (not the base `relative_path`) the signer must have committed to —
    /// otherwise a versioned override could be smuggled in unsigned while
    /// inheriting the base entry's certs. We resolve the served name with
    /// the same descending search `find_in_multi_release_archive` uses.
    ///
    /// # TRUST BOUNDARY: this is the per-entry gate
    ///
    /// Archive-level verification is necessary but nowhere near sufficient.
    /// "The JAR is signed" and "this class is signed" are different claims,
    /// and only the second one licenses attaching `info.chain`. Every path
    /// out of this function that is not the final `info.chain.clone()`
    /// returns an **empty** vector — no chain, no signer, unsigned — and
    /// that is the fail-closed default for: an unverified archive, an entry
    /// the manifest never named, an entry the manifest named without a
    /// digest, an unreadable entry, a bytes-vs-digest mismatch, and (for a
    /// multi-release JAR) a versioned override the signer did not commit
    /// to. The last of those matters on its own: without resolving the
    /// *served* entry name first, an unsigned `META-INF/versions/<N>/`
    /// override would inherit the base entry's certificates.
    fn certs_for_signed_class(
        info: &JarSignerInfo,
        archive: &(impl ArchiveRead + ?Sized),
        relative_path: &str,
        mr_versions: Option<&Mutex<Option<Arc<BTreeSet<u32>>>>>,
        max: u32,
    ) -> Vec<Vec<u8>> {
        // Unsigned JAR / failed verification: nothing to attach.
        if info.chain.is_empty() {
            return Vec::new();
        }

        // Resolve the entry name that actually serves this class. For a
        // multi-release JAR the highest present versioned override wins; the
        // base entry is the fallback. This mirrors
        // `find_in_multi_release_archive` so the entry we digest-check is the
        // one we hand to the classloader.
        let served_name: String = match mr_versions {
            Some(versions_cache) => {
                let present = Self::ensure_versions_cache(archive, versions_cache);
                let mut resolved: Option<String> = None;
                for &ver in present.range(versioned_lookup_range(relative_path, max)).rev() {
                    let versioned = format!("META-INF/versions/{ver}/{relative_path}");
                    // Existence only — the bytes are re-read below once the
                    // manifest has committed to the resolved name, so
                    // inflating here would decompress the entry twice.
                    if archive.has_entry(&versioned) {
                        resolved = Some(versioned);
                        break;
                    }
                }
                resolved.unwrap_or_else(|| relative_path.to_string())
            }
            None => relative_path.to_string(),
        };

        // The served entry must be one the signer explicitly committed to.
        let Some((alg, expected)) = info.signed_entries.get(&served_name) else {
            // Present in a signed JAR but NOT named (with a digest) in the
            // manifest → unsigned entry. Do not inherit the signer's certs.
            debug!(
                "jar signer: class entry {} is not committed-to by the signed \
                 manifest — reporting CodeSource as unsigned",
                served_name
            );
            return Vec::new();
        };
        // Re-read and re-hash the exact bytes we are about to load.
        let Some(bytes) = archive.read_vec(&served_name) else {
            return Vec::new();
        };
        if crate::jar_signer::digest_matches(*alg, &bytes, expected) {
            info.chain.clone()
        } else {
            // Manifest names this entry but the on-disk bytes don't match the
            // signed digest (post-sign tamper) → unsigned.
            debug!(
                "jar signer: class entry {} digest mismatch vs signed manifest \
                 — reporting CodeSource as unsigned",
                served_name
            );
            Vec::new()
        }
    }

    /// V1: bind a verified signer to the actual entry bytes.
    ///
    /// Given the already signature-verified `.SF` bytes, this:
    ///   1. reads `META-INF/MANIFEST.MF` from the archive,
    ///   2. confirms the `.SF`'s `<alg>-Digest-Manifest` matches the
    ///      digest of that `MANIFEST.MF` (so the manifest is the one the
    ///      signer committed to), and
    ///   3. for every per-entry section in the manifest, re-reads the
    ///      named entry and confirms its bytes hash to the manifest's
    ///      `<alg>-Digest` value.
    ///
    /// Returns `Some(entries)` only when the manifest is bound by the `.SF`
    /// **and** every declared entry digest matches; each tuple is
    /// `(entry_name, algorithm, expected_digest)` for an entry the signer
    /// committed to. Any missing manifest, missing committed entry,
    /// unreadable entry, or digest mismatch returns `None` (fail-closed).
    /// Directory entries (`Name:` ending in `/`) carry no digest and are
    /// skipped by the parser.
    ///
    /// V3: the returned list is the authoritative set of *signed* entries
    /// for this signer. `find_class_code_source_info` uses it to attach the
    /// signer chain per class — entries absent from this list are unsigned
    /// even though they live inside the signed JAR (closing the JAR-spec
    /// unsigned-entry attack, where an injected `.class` not named in the
    /// manifest would otherwise inherit the signer's certificates).
    ///
    /// # TRUST BOUNDARY: INTEGRITY, conditional on the caller
    ///
    /// Every check here is a digest comparison — no key, no certificate.
    /// It is meaningful only because the caller has *already* obtained a
    /// `Some(_)` from [`crate::jar_signer::verify_signer_block`] for these
    /// exact `sf_bytes`; that is what turns "the manifest matches the `.SF`"
    /// into "the manifest matches what a verified signer committed to".
    /// Called with an unverified `.SF` this function would happily confirm
    /// an attacker's own manifest, so the ordering in
    /// [`Self::extract_jar_signer_blocks`] is load-bearing, not stylistic.
    ///
    /// The returned list is also **exhaustive by omission**: an archive
    /// entry with no manifest section simply does not appear, and callers
    /// must read "absent" as "unsigned".
    #[allow(clippy::type_complexity)]
    fn verify_signed_entries(
        archive: &mut SharedArchive,
        sf_bytes: &[u8],
    ) -> Option<Vec<(String, crate::jar_signer::DigestAlg, Vec<u8>)>> {
        // (1) Read MANIFEST.MF (streaming-bounded against zip-bombs).
        let manifest_bytes = match archive.by_name("META-INF/MANIFEST.MF").and_then(|mut e| {
            let size = e.size();
            Ok(read_entry_capped(&mut e, size)?)
        }) {
            Ok(d) => d,
            Err(_) => return None,
        };

        // (2) The `.SF` must commit to this exact MANIFEST.MF.
        if !crate::jar_signer::verify_sf_binds_manifest(sf_bytes, &manifest_bytes) {
            return None;
        }

        // (3) Every entry the manifest declares a digest for must match.
        let declared = crate::jar_signer::parse_manifest_entry_digests(&manifest_bytes);
        let mut verified: Vec<(String, crate::jar_signer::DigestAlg, Vec<u8>)> =
            Vec::with_capacity(declared.len());
        for entry in &declared {
            // Defence-in-depth: never let a manifest `Name:` smuggle a
            // traversal/drive-letter key into the lookup.
            if !is_safe_entry_name(&entry.name) {
                return None;
            }
            let bytes = match archive.by_name(&entry.name).and_then(|mut e| {
                let size = e.size();
                Ok(read_entry_capped(&mut e, size)?)
            }) {
                Ok(d) => d,
                // A manifest that signs an entry which is absent or
                // unreadable is a tampered/broken JAR — fail-closed.
                Err(_) => return None,
            };
            if !crate::jar_signer::digest_matches(entry.alg, &bytes, &entry.expected) {
                return None;
            }
            // V3: this entry is committed-to by the signer and its bytes
            // match — record it as a genuinely signed entry.
            verified.push((entry.name.clone(), entry.alg, entry.expected.clone()));
        }
        Some(verified)
    }

    /// Find a raw resource file by its classpath-relative name.
    ///
    /// The `resource_name` is a forward-slash-separated path (e.g., `scrabble.txt`
    /// or `org/renaissance/jdk/streams/data.txt`). Leading slashes are stripped.
    /// Searches all classpath entries in order; returns `Some(bytes)` on first match.
    pub fn find_resource(&self, resource_name: &str) -> Option<Vec<u8>> {
        diag_resource_call_wrapper("find_resource", || self.find_resource_impl(resource_name))
    }

    /// Test resource membership without reading or inflating its contents.
    ///
    /// VM bootstrap uses this to select optional native compatibility packs.
    /// Keeping the operation existence-only avoids turning a few classpath
    /// witnesses into archive decompression and allocation during startup.
    pub fn contains_resource(&self, resource_name: &str) -> bool {
        let name = resource_name.trim_start_matches('/');
        if !is_safe_resource_name(name) {
            return false;
        }
        self.entries.iter().any(|entry| match entry {
            ClassPathEntry::Directory(dir) => {
                let full_path = dir.join(Path::new(name));
                full_path.is_file()
                    && self
                        .checked_directory_resource_canonical(dir, &full_path, name)
                        .is_some()
            }
            ClassPathEntry::JarFile { entry_index, .. }
            | ClassPathEntry::NestedJar { entry_index, .. } => entry_index.contains(name),
            ClassPathEntry::NestedDirectory { entries_cache, .. } => {
                entries_cache.contains_key(name)
            }
            ClassPathEntry::JmodFile {
                class_entry_index,
                all_entry_names,
                ..
            } => {
                let class_name = name.strip_suffix(".class");
                class_name.is_some_and(|class_name| class_entry_index.contains(class_name))
                    || all_entry_names.iter().any(|entry| {
                        entry == name
                            || entry
                                .strip_prefix("classes/")
                                .is_some_and(|entry| entry == name)
                    })
            }
            ClassPathEntry::JImageFile {
                reader,
                class_to_module,
                resource_to_modules,
                ..
            } => {
                name.strip_suffix(".class")
                    .is_some_and(|class_name| class_to_module.contains(reader, class_name))
                    || resource_to_modules.contains_key(name)
            }
        })
    }

    fn find_resource_impl(&self, resource_name: &str) -> Option<Vec<u8>> {
        let name = resource_name.trim_start_matches('/');
        // Path safety: align with `find_class`'s input filter (rejects `..`,
        // NUL, leading slashes, `\\`, drive letters `:`, and `./` / `.\\`)
        // via the shared `is_safe_resource_name` helper. The canonicalize
        // check below remains the authoritative backstop.
        let archive_safe = is_safe_resource_name(name);
        let directory_safe = is_directory_resolvable_resource_name(name);
        if !archive_safe && !directory_safe {
            return None;
        }

        for entry in &self.entries {
            match entry {
                ClassPathEntry::Directory(dir) => {
                    if !directory_safe {
                        continue;
                    }
                    if simple_resource_glob(name).is_some() {
                        for full_path in Self::matching_directory_resource_paths(dir, name) {
                            if self
                                .checked_directory_resource_canonical(dir, &full_path, name)
                                .is_none()
                            {
                                continue;
                            }
                            if let Ok(data) = read_file_for_classpath(&full_path) {
                                debug!(
                                    "Found resource glob {name} in directory {} as {}",
                                    dir.display(),
                                    full_path.display()
                                );
                                return Some(data);
                            }
                        }
                        continue;
                    }
                    let full_path = dir.join(Path::new(name));
                    if full_path.exists() {
                        // C35 audit fix (HIGH security): mirror
                        // `find_class`'s fail-CLOSED canonicalize
                        // contract. Previously the
                        // `if let (Ok(canon_dir), Ok(canon_path))` block
                        // *silently fell through* on canonicalize error
                        // and the un-canonicalized `full_path` was still
                        // read — an attacker who could plant a symlink
                        // that causes `fs::canonicalize` to fail (e.g.
                        // dangling symlink, permission-denied on a
                        // segment, or a Windows reparse point we can't
                        // resolve) could load arbitrary files via
                        // `getResourceAsStream`, even though the
                        // matching `find_class` path correctly rejected
                        // them. Now: on canonicalize failure we skip
                        // this entry (`continue`) so other classpath
                        // entries may still answer the probe, but this
                        // specific filesystem read does NOT proceed.
                        let canon_dir = match self.canonicalize_root(dir) {
                            Ok(p) => p,
                            Err(e) => {
                                debug!(
                                    "Refusing to read resource {name}: cannot \
                                     canonicalize classpath root {}: {e}",
                                    dir.display()
                                );
                                continue;
                            }
                        };
                        let canon_path = match self.canonicalize_cached(&full_path) {
                            Ok(p) => p,
                            Err(e) => {
                                debug!(
                                    "Refusing to read resource {name}: cannot \
                                     canonicalize resolved path {}: {e}",
                                    full_path.display()
                                );
                                continue;
                            }
                        };
                        if !canon_path.starts_with(&canon_dir) {
                            debug!(
                                "Resource path traversal blocked: {} escapes {}",
                                canon_path.display(),
                                canon_dir.display()
                            );
                            return None;
                        }
                        // Use the stable classpath read helper for resources
                        // too; a file that changes mid-read is skipped.
                        if let Ok(data) = read_file_for_classpath(&full_path) {
                            debug!("Found resource {name} in directory {}", dir.display());
                            return Some(data);
                        }
                    }
                }
                ClassPathEntry::JarFile {
                    archive,
                    multi_release,
                    versions_cache,
                    entry_index,
                    ..
                } => {
                    if !archive_safe {
                        continue;
                    }
                    if simple_resource_glob(name).is_some() {
                        let candidates = Self::matching_resource_entry_names(
                            entry_index
                                .iter()
                                .filter(|entry| !entry.starts_with("META-INF/versions/")),
                            name,
                        );
                        for candidate in candidates {
                            let found = if *multi_release {
                                Self::find_in_multi_release_archive(
                                    archive,
                                    versions_cache,
                                    Some(entry_index),
                                    &candidate,
                                    self.multi_release_feature(),
                                )
                            } else {
                                Self::find_in_indexed_archive(archive, entry_index, &candidate)
                            };
                            if let Some(data) = found {
                                debug!("Found resource glob {name} in JAR as {candidate}");
                                return Some(data);
                            }
                        }
                        continue;
                    }
                    let found = if *multi_release {
                        Self::find_in_multi_release_archive(
                            archive,
                            versions_cache,
                            Some(entry_index),
                            name,
                            self.multi_release_feature(),
                        )
                    } else {
                        Self::find_in_indexed_archive(archive, entry_index, name)
                    };
                    if let Some(data) = found {
                        debug!("Found resource {name} in JAR");
                        return Some(data);
                    }
                    // Slash-tolerant retry: HotSpot resolves a `cnf` request
                    // against a `cnf/` directory entry inside a JAR. Mirror
                    // that so `getResource(name)` is non-null for known
                    // archive subdirectories (DaCapo's bench loader does
                    // `getResource("cnf").getProtocol()` with no null check).
                    if !name.ends_with('/') {
                        let alt = format!("{name}/");
                        let alt_found = if *multi_release {
                            Self::find_in_multi_release_archive(
                                archive,
                                versions_cache,
                                Some(entry_index),
                                &alt,
                                self.multi_release_feature(),
                            )
                        } else {
                            Self::find_in_indexed_archive(archive, entry_index, &alt)
                        };
                        if let Some(data) = alt_found {
                            debug!("Found resource {alt} in JAR (slash-tolerant)");
                            return Some(data);
                        }
                    }
                }
                ClassPathEntry::NestedDirectory { entries_cache, .. } => {
                    if !archive_safe {
                        continue;
                    }
                    if simple_resource_glob(name).is_some() {
                        let candidates =
                            Self::matching_resource_entry_names(entries_cache.keys(), name);
                        if let Some(candidate) = candidates.first() {
                            if let Some(data) = entries_cache.get(candidate) {
                                debug!(
                                    "Found resource glob {name} in nested directory as {candidate}"
                                );
                                return Some(data.to_vec());
                            }
                        }
                        continue;
                    }
                    if let Some(data) = entries_cache.get(name) {
                        debug!("Found resource {name} in nested directory");
                        return Some(data.to_vec());
                    }
                }
                ClassPathEntry::NestedJar {
                    archive,
                    nested_path,
                    entry_index,
                    ..
                } => {
                    if !archive_safe {
                        continue;
                    }
                    if simple_resource_glob(name).is_some() {
                        let candidates =
                            Self::matching_resource_entry_names(entry_index.iter(), name);
                        for candidate in candidates {
                            if let Some(data) =
                                Self::find_in_indexed_archive(archive, entry_index, &candidate)
                            {
                                debug!(
                                    "Found resource glob {name} in nested JAR {nested_path} as {candidate}"
                                );
                                return Some(data);
                            }
                        }
                        continue;
                    }
                    if let Some(data) = Self::find_in_indexed_archive(archive, entry_index, name) {
                        debug!("Found resource {name} in nested JAR {nested_path}");
                        return Some(data);
                    }
                }
                ClassPathEntry::JmodFile {
                    path,
                    class_entry_index,
                    archive,
                    backing,
                    ..
                } => {
                    if !archive_safe {
                        continue;
                    }
                    if simple_resource_glob(name).is_some() {
                        let candidates =
                            Self::matching_resource_entry_names(class_entry_index.iter(), name);
                        if let Some(candidate) = candidates.first() {
                            if let Some(data) = Self::jmod_class_bytes(
                                archive,
                                backing,
                                class_entry_index,
                                candidate,
                            ) {
                                debug!(
                                    "Found resource glob {name} in JMOD {} as {candidate}",
                                    path.display()
                                );
                                return Some(data.to_vec());
                            }
                        }
                        continue;
                    }
                    // The `classes/` subtree covers both .class files and the
                    // resources that ship inside the module.
                    if let Some(data) =
                        Self::jmod_class_bytes(archive, backing, class_entry_index, name)
                    {
                        debug!("Found resource {name} in JMOD {}", path.display());
                        return Some(data.to_vec());
                    }
                    // Fall back to archive for non-class entries
                    let jmod_name = format!("{JMOD_CLASSES_PREFIX}{name}");
                    if let Some(data) = Self::jmod_find_vec(archive, backing, &jmod_name) {
                        debug!("Found resource {name} in JMOD {}", path.display());
                        return Some(data);
                    }
                }
                ClassPathEntry::JImageFile {
                    path,
                    reader,
                    resource_to_modules,
                    class_to_module,
                    ..
                } => {
                    if !archive_safe {
                        continue;
                    }
                    // Classes live in `class_to_module`; non-class
                    // resources live in `resource_to_modules`. Try the
                    // appropriate map based on the file extension.
                    let (full_path_attempts, is_class): (Vec<String>, bool) =
                        if let Some(class_name) = name.strip_suffix(".class") {
                            // Candidate only: the attempt is read through
                            // the image below, which confirms it.
                            if let Some(module) = class_to_module.candidate_module(class_name) {
                                (vec![format!("/{module}/{class_name}.class")], true)
                            } else {
                                (Vec::new(), true)
                            }
                        } else {
                            // Non-class resource: try every module that
                            // contains this path.
                            match resource_to_modules.get(name) {
                                Some(modules) => (
                                    modules.iter().map(|m| format!("/{m}/{name}")).collect(),
                                    false,
                                ),
                                None => (Vec::new(), false),
                            }
                        };
                    let _ = is_class;
                    if simple_resource_glob(name).is_some() {
                        let candidates =
                            Self::matching_resource_entry_names(resource_to_modules.keys(), name);
                        for candidate in candidates {
                            if let Some(modules) = resource_to_modules.get(&candidate) {
                                for module in modules {
                                    let attempt = format!("/{module}/{candidate}");
                                    match reader.find_resource(&attempt) {
                                        Ok(Some(bytes)) => {
                                            debug!(
                                                "Found resource glob {name} in jimage {} at {attempt}",
                                                path.display()
                                            );
                                            return Some(bytes);
                                        }
                                        Ok(None) => continue,
                                        Err(_) => continue,
                                    }
                                }
                            }
                        }
                        continue;
                    }
                    for attempt in full_path_attempts {
                        match reader.find_resource(&attempt) {
                            Ok(Some(bytes)) => {
                                debug!(
                                    "Found resource {name} in jimage {} at {attempt}",
                                    path.display()
                                );
                                return Some(bytes);
                            }
                            Ok(None) => continue,
                            Err(_) => continue,
                        }
                    }
                }
            }
        }

        None
    }

    /// Find EVERY classpath entry that contains a resource with the given name
    /// and return a URL string for each match. This is the analogue of the
    /// JDK's `ClassLoader.getResources` enumeration — it walks every entry in
    /// order rather than stopping at the first hit.
    ///
    /// Returned URLs are strings of the form `file:/path/to/dir/<name>` for
    /// directory entries and `jar:file:/path/to/foo.jar!/<name>` for JAR /
    /// nested-JAR / JMOD / jimage entries. Callers can feed these into
    /// `java.net.URL` directly.
    /// Return the raw bytes of every classpath entry that contains a
    /// resource with the given name. Parallel to [`find_all_resource_urls`]
    /// but returns content rather than URL strings — used by Rust-side
    /// resource enumeration paths (e.g. `ServiceLoader` provider discovery)
    /// that want to bypass the JDK's `URL.openStream` / `BufferedReader`
    /// chain.
    pub fn find_all_resource_bytes(&self, resource_name: &str) -> Vec<Vec<u8>> {
        let name = resource_name.trim_start_matches('/');
        // Same input filter as `find_class`/`find_resource` (see
        // `is_safe_resource_name`).
        let archive_safe = is_safe_resource_name(name);
        let directory_safe = is_directory_resolvable_resource_name(name);
        if !archive_safe && !directory_safe {
            return Vec::new();
        }
        let mut out: Vec<Vec<u8>> = Vec::new();
        for entry in &self.entries {
            match entry {
                ClassPathEntry::Directory(dir) => {
                    if !directory_safe {
                        continue;
                    }
                    if simple_resource_glob(name).is_some() {
                        for full_path in Self::matching_directory_resource_paths(dir, name) {
                            if self
                                .checked_directory_resource_canonical(dir, &full_path, name)
                                .is_none()
                            {
                                continue;
                            }
                            if let Ok(bytes) = read_file_for_classpath(&full_path) {
                                out.push(bytes);
                            }
                        }
                        continue;
                    }
                    let full_path = dir.join(Path::new(name));
                    // C35 audit fix (HIGH security): fail-CLOSED on
                    // canonicalize error. The previous
                    // `if let (Ok, Ok)` silently fell through to the
                    // unchecked `read_file_for_classpath` call below,
                    // matching the `find_resource` fail-open hole. A
                    // symlink an attacker can plant such that one of
                    // these canonicalize calls fails is now skipped
                    // rather than read.
                    let canon_dir = match self.canonicalize_root(dir) {
                        Ok(p) => p,
                        Err(_) => continue,
                    };
                    let canon_path = match self.canonicalize_cached(&full_path) {
                        Ok(p) => p,
                        Err(_) => continue,
                    };
                    if !canon_path.starts_with(&canon_dir) {
                        continue;
                    }
                    // Same stable read helper as the single-resource path.
                    if let Ok(bytes) = read_file_for_classpath(&full_path) {
                        out.push(bytes);
                    }
                }
                ClassPathEntry::JarFile {
                    archive,
                    multi_release,
                    versions_cache,
                    entry_index,
                    ..
                } => {
                    if !archive_safe {
                        continue;
                    }
                    if simple_resource_glob(name).is_some() {
                        let candidates = Self::matching_resource_entry_names(
                            entry_index
                                .iter()
                                .filter(|entry| !entry.starts_with("META-INF/versions/")),
                            name,
                        );
                        for candidate in candidates {
                            let bytes = if *multi_release {
                                Self::find_in_multi_release_archive(
                                    archive,
                                    versions_cache,
                                    Some(entry_index),
                                    &candidate,
                                    self.multi_release_feature(),
                                )
                            } else {
                                Self::find_in_indexed_archive(archive, entry_index, &candidate)
                            };
                            if let Some(b) = bytes {
                                out.push(b);
                            }
                        }
                        continue;
                    }
                    let bytes = if *multi_release {
                        Self::find_in_multi_release_archive(
                            archive,
                            versions_cache,
                            Some(entry_index),
                            name,
                            self.multi_release_feature(),
                        )
                    } else {
                        Self::find_in_indexed_archive(archive, entry_index, name)
                    };
                    if let Some(b) = bytes {
                        out.push(b);
                    }
                }
                ClassPathEntry::NestedDirectory { entries_cache, .. } => {
                    if !archive_safe {
                        continue;
                    }
                    if simple_resource_glob(name).is_some() {
                        for candidate in
                            Self::matching_resource_entry_names(entries_cache.keys(), name)
                        {
                            if let Some(b) = entries_cache.get(&candidate) {
                                out.push(b.to_vec());
                            }
                        }
                        continue;
                    }
                    if let Some(b) = entries_cache.get(name) {
                        out.push(b.to_vec());
                    }
                }
                ClassPathEntry::NestedJar {
                    archive,
                    entry_index,
                    ..
                } => {
                    if !archive_safe {
                        continue;
                    }
                    if simple_resource_glob(name).is_some() {
                        for candidate in
                            Self::matching_resource_entry_names(entry_index.iter(), name)
                        {
                            if let Some(b) =
                                Self::find_in_indexed_archive(archive, entry_index, &candidate)
                            {
                                out.push(b);
                            }
                        }
                        continue;
                    }
                    if let Some(b) = Self::find_in_indexed_archive(archive, entry_index, name) {
                        out.push(b);
                    }
                }
                ClassPathEntry::JmodFile {
                    class_entry_index,
                    archive,
                    backing,
                    ..
                } => {
                    if !archive_safe {
                        continue;
                    }
                    if simple_resource_glob(name).is_some() {
                        for candidate in
                            Self::matching_resource_entry_names(class_entry_index.iter(), name)
                        {
                            if let Some(b) = Self::jmod_class_bytes(
                                archive,
                                backing,
                                class_entry_index,
                                &candidate,
                            ) {
                                out.push(b.to_vec());
                            }
                        }
                        continue;
                    }
                    if let Some(b) =
                        Self::jmod_class_bytes(archive, backing, class_entry_index, name)
                    {
                        out.push(b.to_vec());
                    } else {
                        let jmod_name = format!("{JMOD_CLASSES_PREFIX}{name}");
                        if let Some(b) = Self::jmod_find_vec(archive, backing, &jmod_name) {
                            out.push(b);
                        }
                    }
                }
                ClassPathEntry::JImageFile {
                    reader,
                    resource_to_modules,
                    class_to_module,
                    ..
                } => {
                    if !archive_safe {
                        continue;
                    }
                    let attempts: Vec<String> =
                        if let Some(class_name) = name.strip_suffix(".class") {
                            // Candidate only: the attempt is read below.
                            if let Some(module) = class_to_module.candidate_module(class_name) {
                                vec![format!("/{module}/{class_name}.class")]
                            } else {
                                Vec::new()
                            }
                        } else {
                            match resource_to_modules.get(name) {
                                Some(modules) => {
                                    modules.iter().map(|m| format!("/{m}/{name}")).collect()
                                }
                                None => Vec::new(),
                            }
                        };
                    if simple_resource_glob(name).is_some() {
                        for candidate in
                            Self::matching_resource_entry_names(resource_to_modules.keys(), name)
                        {
                            if let Some(modules) = resource_to_modules.get(&candidate) {
                                for module in modules {
                                    let attempt = format!("/{module}/{candidate}");
                                    if let Ok(Some(b)) = reader.find_resource(&attempt) {
                                        out.push(b);
                                    }
                                }
                            }
                        }
                        continue;
                    }
                    for attempt in attempts {
                        if let Ok(Some(b)) = reader.find_resource(&attempt) {
                            out.push(b);
                        }
                    }
                }
            }
        }
        out
    }

    /// Build the `file:`-path segment of a reconstructed `jar:file:...!/...`
    /// resource URL for a `NestedDirectory`/`NestedJar` entry's `parent_jar`.
    ///
    /// `NestedDirectory`/`NestedJar` entries reached via `add_path`'s
    /// DaCapo-style `<jar>!/<prefix>/` handoff (see its doc comment) carry a
    /// `parent_jar` that is the UNTOUCHED substring of whatever raw `jar:`
    /// URL string the caller built — `extract_url_path` in
    /// `native-builtins/src/classloader.rs` strips only a spurious leading
    /// `/` before a Windows drive letter (for `PathBuf` resolvability) and
    /// otherwise never rewrites separators. Real Java's `URL`/`URLClassLoader`
    /// machinery is equally hands-off: `new URL(String)` never normalizes,
    /// so a caller-built `"jar:file:" + file.getAbsolutePath() + "!/..."`
    /// (Windows: backslash-separated, no leading `/`) round-trips through
    /// HotSpot byte-for-byte. Forcibly rewriting `parent_jar` to a canonical
    /// forward-slash `/C:/...` form here — as this used to do unconditionally
    /// — produced a URL that differed from HotSpot's for exactly that raw,
    /// backslash-spelled case (Spring Boot's
    /// `TomcatEmbeddedWebappClassLoaderTests`, which builds the expected URL
    /// via `File.getAbsolutePath()` directly, no `toURI()`).
    ///
    /// A backslash anywhere in `parent_jar` can only mean it came from that
    /// raw, unnormalized path (real on-disk jar discovery always resolves
    /// through `canonicalize_cached` into the `JarFile`/`Directory` variants,
    /// not this one) — preserve it verbatim. Otherwise fall back to the
    /// historical canonical-forward-slash form.
    fn nested_jar_url_path(parent_jar: &Path) -> std::borrow::Cow<'_, str> {
        let raw = parent_jar.to_string_lossy();
        if raw.contains('\\') {
            raw
        } else {
            let p = raw.trim_start_matches('/');
            std::borrow::Cow::Owned(format!("/{p}"))
        }
    }

    /// Every URL a SINGLE classpath entry serves for `name`.
    ///
    /// Split out of [`Self::find_all_resource_urls_impl`] so the whole-list
    /// walk and the incremental walk ([`Self::next_resource_url_from`]) run
    /// the same per-entry rules instead of two copies that can drift. The
    /// `continue`s that used to mean "next entry" are `return`s here; nothing
    /// else moved.
    fn resource_urls_for_entry(
        &self,
        entry: &ClassPathEntry,
        name: &str,
        archive_safe: bool,
        directory_safe: bool,
        dbg: bool,
    ) -> Vec<String> {
        let mut urls = Vec::new();
        match entry {
            ClassPathEntry::Directory(dir) => {
                if !directory_safe {
                    return urls;
                }
                if simple_resource_glob(name).is_some() {
                    for full_path in Self::matching_directory_resource_paths(dir, name) {
                        if let Some(canon_path) =
                            self.checked_directory_resource_canonical(dir, &full_path, name)
                        {
                            urls.push(Self::directory_resource_url_from_canonical(&canon_path));
                        }
                    }
                    return urls;
                }
                // `getResources("")` names the classpath root ITSELF, and
                // the generic path below answers it the expensive way:
                // `dir.join("")` is `dir` with a trailing separator, so it
                // costs an `exists()` statx, a `canonicalize` of `dir`, a
                // SECOND `canonicalize` of the trailing-slash spelling
                // (a distinct cache key), and an `is_dir()` statx — per
                // directory entry, per call.
                //
                // That is not a hypothetical shape. SmallRye Config's
                // `AbstractLocationConfigSourceLoader.isInClassloader` is
                // exactly `classLoader.resources(uri.getPath()).anyMatch(..)`
                // with an empty path, and Quarkus's config bootstrap runs
                // it once per discovered config source per profile — 1388
                // times over a 4230-entry classpath for ONE test class,
                // which is where 4.43M `statx` calls came from against
                // HotSpot's 11.9K for the same run.
                //
                // A `Directory` entry is a directory that is on the
                // classpath; both facts were established when it was
                // admitted. Re-deriving them from the filesystem on every
                // probe buys nothing, so answer from the memoized root.
                if name.is_empty() {
                    if let Ok(canon_dir) = self.canonicalize_root(dir) {
                        let p = canon_dir.to_string_lossy().replace('\\', "/");
                        let p = p.strip_prefix("//?/").unwrap_or(&p);
                        let p = p.trim_start_matches('/').trim_end_matches('/');
                        urls.push(format!("file:/{}/", encode_path_for_url(p)));
                    }
                    return urls;
                }
                let full_path = dir.join(Path::new(name));
                if full_path.exists() {
                    // C35 audit fix (HIGH security): fail-CLOSED on
                    // canonicalize error. Previously the
                    // `if let (Ok, Ok)` silently fell through to the
                    // `urls.push(file:/...)` emission below — an
                    // attacker who could plant a symlink causing
                    // canonicalize to fail could leak the existence
                    // of arbitrary files via `getResources()`
                    // enumeration (even though the matching
                    // `find_class` path correctly rejected the same
                    // symlink).
                    let canon_dir = match self.canonicalize_root(dir) {
                        Ok(p) => p,
                        Err(_) => return urls,
                    };
                    let canon_path = match self.canonicalize_cached(&full_path) {
                        Ok(p) => p,
                        Err(_) => return urls,
                    };
                    if !canon_path.starts_with(&canon_dir) {
                        return urls;
                    }
                    // `canon_path` is the already-canonicalized
                    // resolved path; reuse it directly instead of
                    // re-canonicalizing (and avoid the
                    // `unwrap_or(full_path)` fallback that, prior to
                    // this fix, would have emitted the
                    // un-canonicalized path on canonicalize error).
                    let p = canon_path.to_string_lossy().replace('\\', "/");
                    let p = p.strip_prefix("//?/").unwrap_or(&p);
                    let p = p.trim_start_matches('/');
                    // A directory resource (e.g. a package path queried via
                    // `ClassLoader.getResources("com/example/pkg/")`) must
                    // keep its trailing slash — real `URLClassLoader`
                    // preserves it, and Spring's
                    // `PathMatchingResourcePatternResolver` relies on it:
                    // its `rootDirCache` collapses sibling directory scans
                    // onto a shared parent `Resource` and reconstructs
                    // child paths via `createRelative`/
                    // `StringUtils.applyRelativePath`, which treats a
                    // no-trailing-slash URL as a FILE path and strips the
                    // last segment when appending a relative child —
                    // silently resolving to a sibling directory instead of
                    // a subdirectory. That broke any SECOND differently-
                    // pathed scan against the same resolver instance, e.g.
                    // a `@ComponentScan`-discovered `@Configuration` class
                    // whose OWN `@ComponentScan` scans a sibling package
                    // (ComponentScanAnnotationRecursionTests, 2+ levels of
                    // recursive `@ComponentScan`). Matches the established
                    // unconditional-slash pattern in
                    // `find_class_code_source_info` above, but here it
                    // must be conditional since this function also serves
                    // plain (non-directory) resource lookups.
                    if canon_path.is_dir() && !p.ends_with('/') {
                        urls.push(format!("file:/{}/", encode_path_for_url(&p)));
                    } else {
                        urls.push(format!("file:/{}", encode_path_for_url(&p)));
                    }
                }
            }
            ClassPathEntry::JarFile {
                archive,
                multi_release,
                versions_cache,
                entry_index,
                path,
                ..
            } => {
                if !archive_safe {
                    if dbg {
                        eprintln!(
                            "[GRES-DBG]   jar {} mr={} -> skipped unsafe name",
                            path.display(),
                            multi_release
                        );
                    }
                    return urls;
                }
                if simple_resource_glob(name).is_some() {
                    let candidates = Self::matching_resource_entry_names(
                        entry_index
                            .iter()
                            .filter(|entry| !entry.starts_with("META-INF/versions/")),
                        name,
                    );
                    if dbg {
                        eprintln!(
                            "[GRES-DBG]   jar {} mr={} -> {}",
                            path.display(),
                            multi_release,
                            if candidates.is_empty() { "miss" } else { "HIT" }
                        );
                    }
                    if !candidates.is_empty() {
                        let abs = self
                            .canonicalize_root(path)
                            .unwrap_or_else(|_| path.clone());
                        let p = abs.to_string_lossy().replace('\\', "/");
                        let p = p.strip_prefix("//?/").unwrap_or(&p);
                        let p = p.trim_start_matches('/');
                        for candidate in candidates {
                            urls.push(format!(
                                "jar:file:/{}!/{candidate}",
                                encode_path_for_url(&p)
                            ));
                        }
                    }
                    return urls;
                }
                let direct_entry = if *multi_release {
                    Self::multi_release_entry_name(
                        archive,
                        versions_cache,
                        entry_index,
                        name,
                        self.multi_release_feature(),
                    )
                } else {
                    entry_index.contains(name).then(|| name.to_string())
                };
                // HotSpot's URLClassLoader matches a request for `cnf` against
                // a `cnf/` directory entry inside a JAR. Without the slash-
                // tolerant retry, `getResource("cnf")` returned null even when
                // the JAR clearly contains the directory, breaking DaCapo's
                // `extractBenchmarkSet` (which dereferences the URL's
                // protocol without a null check).
                let slash_entry = if direct_entry.is_none() && !name.ends_with('/') {
                    let alt = format!("{name}/");
                    if *multi_release {
                        Self::multi_release_entry_name(
                            archive,
                            versions_cache,
                            entry_index,
                            &alt,
                            self.multi_release_feature(),
                        )
                    } else {
                        entry_index.contains(&alt).then_some(alt)
                    }
                } else {
                    None
                };
                let selected_entry = direct_entry.or(slash_entry);
                if dbg {
                    eprintln!(
                        "[GRES-DBG]   jar {} mr={} -> {}",
                        path.display(),
                        multi_release,
                        if selected_entry.is_some() {
                            "HIT"
                        } else {
                            "miss"
                        }
                    );
                }
                // Bind the entry name by pattern rather than testing a
                // separate `found` bool and then `expect()`ing the same
                // Option: the two can only ever agree, but the file denies
                // `clippy::expect_used` outside tests, so the pair broke
                // `cargo clippy` for this crate and every crate that
                // depends on it. `None` means no entry matched, which is
                // exactly "push no URL" — the arm the bool already took.
                if let Some(suffix) = selected_entry {
                    let abs = self
                        .canonicalize_root(path)
                        .unwrap_or_else(|_| path.clone());
                    let p = abs.to_string_lossy().replace('\\', "/");
                    // Strip UNC prefix \\?\ that canonicalize produces on Windows.
                    let p = p.strip_prefix("//?/").unwrap_or(&p);
                    let p = p.trim_start_matches('/');
                    urls.push(format!("jar:file:/{}!/{suffix}", encode_path_for_url(&p)));
                }
            }
            ClassPathEntry::NestedDirectory {
                parent_jar,
                prefix,
                entries_cache,
            } => {
                if !archive_safe {
                    return urls;
                }
                if simple_resource_glob(name).is_some() {
                    let p = Self::nested_jar_url_path(parent_jar);
                    for candidate in Self::matching_resource_entry_names(entries_cache.keys(), name)
                    {
                        urls.push(format!("jar:file:{p}!/{prefix}{candidate}"));
                    }
                    return urls;
                }
                if entries_cache.contains_key(name) {
                    let p = Self::nested_jar_url_path(parent_jar);
                    urls.push(format!("jar:file:{p}!/{prefix}{name}"));
                }
            }
            ClassPathEntry::NestedJar {
                parent_jar,
                archive,
                nested_path,
                entry_index,
                ..
            } => {
                if !archive_safe {
                    return urls;
                }
                if simple_resource_glob(name).is_some() {
                    let p = Self::nested_jar_url_path(parent_jar);
                    for candidate in Self::matching_resource_entry_names(entry_index.iter(), name) {
                        urls.push(format!("jar:nested:{p}/!{nested_path}!/{candidate}"));
                    }
                    return urls;
                }
                if Self::find_in_indexed_archive(archive, entry_index, name).is_some() {
                    let p = Self::nested_jar_url_path(parent_jar);
                    urls.push(format!("jar:nested:{p}/!{nested_path}!/{name}"));
                }
            }
            ClassPathEntry::JmodFile {
                path,
                class_entry_index,
                archive,
                ..
            } => {
                if !archive_safe {
                    return urls;
                }
                if simple_resource_glob(name).is_some() {
                    let p = path.to_string_lossy().replace('\\', "/");
                    let p = p.trim_start_matches('/');
                    let jrt_module = Self::runtime_image_jmod_module(path);
                    for candidate in
                        Self::matching_resource_entry_names(class_entry_index.iter(), name)
                    {
                        if let Some(module) = &jrt_module {
                            urls.push(format!("jrt:/{module}/{candidate}"));
                            continue;
                        }
                        urls.push(format!(
                            "jar:file:/{}!/{candidate}",
                            encode_path_for_url(&p)
                        ));
                    }
                    return urls;
                }
                let found = class_entry_index.contains(name) || {
                    let jmod_name = format!("{JMOD_CLASSES_PREFIX}{name}");
                    // URL emission only — never the bytes. Inflating the
                    // entry here just to discard it made every
                    // `getResource` hit on a JMOD pay a full deflate.
                    Self::jmod_has_entry(archive, &jmod_name)
                };
                if found {
                    if let Some(module) = Self::runtime_image_jmod_module(path) {
                        // The JDK's own `jmods/<module>.jmod` stands in for
                        // the runtime image: HotSpot names that resource
                        // `jrt:/<module>/<name>` whichever form boots.
                        urls.push(format!("jrt:/{module}/{name}"));
                    } else {
                        let p = path.to_string_lossy().replace('\\', "/");
                        let p = p.trim_start_matches('/');
                        urls.push(format!("jar:file:/{}!/{name}", encode_path_for_url(&p)));
                    }
                }
            }
            ClassPathEntry::JImageFile {
                reader,
                resource_to_modules,
                class_to_module,
                ..
            } => {
                if !archive_safe {
                    return urls;
                }
                let attempts: Vec<String> = if let Some(class_name) = name.strip_suffix(".class") {
                    // Candidate only: each attempt is read below.
                    if let Some(module) = class_to_module.candidate_module(class_name) {
                        vec![format!("/{module}/{class_name}.class")]
                    } else {
                        Vec::new()
                    }
                } else {
                    match resource_to_modules.get(name) {
                        Some(modules) => modules.iter().map(|m| format!("/{m}/{name}")).collect(),
                        None => Vec::new(),
                    }
                };
                if simple_resource_glob(name).is_some() {
                    for candidate in
                        Self::matching_resource_entry_names(resource_to_modules.keys(), name)
                    {
                        if let Some(modules) = resource_to_modules.get(&candidate) {
                            for module in modules {
                                let attempt = format!("/{module}/{candidate}");
                                if matches!(reader.find_resource(&attempt), Ok(Some(_))) {
                                    urls.push(format!("jrt:{attempt}"));
                                }
                            }
                        }
                    }
                    return urls;
                }
                for attempt in attempts {
                    if matches!(reader.find_resource(&attempt), Ok(Some(_))) {
                        // JEP 220 jrt URL scheme: `jrt:/<module>/<resource>`.
                        // The TOC entry path is already `/<module>/<resource>`,
                        // so emit it verbatim with the `jrt:` scheme prefix.
                        // HotSpot's BuiltinClassLoader.findMiscResource builds
                        // the same URL via JNUFileSystemProvider.
                        urls.push(format!("jrt:{attempt}"));
                    }
                }
            }
        }
        urls
    }

    /// `true` when a resource name can be served one entry at a time.
    ///
    /// A glob name (`simple_resource_glob`) can match SEVERAL names inside a
    /// single entry, so "the first URL this entry serves" would silently drop
    /// the rest. Every other name yields at most one URL per entry, which is
    /// what makes the incremental walk equivalent to the whole-list one.
    pub fn name_supports_incremental_scan(resource_name: &str) -> bool {
        simple_resource_glob(resource_name.trim_start_matches('/')).is_none()
    }

    /// The next URL at or after entry `from`, and the entry index to resume at.
    ///
    /// The whole-list [`Self::find_all_resource_urls`] is the wrong shape for a
    /// caller that stops early, and `ClassLoader.resources(name).anyMatch(..)`
    /// — SmallRye Config's `isInClassloader`, and every `findFirst` over
    /// `resources()` — stops at the first match by construction. The JDK's own
    /// enumeration is lazy per element, which is why HotSpot answers such a
    /// call in 0.03 ms against 1.20 ms here for a 4230-entry classpath: it
    /// touches one entry, we touched all of them.
    ///
    /// Callers must gate on [`Self::name_supports_incremental_scan`].
    pub fn next_resource_url_from(
        &self,
        resource_name: &str,
        from: usize,
    ) -> Option<(String, usize)> {
        let name = resource_name.trim_start_matches('/');
        let archive_safe = is_safe_resource_name(name);
        let directory_safe = is_directory_resolvable_resource_name(name);
        if !archive_safe && !directory_safe {
            return None;
        }
        let dbg = dbg_getresources();
        for (index, entry) in self.entries.iter().enumerate().skip(from) {
            let urls = self.resource_urls_for_entry(entry, name, archive_safe, directory_safe, dbg);
            if let Some(url) = urls.into_iter().next() {
                return Some((url, index + 1));
            }
        }
        None
    }

    pub fn find_all_resource_urls(&self, resource_name: &str) -> Vec<String> {
        diag_resource_call_wrapper("find_all_resource_urls", || {
            self.find_all_resource_urls_impl(resource_name)
        })
    }

    /// The first `jrt:` URL [`Self::find_all_resource_urls`] would list for
    /// `resource_name`, asking only the entries that can serve one.
    ///
    /// Only a [`ClassPathEntry::JImageFile`] and a JDK `jmods/<module>.jmod`
    /// ([`ClassPathEntry::JmodFile`]) ever name a resource `jrt:/...`; a
    /// directory answers `file:`, a jar or a loose `.jmod` `jar:file:`, a
    /// nested entry `jar:file:` / `jar:nested:`. So skipping every other
    /// entry keeps the answer of
    /// `find_all_resource_urls(name).into_iter().find(|u| u.starts_with("jrt:"))`
    /// exactly, without the `exists()` / `canonicalize` of each directory
    /// entry, the jar-index probes and the `String`s of the URLs the caller
    /// then throws away. Interpreter round i3 wave 73, lane L5
    /// (`CRATONVM_PLATFORM_RESOURCE_JRT_WALK`): the platform loader's and a
    /// null-parent loader's `getResource`.
    pub fn first_runtime_image_resource_url(&self, resource_name: &str) -> Option<String> {
        let name = resource_name.trim_start_matches('/');
        let archive_safe = is_safe_resource_name(name);
        let directory_safe = is_directory_resolvable_resource_name(name);
        if !archive_safe && !directory_safe {
            return None;
        }
        let dbg = dbg_getresources();
        self.entries
            .iter()
            .filter(|entry| {
                matches!(
                    entry,
                    ClassPathEntry::JImageFile { .. } | ClassPathEntry::JmodFile { .. }
                )
            })
            .find_map(|entry| {
                self.resource_urls_for_entry(entry, name, archive_safe, directory_safe, dbg)
                    .into_iter()
                    .find(|url| url.starts_with("jrt:"))
            })
    }

    /// [`Self::find_all_resource_urls`] asked of the ONE entry whose root is
    /// `root` (a directory or a jar, matched by spelling, then canonically).
    /// Empty when no entry has that root or it does not hold `name`.
    ///
    /// For a `--module-path` module's `ModuleReader.find(name)`
    /// (`ClassManager::module_path_resource_url`; interpreter round i4 wave
    /// 98, lane L3): a module reader looks a name up literally, so a glob
    /// name answers nothing here.
    pub fn resource_urls_in_root(&self, root: &Path, resource_name: &str) -> Vec<String> {
        let name = resource_name.trim_start_matches('/');
        if simple_resource_glob(name).is_some() {
            return Vec::new();
        }
        let archive_safe = is_safe_resource_name(name);
        let directory_safe = is_directory_resolvable_resource_name(name);
        if !archive_safe && !directory_safe {
            return Vec::new();
        }
        match self.entry_with_root(root) {
            Some(entry) => {
                let dbg = dbg_getresources();
                self.resource_urls_for_entry(entry, name, archive_safe, directory_safe, dbg)
            }
            None => Vec::new(),
        }
    }

    /// The bytes of `resource_name` in the ONE directory or jar entry whose
    /// root is `root` (the bytes sibling of [`Self::resource_urls_in_root`]).
    /// `None` when no entry has that root or it does not hold the name.
    pub fn resource_bytes_in_root(&self, root: &Path, resource_name: &str) -> Option<Vec<u8>> {
        let name = resource_name.trim_start_matches('/');
        if name.is_empty() || name.ends_with('/') || simple_resource_glob(name).is_some() {
            return None;
        }
        match self.entry_with_root(root)? {
            ClassPathEntry::Directory(dir) => {
                if !is_directory_resolvable_resource_name(name) {
                    return None;
                }
                let full_path = dir.join(Path::new(name));
                if !full_path.is_file() {
                    return None;
                }
                self.checked_directory_resource_canonical(dir, &full_path, name)?;
                read_file_for_classpath(&full_path).ok()
            }
            ClassPathEntry::JarFile {
                archive,
                multi_release,
                versions_cache,
                entry_index,
                ..
            } => {
                if !is_safe_resource_name(name) {
                    return None;
                }
                if *multi_release {
                    Self::find_in_multi_release_archive(
                        archive,
                        versions_cache,
                        Some(entry_index),
                        name,
                        self.multi_release_feature(),
                    )
                } else {
                    Self::find_in_indexed_archive(archive, entry_index, name)
                }
            }
            _ => None,
        }
    }

    /// The directory or jar entry whose root is `root`: by spelling first,
    /// then canonically.
    fn entry_with_root(&self, root: &Path) -> Option<&ClassPathEntry> {
        fn entry_root(entry: &ClassPathEntry) -> Option<&Path> {
            match entry {
                ClassPathEntry::Directory(dir) => Some(dir.as_path()),
                ClassPathEntry::JarFile { path, .. } => Some(path.as_path()),
                _ => None,
            }
        }
        self.entries
            .iter()
            .find(|e| entry_root(e).is_some_and(|p| p == root))
            .or_else(|| {
                let canon = fs::canonicalize(root).ok()?;
                self.entries.iter().find(|e| {
                    entry_root(e)
                        .is_some_and(|p| self.canonicalize_root(p).is_ok_and(|c| c == canon))
                })
            })
    }

    fn find_all_resource_urls_impl(&self, resource_name: &str) -> Vec<String> {
        let name = resource_name.trim_start_matches('/');
        // Same input filter as `find_class`/`find_resource` (see
        // `is_safe_resource_name`).
        let archive_safe = is_safe_resource_name(name);
        let directory_safe = is_directory_resolvable_resource_name(name);
        if !archive_safe && !directory_safe {
            return Vec::new();
        }
        // ES2-DBG: env-gated tracing for the classpath resource walk.
        // `CRATONVM_DBG_GETRESOURCES=1` emits per-entry hit/miss for every
        // call. Each `ClassPath` instance (bootstrap / extension /
        // application) calls this independently, so a real probe prints
        // one block per loader — useful for spotting whether a specific
        // jar is missing from the application classpath entirely vs
        // simply lacking the resource.
        let dbg = dbg_getresources();
        if dbg {
            eprintln!(
                "[GRES-DBG] find_all_resource_urls({}) — scanning {} entries",
                name,
                self.entries.len()
            );
        }
        let mut urls = Vec::new();
        for entry in &self.entries {
            urls.extend(self.resource_urls_for_entry(
                entry,
                name,
                archive_safe,
                directory_safe,
                dbg,
            ));
        }
        if dbg {
            eprintln!(
                "[GRES-DBG] find_all_resource_urls({}) -> {} URLs",
                name,
                urls.len()
            );
        }
        urls
    }

    /// Scan every JAR/directory for `module-info.class` files and return
    /// their raw bytes.  A JAR may contain at most one `module-info.class`
    /// at its root; a directory may contain one too.
    ///
    /// Used by [`ClassManager`](super::ClassManager) during initialisation to
    /// pre-populate the module registry before any class is loaded.
    pub fn scan_module_infos(&self) -> Vec<Vec<u8>> {
        let mut results = Vec::new();
        for entry in &self.entries {
            match entry {
                ClassPathEntry::Directory(dir) => {
                    let path = dir.join("module-info.class");
                    // Keep directory module-info reads on the same stable
                    // classpath file helper.
                    if let Ok(data) = read_file_for_classpath(&path) {
                        debug!("Found module-info.class in directory {}", dir.display());
                        results.push(data);
                    }
                }
                ClassPathEntry::JarFile {
                    archive,
                    multi_release,
                    versions_cache,
                    entry_index,
                    ..
                } => {
                    let found = if *multi_release {
                        Self::find_in_multi_release_archive(
                            archive,
                            versions_cache,
                            Some(entry_index),
                            "module-info.class",
                            self.multi_release_feature(),
                        )
                    } else {
                        Self::find_in_indexed_archive(archive, entry_index, "module-info.class")
                    };
                    if let Some(data) = found {
                        debug!("Found module-info.class in JAR");
                        results.push(data);
                    }
                }
                ClassPathEntry::NestedDirectory { entries_cache, .. } => {
                    if let Some(data) = entries_cache.get("module-info.class") {
                        debug!("Found module-info.class in nested directory");
                        results.push(data.to_vec());
                    }
                }
                ClassPathEntry::NestedJar {
                    archive,
                    nested_path,
                    entry_index,
                    ..
                } => {
                    if let Some(data) =
                        Self::find_in_indexed_archive(archive, entry_index, "module-info.class")
                    {
                        debug!("Found module-info.class in nested JAR {nested_path}");
                        results.push(data);
                    }
                }
                ClassPathEntry::JmodFile {
                    path,
                    class_entry_index,
                    archive,
                    backing,
                    ..
                } => {
                    if let Some(data) = Self::jmod_class_bytes(
                        archive,
                        backing,
                        class_entry_index,
                        "module-info.class",
                    ) {
                        debug!("Found module-info.class in JMOD {}", path.display());
                        results.push(data.to_vec());
                    }
                }
                ClassPathEntry::JImageFile {
                    path,
                    reader,
                    module_names,
                    ..
                } => {
                    // One module-info per module inside the jimage. Ask
                    // the reader for `/<module>/module-info.class` for
                    // every known module name.
                    for module in module_names {
                        let query = format!("/{module}/module-info.class");
                        match reader.find_resource(&query) {
                            Ok(Some(bytes)) => {
                                debug!(
                                    "Found module-info.class for {module} in jimage {}",
                                    path.display()
                                );
                                results.push(bytes);
                            }
                            Ok(None) => continue,
                            Err(_) => continue,
                        }
                    }
                }
            }
        }
        results
    }

    /// Populate package-to-module mappings from any JIMAGE entries on this classpath into `registry`.
    pub fn populate_module_registry_packages(&self, registry: &mut crate::module::ModuleRegistry) {
        for entry in &self.entries {
            if let ClassPathEntry::JImageFile { class_to_module, .. } = entry {
                class_to_module.populate_registry_packages(registry);
            }
        }
    }

    /// List all class names available on this classpath.
    ///
    /// Returns binary class names (e.g. `com/example/MyClass`) for every
    /// `.class` file found across all entries. `module-info` is excluded.
    pub fn list_class_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        for entry in &self.entries {
            match entry {
                ClassPathEntry::Directory(dir) => {
                    Self::walk_dir_classes(dir, dir, &mut names);
                }
                ClassPathEntry::JarFile { archive, .. } => {
                    Self::list_archive_classes(archive, &mut names);
                }
                ClassPathEntry::NestedDirectory { entries_cache, .. } => {
                    for key in entries_cache.keys() {
                        if let Some(cn) = Self::class_file_to_name(key) {
                            names.push(cn);
                        }
                    }
                }
                ClassPathEntry::NestedJar { archive, .. } => {
                    Self::list_archive_classes(archive, &mut names);
                }
                ClassPathEntry::JmodFile {
                    all_entry_names, ..
                } => {
                    for name in all_entry_names {
                        if let Some(class_path) = name.strip_prefix("classes/") {
                            if let Some(cn) = Self::class_file_to_name(class_path) {
                                names.push(cn);
                            }
                        }
                    }
                }
                ClassPathEntry::JImageFile {
                    reader,
                    class_to_module,
                    ..
                } => {
                    // Every class the index knows is exactly a binary
                    // class name.
                    names.extend(class_to_module.class_names(reader));
                }
            }
        }
        names
    }

    /// The module a `<java.home>/jmods/<module>.jmod` file stands for, when
    /// `path` is one of the JDK's own (a `jmods` directory beside a `lib`
    /// directory); `None` for any other `.jmod`, which stays a `jar:file:` URL.
    /// A JDK booted from `jmods/` and one booted from `lib/modules` must name
    /// a `java.base` resource alike (`jrt:/java.base/...`, as HotSpot does).
    fn runtime_image_jmod_module(path: &Path) -> Option<String> {
        let dir = path.parent()?;
        if dir.file_name()? != "jmods" || !dir.parent()?.join("lib").is_dir() {
            return None;
        }
        Some(path.file_stem()?.to_string_lossy().into_owned())
    }

    /// Return the list of JMOD module names on this classpath.
    ///
    /// Module names are extracted from the JMOD file name stem
    /// (e.g. `java.base.jmod` → `"java.base"`); a runtime-image entry lists
    /// the image's modules. JARs and directories are skipped.
    pub fn list_jmod_modules(&self) -> Vec<String> {
        let mut modules = Vec::new();
        for entry in &self.entries {
            match entry {
                ClassPathEntry::JmodFile { path, .. } => {
                    if let Some(stem) = path.file_stem() {
                        modules.push(stem.to_string_lossy().into_owned());
                    }
                }
                // A boot path that is the runtime image alone (a JDK without
                // `jmods/`, or `CRATONVM_BOOT_JIMAGE_OVER_JMODS`) lists the
                // image's modules.
                ClassPathEntry::JImageFile { module_names, .. } => {
                    modules.extend(module_names.iter().cloned());
                }
                _ => {}
            }
        }
        modules
    }

    /// Return the total number of classes available in JMOD entries.
    pub fn jmod_class_count(&self) -> usize {
        self.entries
            .iter()
            .map(|e| match e {
                ClassPathEntry::JmodFile {
                    class_entry_index, ..
                } => class_entry_index.len(),
                _ => 0,
            })
            .sum()
    }

    /// Return the number of classes known to the jimage entries on this
    /// classpath. Like `jmod_class_count`, this is O(1) per entry since
    /// the class-to-module index was built at load time.
    pub fn jimage_class_count(&self) -> usize {
        self.entries
            .iter()
            .map(|e| match e {
                ClassPathEntry::JImageFile {
                    class_to_module, ..
                } => class_to_module.class_count(),
                _ => 0,
            })
            .sum()
    }

    /// Whether the runtime image on this classpath defines the class
    /// `internal` (slash form, no `.class`): a `lib/modules` jimage, or the
    /// `jmods/*.jmod` a JDK that ships them is booted from (the Windows
    /// Microsoft build does; a jlinked linux image does not). `None` when
    /// neither kind of entry is on this classpath, so there is no image to
    /// ask.
    pub fn runtime_image_defines_class(&self, internal: &str) -> Option<bool> {
        let mut any_image = false;
        for entry in &self.entries {
            let defines = match entry {
                ClassPathEntry::JImageFile {
                    reader,
                    class_to_module,
                    ..
                } => class_to_module.contains(reader, internal),
                // Keyed by the entry name under `classes/`, `.class` kept.
                ClassPathEntry::JmodFile {
                    class_entry_index, ..
                } => class_entry_index.contains(&format!("{internal}.class")),
                _ => continue,
            };
            if defines {
                return Some(true);
            }
            any_image = true;
        }
        any_image.then_some(false)
    }

    /// List the module names contributed by jimage entries on this
    /// classpath. Deduplicated, sorted alphabetically.
    pub fn list_jimage_modules(&self) -> Vec<String> {
        let mut modules = Vec::new();
        for entry in &self.entries {
            if let ClassPathEntry::JImageFile { module_names, .. } = entry {
                for m in module_names {
                    if !modules.contains(m) {
                        modules.push(m.clone());
                    }
                }
            }
        }
        modules.sort();
        modules
    }

    /// Load a JDK 9+ `lib/modules` jimage file onto this classpath.
    ///
    /// Opens the file via [`cratonvm_reader::JImageReader`], walks every
    /// entry once to build the class-to-module and resource-to-modules
    /// indexes, then inserts a [`ClassPathEntry::JImageFile`] onto the
    /// classpath. Lookups after this call return bytes directly from
    /// the jimage's perfect-hash index.
    ///
    /// Returns an error string if the file cannot be opened or is not
    /// a valid jimage. Callers that want a typed error should match on
    /// [`cratonvm_reader::JImageError`] via the direct reader API.
    pub fn add_jimage(&mut self, path: impl AsRef<Path>) -> Result<(), String> {
        let path = path.as_ref().to_path_buf();
        let entry = Self::load_jimage(&path)?;
        push_classpath_entry(&mut self.entries, entry);
        Ok(())
    }

    /// Internal constructor for a [`ClassPathEntry::JImageFile`]. Reads
    /// the entire jimage, decodes every location record once, and
    /// builds the two lookup indexes. Returns a string error so the
    /// caller can surface it through the same error path as `load_jmod`.
    fn load_jimage(path: &Path) -> Result<ClassPathEntry, String> {
        Self::load_jimage_with(path, loader_flags().jimage_package_index)
    }

    /// [`Self::load_jimage`] with the `CRATONVM_JIMAGE_PACKAGE_INDEX` decision
    /// made by the caller (`by_package`), so a test can index one image both
    /// ways.
    fn load_jimage_with(path: &Path, by_package: bool) -> Result<ClassPathEntry, String> {
        Self::load_jimage_with_opts(
            path,
            by_package,
            loader_flags().jimage_skip_directory_records,
        )
    }

    /// The jimage module components that name directory records, not modules:
    /// libjimage's `/packages/<pkg>/<module>` package table and the `/modules`
    /// tree `jrt:/` lists (zt t3/fp,
    /// `docs/feature-designs/zgc-architecture.md`).
    fn is_jimage_directory_record_module(module: &str) -> bool {
        module == "packages" || module == "modules"
    }

    /// [`Self::load_jimage_with`] with the `CRATONVM_JIMAGE_SKIP_DIRECTORY_RECORDS`
    /// decision made by the caller (`skip_dirs`), so a test can index one
    /// image both ways. With `skip_dirs` the image's directory records are in
    /// neither `resource_to_modules` nor `module_names`: HotSpot serves none
    /// of them as a resource and has no module `packages` / `modules`.
    fn load_jimage_with_opts(
        path: &Path,
        by_package: bool,
        skip_dirs: bool,
    ) -> Result<ClassPathEntry, String> {
        let reader = Self::open_jimage_reader(path)
            .map_err(|e| format!("open jimage {}: {e}", path.display()))?;

        // Walk every entry once to build the indexes. SBR round 5, lane
        // warmup: this runs at every boot over ~30 000 entries, so it reads
        // each location's path components in place (no assembled path
        // `String` per entry) and keys the maps with the fast hasher.
        //
        // zs s6/rs: classes go into a per-PACKAGE index by default
        // (`CRATONVM_JIMAGE_PACKAGE_INDEX`, `=0` kills): the per-class map was
        // ~27 000 `String` pairs (~5 MB with its reserve, written across every
        // page of the table) on every boot.
        let mut class_to_module =
            JImageIndexBuilder::new(by_package, reader.resource_count() as usize);
        let mut resource_to_modules: FxHashMap<String, Vec<String>> = FxHashMap::default();
        let mut module_set: std::collections::BTreeSet<String> = Default::default();
        // p5-cp: the walk reads each location's component OFFSETS and a
        // string only where it needs one: a module's name once per distinct
        // offset, the `class` extension and `module-info` base compared in
        // place, a class's package once per distinct offset (the builder's
        // `add_at`); a non-class resource still reads all its parts. Same
        // order, same entries, same indexes as reading every string.
        // Module name per string offset, and whether it is in `module_set`
        // yet: the set insert (a `String` allocation and a tree search) once
        // per module, not once per entry (the walk is in hash-slot order, so
        // consecutive entries rarely share a module).
        let mut module_at: FxHashMap<u64, (&str, bool)> = FxHashMap::default();
        let mut class_extension_at: Option<u64> = None;
        let mut walk_error: Option<cratonvm_reader::JImageError> = None;

        let walked = reader.for_each_entry_offsets(|e| {
            macro_rules! read {
                ($r:expr) => {
                    match $r {
                        Ok(v) => v,
                        Err(err) => {
                            walk_error.get_or_insert(err);
                            return;
                        }
                    }
                };
            }
            if walk_error.is_some() {
                return;
            }
            let module_slot = match module_at.entry(e.module) {
                std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert((read!(reader.string_at(e.module)), false))
                }
            };
            let module: &str = module_slot.0;
            // The entry's path is "/module/parent/base.ext" (empty parts
            // and their separators omitted). "Without the leading '/', split
            // at the first '/'" gives (module, rest); an entry without a
            // module, or whose rest is empty, is skipped, as before.
            if module.is_empty() {
                // No leading "/module": rebuild the path and take the
                // generic split the walk always took.
                let parent = read!(reader.string_at(e.parent));
                let base = read!(reader.string_at(e.base));
                let extension = read!(reader.string_at(e.extension));
                let mut full = String::new();
                if !parent.is_empty() {
                    full.push('/');
                    full.push_str(parent);
                }
                if !base.is_empty() {
                    full.push('/');
                    full.push_str(base);
                }
                if !extension.is_empty() {
                    full.push('.');
                    full.push_str(extension);
                }
                let Some(without_lead) = full.strip_prefix('/') else {
                    return;
                };
                let Some((m, r)) = without_lead.split_once('/') else {
                    return;
                };
                if skip_dirs && Self::is_jimage_directory_record_module(m) {
                    return;
                }
                let r = r.to_string();
                module_set.insert(m.to_string());
                Self::index_jimage_entry(&mut class_to_module, &mut resource_to_modules, m, r);
                return;
            }
            if read!(reader.string_is_empty(e.parent)) && read!(reader.string_is_empty(e.base)) {
                // "/module" or "/module.ext": no '/' after the module.
                return;
            }
            // zt t3/fp: the image's directory records are not resources.
            if skip_dirs && Self::is_jimage_directory_record_module(module) {
                return;
            }
            if !module_slot.1 {
                module_slot.1 = true;
                module_set.insert(module.to_string());
            }
            let is_class = if class_extension_at == Some(e.extension) {
                true
            } else if read!(reader.string_is(e.extension, "class")) {
                class_extension_at = Some(e.extension);
                true
            } else {
                false
            };
            if is_class {
                if !read!(reader.string_is(e.base, "module-info")) {
                    read!(class_to_module.add_at(&reader, e.parent, e.base, module));
                }
            } else {
                let parent = read!(reader.string_at(e.parent));
                let base = read!(reader.string_at(e.base));
                let extension = read!(reader.string_at(e.extension));
                let mut rest =
                    String::with_capacity(parent.len() + base.len() + extension.len() + 2);
                rest.push_str(parent);
                if !base.is_empty() {
                    if !parent.is_empty() {
                        rest.push('/');
                    }
                    rest.push_str(base);
                }
                if !extension.is_empty() {
                    rest.push('.');
                    rest.push_str(extension);
                }
                resource_to_modules
                    .entry(rest)
                    .or_default()
                    .push(module.to_string());
            }
        });
        walked
            .and_then(|()| walk_error.map_or(Ok(()), Err))
            .map_err(|e| format!("walk jimage {}: {e}", path.display()))?;
        let class_to_module = class_to_module
            .finish(&reader)
            .map_err(|e| format!("walk jimage {}: {e}", path.display()))?;

        debug!(
            "Loaded jimage {}: {} classes, {} non-class resources, {} modules",
            path.display(),
            class_to_module.class_count(),
            resource_to_modules.len(),
            module_set.len()
        );

        Ok(ClassPathEntry::JImageFile {
            path: path.to_path_buf(),
            reader,
            class_to_module,
            resource_to_modules,
            module_names: module_set.into_iter().collect(),
        })
    }

    /// File one jimage entry (`rest` is the path below its module) into the
    /// class or the resource index.
    fn index_jimage_entry(
        class_to_module: &mut JImageIndexBuilder,
        resource_to_modules: &mut FxHashMap<String, Vec<String>>,
        module: &str,
        rest: String,
    ) {
        if let Some(class_name) = rest.strip_suffix(".class") {
            // `module-info` is per-module, not a globally unique
            // class name. Skipping it from `class_to_module` avoids
            // one module's module-info silently overwriting another's
            // via the final insert. Callers that need module-info
            // bytes go through `scan_module_infos`, which walks the
            // module list directly and queries the reader for each.
            if class_name != "module-info" {
                class_to_module.add(class_name, module);
            }
        } else {
            resource_to_modules
                .entry(rest)
                .or_default()
                .push(module.to_string());
        }
    }

    /// The jimage reader over `path`: the file mapped read-only (a boot reads
    /// the index and the classes it loads, not the whole ~146 MB image), or,
    /// when mapping is disabled or fails, read into memory as before.
    fn open_jimage_reader(
        path: &Path,
    ) -> Result<cratonvm_reader::JImageReader, cratonvm_reader::JImageError> {
        if !loader_flags().disable_jar_mmap {
            if let Ok(file) = std::fs::File::open(path) {
                // Phase 5 boot image (WP2): brokered fixed address in a capture run.
                if let Some(fixed) = cratonvm_types::image_layout::map_file_readonly(
                    cratonvm_types::image_layout::SegmentKind::RuntimeImage,
                    &file,
                    path,
                ) {
                    let shared: Arc<dyn AsRef<[u8]> + Send + Sync> = Arc::new(fixed);
                    return cratonvm_reader::JImageReader::from_shared(shared);
                }
                // SAFETY: a read-only private mapping of the runtime image, a
                // file the JDK installs once and never rewrites while a VM
                // runs (HotSpot maps the same file the same way).
                if let Ok(mapped) = unsafe { memmap2::MmapOptions::new().map(&file) } {
                    let shared: Arc<dyn AsRef<[u8]> + Send + Sync> = Arc::new(mapped);
                    return cratonvm_reader::JImageReader::from_shared(shared);
                }
            }
        }
        cratonvm_reader::JImageReader::open(path)
    }

    fn walk_dir_classes(base: &Path, dir: &Path, out: &mut Vec<String>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for e in entries.flatten() {
                let path = e.path();
                if path.is_dir() {
                    Self::walk_dir_classes(base, &path, out);
                } else if path.extension().is_some_and(|e| e == "class") {
                    if let Ok(rel) = path.strip_prefix(base) {
                        let s = rel.to_string_lossy().replace('\\', "/");
                        if let Some(cn) = Self::class_file_to_name(&s) {
                            out.push(cn);
                        }
                    }
                }
            }
        }
    }

    fn list_archive_classes(archive: &(impl ArchiveRead + ?Sized), out: &mut Vec<String>) {
        archive.each_name(&mut |name| {
            if let Some(cn) = Self::class_file_to_name(name) {
                out.push(cn);
            }
        });
    }

    fn class_file_to_name(path: &str) -> Option<String> {
        let path = path.strip_suffix(".class")?;
        if path == "module-info" || path.ends_with("/module-info") {
            return None;
        }
        Some(path.to_string())
    }

    /// Does `name` exist in the archive?
    ///
    /// PERF (2026-07-26 classpath-scan audit): the existence-only callers used
    /// to spell this `find_in_archive(..).is_some()`, which **inflates the
    /// whole entry** and throws the bytes away — a full deflate pass (and, in
    /// debug builds, an extremely slow one) to answer a yes/no. `by_name`
    /// seeks the central-directory record and builds the reader without
    /// reading any compressed data, so dropping the reader immediately costs
    /// only the seek. The mutex is held for the same short window either way.
    fn archive_has_entry(archive: &Mutex<SharedArchive>, name: &str) -> bool {
        archive.lock().by_name(name).is_ok()
    }

    /// Helper: try to read a named entry from a mutex-guarded ZipArchive.
    fn find_in_archive(archive: &Mutex<SharedArchive>, name: &str) -> Option<Vec<u8>> {
        let mut guard = archive.lock();
        let result = guard.by_name(name).and_then(|mut zip_entry| {
            // Audit-fix #2 / zip-bomb: the declared `size()` comes from the
            // JAR central directory and is attacker-controlled (up to
            // `u64::MAX`). `read_entry_capped` clamps the pre-allocation
            // AND bounds the actual streaming inflate (V2), matching the
            // sibling entry-read paths. The `?` converts the `io::Error`
            // overflow into the `ZipError` the `and_then` expects.
            let size = zip_entry.size();
            Ok(read_entry_capped(&mut zip_entry, size)?)
        });
        match result {
            Ok(data) => Some(data),
            Err(_) => None,
        }
    }

    /// Read an entry as shared class bytes. Stored entries become a direct
    /// range view over the archive mapping; compressed entries are inflated
    /// once into reference-counted owned storage.
    fn find_shared_in_archive(
        archive: &Mutex<SharedArchive>,
        backing: &ArchiveBacking,
        name: &str,
    ) -> Option<SharedBytes> {
        let mut guard = archive.lock();
        Self::find_shared_in_archive_locked(&mut guard, backing, name)
    }

    fn find_shared_in_archive_locked(
        archive: &mut SharedArchive,
        backing: &ArchiveBacking,
        name: &str,
    ) -> Option<SharedBytes> {
        let mut entry = archive.by_name(name).ok()?;
        let size = entry.size();
        if size > MAX_UNCOMPRESSED_ENTRY_BYTES {
            return None;
        }
        if entry.compression() == zip::CompressionMethod::Stored {
            let start = usize::try_from(entry.data_start()).ok()?;
            let len = usize::try_from(size).ok()?;
            let end = start.checked_add(len)?;
            if end > backing.as_ref().len() {
                return None;
            }
            return Some(SharedBytes::from_external(ArchiveSlice {
                backing: backing.clone(),
                start,
                end,
            }));
        }
        read_entry_capped(&mut entry, size)
            .ok()
            .map(SharedBytes::from)
    }

    /// Serve one `classes/` entry out of a JMOD, inflating it on demand.
    ///
    /// `relative` is the name with the `classes/` prefix already stripped
    /// (e.g. `java/lang/Object.class`). The index probe comes first so a
    /// miss costs a hash lookup and no ZIP work at all — that is the
    /// property the old eager `classes_cache` was bought with 136 MB of
    /// inflated bytes, and it is exactly what `JarFile::entry_index` buys
    /// the JAR path for free.
    ///
    /// Returns `None` when the name is not in this JMOD, when the entry
    /// exceeds [`MAX_UNCOMPRESSED_ENTRY_BYTES`], or when the entry is
    /// corrupt — the same three cases in which the eager cache simply had
    /// no key, so every caller's fall-through behaviour is unchanged.
    fn jmod_class_bytes(
        archive: &JmodArchive,
        backing: &ArchiveBacking,
        class_entry_index: &crate::jmod_index::JmodClassNames,
        relative: &str,
    ) -> Option<SharedBytes> {
        if !class_entry_index.contains(relative) {
            return None;
        }
        let mut name = String::with_capacity(JMOD_CLASSES_PREFIX.len() + relative.len());
        name.push_str(JMOD_CLASSES_PREFIX);
        name.push_str(relative);
        Self::jmod_find_shared(archive, backing, &name)
    }

    /// [`Self::find_shared_in_archive`] for a JMOD: the same bytes from
    /// either [`JmodArchive`] arm. A stored entry is a range view of `size()`
    /// bytes at the data start (no CRC check, as before); a deflated one is
    /// inflated under the same cap and CRC-checked as zip's reader checks it.
    fn jmod_find_shared(
        archive: &JmodArchive,
        backing: &ArchiveBacking,
        name: &str,
    ) -> Option<SharedBytes> {
        match archive {
            JmodArchive::Zip(zip) => Self::find_shared_in_archive(zip, backing, name),
            JmodArchive::Central(index) => Self::central_find_shared(index, backing, name),
        }
    }

    /// [`Self::find_shared_in_archive`] over a central-directory index (JMOD
    /// or jar).
    fn central_find_shared(
        index: &crate::jmod_index::JmodCentralIndex,
        backing: &ArchiveBacking,
        name: &str,
    ) -> Option<SharedBytes> {
        let loc = index.locate(backing.as_ref(), name)?;
        if loc.uncompressed > MAX_UNCOMPRESSED_ENTRY_BYTES {
            return None;
        }
        if loc.stored {
            let len = usize::try_from(loc.uncompressed).ok()?;
            let end = loc.data_start.checked_add(len)?;
            if end > backing.as_ref().len() {
                return None;
            }
            return Some(SharedBytes::from_external(ArchiveSlice {
                backing: backing.clone(),
                start: loc.data_start,
                end,
            }));
        }
        Self::jmod_inflate_checked(backing.as_ref(), &loc).map(SharedBytes::from)
    }

    /// [`Self::find_in_archive`] for a JMOD: the entry's bytes as an owned
    /// `Vec`, read the way zip's `ZipFile` reader reads them (the
    /// `compressed_size` bytes at the data start, inflated if deflated, the
    /// CRC checked at the end, [`read_entry_capped`]'s cap on the output).
    fn jmod_find_vec(
        archive: &JmodArchive,
        backing: &ArchiveBacking,
        name: &str,
    ) -> Option<Vec<u8>> {
        match archive {
            JmodArchive::Zip(zip) => Self::find_in_archive(zip, name),
            JmodArchive::Central(index) => Self::central_find_vec(index, backing, name),
        }
    }

    /// [`Self::find_in_archive`] over a central-directory index (JMOD or jar).
    fn central_find_vec(
        index: &crate::jmod_index::JmodCentralIndex,
        backing: &ArchiveBacking,
        name: &str,
    ) -> Option<Vec<u8>> {
        let loc = index.locate(backing.as_ref(), name)?;
        Self::jmod_inflate_checked(backing.as_ref(), &loc)
    }

    /// [`Self::archive_has_entry`] for a JMOD.
    fn jmod_has_entry(archive: &JmodArchive, name: &str) -> bool {
        match archive {
            JmodArchive::Zip(zip) => Self::archive_has_entry(zip, name),
            JmodArchive::Central(index) => index.contains(name),
        }
    }

    /// Read one located JMOD entry as zip 2.4.2's `ZipFile` reader would:
    /// `Decompressor` (stored = the raw bytes, deflated = a raw-deflate
    /// stream) under a `Crc32Reader`, drained through [`read_entry_capped`].
    /// A CRC mismatch, a corrupt stream, or an output over the cap is `None`,
    /// the three cases in which the `ZipArchive` path answered `None`.
    fn jmod_inflate_checked(
        zip: &[u8],
        loc: &crate::jmod_index::JmodEntryLocation,
    ) -> Option<Vec<u8>> {
        let raw = zip.get(loc.data_start..loc.data_start.checked_add(loc.compressed)?)?;
        let data = if loc.stored {
            let mut reader = raw;
            read_entry_capped(&mut reader, loc.uncompressed).ok()?
        } else {
            let mut reader = flate2::read::DeflateDecoder::new(raw);
            read_entry_capped(&mut reader, loc.uncompressed).ok()?
        };
        // Past the cap `read_entry_capped` already refused; below it the whole
        // stream was drained, so the CRC is over all of it, as zip's is.
        (crc32fast::hash(&data) == loc.crc32).then_some(data)
    }

    /// JMOD file header prefix: `JM` (0x4A 0x4D).
    const JMOD_MAGIC_PREFIX: [u8; 2] = [0x4A, 0x4D];

    /// Known JMOD major versions we support. JDK 9-25 all use major version 1.
    const JMOD_SUPPORTED_MAJOR: u8 = 0x01;

    /// Maximum JMOD minor version we accept (currently 0, future-proofed to 0xFF).
    const JMOD_MAX_MINOR: u8 = 0xFF;

    /// Load a JDK 9+ JMOD file. JMOD is a ZIP archive with a 4-byte header:
    ///   - Bytes 0-1: Magic `JM` (0x4A 0x4D)
    ///   - Byte 2: Major version (currently 0x01 for all JDK 9-25)
    ///   - Byte 3: Minor version (currently 0x00)
    ///
    /// Class files inside are stored under `classes/` (e.g. `classes/java/lang/Object.class`).
    ///
    /// Every entry name is read from the central directory into
    /// `class_entry_index`; **no entry is inflated here**. Class bytes are
    /// produced on demand by [`Self::jmod_class_bytes`].
    ///
    /// PERF (2026-07-26 boot-classpath-lazy): this used to pre-extract every
    /// `classes/` entry into a `HashMap<String, SharedBytes>`. That made
    /// existence probes free but paid for it up front — on a stock JDK 25 the
    /// 70 JMODs `discover_boot_classpath` puts on the boot classpath meant
    /// 27,962 deflate passes and ~136 MB resident before the first Java class
    /// loaded. The name index gives the same free existence probe (that was
    /// the actual point of the cache: the old `find_in_archive(..).is_some()`
    /// callers inflated an entry to answer a yes/no, which is where the
    /// "~30 s for 200 classes" debug-build figure came from) at ~1 % of the
    /// memory and none of the up-front CPU. It mirrors `build_archive_entry_index`
    /// on the JAR path.
    ///
    /// Repeat requests for the same class do not re-inflate in practice:
    /// `ClassManager::class_bytes_cache` already memoizes class bytes behind a
    /// 16 MiB FIFO cap, which is the right place for that bound to live.
    fn load_jmod(path: &Path) -> Result<ClassPathEntry, String> {
        Self::load_jmod_with(path, loader_flags().jmod_lazy_index)
    }

    /// [`Self::load_jmod`] with the `CRATONVM_JMOD_LAZY_INDEX` decision made
    /// by the caller (`lazy_index`), so a test can open one file both ways.
    fn load_jmod_with(path: &Path, lazy_index: bool) -> Result<ClassPathEntry, String> {
        // JMOD files are ordinary classpath archives here; read them through
        // the same owned, mutation-detecting helper as JARs.
        let data = read_archive_for_classpath(path).map_err(|e| format!("failed to read: {e}"))?;
        if data.len() < 4 {
            return Err("file too small to be a valid JMOD".to_string());
        }
        // Validate magic prefix
        if data[..2] != Self::JMOD_MAGIC_PREFIX {
            return Err(format!(
                "not a valid JMOD file (expected magic 0x4A4D, got 0x{:02X}{:02X})",
                data[0], data[1]
            ));
        }
        // Validate major version
        let major = data[2];
        let minor = data[3];
        if major != Self::JMOD_SUPPORTED_MAJOR {
            return Err(format!(
                "unsupported JMOD major version {major} (expected {}); \
                 this may be from a future JDK",
                Self::JMOD_SUPPORTED_MAJOR
            ));
        }
        if minor > Self::JMOD_MAX_MINOR {
            return Err(format!(
                "unsupported JMOD minor version {minor} (max {})",
                Self::JMOD_MAX_MINOR
            ));
        }
        debug!("JMOD {} version {}.{}", path.display(), major, minor);
        // Strip the 4-byte JMOD magic prefix to get standard ZIP data
        let zip_bytes = SharedBytes::from_external(ArchiveSlice {
            backing: data.clone(),
            start: 4,
            end: data.as_ref().len(),
        });
        let zip_data = ArchiveBacking::Shared(zip_bytes);

        // zr z8/fm: index from the central directory alone, so the open
        // touches only the directory's pages (`crate::jmod_index`). The
        // `ZipArchive` below reads every entry's local header at open -- one
        // page fault per entry on a mapped file. Kill: CRATONVM_JMOD_LAZY_INDEX=0.
        if lazy_index {
            if let Some(index) = crate::jmod_index::JmodCentralIndex::parse(zip_data.as_ref()) {
                // One copy of the names, shared by both views and the index.
                let class_entry_index =
                    crate::jmod_index::JmodClassNames(Arc::clone(index.names()));
                let all_entry_names = crate::jmod_index::JmodAllNames(Arc::clone(index.names()));
                debug!(
                    "Loaded JMOD {} ({} entries, {} classes indexed, central directory only)",
                    path.display(),
                    index.len(),
                    class_entry_index.len()
                );
                return Ok(ClassPathEntry::JmodFile {
                    path: path.to_path_buf(),
                    class_entry_index,
                    all_entry_names,
                    archive: JmodArchive::Central(index),
                    backing: zip_data,
                });
            }
        }

        let cursor = Cursor::new(zip_data.clone());
        let mut archive =
            ZipArchive::new(cursor).map_err(|e| format!("failed to parse ZIP inside JMOD: {e}"))?;

        let total_entries = archive.len();

        // Index every entry name from the central directory. `by_index_raw`
        // builds the entry reader without touching compressed data, so this
        // loop reads no payload bytes at all (`ZipArchive::new` above has
        // already read every LOCAL HEADER, though: see `JmodArchive`).
        let mut all_entry_names = Vec::with_capacity(total_entries);
        for i in 0..total_entries {
            let name = match archive.by_index_raw(i) {
                Ok(entry) => entry.name().to_string(),
                Err(_) => continue,
            };
            all_entry_names.push(name);
        }
        let names = Arc::new(
            crate::jmod_index::JmodNames::build(&all_entry_names)
                .ok_or_else(|| "JMOD entry names too large".to_string())?,
        );
        drop(all_entry_names);
        let class_entry_index = crate::jmod_index::JmodClassNames(Arc::clone(&names));
        let all_entry_names = crate::jmod_index::JmodAllNames(names);

        debug!(
            "Loaded JMOD {} ({} entries, {} classes indexed, none inflated)",
            path.display(),
            total_entries,
            class_entry_index.len()
        );

        Ok(ClassPathEntry::JmodFile {
            path: path.to_path_buf(),
            class_entry_index,
            all_entry_names,
            archive: JmodArchive::Zip(Mutex::new(archive)),
            backing: zip_data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn jar_subdirectory_spec_accepts_war_archives() {
        assert_eq!(
            parse_jar_subdir_spec("/apps/test.war!/WEB-INF/classes/"),
            Some(("/apps/test.war".to_string(), "WEB-INF/classes/".to_string()))
        );
    }

    // -----------------------------------------------------------------------
    // Glob parse-once refactor (2026-07-26 classpath-scan audit)
    // -----------------------------------------------------------------------

    /// Hoisting the glob parse out of the per-candidate filter must not change
    /// which names match. Compare the batch API against the per-candidate
    /// predicate it replaced, over globs that exercise `*`, `?`, directory
    /// prefixes, the root, and the reject paths (no wildcard, wildcard in the
    /// prefix, empty pattern, unsafe prefix).
    #[test]
    fn matching_resource_entry_names_agrees_with_per_candidate_predicate() {
        let names: Vec<String> = [
            "java/lang/Object.class",
            "java/lang/String.class",
            "java/lang/invoke/MethodHandle.class",
            "java/util/List.class",
            "Root.class",
            "java/lang/notes.txt",
            "java/lang/",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        for glob in [
            "java/lang/*.class",
            "java/lang/*",
            "java/lang/?bject.class",
            "java/util/*.class",
            "*.class",
            "java/*/List.class",      // wildcard in the prefix → unparseable
            "java/lang/Object.class", // no wildcard → unparseable
            "java/lang/",             // empty pattern → unparseable
            "/java/lang/*",           // unsafe prefix → unparseable
        ] {
            let batch = ClassPath::matching_resource_entry_names(names.iter(), glob);
            let mut expected: Vec<String> = names
                .iter()
                .filter(|n| resource_name_matches_simple_glob(glob, n))
                .cloned()
                .collect();
            expected.sort();
            assert_eq!(batch, expected, "glob {glob:?} changed meaning");
        }
    }

    /// The parsed and unparsed forms must be the same predicate.
    #[test]
    fn parsed_glob_predicate_matches_unparsed_form() {
        let glob = "java/lang/*.class";
        let (prefix, pattern) = simple_resource_glob(glob).expect("glob must parse");
        for candidate in [
            "java/lang/Object.class",
            "java/lang/invoke/MethodHandle.class",
            "java/util/List.class",
            "java/lang/",
            "",
        ] {
            assert_eq!(
                resource_name_matches_parsed_glob(prefix, pattern, candidate),
                resource_name_matches_simple_glob(glob, candidate),
                "candidate {candidate:?} disagreed"
            );
        }
    }

    /// `archive_has_entry` must answer exactly what `find_in_archive` answers,
    /// without paying for the inflate. Uses a Deflated entry so the two paths
    /// genuinely differ in work done.
    #[test]
    fn archive_has_entry_agrees_with_find_in_archive() {
        let dir = std::env::temp_dir().join("cratonvm_archive_has_entry");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let jar_path = dir.join("probe.jar");
        {
            let file = fs::File::create(&jar_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zip.start_file("pkg/present.txt", options).unwrap();
            zip.write_all(&b"payload".repeat(512)).unwrap();
            zip.finish().unwrap();
        }
        let backing = read_archive_for_classpath(&jar_path).unwrap();
        let archive: Mutex<SharedArchive> =
            Mutex::new(ZipArchive::new(Cursor::new(backing)).unwrap());

        for name in ["pkg/present.txt", "pkg/absent.txt", "", "pkg/"] {
            assert_eq!(
                ClassPath::archive_has_entry(&archive, name),
                ClassPath::find_in_archive(&archive, name).is_some(),
                "existence verdict for {name:?} diverged"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn classpath_new_finds_resource_in_war_subdirectory() {
        let dir = std::env::temp_dir().join("cratonvm_war_subdirectory_classpath");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let war_path = dir.join("test.war");
        {
            let file = fs::File::create(&war_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file("WEB-INF/classes/test.txt", options).unwrap();
            zip.write_all(b"test resource").unwrap();
            zip.finish().unwrap();
        }

        let spec = format!("{}!/WEB-INF/classes/", war_path.display());
        let cp = ClassPath::new(&[spec]);
        assert_eq!(
            cp.find_resource("test.txt"),
            Some(b"test resource".to_vec())
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn classpath_new_expands_manifest_class_path() {
        let dir = std::env::temp_dir().join("cratonvm_manifest_class_path");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let dependency = dir.join("dependency.jar");
        let launcher = dir.join("launcher.jar");
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        {
            let file = fs::File::create(&dependency).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file("META-INF/services/example.Service", options)
                .unwrap();
            zip.write_all(b"example.ServiceImpl\n").unwrap();
            zip.finish().unwrap();
        }
        {
            let file = fs::File::create(&launcher).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file("META-INF/MANIFEST.MF", options).unwrap();
            zip.write_all(b"Manifest-Version: 1.0\r\nClass-Path: dependency.jar\r\n\r\n")
                .unwrap();
            zip.finish().unwrap();
        }

        let cp = ClassPath::new(&[launcher.to_string_lossy().into_owned()]);
        assert_eq!(cp.entry_count(), 2, "manifest dependency must be added");
        assert_eq!(
            cp.find_resource("META-INF/services/example.Service"),
            Some(b"example.ServiceImpl\n".to_vec())
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_class_path_cycle_is_not_reloaded() {
        let dir = std::env::temp_dir().join("cratonvm_manifest_class_path_cycle");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let first = dir.join("first.jar");
        let second = dir.join("second.jar");
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        for (path, dependency) in [(&first, "second.jar"), (&second, "first.jar")] {
            let file = fs::File::create(path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            zip.start_file("META-INF/MANIFEST.MF", options).unwrap();
            zip.write_all(
                format!("Manifest-Version: 1.0\r\nClass-Path: {dependency}\r\n\r\n").as_bytes(),
            )
            .unwrap();
            zip.finish().unwrap();
        }

        let cp = ClassPath::new(&[first.to_string_lossy().into_owned()]);
        assert_eq!(
            cp.entry_count(),
            2,
            "manifest cycle must terminate without duplicates"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_classpath_finds_nothing() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("java/lang/Object").is_err());
    }

    #[test]
    fn nonexistent_jar_skipped() {
        let cp = ClassPath::new(&["nonexistent.jar".to_string()]);
        assert!(cp.is_empty());
    }

    #[test]
    fn load_class_from_jar() {
        // Create a temporary JAR file with a fake .class entry
        let dir = std::env::temp_dir().join("cratonvm_test_jar");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("test.jar");

        let file = fs::File::create(&jar_path).unwrap();
        let mut zip_writer = zip::ZipWriter::new(file);

        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip_writer
            .start_file("com/example/Hello.class", options)
            .unwrap();
        zip_writer
            .write_all(b"\xCA\xFE\xBA\xBE_fake_class_data")
            .unwrap();
        zip_writer.finish().unwrap();

        // Load from the JAR
        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        assert!(!cp.is_empty());

        let data = cp.find_class("com/example/Hello").unwrap();
        assert_eq!(&data[..4], b"\xCA\xFE\xBA\xBE");
        assert!(data.len() > 4);
        assert!(
            data.is_external(),
            "stored ZIP entries should retain the mapped archive backing"
        );

        // Not found in JAR
        assert!(cp.find_class("com/example/Missing").is_err());

        // Cleanup
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn compressed_class_uses_shared_owned_bytes() {
        let dir = std::env::temp_dir().join("cratonvm_test_deflated_jar");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let jar_path = dir.join("test.jar");
        {
            let file = fs::File::create(&jar_path).unwrap();
            let mut zip_writer = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zip_writer
                .start_file("com/example/Compressed.class", options)
                .unwrap();
            zip_writer
                .write_all(b"\xCA\xFE\xBA\xBE_compressed_class_data")
                .unwrap();
            zip_writer.finish().unwrap();
        }

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let first = cp.find_class("com/example/Compressed").unwrap();
        let second = first.clone();
        assert_eq!(first.as_ref(), second.as_ref());
        assert!(
            !first.is_external() && !second.is_external(),
            "deflated entries should use reference-counted inflated storage"
        );
        assert_eq!(first.as_ref().as_ptr(), second.as_ref().as_ptr());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn debug_format() {
        let cp = ClassPath::new(&[]);
        let debug = format!("{cp:?}");
        assert!(debug.contains("ClassPath"));
    }

    // -- Path traversal protection tests --

    #[test]
    fn path_traversal_double_dot_rejected() {
        let cp = ClassPath::new(&[]);
        let result = cp.find_class("java/lang/../../etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn path_traversal_absolute_path_rejected() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("/etc/passwd").is_err());
    }

    #[test]
    fn path_traversal_backslash_rejected() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("\\\\server\\share").is_err());
    }

    #[test]
    fn single_windows_separator_component_rejected_everywhere() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("pkg\\Foo").is_err());
        assert!(cp.find_class_source_path("pkg\\Foo").is_none());
        assert!(cp.find_class_code_source_info("pkg\\Foo").is_none());
        assert!(cp.find_resource("pkg\\config.properties").is_none());
        assert!(cp
            .find_all_resource_bytes("pkg\\config.properties")
            .is_empty());
        assert!(cp
            .find_all_resource_urls("pkg\\config.properties")
            .is_empty());
    }

    #[test]
    fn classpath_metadata_change_detects_length_change() {
        let dir = std::env::temp_dir().join("cratonvm_classpath_mutation_guard");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("guard.jar");

        fs::write(&path, b"before").unwrap();
        let before = fs::metadata(&path).unwrap();
        fs::write(&path, b"after mutation").unwrap();
        let after = fs::metadata(&path).unwrap();

        assert!(classpath_file_metadata_changed(&before, &after));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_traversal_null_byte_rejected() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("java/lang/Object\0.evil").is_err());
    }

    #[test]
    fn valid_class_name_format_accepted() {
        // Valid names should not be rejected (they'll fail with ClassNotFound
        // because there's no classpath entry, but NOT due to validation)
        let cp = ClassPath::new(&[]);
        let result = cp.find_class("java/lang/Object");
        assert!(
            matches!(result, Err(ClassFileError::ClassNotFound { .. })),
            "expected ClassNotFound, got {result:?}"
        );
    }

    #[test]
    fn find_resource_from_directory() {
        let dir = std::env::temp_dir().join("cratonvm_test_resource_dir");
        let _ = fs::create_dir_all(&dir);
        let resource_path = dir.join("hello.txt");
        fs::write(&resource_path, b"hello world").unwrap();

        let cp = ClassPath::new(&[dir.to_string_lossy().into_owned()]);
        let data = cp
            .find_resource("hello.txt")
            .expect("resource should be found");
        assert_eq!(data, b"hello world");

        assert!(cp.find_resource("missing.txt").is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_resource_simple_glob_from_directory() {
        let dir = std::env::temp_dir().join("cratonvm_test_resource_dir_glob");
        let _ = fs::remove_dir_all(&dir);
        let web_inf = dir.join("org/springframework/web/context/WEB-INF");
        fs::create_dir_all(&web_inf).unwrap();
        fs::write(web_inf.join("myplaceholder.properties"), b"name=Rod").unwrap();
        fs::write(web_inf.join("myoverride.properties"), b"age=42").unwrap();
        fs::write(web_inf.join("other.properties"), b"ignored=true").unwrap();

        let cp = ClassPath::new(&[dir.to_string_lossy().into_owned()]);
        let pattern = "org/springframework/web/context/WEB-INF/myplace*.properties";
        let data = cp.find_resource(pattern).expect("globbed resource");
        assert_eq!(data, b"name=Rod");

        let bytes = cp.find_all_resource_bytes(pattern);
        assert_eq!(bytes, vec![b"name=Rod".to_vec()]);

        let urls = cp.find_all_resource_urls(pattern);
        assert_eq!(urls.len(), 1);
        assert!(
            urls[0].ends_with("/org/springframework/web/context/WEB-INF/myplaceholder.properties"),
            "unexpected URL for globbed resource: {:?}",
            urls
        );

        assert!(
            cp.find_resource("org/springframework/web/*/WEB-INF/myplace*.properties")
                .is_none(),
            "wildcards in directory segments are intentionally unsupported"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_resource_from_jar() {
        let dir = std::env::temp_dir().join("cratonvm_test_resource_jar");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("resources.jar");

        let file = fs::File::create(&jar_path).unwrap();
        let mut zip_writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip_writer.start_file("data/words.txt", options).unwrap();
        zip_writer.write_all(b"word1\nword2\nword3").unwrap();
        zip_writer.finish().unwrap();

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp.find_resource("data/words.txt").expect("resource in JAR");
        assert_eq!(data, b"word1\nword2\nword3");

        // Leading slash is stripped
        let data2 = cp
            .find_resource("/data/words.txt")
            .expect("leading slash stripped");
        assert_eq!(data2, b"word1\nword2\nword3");

        assert!(cp.find_resource("data/missing.txt").is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    /// The short-circuiting walk must answer EXACTLY what the whole-list walk
    /// answers first — that equivalence is the licence for
    /// `ClassLoader.getResource` to stop at the first hit rather than build
    /// every URL and discard all but element 0.
    ///
    /// Asserted on a classpath where the name hits in MORE than one entry, and
    /// where the first hit is not the last entry: a fixture with a single hit
    /// cannot tell "returns the first" from "returns the only one", and one
    /// where the hit is last cannot tell "stopped early" from "walked
    /// everything".
    #[test]
    fn the_incremental_walk_returns_what_the_whole_list_walk_returns_first() {
        let dir =
            std::env::temp_dir().join(format!("cratonvm_test_first_url_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let first = dir.join("first");
        let second = dir.join("second");
        let empty = dir.join("empty");
        for d in [&first, &second, &empty] {
            fs::create_dir_all(d.join("pkg")).unwrap();
        }
        fs::write(first.join("pkg/R.txt"), b"from-first").unwrap();
        fs::write(second.join("pkg/R.txt"), b"from-second").unwrap();

        // `empty` last so a walk that does not stop early still has an entry
        // left to visit after the answer is known.
        let cp = ClassPath::new(&[
            first.to_string_lossy().into_owned(),
            second.to_string_lossy().into_owned(),
            empty.to_string_lossy().into_owned(),
        ]);

        let all = cp.find_all_resource_urls("pkg/R.txt");
        assert_eq!(all.len(), 2, "fixture must hit twice, got {all:?}");
        let (incremental, resume) = cp
            .next_resource_url_from("pkg/R.txt", 0)
            .expect("the incremental walk must find it too");
        assert_eq!(
            Some(&incremental),
            all.first(),
            "the two walks disagree on the FIRST url"
        );
        assert_eq!(resume, 1, "it must stop at the entry that answered");

        // A miss agrees too, and costs nothing to state.
        assert!(cp.find_all_resource_urls("pkg/Absent.txt").is_empty());
        assert!(cp.next_resource_url_from("pkg/Absent.txt", 0).is_none());

        // And the gate: a glob name can match several names inside ONE entry,
        // so it must NOT take the incremental path.
        assert!(!ClassPath::name_supports_incremental_scan("pkg/*.txt"));
        assert!(ClassPath::name_supports_incremental_scan("pkg/R.txt"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_resource_path_traversal_rejected() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_resource("../etc/passwd").is_none());
        assert!(cp.find_resource("foo\0bar").is_none());
    }

    // -- Spring Boot fat JAR tests --

    /// Helper: create a Spring Boot fat JAR with:
    /// - META-INF/MANIFEST.MF (with Start-Class)
    /// - BOOT-INF/classes/com/example/App.class
    /// - BOOT-INF/lib/dep.jar (containing org/dep/Util.class)
    /// - org/springframework/boot/loader/JarLauncher.class (launcher at root)
    fn create_fat_jar(jar_path: &Path) {
        let file = fs::File::create(jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        // MANIFEST.MF
        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(
            b"Manifest-Version: 1.0\r\n\
              Main-Class: org.springframework.boot.loader.JarLauncher\r\n\
              Start-Class: com.example.App\r\n\
              Spring-Boot-Classes: BOOT-INF/classes/\r\n\
              Spring-Boot-Lib: BOOT-INF/lib/\r\n",
        )
        .unwrap();

        // Application class in BOOT-INF/classes/
        zip.start_file("BOOT-INF/classes/com/example/App.class", opts)
            .unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_app_class").unwrap();

        // Resource in BOOT-INF/classes/
        zip.start_file("BOOT-INF/classes/application.properties", opts)
            .unwrap();
        zip.write_all(b"server.port=8080").unwrap();

        // Nested dependency JAR in BOOT-INF/lib/
        let mut dep_jar_buf = Vec::new();
        {
            let cursor = Cursor::new(&mut dep_jar_buf);
            let mut dep_zip = zip::ZipWriter::new(cursor);
            dep_zip.start_file("org/dep/Util.class", opts).unwrap();
            dep_zip.write_all(b"\xCA\xFE\xBA\xBE_util_class").unwrap();
            dep_zip
                .start_file("META-INF/services/org.dep.SPI", opts)
                .unwrap();
            dep_zip.write_all(b"org.dep.SpiImpl").unwrap();
            dep_zip.finish().unwrap();
        }
        zip.start_file("BOOT-INF/lib/dep-1.0.jar", opts).unwrap();
        zip.write_all(&dep_jar_buf).unwrap();

        // Launcher class at JAR root
        zip.start_file("org/springframework/boot/loader/JarLauncher.class", opts)
            .unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_launcher").unwrap();

        zip.finish().unwrap();
    }

    #[test]
    fn fat_jar_detects_spring_boot_structure() {
        let dir = std::env::temp_dir().join("cratonvm_test_fatjar_detect");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);

        // Should have multiple entries: NestedDirectory + NestedJar(s) + outer JarFile
        assert!(
            cp.entry_count() >= 3,
            "expected >=3 entries, got {}",
            cp.entry_count()
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fat_jar_loads_class_from_boot_inf_classes() {
        let dir = std::env::temp_dir().join("cratonvm_test_fatjar_classes");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp
            .find_class("com/example/App")
            .expect("should find App.class from BOOT-INF/classes/");
        assert_eq!(&data[..4], b"\xCA\xFE\xBA\xBE");
        assert!(data.ends_with(b"_app_class"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fat_jar_loads_class_from_nested_jar() {
        let dir = std::env::temp_dir().join("cratonvm_test_fatjar_nested");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp
            .find_class("org/dep/Util")
            .expect("should find Util.class from BOOT-INF/lib/dep-1.0.jar");
        assert_eq!(&data[..4], b"\xCA\xFE\xBA\xBE");
        assert!(data.ends_with(b"_util_class"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fat_jar_loads_launcher_from_root() {
        let dir = std::env::temp_dir().join("cratonvm_test_fatjar_root");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp
            .find_class("org/springframework/boot/loader/JarLauncher")
            .expect("should find JarLauncher from JAR root");
        assert_eq!(&data[..4], b"\xCA\xFE\xBA\xBE");
        assert!(data.ends_with(b"_launcher"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fat_jar_loads_resource_from_boot_inf_classes() {
        let dir = std::env::temp_dir().join("cratonvm_test_fatjar_resource");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp
            .find_resource("application.properties")
            .expect("should find application.properties from BOOT-INF/classes/");
        assert_eq!(data, b"server.port=8080");

        let _ = fs::remove_dir_all(&dir);
    }

    /// Without emulation (`--jdk-only`'s application class path) a fat JAR
    /// is what HotSpot sees: its root, and nothing under `BOOT-INF/`, neither
    /// as entries of their own nor through the prefixed class lookup.
    #[test]
    fn fat_jar_without_emulation_is_its_root_only() {
        let dir = std::env::temp_dir().join("cratonvm_test_fatjar_no_emulation");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp =
            ClassPath::with_fat_jar_emulation(&[jar_path.to_string_lossy().into_owned()], false);
        assert_eq!(cp.entry_count(), 1, "the outer JAR alone");
        let data = cp
            .find_class("org/springframework/boot/loader/JarLauncher")
            .expect("the root still loads");
        assert!(data.ends_with(b"_launcher"));
        assert!(
            cp.find_class("com/example/App").is_err(),
            "BOOT-INF/classes is Spring's launcher's to load, not the class path's"
        );
        assert!(cp.find_class("org/dep/Util").is_err(), "nor BOOT-INF/lib");
        assert!(cp.find_resource("application.properties").is_none());
        assert!(cp.find_resource("META-INF/services/org.dep.SPI").is_none());
        assert_eq!(
            cp.find_resource("BOOT-INF/classes/application.properties")
                .as_deref(),
            Some(&b"server.port=8080"[..]),
            "the archive's own entries stay readable by their real names"
        );

        let emulated = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        assert!(
            emulated.find_class("com/example/App").is_ok(),
            "control: the default class path still emulates"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// `nested_archive_entry_names`: the nested jar's exact central directory
    /// (the walk index of `native-builtins` `classloader::nested_jar_entry_index`),
    /// `None` for an entry that is missing, not an archive, or unsafe.
    #[test]
    fn nested_archive_entry_names_index_one_nested_jar() {
        let dir = std::env::temp_dir().join("cratonvm_test_nested_entry_names");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let names = ClassPath::nested_archive_entry_names(&jar_path, "BOOT-INF/lib/dep-1.0.jar")
            .expect("the nested jar is indexed");
        assert!(names.contains("org/dep/Util.class"));
        assert!(names.contains("META-INF/services/org.dep.SPI"));
        assert!(
            !names.contains("com/example/App.class"),
            "the outer jar's entries are not its"
        );
        assert_eq!(names.len(), 2);
        assert!(
            ClassPath::nested_archive_entry_names(&jar_path, "BOOT-INF/lib/none.jar").is_none()
        );
        assert!(
            ClassPath::nested_archive_entry_names(
                &jar_path,
                "BOOT-INF/classes/application.properties"
            )
            .is_none(),
            "not an archive"
        );
        assert!(ClassPath::nested_archive_entry_names(&jar_path, "../x.jar").is_none());
        assert!(ClassPath::nested_archive_entry_names(&dir.join("absent.jar"), "a.jar").is_none());

        // A Multi-Release nested jar: a versioned-only entry is indexed under
        // its unversioned name too (the walk index must never call it absent).
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let mut mr = Vec::new();
        {
            let mut z = zip::ZipWriter::new(Cursor::new(&mut mr));
            z.start_file("META-INF/versions/17/p/V.class", opts)
                .unwrap();
            z.write_all(b"\xCA\xFE\xBA\xBE").unwrap();
            z.start_file("META-INF/versions/x/q.txt", opts).unwrap();
            z.write_all(b"x").unwrap();
            z.finish().unwrap();
        }
        let mr_outer = dir.join("mr.jar");
        {
            let mut z = zip::ZipWriter::new(fs::File::create(&mr_outer).unwrap());
            z.start_file("BOOT-INF/lib/mr.jar", opts).unwrap();
            z.write_all(&mr).unwrap();
            z.finish().unwrap();
        }
        let names =
            ClassPath::nested_archive_entry_names(&mr_outer, "BOOT-INF/lib/mr.jar").unwrap();
        assert!(names.contains("p/V.class"), "{names:?}");
        assert!(names.contains("META-INF/versions/17/p/V.class"));
        assert!(!names.contains("q.txt"), "not a numeric version directory");

        let _ = fs::remove_dir_all(&dir);
    }

    /// `nested_archive_entry_index` (interpreter round i2 wave 65, lane L5):
    /// the same names as `nested_archive_entry_names`, and a stamp that tells
    /// a fat jar rewritten at the same path (the index holds no mapping, so
    /// the file can be rewritten on every host).
    #[test]
    fn nested_archive_entry_index_notices_a_rewritten_outer_jar() {
        fn fat_jar(path: &Path, nested_entries: &[&str]) {
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let mut inner = Vec::new();
            {
                let mut z = zip::ZipWriter::new(Cursor::new(&mut inner));
                for name in nested_entries {
                    z.start_file(*name, opts).unwrap();
                    z.write_all(b"\xCA\xFE\xBA\xBE").unwrap();
                }
                z.finish().unwrap();
            }
            let mut z = zip::ZipWriter::new(fs::File::create(path).unwrap());
            z.start_file("BOOT-INF/lib/dep.jar", opts).unwrap();
            z.write_all(&inner).unwrap();
            z.finish().unwrap();
        }
        let dir = std::env::temp_dir().join("cratonvm_test_nested_entry_index_stamp");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        fat_jar(&jar_path, &["p/Old.class"]);

        let index = ClassPath::nested_archive_entry_index(&jar_path, "BOOT-INF/lib/dep.jar")
            .expect("indexed");
        assert!(index.contains("p/Old.class"));
        assert!(!index.contains("p/New.class"));
        assert_eq!(index.len(), 1);
        assert!(!index.is_empty());
        assert!(index.outer_unchanged());

        // Rewritten at the same path with one more class: a longer file.
        fat_jar(&jar_path, &["p/Old.class", "p/New.class"]);
        assert!(!index.outer_unchanged(), "the rewrite is noticed");
        let rebuilt = ClassPath::nested_archive_entry_index(&jar_path, "BOOT-INF/lib/dep.jar")
            .expect("re-indexed");
        assert!(rebuilt.contains("p/New.class"));
        assert!(rebuilt.outer_unchanged());

        assert!(
            ClassPath::nested_archive_entry_index(&jar_path, "BOOT-INF/lib/none.jar").is_none()
        );
        assert!(ClassPath::nested_archive_entry_index(&dir.join("absent.jar"), "a.jar").is_none());
        assert!(
            ClassPath::nested_archive_entry_index(&dir, "a.jar").is_none(),
            "a directory"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// `OuterArchive` (interpreter round i2 wave 66, lane L5,
    /// `CRATONVM_NESTED_JAR_SHARED_OUTER`): one open of the outer fat jar
    /// answers every nested jar exactly as the per-spec opens do -- the same
    /// walk-index names and the same entry bytes, for a stored and a deflated
    /// nested jar -- carries the outer stamp, and tells a rewrite.
    #[test]
    fn outer_archive_answers_every_nested_jar_as_the_per_spec_opens_do() {
        fn inner(entries: &[(&str, &[u8])]) -> Vec<u8> {
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            let mut out = Vec::new();
            {
                let mut z = zip::ZipWriter::new(Cursor::new(&mut out));
                for (name, data) in entries {
                    z.start_file(*name, opts).unwrap();
                    z.write_all(data).unwrap();
                }
                z.finish().unwrap();
            }
            out
        }
        fn fat_jar(path: &Path, extra: bool) {
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let deflated = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            let a = inner(&[
                ("a/A.class", &b"\xCA\xFE\xBA\xBEA"[..]),
                ("a/r.txt", &b"alpha"[..]),
            ]);
            let mut b_entries: Vec<(&str, &[u8])> = vec![("b/B.class", &b"\xCA\xFE\xBA\xBEB"[..])];
            if extra {
                b_entries.push(("b/C.class", &b"\xCA\xFE\xBA\xBEC"[..]));
            }
            let b = inner(&b_entries);
            let mut z = zip::ZipWriter::new(fs::File::create(path).unwrap());
            z.start_file("BOOT-INF/classes/app.properties", stored)
                .unwrap();
            z.write_all(b"k=v").unwrap();
            z.start_file("BOOT-INF/lib/a.jar", stored).unwrap();
            z.write_all(&a).unwrap();
            z.start_file("BOOT-INF/lib/b.jar", deflated).unwrap();
            z.write_all(&b).unwrap();
            z.finish().unwrap();
        }
        let dir = std::env::temp_dir().join("cratonvm_test_shared_outer_archive");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        fat_jar(&jar_path, false);

        let outer = OuterArchive::open(&jar_path).expect("opened");
        assert_eq!(outer.path(), jar_path.as_path());
        assert_eq!(outer.entry_count(), 3);
        assert!(outer.unchanged());
        for (nested, present, absent) in [
            ("BOOT-INF/lib/a.jar", "a/A.class", "b/B.class"),
            ("BOOT-INF/lib/b.jar", "b/B.class", "a/A.class"),
        ] {
            let shared = ClassPath::nested_archive_entry_index_in(&outer, nested).expect("indexed");
            let own = ClassPath::nested_archive_entry_names(&jar_path, nested).expect("indexed");
            assert_eq!(shared.len(), own.len(), "{nested}");
            for name in &own {
                assert!(shared.contains(name), "{nested}: {name}");
            }
            assert!(shared.contains(present));
            assert!(!shared.contains(absent));
            assert!(shared.outer_unchanged());

            let shared_bytes = ClassPath::open_nested_archive_in(&outer, nested).expect("opened");
            let own_bytes =
                ClassPath::open_nested_archive(&jar_path, nested, true).expect("opened");
            assert_eq!(shared_bytes.plain_view(), own_bytes.plain_view());
            assert_eq!(
                shared_bytes.entry_bytes(present),
                own_bytes.entry_bytes(present)
            );
            assert!(
                shared_bytes.entry_bytes(present).is_some(),
                "{nested}: {present}"
            );
            assert!(shared_bytes.outer_unchanged());
        }
        assert!(
            ClassPath::nested_archive_entry_index_in(&outer, "BOOT-INF/lib/none.jar").is_none()
        );
        assert!(
            ClassPath::nested_archive_entry_index_in(&outer, "BOOT-INF/classes/app.properties")
                .is_none(),
            "not an archive"
        );
        assert!(ClassPath::nested_archive_entry_index_in(&outer, "../x.jar").is_none());
        assert!(ClassPath::nested_archive_entry_index_in(&outer, "").is_none());
        assert!(ClassPath::open_nested_archive_in(&outer, "BOOT-INF/lib/none.jar").is_none());
        assert!(OuterArchive::open(&dir.join("absent.jar")).is_none());
        assert!(OuterArchive::open(&dir).is_none(), "a directory");

        // Rewritten at the same path with one more class in b.jar. On
        // Windows the outer holds no mapping (read into memory), so the
        // rewrite succeeds there too.
        let index = ClassPath::nested_archive_entry_index_in(&outer, "BOOT-INF/lib/b.jar").unwrap();
        fat_jar(&jar_path, true);
        assert!(!outer.unchanged(), "the rewrite is noticed");
        assert!(!index.outer_unchanged(), "and by what the outer handed out");
        let reopened = OuterArchive::open(&jar_path).expect("reopened");
        let rebuilt =
            ClassPath::nested_archive_entry_index_in(&reopened, "BOOT-INF/lib/b.jar").unwrap();
        assert!(rebuilt.contains("b/C.class"));
        assert!(rebuilt.outer_unchanged());
        drop((outer, reopened));

        let _ = fs::remove_dir_all(&dir);
    }

    /// `OuterArchiveIndex` (interpreter round i2 wave 67, lane L5,
    /// `CRATONVM_NESTED_JAR_OUTER_INDEX`): the outer central directory held
    /// without the outer bytes answers every nested jar as the per-spec opens
    /// do, holds no open file between asks, and refuses -- never crashes on
    /// -- a fat jar truncated, rewritten, or rewritten in place with the same
    /// length and modification time, then deleted.
    #[test]
    fn outer_archive_index_reads_each_nested_jar_at_the_ask() {
        fn inner(entries: &[(&str, &[u8])]) -> Vec<u8> {
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            let mut out = Vec::new();
            {
                let mut z = zip::ZipWriter::new(Cursor::new(&mut out));
                for (name, data) in entries {
                    z.start_file(*name, opts).unwrap();
                    z.write_all(data).unwrap();
                }
                z.finish().unwrap();
            }
            out
        }
        fn fat_jar(path: &Path, extra: bool) {
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let deflated = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            let a = inner(&[
                ("a/A.class", &b"\xCA\xFE\xBA\xBEA"[..]),
                ("a/r.txt", &b"alpha"[..]),
            ]);
            let mut b_entries: Vec<(&str, &[u8])> = vec![("b/B.class", &b"\xCA\xFE\xBA\xBEB"[..])];
            if extra {
                b_entries.push(("b/C.class", &b"\xCA\xFE\xBA\xBEC"[..]));
            }
            let b = inner(&b_entries);
            let mut z = zip::ZipWriter::new(fs::File::create(path).unwrap());
            z.start_file("BOOT-INF/classes/app.properties", stored)
                .unwrap();
            z.write_all(b"k=v").unwrap();
            z.start_file("BOOT-INF/lib/a.jar", stored).unwrap();
            z.write_all(&a).unwrap();
            z.start_file("BOOT-INF/lib/b.jar", deflated).unwrap();
            z.write_all(&b).unwrap();
            z.finish().unwrap();
        }
        let dir = std::env::temp_dir().join("cratonvm_test_outer_archive_index");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        fat_jar(&jar_path, false);

        let outer = OuterArchiveIndex::open(&jar_path).expect("parsed");
        assert_eq!(outer.path(), jar_path.as_path());
        assert_eq!(outer.entry_count(), 3);
        assert!(outer.unchanged());
        assert!(!outer.holds_open_file(), "closed after the parse");
        for (nested, present, absent) in [
            ("BOOT-INF/lib/a.jar", "a/A.class", "b/B.class"),
            ("BOOT-INF/lib/b.jar", "b/B.class", "a/A.class"),
        ] {
            let index =
                ClassPath::nested_archive_entry_index_in_index(&outer, nested).expect("indexed");
            let own = ClassPath::nested_archive_entry_names(&jar_path, nested).expect("indexed");
            assert_eq!(index.len(), own.len(), "{nested}");
            for name in &own {
                assert!(index.contains(name), "{nested}: {name}");
            }
            assert!(index.contains(present));
            assert!(!index.contains(absent));
            assert!(index.outer_unchanged());
            assert!(!outer.holds_open_file(), "closed after the ask");

            let bytes = ClassPath::open_nested_archive_in_index(&outer, nested).expect("opened");
            let own_bytes =
                ClassPath::open_nested_archive(&jar_path, nested, true).expect("opened");
            assert_eq!(bytes.plain_view(), own_bytes.plain_view());
            assert_eq!(bytes.entry_bytes(present), own_bytes.entry_bytes(present));
            assert!(bytes.entry_bytes(present).is_some(), "{nested}: {present}");
            assert!(bytes.outer_unchanged());
            drop(own_bytes);
        }
        assert!(
            ClassPath::nested_archive_entry_index_in_index(&outer, "BOOT-INF/lib/none.jar")
                .is_none()
        );
        assert!(
            ClassPath::nested_archive_entry_index_in_index(
                &outer,
                "BOOT-INF/classes/app.properties"
            )
            .is_none(),
            "not an archive"
        );
        assert!(ClassPath::nested_archive_entry_index_in_index(&outer, "../x.jar").is_none());
        assert!(ClassPath::nested_archive_entry_index_in_index(&outer, "").is_none());
        assert!(ClassPath::open_nested_archive_in_index(&outer, "BOOT-INF/lib/none.jar").is_none());
        assert!(OuterArchiveIndex::open(&dir.join("absent.jar")).is_none());
        assert!(OuterArchiveIndex::open(&dir).is_none(), "a directory");
        assert!(!outer.holds_open_file());

        // A row handed out before the file changes keeps its own (owned)
        // bytes: it is still readable after a truncation, and its stamp
        // tells the change.
        let held = ClassPath::open_nested_archive_in_index(&outer, "BOOT-INF/lib/a.jar").unwrap();

        // Truncated to half: no handle is held, so this works on every host,
        // and the next ask is refused (stamp), never a signal.
        let full = fs::read(&jar_path).unwrap();
        fs::write(&jar_path, &full[..full.len() / 2]).unwrap();
        assert!(!outer.unchanged(), "the truncation is noticed");
        assert!(
            ClassPath::nested_archive_entry_index_in_index(&outer, "BOOT-INF/lib/a.jar").is_none()
        );
        assert!(ClassPath::open_nested_archive_in_index(&outer, "BOOT-INF/lib/b.jar").is_none());
        // Half a fat jar can still parse: a STORED nested jar's own end-of-
        // central-directory record lies in the first half, so the zip reader
        // may find that inner directory. Whatever it finds is not the outer
        // jar the index was built from.
        if let Some(half) = OuterArchiveIndex::open(&jar_path) {
            assert_ne!(
                half.entry_count(),
                outer.entry_count(),
                "half a zip is not the fat jar"
            );
        }
        assert!(!held.outer_unchanged());
        assert_eq!(
            held.entry_bytes("a/r.txt").as_deref(),
            Some(&b"alpha"[..]),
            "owned bytes"
        );

        // Rewritten with one more class in b.jar: a fresh index sees it.
        fat_jar(&jar_path, true);
        let reparsed = OuterArchiveIndex::open(&jar_path).expect("reparsed");
        let rebuilt =
            ClassPath::nested_archive_entry_index_in_index(&reparsed, "BOOT-INF/lib/b.jar")
                .unwrap();
        assert!(rebuilt.contains("b/C.class"));
        assert!(rebuilt.outer_unchanged());

        // Rewritten IN PLACE with the same length and modification time (one
        // byte of the stored a.jar flipped): the stamp cannot tell, the
        // outer entry's CRC does.
        let data_start = {
            let mut z = ZipArchive::new(fs::File::open(&jar_path).unwrap()).unwrap();
            let entry = z.by_name("BOOT-INF/lib/a.jar").unwrap();
            entry.data_start() as usize
        };
        let mtime = fs::metadata(&jar_path).unwrap().modified().unwrap();
        let mut bytes = fs::read(&jar_path).unwrap();
        bytes[data_start + 8] ^= 0xFF;
        fs::write(&jar_path, &bytes).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&jar_path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        if reparsed.unchanged() {
            assert!(
                ClassPath::open_nested_archive_in_index(&reparsed, "BOOT-INF/lib/a.jar").is_none(),
                "a bad CRC answers None"
            );
        }
        assert!(!reparsed.holds_open_file());

        // Deleted: no handle blocks it (Windows too), and the next ask is
        // refused.
        fs::remove_file(&jar_path).unwrap();
        assert!(!reparsed.unchanged());
        assert!(ClassPath::open_nested_archive_in_index(&reparsed, "BOOT-INF/lib/b.jar").is_none());
        drop((outer, reparsed, held));

        let _ = fs::remove_dir_all(&dir);
    }

    /// `open_nested_archive_directory_in_index` (interpreter round i2 wave
    /// 68, lane L5, `CRATONVM_NESTED_JAR_BYTES_DIRECTORY`): a STORED nested
    /// jar's row keeps only its directory and serves every entry -- small,
    /// stored, deflated, and larger than the read-ahead -- byte for byte as
    /// the owned copy does, with the same index and plain view, holding no
    /// file between asks; a deflated nested jar keeps the copy; a truncated,
    /// rewritten or deleted fat jar answers `None`, never a signal.
    #[test]
    fn a_directory_row_reads_each_entry_from_the_outer_file() {
        let big: Vec<u8> = (0..(3 * NESTED_WINDOW_READ_AHEAD + 17))
            .map(|i| (i * 31 % 251) as u8)
            .collect();
        let big_deflated: Vec<u8> = (0..(2 * NESTED_WINDOW_READ_AHEAD))
            .map(|i| (i % 7) as u8)
            .collect();
        let inner = |extra: bool| {
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let deflated = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            let mut out = Vec::new();
            {
                let mut z = zip::ZipWriter::new(Cursor::new(&mut out));
                z.add_directory("a/", stored).unwrap();
                z.start_file("a/A.class", stored).unwrap();
                z.write_all(b"\xCA\xFE\xBA\xBEA").unwrap();
                z.start_file("a/r.txt", deflated).unwrap();
                z.write_all(b"alpha alpha alpha").unwrap();
                z.start_file("a/Big.class", stored).unwrap();
                z.write_all(&big).unwrap();
                z.start_file("a/Big2.class", deflated).unwrap();
                z.write_all(&big_deflated).unwrap();
                if extra {
                    z.start_file("a/New.class", stored).unwrap();
                    z.write_all(b"\xCA\xFE\xBA\xBEN").unwrap();
                }
                z.finish().unwrap();
            }
            out
        };
        let fat_jar = |path: &Path, extra: bool| {
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let deflated = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            let a = inner(extra);
            let mut z = zip::ZipWriter::new(fs::File::create(path).unwrap());
            z.start_file("BOOT-INF/classes/app.properties", stored)
                .unwrap();
            z.write_all(b"k=v").unwrap();
            z.start_file("BOOT-INF/lib/a.jar", stored).unwrap();
            z.write_all(&a).unwrap();
            z.start_file("BOOT-INF/lib/d.jar", deflated).unwrap();
            z.write_all(&a).unwrap();
            z.finish().unwrap();
        };
        let dir = std::env::temp_dir().join("cratonvm_test_nested_directory_row");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        fat_jar(&jar_path, false);

        let outer = OuterArchiveIndex::open(&jar_path).expect("parsed");
        let row = ClassPath::open_nested_archive_directory_in_index(&outer, "BOOT-INF/lib/a.jar")
            .expect("a directory row");
        assert!(
            !row.holds_nested_bytes(),
            "a stored nested jar: directory only"
        );
        let copy =
            ClassPath::open_nested_archive_in_index(&outer, "BOOT-INF/lib/a.jar").expect("copy");
        assert!(copy.holds_nested_bytes());
        assert_eq!(row.plain_view(), copy.plain_view());
        assert_eq!(row.entry_index, copy.entry_index, "the same index");
        for name in [
            "a/A.class",
            "a/r.txt",
            "a/Big.class",
            "a/Big2.class",
            "a/",
            "a/None.class",
            "",
            "a",
        ] {
            assert_eq!(row.entry_bytes(name), copy.entry_bytes(name), "{name}");
        }
        assert_eq!(row.entry_bytes("a/Big.class").as_deref(), Some(&big[..]));
        assert_eq!(
            row.entry_bytes("a/Big2.class").as_deref(),
            Some(&big_deflated[..])
        );
        assert_eq!(
            row.entry_bytes("a/r.txt").as_deref(),
            Some(&b"alpha alpha alpha"[..])
        );
        // Asked twice, and out of order: the read-ahead never serves stale
        // or misplaced bytes.
        assert_eq!(
            row.entry_bytes("a/A.class").as_deref(),
            Some(&b"\xCA\xFE\xBA\xBEA"[..])
        );
        assert_eq!(row.entry_bytes("a/Big.class").as_deref(), Some(&big[..]));
        let crc = ZipArchive::new(Cursor::new(inner(false)))
            .unwrap()
            .by_name("a/A.class")
            .unwrap()
            .crc32();
        assert_eq!(
            row.entry_bytes_matching("a/A.class", crc, 5).as_deref(),
            Some(&b"\xCA\xFE\xBA\xBEA"[..])
        );
        assert!(row.entry_bytes_matching("a/A.class", crc ^ 1, 5).is_none());
        match &row.archive {
            NestedArchiveReader::Window { state, .. } => {
                let state = state.lock();
                assert!(
                    state.file.is_none() && state.buf.is_empty(),
                    "nothing held between asks"
                );
            }
            NestedArchiveReader::Held(_) => unreachable!(),
        }
        assert!(!outer.holds_open_file());

        // A deflated nested jar: the owned copy, as before.
        let deflated =
            ClassPath::open_nested_archive_directory_in_index(&outer, "BOOT-INF/lib/d.jar")
                .expect("a copy row");
        assert!(deflated.holds_nested_bytes());
        assert_eq!(
            deflated.entry_bytes("a/Big.class").as_deref(),
            Some(&big[..])
        );
        assert!(
            ClassPath::open_nested_archive_directory_in_index(&outer, "BOOT-INF/lib/none.jar")
                .is_none()
        );
        assert!(
            ClassPath::open_nested_archive_directory_in_index(
                &outer,
                "BOOT-INF/classes/app.properties"
            )
            .is_none(),
            "not an archive"
        );
        assert!(ClassPath::open_nested_archive_directory_in_index(&outer, "../x.jar").is_none());

        // Truncated under the held row: refused by the stamp, never a signal;
        // the copy still answers from its own bytes.
        let full = fs::read(&jar_path).unwrap();
        fs::write(&jar_path, &full[..full.len() / 2]).unwrap();
        assert!(!row.outer_unchanged());
        assert!(row.entry_bytes("a/A.class").is_none());
        assert!(copy.entry_bytes("a/A.class").is_some());

        // Rewritten: the held row stays refused (its stamp); a row built
        // from a reparsed outer serves the new entry.
        fat_jar(&jar_path, true);
        assert!(row.entry_bytes("a/A.class").is_none());
        let reparsed = OuterArchiveIndex::open(&jar_path).expect("reparsed");
        let fresh =
            ClassPath::open_nested_archive_directory_in_index(&reparsed, "BOOT-INF/lib/a.jar")
                .unwrap();
        assert_eq!(
            fresh.entry_bytes("a/New.class").as_deref(),
            Some(&b"\xCA\xFE\xBA\xBEN"[..])
        );

        // Deleted: no handle blocks it (Windows too).
        fs::remove_file(&jar_path).unwrap();
        assert!(fresh.entry_bytes("a/A.class").is_none());
        drop((outer, reparsed, row, copy, deflated, fresh));

        let _ = fs::remove_dir_all(&dir);
    }

    /// `OuterHandlePool` (interpreter round i2 wave 69, lane L5,
    /// `CRATONVM_NESTED_JAR_OUTER_HANDLE_REUSE`): an isolated read opens and
    /// closes; the second read of a burst keeps its handle and the third
    /// reuses it, serving the same bytes; a file replaced by a rename with the
    /// SAME length and modification time (the row's stamp still matches) is
    /// never read through the old handle; a deleted file is refused; the
    /// reaper closes an idle handle and exits. On Windows the pool keeps
    /// nothing and every read opens afresh.
    #[test]
    fn a_kept_outer_handle_serves_a_burst_and_never_a_replaced_file() {
        let fat_jar = |path: &Path, a_byte: u8| {
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let mut inner = Vec::new();
            {
                let mut z = zip::ZipWriter::new(Cursor::new(&mut inner));
                z.start_file("a/A.class", stored).unwrap();
                z.write_all(&[0xCA, 0xFE, 0xBA, 0xBE, a_byte]).unwrap();
                z.start_file("a/B.class", stored).unwrap();
                z.write_all(b"\xCA\xFE\xBA\xBEB").unwrap();
                z.finish().unwrap();
            }
            let mut z = zip::ZipWriter::new(fs::File::create(path).unwrap());
            z.start_file("BOOT-INF/lib/a.jar", stored).unwrap();
            z.write_all(&inner).unwrap();
            z.finish().unwrap();
        };
        let dir = std::env::temp_dir().join("cratonvm_test_outer_handle_pool");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        fat_jar(&jar_path, b'A');
        let outer = OuterArchiveIndex::open(&jar_path).expect("parsed");
        let row = ClassPath::open_nested_archive_directory_in_index(&outer, "BOOT-INF/lib/a.jar")
            .expect("a directory row");
        assert!(!row.holds_nested_bytes());

        // A long interval: the burst does not end by itself mid-test.
        let pool = OuterHandlePool::with_idle(std::time::Duration::from_secs(30));
        let a = Some(b"\xCA\xFE\xBA\xBEA".to_vec());
        let b = Some(b"\xCA\xFE\xBA\xBEB".to_vec());
        assert_eq!(row.entry_bytes_pooled("a/A.class", Some(&pool)), a);
        assert_eq!(
            (pool.opens(), pool.reuses(), pool.held()),
            (1, 0, 0),
            "isolated: closed"
        );
        assert_eq!(row.entry_bytes_pooled("a/B.class", Some(&pool)), b);
        let kept = usize::from(OUTER_HANDLE_REUSE_SUPPORTED);
        assert_eq!(
            (pool.opens(), pool.held()),
            (2, kept),
            "second of a burst: kept (Unix)"
        );
        assert_eq!(row.entry_bytes_pooled("a/A.class", Some(&pool)), a);
        assert_eq!(row.entry_bytes_pooled("a/B.class", Some(&pool)), b);
        if !OUTER_HANDLE_REUSE_SUPPORTED {
            assert_eq!((pool.opens(), pool.reuses(), pool.held()), (4, 0, 0));
            assert_eq!(pool.reapers_spawned(), 0);
            drop((pool, row, outer));
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        assert_eq!((pool.opens(), pool.reuses(), pool.held()), (2, 2, 1));
        assert_eq!(pool.reapers_spawned(), 1, "one reaper per burst");
        // Without a pool: the same bytes, opened afresh.
        assert_eq!(row.entry_bytes("a/A.class"), a);

        // Replaced by a rename with a file of the same length and the same
        // modification time: the row's stamp still matches, the kept handle
        // reads the OLD inode; the path's inode differs, so the read opens
        // the new file, whose bytes fail the old directory's CRC -> `None`
        // (the body runs, as without the pool).
        let mtime = fs::metadata(&jar_path).unwrap().modified().unwrap();
        let next = dir.join("app.jar.next");
        fat_jar(&next, b'Z');
        fs::File::options()
            .write(true)
            .open(&next)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        fs::rename(&next, &jar_path).unwrap();
        if row.outer_unchanged() {
            assert_eq!(
                row.entry_bytes_pooled("a/A.class", Some(&pool)),
                None,
                "never the old file"
            );
            assert_eq!(
                row.entry_bytes("a/A.class"),
                None,
                "the same answer without the pool"
            );
            assert_eq!(pool.held(), 0, "the old handle was closed");
        }

        // A fresh file and a fresh burst; then deleted: refused, nothing kept.
        fat_jar(&jar_path, b'A');
        let outer = OuterArchiveIndex::open(&jar_path).expect("reparsed");
        let row = ClassPath::open_nested_archive_directory_in_index(&outer, "BOOT-INF/lib/a.jar")
            .unwrap();
        assert_eq!(row.entry_bytes_pooled("a/A.class", Some(&pool)), a);
        assert_eq!(row.entry_bytes_pooled("a/A.class", Some(&pool)), a);
        assert_eq!(pool.held(), 1);
        fs::remove_file(&jar_path).unwrap();
        assert_eq!(row.entry_bytes_pooled("a/A.class", Some(&pool)), None);
        assert_eq!(pool.held(), 0);

        // Truncated in place under a kept handle: refused.
        fat_jar(&jar_path, b'A');
        let outer = OuterArchiveIndex::open(&jar_path).expect("reparsed");
        let row = ClassPath::open_nested_archive_directory_in_index(&outer, "BOOT-INF/lib/a.jar")
            .unwrap();
        assert_eq!(row.entry_bytes_pooled("a/A.class", Some(&pool)), a);
        assert_eq!(row.entry_bytes_pooled("a/A.class", Some(&pool)), a);
        assert_eq!(pool.held(), 1);
        let full = fs::read(&jar_path).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&jar_path)
            .unwrap()
            .set_len(full.len() as u64 / 2)
            .unwrap();
        assert_eq!(row.entry_bytes_pooled("a/A.class", Some(&pool)), None);
        assert_eq!(pool.held(), 0);
        drop(pool);

        // The reaper closes an idle handle and exits.
        fat_jar(&jar_path, b'A');
        let outer = OuterArchiveIndex::open(&jar_path).expect("reparsed");
        let row = ClassPath::open_nested_archive_directory_in_index(&outer, "BOOT-INF/lib/a.jar")
            .unwrap();
        let short = OuterHandlePool::with_idle(std::time::Duration::from_millis(20));
        assert_eq!(row.entry_bytes_pooled("a/A.class", Some(&short)), a);
        assert_eq!(row.entry_bytes_pooled("a/A.class", Some(&short)), a);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while (short.held() > 0 || short.reaper_running()) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(short.held(), 0, "closed by the reaper");
        assert!(!short.reaper_running(), "the reaper exited");
        drop((short, row, outer));
        let _ = fs::remove_dir_all(&dir);
    }

    /// `NestedArchiveEntries::pooled_read_checks_outer` (interpreter round i3
    /// wave 71, lane L5, `CRATONVM_NESTED_JAR_POOLED_STAT`): `true` only for a
    /// directory-only row while the pool keeps a handle of its fat jar (Unix);
    /// a row holding its bytes never leaves its `stat` to a read. A fat jar
    /// rewritten under a kept handle is answered `None` by the pooled read
    /// (never stale bytes), after which the pool keeps nothing, the hit
    /// checks the file itself again and finds it changed.
    #[test]
    fn a_pooled_read_checks_the_outer_file_only_while_a_handle_is_kept() {
        let fat_jar = |path: &Path, extra: bool| {
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let mut inner = Vec::new();
            {
                let mut z = zip::ZipWriter::new(Cursor::new(&mut inner));
                z.start_file("a/A.class", stored).unwrap();
                z.write_all(b"\xCA\xFE\xBA\xBEA").unwrap();
                if extra {
                    z.start_file("a/New.class", stored).unwrap();
                    z.write_all(b"\xCA\xFE\xBA\xBEN").unwrap();
                }
                z.finish().unwrap();
            }
            let mut z = zip::ZipWriter::new(fs::File::create(path).unwrap());
            z.start_file("BOOT-INF/lib/a.jar", stored).unwrap();
            z.write_all(&inner).unwrap();
            z.finish().unwrap();
        };
        let dir = std::env::temp_dir().join("cratonvm_test_pooled_read_checks_outer");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        fat_jar(&jar_path, false);
        let outer = OuterArchiveIndex::open(&jar_path).expect("parsed");
        let row = ClassPath::open_nested_archive_directory_in_index(&outer, "BOOT-INF/lib/a.jar")
            .expect("a directory row");
        let copy =
            ClassPath::open_nested_archive_in_index(&outer, "BOOT-INF/lib/a.jar").expect("a copy");
        assert!(!row.holds_nested_bytes() && copy.holds_nested_bytes());
        let pool = OuterHandlePool::with_idle(std::time::Duration::from_secs(30));
        assert!(!row.pooled_read_checks_outer(&pool), "nothing kept yet");

        let a = Some(b"\xCA\xFE\xBA\xBEA".to_vec());
        assert_eq!(row.entry_bytes_pooled("a/A.class", Some(&pool)), a);
        assert_eq!(row.entry_bytes_pooled("a/A.class", Some(&pool)), a);
        assert_eq!(
            row.pooled_read_checks_outer(&pool),
            OUTER_HANDLE_REUSE_SUPPORTED,
            "a kept handle (Unix)"
        );
        assert!(
            !copy.pooled_read_checks_outer(&pool),
            "a row holding its bytes checks the file itself"
        );

        if OUTER_HANDLE_REUSE_SUPPORTED {
            // Rewritten with another length under the kept handle: the hit
            // would skip its `stat`, and the pooled read refuses the file.
            fat_jar(&jar_path, true);
            assert!(
                row.pooled_read_checks_outer(&pool),
                "no system call: still kept"
            );
            assert_eq!(
                row.entry_bytes_pooled("a/A.class", Some(&pool)),
                None,
                "never stale bytes"
            );
            assert_eq!(pool.held(), 0, "a refused read keeps nothing");
            assert!(
                !row.pooled_read_checks_outer(&pool),
                "the next hit checks the file"
            );
            assert!(
                !row.outer_unchanged(),
                "and finds it changed (the row is rebuilt)"
            );
        }
        drop((pool, row, copy, outer));
        let _ = fs::remove_dir_all(&dir);
    }

    /// `ClassPath::outer_archive_for` (interpreter round i2 wave 69, lane L5,
    /// `CRATONVM_NESTED_JAR_CLASS_PATH_OUTER`): a fat jar that is an archive
    /// of the class path (with or without fat-jar emulation) is lent with
    /// its load-time stamp, and the walk and bytes rows built from it answer
    /// what rows built from a fresh open answer; a file that is not on the
    /// class path is not lent, nor (Unix: a mapped file cannot be truncated
    /// on Windows) one rewritten since the class path loaded it.
    #[test]
    fn the_class_path_lends_its_own_parse_of_a_fat_jar() {
        let fat_jar = |path: &Path, extra: bool| {
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let mut inner = Vec::new();
            {
                let mut z = zip::ZipWriter::new(Cursor::new(&mut inner));
                z.start_file("a/A.class", stored).unwrap();
                z.write_all(b"\xCA\xFE\xBA\xBEA").unwrap();
                if extra {
                    z.start_file("a/New.class", stored).unwrap();
                    z.write_all(b"\xCA\xFE\xBA\xBEN").unwrap();
                }
                z.finish().unwrap();
            }
            let mut z = zip::ZipWriter::new(fs::File::create(path).unwrap());
            z.start_file("BOOT-INF/lib/a.jar", stored).unwrap();
            z.write_all(&inner).unwrap();
            z.finish().unwrap();
        };
        let dir = std::env::temp_dir().join("cratonvm_test_class_path_outer_archive");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        fat_jar(&jar_path, false);
        let spelled = jar_path.to_string_lossy().into_owned();

        for emulation in [false, true] {
            let cp = ClassPath::with_fat_jar_emulation(std::slice::from_ref(&spelled), emulation);
            let lent = cp
                .outer_archive_for(&jar_path)
                .expect("an archive of the class path");
            assert!(lent.unchanged(), "the load-time stamp is the file's");
            let row =
                ClassPath::open_nested_archive_in(&lent, "BOOT-INF/lib/a.jar").expect("a row");
            let fresh =
                ClassPath::open_nested_archive(&jar_path, "BOOT-INF/lib/a.jar", false).unwrap();
            assert_eq!(row.entry_bytes("a/A.class"), fresh.entry_bytes("a/A.class"));
            assert_eq!(
                row.entry_bytes("a/A.class").as_deref(),
                Some(&b"\xCA\xFE\xBA\xBEA"[..])
            );
            let names =
                ClassPath::nested_archive_entry_index_in(&lent, "BOOT-INF/lib/a.jar").unwrap();
            assert!(names.contains("a/A.class") && !names.contains("a/New.class"));
            assert!(names.outer_unchanged());
            assert!(
                cp.outer_archive_for(&dir.join("other.jar")).is_none(),
                "not on the class path"
            );
            drop((lent, row, fresh, names, cp));
        }
        let cp = ClassPath::with_fat_jar_emulation(std::slice::from_ref(&spelled), false);
        assert!(
            ClassPath::new(&[]).outer_archive_for(&jar_path).is_none(),
            "an empty class path"
        );
        if cfg!(unix) {
            // Rewritten with another length since the class path loaded it:
            // never lent; the caller opens the file as it now is.
            fat_jar(&jar_path, true);
            assert!(
                cp.outer_archive_for(&jar_path).is_none(),
                "a rewritten fat jar"
            );
        }
        drop(cp);
        let _ = fs::remove_dir_all(&dir);
    }

    /// `ClassPath::open_jar_archive_lent` (interpreter round i2 wave 70, lane
    /// L5, `CRATONVM_JAR_LOADER_CLASS_PATH_ARCHIVE`): a jar-loader row built
    /// from the class path's own archive answers what a row opened from the
    /// file answers (bytes, CRC/size check, plain view, entry index), carries
    /// the load-time stamp, and a Multi-Release jar's lent row has no plain
    /// view either; on Unix a jar rewritten after the lend is no longer
    /// `outer_unchanged` (the memo then drops the row), and is not lent again.
    #[test]
    fn a_jar_loader_row_over_the_class_path_archive_answers_as_an_opened_one() {
        fn write_jar(path: &Path, entries: &[(&str, &[u8], bool)]) {
            let mut z = zip::ZipWriter::new(fs::File::create(path).unwrap());
            for (name, data, deflate) in entries {
                let method = if *deflate {
                    zip::CompressionMethod::Deflated
                } else {
                    zip::CompressionMethod::Stored
                };
                let opts = zip::write::SimpleFileOptions::default().compression_method(method);
                z.start_file(*name, opts).unwrap();
                z.write_all(data).unwrap();
            }
            z.finish().unwrap();
        }
        fn crc_and_size(path: &Path, name: &str) -> (u32, u64) {
            let mut archive = ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
            let entry = archive.by_name(name).unwrap();
            (entry.crc32(), entry.size())
        }
        let dir = std::env::temp_dir().join("cratonvm_test_jar_loader_lent_archive");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let plain = dir.join("lib.jar");
        let big: Vec<u8> = (0..9000u32).map(|i| (i % 251) as u8).collect();
        write_jar(
            &plain,
            &[
                ("p/Stored.class", &b"\xCA\xFE\xBA\xBEs"[..], false),
                ("p/Deflated.class", &big[..], true),
            ],
        );
        let mr = dir.join("mr.jar");
        write_jar(
            &mr,
            &[
                ("p/A.class", &b"base"[..], false),
                ("META-INF/versions/11/p/A.class", &b"x"[..], false),
            ],
        );
        let spelled: Vec<String> = [&plain, &mr]
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let cp = ClassPath::new(&spelled);

        let lent = cp
            .outer_archive_for(&plain)
            .expect("an archive of the class path");
        let row = ClassPath::open_jar_archive_lent(lent).expect("a row");
        let opened = ClassPath::open_jar_archive(&plain, false).expect("opened");
        assert!(row.plain_view() && opened.plain_view());
        assert!(row.outer_unchanged(), "the load-time stamp is the file's");
        assert_eq!(row.entry_index, opened.entry_index, "the same index");
        for name in ["p/Stored.class", "p/Deflated.class"] {
            let (crc, size) = crc_and_size(&plain, name);
            assert_eq!(
                row.entry_bytes_matching(name, crc, size),
                opened.entry_bytes_matching(name, crc, size)
            );
            assert!(row.entry_bytes_matching(name, crc, size).is_some());
            assert_eq!(
                row.entry_bytes_matching(name, crc ^ 1, size),
                None,
                "another CRC"
            );
        }
        assert_eq!(row.entry_bytes("p/Deflated.class"), Some(big.clone()));
        assert_eq!(row.entry_bytes("p/Absent.class"), None);

        let mr_row = ClassPath::open_jar_archive_lent(cp.outer_archive_for(&mr).unwrap()).unwrap();
        assert!(
            !mr_row.plain_view(),
            "a Multi-Release jar: no plain view, lent or not"
        );
        assert_eq!(mr_row.entry_bytes("p/A.class"), None);

        if cfg!(unix) {
            // Rewritten with another length after the lend: the row's stamp
            // (the class path's load-time one) no longer matches, and the
            // class path lends it no more; the caller opens the file as it is.
            write_jar(
                &plain,
                &[("p/Stored.class", &b"\xCA\xFE\xBA\xBEsecond"[..], false)],
            );
            assert!(!row.outer_unchanged(), "a rewritten jar");
            assert!(cp.outer_archive_for(&plain).is_none(), "not lent again");
        }
        drop((row, opened, mr_row, cp));
        let _ = fs::remove_dir_all(&dir);
    }

    /// `ClassPath::jar_entry_bytes_matching` (interpreter round i3 wave 71,
    /// lane L5, `CRATONVM_JAR_LOADER_CLASS_PATH_DIRECT`): the class path's
    /// own entry answers what a jar-loader row opened from the file answers
    /// (bytes on a CRC / size match, `Body` on a mismatch, an absent or
    /// directory name, or a jar without the plain view), `NotHeld` for a jar
    /// that is not on the class path, the same answer for a path spelled
    /// another way (canonical match, remembered), and on Unix `NotHeld` once
    /// the jar is rewritten with another length.
    #[test]
    fn a_class_path_jar_serves_its_entries_without_a_row() {
        use cratonvm_native_api::ClassPathJarBytes as Answer;
        fn write_jar(path: &Path, entries: &[(&str, &[u8], bool)]) {
            let mut z = zip::ZipWriter::new(fs::File::create(path).unwrap());
            for (name, data, deflate) in entries {
                let method = if *deflate {
                    zip::CompressionMethod::Deflated
                } else {
                    zip::CompressionMethod::Stored
                };
                let opts = zip::write::SimpleFileOptions::default().compression_method(method);
                if name.ends_with('/') {
                    z.add_directory(name.trim_end_matches('/'), opts).unwrap();
                } else {
                    z.start_file(*name, opts).unwrap();
                    z.write_all(data).unwrap();
                }
            }
            z.finish().unwrap();
        }
        fn crc_and_size(path: &Path, name: &str) -> (u32, u64) {
            let mut archive = ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
            let entry = archive.by_name(name).unwrap();
            (entry.crc32(), entry.size())
        }
        let dir = std::env::temp_dir().join("cratonvm_test_jar_entry_bytes_direct");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(dir.join("sub"));
        let plain = dir.join("lib.jar");
        let big: Vec<u8> = (0..9000u32).map(|i| (i % 251) as u8).collect();
        write_jar(
            &plain,
            &[
                ("p/", &b""[..], false),
                ("p/Stored.class", &b"\xCA\xFE\xBA\xBEs"[..], false),
                ("p/Deflated.class", &big[..], true),
            ],
        );
        let mr = dir.join("mr.jar");
        write_jar(
            &mr,
            &[
                ("p/A.class", &b"base"[..], false),
                ("META-INF/versions/11/p/A.class", &b"x"[..], false),
            ],
        );
        let spelled: Vec<String> = [&plain, &mr]
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let cp = ClassPath::new(&spelled);
        let opened = ClassPath::open_jar_archive(&plain, false).expect("opened");

        for name in ["p/Stored.class", "p/Deflated.class"] {
            let (crc, size) = crc_and_size(&plain, name);
            let expected = opened
                .entry_bytes_matching(name, crc, size)
                .expect("a row serves it");
            assert_eq!(
                cp.jar_entry_bytes_matching(&plain, name, crc, size),
                Answer::Bytes(expected)
            );
            assert_eq!(
                cp.jar_entry_bytes_matching(&plain, name, crc ^ 1, size),
                Answer::Body,
                "another CRC"
            );
            assert_eq!(
                cp.jar_entry_bytes_matching(&plain, name, crc, size + 1),
                Answer::Body,
                "another size"
            );
        }
        assert_eq!(
            cp.jar_entry_bytes_matching(&plain, "p/Absent.class", 0, 0),
            Answer::Body
        );
        assert_eq!(
            cp.jar_entry_bytes_matching(&plain, "p/", 0, 0),
            Answer::Body,
            "a directory"
        );
        assert_eq!(cp.jar_entry_bytes_matching(&plain, "", 0, 0), Answer::Body);

        // Another spelling of the same file: matched once canonicalised.
        let other_spelling = dir.join("sub").join("..").join("lib.jar");
        let (crc, size) = crc_and_size(&plain, "p/Stored.class");
        for _ in 0..2 {
            assert_eq!(
                cp.jar_entry_bytes_matching(&other_spelling, "p/Stored.class", crc, size),
                Answer::Bytes(b"\xCA\xFE\xBA\xBEs".to_vec()),
                "the same jar spelled another way"
            );
        }

        // A Multi-Release jar has no plain view: the body, as for its row.
        let (crc, size) = crc_and_size(&mr, "p/A.class");
        assert_eq!(
            cp.jar_entry_bytes_matching(&mr, "p/A.class", crc, size),
            Answer::Body
        );

        // Not on the class path: not held (twice: the absence is remembered).
        let elsewhere = dir.join("elsewhere.jar");
        write_jar(
            &elsewhere,
            &[("p/Stored.class", &b"\xCA\xFE\xBA\xBEs"[..], false)],
        );
        for _ in 0..2 {
            assert_eq!(
                cp.jar_entry_bytes_matching(&elsewhere, "p/Stored.class", crc, size),
                Answer::NotHeld
            );
        }
        assert_eq!(
            ClassPath::new(&[]).jar_entry_bytes_matching(&plain, "p/Stored.class", crc, size),
            Answer::NotHeld,
            "an empty class path"
        );

        if cfg!(unix) {
            // Rewritten with another length since the class path loaded it:
            // not held; the caller's rows open the file as it now is.
            write_jar(
                &plain,
                &[("p/Stored.class", &b"\xCA\xFE\xBA\xBEsecond"[..], false)],
            );
            let (crc, size) = crc_and_size(&plain, "p/Stored.class");
            assert_eq!(
                cp.jar_entry_bytes_matching(&plain, "p/Stored.class", crc, size),
                Answer::NotHeld
            );
        }
        drop((opened, cp));
        let _ = fs::remove_dir_all(&dir);
    }

    /// `OuterFileStamp` (interpreter round i2 wave 68, lane L5): equal for an
    /// unchanged file, different after a rewrite of another length, `None`
    /// for no file or a directory.
    #[test]
    fn outer_file_stamp_tells_a_rewrite() {
        let dir = std::env::temp_dir().join("cratonvm_test_outer_file_stamp");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("f.jar");
        fs::write(&path, b"one").unwrap();
        let first = OuterFileStamp::read(&path).expect("a file");
        assert_eq!(OuterFileStamp::read(&path), Some(first));
        fs::write(&path, b"three").unwrap();
        assert_ne!(OuterFileStamp::read(&path), Some(first));
        fs::remove_file(&path).unwrap();
        assert_eq!(OuterFileStamp::read(&path), None);
        assert_eq!(OuterFileStamp::read(&dir), None, "a directory");
        let _ = fs::remove_dir_all(&dir);
    }

    /// `open_nested_archive` / `NestedArchiveEntries::entry_bytes` (the
    /// nested-jar bytes index, interpreter round i2 wave 63, lane L5): a
    /// stored and a deflated entry read back exactly; directories, absent
    /// names, a name that is also a directory, Multi-Release and signed jars
    /// answer `None`; the outer archive's stamp tells a changed file.
    #[test]
    fn open_nested_archive_serves_plain_entries_only() {
        fn nested(entries: &[(&str, &[u8], bool)]) -> Vec<u8> {
            let mut buf = Vec::new();
            {
                let mut z = zip::ZipWriter::new(Cursor::new(&mut buf));
                for (name, data, deflate) in entries {
                    let method = if *deflate {
                        zip::CompressionMethod::Deflated
                    } else {
                        zip::CompressionMethod::Stored
                    };
                    let opts = zip::write::SimpleFileOptions::default().compression_method(method);
                    if name.ends_with('/') {
                        z.add_directory(name.trim_end_matches('/'), opts).unwrap();
                    } else {
                        z.start_file(*name, opts).unwrap();
                        z.write_all(data).unwrap();
                    }
                }
                z.finish().unwrap();
            }
            buf
        }
        let dir = std::env::temp_dir().join("cratonvm_test_open_nested_archive");
        let _ = fs::create_dir_all(&dir);
        let outer = dir.join("outer.jar");
        let big: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let plain = nested(&[
            ("p/", &b""[..], false),
            ("p/Stored.class", &b"\xCA\xFE\xBA\xBEstored"[..], false),
            ("p/Deflated.class", &big[..], true),
            ("both", &b"file"[..], false),
            ("both/", &b""[..], false),
        ]);
        let mr = nested(&[
            ("p/A.class", &b"base"[..], false),
            ("META-INF/versions/9/p/A.class", &b"nine"[..], false),
        ]);
        let signed = nested(&[
            ("p/A.class", &b"a"[..], false),
            ("meta-inf/signer.sf", &b"x"[..], false),
        ]);
        {
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let mut z = zip::ZipWriter::new(fs::File::create(&outer).unwrap());
            for (name, data) in [
                ("BOOT-INF/lib/plain.jar", &plain),
                ("BOOT-INF/lib/mr.jar", &mr),
                ("BOOT-INF/lib/signed.jar", &signed),
            ] {
                z.start_file(name, stored).unwrap();
                z.write_all(data).unwrap();
            }
            z.finish().unwrap();
        }

        let jar =
            ClassPath::open_nested_archive(&outer, "BOOT-INF/lib/plain.jar", true).expect("opened");
        assert!(jar.plain_view());
        assert!(jar.outer_unchanged());
        assert_eq!(
            jar.entry_bytes("p/Stored.class").as_deref(),
            Some(&b"\xCA\xFE\xBA\xBEstored"[..])
        );
        assert_eq!(jar.entry_bytes("p/Deflated.class"), Some(big.clone()));
        assert!(jar.has_entry("p/"));
        assert_eq!(jar.entry_bytes("p/"), None, "a directory");
        assert_eq!(jar.entry_bytes("p"), None, "only the directory exists");
        assert_eq!(jar.entry_bytes("both"), None, "the name is a directory too");
        assert_eq!(jar.entry_bytes("p/Missing.class"), None);
        assert_eq!(jar.entry_bytes(""), None);

        let mr =
            ClassPath::open_nested_archive(&outer, "BOOT-INF/lib/mr.jar", true).expect("opened");
        assert!(!mr.plain_view(), "Multi-Release");
        assert_eq!(mr.entry_bytes("p/A.class"), None);
        let signed = ClassPath::open_nested_archive(&outer, "BOOT-INF/lib/signed.jar", true)
            .expect("opened");
        assert!(!signed.plain_view(), "a signature file, any case");
        assert_eq!(signed.entry_bytes("p/A.class"), None);

        assert!(ClassPath::open_nested_archive(&outer, "BOOT-INF/lib/none.jar", true).is_none());
        assert!(ClassPath::open_nested_archive(&outer, "../x.jar", true).is_none());
        assert!(ClassPath::open_nested_archive(&dir.join("absent.jar"), "a.jar", true).is_none());

        // The stamp a later ask compares (not by rewriting `outer`: a mapped
        // file cannot be rewritten on every host).
        let other = dir.join("other.bin");
        fs::write(&other, b"one").unwrap();
        let stamp = OuterArchiveStamp::read(&other).expect("a file");
        fs::write(&other, b"longer").unwrap();
        assert_ne!(
            OuterArchiveStamp::read(&other),
            Some(stamp),
            "the length changed"
        );
        assert_eq!(
            OuterArchiveStamp::read(&dir),
            None,
            "a directory is no archive"
        );
        drop(jar);

        let _ = fs::remove_dir_all(&dir);
    }

    /// Without `map`, `open_nested_archive` and `open_jar_archive` keep no
    /// mapping and no handle of the file: a jar an application loader was
    /// handed may be truncated or rewritten in place afterwards, on every
    /// host, and the copy keeps serving what it read (the stamp turns the
    /// caller away). With a mapping that truncation would be `SIGBUS` on
    /// Linux and macOS. Interpreter round i2 wave 64, lane L5 follow-up.
    #[test]
    fn unmapped_archive_opens_survive_a_truncation_of_the_file() {
        let dir = std::env::temp_dir().join(format!(
            "cratonvm_test_unmapped_archive_opens_{}",
            std::process::id()
        ));
        let _ = fs::create_dir_all(&dir);
        let big: Vec<u8> = (0..9000u32).map(|i| (i % 239) as u8).collect();
        let inner = {
            let mut z = zip::ZipWriter::new(Cursor::new(Vec::new()));
            let deflated = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            z.start_file("p/A.class", deflated).unwrap();
            z.write_all(&big).unwrap();
            z.finish().unwrap().into_inner()
        };
        let jar_path = dir.join("app.jar");
        fs::write(&jar_path, &inner).unwrap();
        let outer = dir.join("fat.jar");
        {
            let mut z = zip::ZipWriter::new(fs::File::create(&outer).unwrap());
            for method in [
                zip::CompressionMethod::Stored,
                zip::CompressionMethod::Deflated,
            ] {
                let name = format!("BOOT-INF/lib/{method:?}.jar");
                z.start_file(
                    name,
                    zip::write::SimpleFileOptions::default().compression_method(method),
                )
                .unwrap();
                z.write_all(&inner).unwrap();
            }
            z.finish().unwrap();
        }

        let stored = ClassPath::open_nested_archive(&outer, "BOOT-INF/lib/Stored.jar", false)
            .expect("opened");
        let deflated = ClassPath::open_nested_archive(&outer, "BOOT-INF/lib/Deflated.jar", false)
            .expect("opened");
        let jar = ClassPath::open_jar_archive(&jar_path, false).expect("opened");
        assert!(ClassPath::open_nested_archive(&outer, "BOOT-INF/lib/none.jar", false).is_none());
        for archive in [&stored, &deflated, &jar] {
            assert!(archive.plain_view());
            assert!(archive.outer_unchanged());
            assert_eq!(archive.entry_bytes("p/A.class"), Some(big.clone()));
        }

        // Truncate both files in place (no handle of ours keeps them).
        fs::File::create(&outer).unwrap();
        fs::File::create(&jar_path).unwrap();
        for archive in [&stored, &deflated, &jar] {
            assert!(!archive.outer_unchanged(), "the stamp sees the rewrite");
            assert_eq!(
                archive.entry_bytes("p/A.class"),
                Some(big.clone()),
                "served from memory"
            );
        }
        drop((stored, deflated, jar));
        let _ = fs::remove_dir_all(&dir);
    }

    /// `open_jar_archive` / `NestedArchiveEntries::entry_bytes_matching` (the
    /// jar-loader bytes index, interpreter round i2 wave 64, lane L5): a plain
    /// jar's stored and deflated entries read back when the caller's CRC and
    /// size agree with the central directory, and not otherwise;
    /// Multi-Release and signed jars, directories and absent names answer
    /// `None`; a non-archive is not opened.
    #[test]
    fn open_jar_archive_serves_entries_whose_crc_and_size_match() {
        fn write_jar(path: &Path, entries: &[(&str, &[u8], bool)]) {
            let mut z = zip::ZipWriter::new(fs::File::create(path).unwrap());
            for (name, data, deflate) in entries {
                let method = if *deflate {
                    zip::CompressionMethod::Deflated
                } else {
                    zip::CompressionMethod::Stored
                };
                let opts = zip::write::SimpleFileOptions::default().compression_method(method);
                if name.ends_with('/') {
                    z.add_directory(name.trim_end_matches('/'), opts).unwrap();
                } else {
                    z.start_file(*name, opts).unwrap();
                    z.write_all(data).unwrap();
                }
            }
            z.finish().unwrap();
        }
        fn crc_and_size(path: &Path, name: &str) -> (u32, u64) {
            let mut archive = ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
            let entry = archive.by_name(name).unwrap();
            (entry.crc32(), entry.size())
        }
        let dir = std::env::temp_dir().join("cratonvm_test_open_jar_archive");
        let _ = fs::create_dir_all(&dir);
        let plain = dir.join("plain.jar");
        let big: Vec<u8> = (0..7000u32).map(|i| (i % 241) as u8).collect();
        write_jar(
            &plain,
            &[
                ("p/", &b""[..], false),
                ("p/Stored.class", &b"\xCA\xFE\xBA\xBEstored"[..], false),
                ("p/Deflated.class", &big[..], true),
            ],
        );
        let mr = dir.join("mr.jar");
        write_jar(
            &mr,
            &[
                ("p/A.class", &b"base"[..], false),
                ("META-INF/versions/11/p/A.class", &b"x"[..], false),
            ],
        );
        let signed = dir.join("signed.jar");
        write_jar(
            &signed,
            &[
                ("p/A.class", &b"a"[..], true),
                ("META-INF/SIGNER.RSA", &b"x"[..], false),
            ],
        );

        let jar = ClassPath::open_jar_archive(&plain, true).expect("opened");
        assert!(jar.plain_view());
        assert!(jar.outer_unchanged());
        let (crc, size) = crc_and_size(&plain, "p/Stored.class");
        assert_eq!(
            jar.entry_bytes_matching("p/Stored.class", crc, size)
                .as_deref(),
            Some(&b"\xCA\xFE\xBA\xBEstored"[..])
        );
        assert_eq!(
            jar.entry_bytes_matching("p/Stored.class", crc ^ 1, size),
            None,
            "another CRC"
        );
        assert_eq!(
            jar.entry_bytes_matching("p/Stored.class", crc, size + 1),
            None,
            "another size"
        );
        let (crc, size) = crc_and_size(&plain, "p/Deflated.class");
        assert_eq!(size, big.len() as u64);
        assert_eq!(
            jar.entry_bytes_matching("p/Deflated.class", crc, size),
            Some(big.clone())
        );
        assert_eq!(jar.entry_bytes("p/Deflated.class"), Some(big.clone()));
        assert_eq!(jar.entry_bytes_matching("p/", 0, 0), None, "a directory");
        assert_eq!(jar.entry_bytes_matching("p/Missing.class", crc, size), None);

        let mr_jar = ClassPath::open_jar_archive(&mr, true).expect("opened");
        assert!(!mr_jar.plain_view(), "Multi-Release");
        let (crc, size) = crc_and_size(&mr, "p/A.class");
        assert_eq!(mr_jar.entry_bytes_matching("p/A.class", crc, size), None);
        let signed_jar = ClassPath::open_jar_archive(&signed, true).expect("opened");
        assert!(!signed_jar.plain_view(), "a signature file");
        let (crc, size) = crc_and_size(&signed, "p/A.class");
        assert_eq!(
            signed_jar.entry_bytes_matching("p/A.class", crc, size),
            None
        );

        let not_zip = dir.join("not.jar");
        fs::write(&not_zip, b"not a zip").unwrap();
        assert!(ClassPath::open_jar_archive(&not_zip, true).is_none());
        assert!(ClassPath::open_jar_archive(&dir.join("absent.jar"), true).is_none());
        assert!(
            ClassPath::open_jar_archive(&dir, true).is_none(),
            "a directory"
        );
        drop(jar);
        drop(mr_jar);
        drop(signed_jar);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fat_jar_loads_resource_from_nested_jar() {
        let dir = std::env::temp_dir().join("cratonvm_test_fatjar_spi");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp
            .find_resource("META-INF/services/org.dep.SPI")
            .expect("should find SPI file from nested JAR");
        assert_eq!(data, b"org.dep.SpiImpl");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fat_jar_class_not_found_returns_error() {
        let dir = std::env::temp_dir().join("cratonvm_test_fatjar_missing");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        assert!(cp.find_class("com/example/Missing").is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_parsing() {
        let manifest = b"Manifest-Version: 1.0\r\n\
            Main-Class: org.springframework.boot.loader.JarLauncher\r\n\
            Start-Class: com.example.DemoApplication\r\n\
            Spring-Boot-Version: 3.2.0\r\n\
            Spring-Boot-Classes: BOOT-INF/classes/\r\n\
            Spring-Boot-Lib: BOOT-INF/lib/\r\n";
        let info = ManifestInfo::parse(manifest);
        assert_eq!(
            info.main_class.as_deref(),
            Some("org.springframework.boot.loader.JarLauncher")
        );
        assert_eq!(
            info.start_class.as_deref(),
            Some("com.example.DemoApplication")
        );
        assert!(info.is_spring_boot());
    }

    #[test]
    fn manifest_continuation_lines() {
        // MANIFEST.MF allows continuation lines starting with a single space
        let manifest = b"Manifest-Version: 1.0\r\n\
            Main-Class: org.springframework.boot.loader.JarLa\r\n \
            uncher\r\n\
            Start-Class: com.example.App\r\n";
        let info = ManifestInfo::parse(manifest);
        assert_eq!(
            info.main_class.as_deref(),
            Some("org.springframework.boot.loader.JarLauncher")
        );
    }

    #[test]
    fn plain_jar_not_treated_as_fat_jar() {
        let dir = std::env::temp_dir().join("cratonvm_test_plain_jar");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("plain.jar");

        let file = fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("com/example/Foo.class", opts).unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_foo").unwrap();
        zip.finish().unwrap();

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        // Plain JAR should have exactly 1 entry (JarFile)
        assert_eq!(cp.entry_count(), 1);

        let data = cp.find_class("com/example/Foo").unwrap();
        assert!(data.ends_with(b"_foo"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dynamic_add_path_fat_jar() {
        let dir = std::env::temp_dir().join("cratonvm_test_dynamic_fatjar");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("app.jar");
        create_fat_jar(&jar_path);

        let mut cp = ClassPath::new(&[]);
        assert!(cp.is_empty());

        cp.add_path(&jar_path.to_string_lossy());
        assert!(!cp.is_empty());

        // Should find classes from nested structure
        assert!(cp.find_class("com/example/App").is_ok());
        assert!(cp.find_class("org/dep/Util").is_ok());

        let _ = fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // JMOD file tests
    // -----------------------------------------------------------------------

    #[test]
    fn jmod_magic_validation() {
        // A file with wrong magic should fail
        let dir = std::env::temp_dir().join("cratonvm_test_jmod");
        let _ = fs::create_dir_all(&dir);
        let bad_path = dir.join("bad.jmod");
        fs::write(&bad_path, b"NOT_JMOD_DATA").unwrap();
        assert!(ClassPath::load_jmod(&bad_path).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    /// A `jmods/<module>.jmod` of a JDK (a `lib` directory beside `jmods`)
    /// names its resources `jrt:/<module>/...`, as the `lib/modules` form and
    /// HotSpot do; a `.jmod` anywhere else keeps its `jar:file:` URL.
    #[test]
    fn jdk_jmod_resources_are_jrt_urls() {
        fn write_jmod(path: &Path) {
            let mut zip_buf = Vec::new();
            {
                let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut zip_buf));
                let options = zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored);
                w.start_file("classes/pkg/res.txt", options).unwrap();
                w.write_all(b"x").unwrap();
                w.finish().unwrap();
            }
            let mut data = Vec::new();
            data.extend_from_slice(&ClassPath::JMOD_MAGIC_PREFIX);
            data.push(ClassPath::JMOD_SUPPORTED_MAJOR);
            data.push(0x00);
            data.extend_from_slice(&zip_buf);
            fs::write(path, &data).unwrap();
        }
        let dir = std::env::temp_dir().join("cratonvm_test_jmod_jrt_url");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("home/jmods")).unwrap();
        fs::create_dir_all(dir.join("home/lib")).unwrap();
        fs::create_dir_all(dir.join("other")).unwrap();
        let jdk_jmod = dir.join("home/jmods/java.base.jmod");
        let loose_jmod = dir.join("other/loose.jmod");
        write_jmod(&jdk_jmod);
        write_jmod(&loose_jmod);

        let cp = ClassPath::new(&[jdk_jmod.to_string_lossy().into_owned()]);
        assert_eq!(
            cp.find_all_resource_urls("pkg/res.txt"),
            vec!["jrt:/java.base/pkg/res.txt".to_string()]
        );
        assert_eq!(
            cp.find_all_resource_urls("pkg/*.txt"),
            vec!["jrt:/java.base/pkg/res.txt".to_string()]
        );
        let cp = ClassPath::new(&[loose_jmod.to_string_lossy().into_owned()]);
        let urls = cp.find_all_resource_urls("pkg/res.txt");
        assert_eq!(urls.len(), 1);
        assert!(urls[0].starts_with("jar:file:/") && urls[0].ends_with("loose.jmod!/pkg/res.txt"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// Interpreter round i3 wave 73, lane L5
    /// (`CRATONVM_PLATFORM_RESOURCE_JRT_WALK`): the runtime-image walk answers
    /// the first `jrt:` URL of the whole-list walk, for a class path that
    /// mixes a directory and a loose `.jmod` holding the same name (neither
    /// counts) with a JDK `jmods/<module>.jmod` (which does), and `None` where
    /// only the non-`jrt:` entries hold it.
    #[test]
    fn i73_the_runtime_image_walk_answers_the_first_jrt_url_of_the_whole_walk() {
        fn write_jmod(path: &Path) {
            let mut zip_buf = Vec::new();
            {
                let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut zip_buf));
                let options = zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored);
                w.start_file("classes/pkg/res.txt", options).unwrap();
                w.write_all(b"x").unwrap();
                w.finish().unwrap();
            }
            let mut data = Vec::new();
            data.extend_from_slice(&ClassPath::JMOD_MAGIC_PREFIX);
            data.push(ClassPath::JMOD_SUPPORTED_MAJOR);
            data.push(0x00);
            data.extend_from_slice(&zip_buf);
            fs::write(path, &data).unwrap();
        }
        let dir = std::env::temp_dir().join("cratonvm_test_i73_jrt_walk");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("home/jmods")).unwrap();
        fs::create_dir_all(dir.join("home/lib")).unwrap();
        fs::create_dir_all(dir.join("other")).unwrap();
        fs::create_dir_all(dir.join("classes/pkg")).unwrap();
        fs::write(dir.join("classes/pkg/res.txt"), b"y").unwrap();
        let jdk_jmod = dir.join("home/jmods/java.base.jmod");
        let loose_jmod = dir.join("other/loose.jmod");
        write_jmod(&jdk_jmod);
        write_jmod(&loose_jmod);
        let classes = dir.join("classes");

        let first_jrt = |cp: &ClassPath, name: &str| {
            cp.find_all_resource_urls(name)
                .into_iter()
                .find(|url| url.starts_with("jrt:"))
        };
        let mixed = ClassPath::new(&[
            classes.to_string_lossy().into_owned(),
            loose_jmod.to_string_lossy().into_owned(),
            jdk_jmod.to_string_lossy().into_owned(),
        ]);
        for name in [
            "pkg/res.txt",
            "/pkg/res.txt",
            "pkg/*.txt",
            "pkg/absent.txt",
            "../x",
        ] {
            assert_eq!(
                mixed.first_runtime_image_resource_url(name),
                first_jrt(&mixed, name),
                "{name}"
            );
        }
        assert_eq!(
            mixed
                .first_runtime_image_resource_url("pkg/res.txt")
                .as_deref(),
            Some("jrt:/java.base/pkg/res.txt")
        );
        let no_image = ClassPath::new(&[
            classes.to_string_lossy().into_owned(),
            loose_jmod.to_string_lossy().into_owned(),
        ]);
        assert_eq!(no_image.find_all_resource_urls("pkg/res.txt").len(), 2);
        assert_eq!(
            no_image.first_runtime_image_resource_url("pkg/res.txt"),
            None
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_class_from_synthetic_jmod() {
        // Create a synthetic JMOD file: 4-byte magic + ZIP with classes/ prefix
        let dir = std::env::temp_dir().join("cratonvm_test_jmod2");
        let _ = fs::create_dir_all(&dir);
        let jmod_path = dir.join("test.jmod");

        // Build the ZIP portion in memory
        let mut zip_buf = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut zip_buf);
            let mut zip_writer = zip::ZipWriter::new(cursor);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip_writer
                .start_file("classes/java/lang/Object.class", options)
                .unwrap();
            zip_writer
                .write_all(b"\xCA\xFE\xBA\xBE_fake_object")
                .unwrap();
            zip_writer
                .start_file("classes/java/lang/String.class", options)
                .unwrap();
            zip_writer
                .write_all(b"\xCA\xFE\xBA\xBE_fake_string")
                .unwrap();
            zip_writer
                .start_file("classes/module-info.class", options)
                .unwrap();
            zip_writer
                .write_all(b"\xCA\xFE\xBA\xBE_module_info")
                .unwrap();
            zip_writer.finish().unwrap();
        }

        // Write JMOD = magic + ZIP data
        let mut jmod_data = Vec::new();
        jmod_data.extend_from_slice(&ClassPath::JMOD_MAGIC_PREFIX);
        jmod_data.push(ClassPath::JMOD_SUPPORTED_MAJOR);
        jmod_data.push(0x00); // minor version
        jmod_data.extend_from_slice(&zip_buf);
        fs::write(&jmod_path, &jmod_data).unwrap();

        // Load JMOD as classpath entry
        let cp = ClassPath::new(&[jmod_path.to_string_lossy().into_owned()]);
        assert!(!cp.is_empty());

        // Find classes (the prefix "classes/" should be stripped automatically)
        let obj_bytes = cp.find_class("java/lang/Object").unwrap();
        assert_eq!(&obj_bytes[..4], b"\xCA\xFE\xBA\xBE");

        let str_bytes = cp.find_class("java/lang/String").unwrap();
        assert_eq!(&str_bytes[..4], b"\xCA\xFE\xBA\xBE");

        // Unknown class not found
        assert!(cp.find_class("java/lang/Missing").is_err());

        // list_class_names should find both (but not module-info)
        let names = cp.list_class_names();
        assert!(names.contains(&"java/lang/Object".to_string()));
        assert!(names.contains(&"java/lang/String".to_string()));
        assert!(!names.iter().any(|n| n.contains("module-info")));

        // module-info should be found by scan_module_infos
        let modules = cp.scan_module_infos();
        assert_eq!(modules.len(), 1);

        // Resource lookup should work via the classes/ prefix
        let resource = cp.find_resource("java/lang/Object.class");
        assert!(resource.is_some());

        let _ = fs::remove_dir_all(&dir);
    }

    /// A JDK booted from `jmods/` (the Windows Microsoft build) is a runtime
    /// image too: `runtime_image_defines_class` must answer from its entries,
    /// not report "no image" and leave the caller on a name-prefix guess.
    #[test]
    fn runtime_image_defines_class_reads_jmod_entries() {
        let dir = std::env::temp_dir().join("cratonvm_test_jmod_image_defines");
        let _ = fs::create_dir_all(&dir);
        let jmod_path = dir.join("java.base.jmod");
        let mut zip_buf = Vec::new();
        {
            let mut zip_writer = zip::ZipWriter::new(std::io::Cursor::new(&mut zip_buf));
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip_writer
                .start_file("classes/com/sun/crypto/provider/SunJCE.class", options)
                .unwrap();
            zip_writer.write_all(b"\xCA\xFE\xBA\xBE_fake").unwrap();
            zip_writer.finish().unwrap();
        }
        let mut jmod_data = Vec::new();
        jmod_data.extend_from_slice(&ClassPath::JMOD_MAGIC_PREFIX);
        jmod_data.push(ClassPath::JMOD_SUPPORTED_MAJOR);
        jmod_data.push(0x00);
        jmod_data.extend_from_slice(&zip_buf);
        fs::write(&jmod_path, &jmod_data).unwrap();

        let cp = ClassPath::new(&[jmod_path.to_string_lossy().into_owned()]);
        assert_eq!(
            cp.runtime_image_defines_class("com/sun/crypto/provider/SunJCE"),
            Some(true)
        );
        // JDK-shaped, not in the image: an application class.
        assert_eq!(
            cp.runtime_image_defines_class("com/sun/el/ExpressionFactoryImpl"),
            Some(false)
        );
        // No image entry at all: no answer.
        let no_image = ClassPath::new(&[dir.to_string_lossy().into_owned()]);
        assert_eq!(
            no_image.runtime_image_defines_class("com/sun/crypto/provider/SunJCE"),
            None
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_real_jdk_jmod() {
        // Skip this test if no JDK is available
        let java_home = cratonvm_types::flags::runtime_var("JAVA_HOME")
            .ok()
            .or_else(|| {
                let candidate = std::path::PathBuf::from("C:/Program Files/Java/jdk-25");
                if candidate.exists() {
                    Some(candidate.to_string_lossy().into_owned())
                } else {
                    None
                }
            });
        let Some(java_home) = java_home else {
            eprintln!("Skipping load_real_jdk_jmod: no JAVA_HOME set");
            return;
        };
        let jmod_path = PathBuf::from(&java_home)
            .join("jmods")
            .join("java.base.jmod");
        if !jmod_path.exists() {
            eprintln!(
                "Skipping load_real_jdk_jmod: {} not found",
                jmod_path.display()
            );
            return;
        }

        let cp = ClassPath::new(&[jmod_path.to_string_lossy().into_owned()]);
        assert!(!cp.is_empty(), "JMOD classpath should not be empty");

        // Must find java.lang.Object
        let obj_bytes = cp.find_class("java/lang/Object").unwrap();
        assert_eq!(
            &obj_bytes[..4],
            b"\xCA\xFE\xBA\xBE",
            "Object.class should have class file magic"
        );
        assert!(obj_bytes.len() > 100, "Object.class should be non-trivial");

        // Must find java.lang.String
        let str_bytes = cp.find_class("java/lang/String").unwrap();
        assert_eq!(&str_bytes[..4], b"\xCA\xFE\xBA\xBE");
        assert!(str_bytes.len() > 1000, "String.class should be substantial");

        // Must find java.util.HashMap
        let hm_bytes = cp.find_class("java/util/HashMap").unwrap();
        assert_eq!(&hm_bytes[..4], b"\xCA\xFE\xBA\xBE");

        // Class listing should have thousands of classes
        let names = cp.list_class_names();
        assert!(
            names.len() > 1000,
            "java.base should have >1000 classes, got {}",
            names.len()
        );
        assert!(names.contains(&"java/lang/Object".to_string()));
        assert!(names.contains(&"java/lang/String".to_string()));
        assert!(names.contains(&"java/util/HashMap".to_string()));
        assert!(names.contains(&"java/util/ArrayList".to_string()));
        assert!(names.contains(&"java/io/InputStream".to_string()));

        // module-info.class should be found
        let modules = cp.scan_module_infos();
        assert_eq!(
            modules.len(),
            1,
            "java.base.jmod should have exactly 1 module-info"
        );
    }

    // --- Phase 80.5: Path Traversal Fix Tests ---

    #[test]
    fn find_class_rejects_dotdot_traversal() {
        let cp = ClassPath::new(&[]);
        let result = cp.find_class("../../../etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn find_class_rejects_absolute_path() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("/etc/passwd").is_err());
        assert!(cp.find_class("\\Windows\\System32\\cmd").is_err());
    }

    #[test]
    fn find_class_rejects_null_byte() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("java/lang/Object\0.evil").is_err());
    }

    #[test]
    fn find_class_rejects_windows_drive() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("C:Windows/System32/cmd").is_err());
    }

    #[test]
    fn find_class_rejects_current_dir_reference() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_class("./java/lang/Object").is_err());
        assert!(cp.find_class(".\\java\\lang\\Object").is_err());
    }

    #[test]
    fn find_resource_rejects_dotdot_traversal() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_resource("../../../etc/passwd").is_none());
    }

    #[test]
    fn find_resource_rejects_null_byte() {
        let cp = ClassPath::new(&[]);
        assert!(cp.find_resource("file\0.txt").is_none());
    }

    #[test]
    fn find_class_canonicalize_with_real_dir() {
        // Create a temp directory with a class file, verify canonicalization works
        let tmp = std::env::temp_dir().join("cratonvm_test_80_5");
        let _ = std::fs::create_dir_all(&tmp);
        let class_dir = tmp.join("com").join("test");
        let _ = std::fs::create_dir_all(&class_dir);
        // Write a minimal (invalid but loadable) class file
        let _ = std::fs::write(class_dir.join("Foo.class"), b"CAFEBABE_fake");
        let mut cp = ClassPath::new(&[]);
        cp.add_path(tmp.to_str().unwrap());
        // Normal lookup should work
        let result = cp.find_class("com/test/Foo");
        assert!(result.is_ok());
        // Cleanup
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── JMOD integration tests ────────────────────────────────────────
    // These require a real JDK 9+ installation. Run with:
    //   cargo test -p cratonvm-classloading -- --ignored

    /// Helper: find java.base.jmod on this machine.
    fn find_java_base_jmod() -> Option<PathBuf> {
        // Try JAVA_HOME first
        if let Ok(val) = cratonvm_types::flags::runtime_var("JAVA_HOME") {
            let p = PathBuf::from(&val).join("jmods").join("java.base.jmod");
            if p.exists() {
                return Some(p);
            }
        }
        // Try common Windows paths
        for dir in &[
            r"C:\Program Files\Java",
            r"C:\Program Files\Eclipse Adoptium",
        ] {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let jmod = entry.path().join("jmods").join("java.base.jmod");
                    if jmod.exists() {
                        return Some(jmod);
                    }
                }
            }
        }
        // Try running java to find it
        let output = std::process::Command::new("java")
            .args(["-XshowSettings:properties", "-version"])
            .output()
            .ok()?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        for line in stderr.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("java.home") {
                if let Some(value) = rest.trim().strip_prefix('=') {
                    let jmod = PathBuf::from(value.trim())
                        .join("jmods")
                        .join("java.base.jmod");
                    if jmod.exists() {
                        return Some(jmod);
                    }
                }
            }
        }
        None
    }

    #[test]
    #[ignore] // requires JDK on host
    fn load_jmod_validates_magic_and_version() {
        let jmod_path = find_java_base_jmod();
        assert!(
            jmod_path.is_some(),
            "No JDK found — cannot test JMOD loading"
        );
        let jmod_path = jmod_path.unwrap();

        // Should load successfully
        let entry = ClassPath::load_jmod(&jmod_path);
        assert!(
            entry.is_ok(),
            "Failed to load {}: {:?}",
            jmod_path.display(),
            entry.err()
        );

        // Verify it's a JmodFile variant with a populated class_entry_index
        match entry.unwrap() {
            ClassPathEntry::JmodFile {
                class_entry_index,
                all_entry_names,
                ..
            } => {
                assert!(
                    class_entry_index.len() > 100,
                    "java.base should have >100 classes, got {}",
                    class_entry_index.len()
                );
                assert!(
                    all_entry_names.len() > class_entry_index.len(),
                    "all_entry_names should include non-class entries too"
                );
                // Should contain java.lang.Object
                assert!(
                    class_entry_index.contains("java/lang/Object.class"),
                    "java.base should contain java/lang/Object.class"
                );
                // Should contain java.lang.String
                assert!(
                    class_entry_index.contains("java/lang/String.class"),
                    "java.base should contain java/lang/String.class"
                );
                eprintln!(
                    "java.base.jmod: {} classes, {} total entries",
                    class_entry_index.len(),
                    all_entry_names.len()
                );
            }
            other => panic!("Expected JmodFile, got {other:?}"),
        }
    }

    #[test]
    #[ignore] // requires JDK on host
    fn find_class_from_jmod_returns_valid_classfile() {
        let jmod_path = find_java_base_jmod();
        if jmod_path.is_none() {
            eprintln!("Skipping: no JDK found");
            return;
        }
        let jmod_path = jmod_path.unwrap();

        let cp = ClassPath::new(&[jmod_path.to_string_lossy().into_owned()]);

        // Find java.lang.Object
        let bytes = cp.find_class("java/lang/Object").unwrap();
        // Verify it starts with the class file magic
        assert_eq!(
            &bytes[..4],
            &[0xCA, 0xFE, 0xBA, 0xBE],
            "java.lang.Object should start with CAFEBABE"
        );
        // Check class file version is reasonable (major >= 45 for any JDK)
        let major = u16::from_be_bytes([bytes[6], bytes[7]]);
        assert!(
            major >= 45 && major <= 80,
            "class file major version {major} out of expected range"
        );
        eprintln!(
            "java.lang.Object: {} bytes, class file version {major}",
            bytes.len()
        );

        // Find java.util.HashMap
        let bytes = cp.find_class("java/util/HashMap").unwrap();
        assert_eq!(&bytes[..4], &[0xCA, 0xFE, 0xBA, 0xBE]);

        // Non-existent class should fail
        assert!(cp.find_class("java/lang/NonExistent").is_err());
    }

    #[test]
    #[ignore] // requires JDK on host
    fn list_jmod_modules_returns_module_names() {
        let jmod_path = find_java_base_jmod();
        if jmod_path.is_none() {
            eprintln!("Skipping: no JDK found");
            return;
        }
        let jmod_path = jmod_path.unwrap();

        // Load just java.base
        let cp = ClassPath::new(&[jmod_path.to_string_lossy().into_owned()]);
        let modules = cp.list_jmod_modules();
        assert_eq!(modules, vec!["java.base"]);

        // Load all jmods from the JDK
        let jmods_dir = jmod_path.parent().unwrap();
        let mut all_jmods: Vec<String> = fs::read_dir(jmods_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "jmod"))
            .map(|e| e.path().to_string_lossy().into_owned())
            .collect();
        all_jmods.sort();

        let cp = ClassPath::new(&all_jmods);
        let modules = cp.list_jmod_modules();
        assert!(modules.contains(&"java.base".to_string()));
        assert!(modules.contains(&"java.logging".to_string()));
        assert!(
            modules.len() >= 10,
            "Expected at least 10 modules, got {}",
            modules.len()
        );
        eprintln!(
            "Found {} modules: {:?}",
            modules.len(),
            &modules[..5.min(modules.len())]
        );
    }

    #[test]
    fn load_jmod_rejects_bad_magic() {
        let dir = std::env::temp_dir().join("cratonvm_test_bad_jmod");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("bad.jmod");

        // Write a file with wrong magic
        fs::write(&path, b"BAAD\x00\x00\x00\x00").unwrap();
        let result = ClassPath::load_jmod(&path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("magic"), "Error should mention magic: {err}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_jmod_rejects_unsupported_major_version() {
        let dir = std::env::temp_dir().join("cratonvm_test_future_jmod");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("future.jmod");

        // Write JMOD header with major version 99 (unsupported)
        // followed by minimal ZIP data (will fail ZIP parse, but version check comes first)
        let mut data = vec![0x4A, 0x4D, 99, 0x00]; // JM + major=99 + minor=0
        data.extend_from_slice(&[0; 100]); // padding
        fs::write(&path, &data).unwrap();

        let result = ClassPath::load_jmod(&path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("major version"),
            "Error should mention major version: {err}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_jmod_rejects_too_small_file() {
        let dir = std::env::temp_dir().join("cratonvm_test_tiny_jmod");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("tiny.jmod");

        fs::write(&path, b"JM").unwrap(); // Only 2 bytes
        let result = ClassPath::load_jmod(&path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("too small"),
            "Error should mention file too small: {err}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn jmod_class_count_empty_classpath() {
        let cp = ClassPath::new(&[]);
        assert_eq!(cp.jmod_class_count(), 0);
    }

    #[test]
    fn list_jmod_modules_empty_classpath() {
        let cp = ClassPath::new(&[]);
        assert!(cp.list_jmod_modules().is_empty());
    }

    // -- ManifestInfo parsing tests --

    #[test]
    fn manifest_multi_release_true() {
        let data = b"Multi-Release: true\nMain-Class: com.example.Main\n";
        let info = ManifestInfo::parse(data);
        assert!(info.multi_release);
        assert_eq!(info.main_class.as_deref(), Some("com.example.Main"));
    }

    #[test]
    fn manifest_multi_release_false_by_default() {
        let data = b"Main-Class: com.example.Main\n";
        let info = ManifestInfo::parse(data);
        assert!(!info.multi_release);
    }

    #[test]
    fn manifest_multi_release_case_insensitive() {
        let data = b"Multi-Release: TRUE\n";
        let info = ManifestInfo::parse(data);
        assert!(info.multi_release);
    }

    #[test]
    fn manifest_classpath_not_dropped_when_large() {
        let mut entries = Vec::new();
        for i in 0..1200 {
            entries.push(format!("file:/C:/repo/lib/dep{i}.jar"));
        }
        let cp = entries.join(" ");
        let raw = format!("Manifest-Version: 1.0\nClass-Path: {cp}\n");
        let info = ManifestInfo::parse(raw.as_bytes());
        assert!(
            info.class_path.is_some(),
            "large Class-Path must be preserved"
        );
    }

    #[test]
    fn manifest_classpath_file_uri_windows_drive() {
        let data = b"Class-Path: file:/C:/Users/dev/.m2/repository/x/y.jar\n";
        let info = ManifestInfo::parse(data);
        let cp = info.resolve_class_path(Path::new("C:/tmp/booter.jar"));
        assert_eq!(cp.len(), 1);
        let norm = cp[0].replace('\\', "/");
        assert_eq!(norm, "C:/Users/dev/.m2/repository/x/y.jar");
    }

    #[test]
    fn manifest_classpath_relative_entries_still_resolve_against_jar_dir() {
        let data = b"Class-Path: lib/a.jar ../shared/b.jar\n";
        let info = ManifestInfo::parse(data);
        let cp = info.resolve_class_path(Path::new("C:/tmp/boot/booter.jar"));
        assert_eq!(cp.len(), 2);
        assert!(cp[0].replace('\\', "/").ends_with("/tmp/boot/lib/a.jar"));
        // `..` folded as `new URL(base, token)` folds it (wave 108, L5).
        assert!(cp[1].replace('\\', "/").ends_with("/tmp/shared/b.jar"), "{cp:?}");
    }

    /// Interpreter round i5 wave 106, lane L5: a relative token is a URL, so
    /// its `%XX` escapes are decoded before the join (HotSpot's
    /// `FileURLMapper` opens `lib dir/dep.jar` for `lib%20dir/dep.jar`).
    #[test]
    fn manifest_classpath_relative_entries_are_percent_decoded() {
        let data = b"Class-Path: lib%20dir/dep.jar plain.jar\n";
        let info = ManifestInfo::parse(data);
        let cp = info.resolve_class_path(Path::new("C:/tmp/boot/booter.jar"));
        assert_eq!(cp.len(), 2);
        assert!(cp[0].replace('\\', "/").ends_with("/tmp/boot/lib dir/dep.jar"), "{cp:?}");
        assert!(cp[1].replace('\\', "/").ends_with("/tmp/boot/plain.jar"), "{cp:?}");
    }

    /// Interpreter round i5 wave 107, lane L5: JDK 25 `URLClassPath` skips a
    /// whole jar whose `Class-Path` has a token with an unknown URL scheme (a
    /// drive spelling), and ignores a token with a known non-`file` scheme.
    #[test]
    fn manifest_classpath_unknown_scheme_drops_the_jar() {
        for drops in [
            "Class-Path: dep.jar C:/x.jar\n",
            "Class-Path: c:\\x.jar\n",
            "Class-Path: url:Z:/x.jar\n",
            "Class-Path: foo+bar:baz.jar\n",
        ] {
            assert!(ManifestInfo::parse(drops.as_bytes()).class_path_drops_jar(), "{drops}");
        }
        for keeps in [
            "Class-Path: dep.jar lib/a:b.jar 1:x.jar\n",
            "Class-Path: file:/C:/x.jar http://example.invalid/y.jar\n",
            "Class-Path: jar:file:/x.jar!/ mailto:a@b #c:d\n",
            "Manifest-Version: 1.0\n",
        ] {
            assert!(!ManifestInfo::parse(keeps.as_bytes()).class_path_drops_jar(), "{keeps}");
        }
        let info = ManifestInfo::parse(b"Class-Path: http://example.invalid/y.jar dep.jar\n");
        let cp = info.resolve_class_path(Path::new("C:/tmp/boot/main.jar"));
        assert_eq!(cp.len(), 1, "{cp:?}");
        assert!(cp[0].replace('\\', "/").ends_with("/tmp/boot/dep.jar"), "{cp:?}");
        assert_eq!(super::manifest_token_protocol("C:/x.jar").as_deref(), Some("c"));
        assert_eq!(super::manifest_token_protocol("lib/a:b.jar"), None);
    }

    /// Interpreter round i5 wave 108, lane L5: JDK 25's lazy application
    /// `URLClassPath` with URL handler provider lookups
    /// (`simulate_url_class_path`), measured on HotSpot 25.0.3 by
    /// `L5W107ClassPathDriveToken`: `main.jar` (`Class-Path: bad.jar
    /// good.jar`), `bad.jar` (`dep.jar C:/nowhere/x.jar`), `good.jar`
    /// (`http://example.invalid/y.jar`) ends with `main.jar` alone and one
    /// circularity `Error`.
    #[test]
    fn url_class_path_provider_lookups_follow_the_jdk() {
        use super::{simulate_url_class_path, ClassPathToken, UrlClassPathModel};
        fn run(
            top: &[&str],
            manifests: &[(&str, Vec<ClassPathToken>)],
        ) -> UrlClassPathModel {
            let top: Vec<PathBuf> = top.iter().map(PathBuf::from).collect();
            let mut tokens_of = |path: &Path| -> Option<Vec<ClassPathToken>> {
                manifests
                    .iter()
                    .find(|(name, _)| Path::new(name) == path)
                    .map(|(_, tokens)| tokens.clone())
            };
            simulate_url_class_path(&top, &mut tokens_of)
        }
        let file = |s: &str| ClassPathToken::File(s.to_string());
        let scheme = |s: &str| ClassPathToken::Scheme(s.to_string());
        let probe = [
            ("main.jar", vec![file("bad.jar"), file("good.jar")]),
            ("bad.jar", vec![file("dep.jar"), scheme("c")]),
            ("dep.jar", vec![]),
            ("good.jar", vec![scheme("http")]),
        ];
        let model = run(&["main.jar"], &probe);
        assert_eq!(model.order, vec![PathBuf::from("main.jar")]);
        assert_eq!(model.circularity_errors, 1);
        // The URLClassLoader rows: no circularity over a loader of its own.
        let model = run(&["bad.jar"], &probe);
        assert_eq!((model.order.len(), model.circularity_errors), (0, 0));
        let model = run(&["good.jar"], &probe);
        assert_eq!(model.order, vec![PathBuf::from("good.jar")]);
        // A known scheme alone: the jar goes AFTER the rest, then its own
        // `Class-Path`.
        let reorder = [
            ("a.jar", vec![scheme("http"), file("x.jar")]),
            ("b.jar", vec![]),
            ("x.jar", vec![]),
        ];
        let model = run(&["a.jar", "b.jar"], &reorder);
        let names: Vec<_> = model.order.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["b.jar", "a.jar", "x.jar"]);
        assert_eq!((model.circularity_errors, model.provider_lookups), (0, 1));
        // An unknown scheme with nothing after it: only the jar is dropped
        // (the wave-107 rule).
        let model = run(&["bad.jar", "good.jar"], &[("bad.jar", vec![scheme("c")]), ("good.jar", vec![])]);
        assert_eq!(model.order, vec![PathBuf::from("good.jar")]);
        assert_eq!(model.circularity_errors, 0);
        // `jar:` and `jrt:` never look a provider up.
        let model = run(&["a.jar"], &[("a.jar", vec![scheme("jar"), scheme("jrt")])]);
        assert_eq!((model.order.len(), model.provider_lookups), (1, 0));
    }

    /// Wave 108, lane L5 (`L5W108ClassPathDotSegments`): `dep.jar`,
    /// `sub/../dep.jar` and `./dep.jar` are one URL to the JDK, so one entry.
    #[test]
    fn manifest_classpath_dot_segments_fold_like_a_url() {
        let info = ManifestInfo::parse(b"Class-Path: dep.jar sub/../dep.jar ./dep.jar ../../../up.jar\n");
        let cp = info.resolve_class_path(Path::new("out/x/main.jar"));
        let cp: Vec<String> = cp.iter().map(|s| s.replace('\\', "/")).collect();
        assert_eq!(cp[0], "out/x/dep.jar");
        assert_eq!(cp[1], "out/x/dep.jar");
        assert_eq!(cp[2], "out/x/dep.jar");
        assert_eq!(cp[3], "../up.jar");
        assert_eq!(
            super::url_path_normalized(PathBuf::from("a/b.jar")),
            PathBuf::from("a/b.jar")
        );
    }

    #[test]
    fn manifest_classpath_continuation_lines_preserve_entries() {
        // 72-column style split: first line ends with a space before the break;
        // continuation line begins with one marker space then the next path.
        let raw = concat!(
            "Manifest-Version: 1.0\n",
            "Class-Path: file:/C:/repo/lib/one.jar \n",
            " file:/C:/repo/lib/two.jar\n",
            "\n"
        );
        let info = ManifestInfo::parse(raw.as_bytes());
        let cp = info.resolve_class_path(Path::new("C:/tmp/booter.jar"));
        assert_eq!(cp.len(), 2, "expected two jars, got {:?}", cp);
        assert!(cp[0].replace('\\', "/").contains("/repo/lib/one.jar"));
        assert!(cp[1].replace('\\', "/").contains("/repo/lib/two.jar"));
    }

    #[test]
    fn manifest_classpath_mid_token_continuation() {
        let raw = concat!(
            "Class-Path: file:/C:/repo/lib/prefix-\n",
            " suffix.jar\n",
            "\n"
        );
        let info = ManifestInfo::parse(raw.as_bytes());
        let cp = info.resolve_class_path(Path::new("C:/tmp/booter.jar"));
        assert_eq!(cp.len(), 1);
        assert!(cp[0].replace('\\', "/").contains("prefix-suffix.jar"));
    }

    /// Optional fixture: copy a Surefire booter `META-INF/MANIFEST.MF` to
    /// `CratonVM/target/booter-extract/MANIFEST.MF` to validate folding on a
    /// real multi-kilobyte Class-Path header.
    #[test]
    fn manifest_parse_optional_surefire_booter_fixture() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target/booter-extract/META-INF/MANIFEST.MF");
        if !path.exists() {
            return;
        }
        let data = std::fs::read(&path).expect("read fixture");
        let info = ManifestInfo::parse(&data);
        let cp = info.class_path.expect("Class-Path");
        assert!(
            cp.contains("junit-platform-launcher-1.10.1.jar"),
            "merged Class-Path should include junit launcher; len={}",
            cp.len()
        );
        assert!(
            !cp.contains(".././"),
            "malformed .././ segment after fold: {cp:?}"
        );
    }

    // -- Multi-release JAR tests --

    #[test]
    fn multi_release_jar_prefers_versioned_class() {
        let dir = std::env::temp_dir().join("cratonvm_test_mr_jar");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("mr.jar");

        let file = fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        // MANIFEST.MF with Multi-Release
        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(b"Multi-Release: true\n").unwrap();

        // Base class
        zip.start_file("com/example/Hello.class", opts).unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_base").unwrap();

        // Versioned class (Java 11)
        zip.start_file("META-INF/versions/11/com/example/Hello.class", opts)
            .unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_v11").unwrap();

        zip.finish().unwrap();

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp.find_class("com/example/Hello").unwrap();
        // Should get the versioned class (v11), not the base.
        assert_eq!(&data[4..], b"_v11");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn multi_release_jar_highest_version_wins() {
        let dir = std::env::temp_dir().join("cratonvm_test_mr_jar_highest");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("mr2.jar");

        let file = fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(b"Multi-Release: true\n").unwrap();

        // Base class
        zip.start_file("com/example/Foo.class", opts).unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_base").unwrap();

        // Java 11 version
        zip.start_file("META-INF/versions/11/com/example/Foo.class", opts)
            .unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_v11").unwrap();

        // Java 17 version
        zip.start_file("META-INF/versions/17/com/example/Foo.class", opts)
            .unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_v17").unwrap();

        zip.finish().unwrap();

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp.find_class("com/example/Foo").unwrap();
        // Should get the highest available version (v17), since our VM targets Java 25.
        assert_eq!(&data[4..], b"_v17");

        let _ = fs::remove_dir_all(&dir);
    }

    /// Interpreter round i5 wave 100, lane L5: the versioned view is the
    /// booted library's (`set_multi_release_feature`), clamped to 8..=25.
    #[test]
    fn i100_multi_release_jar_is_read_at_the_booted_feature_version() {
        let dir = std::env::temp_dir().join("cratonvm_test_mr_jar_i100_feature");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("mr3.jar");
        let file = fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(b"Multi-Release: true\n").unwrap();
        for (name, body) in [
            ("p/V.class", &b"\xCA\xFE\xBA\xBE_base"[..]),
            ("META-INF/versions/21/p/V.class", &b"\xCA\xFE\xBA\xBE_v21"[..]),
            ("META-INF/versions/25/p/V.class", &b"\xCA\xFE\xBA\xBE_v25"[..]),
        ] {
            zip.start_file(name, opts).unwrap();
            zip.write_all(body).unwrap();
        }
        zip.finish().unwrap();

        let at = |feature: Option<u32>| {
            let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
            if let Some(feature) = feature {
                cp.set_multi_release_feature(feature);
            }
            let bytes = cp.find_class("p/V").unwrap();
            (cp.multi_release_feature(), bytes[4..].to_vec())
        };
        assert_eq!(at(None), (25, b"_v25".to_vec()));
        assert_eq!(at(Some(21)), (21, b"_v21".to_vec()));
        assert_eq!(at(Some(17)), (17, b"_base".to_vec()));
        assert_eq!(at(Some(99)), (25, b"_v25".to_vec()));
        // Wave 103: 8 (`JarFile.BASE_VERSION_FEATURE`) serves the base entry only.
        assert_eq!(at(Some(8)), (8, b"_base".to_vec()));
        assert_eq!(at(Some(5)).0, 8);

        // `JarFile.<clinit>`'s `jdk.util.jar.version` / `enableMultiRelease` rule.
        assert_eq!(ClassPath::jar_runtime_version(25, None, None), 25);
        assert_eq!(ClassPath::jar_runtime_version(25, Some("21"), None), 21);
        assert_eq!(ClassPath::jar_runtime_version(21, Some("25"), None), 21);
        assert_eq!(ClassPath::jar_runtime_version(25, Some("7"), None), 8);
        assert_eq!(ClassPath::jar_runtime_version(25, Some("-3"), None), 8);
        assert_eq!(ClassPath::jar_runtime_version(25, Some("x"), None), 25);
        assert_eq!(ClassPath::jar_runtime_version(25, Some("21"), Some("false")), 8);
        assert_eq!(ClassPath::jar_runtime_version(25, None, Some("force")), 25);

        let _ = fs::remove_dir_all(&dir);
    }

    /// Interpreter round i5 wave 103, lane L5: the fast-define gate's name index
    /// counts a versioned class only under a version `JarFile.getEntry` searches
    /// at the class path's feature version (9..=feature, canonical decimal).
    #[test]
    fn i103_fast_absent_gate_honours_the_multi_release_feature() {
        let dir = std::env::temp_dir().join("cratonvm_test_mr_jar_i103_gate");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("mr_gate.jar");
        let file = fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(b"Multi-Release: true\n").unwrap();
        for name in [
            "p/V.class",
            "META-INF/versions/21/q/At21.class",
            "META-INF/versions/25/q/At25.class",
            "META-INF/versions/x/q/Odd.class",
            "META-INF/versions/021/q/Padded.class",
        ] {
            zip.start_file(name, opts).unwrap();
            zip.write_all(b"\xCA\xFE\xBA\xBE").unwrap();
        }
        zip.finish().unwrap();

        let absent = |feature: u32, name: &str| {
            let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
            cp.set_multi_release_feature(feature);
            cp.fast_absent_ok(name)
        };
        assert!(!absent(25, "p/V"));
        assert!(!absent(25, "q/At25"));
        assert!(!absent(25, "q/At21"));
        assert!(absent(21, "q/At25"));
        assert!(!absent(21, "q/At21"));
        assert!(absent(17, "q/At21"));
        assert!(absent(25, "q/Odd"));
        assert!(absent(25, "q/Padded"));

        let _ = fs::remove_dir_all(&dir);
    }

    /// Interpreter round i5 wave 104, lane L5: JDK 25's `getVersionedEntry`
    /// walks the present versions down to 8 (`BASE_VERSION_FEATURE`), so a
    /// `META-INF/versions/8/` entry is served at any version above 8 and
    /// never at 8 itself; versions below 8 and `META-INF/` names are never
    /// versioned.
    #[test]
    fn i104_multi_release_versions_8_is_served_above_the_base_version() {
        let dir = std::env::temp_dir().join("cratonvm_test_mr_jar_i104_v8");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("mr_v8.jar");
        let file = fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(b"Multi-Release: true\n").unwrap();
        for (name, body) in [
            ("p/V.class", &b"\xCA\xFE\xBA\xBE_base"[..]),
            ("META-INF/versions/8/p/V.class", &b"\xCA\xFE\xBA\xBE_v8"[..]),
            ("p/W.class", &b"\xCA\xFE\xBA\xBE_base"[..]),
            ("META-INF/versions/7/p/W.class", &b"\xCA\xFE\xBA\xBE_v7"[..]),
            ("META-INF/versions/8/q/Only8.class", &b"\xCA\xFE\xBA\xBE_v8"[..]),
        ] {
            zip.start_file(name, opts).unwrap();
            zip.write_all(body).unwrap();
        }
        zip.finish().unwrap();

        let at = |feature: u32, name: &str| {
            let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
            cp.set_multi_release_feature(feature);
            cp.find_class(name).ok().map(|b| b[4..].to_vec())
        };
        assert_eq!(at(25, "p/V"), Some(b"_v8".to_vec()));
        assert_eq!(at(9, "p/V"), Some(b"_v8".to_vec()));
        assert_eq!(at(8, "p/V"), Some(b"_base".to_vec()));
        assert_eq!(at(25, "p/W"), Some(b"_base".to_vec()));
        assert_eq!(at(25, "q/Only8"), Some(b"_v8".to_vec()));
        assert_eq!(at(8, "q/Only8"), None);

        let absent = |feature: u32, name: &str| {
            let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
            cp.set_multi_release_feature(feature);
            cp.fast_absent_ok(name)
        };
        assert!(!absent(25, "q/Only8"));
        assert!(absent(8, "q/Only8"));

        assert_eq!(versioned_lookup_range("p/V.class", 25), 8..26);
        assert_eq!(versioned_lookup_range("p/V.class", 8), 0..0);
        assert_eq!(versioned_lookup_range("META-INF/services/x", 25), 0..0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_multi_release_jar_ignores_versioned() {
        let dir = std::env::temp_dir().join("cratonvm_test_no_mr");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("no_mr.jar");

        let file = fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        // No Multi-Release header
        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(b"Main-Class: com.example.Main\n").unwrap();

        zip.start_file("com/example/Hello.class", opts).unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_base").unwrap();

        zip.start_file("META-INF/versions/17/com/example/Hello.class", opts)
            .unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_v17").unwrap();

        zip.finish().unwrap();

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp.find_class("com/example/Hello").unwrap();
        // Without Multi-Release flag, should get the base version.
        assert_eq!(&data[4..], b"_base");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn multi_release_jar_falls_back_to_base() {
        let dir = std::env::temp_dir().join("cratonvm_test_mr_fallback");
        let _ = fs::create_dir_all(&dir);
        let jar_path = dir.join("mr_fallback.jar");

        let file = fs::File::create(&jar_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(b"Multi-Release: true\n").unwrap();

        // Only base class (no versioned entry)
        zip.start_file("com/example/Base.class", opts).unwrap();
        zip.write_all(b"\xCA\xFE\xBA\xBE_base_only").unwrap();

        zip.finish().unwrap();

        let cp = ClassPath::new(&[jar_path.to_string_lossy().into_owned()]);
        let data = cp.find_class("com/example/Base").unwrap();
        assert_eq!(&data[4..], b"_base_only");

        let _ = fs::remove_dir_all(&dir);
    }

    // =======================================================================
    // NEW-5 — jimage integration
    // =======================================================================

    /// Produce a temp-file path unique to this test run so concurrent
    /// tests don't collide.
    fn jimage_temp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("cratonvm_jimage_test");
        let _ = fs::create_dir_all(&dir);
        dir.join(format!(
            "{tag}_{}.img",
            std::process::id().wrapping_mul(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_nanos())
                    .unwrap_or(0)
            )
        ))
    }

    fn sample_jimage_resources() -> Vec<(
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static [u8],
    )> {
        vec![
            (
                "java.base",
                "java/lang",
                "String",
                "class",
                b"\xCA\xFE\xBA\xBE_jimage_string".as_ref(),
            ),
            (
                "java.base",
                "java/util",
                "HashMap",
                "class",
                b"\xCA\xFE\xBA\xBE_jimage_hashmap".as_ref(),
            ),
            (
                "java.desktop",
                "javax/swing",
                "JFrame",
                "class",
                b"\xCA\xFE\xBA\xBE_jimage_jframe".as_ref(),
            ),
            (
                "java.base",
                "",
                "module-info",
                "class",
                b"\xCA\xFE\xBA\xBE_jimage_base_module_info".as_ref(),
            ),
            (
                "java.desktop",
                "",
                "module-info",
                "class",
                b"\xCA\xFE\xBA\xBE_jimage_desktop_module_info".as_ref(),
            ),
        ]
    }

    /// Write a synthetic jimage to disk and load it through
    /// `ClassPath::add_jimage`. The resulting classpath must resolve
    /// every class via `find_class`.
    #[test]
    fn jimage_find_class_round_trip() {
        let data = cratonvm_reader::jimage::test_builder::build_simple(&sample_jimage_resources());
        let path = jimage_temp_path("find_class");
        fs::write(&path, &data).unwrap();

        let mut cp = ClassPath::new(&[]);
        cp.add_jimage(&path).expect("add_jimage");

        // `module-info` is scoped per-module and not resolvable via
        // the flat `find_class(name)` API (see `load_jimage`). All
        // other sample classes round-trip exactly.
        for (_module, parent, base, _ext, bytes) in sample_jimage_resources() {
            if parent.is_empty() && base == "module-info" {
                continue;
            }
            let internal = format!("{parent}/{base}");
            let got = cp.find_class(&internal).expect("class present");
            assert_eq!(got.as_ref(), bytes, "bytes for {internal} must round-trip");
        }

        let _ = fs::remove_file(&path);
    }

    /// The `lib/modules` auto-detection in `ClassPath::new` should pick
    /// up a file literally named `modules` without an extension.
    #[test]
    fn jimage_auto_detected_by_filename() {
        let data = cratonvm_reader::jimage::test_builder::build_simple(&sample_jimage_resources());
        let dir = std::env::temp_dir().join(format!("cratonvm_jimage_auto_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("modules");
        fs::write(&path, &data).unwrap();

        let cp = ClassPath::new(&[path.to_string_lossy().into_owned()]);
        assert!(
            !cp.is_empty(),
            "ClassPath::new should auto-detect a file named `modules`"
        );
        let bytes = cp
            .find_class("java/lang/String")
            .expect("class present via auto-detected jimage");
        assert!(bytes.starts_with(b"\xCA\xFE\xBA\xBE"));

        let _ = fs::remove_dir_all(&dir);
    }

    /// `scan_module_infos` should return a module-info entry for every
    /// distinct module in the jimage.
    #[test]
    fn jimage_scan_module_infos_returns_every_module() {
        let data = cratonvm_reader::jimage::test_builder::build_simple(&sample_jimage_resources());
        let path = jimage_temp_path("scan_module_infos");
        fs::write(&path, &data).unwrap();

        let mut cp = ClassPath::new(&[]);
        cp.add_jimage(&path).unwrap();

        let infos = cp.scan_module_infos();
        assert_eq!(
            infos.len(),
            2,
            "exactly two modules in the sample jimage ({} found)",
            infos.len()
        );
        // Both should start with the class-file magic. The exact bytes
        // reflect what our sample resources wrote.
        for info in &infos {
            assert!(info.starts_with(b"\xCA\xFE\xBA\xBE"));
        }
        let _ = fs::remove_file(&path);
    }

    /// The class-to-module index exposed via `jimage_class_count` must
    /// reflect every `.class` entry in the image (module-infos count).
    #[test]
    fn jimage_class_count_matches_resources() {
        let data = cratonvm_reader::jimage::test_builder::build_simple(&sample_jimage_resources());
        let path = jimage_temp_path("class_count");
        fs::write(&path, &data).unwrap();

        let mut cp = ClassPath::new(&[]);
        cp.add_jimage(&path).unwrap();

        // 3 regular classes (module-info entries are not counted
        // because they're scoped per-module, not global).
        assert_eq!(cp.jimage_class_count(), 3);
        let modules = cp.list_jimage_modules();
        assert_eq!(
            modules,
            vec!["java.base".to_string(), "java.desktop".to_string()]
        );

        let _ = fs::remove_file(&path);
    }

    /// zs s6/rs: the per-package index (default) and the per-class map
    /// (`CRATONVM_JIMAGE_PACKAGE_INDEX=0`) answer every class-path question
    /// alike, including for a class absent from a package the image holds, and
    /// the default arm really is the per-package one.
    /// zt t3/fp: with the switch, an image's `/packages` and `/modules`
    /// directory records are neither resources nor modules; real resources
    /// and classes answer alike on both arms; the kill arm (positive control)
    /// still indexes them.
    #[test]
    fn zt3fp_jimage_directory_records_are_not_resources() {
        let mut resources = sample_jimage_resources();
        resources.push(("java.base", "java/lang", "zt3fp", "dat", b"zt3fp_res".as_ref()));
        resources.push(("packages", "java.lang", "java.base", "", b"\0\0\0\0".as_ref()));
        resources.push(("modules", "java.base", "zt3fp", "dir", b"zt3fp_dir".as_ref()));
        let data = cratonvm_reader::jimage::test_builder::build_simple(&resources);
        let path = jimage_temp_path("zt3fp_dir_records");
        fs::write(&path, &data).unwrap();
        let open = |skip: bool| {
            let mut cp = ClassPath::new(&[]);
            let entry = ClassPath::load_jimage_with_opts(&path, true, skip).unwrap();
            push_classpath_entry(&mut cp.entries, entry);
            cp
        };
        let (skip, keep) = (open(true), open(false));
        assert!(
            keep.contains_resource("java.lang/java.base"),
            "positive control"
        );
        assert!(keep.list_jimage_modules().contains(&"packages".to_string()));
        assert!(!skip.contains_resource("java.lang/java.base"));
        assert!(skip.find_resource("java.lang/java.base").is_none());
        assert!(!skip.contains_resource("java.base/zt3fp.dir"));
        for m in ["packages", "modules"] {
            assert!(!skip.list_jimage_modules().contains(&m.to_string()), "{m}");
        }
        for res in ["java/lang/zt3fp.dat", "java/lang/String.class"] {
            assert!(skip.contains_resource(res), "{res}");
            assert_eq!(skip.find_resource(res), keep.find_resource(res), "{res}");
        }
        assert_eq!(skip.jimage_class_count(), keep.jimage_class_count());
        let _ = fs::remove_file(&path);
    }

    /// The index `load_jimage_with_opts` built before p5-cp (every component
    /// string read per entry, in walk order): `(class -> module, resource ->
    /// modules, module names)`. The reference for the offset walk.
    fn p5cp_reference_jimage_index(
        reader: &cratonvm_reader::JImageReader,
        skip_dirs: bool,
    ) -> (
        Vec<(String, String)>,
        FxHashMap<String, Vec<String>>,
        Vec<String>,
    ) {
        let mut classes: FxHashMap<String, String> = FxHashMap::default();
        let mut resources: FxHashMap<String, Vec<String>> = FxHashMap::default();
        let mut module_set: std::collections::BTreeSet<String> = Default::default();
        reader
            .for_each_entry_parts(|module, parent, base, extension| {
                let mut full = String::new();
                for (sep, part) in [('/', module), ('/', parent), ('/', base), ('.', extension)] {
                    if !part.is_empty() {
                        full.push(sep);
                        full.push_str(part);
                    }
                }
                let Some(without_lead) = full.strip_prefix('/') else {
                    return;
                };
                let Some((m, r)) = without_lead.split_once('/') else {
                    return;
                };
                if m.is_empty() || r.is_empty() {
                    return;
                }
                if skip_dirs && ClassPath::is_jimage_directory_record_module(m) {
                    return;
                }
                module_set.insert(m.to_string());
                match r.strip_suffix(".class") {
                    // A `module-info` base is never a class, in any package.
                    Some(c) if c == "module-info" || c.ends_with("/module-info") => {}
                    Some(c) => {
                        classes.insert(c.to_string(), m.to_string());
                    }
                    None => resources
                        .entry(r.to_string())
                        .or_default()
                        .push(m.to_string()),
                }
            })
            .unwrap();
        let mut classes: Vec<(String, String)> = classes.into_iter().collect();
        classes.sort();
        (classes, resources, module_set.into_iter().collect())
    }

    /// p5-cp: the offset walk (memoised module, package, `class` and
    /// `module-info` per string offset) builds exactly the index the
    /// string-per-entry walk did, on both index arms and both directory
    /// record arms: same classes and modules, same resources with their
    /// modules IN WALK ORDER (the first one is the one served), same module
    /// list. The image repeats a resource across modules, has an entry
    /// without a module, a class in the unnamed package and directory records.
    #[test]
    fn p5cp_offset_walk_builds_the_string_walks_index() {
        let mut resources = sample_jimage_resources();
        let extra: [(&str, &str, &str, &str, &[u8]); 9] = [
            ("java.base", "java/lang", "p5cp", "dat", b"p5_a"),
            ("java.desktop", "java/lang", "p5cp", "dat", b"p5_b"),
            ("jdk.zipfs", "java/lang", "p5cp", "dat", b"p5_c"),
            ("java.base", "", "Root", "class", b"p5_root"),
            ("java.base", "META-INF", "p5", "", b"p5_noext"),
            ("", "java.base", "Loose", "class", b"p5_loose"),
            ("packages", "java.lang", "java.base", "", b"\0\0\0\0"),
            ("modules", "java.base", "p5", "dir", b"p5_dir"),
            ("java.base", "java/lang", "module-info", "class", b"p5_mi"),
        ];
        resources.extend(extra);
        let data = cratonvm_reader::jimage::test_builder::build_simple(&resources);
        let path = jimage_temp_path("p5cp_offset_walk");
        fs::write(&path, &data).unwrap();
        let reader = ClassPath::open_jimage_reader(&path).unwrap();
        for skip in [true, false] {
            let (want_classes, want_resources, want_modules) =
                p5cp_reference_jimage_index(&reader, skip);
            assert!(want_resources["java/lang/p5cp.dat"].len() == 3);
            for by_package in [true, false] {
                let ClassPathEntry::JImageFile {
                    reader: r,
                    class_to_module,
                    resource_to_modules,
                    module_names,
                    ..
                } = ClassPath::load_jimage_with_opts(&path, by_package, skip).unwrap()
                else {
                    panic!("not a jimage entry");
                };
                let arm = format!("skip={skip} by_package={by_package}");
                assert_eq!(resource_to_modules, want_resources, "{arm}");
                assert_eq!(module_names, want_modules, "{arm}");
                assert_eq!(class_to_module.class_count(), want_classes.len(), "{arm}");
                let mut names = class_to_module.class_names(&r);
                names.sort();
                let want_names: Vec<String> = want_classes.iter().map(|c| c.0.clone()).collect();
                assert_eq!(names, want_names, "{arm}");
                for (class, module) in &want_classes {
                    assert_eq!(
                        class_to_module.candidate_module(class),
                        Some(module.as_str()),
                        "{arm} {class}"
                    );
                }
            }
        }
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn zs6rs_jimage_package_index_answers_as_the_class_map() {
        let mut resources = sample_jimage_resources();
        resources.push((
            "java.base",
            "",
            "Unnamed",
            "class",
            b"zs6rs_unnamed".as_ref(),
        ));
        resources.push((
            "java.base",
            "java/lang",
            "module-info",
            "class",
            b"zs6rs_mi".as_ref(),
        ));
        let data = cratonvm_reader::jimage::test_builder::build_simple(&resources);
        let path = jimage_temp_path("zs6rs_package_index");
        fs::write(&path, &data).unwrap();
        let open = |by_package: bool| {
            let mut cp = ClassPath::new(&[]);
            let entry = ClassPath::load_jimage_with(&path, by_package).unwrap();
            if let ClassPathEntry::JImageFile {
                class_to_module, ..
            } = &entry
            {
                assert_eq!(
                    matches!(class_to_module, JImageClassIndex::Packages { .. }),
                    by_package
                );
            }
            push_classpath_entry(&mut cp.entries, entry);
            cp
        };
        let (pk, cl) = (open(true), open(false));
        assert_eq!(pk.jimage_class_count(), cl.jimage_class_count());
        let sorted = |cp: &ClassPath| {
            let mut v = cp.list_class_names();
            v.sort();
            v
        };
        assert_eq!(sorted(&pk), sorted(&cl));
        for name in [
            "java/lang/String",
            "java/util/HashMap",
            "javax/swing/JFrame",
            "Unnamed",
            // Absent, in a package the image holds / does not hold.
            "java/lang/Missing",
            "javax/swing/Missing",
            "com/example/Absent",
            "module-info",
            "java/lang/module-info",
        ] {
            assert_eq!(
                pk.runtime_image_defines_class(name),
                cl.runtime_image_defines_class(name),
                "{name}"
            );
            assert_eq!(
                pk.find_class(name).ok().map(|b| b.to_vec()),
                cl.find_class(name).ok().map(|b| b.to_vec()),
                "{name}"
            );
            let res = format!("{name}.class");
            assert_eq!(
                pk.contains_resource(&res),
                cl.contains_resource(&res),
                "{res}"
            );
            assert_eq!(
                pk.find_all_resource_urls(&res),
                cl.find_all_resource_urls(&res),
                "{res}"
            );
        }
        assert_eq!(
            pk.runtime_image_defines_class("java/lang/String"),
            Some(true)
        );
        assert_eq!(
            pk.runtime_image_defines_class("java/lang/Missing"),
            Some(false)
        );
        let _ = fs::remove_file(&path);
    }

    /// zs s6/rs: the real JDK image (`$JAVA_HOME/lib/modules`) indexed both
    /// ways gives the same answer for every class it holds and for misses, and
    /// the default arm is the per-package one (no split package in a JDK).
    /// Ignored by default (needs `JAVA_HOME`); prints the per-arm resident
    /// estimate of the index.
    #[test]
    #[ignore]
    fn zs6rs_real_jdk_image_package_index_matches_the_class_map() {
        let Some(home) = std::env::var_os("JAVA_HOME") else {
            eprintln!("zs6rs: JAVA_HOME unset, skipped");
            return;
        };
        let path = std::path::PathBuf::from(home).join("lib").join("modules");
        let pk = ClassPath::load_jimage_with(&path, true).unwrap();
        let cl = ClassPath::load_jimage_with(&path, false).unwrap();
        let (
            ClassPathEntry::JImageFile {
                reader: pr,
                class_to_module: pi,
                ..
            },
            ClassPathEntry::JImageFile {
                class_to_module: ci,
                ..
            },
        ) = (&pk, &cl)
        else {
            panic!("not jimage entries");
        };
        let JImageClassIndex::Packages {
            modules, packages, ..
        } = pi
        else {
            panic!("the JDK image fell back to the per-class map");
        };
        let JImageClassIndex::Classes(map) = ci else {
            panic!("kill arm is not the per-class map");
        };
        assert_eq!(pi.class_count(), map.len());
        let mut names = pi.class_names(pr);
        names.sort();
        let mut want: Vec<String> = map.keys().cloned().collect();
        want.sort();
        assert_eq!(names, want);
        for (class, module) in map {
            assert_eq!(pi.candidate_module(class), Some(module.as_str()), "{class}");
            assert!(pi.contains(pr, class), "{class}");
        }
        for miss in [
            "java/lang/NoSuchClassZs6",
            "com/example/NoSuch",
            "module-info",
            "NoPackage",
        ] {
            assert!(!pi.contains(pr, miss), "{miss}");
            assert!(!ci.contains(pr, miss), "{miss}");
        }
        let key_bytes: usize = map.iter().map(|(k, v)| k.len() + v.len()).sum();
        eprintln!(
            "zs6rs-index classes={} packages={} modules={} class_map_capacity={} \
             class_map_string_bytes={key_bytes}",
            map.len(),
            packages.len(),
            modules.len(),
            map.capacity()
        );
    }

    /// The s6 builder's module numbering (a linear search per entry), kept
    /// as the reference for zs s9/rs2b's name map: `(modules, packages,
    /// classes, split, string compares the linear search made)`.
    fn zs9rs2b_reference_index(
        reader: &cratonvm_reader::JImageReader,
    ) -> (Vec<String>, Vec<(String, u32)>, usize, bool, usize) {
        let mut modules: Vec<String> = Vec::new();
        let mut packages: FxHashMap<String, u32> = FxHashMap::default();
        let (mut last, mut classes, mut split, mut compares) = (None::<u32>, 0, false, 0);
        reader
            .for_each_entry_parts(|module, parent, base, extension| {
                let Some((m, c)) = jimage_class_entry(module, parent, base, extension) else {
                    return;
                };
                if split {
                    return;
                }
                let id = match last {
                    Some(id) if modules[id as usize] == m => id,
                    _ => {
                        let found = modules.iter().position(|x| {
                            compares += 1;
                            *x == m
                        });
                        let id = match found {
                            Some(i) => i as u32,
                            None => {
                                modules.push(m.clone());
                                (modules.len() - 1) as u32
                            }
                        };
                        last = Some(id);
                        id
                    }
                };
                classes += 1;
                let package = jimage_package_of(&c).to_string();
                match packages.get(&package) {
                    Some(&have) if have != id => split = true,
                    Some(_) => {}
                    None => {
                        packages.insert(package, id);
                    }
                }
            })
            .unwrap();
        let mut packages: Vec<(String, u32)> = packages.into_iter().collect();
        packages.sort();
        (modules, packages, classes, split, compares)
    }

    /// The new builder's result in the reference's shape, plus its
    /// `module_lookups`.
    fn zs9rs2b_new_index(
        reader: &cratonvm_reader::JImageReader,
    ) -> (Vec<String>, Vec<(String, u32)>, usize, bool, usize) {
        let mut b = JImageIndexBuilder::new(true, 0);
        reader
            .for_each_entry_parts(|module, parent, base, extension| {
                if let Some((m, c)) = jimage_class_entry(module, parent, base, extension) {
                    b.add(&c, &m);
                }
            })
            .unwrap();
        let lookups = b.module_lookups;
        let (split, modules) = (b.split, b.modules.iter().map(|m| m.to_string()).collect());
        let mut packages: Vec<(String, u32)> = b
            .packages
            .iter()
            .map(|(k, &v)| (k.to_string(), v))
            .collect();
        packages.sort();
        (modules, packages, b.filed, split, lookups)
    }

    /// zs s9/rs2b (rv6 X3): the name-map module numbering gives exactly the
    /// linear search's index (same module order, same package -> module ids,
    /// same counts, same split verdict), on a built image.
    #[test]
    fn zs9rs2b_module_map_builds_the_same_index_as_the_linear_search() {
        let mut resources = sample_jimage_resources();
        resources.push((
            "java.desktop",
            "java/awt",
            "Frame",
            "class",
            b"zs9_f".as_ref(),
        ));
        resources.push(("java.base", "java/util", "List", "class", b"zs9_l".as_ref()));
        let data = cratonvm_reader::jimage::test_builder::build_simple(&resources);
        let path = jimage_temp_path("zs9rs2b_same_index");
        fs::write(&path, &data).unwrap();
        let reader = ClassPath::open_jimage_reader(&path).unwrap();
        let want = zs9rs2b_reference_index(&reader);
        let got = zs9rs2b_new_index(&reader);
        assert_eq!(
            (&got.0, &got.1, got.2, got.3),
            (&want.0, &want.1, want.2, want.3)
        );
        let _ = fs::remove_file(&path);
    }

    /// zs s9/rs2b: the same on the real JDK image (ignored; needs
    /// `JAVA_HOME`). Prints what the walk cost each way.
    #[test]
    #[ignore]
    fn zs9rs2b_real_jdk_image_module_map_matches_the_linear_search() {
        let Some(home) = std::env::var_os("JAVA_HOME") else {
            eprintln!("zs9rs2b: JAVA_HOME unset, skipped");
            return;
        };
        let path = std::path::PathBuf::from(home).join("lib").join("modules");
        let reader = ClassPath::open_jimage_reader(&path).unwrap();
        let t0 = std::time::Instant::now();
        let want = zs9rs2b_reference_index(&reader);
        let t_ref = t0.elapsed();
        let t1 = std::time::Instant::now();
        let got = zs9rs2b_new_index(&reader);
        let t_new = t1.elapsed();
        assert_eq!(
            (&got.0, &got.1, got.2, got.3),
            (&want.0, &want.1, want.2, want.3)
        );
        assert!(!got.3, "a JDK image has no split package");
        eprintln!(
            "zs9rs2b-walk classes={} modules={} linear_compares={} map_lookups={} \
             ref_walk_us={} new_walk_us={}",
            got.2,
            got.0.len(),
            want.4,
            got.4,
            t_ref.as_micros(),
            t_new.as_micros()
        );
    }

    /// zs s6/rs: a package in two modules keeps the per-class map (last entry
    /// wins, as before).
    #[test]
    fn zs6rs_a_split_package_keeps_the_class_map() {
        let resources = vec![
            ("mod.a", "p", "Alpha", "class", b"zs6rs_a".as_ref()),
            ("mod.b", "p", "Beta", "class", b"zs6rs_b".as_ref()),
            ("mod.a", "q", "Gamma", "class", b"zs6rs_c".as_ref()),
        ];
        let data = cratonvm_reader::jimage::test_builder::build_simple(&resources);
        let path = jimage_temp_path("zs6rs_split");
        fs::write(&path, &data).unwrap();
        let entry = ClassPath::load_jimage_with(&path, true).unwrap();
        let ClassPathEntry::JImageFile {
            class_to_module, ..
        } = &entry
        else {
            panic!("not a jimage entry");
        };
        assert!(matches!(class_to_module, JImageClassIndex::Classes(_)));
        assert_eq!(class_to_module.candidate_module("p/Alpha"), Some("mod.a"));
        assert_eq!(class_to_module.candidate_module("p/Beta"), Some("mod.b"));
        assert_eq!(class_to_module.class_count(), 3);
        let _ = fs::remove_file(&path);
    }

    /// `list_class_names` should include every class in the jimage.
    #[test]
    fn jimage_list_class_names_includes_every_entry() {
        let data = cratonvm_reader::jimage::test_builder::build_simple(&sample_jimage_resources());
        let path = jimage_temp_path("list_class_names");
        fs::write(&path, &data).unwrap();

        let mut cp = ClassPath::new(&[]);
        cp.add_jimage(&path).unwrap();

        let mut names = cp.list_class_names();
        names.sort();
        assert!(names.contains(&"java/lang/String".to_string()));
        assert!(names.contains(&"java/util/HashMap".to_string()));
        assert!(names.contains(&"javax/swing/JFrame".to_string()));
        // module-info is deliberately excluded from `class_to_module`
        // because it is scoped per-module rather than globally unique.
        assert!(
            !names.contains(&"module-info".to_string()),
            "module-info must not appear as a regular class name"
        );

        let _ = fs::remove_file(&path);
    }

    /// Missing-class lookups against a jimage classpath must return
    /// ClassNotFound and not leak a panic.
    #[test]
    fn jimage_missing_class_returns_clean_error() {
        let data = cratonvm_reader::jimage::test_builder::build_simple(&sample_jimage_resources());
        let path = jimage_temp_path("missing");
        fs::write(&path, &data).unwrap();

        let mut cp = ClassPath::new(&[]);
        cp.add_jimage(&path).unwrap();

        let err = cp.find_class("no/such/Class").unwrap_err();
        assert!(matches!(err, ClassFileError::ClassNotFound { .. }));

        let _ = fs::remove_file(&path);
    }

    /// `add_jimage` on a non-existent path must return an error, not
    /// a silent no-op.
    #[test]
    fn jimage_add_nonexistent_returns_error() {
        let mut cp = ClassPath::new(&[]);
        let err = cp
            .add_jimage("/nonexistent/cratonvm/jimage/file")
            .unwrap_err();
        assert!(
            err.contains("open jimage"),
            "error message should mention open failure: {err}"
        );
    }

    /// `add_jimage` on a bogus file (not a valid jimage) must return
    /// an error without corrupting the classpath.
    #[test]
    fn jimage_add_bogus_file_returns_error() {
        let path = jimage_temp_path("bogus");
        fs::write(&path, b"this is not a jimage file").unwrap();

        let mut cp = ClassPath::new(&[]);
        let err = cp.add_jimage(&path).unwrap_err();
        assert!(
            err.contains("open jimage") || err.contains("jimage"),
            "error should reference jimage: {err}"
        );
        assert!(cp.is_empty(), "classpath must be unchanged on failure");

        let _ = fs::remove_file(&path);
    }

    // -----------------------------------------------------------------------
    // T19_H10: ManifestInfo.attributes — generic attribute capture so
    // `Class.getPackage().getImplementationVersion()` and friends can read
    // arbitrary `Implementation-*` / `Specification-*` headers without
    // expanding `ManifestInfo` for every new field.
    // -----------------------------------------------------------------------

    #[test]
    fn t19_h10_manifest_attributes_captures_implementation_fields() {
        // Use \n-only line separators (no continuation prefix) so the
        // parser captures every attribute as its own entry.
        let manifest = b"Manifest-Version: 1.0\n\
Implementation-Title: keycloak-common\n\
Implementation-Version: 26.2.4\n\
Implementation-Vendor: Red Hat, Inc.\n\
Specification-Title: Keycloak Common\n\
Specification-Version: 26.2\n\
Specification-Vendor: Keycloak\n";
        let info = ManifestInfo::parse(manifest);
        assert_eq!(
            info.attributes
                .get("Implementation-Title")
                .map(String::as_str),
            Some("keycloak-common")
        );
        assert_eq!(
            info.attributes
                .get("Implementation-Version")
                .map(String::as_str),
            Some("26.2.4")
        );
        assert_eq!(
            info.attributes
                .get("Implementation-Vendor")
                .map(String::as_str),
            Some("Red Hat, Inc.")
        );
        assert_eq!(
            info.attributes
                .get("Specification-Title")
                .map(String::as_str),
            Some("Keycloak Common")
        );
        assert_eq!(
            info.attributes
                .get("Specification-Version")
                .map(String::as_str),
            Some("26.2")
        );
        assert_eq!(
            info.attributes
                .get("Specification-Vendor")
                .map(String::as_str),
            Some("Keycloak")
        );
    }

    #[test]
    fn t19_h10_manifest_attributes_returns_none_for_missing() {
        let manifest = b"Manifest-Version: 1.0\nMain-Class: foo.Bar\n";
        let info = ManifestInfo::parse(manifest);
        assert!(
            info.attributes.get("Implementation-Version").is_none(),
            "absent attribute must yield None"
        );
        assert_eq!(
            info.attributes.get("Manifest-Version").map(String::as_str),
            Some("1.0")
        );
    }

    #[test]
    fn t19_h10_manifest_attributes_skips_per_entry_sections() {
        // The `\n\n` separator ends the main section. Anything after must
        // not contaminate `info.attributes`.
        let manifest = b"Manifest-Version: 1.0\n\
Implementation-Version: 1.0\n\
\n\
Name: foo/Bar.class\n\
Implementation-Version: 999.999\n";
        let info = ManifestInfo::parse(manifest);
        assert_eq!(
            info.attributes
                .get("Implementation-Version")
                .map(String::as_str),
            Some("1.0"),
            "main-section attribute wins; per-entry section is ignored"
        );
        assert!(
            info.attributes.get("Name").is_none(),
            "per-entry `Name` header must not leak into main attributes"
        );
    }

    #[test]
    fn t19_h10_manifest_attributes_caps_oversized_value() {
        // Build a manifest with one valid line + one 16 KiB-value line.
        // The big line must be silently dropped (returned as None) so a
        // hostile signed jar cannot blow our heap.
        let big = "X".repeat(16 * 1024);
        let raw = format!("Manifest-Version: 1.0\nFoo: {big}\nBar: ok\n");
        let info = ManifestInfo::parse(raw.as_bytes());
        assert!(
            info.attributes.get("Foo").is_none(),
            "oversized value must be dropped"
        );
        assert_eq!(
            info.attributes.get("Bar").map(String::as_str),
            Some("ok"),
            "subsequent attributes must still be captured"
        );
    }

    // ── Classpath wildcard expansion (Elasticsearch / Cassandra parity) ──
    //
    // HotSpot's `java -cp lib/*` expands to every `*.jar` directly under
    // `lib/`. Real-world launchers (Elasticsearch, Cassandra, hand-rolled
    // `bin/foo` shell scripts) rely on this; without it `ServiceLoader`
    // sees zero classpath entries and returns an empty iterator.

    /// Build a temp dir with N JARs (valid zips, each containing a
    /// `META-INF/services/dummy.SPI` so `find_all_resource_urls` has
    /// something to enumerate). Returns the dir.
    fn make_jar_dir(name: &str, jar_names: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for jar in jar_names {
            let p = dir.join(jar);
            let f = fs::File::create(&p).unwrap();
            let mut zip = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file("META-INF/services/dummy.SPI", opts).unwrap();
            zip.write_all(b"com.example.Provider\n").unwrap();
            zip.finish().unwrap();
        }
        dir
    }

    /// `dir/*` must expand to every `.jar` in the directory.
    #[test]
    fn wildcard_expands_dir_star_to_all_jars() {
        let dir = make_jar_dir("cratonvm_wildcard_dir_star", &["a.jar", "b.jar", "c.jar"]);
        let pattern = format!("{}/*", dir.to_string_lossy());
        let cp = ClassPath::new(&[pattern]);
        assert_eq!(cp.entry_count(), 3, "wildcard should expand to 3 jars");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The launcher-facing expansion API must publish concrete paths too.
    /// In-process javac reads `java.class.path` and cannot resolve a literal
    /// `lib/*` token on its own.
    #[test]
    fn wildcard_publication_expands_to_concrete_sorted_jars() {
        let dir = make_jar_dir(
            "cratonvm_wildcard_property_publication",
            &["z.jar", "a.jar", "m.jar"],
        );
        let pattern = format!("{}/*", dir.to_string_lossy());
        let expanded = ClassPath::expand_classpath_entries(&[pattern]);
        let names: Vec<_> = expanded
            .iter()
            .map(|path| {
                Path::new(path)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, ["a.jar", "m.jar", "z.jar"]);
        assert!(expanded.iter().all(|path| !path.contains('*')));
        let _ = fs::remove_dir_all(&dir);
    }

    /// All wildcard-expanded jars must be visible to
    /// `find_all_resource_urls` — this is the load-bearing claim for
    /// ServiceLoader. Before the fix this returned an empty Vec because
    /// the literal `lib/*` entry was silently dropped as a non-existent
    /// path.
    #[test]
    fn wildcard_makes_every_jar_searchable_for_services() {
        let dir = make_jar_dir(
            "cratonvm_wildcard_services",
            &["alpha.jar", "beta.jar", "gamma.jar"],
        );
        let pattern = format!("{}/*", dir.to_string_lossy());
        let cp = ClassPath::new(&[pattern]);
        let urls = cp.find_all_resource_urls("META-INF/services/dummy.SPI");
        assert_eq!(
            urls.len(),
            3,
            "every wildcard-expanded jar must contribute its META-INF/services entry"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Backslash form `dir\*` (Windows-style) must work identically.
    #[test]
    fn wildcard_accepts_backslash_form() {
        let dir = make_jar_dir("cratonvm_wildcard_backslash", &["x.jar", "y.jar"]);
        let pattern = format!("{}\\*", dir.to_string_lossy());
        let cp = ClassPath::new(&[pattern]);
        assert_eq!(cp.entry_count(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Non-jar files in the wildcard directory are ignored.
    #[test]
    fn wildcard_skips_non_jar_files() {
        let dir = make_jar_dir("cratonvm_wildcard_skips_non_jar", &["good.jar"]);
        fs::write(dir.join("README.txt"), b"hello").unwrap();
        fs::write(dir.join("native.dll"), [0u8; 8]).unwrap();
        let pattern = format!("{}/*", dir.to_string_lossy());
        let cp = ClassPath::new(&[pattern]);
        assert_eq!(cp.entry_count(), 1, "only `.jar` files must be picked up");
        let _ = fs::remove_dir_all(&dir);
    }

    /// `*.JAR` (upper-case extension) is accepted — Windows is
    /// case-insensitive and real-world launchers occasionally ship
    /// `Some-Lib.JAR`.
    #[test]
    fn wildcard_accepts_uppercase_jar_extension() {
        let dir = make_jar_dir("cratonvm_wildcard_uppercase", &["LIB.JAR"]);
        let pattern = format!("{}/*", dir.to_string_lossy());
        let cp = ClassPath::new(&[pattern]);
        assert_eq!(cp.entry_count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    /// `add_path` (dynamic loading via URLClassLoader / module loader)
    /// honours the same wildcard so app servers that synthesise `lib/*`
    /// strings at runtime benefit.
    #[test]
    fn wildcard_works_via_add_path() {
        let dir = make_jar_dir("cratonvm_wildcard_add_path", &["one.jar", "two.jar"]);
        let pattern = format!("{}/*", dir.to_string_lossy());
        let mut cp = ClassPath::new(&[]);
        cp.add_path(&pattern);
        assert_eq!(cp.entry_count(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Non-wildcard entries pass through unchanged — `foo.jar` must
    /// not be treated as a wildcard pattern (regression guard against
    /// false-positive expansion).
    #[test]
    fn wildcard_passthrough_for_plain_jar() {
        let dir = make_jar_dir("cratonvm_wildcard_passthrough", &["a.jar"]);
        let plain = dir.join("a.jar").to_string_lossy().into_owned();
        let cp = ClassPath::new(&[plain]);
        assert_eq!(cp.entry_count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Explicit URLClassLoader entries are archive URLs, not Java launcher
    /// wildcard entries: a valid ZIP must remain searchable even when its
    /// extension is `.par` rather than `.jar`.
    #[test]
    fn explicit_non_jar_archive_is_searchable() {
        let dir = make_jar_dir("cratonvm_packaged_archive_suffix", &["payload.jar"]);
        let jar = dir.join("payload.jar");
        let par = dir.join("payload.par");
        fs::rename(&jar, &par).unwrap();

        let cp = ClassPath::new(&[par.to_string_lossy().into_owned()]);
        assert_eq!(
            cp.entry_count(),
            1,
            "the .par ZIP must be admitted as an archive"
        );
        let urls = cp.find_all_resource_urls("META-INF/services/dummy.SPI");
        assert_eq!(
            urls.len(),
            1,
            "the resource inside the .par must be visible"
        );
        assert!(urls[0].contains("payload.par"));

        let _ = fs::remove_dir_all(&dir);
    }

    /// Wildcard pointing at a non-existent directory must silently
    /// expand to nothing (matches `java -cp missing/*` behaviour).
    #[test]
    fn wildcard_missing_dir_yields_empty_classpath() {
        let cp = ClassPath::new(&["definitely_does_not_exist_12345/*".to_string()]);
        assert!(cp.is_empty());
    }

    /// Provider ordering must be deterministic across runs. Two
    /// invocations on the same directory must produce the same URL
    /// order — `ServiceLoader` clients can depend on first-match
    /// semantics.
    #[test]
    fn wildcard_yields_deterministic_jar_order() {
        let dir = make_jar_dir(
            "cratonvm_wildcard_order",
            &["zeta.jar", "alpha.jar", "mu.jar"],
        );
        let pattern = format!("{}/*", dir.to_string_lossy());
        let cp1 = ClassPath::new(&[pattern.clone()]);
        let cp2 = ClassPath::new(&[pattern]);
        let urls1 = cp1.find_all_resource_urls("META-INF/services/dummy.SPI");
        let urls2 = cp2.find_all_resource_urls("META-INF/services/dummy.SPI");
        assert_eq!(urls1, urls2, "wildcard expansion order must be stable");
        // Sanity: lexicographic — alpha < mu < zeta.
        assert!(urls1[0].contains("alpha.jar"));
        assert!(urls1[1].contains("mu.jar"));
        assert!(urls1[2].contains("zeta.jar"));
        let _ = fs::remove_dir_all(&dir);
    }

    // ── ES2 multi-jar SPI enumeration (Elasticsearch parity) ───────────
    //
    // Elasticsearch's launcher script passes every jar EXPLICITLY (not
    // via `lib/*` wildcard). Only one of those 60+ jars holds the
    // `META-INF/services/org.elasticsearch.cli.CliToolProvider` SPI
    // descriptor. The other 59 must be walked without producing false
    // positives or short-circuiting the search after the first miss.
    //
    // Regression target: an earlier version of `find_all_resource_urls`
    // could short-circuit on `find_in_archive` returning Err (vs Ok(None)),
    // which would cause the walk to abort silently after the first jar
    // that lacked the entry. The current code returns Ok-or-None for
    // every entry and continues, but a unit test pins the contract.

    /// Build N empty jars + 1 jar that holds the SPI descriptor. All N+1
    /// must be walked by `find_all_resource_urls`, and exactly ONE URL
    /// must come back — the one from the lone jar that has the entry.
    /// Mirrors the Elasticsearch `-cp <60 explicit jars>` shape.
    #[test]
    fn explicit_multi_jar_classpath_finds_lone_spi_provider() {
        let dir = std::env::temp_dir().join("cratonvm_es2_multi_jar_spi");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // 19 empty jars (just an empty MANIFEST.MF) + 1 jar with the SPI.
        let mut cp_entries: Vec<String> = Vec::new();
        for i in 0..19 {
            let p = dir.join(format!("empty-{i}.jar"));
            let f = fs::File::create(&p).unwrap();
            let mut zip = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
            zip.write_all(b"Manifest-Version: 1.0\n").unwrap();
            zip.finish().unwrap();
            cp_entries.push(p.to_string_lossy().into_owned());
        }
        // The lone provider jar — modelled after server-cli-8.15.5.jar.
        let provider = dir.join("server-cli-x.y.z.jar");
        {
            let f = fs::File::create(&provider).unwrap();
            let mut zip = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file(
                "META-INF/services/org.elasticsearch.cli.CliToolProvider",
                opts,
            )
            .unwrap();
            zip.write_all(b"org.elasticsearch.server.cli.ServerCliProvider\n")
                .unwrap();
            zip.finish().unwrap();
        }
        cp_entries.push(provider.to_string_lossy().into_owned());

        let cp = ClassPath::new(&cp_entries);
        assert_eq!(
            cp.entry_count(),
            20,
            "every explicitly-listed jar must register as a classpath entry"
        );
        let urls =
            cp.find_all_resource_urls("META-INF/services/org.elasticsearch.cli.CliToolProvider");
        assert_eq!(
            urls.len(),
            1,
            "exactly one jar holds the SPI descriptor; walk must not \
             short-circuit on the first miss. Got urls={urls:?}",
        );
        assert!(
            urls[0].contains("server-cli-x.y.z.jar"),
            "the lone hit must point at the provider jar, got {}",
            urls[0]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Same multi-jar walk with `find_all_resource_bytes` — the Rust
    /// helper path used by service_loader.rs that bypasses URL stream
    /// handling. Must produce exactly one byte block matching the SPI
    /// content.
    #[test]
    fn explicit_multi_jar_classpath_finds_lone_spi_bytes() {
        let dir = std::env::temp_dir().join("cratonvm_es2_multi_jar_spi_bytes");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let mut cp_entries: Vec<String> = Vec::new();
        for i in 0..9 {
            let p = dir.join(format!("nothing-{i}.jar"));
            let f = fs::File::create(&p).unwrap();
            let mut zip = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file("README", opts).unwrap();
            zip.write_all(b"nothing here\n").unwrap();
            zip.finish().unwrap();
            cp_entries.push(p.to_string_lossy().into_owned());
        }
        let provider = dir.join("provider.jar");
        {
            let f = fs::File::create(&provider).unwrap();
            let mut zip = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file("META-INF/services/foo.Bar", opts).unwrap();
            zip.write_all(b"foo.BarImpl\n").unwrap();
            zip.finish().unwrap();
        }
        cp_entries.push(provider.to_string_lossy().into_owned());

        let cp = ClassPath::new(&cp_entries);
        let bytes = cp.find_all_resource_bytes("META-INF/services/foo.Bar");
        assert_eq!(bytes.len(), 1);
        assert_eq!(bytes[0], b"foo.BarImpl\n");
        let _ = fs::remove_dir_all(&dir);
    }

    // -- T1.8.2 follow-up (MED #32): fat-JAR re-open path no longer panics --

    /// Regression test: a malformed/truncated archive must NOT panic via the
    /// previous `.unwrap()` on the fat-JAR re-open path. Feeding raw garbage
    /// fails the very first `ZipArchive::new` and pushes no entries.
    #[test]
    fn load_jar_data_with_truncated_bytes_does_not_panic() {
        // Eight bytes that are not a valid zip central directory.
        let garbage: Vec<u8> = b"\x00\x01\x02\x03\x04\x05\x06\x07".to_vec();
        let mut entries: Vec<ClassPathEntry> = Vec::new();
        let path = Path::new("truncated.jar");

        // The fix removed `.unwrap()` on re-open; the first open also
        // fails here, exercising the broader no-panic contract.
        ClassPath::load_jar_data(path, garbage, &mut entries);

        assert!(
            entries.is_empty(),
            "expected no entries from malformed archive, got {}",
            entries.len()
        );
    }

    /// Regression test: a fat JAR header that contains a Spring-Boot manifest
    /// but is truncated after the central-directory probe must not panic on
    /// the re-open path either. We construct a fat JAR, mangle the trailing
    /// bytes after the in-memory buffer has been built, and confirm load
    /// proceeds without panicking.
    #[test]
    fn fat_jar_corrupted_after_first_open_does_not_panic() {
        // Build a valid fat JAR in memory.
        let mut buf = Vec::new();
        {
            let cursor = Cursor::new(&mut buf);
            let mut zip = zip::ZipWriter::new(cursor);
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
            zip.write_all(
                b"Manifest-Version: 1.0\r\n\
                  Main-Class: org.springframework.boot.loader.JarLauncher\r\n\
                  Start-Class: com.example.App\r\n\
                  Spring-Boot-Classes: BOOT-INF/classes/\r\n\
                  Spring-Boot-Lib: BOOT-INF/lib/\r\n",
            )
            .unwrap();
            zip.start_file("BOOT-INF/classes/com/example/App.class", opts)
                .unwrap();
            zip.write_all(b"\xCA\xFE\xBA\xBE_app").unwrap();
            zip.finish().unwrap();
        }

        // Sanity: a pristine fat JAR loads fine and pushes >=1 entry.
        let mut ok_entries: Vec<ClassPathEntry> = Vec::new();
        ClassPath::load_jar_data(Path::new("ok.jar"), buf.clone(), &mut ok_entries);
        assert!(
            !ok_entries.is_empty(),
            "pristine fat JAR should push at least the BOOT-INF entries"
        );

        // Truncate the buffer mid-central-directory. The first open in
        // `load_jar_data` may still succeed for some inputs, but in either
        // case the re-open path (formerly `.unwrap()`) must not panic.
        let mut truncated = buf.clone();
        if truncated.len() > 32 {
            truncated.truncate(truncated.len() - 32);
        }
        let mut entries: Vec<ClassPathEntry> = Vec::new();
        ClassPath::load_jar_data(Path::new("corrupt.jar"), truncated, &mut entries);
        // No assertion on entry count: the contract is "does not panic".
    }

    // -- VULN (low) manifest Class-Path hardening: `path_is_within` --
    //
    // These exercise the pure lexical containment helper that backs the
    // opt-in `CRATONVM_HARDEN_MANIFEST_CLASSPATH` policy. The env-gated
    // wiring in `resolve_class_path` is not toggled here (the flag latches
    // in a process-wide `OnceLock`), so we test the decision function
    // directly.

    #[test]
    fn path_is_within_accepts_descendants() {
        assert!(path_is_within(
            Path::new("C:/app/lib/dep.jar"),
            Path::new("C:/app")
        ));
        assert!(path_is_within(
            Path::new("C:/app/lib/sub/dep.jar"),
            Path::new("C:/app/lib")
        ));
        // Identical path is "within" itself.
        assert!(path_is_within(Path::new("C:/app"), Path::new("C:/app")));
    }

    #[test]
    fn path_is_within_rejects_dotdot_escape() {
        // `..` that climbs out of the base directory must be rejected.
        assert!(!path_is_within(
            Path::new("C:/app/../secret/x.jar"),
            Path::new("C:/app")
        ));
        assert!(!path_is_within(
            Path::new("C:/app/lib/../../secret"),
            Path::new("C:/app")
        ));
    }

    #[test]
    fn path_is_within_rejects_unrelated_absolute_root() {
        // A sibling/foreign absolute root (e.g. manifest `file:/etc`) is
        // not a descendant of the JAR directory.
        assert!(!path_is_within(Path::new("C:/etc"), Path::new("C:/app")));
        assert!(!path_is_within(
            Path::new("D:/other/x.jar"),
            Path::new("C:/app")
        ));
    }

    #[test]
    fn path_is_within_dot_dot_normalizes_back_inside() {
        // `lib/../lib2` stays inside `C:/app`.
        assert!(path_is_within(
            Path::new("C:/app/lib/../lib2/dep.jar"),
            Path::new("C:/app")
        ));
    }

    #[test]
    fn manifest_classpath_escape_preserved_by_default() {
        // Regression guard for the DEFAULT (HotSpot-parity) behaviour: with
        // the hardening flag OFF, a manifest Class-Path that escapes the
        // JAR directory MUST still resolve (matching
        // `manifest_classpath_relative_entries_still_resolve_against_jar_dir`).
        // This protects the trusted-manifest contract `decode_manifest_classpath_entry`
        // documents from accidental regression.
        let data = b"Class-Path: ../../shared/b.jar\n";
        let info = ManifestInfo::parse(data);
        let cp = info.resolve_class_path(Path::new("C:/tmp/boot/booter.jar"));
        assert_eq!(
            cp.len(),
            1,
            "default policy must keep escaping entries: {cp:?}"
        );
        // Folded lexically as the JDK's URL folds it (wave 108, L5): the
        // escape is kept, only its spelling changes.
        assert!(cp[0].replace('\\', "/").ends_with("C:/shared/b.jar"), "{cp:?}");
    }

    // -----------------------------------------------------------------------
    // Lazy JMOD class serving (2026-07-26 boot-classpath-lazy)
    //
    // `load_jmod` used to inflate every `classes/` entry at load time. It now
    // indexes names only and inflates per lookup. These tests pin the three
    // things that change could plausibly break: the bytes, the fall-through to
    // other classpath entries, and the behaviour on a damaged archive.
    // -----------------------------------------------------------------------

    /// The 4-byte JMOD header that precedes the embedded ZIP.
    const TEST_JMOD_HEADER: [u8; 4] = [0x4A, 0x4D, 0x01, 0x00];

    /// Build a JMOD image in memory: `JM\x01\x00` followed by a ZIP holding
    /// `entries` verbatim (names are used exactly as given, so callers
    /// control whether an entry lands under `classes/`).
    ///
    /// Entries are **Deflated**, not Stored, so the lazy path is exercised
    /// against genuinely compressed data — a Stored entry would be served by
    /// the zero-copy `ArchiveSlice` branch and would never prove that
    /// on-demand inflation produces the right bytes.
    fn build_test_jmod_bytes(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in entries {
            zip.start_file(*name, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        let zip_bytes = zip.finish().unwrap().into_inner();

        let mut out = Vec::with_capacity(TEST_JMOD_HEADER.len() + zip_bytes.len());
        out.extend_from_slice(&TEST_JMOD_HEADER);
        out.extend_from_slice(&zip_bytes);
        out
    }

    fn jmod_temp_path(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cratonvm_lazy_jmod_{}_{}", std::process::id(), tag));
        let _ = fs::create_dir_all(&dir);
        dir.join(format!("{tag}.jmod"))
    }

    /// A class body big enough that its deflate stream spans many bytes, so
    /// the corruption test below has payload to damage. Deterministic so a
    /// failure is reproducible.
    fn synthetic_class_bytes(seed: u8, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        out.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]);
        let mut x = seed as u32 | 1;
        while out.len() < len {
            // xorshift — pseudo-random so the entry does not compress to
            // nothing, but fully reproducible.
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            out.push((x & 0xFF) as u8);
        }
        out
    }

    /// Equivalence with the eager implementation this replaced: for every
    /// `classes/` entry, `find_class` must return exactly the bytes that
    /// pre-extracting the whole archive would have cached.
    ///
    /// The reference side is computed here by replicating the old eager loop
    /// against the same archive, so this compares the two strategies rather
    /// than comparing the new one against a hand-written expectation.
    #[test]
    fn jmod_lazy_lookup_matches_eager_extraction_byte_for_byte() {
        let entries: Vec<(&str, Vec<u8>)> = vec![
            (
                "classes/java/lang/Object.class",
                synthetic_class_bytes(1, 3000),
            ),
            (
                "classes/java/lang/String.class",
                synthetic_class_bytes(2, 5000),
            ),
            (
                "classes/java/util/List.class",
                synthetic_class_bytes(3, 700),
            ),
            ("classes/module-info.class", synthetic_class_bytes(4, 400)),
            (
                "classes/META-INF/services/example.Service",
                b"example.ServiceImpl\n".to_vec(),
            ),
            // Not under `classes/` — a JMOD's build-time payload. Must not be
            // indexed as a class and must not be counted.
            ("legal/java.base/LICENSE", b"license text".to_vec()),
            ("lib/libjava.so", vec![0u8; 64]),
        ];
        let path = jmod_temp_path("equivalence");
        fs::write(&path, build_test_jmod_bytes(&entries)).unwrap();

        // --- reference: what the old eager pre-extraction would have cached
        let raw = fs::read(&path).unwrap();
        let mut reference: HashMap<String, Vec<u8>> = HashMap::new();
        {
            let mut archive = ZipArchive::new(Cursor::new(raw[4..].to_vec())).unwrap();
            for i in 0..archive.len() {
                let name = archive.by_index_raw(i).unwrap().name().to_string();
                if let Some(relative) = name.strip_prefix("classes/") {
                    if relative.is_empty() || relative.ends_with('/') {
                        continue;
                    }
                    let mut entry = archive.by_name(&name).unwrap();
                    let mut buf = Vec::new();
                    entry.read_to_end(&mut buf).unwrap();
                    reference.insert(relative.to_string(), buf);
                }
            }
        }
        assert_eq!(reference.len(), 5, "five entries live under classes/");

        let cp = ClassPath::new(&[path.to_string_lossy().into_owned()]);

        // The index must agree with the eager cache on membership...
        assert_eq!(
            cp.jmod_class_count(),
            reference.len(),
            "index must cover exactly the classes/ subtree"
        );

        // ...and on bytes, for every class the eager cache would have held.
        for name in ["java/lang/Object", "java/lang/String", "java/util/List"] {
            let got = cp.find_class(name).expect("class must resolve");
            let want = reference
                .get(&format!("{name}.class"))
                .expect("reference entry");
            assert_eq!(
                got.as_ref(),
                want.as_slice(),
                "lazy bytes for {name} must equal eagerly-extracted bytes"
            );
        }

        // Non-class resources under `classes/` come back too.
        assert_eq!(
            cp.find_resource("META-INF/services/example.Service"),
            Some(b"example.ServiceImpl\n".to_vec())
        );

        // A second lookup returns the same bytes — inflating on demand must
        // be idempotent, not stateful.
        let first = cp.find_class("java/lang/String").unwrap();
        let second = cp.find_class("java/lang/String").unwrap();
        assert_eq!(first.as_ref(), second.as_ref());

        // Entries outside `classes/` are not classes.
        assert!(cp.find_class("legal/java.base/LICENSE").is_err());
        assert!(cp.find_class("java/lang/Missing").is_err());

        // The code source still points at the JMOD, and answering that
        // question never needs the bytes.
        assert_eq!(
            cp.find_class_source_path("java/lang/Object"),
            Some(path.to_string_lossy().into_owned())
        );
        assert_eq!(cp.find_class_source_path("java/lang/Missing"), None);

        let _ = fs::remove_file(&path);
    }

    /// A JMOD and a jimage on the same classpath: a class only the JMOD has
    /// must still resolve. This is the case that makes the "just prefer
    /// `lib/modules`" alternative unsafe, and it is what the fall-through in
    /// `find_class` exists for.
    #[test]
    fn class_only_in_jmod_resolves_when_jimage_is_also_on_the_path() {
        let jimage_data =
            cratonvm_reader::jimage::test_builder::build_simple(&sample_jimage_resources());
        let jimage_path = jimage_temp_path("jmod_fallthrough");
        fs::write(&jimage_path, &jimage_data).unwrap();

        let jmod_only = synthetic_class_bytes(9, 2048);
        let jmod_path = jmod_temp_path("fallthrough");
        fs::write(
            &jmod_path,
            build_test_jmod_bytes(&[("classes/com/example/OnlyInJmod.class", jmod_only.clone())]),
        )
        .unwrap();

        // jimage first, JMOD second — the ordering a `lib/modules`-preferring
        // boot classpath would produce.
        let mut cp = ClassPath::new(&[]);
        cp.add_jimage(&jimage_path).expect("add_jimage");
        cp.add_path(&jmod_path.to_string_lossy());
        assert_eq!(cp.entry_count(), 2, "both entries must be on the classpath");

        // The jimage-only class resolves through the jimage reader...
        assert_eq!(
            cp.find_class("java/lang/String").unwrap().as_ref(),
            b"\xCA\xFE\xBA\xBE_jimage_string".as_ref()
        );
        // ...and the JMOD-only class resolves through the lazy JMOD path,
        // after the jimage arm has declined it.
        assert_eq!(
            cp.find_class("com/example/OnlyInJmod").unwrap().as_ref(),
            jmod_only.as_slice()
        );

        let _ = fs::remove_file(&jimage_path);
        let _ = fs::remove_file(&jmod_path);
    }

    /// Malformed and truncated JMODs must produce errors, never panics.
    /// `load_jmod` is reached from `ClassPath::new` on any host-supplied
    /// `JAVA_HOME`, so a damaged install must degrade to "class not found".
    #[test]
    fn malformed_jmod_errors_instead_of_panicking() {
        let good = build_test_jmod_bytes(&[(
            "classes/java/lang/Object.class",
            synthetic_class_bytes(7, 4096),
        )]);

        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("empty", Vec::new()),
            ("shorter_than_header", vec![0x4A, 0x4D, 0x01]),
            ("bad_magic", {
                let mut v = good.clone();
                v[0] = 0x50;
                v[1] = 0x4B;
                v
            }),
            ("future_major_version", {
                let mut v = good.clone();
                v[2] = 0x02;
                v
            }),
            ("header_only", TEST_JMOD_HEADER.to_vec()),
            ("garbage_after_header", {
                let mut v = TEST_JMOD_HEADER.to_vec();
                v.extend_from_slice(&[0xFFu8; 512]);
                v
            }),
            // Valid header, ZIP truncated mid-way: the central directory is
            // gone, so the archive cannot be opened at all.
            ("truncated_zip", good[..good.len() / 2].to_vec()),
        ];

        for (tag, bytes) in cases {
            let path = jmod_temp_path(tag);
            fs::write(&path, &bytes).unwrap();

            // Direct: `load_jmod` reports an error string.
            assert!(
                ClassPath::load_jmod(&path).is_err(),
                "load_jmod({tag}) must return Err, not succeed or panic"
            );

            // Through the public entry point: the damaged file is skipped and
            // lookups fail cleanly.
            let cp = ClassPath::new(&[path.to_string_lossy().into_owned()]);
            assert!(
                cp.find_class("java/lang/Object").is_err(),
                "find_class on damaged JMOD ({tag}) must return Err"
            );

            let _ = fs::remove_file(&path);
        }
    }

    /// Corruption that the *eager* loader would have absorbed at load time
    /// (the entry simply never entered the cache) now surfaces at lookup
    /// time. It must surface as `ClassNotFound`, not as a panic and not as
    /// silently truncated class bytes.
    ///
    /// The archive itself stays well-formed — only the last entry's
    /// compressed payload is damaged — so `load_jmod` succeeds and the name
    /// is in the index, which is precisely the new lazy-path-only case.
    #[test]
    fn jmod_with_corrupt_entry_payload_reports_class_not_found() {
        let mut bytes = build_test_jmod_bytes(&[(
            "classes/java/lang/Object.class",
            synthetic_class_bytes(11, 65536),
        )]);

        // Locate the central directory via the end-of-central-directory
        // record (no ZIP comment is written, so it is the final 22 bytes).
        let eocd = bytes.len() - 22;
        assert_eq!(
            &bytes[eocd..eocd + 4],
            &[0x50, 0x4B, 0x05, 0x06],
            "expected EOCD signature at end of synthesized JMOD"
        );
        let cd_offset = u32::from_le_bytes([
            bytes[eocd + 16],
            bytes[eocd + 17],
            bytes[eocd + 18],
            bytes[eocd + 19],
        ]) as usize
            + TEST_JMOD_HEADER.len(); // offsets are relative to the ZIP, not the JMOD

        // Damage 256 bytes of the compressed stream, ending just before the
        // central directory. Well clear of the local file header.
        let end = cd_offset;
        let start = end - 256;
        for b in &mut bytes[start..end] {
            *b ^= 0xFF;
        }

        let path = jmod_temp_path("corrupt_payload");
        fs::write(&path, &bytes).unwrap();

        // The archive still parses: the index is built from the central
        // directory, which we did not touch.
        let entry = ClassPath::load_jmod(&path).expect("central directory is intact");
        match &entry {
            ClassPathEntry::JmodFile {
                class_entry_index, ..
            } => assert!(
                class_entry_index.contains("java/lang/Object.class"),
                "the damaged entry is still indexed"
            ),
            other => panic!("expected JmodFile, got {other:?}"),
        }

        // But serving it fails cleanly rather than panicking or returning
        // a partially-inflated class file.
        let cp = ClassPath::new(&[path.to_string_lossy().into_owned()]);
        match cp.find_class("java/lang/Object") {
            Err(ClassFileError::ClassNotFound { .. }) => {}
            Err(other) => panic!("expected ClassNotFound, got {other:?}"),
            Ok(bytes) => panic!(
                "corrupt entry must not resolve; got {} bytes",
                bytes.as_ref().len()
            ),
        }

        let _ = fs::remove_file(&path);
    }
}

// ---------------------------------------------------------------------------
// Interpreter round i3 wave 79, lane L5: the class-path stamp
// ---------------------------------------------------------------------------

#[cfg(test)]
mod i79_l5_stamp_tests {
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
}

#[cfg(test)]
mod i79_l5_plain_view_tests {
    use super::*;
    use std::io::Write;

    /// A stored zip of `entries`, in memory.
    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::new());
        {
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let mut z = zip::ZipWriter::new(&mut cursor);
            for (name, data) in entries {
                z.start_file(*name, stored).unwrap();
                z.write_all(data).unwrap();
            }
            z.finish().unwrap();
        }
        cursor.into_inner()
    }

    /// `zip` 2.x's writer refuses a duplicate name, so the duplicate is made
    /// by renaming `p/B.class` to `p/A.class` (same length) in the local
    /// header and the central directory.
    fn with_duplicate() -> Vec<u8> {
        let mut bytes = zip_of(&[("p/A.class", &b"first"[..]), ("p/B.class", &b"other"[..])]);
        let from = b"p/B.class";
        let mut at = 0;
        while let Some(pos) = bytes[at..].windows(from.len()).position(|w| w == from) {
            bytes[at + pos..at + pos + from.len()].copy_from_slice(b"p/A.class");
            at += pos + from.len();
        }
        bytes
    }

    /// i71-L5 "the plain view's duplicate-name check is always true": the
    /// `zip` map keeps one entry per name, so a jar with two records of
    /// `p/A.class` has `len() == 1` == its entry index; the record walk sees
    /// two, and the plain view is refused. A clean jar keeps it.
    #[test]
    fn i79_a_duplicated_name_has_no_plain_view() {
        let dup = ZipArchive::new(Cursor::new(ArchiveBacking::from_vec(with_duplicate()))).unwrap();
        assert_eq!(
            dup.len(),
            1,
            "the zip crate's map keeps one record per name"
        );
        assert_eq!(shared_archive_records(&dup), Some(2), "two records on disk");
        let mut indexed = dup.clone();
        let index = ClassPath::build_archive_entry_index(&mut indexed);
        assert_eq!(index.len(), dup.len(), "the old check could not tell");
        assert!(!plain_view_of(
            &index,
            dup.len(),
            shared_archive_records(&dup)
        ));

        let clean = ZipArchive::new(Cursor::new(ArchiveBacking::from_vec(zip_of(&[
            ("p/A.class", &b"first"[..]),
            ("p/B.class", &b"other"[..]),
        ]))))
        .unwrap();
        let mut indexed = clean.clone();
        let index = ClassPath::build_archive_entry_index(&mut indexed);
        assert_eq!(shared_archive_records(&clean), Some(2));
        assert!(plain_view_of(
            &index,
            clean.len(),
            shared_archive_records(&clean)
        ));

        // Through a row: `open_jar_archive` over the files.
        let dir = std::env::temp_dir().join(format!("cratonvm_i79_l5_dup_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let dup_path = dir.join("dup.jar");
        let clean_path = dir.join("clean.jar");
        fs::write(&dup_path, with_duplicate()).unwrap();
        fs::write(
            &clean_path,
            zip_of(&[("p/A.class", &b"first"[..]), ("p/B.class", &b"other"[..])]),
        )
        .unwrap();
        assert!(!ClassPath::open_jar_archive(&dup_path, false)
            .expect("dup row")
            .plain_view());
        assert!(ClassPath::open_jar_archive(&clean_path, false)
            .expect("clean row")
            .plain_view());
        let _ = fs::remove_dir_all(&dir);
    }

    /// The walk stops at the first non-header and counts at most `limit + 1`.
    #[test]
    fn i79_the_record_walk_is_bounded() {
        let bytes = zip_of(&[("a", &b"1"[..]), ("b", &b"2"[..]), ("c", &b"3"[..])]);
        let archive = ZipArchive::new(Cursor::new(ArchiveBacking::from_vec(bytes))).unwrap();
        let start = archive.central_directory_start();
        let mut reader = archive.clone().into_inner();
        assert_eq!(count_central_records(&mut reader, start, 10), Some(3));
        assert_eq!(
            count_central_records(&mut reader, start, 1),
            Some(2),
            "limit + 1"
        );
        assert_eq!(
            count_central_records(&mut reader, 0, 10),
            Some(0),
            "a local header is not one"
        );
    }
}

/// zr z8/fm: `CRATONVM_JMOD_LAZY_INDEX` -- the central-directory JMOD index
/// answers every lookup exactly as the `ZipArchive` arm does.
#[cfg(test)]
mod zr8_fm_jmod_lazy_tests {
    use super::*;
    use std::io::Write;

    fn jmod_bytes(build: impl FnOnce(&mut zip::ZipWriter<Cursor<Vec<u8>>>)) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        build(&mut zip);
        let zip_bytes = zip.finish().unwrap().into_inner();
        let mut out = vec![0x4A, 0x4D, 0x01, 0x00];
        out.extend_from_slice(&zip_bytes);
        out
    }

    fn sample() -> Vec<u8> {
        let deflated = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        let stored = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        let class: Vec<u8> = (0..4000u32).map(|i| (i * 7 % 251) as u8).collect();
        jmod_bytes(|z| {
            z.start_file("classes/p/A.class", deflated).unwrap();
            z.write_all(&class).unwrap();
            z.start_file("classes/p/res.txt", stored).unwrap();
            z.write_all(b"stored resource bytes").unwrap();
            z.add_directory("classes/p/dir/", stored).unwrap();
            z.start_file("classes/module-info.class", deflated).unwrap();
            z.write_all(b"\xCA\xFE\xBA\xBEmodule").unwrap();
            z.start_file("lib/libx.so", deflated).unwrap();
            z.write_all(&[0u8; 9000]).unwrap();
            z.start_file("conf/x.properties", stored).unwrap();
            z.write_all(b"k=v\n").unwrap();
        })
    }

    fn write_tmp(tag: &str, bytes: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cratonvm_zr8fm_{}_{tag}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join(format!("{tag}.jmod"));
        fs::write(&path, bytes).unwrap();
        path
    }

    struct Arm {
        names: Vec<String>,
        classes: FxHashSet<String>,
        archive: JmodArchive,
        backing: ArchiveBacking,
    }

    fn open(path: &Path, lazy: bool) -> Arm {
        match ClassPath::load_jmod_with(path, lazy).expect("opens") {
            ClassPathEntry::JmodFile {
                all_entry_names,
                class_entry_index,
                archive,
                backing,
                ..
            } => Arm {
                names: all_entry_names.iter().map(str::to_string).collect(),
                classes: class_entry_index.iter().map(str::to_string).collect(),
                archive,
                backing,
            },
            other => panic!("expected JmodFile, got {other:?}"),
        }
    }

    /// Every answer the class path asks a JMOD for, for `name`.
    fn answers(arm: &Arm, name: &str) -> (Option<Vec<u8>>, Option<Vec<u8>>, bool) {
        (
            ClassPath::jmod_find_shared(&arm.archive, &arm.backing, name).map(|b| b.to_vec()),
            ClassPath::jmod_find_vec(&arm.archive, &arm.backing, name),
            ClassPath::jmod_has_entry(&arm.archive, name),
        )
    }

    fn assert_arms_agree(path: &Path, extra_names: &[&str]) -> usize {
        let lazy = open(path, true);
        let zip = open(path, false);
        assert!(
            matches!(lazy.archive, JmodArchive::Central(_)),
            "{}: the central-directory index must take this JMOD",
            path.display()
        );
        assert!(matches!(zip.archive, JmodArchive::Zip(_)));
        assert_eq!(lazy.names, zip.names, "{}: entry names", path.display());
        assert_eq!(lazy.classes, zip.classes, "{}: class index", path.display());
        let mut checked = 0;
        let all = lazy.names.iter().map(String::as_str);
        for name in all.chain(extra_names.iter().copied()) {
            let (l, z) = (answers(&lazy, name), answers(&zip, name));
            assert_eq!(l, z, "{}: {name}", path.display());
            checked += 1;
        }
        checked
    }

    #[test]
    fn zr8_fm_both_arms_serve_the_same_entries() {
        let path = write_tmp("same", &sample());
        let n = assert_arms_agree(
            &path,
            &["classes/p/B.class", "classes/p/dir", "", "classes/", "lib/"],
        );
        assert!(n >= 11);
        // Positive control: the entries do resolve (a vacuous `None == None`
        // agreement would also pass the loop above).
        let lazy = open(&path, true);
        let (shared, vec, has) = answers(&lazy, "classes/p/res.txt");
        assert_eq!(shared.as_deref(), Some(&b"stored resource bytes"[..]));
        assert_eq!(vec.as_deref(), Some(&b"stored resource bytes"[..]));
        assert!(has);
        let class = answers(&lazy, "classes/p/A.class").0;
        assert_eq!(class.map(|b| b.len()), Some(4000));
        let _ = fs::remove_file(&path);
    }

    /// Damaged payloads: a deflated entry's stream and a stored entry's bytes
    /// (whose CRC then fails). Both arms refuse the same reads -- the stored
    /// range view has never checked the CRC, the owned read always has.
    #[test]
    fn zr8_fm_both_arms_refuse_the_same_damaged_entries() {
        let mut bytes = sample();
        let find = |bytes: &[u8], needle: &[u8]| {
            let mut windows = bytes.windows(needle.len());
            windows.position(|w| w == needle).unwrap()
        };
        // The stored resource's payload follows its local header's name.
        let at = find(&bytes, b"stored resource bytes");
        bytes[at] ^= 0x20;
        // The deflated class's payload: just past its local header.
        let name = b"classes/p/A.class";
        let header = find(&bytes, name) - 30;
        let data = header + 30 + name.len() + 4;
        for b in &mut bytes[data..data + 40] {
            *b ^= 0x5A;
        }
        let path = write_tmp("damaged", &bytes);
        assert_arms_agree(&path, &[]);
        let lazy = open(&path, true);
        let (shared, vec, _) = answers(&lazy, "classes/p/res.txt");
        assert_eq!(shared.as_deref(), Some(&b"Stored resource bytes"[..]));
        assert_eq!(vec, None, "the CRC check refuses the owned read");
        assert_eq!(answers(&lazy, "classes/p/A.class").1, None);
        let _ = fs::remove_file(&path);
    }

    /// The real JDK's JMODs, every entry of every file, both arms. Ignored by
    /// default like the other JDK-dependent JMOD tests; run with
    /// `--ignored zr8_fm` on a JDK that ships `jmods/`.
    #[test]
    #[ignore]
    fn zr8_fm_every_real_jdk_jmod_serves_the_same_bytes_both_ways() {
        let Ok(home) = cratonvm_types::flags::runtime_var("JAVA_HOME") else {
            eprintln!("JAVA_HOME unset: skipped");
            return;
        };
        let dir = PathBuf::from(home).join("jmods");
        let mut files: Vec<PathBuf> = fs::read_dir(&dir)
            .expect("JAVA_HOME/jmods")
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "jmod"))
            .collect();
        files.sort();
        assert!(!files.is_empty());
        let mut entries = 0;
        for f in &files {
            entries += assert_arms_agree(f, &["classes/no/such/Thing.class"]);
        }
        eprintln!("zr8fm: {} jmods, {entries} entries agree", files.len());
    }

    /// This process's resident set in KiB: `VmRSS` on Linux, the working set
    /// on Windows, `None` elsewhere.
    fn resident_kib() -> Option<u64> {
        #[cfg(target_os = "linux")]
        {
            let status = fs::read_to_string("/proc/self/status").ok()?;
            let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
            return line.split_whitespace().nth(1)?.parse().ok();
        }
        #[cfg(windows)]
        {
            #[repr(C)]
            #[derive(Default)]
            struct Counters {
                cb: u32,
                page_fault_count: u32,
                peak_working_set_size: usize,
                working_set_size: usize,
                quota_peak_paged_pool_usage: usize,
                quota_paged_pool_usage: usize,
                quota_peak_non_paged_pool_usage: usize,
                quota_non_paged_pool_usage: usize,
                pagefile_usage: usize,
                peak_pagefile_usage: usize,
            }
            extern "system" {
                fn GetCurrentProcess() -> isize;
                fn K32GetProcessMemoryInfo(p: isize, counters: *mut Counters, cb: u32) -> i32;
            }
            let mut c = Counters {
                cb: std::mem::size_of::<Counters>() as u32,
                ..Default::default()
            };
            // SAFETY: a correctly sized PROCESS_MEMORY_COUNTERS for the
            // current-process pseudo handle.
            let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
            return (ok != 0).then_some(c.working_set_size as u64 / 1024);
        }
        #[allow(unreachable_code)]
        None
    }

    /// The resident cost of OPENING the real JDK's 70 JMODs (and serving
    /// `java/lang/Object.class`), each arm measured over its own baseline:
    /// the row that says what `CRATONVM_JMOD_LAZY_INDEX` buys. Ignored by
    /// default; `--ignored zr8_fm_jmod_open_resident --nocapture`.
    #[test]
    #[ignore]
    fn zr8_fm_jmod_open_resident_cost_per_arm() {
        let Ok(home) = cratonvm_types::flags::runtime_var("JAVA_HOME") else {
            eprintln!("JAVA_HOME unset: skipped");
            return;
        };
        let mut files: Vec<PathBuf> = fs::read_dir(PathBuf::from(home).join("jmods"))
            .expect("JAVA_HOME/jmods")
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "jmod"))
            .collect();
        files.sort();
        for lazy in [true, false] {
            let before = resident_kib();
            let arms: Vec<Arm> = files.iter().map(|f| open(f, lazy)).collect();
            let base = arms
                .iter()
                .zip(&files)
                .find(|(_, f)| f.file_name().is_some_and(|n| n == "java.base.jmod"))
                .map(|(a, _)| a)
                .expect("java.base.jmod");
            let object = ClassPath::jmod_find_shared(
                &base.archive,
                &base.backing,
                "classes/java/lang/Object.class",
            );
            assert!(object.is_some(), "positive control: Object.class serves");
            let after = resident_kib();
            let delta = match (before, after) {
                (Some(b), Some(a)) => a as i64 - b as i64,
                _ => -1,
            };
            eprintln!(
                "zr8fm-jmod-open lazy={} jmods={} resident_before_kib={} resident_after_kib={} delta_kib={delta}",
                u8::from(lazy),
                arms.len(),
                before.map_or(-1, |v| v as i64),
                after.map_or(-1, |v| v as i64),
            );
            drop(arms);
        }
    }
}

/// zs s4/rp: `CRATONVM_BOOT_IMAGE_CLASS_BYTES` -- a boot class path of JDK
/// `jmods/` serves the classes of the runtime image beside it.
#[cfg(test)]
mod zs4_rp_runtime_image_tests {
    use super::*;
    use std::io::Write;

    /// A JDK home with `jmods/java.base.jmod` and `lib/modules` whose
    /// `java.base` differs the way jlink's plugins make it differ: a class
    /// regenerated (`p/Holder`), a class only the image has (`p/Species_X`),
    /// a class both have alike (`p/Same`), and a different `module-info`.
    fn fake_jdk(tag: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!(
            "cratonvm_zs4rp_{}_{}_{tag}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(home.join("jmods")).unwrap();
        fs::create_dir_all(home.join("lib")).unwrap();
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in [
            (
                "classes/p/Holder.class",
                b"\xCA\xFE\xBA\xBE_jmod_holder".as_ref(),
            ),
            ("classes/p/Same.class", b"\xCA\xFE\xBA\xBE_same".as_ref()),
            (
                "classes/q/OnlyJmod.class",
                b"\xCA\xFE\xBA\xBE_only_jmod".as_ref(),
            ),
            (
                "classes/module-info.class",
                b"\xCA\xFE\xBA\xBE_jmod_module_info".as_ref(),
            ),
        ] {
            zip.start_file(name, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        let mut jmod = vec![0x4A, 0x4D, 0x01, 0x00];
        jmod.extend_from_slice(&zip.finish().unwrap().into_inner());
        fs::write(home.join("jmods").join("java.base.jmod"), jmod).unwrap();
        let image = cratonvm_reader::jimage::test_builder::build_simple(&[
            (
                "java.base",
                "p",
                "Holder",
                "class",
                b"\xCA\xFE\xBA\xBE_image_holder".as_ref(),
            ),
            (
                "java.base",
                "p",
                "Species_X",
                "class",
                b"\xCA\xFE\xBA\xBE_image_species".as_ref(),
            ),
            (
                "java.base",
                "p",
                "Same",
                "class",
                b"\xCA\xFE\xBA\xBE_same".as_ref(),
            ),
            (
                "java.base",
                "",
                "module-info",
                "class",
                b"\xCA\xFE\xBA\xBE_image_module_info".as_ref(),
            ),
        ]);
        fs::write(home.join("lib").join("modules"), image).unwrap();
        home
    }

    fn boot_path(home: &Path, on: bool) -> ClassPath {
        let jmod = home.join("jmods").join("java.base.jmod");
        let mut cp = ClassPath::new(&[jmod.to_string_lossy().into_owned()]);
        cp.set_runtime_image_classes(on);
        cp
    }

    fn bytes(cp: &ClassPath, name: &str) -> Option<Vec<u8>> {
        cp.find_class(name).ok().map(|b| b.as_ref().to_vec())
    }

    #[test]
    fn zs4_rp_jmod_boot_path_serves_the_runtime_images_classes() {
        let home = fake_jdk("on");
        let cp = boot_path(&home, true);
        // The image's regenerated class wins over the JMOD's.
        assert_eq!(
            bytes(&cp, "p/Holder").unwrap(),
            b"\xCA\xFE\xBA\xBE_image_holder"
        );
        // A class only the image has resolves (its package's module).
        assert_eq!(
            bytes(&cp, "p/Species_X").unwrap(),
            b"\xCA\xFE\xBA\xBE_image_species"
        );
        assert_eq!(bytes(&cp, "p/Same").unwrap(), b"\xCA\xFE\xBA\xBE_same");
        // A class only the JMOD has still resolves from the JMOD.
        assert_eq!(
            bytes(&cp, "q/OnlyJmod").unwrap(),
            b"\xCA\xFE\xBA\xBE_only_jmod"
        );
        // Nothing invents a class neither has, in a known package or not.
        assert!(matches!(
            cp.find_class("p/Missing"),
            Err(ClassFileError::ClassNotFound { .. })
        ));
        assert!(cp.find_class("r/Missing").is_err());
        // Module descriptors stay the JMOD's.
        assert_eq!(
            bytes(&cp, "module-info").unwrap(),
            b"\xCA\xFE\xBA\xBE_jmod_module_info"
        );
        assert_eq!(
            cp.scan_module_infos(),
            vec![b"\xCA\xFE\xBA\xBE_jmod_module_info".to_vec()]
        );
        let _ = fs::remove_dir_all(&home);
    }

    /// The kill switch (`=0`) is the JMOD-only class path it replaced: the
    /// positive control for the rows above.
    #[test]
    fn zs4_rp_kill_switch_serves_the_jmods_only() {
        let home = fake_jdk("off");
        let cp = boot_path(&home, false);
        assert_eq!(
            bytes(&cp, "p/Holder").unwrap(),
            b"\xCA\xFE\xBA\xBE_jmod_holder"
        );
        assert!(matches!(
            cp.find_class("p/Species_X"),
            Err(ClassFileError::ClassNotFound { .. })
        ));
        let _ = fs::remove_dir_all(&home);
    }

    /// A JMOD with no runtime image beside it is served as before with the
    /// switch on.
    #[test]
    fn zs4_rp_a_jmod_without_an_image_is_unchanged() {
        let home = fake_jdk("noimage");
        fs::remove_file(home.join("lib").join("modules")).unwrap();
        let cp = boot_path(&home, true);
        assert_eq!(
            bytes(&cp, "p/Holder").unwrap(),
            b"\xCA\xFE\xBA\xBE_jmod_holder"
        );
        assert!(cp.find_class("p/Species_X").is_err());
        let _ = fs::remove_dir_all(&home);
    }

    /// The host JDK's home, when it has both `jmods/` and `lib/modules`.
    fn host_jdk_with_jmods_and_image() -> Option<PathBuf> {
        let output = std::process::Command::new("java")
            .args(["-XshowSettings:properties", "-version"])
            .output()
            .ok()?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        let home = stderr.lines().find_map(|line| {
            let value = line.trim().strip_prefix("java.home")?.trim();
            Some(PathBuf::from(value.strip_prefix('=')?.trim()))
        })?;
        (home.join("jmods").join("java.base.jmod").is_file()
            && home.join("lib").join("modules").is_file())
        .then_some(home)
    }

    /// The real JDK: the jlink-generated `java.base` classes answer the
    /// image's bytes (what HotSpot runs), the others are unchanged. Skips
    /// (with a line on stderr) on a host JDK without `jmods/`.
    #[test]
    fn zs4_rp_real_jdk_jlink_generated_classes_come_from_the_image() {
        let Some(home) = host_jdk_with_jmods_and_image() else {
            eprintln!("zs4_rp: SKIP, no host JDK with jmods/ and lib/modules");
            return;
        };
        let on = boot_path(&home, true);
        let off = boot_path(&home, false);
        let species = "java/lang/invoke/BoundMethodHandle$Species_LL";
        assert!(
            on.find_class(species).is_ok(),
            "{species} with the switch on"
        );
        assert!(
            off.find_class(species).is_err(),
            "{species} with the switch off"
        );
        let holder = "java/lang/invoke/DirectMethodHandle$Holder";
        let image = cratonvm_reader::JImageReader::open(home.join("lib").join("modules"))
            .unwrap()
            .find_class("java.base", holder)
            .unwrap()
            .unwrap();
        assert_eq!(bytes(&on, holder).unwrap(), image);
        assert_ne!(bytes(&off, holder).unwrap(), image);
        assert_eq!(
            bytes(&on, "java/lang/Object"),
            bytes(&off, "java/lang/Object")
        );
        // s4 follow-up: `Invokers` and its regenerated holder, the outer whose
        // `getDeclaredClasses` the host row reads.
        let reader = cratonvm_reader::JImageReader::open(home.join("lib").join("modules")).unwrap();
        for name in [
            "java/lang/invoke/Invokers",
            "java/lang/invoke/Invokers$Holder",
        ] {
            let image = reader.find_class("java.base", name).unwrap().unwrap();
            assert_eq!(
                bytes(&on, name).unwrap(),
                image,
                "{name} with the switch on"
            );
        }
    }

    /// The launcher's two `java.home` shapes (`discover_boot_classpath`): a
    /// home WITH `jmods/` boots from the JMODs (java.base first, then the
    /// others), and the image serves their classes under the switch; a home
    /// WITHOUT `jmods/` (a JDK linked without them, JEP 493, or a JRE) boots
    /// from `lib/modules` itself, so it has the image's bytes whatever the
    /// switch says -- the switch has nothing to do there.
    #[test]
    fn zs4_rp_both_launcher_java_home_shapes_serve_the_image_holder() {
        let home = fake_jdk("shapes");
        let jmods = home.join("jmods").join("java.base.jmod");
        let modules = home.join("lib").join("modules");
        for on in [true, false] {
            let mut cp = ClassPath::new(&[jmods.to_string_lossy().into_owned()]);
            cp.set_runtime_image_classes(on);
            let want: &[u8] = if on {
                b"\xCA\xFE\xBA\xBE_image_holder"
            } else {
                b"\xCA\xFE\xBA\xBE_jmod_holder"
            };
            assert_eq!(bytes(&cp, "p/Holder").unwrap(), want, "jmods home, on={on}");
            let mut cp = ClassPath::new(&[modules.to_string_lossy().into_owned()]);
            cp.set_runtime_image_classes(on);
            assert_eq!(
                bytes(&cp, "p/Holder").unwrap(),
                b"\xCA\xFE\xBA\xBE_image_holder",
                "image-only home, on={on}"
            );
            assert!(cp.find_class("p/Species_X").is_ok());
        }
        let _ = fs::remove_dir_all(&home);
    }
}

/// zt/fp: `CRATONVM_JAR_LAZY_INDEX` -- a plain class-path jar opened from its
/// central directory answers what the `ZipArchive` open answered.
#[cfg(test)]
mod fp_jar_lazy_index_tests {
    use super::*;
    use std::io::Write;

    fn write_jar(path: &Path, entries: &[(&str, &[u8], bool)]) {
        let mut z = zip::ZipWriter::new(fs::File::create(path).unwrap());
        for (name, data, deflate) in entries {
            let method = if *deflate {
                zip::CompressionMethod::Deflated
            } else {
                zip::CompressionMethod::Stored
            };
            let opts = zip::write::SimpleFileOptions::default().compression_method(method);
            z.start_file(*name, opts).unwrap();
            z.write_all(data).unwrap();
        }
        z.finish().unwrap();
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cratonvm_fpjar_{}_{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The entries a jar opens as, with the index on or off.
    fn open(path: &Path, lazy: bool) -> Vec<ClassPathEntry> {
        let (data, stamp) = read_archive_for_classpath_stamped(path).unwrap();
        let mut entries = Vec::new();
        ClassPath::load_jar_data_at_depth_with(
            path,
            data,
            stamp,
            &mut entries,
            99,
            true,
            false,
            lazy,
        );
        entries
    }

    fn the_jar(
        entries: &[ClassPathEntry],
    ) -> (&JarArchive, &ArchiveBacking, &FxHashSet<String>, bool) {
        match entries
            .iter()
            .find(|e| matches!(e, ClassPathEntry::JarFile { .. }))
        {
            Some(ClassPathEntry::JarFile {
                archive,
                backing,
                entry_index,
                multi_release,
                ..
            }) => (archive, backing, entry_index, *multi_release),
            _ => panic!("no JarFile entry in {entries:?}"),
        }
    }

    fn big(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 % 251) as u8).collect()
    }

    #[test]
    fn fp_both_arms_serve_the_same_jar() {
        let dir = temp_dir("same");
        let jar = dir.join("plain.jar");
        let manifest = b"Manifest-Version: 1.0\r\nMulti-Release: true\r\nMain-Class: p.Main\r\n\r\n";
        let compressible = vec![b'a'; 4096];
        let noisy = big(3000);
        write_jar(
            &jar,
            &[
                ("META-INF/MANIFEST.MF", &manifest[..], true),
                ("p/A.class", &compressible, true),
                ("p/B.class", &noisy, false),
                ("p/res.txt", b"hello", false),
                ("META-INF/versions/11/p/A.class", &noisy, true),
                ("META-INF/versions/17/p/A.class", &compressible, false),
                ("META-INF/versions/9/q/Only.class", b"nine", true),
                ("module-info.class", b"mi", false),
                ("dir/", b"", false),
            ],
        );
        let lazy = open(&jar, true);
        let zip = open(&jar, false);
        let (la, lb, lidx, lmr) = the_jar(&lazy);
        let (za, zb, zidx, zmr) = the_jar(&zip);
        assert!(la.central.is_some(), "the index must take this jar");
        assert!(za.central.is_none());
        assert!(
            !la.zip_is_open(),
            "opening the lazy jar must not open the ZipArchive"
        );
        assert_eq!(lidx, zidx, "entry names");
        assert_eq!(lmr, zmr);
        assert!(lmr);
        let mut names: Vec<String> = Vec::new();
        la.each_name(&mut |n| names.push(n.to_string()));
        let mut znames: Vec<String> = Vec::new();
        za.each_name(&mut |n| znames.push(n.to_string()));
        assert_eq!(names, znames, "entry order");
        for name in names
            .iter()
            .map(String::as_str)
            .chain(["p/Missing.class", "", "p/"])
        {
            assert_eq!(la.has_entry(name), za.has_entry(name), "has {name:?}");
            assert_eq!(la.read_vec(name), za.read_vec(name), "vec {name:?}");
            assert_eq!(
                la.read_shared(lb, name).map(|b| b.to_vec()),
                za.read_shared(zb, name).map(|b| b.to_vec()),
                "shared {name:?}"
            );
        }
        assert!(
            !la.zip_is_open(),
            "serving entries must not open the ZipArchive"
        );
        let (lv, zv) = (Mutex::new(None), Mutex::new(None));
        assert_eq!(
            *ClassPath::ensure_versions_cache(la, &lv),
            *ClassPath::ensure_versions_cache(za, &zv)
        );
        for want in ["p/A.class", "p/B.class", "q/Only.class", "p/res.txt"] {
            for max in [8, 9, 11, 17, 25] {
                assert_eq!(
                    ClassPath::find_in_multi_release_archive(la, &lv, Some(lidx), want, max),
                    ClassPath::find_in_multi_release_archive(za, &zv, Some(zidx), want, max),
                    "mr {want} at {max}"
                );
            }
        }
        let (mut lc, mut zc) = (Vec::new(), Vec::new());
        ClassPath::list_archive_classes(la, &mut lc);
        ClassPath::list_archive_classes(za, &mut zc);
        assert_eq!(lc, zc);
        assert!(la.lacks_signer_blocks());
        assert_eq!(la.read_manifest().main_class, za.read_manifest().main_class);
        assert!(!la.zip_is_open());
        // Anything outside the by-name readers opens the ZipArchive on demand
        // and finds the same jar.
        assert_eq!(la.lock().len(), za.lock().len());
        assert!(la.zip_is_open());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fp_a_signed_jar_still_verifies_through_the_zip_archive() {
        let dir = temp_dir("signed");
        let jar = dir.join("signed.jar");
        write_jar(
            &jar,
            &[
                (
                    "META-INF/MANIFEST.MF",
                    b"Manifest-Version: 1.0\r\n\r\n",
                    false,
                ),
                ("META-INF/FOO.SF", b"Signature-Version: 1.0\r\n\r\n", false),
                ("META-INF/FOO.RSA", b"not a real block", false),
                ("p/A.class", b"a", false),
            ],
        );
        let lazy = open(&jar, true);
        let (la, _, _, _) = the_jar(&lazy);
        assert!(la.central.is_some());
        assert!(
            !la.lacks_signer_blocks(),
            "a signer block is there to verify"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fp_a_fat_jar_and_a_jar_the_index_refuses_take_the_old_open() {
        let dir = temp_dir("fallback");
        let fat = dir.join("fat.jar");
        write_jar(
            &fat,
            &[
                (
                    "META-INF/MANIFEST.MF",
                    b"Manifest-Version: 1.0\r\n\r\n",
                    false,
                ),
                ("BOOT-INF/classes/p/A.class", b"a", false),
                ("BOOT-INF/lib/", b"", false),
            ],
        );
        let (lazy, zip) = (open(&fat, true), open(&fat, false));
        assert_eq!(format!("{lazy:?}"), format!("{zip:?}"), "fat jar entries");
        assert!(lazy
            .iter()
            .any(|e| matches!(e, ClassPathEntry::NestedDirectory { .. })));
        for e in &lazy {
            if let ClassPathEntry::JarFile { archive, .. } = e {
                assert!(
                    archive.central.is_none(),
                    "a fat jar's root is the old open's"
                );
            }
        }
        // A non-ASCII entry name is not modelled: the jar opens the old way.
        let odd = dir.join("odd.jar");
        write_jar(
            &odd,
            &[("p/caf\u{e9}.class", b"a", false), ("p/A.class", b"b", true)],
        );
        let entries = open(&odd, true);
        let (archive, _, idx, _) = the_jar(&entries);
        assert!(archive.central.is_none());
        assert!(idx.contains("p/A.class"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fp_a_class_path_loads_the_jar_without_opening_the_zip_archive() {
        let dir = temp_dir("classpath");
        let jar = dir.join("lib.jar");
        let dep = dir.join("dep.jar");
        write_jar(
            &jar,
            &[
                (
                    "META-INF/MANIFEST.MF",
                    b"Manifest-Version: 1.0\r\nClass-Path: dep.jar\r\n\r\n",
                    false,
                ),
                ("p/A.class", b"\xCA\xFE\xBA\xBEaaaa", true),
            ],
        );
        write_jar(&dep, &[("q/B.class", b"\xCA\xFE\xBA\xBEbbbb", false)]);
        let cp = ClassPath::new(&[jar.to_string_lossy().into_owned()]);
        assert_eq!(
            cp.find_class("p/A").unwrap().as_ref(),
            b"\xCA\xFE\xBA\xBEaaaa"
        );
        assert_eq!(
            cp.find_class("q/B").unwrap().as_ref(),
            b"\xCA\xFE\xBA\xBEbbbb"
        );
        assert!(cp.find_class("p/Missing").is_err());
        let mut jars = 0;
        for entry in &cp.entries {
            if let ClassPathEntry::JarFile { archive, path, .. } = entry {
                jars += 1;
                assert!(archive.central.is_some(), "{}", path.display());
                assert!(!archive.zip_is_open(), "{}", path.display());
            }
        }
        assert_eq!(jars, 2, "the jar and its Class-Path jar");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The reads the JDK's own `URLClassPath` makes over the VM's class path
    /// (jar-loader bytes, code source, resources, the define gate) are
    /// answered from the index too.
    #[test]
    fn fp_the_jar_loader_rows_code_source_and_resources_stay_off_the_zip_archive() {
        let dir = temp_dir("rows");
        let jar = dir.join("rows.jar");
        let data = big(5000);
        write_jar(
            &jar,
            &[
                ("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0\r\n\r\n", true),
                ("p/A.class", b"\xCA\xFE\xBA\xBEaaaa", true),
                ("p/res.txt", &data, true),
                ("p/stored.bin", &data, false),
            ],
        );
        let cp = ClassPath::new(&[jar.to_string_lossy().into_owned()]);
        let zip_open = || {
            cp.entries.iter().any(|e| match e {
                ClassPathEntry::JarFile { archive, .. } => archive.zip_is_open(),
                _ => false,
            })
        };
        let _ = cp.fast_define_ok("p/A");
        assert!(!zip_open(), "the define gate");
        let (url, certs) = cp.find_class_code_source_info("p/A").expect("code source");
        assert!(url.contains("rows.jar") && certs.is_empty());
        assert!(!zip_open(), "code source");
        assert_eq!(cp.find_resource("p/res.txt").as_deref(), Some(&data[..]));
        assert_eq!(cp.find_all_resource_bytes("p/res.txt"), vec![data.clone()]);
        assert_eq!(cp.find_all_resource_urls("p/res.txt").len(), 1);
        assert!(cp.find_resource("p/none.txt").is_none());
        assert!(!zip_open(), "resources");
        for name in ["p/res.txt", "p/stored.bin"] {
            let answer = cp.jar_entry_bytes_matching(
                &jar,
                name,
                crc32fast::hash(&data),
                data.len() as u64,
            );
            match answer {
                cratonvm_native_api::ClassPathJarBytes::Bytes(bytes) => assert_eq!(bytes, data),
                other => panic!("{name}: expected the bytes, got {other:?}"),
            }
        }
        // A wrong CRC-32 or size is the Java body's business, as before.
        assert!(matches!(
            cp.jar_entry_bytes_matching(&jar, "p/res.txt", 1, data.len() as u64),
            cratonvm_native_api::ClassPathJarBytes::Body
        ));
        assert!(matches!(
            cp.jar_entry_bytes_matching(&jar, "p/res.txt", crc32fast::hash(&data), 7),
            cratonvm_native_api::ClassPathJarBytes::Body
        ));
        assert!(!zip_open(), "the jar-loader rows");
        let _ = fs::remove_dir_all(&dir);
    }
}


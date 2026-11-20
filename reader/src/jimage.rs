// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! NEW-5 — jimage v1 reader for `$JAVA_HOME/lib/modules`.
//!
//! # Format overview
//!
//! A jimage file is produced by `jlink` when the JDK runtime image is
//! assembled. It is the single file under `$JAVA_HOME/lib/modules` that
//! holds *every* class and resource in the boot layer (`java.base`,
//! `java.desktop`, `java.logging`, …). Booting any real JDK requires
//! reading it.
//!
//! The on-disk layout (all multi-byte integers in the endianness
//! detected from the magic number):
//!
//! ```text
//! +---------------------------+
//! | Header (28 bytes)         |
//! |   u4 magic = 0xCAFEDADA   |
//! |   u4 version              | // (major << 16) | minor — i.e. version 1.0
//! |                           | // is `0x0001_0000` (HIGH=major, LOW=minor).
//! |   u4 flags                |
//! |   u4 resource_count       |
//! |   u4 table_length         |
//! |   u4 locations_size       |
//! |   u4 strings_size         |
//! +---------------------------+
//! | Redirect table            | table_length * i32
//! +---------------------------+
//! | Offset table              | table_length * u32
//! +---------------------------+
//! | Locations buffer          | locations_size bytes
//! +---------------------------+
//! | Strings buffer            | strings_size bytes
//! +---------------------------+
//! | Resource data             | file_size - preceding
//! +---------------------------+
//! ```
//!
//! ## Location records
//!
//! Each entry in the locations buffer is a variable-length record that
//! decodes to a struct `{ module, parent, base, extension, offset,
//! compressed_size, uncompressed_size }`. Every field is optional except
//! END. The encoding is a sequence of `(header_byte, value_bytes)` pairs:
//!
//! ```text
//! header_byte = (kind << 3) | (value_length - 1)
//! kind: 0..7, value_length: 1..8
//! ```
//!
//! Kinds:
//! - `0` END — no value; decoder stops
//! - `1` MODULE — string-table offset
//! - `2` PARENT — string-table offset (e.g. `java/lang`)
//! - `3` BASE — string-table offset (e.g. `String`)
//! - `4` EXTENSION — string-table offset (e.g. `class`)
//! - `5` OFFSET — resource-data offset
//! - `6` COMPRESSED — compressed size (0 = stored uncompressed)
//! - `7` UNCOMPRESSED — uncompressed size
//!
//! Values are big-endian unsigned integers regardless of the file's
//! byte order. This is a jimage format quirk — the header uses native
//! endianness, the location attribute values use network byte order.
//!
//! ## Perfect-hash lookup
//!
//! Resources are indexed with a CHD-style two-level displacement hash.
//! The first lookup hashes the full path `/module/parent/base.ext` into
//! the redirect table with a fixed seed `HASH_MULTIPLIER`. The bucket
//! value is then interpreted:
//!
//! - `> 0`: a new hash seed; re-hash the path and index the offset table
//! - `< 0`: a direct index `-value - 1` into the offset table
//! - `== 0`: not present
//!
//! The hash function is a multiply-then-xor variant of FNV-1a:
//!
//! ```text
//! fn hash(path, seed):
//!     h = seed
//!     for byte in path.bytes():
//!         h = (h * 0x01000193) ^ byte
//!     return h & 0x7fffffff
//! ```
//!
//! After indexing, the resolved offset points into the locations buffer.
//! The location record is decoded and the reconstructed path MUST equal
//! the query exactly; otherwise the lookup is a hash collision and the
//! result is `None`.
//!
//! ## Endianness
//!
//! Production JDKs ship jimage files in the endianness of the build
//! host: little-endian on x86/x64/ARM, big-endian on SPARC/PowerPC.
//! The reader detects this from the magic number (which reads as
//! `0xCAFEDADA` in LE or as the byte-swapped equivalent in BE) and
//! routes all header reads through an endian-aware helper.
//!
//! ## Safety & error handling
//!
//! All reads are bounds-checked and return typed `JImageError` values
//! rather than panicking. Malformed headers, out-of-range offsets, and
//! invalid attribute kinds are reported as errors so the caller can
//! surface a `NoClassDefFoundError` to Java code. The reader is
//! read-only and performs no writes; the backing buffer is an owned
//! `Vec<u8>` so the API is lifetime-free for callers.
//!
//! ## Unsupported
//!
//! - **Compressed resources**: jlink can emit resources compressed with
//!   Zstd (attribute `COMPRESSED > 0`). Production JDKs ship uncompressed
//!   by default (`--compress=0`). A compressed entry produces
//!   `JImageError::Compressed` on read, with the message pointing at the
//!   specific path so the caller can diagnose it.

use std::fs::File;
use std::io::Read;
use std::path::Path;

// ---------------------------------------------------------------------------
// Constants (jimage format)
// ---------------------------------------------------------------------------

/// Magic number in native byte order of the file.
const JIMAGE_MAGIC: u32 = 0xCAFE_DADA;

/// Size of the fixed header in bytes.
const HEADER_SIZE: usize = 28;

/// Initial seed for the perfect-hash function.
const HASH_MULTIPLIER: i32 = 0x0100_0193;

/// Attribute kinds encoded in location records.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttributeKind {
    End = 0,
    Module = 1,
    Parent = 2,
    Base = 3,
    Extension = 4,
    Offset = 5,
    Compressed = 6,
    Uncompressed = 7,
}

impl AttributeKind {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::End),
            1 => Some(Self::Module),
            2 => Some(Self::Parent),
            3 => Some(Self::Base),
            4 => Some(Self::Extension),
            5 => Some(Self::Offset),
            6 => Some(Self::Compressed),
            7 => Some(Self::Uncompressed),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors raised by the jimage reader. Every failure path returns one of
/// these rather than panicking, so callers can surface a well-formed
/// Java exception to user code.
#[derive(Debug)]
pub enum JImageError {
    /// The file could not be opened or read from disk.
    Io(std::io::Error),
    /// The magic number did not match `0xCAFEDADA`. Carries the first
    /// four bytes of the file exactly as read from disk (MSB-first), so
    /// the diagnostic is unambiguous regardless of the file's endianness.
    BadMagic([u8; 4]),
    /// The header declared an unsupported major version.
    UnsupportedVersion { major: u16, minor: u16 },
    /// A section (redirect / offset / locations / strings) extends past
    /// the end of the file.
    TruncatedSection(&'static str),
    /// A location record had a bad attribute kind or length.
    BadLocationRecord(&'static str),
    /// A string table offset pointed outside the strings buffer.
    BadStringOffset(usize),
    /// A resource offset or size extended past the end of the resource
    /// section.
    BadResourceRange { offset: u64, size: u64 },
    /// The resource is stored compressed; decompression is not yet
    /// supported by this reader.
    Compressed(String),
}

impl std::fmt::Display for JImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JImageError::Io(e) => write!(f, "jimage io error: {e}"),
            JImageError::BadMagic(bytes) => {
                // Report both endian interpretations so the diagnostic is
                // not misleading regardless of how the file was written.
                let le = u32::from_le_bytes(*bytes);
                let be = u32::from_be_bytes(*bytes);
                write!(
                    f,
                    "not a jimage file (bad magic bytes [{:02x} {:02x} {:02x} {:02x}], \
                     LE 0x{le:08x} / BE 0x{be:08x})",
                    bytes[0], bytes[1], bytes[2], bytes[3]
                )
            }
            JImageError::UnsupportedVersion { major, minor } => write!(
                f,
                "unsupported jimage version {major}.{minor} (only 1.0 is supported)"
            ),
            JImageError::TruncatedSection(s) => {
                write!(f, "jimage {s} section extends past end of file")
            }
            JImageError::BadLocationRecord(s) => {
                write!(f, "malformed jimage location record: {s}")
            }
            JImageError::BadStringOffset(o) => {
                write!(f, "jimage string offset {o} outside strings buffer")
            }
            JImageError::BadResourceRange { offset, size } => write!(
                f,
                "jimage resource (offset={offset}, size={size}) outside resource section"
            ),
            JImageError::Compressed(path) => write!(
                f,
                "jimage resource {path} is compressed (not yet supported)"
            ),
        }
    }
}

impl std::error::Error for JImageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            JImageError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for JImageError {
    fn from(e: std::io::Error) -> Self {
        JImageError::Io(e)
    }
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Header {
    /// If `true`, every multi-byte integer in the redirect/offset tables
    /// and the header itself is stored little-endian. Location attribute
    /// values are *always* big-endian regardless of this flag — see the
    /// module docs.
    little_endian: bool,
    major_version: u16,
    minor_version: u16,
    #[allow(dead_code)]
    flags: u32,
    #[allow(dead_code)]
    resource_count: u32,
    table_length: u32,
    locations_size: u32,
    strings_size: u32,
}

impl Header {
    fn parse(bytes: &[u8]) -> Result<Self, JImageError> {
        if bytes.len() < HEADER_SIZE {
            return Err(JImageError::TruncatedSection("header"));
        }
        // Detect endianness from the magic number. A little-endian jimage
        // has the magic bytes as `DA DA FE CA` on disk; a big-endian one
        // as `CA FE DA DA`.
        let magic_le = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let magic_be = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let little_endian = if magic_le == JIMAGE_MAGIC {
            true
        } else if magic_be == JIMAGE_MAGIC {
            false
        } else {
            return Err(JImageError::BadMagic([
                bytes[0], bytes[1], bytes[2], bytes[3],
            ]));
        };

        let read_u16 = |off: usize| -> u16 {
            if little_endian {
                u16::from_le_bytes([bytes[off], bytes[off + 1]])
            } else {
                u16::from_be_bytes([bytes[off], bytes[off + 1]])
            }
        };
        let read_u32 = |off: usize| -> u32 {
            if little_endian {
                u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
            } else {
                u32::from_be_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
            }
        };

        // RKC16N.9: the jimage header stores `version` as a single u4 with
        // `major` in the HIGH 16 bits and `minor` in the LOW 16 bits — i.e.
        // version 1.0 is encoded as `(1 << 16) | 0 = 0x0001_0000`. On a
        // little-endian disk that becomes the byte sequence `00 00 01 00`
        // at offsets 4..7 — reading two consecutive u16's would swap major
        // and minor. (See OpenJDK
        // src/java.base/share/native/libjimage/imageFile.hpp `image_version`.)
        // We were misreading every JDK 9+ runtime image as version "0.1"
        // and bouncing it as unsupported, leaving the boot classpath empty
        // and `Void.TYPE` (and every wrapper TYPE field) null.
        let version = read_u32(4);
        let major_version = (version >> 16) as u16;
        let minor_version = (version & 0xFFFF) as u16;
        let flags = read_u32(8);
        let resource_count = read_u32(12);
        let table_length = read_u32(16);
        let locations_size = read_u32(20);
        let strings_size = read_u32(24);

        if major_version != 1 || minor_version != 0 {
            return Err(JImageError::UnsupportedVersion {
                major: major_version,
                minor: minor_version,
            });
        }

        Ok(Header {
            little_endian,
            major_version,
            minor_version,
            flags,
            resource_count,
            table_length,
            locations_size,
            strings_size,
        })
    }
}

// ---------------------------------------------------------------------------
// Location record
// ---------------------------------------------------------------------------

/// Decoded location record for a single resource.
#[derive(Debug, Clone, Default)]
struct Location {
    module: u64,
    parent: u64,
    base: u64,
    extension: u64,
    offset: u64,
    compressed_size: u64,
    uncompressed_size: u64,
}

/// Decode the location record starting at `loc_offset` inside
/// `locations`. Advances through the variable-length attribute stream
/// until an END attribute is hit.
fn decode_location(locations: &[u8], loc_offset: usize) -> Result<Location, JImageError> {
    let mut loc = Location::default();
    let mut i = loc_offset;
    let end = locations.len();

    while i < end {
        let header_byte = locations[i];
        i += 1;
        if header_byte == 0 {
            // END marker: no payload, decoder stops.
            return Ok(loc);
        }
        // The header byte encodes kind in the upper 5 bits and
        // `(length - 1)` in the lower 3 bits.
        let kind_bits = header_byte >> 3;
        let length = ((header_byte & 0x07) as usize) + 1;
        let kind = AttributeKind::from_u8(kind_bits)
            .ok_or(JImageError::BadLocationRecord("invalid attribute kind"))?;
        if kind == AttributeKind::End {
            // END encoded with a nonzero length is malformed.
            return Err(JImageError::BadLocationRecord("END with length"));
        }
        if i + length > end {
            return Err(JImageError::BadLocationRecord("value truncated"));
        }
        // Values are always big-endian unsigned regardless of the file's
        // declared endianness — see module docs.
        let mut value: u64 = 0;
        for &byte in &locations[i..i + length] {
            value = (value << 8) | (byte as u64);
        }
        i += length;
        match kind {
            AttributeKind::Module => loc.module = value,
            AttributeKind::Parent => loc.parent = value,
            AttributeKind::Base => loc.base = value,
            AttributeKind::Extension => loc.extension = value,
            AttributeKind::Offset => loc.offset = value,
            AttributeKind::Compressed => loc.compressed_size = value,
            AttributeKind::Uncompressed => loc.uncompressed_size = value,
            AttributeKind::End => unreachable!("handled above"),
        }
    }
    Err(JImageError::BadLocationRecord("missing END"))
}

/// Read a null-terminated UTF-8 string from the strings buffer starting
/// at `offset`. Returns an empty string for offset 0 (matches the jimage
/// convention for "no value").
fn read_string(strings: &[u8], offset: u64) -> Result<&str, JImageError> {
    let off = offset as usize;
    if off == 0 {
        return Ok("");
    }
    if off >= strings.len() {
        return Err(JImageError::BadStringOffset(off));
    }
    // Find the terminating NUL. Limit the search so a malformed strings
    // buffer without terminators cannot read past the end.
    let remainder = &strings[off..];
    let end = remainder
        .iter()
        .position(|&b| b == 0)
        .ok_or(JImageError::BadStringOffset(off))?;
    std::str::from_utf8(&remainder[..end]).map_err(|_| JImageError::BadStringOffset(off))
}

// ---------------------------------------------------------------------------
// Hash lookup
// ---------------------------------------------------------------------------

/// Jimage-specific string hash. The algorithm is the multiply-xor
/// variant of FNV used by OpenJDK's `ImageStrings::hash_code`. Overflow
/// is *intended* — we accumulate in a wrapping `i32` to match the C
/// implementation's 32-bit `int` behavior exactly, then mask the sign
/// bit before returning.
fn jimage_hash(path: &str, seed: i32) -> i32 {
    let mut h = seed;
    for &b in path.as_bytes() {
        h = h.wrapping_mul(HASH_MULTIPLIER) ^ (b as i32);
    }
    h & 0x7FFF_FFFF
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

/// The bytes of an image: owned, or shared with the caller's mapping.
enum ImageBacking {
    Owned(Vec<u8>),
    Shared(std::sync::Arc<dyn AsRef<[u8]> + Send + Sync>),
}

impl std::ops::Deref for ImageBacking {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        match self {
            ImageBacking::Owned(v) => v,
            ImageBacking::Shared(s) => (**s).as_ref(),
        }
    }
}

/// A read-only jimage file.
///
/// Construct via [`JImageReader::open`] or [`JImageReader::from_bytes`].
/// Use [`JImageReader::find_resource`] to look up a fully-qualified path
/// (e.g. `"/java.base/java/lang/String.class"`) and retrieve its bytes.
/// The reader owns the backing buffer for its lifetime.
pub struct JImageReader {
    /// The full file contents: owned in memory ([`JImageReader::open`],
    /// [`JImageReader::from_bytes`]) or shared with a caller-supplied mapping
    /// ([`JImageReader::from_shared`], SBR round 5 lane warmup: the VM maps the
    /// ~146 MB `lib/modules` instead of reading it, so a boot touches only the
    /// pages it indexes and the classes it loads).
    data: ImageBacking,
    header: Header,
    /// Byte offset of the redirect table within `data`.
    redirect_offset: usize,
    /// Byte offset of the offset table within `data`.
    offsets_offset: usize,
    /// Byte offset of the locations buffer within `data`.
    locations_offset: usize,
    /// Byte offset of the strings buffer within `data`.
    strings_offset: usize,
    /// Byte offset where the resource data section begins.
    resources_offset: usize,
    /// p5-cp: the strings buffer is valid UTF-8 as a whole and ends in a NUL,
    /// checked once at open. Then every in-bounds offset that does not land
    /// inside a multi-byte character names a terminated, valid string, so a
    /// walk can check an offset in O(1) ([`Self::check_string`]) with exactly
    /// [`read_string`]'s verdict instead of scanning and re-validating it.
    /// `false` (any other buffer) keeps the per-string check.
    strings_trusted: bool,
}

/// The string-table offsets of one location's path components (p5-cp,
/// [`JImageReader::for_each_entry_offsets`]): `0` is the empty string. Two
/// locations with the same offset in a field have the same string there,
/// which is what lets a walk memoise per offset; equal strings at different
/// offsets are possible and must still compare equal by content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JImageEntryOffsets {
    pub module: u64,
    pub parent: u64,
    pub base: u64,
    pub extension: u64,
}

impl JImageReader {
    /// Open a jimage file from disk. Reads the entire file into memory.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, JImageError> {
        let mut file = File::open(path.as_ref())?;
        // Pre-allocate based on the file size so we do one big read.
        let size = file.metadata().map(|m| m.len() as usize).unwrap_or(0);
        let mut data = Vec::with_capacity(size);
        file.read_to_end(&mut data)?;
        Self::from_bytes(data)
    }

    /// Construct a reader from an already-loaded buffer. Used by unit
    /// tests and by callers that have already read the file for their
    /// own reasons.
    pub fn from_bytes(data: Vec<u8>) -> Result<Self, JImageError> {
        Self::from_backing(ImageBacking::Owned(data))
    }

    /// Construct a reader over bytes the caller owns elsewhere (a memory
    /// mapping of the image file, typically). The reader keeps the `Arc`
    /// alive for its lifetime and reads only through it.
    pub fn from_shared(
        data: std::sync::Arc<dyn AsRef<[u8]> + Send + Sync>,
    ) -> Result<Self, JImageError> {
        Self::from_backing(ImageBacking::Shared(data))
    }

    fn from_backing(data: ImageBacking) -> Result<Self, JImageError> {
        let header = Header::parse(&data)?;
        // Compute section offsets. Every addition is guarded against
        // overflow and against extending past the file length.
        let table_bytes = (header.table_length as usize)
            .checked_mul(4)
            .ok_or(JImageError::TruncatedSection("redirect"))?;

        let redirect_offset = HEADER_SIZE;
        let offsets_offset = redirect_offset
            .checked_add(table_bytes)
            .ok_or(JImageError::TruncatedSection("offsets"))?;
        let locations_offset = offsets_offset
            .checked_add(table_bytes)
            .ok_or(JImageError::TruncatedSection("locations"))?;
        let strings_offset = locations_offset
            .checked_add(header.locations_size as usize)
            .ok_or(JImageError::TruncatedSection("strings"))?;
        let resources_offset = strings_offset
            .checked_add(header.strings_size as usize)
            .ok_or(JImageError::TruncatedSection("resources"))?;

        if resources_offset > data.len() {
            return Err(JImageError::TruncatedSection("data"));
        }

        let strings = &data[strings_offset..resources_offset];
        let strings_trusted = strings.last() == Some(&0) && std::str::from_utf8(strings).is_ok();

        Ok(JImageReader {
            data,
            header,
            redirect_offset,
            offsets_offset,
            locations_offset,
            strings_offset,
            resources_offset,
            strings_trusted,
        })
    }

    /// Reader version (from the header).
    pub fn version(&self) -> (u16, u16) {
        (self.header.major_version, self.header.minor_version)
    }

    /// Number of resources in the image (as declared by the header).
    pub fn resource_count(&self) -> u32 {
        self.header.resource_count
    }

    /// Reconstruct the full path `/module/parent/base.extension` from a
    /// location record. Empty fields are skipped cleanly.
    fn location_path(&self, loc: &Location) -> Result<String, JImageError> {
        let strings = self.strings_buffer();
        let module = read_string(strings, loc.module)?;
        let parent = read_string(strings, loc.parent)?;
        let base = read_string(strings, loc.base)?;
        let extension = read_string(strings, loc.extension)?;

        let mut path = String::with_capacity(
            1 + module.len() + 1 + parent.len() + 1 + base.len() + 1 + extension.len(),
        );
        if !module.is_empty() {
            path.push('/');
            path.push_str(module);
        }
        if !parent.is_empty() {
            path.push('/');
            path.push_str(parent);
        }
        if !base.is_empty() {
            path.push('/');
            path.push_str(base);
        }
        if !extension.is_empty() {
            path.push('.');
            path.push_str(extension);
        }
        Ok(path)
    }

    fn redirect_table(&self) -> &[u8] {
        &self.data[self.redirect_offset..self.offsets_offset]
    }

    fn offset_table(&self) -> &[u8] {
        &self.data[self.offsets_offset..self.locations_offset]
    }

    fn locations_buffer(&self) -> &[u8] {
        &self.data[self.locations_offset..self.strings_offset]
    }

    fn strings_buffer(&self) -> &[u8] {
        &self.data[self.strings_offset..self.resources_offset]
    }

    fn resources_buffer(&self) -> &[u8] {
        &self.data[self.resources_offset..]
    }

    /// Read a 4-byte redirect-table entry. Returns `None` if `index` is out
    /// of range — the redirect/offset tables come from an untrusted jimage,
    /// so an unchecked slice would panic on a malformed file.
    fn read_redirect(&self, index: usize) -> Option<i32> {
        let off = index.checked_mul(4)?;
        let bytes = self.redirect_table().get(off..off + 4)?;
        Some(if self.header.little_endian {
            i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        } else {
            i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        })
    }

    /// Read a 4-byte offset-table entry. Returns `None` if `index` is out of
    /// range — see [`Self::read_redirect`].
    fn read_offset_entry(&self, index: usize) -> Option<u32> {
        let off = index.checked_mul(4)?;
        let bytes = self.offset_table().get(off..off + 4)?;
        Some(if self.header.little_endian {
            u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        } else {
            u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        })
    }

    /// Resolve `path` through the perfect-hash tables and return the
    /// decoded location record, or `None` if the path is not present.
    fn find_location(&self, path: &str) -> Result<Option<Location>, JImageError> {
        let n = self.header.table_length as usize;
        if n == 0 {
            return Ok(None);
        }

        let h = jimage_hash(path, HASH_MULTIPLIER);
        let bucket = (h as usize) % n;
        // `bucket < n`, but the redirect table may itself be truncated —
        // treat an out-of-range entry as "not present".
        let redirect = match self.read_redirect(bucket) {
            Some(r) => r,
            None => return Ok(None),
        };
        if redirect == 0 {
            return Ok(None);
        }
        let index = if redirect < 0 {
            // Direct index into the offset table.
            match i64::from(redirect)
                .checked_neg()
                .and_then(|i| i.checked_sub(1))
                .and_then(|i| usize::try_from(i).ok())
            {
                Some(i) => i,
                None => return Ok(None),
            }
        } else {
            // Re-hash with the supplied seed.
            let h2 = jimage_hash(path, redirect);
            (h2 as usize) % n
        };
        if index >= n {
            return Ok(None);
        }
        let loc_offset = match self.read_offset_entry(index) {
            Some(o) => o as usize,
            None => return Ok(None),
        };
        // A location offset that points past the locations buffer is data
        // corruption, not an "absent" resource — surface it as a hard error
        // so the same corruption is reported consistently by `iter_entries`
        // (which propagates `decode_location` errors) and by this path.
        if loc_offset >= self.locations_buffer().len() {
            return Err(JImageError::BadLocationRecord(
                "location offset past end of locations buffer",
            ));
        }
        let loc = decode_location(self.locations_buffer(), loc_offset)?;
        // Verify the reconstructed path matches exactly: hash collisions
        // can route two paths to the same bucket, and the displacement
        // table cannot distinguish them without a string compare. p5-cp:
        // compared component by component in place, the verdict (and the
        // string errors) of building `location_path` and comparing it.
        if self.location_path_is(&loc, path)? {
            Ok(Some(loc))
        } else {
            Ok(None)
        }
    }

    /// `self.location_path(loc)? == path`, without building the path.
    fn location_path_is(&self, loc: &Location, path: &str) -> Result<bool, JImageError> {
        let strings = self.strings_buffer();
        let module = read_string(strings, loc.module)?;
        let parent = read_string(strings, loc.parent)?;
        let base = read_string(strings, loc.base)?;
        let extension = read_string(strings, loc.extension)?;
        let mut rest = path.as_bytes();
        let mut eat = |sep: u8, part: &str| -> bool {
            if part.is_empty() {
                return true;
            }
            let p = part.as_bytes();
            if rest.len() > p.len() && rest[0] == sep && &rest[1..=p.len()] == p {
                rest = &rest[1 + p.len()..];
                true
            } else {
                false
            }
        };
        let matched =
            eat(b'/', module) && eat(b'/', parent) && eat(b'/', base) && eat(b'.', extension);
        Ok(matched && rest.is_empty())
    }

    /// Look up a resource by its full jimage path and return its bytes.
    ///
    /// `path` must be the fully-qualified form produced by jimage, e.g.
    /// `"/java.base/java/lang/String.class"`. Callers that have a
    /// `(module, internal_name)` pair should use
    /// [`JImageReader::find_class`] instead, which builds the full path
    /// automatically.
    ///
    /// Returns `Ok(None)` if the resource is not present. Returns
    /// [`JImageError::Compressed`] if the resource is compressed (rare
    /// in production JDKs; opt-in via jlink's `--compress` flag).
    pub fn find_resource(&self, path: &str) -> Result<Option<Vec<u8>>, JImageError> {
        Ok(self.find_resource_bytes(path)?.map(<[u8]>::to_vec))
    }

    /// [`Self::find_resource`], borrowing the bytes from the image instead of
    /// copying them (p5-cp: the class path copies a class once, into the
    /// shared buffer it hands out, rather than into a `Vec` first).
    pub fn find_resource_bytes(&self, path: &str) -> Result<Option<&[u8]>, JImageError> {
        let loc = match self.find_location(path)? {
            Some(l) => l,
            None => return Ok(None),
        };
        if loc.compressed_size > 0 {
            return Err(JImageError::Compressed(path.to_string()));
        }
        let start = loc.offset;
        let size = loc.uncompressed_size;
        let end = start
            .checked_add(size)
            .ok_or(JImageError::BadResourceRange {
                offset: start,
                size,
            })?;
        let resources = self.resources_buffer();
        if end as usize > resources.len() {
            return Err(JImageError::BadResourceRange {
                offset: start,
                size,
            });
        }
        Ok(Some(&resources[start as usize..end as usize]))
    }

    /// Whether the image holds `/<module>/<internal_name>.class`, by the same
    /// perfect-hash lookup and full-path check as [`Self::find_class`], without
    /// copying the class file (zs s6/rs: the class path's per-package index
    /// confirms a class with it).
    pub fn has_class(&self, module: &str, internal_name: &str) -> Result<bool, JImageError> {
        let mut path = String::with_capacity(module.len() + internal_name.len() + 10);
        path.push('/');
        path.push_str(module);
        path.push('/');
        path.push_str(internal_name);
        path.push_str(".class");
        Ok(self.find_location(&path)?.is_some())
    }

    /// Look up a class file by its module and internal name.
    ///
    /// Example:
    ///
    /// ```ignore
    /// let bytes = reader.find_class("java.base", "java/lang/String")?;
    /// ```
    pub fn find_class(
        &self,
        module: &str,
        internal_name: &str,
    ) -> Result<Option<Vec<u8>>, JImageError> {
        Ok(self
            .find_class_bytes(module, internal_name)?
            .map(<[u8]>::to_vec))
    }

    /// [`Self::find_class`], borrowing the bytes ([`Self::find_resource_bytes`]).
    pub fn find_class_bytes(
        &self,
        module: &str,
        internal_name: &str,
    ) -> Result<Option<&[u8]>, JImageError> {
        let mut path = String::with_capacity(module.len() + internal_name.len() + 10);
        path.push('/');
        path.push_str(module);
        path.push('/');
        path.push_str(internal_name);
        path.push_str(".class");
        self.find_resource_bytes(&path)
    }

    /// Iterate over every (path, offset, uncompressed_size) tuple in the
    /// jimage. Used by integration tests to sanity-check the reader
    /// against a synthesized image, and by
    /// `ClassPath::load_jimage` to pre-populate a module-name map.
    ///
    /// The iteration walks the offset table in order, decoding each
    /// location record once. Bucket entries that redirect to an already-
    /// visited location are skipped to avoid double-reporting.
    pub fn iter_entries(&self) -> Result<Vec<(String, u64, u64)>, JImageError> {
        let n = self.header.table_length as usize;
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for i in 0..n {
            let loc_offset = match self.read_offset_entry(i) {
                Some(o) => o as usize,
                None => continue,
            };
            if loc_offset == 0 || !seen.insert(loc_offset) {
                continue;
            }
            // Mirror `find_location`: an offset past the locations buffer is
            // corruption and is surfaced as a hard error rather than silently
            // skipped, so both lookup paths treat a corrupt buffer alike.
            if loc_offset >= self.locations_buffer().len() {
                return Err(JImageError::BadLocationRecord(
                    "location offset past end of locations buffer",
                ));
            }
            let loc = decode_location(self.locations_buffer(), loc_offset)?;
            let path = self.location_path(&loc)?;
            out.push((path, loc.offset, loc.uncompressed_size));
        }
        Ok(out)
    }

    /// Visit every entry exactly as [`Self::iter_entries`] enumerates them
    /// (same order, same de-duplication, same errors), handing the callback the
    /// location's four path components `(module, parent, base, extension)`
    /// instead of an assembled path `String`. The full path of an entry is
    /// `"/" + module + "/" + parent + "/" + base + "." + extension` with every
    /// empty component (and its separator) left out, see
    /// [`Self::location_path`].
    ///
    /// SBR round 5, lane warmup: the VM walks all ~30 000 entries at every
    /// boot to build its class index; assembling, splitting and re-allocating a
    /// path per entry was a measurable slice of startup.
    pub fn for_each_entry_parts(
        &self,
        mut f: impl FnMut(&str, &str, &str, &str),
    ) -> Result<(), JImageError> {
        let strings = self.strings_buffer();
        self.for_each_entry_offsets(|e| {
            // `for_each_entry_offsets` checked all four offsets, so these
            // reads cannot fail; `unwrap_or` keeps the walk panic-free anyway.
            let module = read_string(strings, e.module).unwrap_or("");
            let parent = read_string(strings, e.parent).unwrap_or("");
            let base = read_string(strings, e.base).unwrap_or("");
            let extension = read_string(strings, e.extension).unwrap_or("");
            f(module, parent, base, extension);
        })
    }

    /// Visit every entry exactly as [`Self::for_each_entry_parts`] does (same
    /// order, same de-duplication, same errors: each of the four component
    /// offsets is checked as [`read_string`] would, before the callback),
    /// handing the callback the component OFFSETS (p5-cp). A walk that only
    /// needs a component's identity (the module of a run of entries, the
    /// `class` extension, a package seen before) memoises by offset and reads
    /// the string ([`Self::string_at`]) once per distinct offset.
    pub fn for_each_entry_offsets(
        &self,
        mut f: impl FnMut(JImageEntryOffsets),
    ) -> Result<(), JImageError> {
        let n = self.header.table_length as usize;
        let locations = self.locations_buffer();
        // De-duplication by location offset: a bit per locations-buffer byte
        // (an offset is checked to be in bounds before its bit is read).
        let mut seen = vec![0u64; locations.len().div_ceil(64)];
        for i in 0..n {
            let loc_offset = match self.read_offset_entry(i) {
                Some(o) => o as usize,
                None => continue,
            };
            if loc_offset == 0 {
                continue;
            }
            if loc_offset >= locations.len() {
                return Err(JImageError::BadLocationRecord(
                    "location offset past end of locations buffer",
                ));
            }
            let (word, bit) = (loc_offset / 64, 1u64 << (loc_offset % 64));
            if seen[word] & bit != 0 {
                continue;
            }
            seen[word] |= bit;
            let loc = decode_location(locations, loc_offset)?;
            self.check_string(loc.module)?;
            self.check_string(loc.parent)?;
            self.check_string(loc.base)?;
            self.check_string(loc.extension)?;
            f(JImageEntryOffsets {
                module: loc.module,
                parent: loc.parent,
                base: loc.base,
                extension: loc.extension,
            });
        }
        Ok(())
    }

    /// `read_string(strings, offset).map(drop)`: in O(1) when the strings
    /// buffer is trusted (see `strings_trusted`).
    #[inline]
    fn check_string(&self, offset: u64) -> Result<(), JImageError> {
        if !self.strings_trusted {
            return read_string(self.strings_buffer(), offset).map(drop);
        }
        let off = offset as usize;
        if off == 0 {
            return Ok(());
        }
        match self.strings_buffer().get(off) {
            // A whole valid UTF-8 buffer that ends in NUL: a string starting
            // at a character boundary ends at the next NUL (ASCII, so also a
            // boundary) and is valid. A continuation byte is not a boundary.
            Some(&b) if (b & 0xC0) != 0x80 => Ok(()),
            _ => Err(JImageError::BadStringOffset(off)),
        }
    }

    /// The string at `offset` in the strings table (`""` for `0`), with
    /// [`read_string`]'s errors.
    pub fn string_at(&self, offset: u64) -> Result<&str, JImageError> {
        read_string(self.strings_buffer(), offset)
    }

    /// Whether the string at `offset` is `s`, with [`read_string`]'s errors,
    /// comparing in place (no scan to the terminator when it differs early).
    pub fn string_is(&self, offset: u64, s: &str) -> Result<bool, JImageError> {
        if !self.strings_trusted {
            return Ok(self.string_at(offset)? == s);
        }
        self.check_string(offset)?;
        if offset == 0 {
            return Ok(s.is_empty());
        }
        let strings = self.strings_buffer();
        let off = offset as usize;
        let want = s.as_bytes();
        Ok(strings.get(off..off + want.len()) == Some(want)
            && strings.get(off + want.len()) == Some(&0))
    }

    /// Whether the string at `offset` is empty, with [`read_string`]'s errors.
    pub fn string_is_empty(&self, offset: u64) -> Result<bool, JImageError> {
        self.string_is(offset, "")
    }
}

// ---------------------------------------------------------------------------
// Writer (test support only — builds a minimal valid jimage in memory)
// ---------------------------------------------------------------------------

/// Public helper that synthesizes a small but standard-conformant jimage
/// from a list of resources. Used by `reader`'s own unit tests and by
/// downstream crates (e.g. `classloading`) to drive the reader without
/// needing a real `lib/modules` file on disk.
///
/// This helper is intentionally kept lightweight and does not handle
/// every corner of the jimage spec (no compression, no >8-byte attribute
/// values). It produces files that the reader can parse and round-trip,
/// which is all that's needed for tests.
pub mod test_builder {
    use super::*;

    /// Build an in-memory jimage containing `resources`, each described
    /// by `(module, parent, base, extension, bytes)`. The resulting file
    /// passes the reader's round-trip checks.
    ///
    /// The builder uses a deliberately simple perfect-hash assignment:
    /// `table_length = max(8, 2 * resource_count)` with single-step
    /// lookup — each bucket points directly at its offset-table slot via
    /// a negative redirect value, so the re-hash path is never exercised
    /// by the primary test. A second helper below forces the two-level
    /// path to exercise it independently.
    pub fn build_simple(resources: &[(&str, &str, &str, &str, &[u8])]) -> Vec<u8> {
        build_with_options(
            resources,
            BuildOptions {
                force_rehash: false,
            },
        )
    }

    /// As [`build_simple`], but forces every lookup to exercise the
    /// two-level re-hash path. Used to test that jimage_hash + the
    /// redirect-value seed agree with the C implementation.
    pub fn build_with_rehash(resources: &[(&str, &str, &str, &str, &[u8])]) -> Vec<u8> {
        build_with_options(resources, BuildOptions { force_rehash: true })
    }

    pub(crate) struct BuildOptions {
        pub force_rehash: bool,
    }

    pub(crate) fn build_with_options(
        resources: &[(&str, &str, &str, &str, &[u8])],
        opts: BuildOptions,
    ) -> Vec<u8> {
        // 1. Build the strings buffer. We intern every distinct string
        //    into it and record its offset.
        let mut strings: Vec<u8> = vec![0]; // offset 0 = empty string
        let mut string_offset = |s: &str, strings: &mut Vec<u8>| -> u64 {
            if s.is_empty() {
                return 0;
            }
            // Linear search for existing instance (test data is tiny).
            let needle = s.as_bytes();
            let mut i = 0;
            while i + needle.len() < strings.len() {
                if strings[i + needle.len()] == 0 && &strings[i..i + needle.len()] == needle {
                    return i as u64;
                }
                // Skip to the next null-terminated entry.
                while i < strings.len() && strings[i] != 0 {
                    i += 1;
                }
                i += 1;
            }
            let off = strings.len() as u64;
            strings.extend_from_slice(needle);
            strings.push(0);
            off
        };

        // 2. Build the locations buffer and the resource data buffer.
        //    For each resource, emit its attribute stream at the current
        //    locations cursor and record (path, loc_offset).
        let mut locations: Vec<u8> = vec![0]; // offset 0 is reserved sentinel
        let mut rdata: Vec<u8> = Vec::new();
        let mut entries: Vec<(String, u64)> = Vec::new();
        for (module, parent, base, extension, bytes) in resources {
            let mod_off = string_offset(module, &mut strings);
            let par_off = string_offset(parent, &mut strings);
            let base_off = string_offset(base, &mut strings);
            let ext_off = string_offset(extension, &mut strings);

            let res_offset = rdata.len() as u64;
            let res_size = bytes.len() as u64;
            rdata.extend_from_slice(bytes);

            let loc_start = locations.len() as u64;
            append_attr(&mut locations, AttributeKind::Module, mod_off);
            append_attr(&mut locations, AttributeKind::Parent, par_off);
            append_attr(&mut locations, AttributeKind::Base, base_off);
            append_attr(&mut locations, AttributeKind::Extension, ext_off);
            append_attr(&mut locations, AttributeKind::Offset, res_offset);
            append_attr(&mut locations, AttributeKind::Uncompressed, res_size);
            locations.push(0); // END

            // Reconstruct the path the same way the reader will.
            let mut path = String::new();
            if !module.is_empty() {
                path.push('/');
                path.push_str(module);
            }
            if !parent.is_empty() {
                path.push('/');
                path.push_str(parent);
            }
            if !base.is_empty() {
                path.push('/');
                path.push_str(base);
            }
            if !extension.is_empty() {
                path.push('.');
                path.push_str(extension);
            }
            entries.push((path, loc_start));
        }

        // 3. Build the redirect + offset tables using a CHD-style
        //    assignment. Strategy:
        //      a. Compute each entry's primary bucket.
        //      b. Group entries by primary bucket.
        //      c. For a group of size 1: "single-step" assignment —
        //         pick any currently-unused offset-table slot, store
        //         `redirect[primary] = -(slot + 1)` so the reader
        //         decodes slot = -redirect - 1 directly.
        //      d. For a group of size > 1: brute-force search for a
        //         positive seed `s` such that re-hashing every entry
        //         in the group with `s` yields distinct, currently-
        //         unused secondary slots. Store `redirect[primary] = s`
        //         and write each entry's loc_offset into its secondary.
        //
        //    The table_len is oversized (`max(16, 4*n)`) so seeds
        //    converge quickly even for small inputs.
        let n = entries.len().max(4);
        let table_len = (n * 4).max(16);
        let mut redirect = vec![0i32; table_len];
        let mut offset_table = vec![0u32; table_len];
        let mut used = vec![false; table_len];

        // Group entries by primary bucket.
        let mut groups: std::collections::BTreeMap<usize, Vec<usize>> =
            std::collections::BTreeMap::new();
        for (i, (path, _)) in entries.iter().enumerate() {
            let primary = (jimage_hash(path, HASH_MULTIPLIER) as usize) % table_len;
            groups.entry(primary).or_default().push(i);
        }

        if !opts.force_rehash {
            // Single-step where possible, rehash only when a primary is
            // shared. This exercises both reader branches naturally.
            // Process groups with size 1 first, then multi-entry
            // groups, so multi-entry groups have fewer slots pre-taken
            // and their seed search converges faster.
            let mut singles: Vec<(usize, usize)> = Vec::new();
            let mut multis: Vec<(usize, Vec<usize>)> = Vec::new();
            for (primary, members) in groups {
                if members.len() == 1 {
                    singles.push((primary, members[0]));
                } else {
                    multis.push((primary, members));
                }
            }
            for (primary, entry_idx) in singles {
                // Pick the first free slot linearly — deterministic.
                let slot = (0..table_len)
                    .find(|&s| !used[s])
                    .expect("test builder: table full (single)");
                used[slot] = true;
                redirect[primary] = -((slot as i32) + 1);
                offset_table[slot] = entries[entry_idx].1 as u32;
            }
            for (primary, members) in multis {
                let (seed, slots) = find_group_seed(&entries, &members, &used, table_len);
                redirect[primary] = seed;
                for (member_idx, slot) in members.iter().zip(slots.iter()) {
                    used[*slot] = true;
                    offset_table[*slot] = entries[*member_idx].1 as u32;
                }
            }
        } else {
            // Force every entry through the re-hash path, including
            // single-entry groups. This exercises the reader's seeded
            // re-hash branch even when there are no real collisions.
            for (primary, members) in groups {
                let (seed, slots) = find_group_seed(&entries, &members, &used, table_len);
                redirect[primary] = seed;
                for (member_idx, slot) in members.iter().zip(slots.iter()) {
                    used[*slot] = true;
                    offset_table[*slot] = entries[*member_idx].1 as u32;
                }
            }
        }

        // 4. Assemble the file: header, redirect, offsets, locations,
        //    strings, resources.
        let mut out = Vec::with_capacity(
            HEADER_SIZE + table_len * 8 + locations.len() + strings.len() + rdata.len(),
        );
        // Header (little-endian).
        out.extend_from_slice(&JIMAGE_MAGIC.to_le_bytes());
        // u4 version = (major << 16) | minor — version 1.0 = 0x0001_0000.
        out.extend_from_slice(&((1u32 << 16) | 0).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // flags
        out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        out.extend_from_slice(&(table_len as u32).to_le_bytes());
        out.extend_from_slice(&(locations.len() as u32).to_le_bytes());
        out.extend_from_slice(&(strings.len() as u32).to_le_bytes());

        for v in &redirect {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for v in &offset_table {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&locations);
        out.extend_from_slice(&strings);
        out.extend_from_slice(&rdata);
        out
    }

    /// Brute-force search for a positive seed that routes every entry
    /// in `members` to a distinct currently-free slot in the offset
    /// table. `entries` is the full list of `(path, loc_offset)` tuples
    /// and `members` is the set of indices belonging to one primary
    /// bucket.
    ///
    /// Returns `(seed, slots)` where `slots[i]` is the secondary slot
    /// chosen for `members[i]`. Panics if no seed is found within a
    /// sane iteration limit; in practice this never triggers for the
    /// test inputs used by the suite because `table_len` is
    /// generously oversized.
    pub(crate) fn find_group_seed(
        entries: &[(String, u64)],
        members: &[usize],
        used: &[bool],
        table_len: usize,
    ) -> (i32, Vec<usize>) {
        for seed in 1i32..=100_000 {
            let mut slots = Vec::with_capacity(members.len());
            let mut local_used = std::collections::HashSet::new();
            let mut ok = true;
            for &mi in members {
                let path = &entries[mi].0;
                let h = jimage_hash(path, seed);
                let slot = (h as usize) % table_len;
                if used[slot] || !local_used.insert(slot) {
                    ok = false;
                    break;
                }
                slots.push(slot);
            }
            if ok {
                return (seed, slots);
            }
        }
        panic!(
            "test builder: could not find a seed for a primary bucket \
             after 100k iterations (table_len={table_len}, group={})",
            members.len()
        );
    }

    /// Emit a single variable-length attribute into `buf`. Chooses the
    /// minimum encoding width (1..8 bytes) needed to hold `value`.
    fn append_attr(buf: &mut Vec<u8>, kind: AttributeKind, value: u64) {
        // Determine the minimum byte length needed.
        let mut length = 1;
        let mut tmp = value >> 8;
        while tmp != 0 {
            length += 1;
            tmp >>= 8;
        }
        assert!(length <= 8, "attribute value requires >8 bytes");
        let header = ((kind as u8) << 3) | ((length - 1) as u8);
        buf.push(header);
        // Big-endian value bytes.
        for i in (0..length).rev() {
            buf.push((value >> (i * 8)) as u8);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;

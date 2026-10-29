//! Restoring an image: validate, map every segment at its recorded address
//! straight from the image file (lazy, copy-on-write), then -- only once every
//! segment is in place -- copy the data diff into the binary's `.data`/`.bss`.
//!
//! # Allocation discipline
//!
//! Applying the data diff rewrites the binary's statics, which include the
//! global allocator's state when the allocator lives in the binary (mimalloc).
//! A heap object allocated before the diff and freed after it is freed into an
//! allocator that never handed it out. So this loader frees every heap object
//! it allocated before it applies the diff, applies it with plain copies out of
//! a read-only mapping of the metadata, and returns a [`MappedImage`] holding no
//! heap memory. The caller owns the same problem for its own pre-restore heap
//! objects (including the `expected` key): leak them, or make sure the
//! allocator state they belong to survives the diff.

use super::format::Refusal;
use super::key::BootKey;
use std::path::Path;

/// Loader knobs.
#[derive(Clone, Copy, Debug, Default)]
pub struct MapOptions {
    /// Hash every stored payload page after mapping and refuse on mismatch.
    /// Reads every page, so it costs the lazy faulting the format exists to
    /// avoid; for diagnosis and tests, not the fast path.
    pub verify_payload: bool,
}

/// A successfully restored image. Holds no heap memory and owns nothing: the
/// mappings are the process's for its lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MappedImage {
    /// The root pointer recorded at capture.
    pub root: usize,
    pub segments: usize,
    /// Address space mapped (all segments, including zero tails).
    pub mapped_bytes: u64,
    /// Of which mapped from the image file.
    pub image_bytes: u64,
    /// Of which re-mapped from a segment's source file.
    pub source_file_bytes: u64,
    pub diff_runs: usize,
    pub diff_bytes: u64,
}

/// Map the image at `path` if its key equals `expected`. Equivalent to
/// [`try_map_with`] with default options.
pub fn try_map(path: &Path, expected: &BootKey) -> Result<MappedImage, Refusal> {
    try_map_with(path, expected, MapOptions::default())
}

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
pub fn try_map_with(
    _path: &Path,
    _expected: &BootKey,
    _opts: MapOptions,
) -> Result<MappedImage, Refusal> {
    Err(Refusal::Unsupported)
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
pub fn try_map_with(
    path: &Path,
    expected: &BootKey,
    opts: MapOptions,
) -> Result<MappedImage, Refusal> {
    linux::try_map_with(path, expected, opts)
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
mod linux {
    use super::super::format::{
        ImageMeta, Refusal, SegEntry, HEADER_SIZE, SEG_SHARED, SOURCE_ANON, SOURCE_FILE,
    };
    use super::super::hash::hash64;
    use super::super::key::{BootKey, StatIdentity};
    use super::super::prot;
    use super::super::rw_segment::main_object;
    use super::{MapOptions, MappedImage};
    use libc::{
        c_void, MAP_ANONYMOUS, MAP_FAILED, MAP_FIXED_NOREPLACE, MAP_NORESERVE, MAP_PRIVATE,
    };
    use std::fs::File;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::FileExt;
    use std::os::unix::io::AsRawFd;
    use std::path::Path;

    fn errno() -> i32 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    }

    /// A read-only mapping of the metadata prefix, unmapped on drop.
    struct MetaMap {
        ptr: *mut c_void,
        len: usize,
    }
    impl MetaMap {
        fn bytes(&self) -> &[u8] {
            // SAFETY: a live PROT_READ mapping of `len` bytes.
            unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) }
        }
    }
    impl Drop for MetaMap {
        fn drop(&mut self) {
            // SAFETY: we own this mapping.
            unsafe { libc::munmap(self.ptr, self.len) };
        }
    }

    /// The segments mapped so far; unmapped on drop unless committed.
    struct Mapped {
        ranges: Vec<(usize, usize)>,
        committed: bool,
    }
    impl Drop for Mapped {
        fn drop(&mut self) {
            if !self.committed {
                for &(a, l) in &self.ranges {
                    // SAFETY: each range is a mapping this loader created.
                    unsafe { libc::munmap(a as *mut c_void, l) };
                }
            }
        }
    }

    /// `mmap` exactly at `addr` without clobbering anything already there.
    /// Kernels before 4.17 ignore `MAP_FIXED_NOREPLACE` and treat the address
    /// as a hint, so a different result address is also "busy".
    fn map_at(
        addr: usize,
        len: usize,
        prot: u32,
        flags: i32,
        fd: i32,
        off: u64,
    ) -> Result<(), Refusal> {
        // SAFETY: MAP_FIXED_NOREPLACE never replaces an existing mapping.
        let p = unsafe {
            libc::mmap(
                addr as *mut c_void,
                len,
                prot as i32,
                flags | MAP_FIXED_NOREPLACE,
                fd,
                off as libc::off_t,
            )
        };
        if p == MAP_FAILED {
            let e = errno();
            return Err(if e == libc::EEXIST {
                Refusal::AddressBusy {
                    addr: addr as u64,
                    len: len as u64,
                }
            } else {
                Refusal::MapFailed {
                    addr: addr as u64,
                    errno: e,
                }
            });
        }
        if p as usize != addr {
            // SAFETY: the kernel just gave us this mapping.
            unsafe { libc::munmap(p, len) };
            return Err(Refusal::AddressBusy {
                addr: addr as u64,
                len: len as u64,
            });
        }
        Ok(())
    }

    pub(super) fn try_map_with(
        path: &Path,
        expected: &BootKey,
        opts: MapOptions,
    ) -> Result<MappedImage, Refusal> {
        let file = File::open(path).map_err(|e| Refusal::io("open", &e))?;
        let file_len = file.metadata().map_err(|e| Refusal::io("fstat", &e))?.len();
        let mut hdr = [0u8; HEADER_SIZE];
        if file_len < HEADER_SIZE as u64 {
            return Err(Refusal::TooSmall(file_len));
        }
        file.read_exact_at(&mut hdr, 0)
            .map_err(|e| Refusal::io("read header", &e))?;
        let h = super::super::format::Header::decode(&hdr, file_len)?;
        let page = super::super::page_size() as u64;
        if h.page_size as u64 != page {
            return Err(Refusal::PageSize {
                image: h.page_size as u64,
                process: page,
            });
        }

        // SAFETY: read-only private mapping of a file we hold open.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                h.meta_mapped_len as usize,
                libc::PROT_READ,
                MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        if p == MAP_FAILED {
            return Err(Refusal::MapFailed {
                addr: 0,
                errno: errno(),
            });
        }
        let meta_map = MetaMap {
            ptr: p,
            len: h.meta_mapped_len as usize,
        };
        let meta = ImageMeta::parse(meta_map.bytes(), file_len)?;
        expected.check_encoded(meta.key_bytes())?;

        // The binary's writable segment must be the one the diff was taken
        // over (same binary and load base are in the key; this catches a key
        // that omitted them).
        let obj = main_object().ok_or(Refusal::BinaryLayout("main object not found"))?;
        if meta.header.rw_count as usize != obj.n_regions
            || !meta.rw_regions().zip(obj.records()).all(|(a, b)| a == b)
        {
            return Err(Refusal::BinaryLayout("writable regions differ"));
        }

        // File-backed sources: identity first, before anything is mapped.
        for s in meta.segments().filter(|s| s.source == SOURCE_FILE) {
            let sp = Path::new(std::ffi::OsStr::from_bytes(meta.seg_path(&s)));
            match StatIdentity::of(sp) {
                Ok(id) if id == s.src_identity => {}
                _ => {
                    return Err(Refusal::SourceChanged {
                        path: sp.display().to_string(),
                    })
                }
            }
        }

        let mut mapped = Mapped {
            ranges: Vec::with_capacity(meta.header.seg_count as usize * 2),
            committed: false,
        };
        let (mut total, mut from_image, mut from_src) = (0u64, 0u64, 0u64);
        for s in meta.segments() {
            map_segment(&meta, &s, &file, &mut mapped)?;
            total += s.len;
            match s.source {
                SOURCE_ANON => from_image += s.payload_len,
                _ => from_src += s.len,
            }
        }
        if opts.verify_payload {
            for (i, s) in meta.segments().enumerate() {
                if s.source == SOURCE_ANON && s.payload_len > 0 && s.prot & prot::READ != 0 {
                    // SAFETY: just mapped readable.
                    let mem = unsafe {
                        std::slice::from_raw_parts(
                            s.addr as usize as *const u8,
                            s.payload_len as usize,
                        )
                    };
                    if hash64(0, mem) != s.payload_hash {
                        return Err(Refusal::PayloadHash(i)); // `mapped` unmaps.
                    }
                }
            }
        }

        // Point of no return. Free the loader's own heap objects first (see
        // the module doc), then apply the diff from the read-only metadata.
        mapped.committed = true;
        drop(mapped);
        let segments = meta.header.seg_count as usize;
        let root = meta.header.root as usize;
        let diff_runs = meta.header.run_count as usize;
        let diff_bytes = meta.header.diff_len;
        let base = obj.load_base;
        for (off, bytes) in meta.diff_runs() {
            // SAFETY: `parse` proved every run lies inside a recorded writable
            // region, and the regions equal this binary's (checked above).
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    (base + off as usize) as *mut u8,
                    bytes.len(),
                );
            }
        }
        drop(meta_map);
        drop(file);
        Ok(MappedImage {
            root,
            segments,
            mapped_bytes: total,
            image_bytes: from_image,
            source_file_bytes: from_src,
            diff_runs,
            diff_bytes,
        })
    }

    fn map_segment(
        meta: &ImageMeta<'_>,
        s: &SegEntry,
        image: &File,
        mapped: &mut Mapped,
    ) -> Result<(), Refusal> {
        let addr = s.addr as usize;
        match s.source {
            SOURCE_ANON => {
                if s.payload_len > 0 {
                    map_at(
                        addr,
                        s.payload_len as usize,
                        s.prot,
                        MAP_PRIVATE,
                        image.as_raw_fd(),
                        s.file_off,
                    )?;
                    mapped.ranges.push((addr, s.payload_len as usize));
                }
                let tail = (s.len - s.payload_len) as usize;
                if tail > 0 {
                    let mut flags = MAP_PRIVATE | MAP_ANONYMOUS;
                    if s.prot == prot::NONE {
                        flags |= MAP_NORESERVE;
                    }
                    map_at(addr + s.payload_len as usize, tail, s.prot, flags, -1, 0)?;
                    mapped.ranges.push((addr + s.payload_len as usize, tail));
                }
            }
            _ => {
                let sp = Path::new(std::ffi::OsStr::from_bytes(meta.seg_path(s)));
                let f = File::open(sp).map_err(|_| Refusal::SourceChanged {
                    path: sp.display().to_string(),
                })?;
                // Re-check on the open descriptor: the path may have been
                // replaced between the identity check and the open.
                let id = f
                    .metadata()
                    .map(|m| StatIdentity::from_metadata(&m))
                    .map_err(|e| Refusal::io("fstat source", &e))?;
                if id != s.src_identity {
                    return Err(Refusal::SourceChanged {
                        path: sp.display().to_string(),
                    });
                }
                let share = if s.flags & SEG_SHARED != 0 {
                    libc::MAP_SHARED
                } else {
                    MAP_PRIVATE
                };
                map_at(
                    addr,
                    s.len as usize,
                    s.prot,
                    share,
                    f.as_raw_fd(),
                    s.src_offset,
                )?;
                mapped.ranges.push((addr, s.len as usize));
            }
        }
        Ok(())
    }
}

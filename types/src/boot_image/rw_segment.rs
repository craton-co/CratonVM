//! The launcher binary's own writable segment (`.data` + `.bss`): located at
//! run time from the main object's program headers, snapshotted at P0, and
//! diffed at P1.
//!
//! The region is every `PT_LOAD` with `PF_W`, from `p_vaddr` to
//! `p_vaddr + p_memsz` (so `.bss` is included), minus the `PT_GNU_RELRO`
//! prefix: the dynamic loader makes RELRO read-only after relocation, so it
//! cannot be written back, and it is fully determined by the binary and the
//! library layout, which the key pins.

use super::format::{DataDiff, RwRegionRecord};
use std::ops::Range;

/// Most writable `PT_LOAD`s an ELF main object has in practice is one or two.
pub(crate) const MAX_REGIONS: usize = 8;

/// The main object's load facts, gathered without heap allocation (the loader
/// calls this right before it writes the binary's `.data`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct MainObject {
    pub load_base: usize,
    /// Absolute `(start, len)` of each writable region.
    pub regions: [(usize, usize); MAX_REGIONS],
    pub n_regions: usize,
    pub build_id: [u8; 64],
    pub build_id_len: usize,
}

impl MainObject {
    pub fn regions(&self) -> &[(usize, usize)] {
        &self.regions[..self.n_regions]
    }

    pub fn records(&self) -> impl Iterator<Item = RwRegionRecord> + '_ {
        self.regions().iter().map(|&(s, l)| RwRegionRecord {
            offset: (s - self.load_base) as u64,
            len: l as u64,
        })
    }
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
mod sys {
    use super::{MainObject, MAX_REGIONS};
    use libc::{c_int, c_void, dl_phdr_info, size_t, PF_W, PT_GNU_RELRO, PT_LOAD, PT_NOTE};

    const NT_GNU_BUILD_ID: u32 = 3;

    unsafe extern "C" fn first_object(
        info: *mut dl_phdr_info,
        _size: size_t,
        data: *mut c_void,
    ) -> c_int {
        // SAFETY: the loader passes a valid info record; `data` is the
        // `Option<MainObject>` the caller passed in.
        let info = unsafe { &*info };
        let out = unsafe { &mut *(data as *mut Option<MainObject>) };
        let base = info.dlpi_addr as usize;
        let phdrs = unsafe { std::slice::from_raw_parts(info.dlpi_phdr, info.dlpi_phnum as usize) };
        let mut obj = MainObject {
            load_base: base,
            regions: [(0, 0); MAX_REGIONS],
            n_regions: 0,
            build_id: [0; 64],
            build_id_len: 0,
        };
        let relro = phdrs.iter().find(|p| p.p_type == PT_GNU_RELRO).map(|p| {
            let s = base + p.p_vaddr as usize;
            (s, s + p.p_memsz as usize)
        });
        for p in phdrs {
            if p.p_type == PT_LOAD && p.p_flags & PF_W != 0 && obj.n_regions < MAX_REGIONS {
                let mut start = base + p.p_vaddr as usize;
                let end = start + p.p_memsz as usize;
                if let Some((rs, re)) = relro {
                    if rs <= start && re > start {
                        start = re.min(end);
                    }
                }
                if end > start {
                    obj.regions[obj.n_regions] = (start, end - start);
                    obj.n_regions += 1;
                }
            }
            if p.p_type == PT_NOTE && obj.build_id_len == 0 {
                // SAFETY: a PT_NOTE of a loaded object lies in a mapped,
                // readable segment.
                let notes = unsafe {
                    std::slice::from_raw_parts(
                        (base + p.p_vaddr as usize) as *const u8,
                        p.p_filesz as usize,
                    )
                };
                let mut at = 0usize;
                while at + 12 <= notes.len() {
                    let rd =
                        |o: usize| u32::from_le_bytes(notes[o..o + 4].try_into().unwrap()) as usize;
                    let (namesz, descsz, ty) = (rd(at), rd(at + 4), rd(at + 8) as u32);
                    let name_at = at + 12;
                    let desc_at = name_at + ((namesz + 3) & !3);
                    let next = desc_at + ((descsz + 3) & !3);
                    if next > notes.len() {
                        break;
                    }
                    if ty == NT_GNU_BUILD_ID && notes[name_at..name_at + namesz] == *b"GNU\0" {
                        let n = descsz.min(64);
                        obj.build_id[..n].copy_from_slice(&notes[desc_at..desc_at + n]);
                        obj.build_id_len = n;
                        break;
                    }
                    at = next;
                }
            }
        }
        *out = Some(obj);
        1 // the first object is the main program; stop.
    }

    pub fn main_object() -> Option<MainObject> {
        let mut out: Option<MainObject> = None;
        // SAFETY: callback matches the expected signature and only writes `out`.
        unsafe {
            libc::dl_iterate_phdr(Some(first_object), &mut out as *mut _ as *mut c_void);
        }
        out
    }
}

#[cfg(not(all(target_os = "linux", target_pointer_width = "64")))]
mod sys {
    use super::MainObject;
    pub fn main_object() -> Option<MainObject> {
        None
    }
}

pub(crate) use sys::main_object;

pub(crate) fn build_id() -> Option<Vec<u8>> {
    let m = main_object()?;
    (m.build_id_len > 0).then(|| m.build_id[..m.build_id_len].to_vec())
}

pub(crate) fn load_base() -> Option<usize> {
    main_object().map(|m| m.load_base)
}

/// A copy of the binary's writable segment taken at P0.
#[derive(Debug)]
pub struct RwSnapshot {
    obj: MainObject,
    copies: Vec<Vec<u8>>,
}

impl RwSnapshot {
    /// Copy the writable segment now (call at P0).
    pub fn take() -> std::io::Result<RwSnapshot> {
        let obj = main_object().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "binary writable segment not locatable on this platform",
            )
        })?;
        // Allocate every buffer first, then copy, so the copy sees the
        // allocator state after its own allocations.
        let mut copies: Vec<Vec<u8>> = obj.regions().iter().map(|&(_, l)| vec![0u8; l]).collect();
        for (buf, &(s, l)) in copies.iter_mut().zip(obj.regions()) {
            // SAFETY: the region is a mapped, writable (hence readable) part of
            // the main object.
            buf.copy_from_slice(unsafe { std::slice::from_raw_parts(s as *const u8, l) });
        }
        Ok(RwSnapshot { obj, copies })
    }

    /// Absolute address ranges of the writable regions.
    pub fn regions(&self) -> Vec<Range<usize>> {
        self.obj.regions().iter().map(|&(s, l)| s..s + l).collect()
    }

    pub fn load_base(&self) -> usize {
        self.obj.load_base
    }

    /// The exact absolute byte ranges that differ from the P0 copy now (no
    /// merging): what the static classifier resolves to owners.
    pub fn changed(&self) -> Vec<Range<usize>> {
        let mut out = Vec::new();
        for (before, &(s, l)) in self.copies.iter().zip(self.obj.regions()) {
            // SAFETY: as in `take`.
            let now = unsafe { std::slice::from_raw_parts(s as *const u8, l) };
            let mut i = 0;
            while i < l {
                if now[i] == before[i] {
                    i += 1;
                    continue;
                }
                let start = i;
                while i < l && now[i] != before[i] {
                    i += 1;
                }
                out.push(s + start..s + i);
            }
        }
        out
    }

    /// Diff the segment's current contents against the P0 copy (call at P1).
    /// `excludes` are absolute address ranges never to be restored (for
    /// example process-identity statics the restore side sets itself).
    pub fn diff(&self, excludes: &[Range<usize>]) -> DataDiff {
        let base = self.obj.load_base;
        let mut now: Vec<Vec<u8>> = self.copies.iter().map(|c| vec![0u8; c.len()]).collect();
        for (buf, &(s, l)) in now.iter_mut().zip(self.obj.regions()) {
            // SAFETY: as in `take`.
            buf.copy_from_slice(unsafe { std::slice::from_raw_parts(s as *const u8, l) });
        }
        let excl: Vec<Range<u64>> = excludes
            .iter()
            .map(|r| (r.start.saturating_sub(base)) as u64..(r.end.saturating_sub(base)) as u64)
            .collect();
        let mut diff = DataDiff {
            load_base: base as u64,
            regions: self.obj.records().collect(),
            runs: Vec::new(),
        };
        for ((before, after), &(s, _)) in self.copies.iter().zip(&now).zip(self.obj.regions()) {
            DataDiff::compute_region((s - base) as u64, before, after, &excl, &mut diff.runs);
        }
        diff
    }
}

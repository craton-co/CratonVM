//! Boot-image census (Phase 5 fast boot, WP0;
//! `docs/jdk-only/boot-image-snapshot-20261028.md`).
//!
//! What a boot image would have to carry between the launcher prologue (P0)
//! and a snapshot point (P1): the OS threads alive at P1, the mappings that
//! appeared, the open file descriptors, and every byte of this binary's own
//! writable segment (`.data`/`.bss`) that changed. The byte diff is printed as
//! offsets from the binary's load base, so `nm -n` on the same binary resolves
//! each run to the static that owns it.
//!
//! `CRATONVM_DBG_IMAGE_CENSUS=<file>` turns it on (Linux only); the report is
//! appended to `<file>`. Off, [`Census::at_p0`] returns `None` and costs one
//! environment read.

#[cfg(target_os = "linux")]
mod imp {
    use std::io::Write;

    /// The running binary's `.data`+`.bss` span and load base. The RELRO part
    /// of the writable PT_LOAD (`.data.rel.ro`, `.got`) is read-only after
    /// startup, so `__data_start.._end` is all that can change.
    struct RwSegment {
        base: usize,
        start: usize,
        len: usize,
    }

    extern "C" {
        static __executable_start: u8;
        static __data_start: u8;
        static _end: u8;
    }

    fn rw_segment() -> Option<RwSegment> {
        // SAFETY: linker-defined symbols; only their addresses are taken.
        let (base, start, end) = unsafe {
            (
                std::ptr::addr_of!(__executable_start) as usize,
                std::ptr::addr_of!(__data_start) as usize,
                std::ptr::addr_of!(_end) as usize,
            )
        };
        (end > start).then(|| RwSegment { base, start, len: end - start })
    }

    pub struct Census {
        path: String,
        seg: RwSegment,
        p0_data: Vec<u8>,
        p0_maps: String,
    }

    impl Census {
        pub fn at_p0() -> Option<Self> {
            let path = std::env::var("CRATONVM_DBG_IMAGE_CENSUS").ok()?;
            let seg = rw_segment()?;
            // SAFETY: the span is this binary's mapped, readable RW segment.
            let p0_data =
                unsafe { std::slice::from_raw_parts(seg.start as *const u8, seg.len) }.to_vec();
            let p0_maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
            Some(Self { path, seg, p0_data, p0_maps })
        }

        pub fn at_p1(&self, label: &str) {
            let mut out = String::new();
            out.push_str(&format!(
                "== image census {label} (pid {}, base {:#x}, rw {:#x}+{:#x})\n",
                std::process::id(),
                self.seg.base,
                self.seg.start - self.seg.base,
                self.seg.len
            ));
            out.push_str("-- threads\n");
            if let Ok(dir) = std::fs::read_dir("/proc/self/task") {
                for task in dir.flatten() {
                    let comm = std::fs::read_to_string(task.path().join("comm")).unwrap_or_default();
                    out.push_str(&format!("{} {}", task.file_name().to_string_lossy(), comm));
                }
            }
            out.push_str("-- fds\n");
            if let Ok(dir) = std::fs::read_dir("/proc/self/fd") {
                for fd in dir.flatten() {
                    let target = std::fs::read_link(fd.path()).unwrap_or_default();
                    out.push_str(&format!(
                        "{} -> {}\n",
                        fd.file_name().to_string_lossy(),
                        target.display()
                    ));
                }
            }
            out.push_str("-- new mappings\n");
            let maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
            for line in maps.lines() {
                if !self.p0_maps.lines().any(|l| l == line) {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            out.push_str("-- rw diff (offset-from-base len)\n");
            // SAFETY: as in `at_p0`.
            let now = unsafe { std::slice::from_raw_parts(self.seg.start as *const u8, self.seg.len) };
            let rel = self.seg.start - self.seg.base;
            let mut i = 0;
            let mut runs = 0usize;
            let mut bytes = 0usize;
            while i < now.len() {
                if now[i] != self.p0_data[i] {
                    let s = i;
                    // Merge runs separated by fewer than 8 equal bytes.
                    let mut last = i;
                    while i < now.len() && i - last < 8 {
                        if now[i] != self.p0_data[i] {
                            last = i;
                        }
                        i += 1;
                    }
                    out.push_str(&format!("{:#x} {}\n", rel + s, last + 1 - s));
                    runs += 1;
                    bytes += last + 1 - s;
                } else {
                    i += 1;
                }
            }
            out.push_str(&format!("-- rw diff total: {runs} runs, {bytes} bytes\n"));
            if let Ok(mut f) =
                std::fs::OpenOptions::new().create(true).append(true).open(&self.path)
            {
                let _ = f.write_all(out.as_bytes());
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    pub struct Census;
    impl Census {
        pub fn at_p0() -> Option<Self> {
            None
        }
        pub fn at_p1(&self, _label: &str) {}
    }
}

pub use imp::Census;

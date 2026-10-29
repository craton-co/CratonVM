//! The image key: every input that, if it differs between the capture run and
//! this run, makes the image's memory wrong for this process.
//!
//! A key is an ordered list of named byte-string fields. It is stored whole in
//! the image (not only hashed) so a refusal can name the field that differed.
//! Its hash names the cache file, so differently-keyed images coexist.

use super::format::{put_u32, rd_u32, Refusal};
use super::hash::hash64;
use std::path::{Path, PathBuf};

/// Identity of a file: enough to notice it was replaced or rewritten. Used for
/// key inputs (`lib/modules`, class path entries) and as the stored form of a
/// segment source's [`crate::image_layout::FileIdentity`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatIdentity {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_sec: i64,
    pub mtime_nsec: u32,
}

impl StatIdentity {
    pub fn of(path: &Path) -> std::io::Result<StatIdentity> {
        let md = std::fs::metadata(path)?;
        Ok(Self::from_metadata(&md))
    }

    #[cfg(unix)]
    pub fn from_metadata(md: &std::fs::Metadata) -> StatIdentity {
        use std::os::unix::fs::MetadataExt;
        StatIdentity {
            dev: md.dev(),
            ino: md.ino(),
            size: md.size(),
            mtime_sec: md.mtime(),
            mtime_nsec: md.mtime_nsec() as u32,
        }
    }

    #[cfg(not(unix))]
    pub fn from_metadata(md: &std::fs::Metadata) -> StatIdentity {
        let (mtime_sec, mtime_nsec) = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| (d.as_secs() as i64, d.subsec_nanos()))
            .unwrap_or((0, 0));
        StatIdentity {
            dev: 0,
            ino: 0,
            size: md.len(),
            mtime_sec,
            mtime_nsec,
        }
    }

    /// The stored form of a registry file identity.
    pub fn from_layout(id: &crate::image_layout::FileIdentity) -> StatIdentity {
        StatIdentity {
            dev: id.dev,
            ino: id.ino,
            size: id.size,
            mtime_sec: id.mtime_sec,
            mtime_nsec: id.mtime_nsec as u32,
        }
    }

    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.dev.to_le_bytes());
        out.extend_from_slice(&self.ino.to_le_bytes());
        out.extend_from_slice(&self.size.to_le_bytes());
        out.extend_from_slice(&self.mtime_sec.to_le_bytes());
        out.extend_from_slice(&self.mtime_nsec.to_le_bytes());
    }
}

/// What the launcher knows about the run that the key cannot discover itself.
#[derive(Clone, Copy, Debug, Default)]
pub struct KeyInputs<'a> {
    /// `java.home`; the key records the identity of `<java_home>/lib/modules`.
    pub java_home: Option<&'a Path>,
    /// Class path entries, in order.
    pub classpath: &'a [PathBuf],
    /// The VM option string (everything that configures the VM, as the
    /// launcher canonicalised it).
    pub vm_options: &'a str,
}

/// Field names, in the order [`BootKey::gather`] emits them.
pub mod field {
    pub const BINARY: &str = "binary";
    pub const BINARY_LOAD_BASE: &str = "binary_load_base";
    pub const JDK_MODULES: &str = "jdk_modules";
    pub const CLASSPATH: &str = "classpath";
    pub const CWD: &str = "cwd";
    pub const USER: &str = "user";
    pub const ENV: &str = "env";
    pub const CPUS: &str = "cpus";
    pub const PHYS_RAM: &str = "phys_ram";
    pub const PAGE_SIZE: &str = "page_size";
    pub const THP: &str = "thp";
    pub const VM_OPTIONS: &str = "vm_options";
    pub const SO_LAYOUT: &str = "so_layout";
}

/// An ordered list of `(name, value)` fields.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BootKey {
    fields: Vec<(String, Vec<u8>)>,
}

/// Environment variables that affect boot: name prefix match.
const ENV_PREFIXES: &[&str] = &["CRATONVM_", "LC_"];
/// Environment variables that affect boot: exact names.
const ENV_NAMES: &[&str] = &["JAVA_TOOL_OPTIONS", "_JAVA_OPTIONS", "LANG", "TZ"];
/// Under the `CRATONVM_` prefix, the variables that steer the image machinery
/// itself (capture vs. restore, cache directory). They differ by design
/// between the capture and the restore run, so they cannot be key inputs.
/// Exact names: `CRATONVM_BOOT_IMAGE_CLASS_BYTES` (a class-loading knob that
/// shares the prefix) must stay a key input.
const ENV_IMAGE_CONTROL: &[&str] = &["BOOT_IMAGE", "BOOT_IMAGE_DIR"];

impl BootKey {
    pub fn new() -> BootKey {
        BootKey::default()
    }

    /// Append a field. Names must be unique; a later push of the same name
    /// replaces the earlier value in place.
    pub fn push(&mut self, name: &str, value: impl Into<Vec<u8>>) {
        let value = value.into();
        if let Some(f) = self.fields.iter_mut().find(|f| f.0 == name) {
            f.1 = value;
        } else {
            self.fields.push((name.to_string(), value));
        }
    }

    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.fields
            .iter()
            .find(|f| f.0 == name)
            .map(|f| f.1.as_slice())
    }

    /// A copy without the named fields (tests; or a caller that knows a field
    /// cannot matter for its configuration).
    pub fn without(&self, names: &[&str]) -> BootKey {
        BootKey {
            fields: self
                .fields
                .iter()
                .filter(|f| !names.contains(&f.0.as_str()))
                .cloned()
                .collect(),
        }
    }

    pub fn fields(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.fields.iter().map(|f| (f.0.as_str(), f.1.as_slice()))
    }

    /// Gather the full key for this process.
    pub fn gather(inputs: &KeyInputs<'_>) -> BootKey {
        let mut k = BootKey::new();
        k.push(field::BINARY, binary_identity());
        k.push(field::BINARY_LOAD_BASE, u64_bytes(binary_load_base()));
        k.push(field::JDK_MODULES, jdk_modules_identity(inputs.java_home));
        k.push(field::CLASSPATH, classpath_identity(inputs.classpath));
        k.push(field::CWD, cwd_bytes());
        k.push(field::USER, user_bytes());
        k.push(field::ENV, u64_bytes(env_hash(std::env::vars_os())));
        k.push(field::CPUS, cpus_bytes());
        k.push(field::PHYS_RAM, u64_bytes(phys_ram()));
        k.push(field::PAGE_SIZE, u64_bytes(super::page_size() as u64));
        k.push(field::THP, thp_mode());
        k.push(field::VM_OPTIONS, inputs.vm_options.as_bytes().to_vec());
        k.push(field::SO_LAYOUT, so_layout());
        k
    }

    /// Serialised form: `u32 count`, then per field `u32 name_len, name,
    /// u32 value_len, value`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_u32(&mut out, self.fields.len() as u32);
        for (n, v) in &self.fields {
            put_u32(&mut out, n.len() as u32);
            out.extend_from_slice(n.as_bytes());
            put_u32(&mut out, v.len() as u32);
            out.extend_from_slice(v);
        }
        out
    }

    pub fn decode(b: &[u8]) -> Result<BootKey, Refusal> {
        let mut k = BootKey::new();
        for f in encoded_fields(b) {
            let (n, v) = f?;
            k.fields
                .push((String::from_utf8_lossy(n).into_owned(), v.to_vec()));
        }
        Ok(k)
    }

    /// Compare against a stored, encoded key. Returns the first field that
    /// differs (or is present on one side only).
    pub fn check_encoded(&self, stored: &[u8]) -> Result<(), Refusal> {
        let mut n = 0usize;
        for f in encoded_fields(stored) {
            let (name, value) = f?;
            let mine = self.fields.get(n);
            match mine {
                Some((mn, mv)) if mn.as_bytes() == name && mv.as_slice() == value => {}
                Some((mn, _)) if mn.as_bytes() == name => {
                    return Err(Refusal::KeyMismatch { field: mn.clone() })
                }
                _ => {
                    return Err(Refusal::KeyMismatch {
                        field: String::from_utf8_lossy(name).into_owned(),
                    })
                }
            }
            n += 1;
        }
        if let Some((mn, _)) = self.fields.get(n) {
            return Err(Refusal::KeyMismatch { field: mn.clone() });
        }
        Ok(())
    }

    pub fn hash(&self) -> u64 {
        hash64(0x6B65_795F_6861_7368, &self.encode())
    }

    /// `<keyhash>.img`.
    pub fn file_name(&self) -> String {
        format!("{:016x}.img", self.hash())
    }
}

fn encoded_fields(b: &[u8]) -> impl Iterator<Item = Result<(&[u8], &[u8]), Refusal>> {
    let bad = Refusal::Corrupt("key encoding");
    let count = if b.len() >= 4 {
        rd_u32(b, 0) as usize
    } else {
        0
    };
    let mut at = 4usize;
    let mut failed = b.len() < 4;
    let mut left = count;
    std::iter::from_fn(move || {
        if failed {
            failed = false;
            left = 0;
            return Some(Err(bad.clone()));
        }
        if left == 0 {
            return None;
        }
        left -= 1;
        let mut take = |at: &mut usize| -> Option<&[u8]> {
            let l = rd_u32(b.get(*at..*at + 4)?, 0) as usize;
            let s = b.get(*at + 4..(*at + 4).checked_add(l)?)?;
            *at += 4 + l;
            Some(s)
        };
        match (take(&mut at), take(&mut at)) {
            (Some(n), Some(v)) => Some(Ok((n, v))),
            _ => {
                left = 0;
                Some(Err(bad.clone()))
            }
        }
    })
}

fn u64_bytes(v: u64) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

fn identity_bytes(path: &Path) -> Vec<u8> {
    let mut out = Vec::new();
    match StatIdentity::of(path) {
        Ok(id) => id.encode(&mut out),
        Err(_) => out.extend_from_slice(b"absent"),
    }
    out
}

fn path_bytes(p: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        p.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        p.to_string_lossy().as_bytes().to_vec()
    }
}

/// GNU build-id of the running binary, or (fallback) its size, mtime and inode.
fn binary_identity() -> Vec<u8> {
    if let Some(id) = super::rw_segment::build_id() {
        let mut v = b"build-id:".to_vec();
        v.extend_from_slice(&id);
        return v;
    }
    let mut v = b"stat:".to_vec();
    match std::env::current_exe() {
        Ok(p) => v.extend_from_slice(&identity_bytes(&p)),
        Err(_) => v.extend_from_slice(b"unknown"),
    }
    v
}

fn binary_load_base() -> u64 {
    super::rw_segment::load_base().unwrap_or(0) as u64
}

fn jdk_modules_identity(java_home: Option<&Path>) -> Vec<u8> {
    match java_home {
        Some(h) => identity_bytes(&h.join("lib").join("modules")),
        None => b"none".to_vec(),
    }
}

/// Each entry's path and identity. A directory entry is pinned by the
/// directory's own identity only: a class file changed inside it without the
/// directory's mtime moving is not seen (see the module's risks).
fn classpath_identity(cp: &[PathBuf]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in cp {
        let pb = path_bytes(p);
        put_u32(&mut out, pb.len() as u32);
        out.extend_from_slice(&pb);
        let id = identity_bytes(p);
        put_u32(&mut out, id.len() as u32);
        out.extend_from_slice(&id);
    }
    out
}

fn cwd_bytes() -> Vec<u8> {
    std::env::current_dir()
        .map(|p| path_bytes(&p))
        .unwrap_or_else(|_| b"?".to_vec())
}

fn user_bytes() -> Vec<u8> {
    let mut out = Vec::new();
    #[cfg(unix)]
    {
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        out.extend_from_slice(&(uid as u64).to_le_bytes());
    }
    for name in ["USER", "HOME"] {
        let v = std::env::var_os(name)
            .map(|v| path_bytes(Path::new(&v)))
            .unwrap_or_default();
        put_u32(&mut out, v.len() as u32);
        out.extend_from_slice(&v);
    }
    out
}

/// Hash of the boot-affecting variables, sorted by name.
pub(crate) fn env_hash(
    vars: impl Iterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> u64 {
    let mut picked: Vec<(Vec<u8>, Vec<u8>)> = vars
        .filter_map(|(n, v)| {
            let name = n.to_string_lossy().into_owned();
            let take = ENV_NAMES.contains(&name.as_str())
                || ENV_PREFIXES.iter().any(|p| name.starts_with(p));
            let image_control = name
                .strip_prefix(ENV_PREFIXES[0])
                .is_some_and(|rest| ENV_IMAGE_CONTROL.contains(&rest));
            (take && !image_control).then(|| (name.into_bytes(), path_bytes(Path::new(&v))))
        })
        .collect();
    picked.sort();
    let mut h = 0x656E_765F_6861_7368u64;
    for (n, v) in &picked {
        h = hash64(h, n);
        h = hash64(h, b"=");
        h = hash64(h, v);
        h = hash64(h, b"\0");
    }
    h
}

fn cpus_bytes() -> Vec<u8> {
    let mut out = Vec::new();
    let avail = std::thread::available_parallelism()
        .map(|n| n.get() as u64)
        .unwrap_or(0);
    out.extend_from_slice(&avail.to_le_bytes());
    #[cfg(unix)]
    {
        // SAFETY: sysconf has no preconditions.
        let online = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
        out.extend_from_slice(&(online as i64).to_le_bytes());
    }
    out
}

fn phys_ram() -> u64 {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: sysconf has no preconditions.
        let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
        if pages > 0 {
            return pages as u64 * super::page_size() as u64;
        }
    }
    0
}

fn thp_mode() -> Vec<u8> {
    #[cfg(target_os = "linux")]
    {
        let mut out = Vec::new();
        for f in [
            "/sys/kernel/mm/transparent_hugepage/enabled",
            "/sys/kernel/mm/transparent_hugepage/defrag",
        ] {
            let v = std::fs::read(f).unwrap_or_default();
            put_u32(&mut out, v.len() as u32);
            out.extend_from_slice(&v);
        }
        out
    }
    #[cfg(not(target_os = "linux"))]
    {
        b"n/a".to_vec()
    }
}

/// The shared-library load layout from `/proc/self/maps`: for each mapped
/// shared object (and the vDSO), its path and lowest mapped address, in
/// address order. Under ASLR this differs on every run, which is correct: the
/// binary's `.data` holds pointers into these libraries.
pub(crate) fn so_layout() -> Vec<u8> {
    #[cfg(target_os = "linux")]
    {
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
        so_layout_from_maps(&maps)
    }
    #[cfg(not(target_os = "linux"))]
    {
        b"n/a".to_vec()
    }
}

pub(crate) fn so_layout_from_maps(maps: &str) -> Vec<u8> {
    let mut seen: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    for line in maps.lines() {
        // start-end perms offset dev inode [path]
        let mut cols = line.splitn(6, ' ');
        let range = cols.next().unwrap_or("");
        let path = cols.nth(4).map(str::trim).unwrap_or("");
        let file = path.rsplit('/').next().unwrap_or("");
        let is_so = path.starts_with('/') && (file.ends_with(".so") || file.contains(".so."));
        if !(is_so || path == "[vdso]") || seen.contains(&path) {
            continue;
        }
        seen.push(path);
        let start = range.split('-').next().unwrap_or("");
        out.extend_from_slice(path.as_bytes());
        out.push(b'@');
        out.extend_from_slice(start.as_bytes());
        out.push(b'\n');
    }
    out
}

/// Default cache directory: `$XDG_CACHE_HOME/cratonvm/boot-images`, else
/// `$HOME/.cache/cratonvm/boot-images`. `override_dir` (a launcher option or
/// environment variable, owned by the launcher's flag surface) wins.
pub fn cache_dir(override_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(d) = override_dir {
        return Some(d.to_path_buf());
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".cache"))
        })?;
    Some(base.join("cratonvm").join("boot-images"))
}

/// `<dir>/<keyhash>.img`.
pub fn image_path(dir: &Path, key: &BootKey) -> PathBuf {
    dir.join(key.file_name())
}

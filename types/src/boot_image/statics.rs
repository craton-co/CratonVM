//! Static hazards (Phase 5 WP7): every launcher-binary static (and every
//! thread-local of the `main-vm` thread) the capture run's boot changed
//! between P0 and P1, resolved to the symbol that owns it and classified.
//!
//! A boot image copies the changed bytes of the binary's `.data`/`.bss` (the
//! data diff) and of `main-vm`'s TLS block into the restored process. That is
//! right for most statics -- a `OnceLock` pointing into the capture arena, a
//! memo of boot facts -- and wrong for the ones that name *this process*: an
//! allocator's own heap state, a thread id, a clock origin, a pthread key. So
//! each owner is classified, from a checked list the launcher owns
//! (`vm-cli/src/boot_image_statics.txt`, parsed by [`ClassList::parse`]):
//!
//! * `keep` -- restore-correct: the capture's value is valid in the restored
//!   process (it points into the image, or is pure boot data).
//! * `exclude` -- exclude-and-reinit: never copied; the restored process keeps
//!   the value its own prologue left (usually "uninitialised", so it
//!   re-initialises lazily), or the restore path rebinds it.
//! * `refuse` -- must not change between P0 and P1 (the launcher initialises
//!   it before P0); a change refuses the image.
//!
//! An owner the list does not name is treated as `refuse`: the capture run
//! does not write an image, and says which symbol it could not classify. So a
//! new static that a boot writes cannot silently break restore -- it turns the
//! image off until somebody classifies it.
//!
//! Symbols come from the binary's own `.symtab` (read from the file at capture
//! time only), Rust legacy-mangled names demangled with their hash dropped.

use std::io;
use std::ops::Range;
use std::path::Path;

/// How a changed static is treated by the image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StaticClass {
    Keep,
    Exclude,
    Refuse,
}

/// One data or TLS symbol of the binary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sym {
    /// Absolute address (data), or offset into the TLS block (TLS).
    pub addr: u64,
    pub size: u64,
    pub name: String,
}

/// The binary's data and TLS symbols, sorted by address.
#[derive(Clone, Debug, Default)]
pub struct SymTab {
    data: Vec<Sym>,
    tls: Vec<Sym>,
}

const STT_OBJECT: u8 = 1;
const STT_TLS: u8 = 6;
const SHT_SYMTAB: u32 = 2;

fn rd_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}
fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn rd_u64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

impl SymTab {
    /// Read the `.symtab` of the ELF64 little-endian file at `path`, keeping
    /// only the symbols `keep(addr, size, is_tls)` accepts. The file is
    /// mapped, not read into the heap: a capture run calls this while its
    /// arena is armed, and the arena must not grow by the size of a symbol
    /// table it would then dump.
    pub fn read_filtered(path: &Path, keep: &dyn Fn(u64, u64, bool) -> bool) -> io::Result<SymTab> {
        let file = FileBytes::open(path)?;
        SymTab::parse(file.bytes(), keep)
    }

    /// [`Self::read_filtered`] keeping every symbol.
    pub fn read(path: &Path) -> io::Result<SymTab> {
        Self::read_filtered(path, &|_, _, _| true)
    }

    fn parse(bytes: &[u8], keep: &dyn Fn(u64, u64, bool) -> bool) -> io::Result<SymTab> {
        let f = Slice(bytes);
        let eh = f.at(0, 64)?;
        if &eh[..4] != b"\x7fELF" || eh[4] != 2 || eh[5] != 1 {
            return Err(bad("not an ELF64 little-endian file"));
        }
        let shoff = rd_u64(eh, 0x28) as usize;
        let shentsize = rd_u16(eh, 0x3A) as usize;
        let shnum = rd_u16(eh, 0x3C) as usize;
        let shdrs = f.at(shoff, shentsize * shnum)?;
        let sh = |i: usize| &shdrs[i * shentsize..(i + 1) * shentsize];
        let symtab = (0..shnum)
            .find(|&i| rd_u32(sh(i), 4) == SHT_SYMTAB)
            .ok_or_else(|| bad("no .symtab (stripped binary)"))?;
        let s = sh(symtab);
        let (off, size, link, entsize) = (
            rd_u64(s, 0x18) as usize,
            rd_u64(s, 0x20) as usize,
            rd_u32(s, 0x28) as usize,
            rd_u64(s, 0x38) as usize,
        );
        let st = sh(link);
        let strtab = f.at(rd_u64(st, 0x18) as usize, rd_u64(st, 0x20) as usize)?;
        let syms = f.at(off, size)?;
        let mut tab = SymTab::default();
        for e in syms.chunks_exact(entsize.max(24)) {
            let ty = e[4] & 0xF;
            let shndx = rd_u16(e, 6);
            if (ty != STT_OBJECT && ty != STT_TLS) || shndx == 0 {
                continue;
            }
            let (addr, size) = (rd_u64(e, 8), rd_u64(e, 16));
            if !keep(addr, size, ty == STT_TLS) {
                continue;
            }
            let name_off = rd_u32(e, 0) as usize;
            let raw = strtab
                .get(name_off..)
                .and_then(|b| b.split(|&c| c == 0).next())
                .unwrap_or_default();
            let sym = Sym {
                addr,
                size,
                name: demangle(&String::from_utf8_lossy(raw)),
            };
            if ty == STT_TLS {
                tab.tls.push(sym);
            } else {
                tab.data.push(sym);
            }
        }
        tab.data.sort_by_key(|s| s.addr);
        tab.tls.sort_by_key(|s| s.addr);
        Ok(tab)
    }

    fn list(&self, tls: bool) -> &[Sym] {
        if tls {
            &self.tls
        } else {
            &self.data
        }
    }

    /// The symbol whose `[addr, addr+max(size,1))` holds `a`.
    pub fn owner(&self, a: u64, tls: bool) -> Option<&Sym> {
        let l = self.list(tls);
        let i = l.partition_point(|s| s.addr <= a).checked_sub(1)?;
        // Several symbols may share an address (aliases); take any covering one.
        l[..=i]
            .iter()
            .rev()
            .take(8)
            .find(|s| a < s.addr + s.size.max(1))
    }
}

/// Bounds-checked sub-slices of the file.
struct Slice<'a>(&'a [u8]);

impl<'a> Slice<'a> {
    fn at(&self, off: usize, len: usize) -> io::Result<&'a [u8]> {
        let end = off.checked_add(len).ok_or_else(|| bad("ELF offset overflow"))?;
        self.0.get(off..end).ok_or_else(|| bad("truncated ELF file"))
    }
}

/// The binary's bytes: a read-only mapping on Unix (unmapped on drop), a heap
/// copy elsewhere (tests).
struct FileBytes {
    #[cfg(unix)]
    ptr: *mut libc::c_void,
    #[cfg(unix)]
    len: usize,
    #[cfg(not(unix))]
    buf: Vec<u8>,
}

impl FileBytes {
    #[cfg(unix)]
    fn open(path: &Path) -> io::Result<FileBytes> {
        use std::os::unix::io::AsRawFd;
        let f = std::fs::File::open(path)?;
        let len = f.metadata()?.len() as usize;
        if len == 0 {
            return Err(bad("empty file"));
        }
        // SAFETY: a private read-only mapping of a file we hold open.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                f.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(FileBytes { ptr, len })
    }

    #[cfg(not(unix))]
    fn open(path: &Path) -> io::Result<FileBytes> {
        Ok(FileBytes {
            buf: std::fs::read(path)?,
        })
    }

    #[cfg(unix)]
    fn bytes(&self) -> &[u8] {
        // SAFETY: a live PROT_READ mapping of `len` bytes.
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) }
    }

    #[cfg(not(unix))]
    fn bytes(&self) -> &[u8] {
        &self.buf
    }
}

#[cfg(unix)]
impl Drop for FileBytes {
    fn drop(&mut self) {
        // SAFETY: our own mapping.
        unsafe { libc::munmap(self.ptr, self.len) };
    }
}

/// Demangle a Rust legacy (`_ZN...E`) or v0 (`_R...`) symbol, dropping the
/// trailing hash; anything else (C names) is returned unchanged. LLVM's
/// `.<n>` / `.llvm.<n>` suffixes (a release build's shrunk or promoted copy of
/// a static, e.g. `..._ACTIVE.0`) are not part of the mangling and are
/// dropped first, so a debug and a release build name the same static alike.
pub fn demangle(raw: &str) -> String {
    let raw = strip_llvm_suffix(raw);
    if let Some(v0) = raw.strip_prefix("_R") {
        return demangle_v0_path(v0).unwrap_or_else(|| raw.to_string());
    }
    let Some(mut rest) = raw.strip_prefix("_ZN") else {
        return raw.to_string();
    };
    let mut parts: Vec<String> = Vec::new();
    while !rest.starts_with('E') {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let Ok(n) = rest[..digits].parse::<usize>() else {
            return raw.to_string();
        };
        let Some(ident) = rest.get(digits..digits + n) else {
            return raw.to_string();
        };
        parts.push(unescape(ident));
        rest = &rest[digits + n..];
    }
    if parts.last().is_some_and(|p| {
        p.len() == 17 && p.starts_with('h') && p[1..].bytes().all(|b| b.is_ascii_hexdigit())
    }) {
        parts.pop();
    }
    parts.join("::")
}

/// `sym.0`, `sym.llvm.123`, `sym.0.llvm.4` -> `sym`. Only an all-digit or
/// `llvm` dot-segment at the end is stripped: legacy identifiers contain `..`
/// (`std..rt..lang_start`), which this never touches.
fn strip_llvm_suffix(raw: &str) -> &str {
    let mut s = raw;
    while let Some(dot) = s.rfind('.') {
        let seg = &s[dot + 1..];
        let numeric = !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_digit());
        if (numeric || seg == "llvm") && dot > 0 && s.as_bytes()[dot - 1] != b'.' {
            s = &s[..dot];
        } else {
            break;
        }
    }
    s
}

/// Rust v0 (`_R...`) symbols, reduced to their identifiers in path order
/// (`crate::module::item::STATIC`). Not a full v0 demangler: crate and
/// identifier disambiguators and back-references are skipped, and the
/// identifiers of impl/generic type arguments are kept inline, which is
/// enough for the classifier's patterns to be readable and stable across
/// builds (the crate disambiguator hash is dropped).
fn demangle_v0_path(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut parts: Vec<&str> = Vec::new();
    let mut i = 0;
    let base62_run = |from: usize| -> Option<usize> {
        let mut j = from;
        while j < b.len() && b[j].is_ascii_alphanumeric() {
            j += 1;
        }
        (j < b.len() && b[j] == b'_').then_some(j + 1)
    };
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_digit() {
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            let n: usize = s[start..i].parse().ok()?;
            if i < b.len() && b[i] == b'_' {
                i += 1;
            }
            let id = s.get(i..i + n)?;
            parts.push(id);
            i += n;
        } else if (c == b's' || c == b'B') && i + 1 < b.len() {
            // `s<base62>_` disambiguator, `B<base62>_` back-reference.
            match base62_run(i + 1) {
                Some(j) => i = j,
                None => i += 1,
            }
        } else if c == b'N' {
            i += 2; // namespace tag
        } else {
            i += 1;
        }
    }
    (!parts.is_empty()).then(|| parts.join("::"))
}

fn unescape(id: &str) -> String {
    let id = id.strip_prefix("_$").map(|r| format!("${r}")).unwrap_or(id.to_string());
    let mut out = String::new();
    let mut s = id.as_str();
    while !s.is_empty() {
        if let Some(r) = s.strip_prefix("..") {
            out.push_str("::");
            s = r;
        } else if s.starts_with('$') {
            let Some(end) = s[1..].find('$') else {
                out.push_str(s);
                break;
            };
            let code = &s[1..1 + end];
            let rep = match code {
                "LT" => "<".to_string(),
                "GT" => ">".to_string(),
                "C" => ",".to_string(),
                "RF" => "&".to_string(),
                "BP" => "*".to_string(),
                "SP" => "@".to_string(),
                "LP" => "(".to_string(),
                "RP" => ")".to_string(),
                c if c.starts_with('u') => u32::from_str_radix(&c[1..], 16)
                    .ok()
                    .and_then(char::from_u32)
                    .map(String::from)
                    .unwrap_or_else(|| format!("${c}$")),
                c => format!("${c}$"),
            };
            out.push_str(&rep);
            s = &s[2 + end..];
        } else {
            let n = s.find(['$', '.']).unwrap_or(s.len()).max(1);
            out.push_str(&s[..n]);
            s = &s[n..];
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Match {
    Exact,
    Prefix,
    Suffix,
    Contains,
}

/// The checked classification list.
#[derive(Clone, Debug, Default)]
pub struct ClassList {
    /// `(class, pattern, how it matches)`; the first match wins.
    rules: Vec<(StaticClass, String, Match)>,
}

impl ClassList {
    /// Lines: `<keep|exclude|refuse> <pattern>`; `#` comments; a pattern
    /// ending in `*` matches by prefix, one starting with `*` by suffix, one
    /// with both by substring, otherwise exactly. Patterns match the
    /// demangled, hash-free name (`std::io::stdio::STDOUT`).
    pub fn parse(text: &str) -> Result<ClassList, String> {
        let mut rules = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let (class, pat) = line
                .split_once(char::is_whitespace)
                .ok_or_else(|| format!("line {}: no pattern", n + 1))?;
            let class = match class {
                "keep" => StaticClass::Keep,
                "exclude" => StaticClass::Exclude,
                "refuse" => StaticClass::Refuse,
                c => return Err(format!("line {}: unknown class `{c}`", n + 1)),
            };
            let pat = pat.trim();
            let (pat, suffix) = match pat.strip_prefix('*') {
                Some(p) => (p, true),
                None => (pat, false),
            };
            let (pat, prefix) = match pat.strip_suffix('*') {
                Some(p) => (p, true),
                None => (pat, false),
            };
            let mode = match (suffix, prefix) {
                (false, false) => Match::Exact,
                (false, true) => Match::Prefix,
                (true, false) => Match::Suffix,
                (true, true) => Match::Contains,
            };
            rules.push((class, pat.to_string(), mode));
        }
        Ok(ClassList { rules })
    }

    pub fn classify(&self, name: &str) -> Option<StaticClass> {
        self.rules
            .iter()
            .find(|(_, p, mode)| match mode {
                Match::Exact => name == p,
                Match::Prefix => name.starts_with(p.as_str()),
                Match::Suffix => name.ends_with(p.as_str()),
                Match::Contains => name.contains(p.as_str()),
            })
            .map(|(c, _, _)| *c)
    }
}

/// One owner of changed bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolution {
    /// The owning symbol's name, or `?<addr>` for bytes no symbol covers.
    pub owner: String,
    /// The owner's whole range (data: absolute; TLS: block offsets).
    pub range: Range<u64>,
    /// Changed bytes attributed to it.
    pub bytes: u64,
    /// `None`: the list does not name it (treated as `refuse`).
    pub class: Option<StaticClass>,
}

/// Attribute `changed` (exact changed-byte ranges) to owners and classify
/// them. One entry per owner, in address order.
pub fn resolve(changed: &[Range<u64>], tab: &SymTab, list: &ClassList, tls: bool) -> Vec<Resolution> {
    let mut out: Vec<Resolution> = Vec::new();
    for r in changed {
        let mut a = r.start;
        while a < r.end {
            let (owner, range) = match tab.owner(a, tls) {
                Some(s) => (s.name.clone(), s.addr..s.addr + s.size.max(1)),
                // TLS bytes no symbol covers get one stable name (their
                // offset moves with every build); data bytes keep theirs.
                None if tls => ("?tls".to_string(), a..a + 1),
                None => (format!("?{a:#x}"), a..a + 1),
            };
            let end = range.end.min(r.end);
            let n = end - a;
            match out.iter_mut().find(|o| o.range == range && o.owner == owner) {
                Some(o) => o.bytes += n,
                None => {
                    let class = list.classify(&owner);
                    out.push(Resolution {
                        owner,
                        range,
                        bytes: n,
                        class,
                    })
                }
            }
            a = end;
        }
    }
    out.sort_by_key(|o| o.range.start);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demangles_legacy_names_and_drops_the_hash() {
        assert_eq!(
            demangle("_ZN3std2io5stdio6STDOUT17h0123456789abcdefE"),
            "std::io::stdio::STDOUT"
        );
        let seg = "drop_in_place$LT$std..rt..lang_start$u7b$$u7b$closure$u7d$$u7d$$GT$";
        assert_eq!(
            demangle(&format!("_ZN4core3ptr{}{seg}17h0123456789abcdefE", seg.len())),
            "core::ptr::drop_in_place<std::rt::lang_start{{closure}}>"
        );
        assert_eq!(demangle("_mi_heap_main"), "_mi_heap_main");
        assert_eq!(
            demangle("_RNvCs9rY60KznRNe_8cratonvm15VM_FOR_SHUTDOWN"),
            "cratonvm::VM_FOR_SHUTDOWN"
        );
        assert_eq!(
            demangle("_RNvNvNtNtCs1Ccr239EO5T_11cratonvm_vm7runtime9env_cache17tier_osr_backedge5CACHE"),
            "cratonvm_vm::runtime::env_cache::tier_osr_backedge::CACHE"
        );
        // A release build's shrunk copy of a static carries LLVM's `.0`; it must
        // name the same owner as the debug build's symbol.
        assert_eq!(
            demangle("_RNvNtCsiEWYkvuVel1_21cratonvm_classloading13class_manager18CLASS_HOOKS_ACTIVE.0"),
            "cratonvm_classloading::class_manager::CLASS_HOOKS_ACTIVE"
        );
        assert_eq!(
            demangle("_RNvNtCs2leKwoOEIJO_21cratonvm_classloading13class_manager18CLASS_HOOKS_ACTIVE"),
            "cratonvm_classloading::class_manager::CLASS_HOOKS_ACTIVE"
        );
        assert_eq!(
            demangle("_ZN3std2io5stdio6STDOUT17h0123456789abcdefE.llvm.42"),
            "std::io::stdio::STDOUT"
        );
    }

    #[test]
    fn the_first_matching_rule_wins_and_unlisted_names_are_none() {
        let l = ClassList::parse(
            "exclude std::io::stdio::STDOUT\nkeep std::*\n# c\nrefuse x\nkeep *::CALLSITE\nexclude *mid*",
        )
        .unwrap();
        assert_eq!(l.classify("std::io::stdio::STDOUT"), Some(StaticClass::Exclude));
        assert_eq!(l.classify("std::thread::ID"), Some(StaticClass::Keep));
        assert_eq!(l.classify("a::b::CALLSITE"), Some(StaticClass::Keep));
        assert_eq!(l.classify("a::middle::z"), Some(StaticClass::Exclude));
        assert_eq!(l.classify("x"), Some(StaticClass::Refuse));
        assert_eq!(l.classify("y"), None);
    }

    #[test]
    fn resolve_attributes_bytes_to_owners() {
        let tab = SymTab {
            data: vec![
                Sym { addr: 100, size: 8, name: "a".into() },
                Sym { addr: 108, size: 8, name: "b".into() },
            ],
            tls: Vec::new(),
        };
        let list = ClassList::parse("keep a").unwrap();
        let r = resolve(&[102..112, 200..201], &tab, &list, false);
        assert_eq!(r.len(), 3);
        assert_eq!((r[0].owner.as_str(), r[0].bytes, r[0].class), ("a", 6, Some(StaticClass::Keep)));
        assert_eq!((r[1].owner.as_str(), r[1].bytes, r[1].class), ("b", 4, None));
        assert_eq!(r[2].owner, "?0xc8");
    }
}

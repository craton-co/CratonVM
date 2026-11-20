#[allow(unused_imports)]
use super::*;

/// `list0(File)` — `None` maps to a Java `null`, which is what the JDK returns
/// for a path that is not a readable directory. Returning an empty array
/// instead would make `File.list()` on a plain file look like an empty
/// directory.
pub fn fs_list_dir(path: &str) -> Option<Vec<String>> {
    let entries = std::fs::read_dir(path).ok()?;
    Some(
        entries
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect(),
    )
}

/// The `ClassId` for `java/lang/String`, for allocating a genuinely typed
/// `String[]` rather than the untyped `Object[]` that
/// `new_array(ArrayElementType::Reference, ..)` hands back.
pub fn string_class_id(ctx: &mut dyn NativeContext) -> ClassId {
    ref_array_component_id(ctx, "java/lang/String")
}

/// The `ClassId` for `java/io/File`, for the `[Ljava/io/File;`-returning
/// natives (`File.listFiles`, `File.listRoots`).
pub fn file_class_id(ctx: &mut dyn NativeContext) -> ClassId {
    ref_array_component_id(ctx, "java/io/File")
}

pub fn ref_array_component_id(ctx: &mut dyn NativeContext, name: &str) -> ClassId {
    ctx.ensure_class_initialized(name)
        .ok()
        .or_else(|| ctx.class_id_by_name(name))
        .unwrap_or_else(|| ClassId::new(0))
}

/// `listRoots0()` — the drive-letter bitmask (bit 0 = `A:`). Only Windows has
/// a native for this; `UnixFileSystem.listRoots()` is plain Java returning
/// `/`, so the Unix build never reaches here.
pub fn fs_list_roots_bitmask() -> i32 {
    #[cfg(windows)]
    {
        extern "system" {
            fn GetLogicalDrives() -> u32;
        }
        // SAFETY: no arguments, no out-params — the call cannot observe or
        // corrupt anything in this process.
        (unsafe { GetLogicalDrives() }) as i32
    }
    #[cfg(not(windows))]
    {
        0
    }
}

// ---------------------------------------------------------------------------
// G70-1 N1 — the java.io.File path, in code units
// ---------------------------------------------------------------------------
//
// `File` is a STRING WRAPPER until something touches the disk: the path lives
// in a Java field, and `<init>` normalises it and writes it straight back.
// Every read on that round trip used `read_string`, which cannot hold an
// unpaired UTF-16 surrogate, so `new File("d/\uD83Dx").getPath()` came back
// with U+FFFD in place of the surrogate.
//
// That is not only a rendering defect. `equals`/`hashCode`/`compareTo` are
// computed FROM the same path, so two DIFFERENT files — one named with a lone
// surrogate, one named with a literal U+FFFD — collided: equal, same hash,
// compareTo 0. A substitution in a key is a merge, not a typo.
//
// Every separator that decides path structure is ASCII (`/`, `\`, and the
// drive colon), so scanning units for them is the same logic with a different
// index type. As in the URI accessors (G75-1 N1) there is still ONE rule: the
// `_units` functions below are the implementations and the `&str` spellings
// that already existed are thin wrappers over them. A `&str` can never hold a
// lone surrogate, so encoding one to units and back is exact — the wrappers
// lose nothing they did not already lack, and the two spellings cannot drift.

/// The separator(s) `java.io.File` accepts **on this platform**.
///
/// This used to answer "`/` or `\`, on every platform", with a comment saying
/// Java accepts both everywhere. It does not. `java.io.File` delegates to
/// `FileSystem`, and `UnixFileSystem`'s separator is `/` alone — on Unix a
/// backslash is an ORDINARY FILENAME CHARACTER, and a file really can be called
/// `a\b`. MEASURED on Linux with `probes/FilePathSweep.java`, 46 differing rows
/// over seven backslash-bearing inputs, every one of them this single cause:
///
/// ```text
///   new File("..\..\up").getName()      HotSpot "..\..\up"   CratonVM "up"
///   new File("..\..\up").getParent()    HotSpot null           CratonVM "..\.."
///   new File("\x").isAbsolute()          HotSpot false          CratonVM true
///   new File("trailing\").getName()      HotSpot "trailing\"   CratonVM "trailing"
/// ```
///
/// The second row is the one that reaches ordinary code: a file whose name
/// contains a backslash — which a Windows-authored filename copied onto a Linux
/// box routinely does — was split into a directory and a basename that do not
/// exist, so `getParentFile().mkdirs()` created a WRONG DIRECTORY and the file
/// was written somewhere nobody asked for. The third turns a relative path into
/// an absolute one.
///
/// The Windows arm is unchanged and is still both characters, which is what the
/// 23 `#[cfg(windows)]` tests in this file pin.
pub fn u_is_sep(c: u16) -> bool {
    #[cfg(windows)]
    {
        c == u16::from(b'/') || c == u16::from(b'\\')
    }
    #[cfg(not(windows))]
    {
        c == u16::from(b'/')
    }
}

/// `/` or `\`, on every platform — for the two callers that genuinely mean
/// "either spelling", rather than "a separator here".
///
/// [`u_is_sep`] is platform-specific because `java.io.File`'s own separator is.
/// A URI's syntax is not: `file:\C:\...` is a spelling this VM is documented to
/// receive (see `file_uri_reject`), and the scan that classifies it must accept
/// a backslash on a Linux host too or it would newly reject a URI it has always
/// accepted.
pub fn u_is_sep_either(c: u16) -> bool {
    c == u16::from(b'/') || c == u16::from(b'\\')
}

/// Per-unit case folding: `Character.toLowerCase(Character.toUpperCase(c))`,
/// which is exactly the rule `String.compareToIgnoreCase` applies, and is
/// length-preserving (unlike `String.toLowerCase`, which can change length).
///
/// `char::from_u32` answers `None` for a surrogate code unit, so an unpaired
/// surrogate falls through UNCHANGED — which is what we want: it has no case,
/// and folding must not be another way to lose it.
///
/// Only `file_path_key_units` calls this, and only on Windows — hence the
/// allow, so a Linux build does not warn on a function it correctly never uses.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn u_lower(c: u16) -> u16 {
    match char::from_u32(u32::from(c)) {
        Some(ch) => {
            let up = ch.to_uppercase().next().unwrap_or(ch);
            let lo = up.to_lowercase().next().unwrap_or(up);
            let v = lo as u32;
            if v <= 0xFFFF {
                v as u16
            } else {
                c
            }
        }
        None => c,
    }
}

/// `String.hashCode()` over code units — the definition Java gives.
///
/// The previous spelling iterated `path.bytes()`, i.e. UTF-8 bytes, which
/// agrees with Java only for ASCII. It also omitted the `^ 1234321` the JDK's
/// file systems mix in, so `new File("AB").hashCode()` answered 2081 where
/// HotSpot answers 1235376.
pub fn java_string_hash_units(p: &[u16]) -> i32 {
    let mut h: i32 = 0;
    for &c in p {
        h = h.wrapping_mul(31).wrapping_add(i32::from(c));
    }
    h
}

/// The comparison key for a path: case-folded on Windows, verbatim elsewhere.
///
/// `WinNTFileSystem` compares paths case-INSENSITIVELY (`compareToIgnoreCase`)
/// and `UnixFileSystem` compares them exactly. This was measured, not assumed:
/// on Windows HotSpot answers `new File("ab").equals(new File("AB")) == true`.
pub fn file_path_key_units(p: &[u16]) -> Vec<u16> {
    #[cfg(windows)]
    {
        p.iter().map(|&c| u_lower(c)).collect()
    }
    #[cfg(not(windows))]
    {
        p.to_vec()
    }
}

/// `String.compareTo` semantics over units: the difference of the first
/// differing unit, else the length difference. NOT a clamped -1/0/1 — callers
/// that print the value observe the real one.
pub fn u_compare(a: &[u16], b: &[u16]) -> i32 {
    let n = a.len().min(b.len());
    for i in 0..n {
        if a[i] != b[i] {
            return i32::from(a[i]) - i32::from(b[i]);
        }
    }
    a.len() as i32 - b.len() as i32
}

/// `File.getName()`, transcribed:
///
/// ```text
/// int index = path.lastIndexOf(separatorChar);
/// if (index < prefixLength) return path.substring(prefixLength);
/// return path.substring(index + 1);
/// ```
///
/// Transcribing it beats paraphrasing it. The first version of this function
/// trimmed trailing separators and split on the last one, which is the rule as
/// usually described — and it answers `C:` for `C:\` where Java answers the
/// empty string, because the described rule quietly assumes there is no root
/// prefix. The oracle was asked; the JDK's own three lines are what is here.
pub fn file_basename_units(p: &[u16]) -> Vec<u16> {
    let prefix = file_prefix_len_units(p).min(p.len());
    match p.iter().rposition(|&c| u_is_sep(c)) {
        Some(i) if i >= prefix => p[i + 1..].to_vec(),
        _ => p[prefix..].to_vec(),
    }
}

/// The length of the ROOT prefix, which `File.getParent` may never cut into.
///
/// `java.io.File` keeps this as `prefixLength` and returns
/// `path.substring(0, prefixLength)` whenever the last separator falls inside
/// it. Without it `new File("C:\\x").getParent()` answers `C:` where Java
/// answers `C:\` — a fault the `std::path::Path::parent` spelling did NOT have,
/// and one no relative-path probe row can see. Found by re-reading the
/// conversion, not by the sweep; the vector now carries absolute rows for it.
pub fn file_prefix_len_units(p: &[u16]) -> usize {
    #[cfg(windows)]
    {
        // `X:` optionally followed by a separator.
        if p.len() >= 2
            && p[1] == u16::from(b':')
            && char::from_u32(u32::from(p[0])).is_some_and(|c| c.is_ascii_alphabetic())
        {
            return if p.len() >= 3 && u_is_sep(p[2]) { 3 } else { 2 };
        }
        // A UNC path begins with two separators.
        if p.len() >= 2 && u_is_sep(p[0]) && u_is_sep(p[1]) {
            return 2;
        }
    }
    if !p.is_empty() && u_is_sep(p[0]) {
        return 1;
    }
    0
}

/// `File.getParent()`, transcribed:
///
/// ```text
/// int index = path.lastIndexOf(separatorChar);
/// if (index < prefixLength) {
///     if ((prefixLength > 0) && (path.length() > prefixLength))
///         return path.substring(0, prefixLength);
///     return null;
/// }
/// return path.substring(0, index);
/// ```
///
/// Three cases hang on that shape and were each wrong in a paraphrase of it:
/// `C:\` has parent NULL (not `C:\`, because the path is no longer than its own
/// prefix), `C:x` has parent `C:` (there is no separator at all, yet a parent
/// exists), and `\x` has parent `\`. All three were taken from the oracle.
pub fn file_parent_units(p: &[u16]) -> Option<Vec<u16>> {
    let prefix = file_prefix_len_units(p);
    match p.iter().rposition(|&c| u_is_sep(c)) {
        Some(i) if i >= prefix => Some(p[..i].to_vec()),
        _ if prefix > 0 && p.len() > prefix => Some(p[..prefix].to_vec()),
        _ => None,
    }
}

/// [`file_read_path_units`], lossily, for the callers that hand the value to
/// the operating system rather than back to Java. Those genuinely need a
/// `str`: `std::fs` cannot open a path Rust cannot spell.
pub fn file_read_path(ctx: &mut dyn NativeContext, this: ObjectRef) -> String {
    String::from_utf16_lossy(&file_read_path_units(ctx, this))
}

/// The stored path of a `File`, in code units.
pub fn file_read_path_units(ctx: &mut dyn NativeContext, this: ObjectRef) -> Vec<u16> {
    // Prefer the real-JDK `getPath()` implementation when Available (correct
    // `prefixLength` + internal path for `java.io.File` loaded from modules).
    //
    // gc-common w20-d: `getPath()` is Java; when it answers nothing the
    // fallbacks below read the File's fields, which went through its address
    // from before the call. Pinned across it, read back.
    let this_pin = ctx.pin_native_root(this);
    let via_get_path = match ctx.invoke(
        "java/io/File",
        "getPath",
        "()Ljava/lang/String;",
        &[Value::Object(Some(this))],
    ) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string_units(s).unwrap_or_default(),
        _ => Vec::new(),
    };
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    if !via_get_path.is_empty() {
        return file_normalise_path_units(&via_get_path);
    }
    if let Value::Object(Some(s)) = ctx.get_field_by_name(this, "path") {
        let r = ctx.read_string_units(s).unwrap_or_default();
        if !r.is_empty() {
            return file_normalise_path_units(&r);
        }
    }
    let raw = match ctx.get_field(this, 0) {
        Value::Object(Some(s)) => ctx.read_string_units(s).unwrap_or_default(),
        _ => Vec::new(),
    };
    // Defensive: Spring Boot's URI -> File round-trip can hand us a
    // raw URI-style `/C:/...` path on Windows when the real-JDK File
    // bytecode (which would normalise it) is bypassed. Normalise here
    // so std::fs callers see a path Rust's `Path::is_absolute` recognises.
    file_normalise_path_units(&raw)
}

/// Normalise a path string the way `java.io.WinNTFileSystem.normalize` does.
/// On Windows, real-JDK File.<init> strips a leading `/` before a drive
/// letter so URI-style `/C:/Users/foo` round-trips to `C:\Users\foo`. This
/// matters for `URL.toURI().getSchemeSpecificPart() -> new File(...)` —
/// the round-trip Spring Boot's fat-jar launcher relies on. On non-Windows
/// the input is returned unchanged.
#[cfg(windows)]
pub fn file_normalise_path(path: &str) -> String {
    let units: Vec<u16> = path.encode_utf16().collect();
    String::from_utf16_lossy(&file_normalise_path_units(&units))
}

/// [`file_normalise_path`] in code units — the implementation of both.
///
/// One deliberate behaviour change comes with the conversion: the trailing
/// separator guard below counts UNITS where the `&str` spelling counted BYTES.
/// `WinNTFileSystem.normalize` counts chars, so the units count is the correct
/// one; the byte count only agreed for ASCII paths, and disagreed silently for
/// any other (a 2-char non-ASCII path measured 4 and skipped the trim).
#[cfg(windows)]
pub fn file_normalise_path_units(path: &[u16]) -> Vec<u16> {
    let slash = u16::from(b'/');
    let back = u16::from(b'\\');
    // Strip EVERY leading separator, of either kind, before `<drive>:`, as
    // `WinNTFileSystem.normalizePrefix` does: `/C:/foo`, `\C:\foo`
    // (`URLClassPath$FileLoader`'s spelling of a `file:/C:/...` URL) and
    // `//C:/foo` are all `C:\foo`. Only one `/` was stripped until
    // interpreter round i1 wave 52, so `--compatible` kept `\C:\...` and a
    // real `URLClassLoader` over a directory found nothing
    // (`docs/feature-designs/native-dispatch-architecture.md`).
    let leading = path
        .iter()
        .take_while(|&&c| c == slash || c == back)
        .count();
    let body = if path.len() - leading >= 2
        && char::from_u32(u32::from(path[leading])).is_some_and(|c| c.is_ascii_alphabetic())
        && path[leading + 1] == u16::from(b':')
    {
        &path[leading..]
    } else {
        path
    };
    // Normalise forward slashes to Java's canonical Windows separator.
    let swapped: Vec<u16> = body
        .iter()
        .map(|&c| if c == slash { back } else { c })
        .collect();

    // COLLAPSE RUNS OF SEPARATORS. `WinNTFileSystem.normalize` does this and
    // this function did not, so `new File("a//b").getPath()` answered `a\\b`
    // where HotSpot answers `a\b` (MEASURED: `probes/FilePathSweep.java`, 62
    // differing lines against HotSpot 25.0.3+9, in BOTH modes). The `#[cfg(not(
    // windows))]` sibling below has always collapsed runs, with a comment
    // naming the Spring failure that forced it -- the two arms of one function
    // simply disagreed, and the Windows one was the wrong half.
    //
    // A LEADING DOUBLE separator survives as exactly two: that is the UNC
    // prefix (`\\server\share`), and collapsing it to one turns a network
    // path into a drive-relative one. HotSpot keeps it, which is also why it
    // answers `isAbsolute() == true` for `//`.
    let unc = swapped.len() >= 2 && swapped[0] == back && swapped[1] == back;
    let mut normalized: Vec<u16> = Vec::with_capacity(swapped.len());
    if unc {
        normalized.push(back);
        normalized.push(back);
    }
    let mut prev_sep = unc;
    for &c in swapped.iter().skip(usize::from(unc) * 2) {
        let is_sep = c == back;
        if !(is_sep && prev_sep) {
            normalized.push(c);
        }
        prev_sep = is_sep;
    }

    // DROP A REDUNDANT TRAILING SEPARATOR, but never turn a ROOT into the empty
    // string or into a bare drive letter. The roots are `\`, `X:\` and the UNC
    // prefix `\\`. The previous guard was `len() > 3`, which protects `C:\`
    // (len 3) but ALSO protects every short relative path -- so `new File("a/")`
    // kept its separator, and that one surviving character made `getName()`
    // answer "", `getParent()` answer "a", and
    // `new File("a/").equals(new File("a"))` answer FALSE where HotSpot says
    // true. A File that is not equal to itself-without-a-slash breaks any code
    // using it as a map key.
    let is_root = match normalized.len() {
        0 => true,
        1 => normalized[0] == back,
        2 => unc || normalized[1] == u16::from(b':'),
        3 => {
            normalized[1] == u16::from(b':')
                && normalized[2] == back
                && char::from_u32(u32::from(normalized[0])).is_some_and(|c| c.is_ascii_alphabetic())
        }
        _ => false,
    };
    if !is_root && normalized.last() == Some(&back) {
        normalized.pop();
    }
    normalized
}

#[cfg(not(windows))]
pub fn file_normalise_path(path: &str) -> String {
    // `UnixFileSystem.normalize`: collapse runs of `/` and drop a single
    // trailing `/` (but keep the root `/` itself). `File.<init>` stores the
    // NORMALIZED string, so `getPath()`/`getAbsolutePath()` observe it.
    //
    // Leaving it un-normalized let a trailing separator survive into
    // `getAbsolutePath()`, and Spring's
    // `PathMatchingResourcePatternResolver.retrieveMatchingFiles` builds its
    // glob as `rootDir.getAbsolutePath() + "/" + subPattern` — a root
    // directory that already ended in `/` produced `.../scanned//*.txt`, which
    // matches nothing (`core.io.support.PathMatchingResourcePatternResolverTests
    // .encodedHashtagInPath` found zero files). The root came straight from
    // `new File(uri.getSchemeSpecificPart())`, whose value legitimately ends
    // in `/` for a directory URL.
    let units: Vec<u16> = path.encode_utf16().collect();
    String::from_utf16_lossy(&file_normalise_path_units(&units))
}

/// [`file_normalise_path`] in code units — the implementation of both. The
/// doc comment above states the rule; this is where it lives.
#[cfg(not(windows))]
pub fn file_normalise_path_units(path: &[u16]) -> Vec<u16> {
    let slash = u16::from(b'/');
    let mut out: Vec<u16> = Vec::with_capacity(path.len());
    let mut prev_slash = false;
    for &c in path {
        let is_slash = c == slash;
        if !(is_slash && prev_slash) {
            out.push(c);
        }
        prev_slash = is_slash;
    }
    if out.len() > 1 && out.last() == Some(&slash) {
        out.pop();
    }
    out
}


/// Resolve `new File(parent, child)` the way the JDK's
/// `WinNTFileSystem`/`UnixFileSystem.resolve` does, rather than `PathBuf::push`
/// (whose `push("")` adds a trailing separator and `push("/")` discards the
/// parent — both Java-incompatible). Strip leading/trailing separators from the
/// child and a trailing one from the parent, join with one separator, then
/// normalise. An empty child (including a lone "/" ) → just the normalised
/// parent.
pub fn file_join_parent_child(parent: &str, child: &str) -> String {
    let p: Vec<u16> = parent.encode_utf16().collect();
    let c: Vec<u16> = child.encode_utf16().collect();
    String::from_utf16_lossy(&file_join_parent_child_units(&p, &c))
}

/// [`file_join_parent_child`] in code units — the implementation of both.
/// `java.io.File.isAbsolute()`, by the JDK's rule rather than Rust's.
///
/// `std::path::Path::is_absolute` is NOT the same predicate.
/// `WinNTFileSystem.isAbsolute` is
///
/// ```text
/// int pl = getPrefixLength();
/// return ((pl == 2) && path.charAt(0) == slash) || (pl == 3);
/// ```
///
/// i.e. a UNC path (prefix `\\`) is absolute and a bare drive-relative
/// `C:foo` is not. Rust wants a prefix AND a root, so it answered FALSE for a
/// UNC root -- `new File("//").isAbsolute()` was `false` against HotSpot's
/// `true` (MEASURED: `probes/FilePathSweep.java`), which then fabricated a
/// drive letter into `toURI()` (`file:/C://` against HotSpot `file:////`).
///
/// Deliberately NOT shared with `p57_win_is_absolute`, which serves
/// `java.nio.file.Path`: the two classes genuinely disagree here, and
/// `Path.isAbsolute` requires a root AND a drive.
pub fn file_is_absolute(path: &str) -> bool {
    let u: Vec<u16> = path.encode_utf16().collect();
    if cfg!(windows) {
        // prefix length 2, starting with a separator -> UNC
        if u.len() >= 2 && u_is_sep(u[0]) && u_is_sep(u[1]) {
            return true;
        }
        // prefix length 3 -> `X:` followed by a separator
        u.len() >= 3
            && char::from_u32(u32::from(u[0])).is_some_and(|c| c.is_ascii_alphabetic())
            && u[1] == u16::from(b':')
            && u_is_sep(u[2])
    } else {
        !u.is_empty() && u_is_sep(u[0])
    }
}

/// `java.io.File`'s absolutization -- `getAbsolutePath`'s rule, which is not
/// Rust's.
///
/// Three cases, and Rust's `Path` can express only the first and the third:
///
///  * already absolute by **`File`'s** test ([`file_is_absolute`]) -- returned
///    unchanged. Rust disagrees here: `Path::is_absolute` wants a prefix AND a
///    root, so a UNC path is "relative" to it and gets joined onto the working
///    directory, keeping the CWD's drive and discarding the server. That is
///    where `new File("//").toURI()` produced `file:/C://` against HotSpot's
///    `file:////`.
///  * **drive-relative** on Windows (`C:` or `C:foo`) -- a drive letter with no
///    root. Windows keeps a working directory PER DRIVE and this resolves
///    against that one, which is what `WinNTFileSystem` does. MEASURED against
///    HotSpot 25.0.3+9: `new File("C:").toURI()` is
///    `file:/C:/craton/CratonVM/`, the process's directory on C:, not
///    `file:/C:/`.
///  * everything else -- joined onto the process working directory.
///
/// It deliberately does NOT collapse `.` or `..`. `getAbsolutePath` is not
/// `getCanonicalPath`; HotSpot keeps `a\..\b`, and normalising here would move
/// rows that already agree (`probes/FilePathSweep.java`, `[a/../b]`).
pub fn file_absolutize(path: &str) -> String {
    if file_is_absolute(path) {
        return path.to_string();
    }
    #[cfg(windows)]
    {
        // `X:` / `X:rest` -- a drive letter NOT followed by a separator.
        // `file_is_absolute` has already rejected `X:\...`, so reaching here
        // with a drive prefix means drive-relative.
        let c: Vec<char> = path.chars().collect();
        if c.len() >= 2 && c[0].is_ascii_alphabetic() && c[1] == ':' {
            if let Some(drive_cwd) = win_drive_working_directory(c[0]) {
                let rest: String = c[2..].iter().collect();
                if rest.is_empty() {
                    return drive_cwd;
                }
                let sep = if drive_cwd.ends_with('\\') { "" } else { "\\" };
                return format!("{drive_cwd}{sep}{rest}");
            }
        }
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd
            .join(std::path::Path::new(path))
            .to_string_lossy()
            .into_owned(),
        Err(_) => path.to_string(),
    }
}

/// The working directory Windows keeps for one drive, asked for the way the
/// platform answers it: `GetFullPathNameW` on the bare `X:`.
///
/// Only ever reached for the drive-relative shape, so `GetFullPathNameW`'s
/// `.`/`..` collapsing cannot leak into an ordinary relative path -- which
/// would break `getAbsolutePath`, whose contract is explicitly not to
/// canonicalise.
#[cfg(windows)]
pub fn win_drive_working_directory(drive: char) -> Option<String> {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetFullPathNameW(
            lp_file_name: *const u16,
            n_buffer_length: u32,
            lp_buffer: *mut u16,
            lp_file_part: *mut *mut u16,
        ) -> u32;
    }
    let input: Vec<u16> = format!("{drive}:").encode_utf16().chain(Some(0)).collect();
    let mut buf = vec![0u16; 32768];
    // SAFETY: `input` is NUL-terminated and lives across the call, `buf` is
    // writable for `buf.len()` UTF-16 units, and the file-part out-parameter is
    // documented as optional (null).
    let n = unsafe {
        GetFullPathNameW(
            input.as_ptr(),
            buf.len() as u32,
            buf.as_mut_ptr(),
            std::ptr::null_mut(),
        )
    };
    if n == 0 || n as usize >= buf.len() {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..n as usize]))
}

pub fn file_join_parent_child_units(parent: &[u16], child: &[u16]) -> Vec<u16> {
    let mut cs = 0;
    let mut ce = child.len();
    while cs < ce && u_is_sep(child[cs]) {
        cs += 1;
    }
    while ce > cs && u_is_sep(child[ce - 1]) {
        ce -= 1;
    }
    let child_trim = &child[cs..ce];
    if child_trim.is_empty() {
        return file_normalise_path_units(parent);
    }
    if parent.is_empty() {
        // An EMPTY BUT NON-NULL parent is not "no parent". The JDK resolves
        // `File(String parent, String child)` as
        // `fs.resolve(fs.getDefaultParent(), fs.normalize(child))`, and
        // `getDefaultParent()` is `\` on Windows and `/` on Unix -- so
        // `new File("", "kid")` is `\kid`, ROOTED, not the relative `kid`
        // this returned. (MEASURED against HotSpot 25.0.3+9,
        // `probes/FilePathSweep.java`.) The `parent == null` case never reaches
        // here: that is the one-argument constructor's overload.
        let mut rooted: Vec<u16> = Vec::with_capacity(child_trim.len() + 1);
        rooted.push(u16::from(if cfg!(windows) { b'\\' } else { b'/' }));
        rooted.extend_from_slice(child_trim);
        return file_normalise_path_units(&rooted);
    }
    // NORMALISE THE PARENT FIRST, then decide whether it needs a separator.
    // Stripping the parent's trailing separators unconditionally destroys a
    // ROOT: for parent `\\\\` (the UNC prefix) it stripped both and left
    // nothing, so `new File("//", "kid")` came out `\\kid` where HotSpot says
    // `\\\\kid` -- a network path demoted to a driveless-rooted one.
    //
    // A normalised path ends in a separator ONLY when it is a root (`\\`,
    // `X:\\`, `\\\\`); `file_normalise_path_units` has just removed every
    // other trailing separator. So "ends in a separator" IS the root test, and
    // the rule is simply: append a separator unless the parent already supplies
    // one. That also covers `C:\\` + `kid` -> `C:\\kid` rather than `C:\\\\kid`.
    let parent_norm = file_normalise_path_units(parent);
    // A BARE DRIVE is a prefix but not a root, and the "ends in a separator"
    // test above cannot see the difference. `WinNTFileSystem.resolve` names
    // this case itself:
    //
    //     boolean isDirectoryRelative =
    //         pn == 2 && isLetter(parent.charAt(0)) && parent.charAt(1) == ':';
    //
    // and when it holds it copies the child straight after the parent with NO
    // separator. `C:kid` is "kid in the current directory ON drive C" and
    // `C:\kid` is "kid in the ROOT of drive C" — Windows keeps a working
    // directory per drive, so those name the same file only when the process's
    // directory on C: happens to be the root. MEASURED against HotSpot
    // 25.0.3+9, `probes/FilePathSweep.java`:
    //
    //   new File("C:", "kid").getPath()
    //     HotSpot   C:kid
    //     CratonVM  C:\kid
    //
    // Windows-only: on Unix `C:` is an ordinary relative name with no drive
    // meaning at all, and HotSpot answers `C:/kid` there.
    let parent_is_drive_relative = cfg!(windows)
        && parent_norm.len() == 2
        && parent_norm[1] == u16::from(b':')
        && char::from_u32(u32::from(parent_norm[0])).is_some_and(|c| c.is_ascii_alphabetic());
    let mut joined: Vec<u16> = parent_norm.clone();
    if !parent_is_drive_relative && !parent_norm.last().is_some_and(|&c| u_is_sep(c)) {
        joined.push(u16::from(b'/'));
    }
    joined.extend_from_slice(child_trim);
    file_normalise_path_units(&joined)
}

/// Strip the Windows `\\?\` / `\\?\UNC\` extended-length prefix that
/// `std::fs::canonicalize` prepends. The real JDK's `getCanonicalPath`
/// never returns a verbatim/UNC-prefixed path; leaving `\\?\` in place
/// makes a later `File.toURI()` produce `file://?/C:/...`, an invalid URL.
pub fn strip_unc(p: &str) -> String {
    if let Some(rest) = p.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = p.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        p.to_string()
    }
}

/// String-based path canonicalization via Win32 `GetFullPathNameW` — makes the
/// path absolute, collapses `.`/`..`, converts `/` to `\`, and strips a trailing
/// separator, all WITHOUT opening the file (so the `cpcrypt` filesystem filter is
/// never triggered, unlike `std::fs::canonicalize`/`GetFinalPathNameByHandleW`).
/// Returns `None` for an empty input or on API error so the caller can fall back
/// to the pure-Rust lexical normalizer. Does NOT resolve symlinks — matching
/// HotSpot's largely-string-based `WinNTFileSystem.canonicalize`.
#[cfg(windows)]
pub fn win_get_full_path_name(path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    extern "system" {
        fn GetFullPathNameW(
            lpFileName: *const u16,
            nBufferLength: u32,
            lpBuffer: *mut u16,
            lpFilePart: *mut *mut u16,
        ) -> u32;
    }
    // Normalize the URI-style leading `/C:` quirk and forward slashes first, so a
    // raw `/C:/foo` doesn't confuse GetFullPathName into a per-drive-relative join.
    let norm = file_normalise_path(path);
    let wide: Vec<u16> = std::ffi::OsStr::new(&norm)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        // First call (null buffer) returns the required length INCLUDING the NUL.
        let needed = GetFullPathNameW(wide.as_ptr(), 0, std::ptr::null_mut(), std::ptr::null_mut());
        if needed == 0 {
            return None;
        }
        let mut buf: Vec<u16> = vec![0u16; needed as usize];
        // Second call returns the length WITHOUT the NUL on success.
        let written = GetFullPathNameW(
            wide.as_ptr(),
            buf.len() as u32,
            buf.as_mut_ptr(),
            std::ptr::null_mut(),
        );
        if written == 0 || written as usize >= buf.len() {
            return None;
        }
        buf.truncate(written as usize);
        let full = std::ffi::OsString::from_wide(&buf)
            .to_string_lossy()
            .into_owned();
        // GetFullPathNameW preserves a trailing separator (`foo\bar\`), but the JDK's
        // canonicalize drops it (`foo\bar`). Strip a single trailing `\` unless the
        // result is a drive root (`C:\`) or a bare root (`\`), which must keep it.
        let trimmed = if full.ends_with('\\') && !full.ends_with(":\\") && full.len() > 1 {
            full.trim_end_matches('\\').to_string()
        } else {
            full
        };
        Some(trimmed)
    }
}

/// Case-correct each *existing* path component of an absolute,
/// `GetFullPathNameW`-normalized Windows path to match the real on-disk
/// filename casing — restoring the behaviour real HotSpot's
/// `WinNTFileSystem.canonicalize` provides (it resolves the true on-disk case
/// for the existing prefix of a path, leaving any non-existent tail as given).
///
/// `GetFullPathNameW` is purely lexical: it does not query the filesystem, so
/// `getCanonicalPath("dir/D1-F1.TXT")` on a case-insensitive volume would
/// otherwise echo back the caller's requested casing verbatim, even when the
/// real on-disk file is `d1-f1.txt`. Callers that compare the canonical path
/// against the requested path to detect case mismatches (e.g. Tomcat's
/// `AbstractFileResourceSet.file()`, which rejects a request whose case
/// doesn't match the real file to guard against case-insensitive-filesystem
/// false positives) would then never observe a mismatch — silently defeating
/// the check.
///
/// This queries a per-component **targeted** `FindFirstFileW` (exact name, no
/// wildcard) against each existing ancestor directory. `FindFirstFileW`
/// enumerates directory entries; it never opens the target file itself, so —
/// like `GetFullPathNameW` — it does not engage the `cpcrypt.dll` AppCompat
/// filesystem filter that motivated moving off `std::fs::canonicalize`/
/// `GetFinalPathNameByHandleW` (see the doc comment on `win_get_full_path_name`).
///
/// As JDK 25's `canonicalize_md.c` does (interpreter round i1 wave 52): the
/// drive letter is upper-cased, a UNC share root keeps no trailing separator,
/// and a probe of an existing directory's entry that fails with a REPORTABLE
/// error (`lastErrorReportable`: anything but not-found, not-a-directory,
/// access-denied and the network-path errors; in practice
/// `ERROR_INVALID_NAME` for a `:`, `*`, `?` or `|` in a component) is an
/// error, `Err(win32 code)`, which the caller raises as an `IOException`
/// with the system's text. A not-existing ancestor stops the probing, so
/// `C:\nope\a:b` is answered as given, as on HotSpot.
#[cfg(windows)]
pub fn win_case_correct(full: &str) -> Result<String, u32> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    #[repr(C)]
    struct FileTime {
        _low: u32,
        _high: u32,
    }
    #[repr(C)]
    struct Win32FindDataW {
        _attrs: u32,
        _creation: FileTime,
        _access: FileTime,
        _write: FileTime,
        _size_high: u32,
        _size_low: u32,
        _reserved0: u32,
        _reserved1: u32,
        file_name: [u16; 260],
        _alt_name: [u16; 14],
    }
    extern "system" {
        fn FindFirstFileW(
            lp_file_name: *const u16,
            lp_find_file_data: *mut Win32FindDataW,
        ) -> *mut std::ffi::c_void;
        fn FindClose(h_find_file: *mut std::ffi::c_void) -> i32;
        fn GetLastError() -> u32;
    }
    const INVALID_HANDLE_VALUE: *mut std::ffi::c_void = -1isize as *mut std::ffi::c_void;

    // Find the real on-disk name of `name` inside `dir` (both directory paths as
    // given, any case) via an exact-name `FindFirstFileW` probe. Returns
    // `Ok(None)` if the entry doesn't exist (or the error is one the JDK does
    // not report), `Err(code)` for a reportable error.
    fn find_real_name(dir: &str, name: &str) -> Result<Option<String>, u32> {
        // `canonicalize_md.c` `lastErrorReportable`: FILE_NOT_FOUND,
        // PATH_NOT_FOUND, ACCESS_DENIED, BAD_NETPATH, NETWORK_ACCESS_DENIED,
        // BAD_NET_NAME, DIRECTORY, NETWORK_UNREACHABLE.
        const UNREPORTED: [u32; 8] = [2, 3, 5, 53, 65, 67, 267, 1231];
        let probe = format!(r"{dir}\{name}");
        let wide: Vec<u16> = std::ffi::OsStr::new(&probe)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            let mut data: Win32FindDataW = std::mem::zeroed();
            let handle = FindFirstFileW(wide.as_ptr(), &mut data);
            if handle == INVALID_HANDLE_VALUE {
                let code = GetLastError();
                return if UNREPORTED.contains(&code) {
                    Ok(None)
                } else {
                    Err(code)
                };
            }
            FindClose(handle);
            let end = data
                .file_name
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(data.file_name.len());
            Ok(Some(
                std::ffi::OsString::from_wide(&data.file_name[..end])
                    .to_string_lossy()
                    .into_owned(),
            ))
        }
    }

    // `canonicalize_md.c` refuses a path holding a wildcard before it probes
    // anything (`wild`): `ERROR_INVALID_NAME`. A `\\?\` extended-length
    // prefix is not a wildcard.
    if full
        .strip_prefix(r"\\?\")
        .unwrap_or(full)
        .contains(['*', '?'])
    {
        return Err(123);
    }
    let path = std::path::Path::new(full);
    let mut components = path.components();
    let (prefix, unc) = match components.next() {
        Some(std::path::Component::Prefix(p)) => {
            let text = p.as_os_str().to_string_lossy().into_owned();
            match p.kind() {
                // `canonicalize_md.c` copies the drive letter `towupper`ed.
                std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_) => {
                    (text.to_uppercase(), false)
                }
                std::path::Prefix::UNC(..) | std::path::Prefix::VerbatimUNC(..) => (text, true),
                _ => (text, false),
            }
        }
        _ => return Ok(full.to_string()),
    };
    // Consume the RootDir component that follows a drive/UNC prefix, if present.
    let mut current = prefix;
    let had_root = matches!(
        components.clone().next(),
        Some(std::path::Component::RootDir)
    );
    if had_root {
        components.next();
        current.push('\\');
    }
    let mut still_existing = true;
    let mut first_comp = true;
    for comp in components {
        let std::path::Component::Normal(name) = comp else {
            continue; // GetFullPathNameW already collapsed `.`/`..`
        };
        let name = name.to_string_lossy();
        let probe_dir = current.trim_end_matches('\\').to_string();
        if !current.ends_with('\\') && !first_comp {
            current.push('\\');
        }
        first_comp = false;
        if still_existing {
            if let Some(real) = find_real_name(&probe_dir, &name)? {
                current.push_str(&real);
                continue;
            }
            still_existing = false;
        }
        current.push_str(&name);
    }
    // A UNC share root (`\\host\share\`) is answered without its separator.
    if unc && first_comp && current.ends_with('\\') {
        current.pop();
    }
    Ok(current)
}

/// The system's text for a Win32 error code, trimmed as the JDK's
/// `getLastErrorString` trims it (a trailing `\n`, `\r` and `.` dropped from a
/// message longer than 3 units), or `None` when the code has none.
#[cfg(windows)]
pub fn win32_error_text(code: u32) -> Option<String> {
    use std::ffi::c_void;
    extern "system" {
        fn FormatMessageW(
            flags: u32,
            source: *const c_void,
            message_id: u32,
            language_id: u32,
            buffer: *mut u16,
            size: u32,
            arguments: *mut c_void,
        ) -> u32;
    }
    const FORMAT_MESSAGE_FROM_SYSTEM: u32 = 0x0000_1000;
    const FORMAT_MESSAGE_IGNORE_INSERTS: u32 = 0x0000_0200;
    let mut buf = [0u16; 512];
    // SAFETY: `buf` is a live buffer whose length is passed; no source, no
    // arguments.
    let mut len = unsafe {
        FormatMessageW(
            FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
            std::ptr::null(),
            code,
            0,
            buf.as_mut_ptr(),
            buf.len() as u32,
            std::ptr::null_mut(),
        )
    } as usize;
    if len == 0 {
        return None;
    }
    if len > 3 {
        if buf[len - 1] == u16::from(b'\n') {
            len -= 1;
        }
        if buf[len - 1] == u16::from(b'\r') {
            len -= 1;
        }
        if buf[len - 1] == u16::from(b'.') {
            len -= 1;
        }
    }
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// Canonicalize a `java.io.File` path the way `File.getCanonicalPath()` does.
///
/// `std::fs::canonicalize` only works for paths that *exist* on disk; the real
/// JDK's `getCanonicalPath` also normalizes non-existent paths — it makes them
/// absolute, collapses `.`/`..` segments, and normalizes separators. The old
/// fallback returned a raw absolute path with `..` segments intact, so a
/// containment check (`child.startsWith(parentDir)`) — as Felix's
/// `getDataFile` does — would spuriously fail.
pub fn file_canonicalize_path(path: &str) -> Result<String, MethodCallFailed> {
    // JDK-faithful canonicalization cache. The real `WinNTFileSystem.canonicalize`
    // fronts a 30s `ExpiringCache` for exactly this reason: Tomcat (and most apps)
    // re-resolve the same docBase/appBase/work/temp paths on every start, and each
    // `std::fs::canonicalize` here OPENS the file → `GetFinalPathNameByHandleW`,
    // which on this box can stall for seconds inside the Crypto Pro `cpcrypt.dll`
    // filesystem filter. Caching keeps repeated `File.getCanonicalPath()` off the
    // filesystem (and the filter), cutting both per-call latency and the stall
    // variance that otherwise pushes a heavy parameterized Tomcat-churn test
    // (~144 full start/stop cycles) past its timeout.
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};
    static CACHE: OnceLock<Mutex<std::collections::HashMap<String, (String, Instant)>>> =
        OnceLock::new();
    const TTL: Duration = Duration::from_secs(30);
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    if let Some((canon, at)) = cache.lock().unwrap().get(path) {
        if at.elapsed() < TTL {
            return Ok(canon.clone());
        }
    }
    let result = file_canonicalize_path_uncached(path)?;
    {
        let mut c = cache.lock().unwrap();
        // Bound memory: a long-running app may canonicalize many distinct paths.
        if c.len() > 8192 {
            c.clear();
        }
        c.insert(path.to_string(), (result.clone(), Instant::now()));
    }
    Ok(result)
}

/// Resolve symlinks in the longest existing ancestor of `path` (via repeated
/// `std::fs::canonicalize` on shrinking prefixes, i.e. `realpath`), then
/// re-append whatever nonexistent trailing components were stripped off,
/// literally and lexically `.`/`..`-collapsed. Mirrors the real JDK's
/// `UnixFileSystem.canonicalize0` behavior for a path that doesn't fully
/// exist on disk (`realpath -m` semantics), unlike a pure string-only
/// normalization which never touches the filesystem and so can't resolve
/// symlinks at all. Returns `None` only if `path` can't even be made
/// absolute (no CWD available for a relative path).
#[cfg(not(windows))]
pub fn resolve_existing_ancestor_then_literal_tail(path: &str) -> Option<String> {
    let norm = file_normalise_path(path);
    let p = std::path::Path::new(&norm);
    let abs: std::path::PathBuf = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(p)
    };
    use std::path::Component;
    let mut comps: Vec<std::ffi::OsString> = Vec::new();
    for comp in abs.components() {
        match comp {
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
            Component::ParentDir => {
                comps.pop();
            }
            Component::Normal(s) => comps.push(s.to_os_string()),
        }
    }
    // Shrink from the immediate parent of the (lexically-collapsed) full path
    // down to just "/", canonicalizing each candidate ancestor, and re-append
    // whichever trailing components had to be stripped off once one resolves.
    // (The full path itself was already tried, via `std::fs::canonicalize`,
    // by the caller before falling back to this function.) "/" itself always
    // canonicalizes, so this always terminates with `Some`.
    for split in (0..comps.len()).rev() {
        let mut candidate = std::path::PathBuf::from("/");
        for c in &comps[..split] {
            candidate.push(c);
        }
        if let Ok(canon) = std::fs::canonicalize(&candidate) {
            let mut result = canon;
            for c in &comps[split..] {
                result.push(c);
            }
            return Some(result.to_string_lossy().into_owned());
        }
    }
    Some("/".to_string())
}

pub fn file_canonicalize_path_uncached(path: &str) -> Result<String, MethodCallFailed> {
    #[cfg(windows)]
    {
        // DEFAULT on Windows: canonicalize the path as a STRING via `GetFullPathNameW`,
        // which makes it absolute (against the cwd / per-drive cwd), collapses `.`/`..`,
        // normalizes separators, and drops trailing separators — all WITHOUT opening the
        // file. This mirrors HotSpot's `WinNTFileSystem.canonicalize`, which is largely
        // string-based (`GetFullPathName`) and, like us, does NOT fully resolve symlinks
        // the way `realpath`/`std::fs::canonicalize` does.
        //
        // Why this matters here: `std::fs::canonicalize` opens the file
        // (`GetFinalPathNameByHandleW` → `NtQueryInformationFile`), which on a box with
        // the Crypto Pro `cpcrypt.dll` AppCompat filesystem filter loaded triggers the
        // filter on EVERY `File.getCanonicalPath()` and can stall for ~30s globally.
        // `GetFullPathNameW` touches no file handle, so the filter is never engaged.
        // HotSpot is immune for the same reason. (cf. memory notes
        // tomcat_dohead_speed_oncpu_not_shutdown / tomcat_dohead_gc_safepoint_deadlock.)
        //
        // Escape hatch: set `CRATONVM_CANON_OPENFILE=1` to restore the old, symlink-
        // resolving, file-opening behavior if an app genuinely needs realpath semantics.
        if crate::nbflags().canon_openfile {
            if let Ok(c) = std::fs::canonicalize(path) {
                return Ok(strip_unc(&c.to_string_lossy()));
            }
        } else if let Some(full) = win_get_full_path_name(path) {
            return match win_case_correct(&full) {
                Ok(c) => Ok(strip_unc(&c)),
                // `JNU_ThrowIOExceptionWithLastError(env, "Bad pathname")`.
                Err(code) => Err(RuntimeError::IOException {
                    message: win32_error_text(code).unwrap_or_else(|| "Bad pathname".to_string()),
                }
                .into()),
            };
        }
        // GetFullPathNameW failed (empty input / API error) — fall through to the
        // pure-Rust lexical normalization below.
    }
    #[cfg(not(windows))]
    {
        // Non-Windows has no cpcrypt-style filter hazard, so keep the symlink-resolving
        // filesystem call for existing paths.
        if let Ok(c) = std::fs::canonicalize(path) {
            return Ok(strip_unc(&c.to_string_lossy()));
        }
        // `path` (or its final component(s)) doesn't exist yet — e.g. a file about
        // to be created by `WebResourceRoot.write()`/`DirResourceSet.write()`.
        // `std::fs::canonicalize` (== `realpath`) fails outright in that case, but
        // falling through to pure lexical normalization below would silently
        // UN-resolve any symlink in an EXISTING ancestor directory. That breaks any
        // `child.startsWith(canonicalBase)`-style containment check where
        // `canonicalBase` was itself computed by successfully canonicalizing an
        // existing directory through a symlink (e.g. Tomcat's
        // `AbstractFileResourceSet.file()`, or java.io.File's own canonical-path
        // contract): the base resolves through the symlink, the not-yet-existing
        // child doesn't, and the prefix check spuriously fails — a real bug,
        // confirmed via `TestWebdavServlet`'s PUT-to-a-symlinked-fixture-root
        // 201-vs-409 regression cluster. Real JDK's `UnixFileSystem.canonicalize0`
        // resolves symlinks in the longest EXISTING ancestor and appends the
        // nonexistent tail literally (`realpath -m` semantics) — do the same here.
        if let Some(resolved) = resolve_existing_ancestor_then_literal_tail(path) {
            return Ok(strip_unc(&resolved));
        }
    }
    // Lexical fallback: normalize the path as a string without touching the filesystem.
    // Make absolute against CWD, collapse `.`/`..`, normalize separators.
    let norm = file_normalise_path(path);
    let p = std::path::Path::new(&norm);
    let abs: std::path::PathBuf = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    };
    // Collapse `.` and `..` segments lexically (the prefix/root are preserved).
    use std::path::Component;
    let mut out: Vec<Component> = Vec::new();
    for comp in abs.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                // Pop the last normal segment, but never past the root/prefix.
                match out.last() {
                    Some(Component::Normal(_)) => {
                        out.pop();
                    }
                    _ => out.push(comp),
                }
            }
            other => out.push(other),
        }
    }
    let mut result = std::path::PathBuf::new();
    for comp in out {
        result.push(comp.as_os_str());
    }
    Ok(strip_unc(&result.to_string_lossy()))
}

/// Query real OS disk-space stats for the volume containing `path`, matching
/// HotSpot's `File.getTotalSpace()`/`getFreeSpace()`/`getUsableSpace()`
/// contract: returns `None` (callers report `0`) if `path` does not name an
/// existing file or directory — real HotSpot does the same rather than
/// reporting the containing volume's space for a nonexistent path (see
/// `DiskSpaceHealthIndicatorTests.whenPathDoesNotExistDiskSpaceIsDown`).
/// Returns `(total, free, usable)` in bytes on success.
#[cfg(windows)]
pub fn file_disk_space_bytes(path: &str) -> Option<(u64, u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    let dir_path = match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => path.to_string(),
        Ok(_) => {
            let full = win_get_full_path_name(path)?;
            std::path::Path::new(&full)
                .parent()?
                .to_string_lossy()
                .into_owned()
        }
        Err(_) => return None,
    };
    extern "system" {
        fn GetDiskFreeSpaceExW(
            lpDirectoryName: *const u16,
            lpFreeBytesAvailableToCaller: *mut u64,
            lpTotalNumberOfBytes: *mut u64,
            lpTotalNumberOfFreeBytes: *mut u64,
        ) -> i32;
    }
    let wide: Vec<u16> = std::ffi::OsStr::new(&dir_path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut usable: u64 = 0;
    let mut total: u64 = 0;
    let mut free: u64 = 0;
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut usable, &mut total, &mut free) };
    if ok == 0 {
        return None;
    }
    Some((total, free, usable))
}

#[cfg(not(windows))]
pub fn file_disk_space_bytes(path: &str) -> Option<(u64, u64, u64)> {
    if std::fs::metadata(path).is_err() {
        return None;
    }
    let c_path = std::ffi::CString::new(path).ok()?;
    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &mut stat) != 0 {
            return None;
        }
        let block_size = stat.f_frsize as u64;
        let total = block_size * stat.f_blocks as u64;
        let free = block_size * stat.f_bfree as u64;
        let usable = block_size * stat.f_bavail as u64;
        Some((total, free, usable))
    }
}

/// `FileStore.getBlockSize()` — the volume's allocation/transfer unit, i.e.
/// `statvfs.f_frsize` on Unix and `GetDiskFreeSpaceW`'s bytes-per-sector on
/// Windows (exactly what the JDK's `UnixFileStore`/`WindowsFileStore` report).
/// `None` when the volume cannot be queried; callers fall back to 4 KiB.
#[cfg(windows)]
pub fn file_store_block_size(path: &str) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    let full = win_get_full_path_name(path).unwrap_or_else(|| path.to_string());
    // Build the volume root (`C:\`, `\\server\share\`) GetDiskFreeSpaceW wants.
    let mut root = String::new();
    for comp in std::path::Path::new(&full).components() {
        match comp {
            std::path::Component::Prefix(prefix) => {
                root.push_str(&prefix.as_os_str().to_string_lossy());
            }
            std::path::Component::RootDir => root.push('\\'),
            _ => break,
        }
    }
    if root.is_empty() {
        return None;
    }
    if !root.ends_with('\\') {
        root.push('\\');
    }
    extern "system" {
        fn GetDiskFreeSpaceW(
            lpRootPathName: *const u16,
            lpSectorsPerCluster: *mut u32,
            lpBytesPerSector: *mut u32,
            lpNumberOfFreeClusters: *mut u32,
            lpTotalNumberOfClusters: *mut u32,
        ) -> i32;
    }
    let wide: Vec<u16> = std::ffi::OsStr::new(&root)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut sectors_per_cluster: u32 = 0;
    let mut bytes_per_sector: u32 = 0;
    let mut free_clusters: u32 = 0;
    let mut total_clusters: u32 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceW(
            wide.as_ptr(),
            &mut sectors_per_cluster,
            &mut bytes_per_sector,
            &mut free_clusters,
            &mut total_clusters,
        )
    };
    if ok == 0 || bytes_per_sector == 0 {
        return None;
    }
    Some(u64::from(bytes_per_sector))
}

#[cfg(not(windows))]
pub fn file_store_block_size(path: &str) -> Option<u64> {
    let c_path = std::ffi::CString::new(path).ok()?;
    // SAFETY: `stat` is a POD out-parameter and `c_path` is NUL-terminated and
    // live for the call.
    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &mut stat) != 0 {
            return None;
        }
        let block_size = stat.f_frsize as u64;
        if block_size == 0 {
            None
        } else {
            Some(block_size)
        }
    }
}

/// `FileStore.isReadOnly()` — whether the VOLUME holding `path` is mounted
/// read-only. This is the mount flag, not a per-file permission bit: Unix
/// tests `ST_RDONLY` (value 1 on Linux and the BSDs) in `statvfs.f_flag`, the
/// same bit `sun.nio.fs.UnixFileStore.isReadOnly()` reads out of the mount
/// entry; Windows tests `FILE_READ_ONLY_VOLUME` in `GetVolumeInformationW`'s
/// filesystem flags, exactly as `sun.nio.fs.WindowsFileStore.isReadOnly()`
/// does. `None` when the volume cannot be queried at all.
#[cfg(windows)]
pub fn file_store_is_read_only(path: &str) -> Option<bool> {
    use std::os::windows::ffi::OsStrExt;
    const FILE_READ_ONLY_VOLUME: u32 = 0x0008_0000;
    let full = win_get_full_path_name(path).unwrap_or_else(|| path.to_string());
    // Same volume-root derivation as `file_store_block_size` above.
    let mut root = String::new();
    for comp in std::path::Path::new(&full).components() {
        match comp {
            std::path::Component::Prefix(prefix) => {
                root.push_str(&prefix.as_os_str().to_string_lossy());
            }
            std::path::Component::RootDir => root.push('\\'),
            _ => break,
        }
    }
    if root.is_empty() {
        return None;
    }
    if !root.ends_with('\\') {
        root.push('\\');
    }
    extern "system" {
        fn GetVolumeInformationW(
            lpRootPathName: *const u16,
            lpVolumeNameBuffer: *mut u16,
            nVolumeNameSize: u32,
            lpVolumeSerialNumber: *mut u32,
            lpMaximumComponentLength: *mut u32,
            lpFileSystemFlags: *mut u32,
            lpFileSystemNameBuffer: *mut u16,
            nFileSystemNameSize: u32,
        ) -> i32;
    }
    let wide: Vec<u16> = std::ffi::OsStr::new(&root)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut flags: u32 = 0;
    // SAFETY: `wide` is NUL-terminated and live for the call; every other
    // out-parameter is either null (not requested) or a live `u32`.
    let ok = unsafe {
        GetVolumeInformationW(
            wide.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut flags,
            std::ptr::null_mut(),
            0,
        )
    };
    if ok == 0 {
        return None;
    }
    Some(flags & FILE_READ_ONLY_VOLUME != 0)
}

#[cfg(not(windows))]
pub fn file_store_is_read_only(path: &str) -> Option<bool> {
    // `ST_RDONLY` is 1 on Linux/macOS/BSD; spelled literally rather than via
    // `libc::ST_RDONLY`, which is not exported for every unix target.
    const ST_RDONLY_BIT: u64 = 1;
    let probe = if path.is_empty() { "/" } else { path };
    let c_path = std::ffi::CString::new(probe).ok()?;
    // SAFETY: `stat` is a POD out-parameter and `c_path` is NUL-terminated and
    // live for the call.
    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &mut stat) != 0 {
            return None;
        }
        Some(stat.f_flag as u64 & ST_RDONLY_BIT != 0)
    }
}

/// `FileSystem.getNameMax0(path)` — the longest single name component the
/// volume holding `path` accepts. Unix answers with `pathconf(_PC_NAME_MAX)`,
/// exactly like the JDK's `UnixFileSystem`. Every Windows volume type
/// (NTFS/FAT32/exFAT) reports 255, which is also the fallback when the query
/// fails or the path does not exist.
pub fn file_system_name_max(path: &str) -> i32 {
    #[cfg(unix)]
    {
        let probe = if path.is_empty() { "/" } else { path };
        if let Ok(c_path) = std::ffi::CString::new(probe) {
            // SAFETY: `c_path` is NUL-terminated and live for the call;
            // `pathconf` writes nothing back.
            let n = unsafe { libc::pathconf(c_path.as_ptr(), libc::_PC_NAME_MAX) } as i64;
            if n > 0 {
                return n.min(i64::from(i32::MAX)) as i32;
            }
        }
    }
    255
}

/// Windows file identity — `(dwVolumeSerialNumber, nFileIndexHigh,
/// nFileIndexLow)` from `GetFileInformationByHandle`, i.e. what the JDK's
/// `WindowsFileKey` wraps. `FILE_FLAG_BACKUP_SEMANTICS` is what lets a
/// DIRECTORY be opened, and directories are exactly where `fileKey()` matters
/// (`FileTreeWalker.wouldLoop`'s symlink-loop check). `dwDesiredAccess = 0` is
/// enough for a metadata query and does not need read rights on the file.
#[cfg(windows)]
pub fn win_file_identity(path: &str) -> Option<(u32, u32, u32)> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;

    #[repr(C)]
    struct WinFiletime {
        dw_low_date_time: u32,
        dw_high_date_time: u32,
    }
    /// Mirror of Win32 `BY_HANDLE_FILE_INFORMATION` (ABI-fixed field order).
    #[repr(C)]
    struct ByHandleFileInformation {
        dw_file_attributes: u32,
        ft_creation_time: WinFiletime,
        ft_last_access_time: WinFiletime,
        ft_last_write_time: WinFiletime,
        dw_volume_serial_number: u32,
        n_file_size_high: u32,
        n_file_size_low: u32,
        n_number_of_links: u32,
        n_file_index_high: u32,
        n_file_index_low: u32,
    }
    #[link(name = "Kernel32")]
    extern "system" {
        // Declared over `*mut u8` so it matches `win32_register_link_natives`'s
        // declaration of the same symbol (`clashing-extern-declarations`).
        fn GetFileInformationByHandle(
            h_file: *mut std::ffi::c_void,
            lp_file_information: *mut u8,
        ) -> i32;
    }

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let file = std::fs::OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .ok()?;
    // SAFETY: `info` is POD and fully written by the OS on success; the handle
    // is owned by `file` and outlives the call.
    let mut info: ByHandleFileInformation = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetFileInformationByHandle(
            file.as_raw_handle() as *mut _,
            std::ptr::addr_of_mut!(info).cast::<u8>(),
        )
    };
    if ok == 0 {
        return None;
    }
    Some((
        info.dw_volume_serial_number,
        info.n_file_index_high,
        info.n_file_index_low,
    ))
}

/// [`file_alloc`] in code units.
///
/// `getParentFile` used to build its answer with `file_alloc`, i.e. through a
/// `String`, which lost a surrogate-bearing path (G78-1 §4). It now has no
/// reason to: the parent is computed in units and stays in units.
pub fn file_alloc_units(
    ctx: &mut dyn NativeContext,
    path: &[u16],
) -> Result<ObjectRef, MethodCallFailed> {
    let w = cratonvm_native_api::file_layout::alloc_width(ctx);
    let obj = try_alloc_concurrent_synthetic(ctx, "java/io/File", w)?;
    // Pin across the create_string below — a moving young GC there would
    // relocate the fresh File (native stale-local family).
    let obj_pin = ctx.pin_native_root(obj);
    let s = ctx.create_string_from_units(path);
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.set_field(obj, 0, Value::Object(Some(s)));
    let text = String::from_utf16_lossy(path);
    cratonvm_native_api::file_layout::write(ctx, obj, s, &text);
    ctx.unpin_native_roots(obj_pin);
    Ok(obj)
}

/// `new File(URI)`'s six refusals, in the constructor's own order.
///
/// ```text
/// if (!uri.isAbsolute())          throw new IllegalArgumentException("URI is not absolute");
/// if (uri.isOpaque())             throw new IllegalArgumentException("URI is not hierarchical");
/// if (scheme == null || !scheme.equalsIgnoreCase("file"))
///                                 throw new IllegalArgumentException("URI scheme is not \"file\"");
/// if (uri.getAuthority() != null) throw new IllegalArgumentException("URI has an authority component");
/// if (uri.getFragment() != null)  throw new IllegalArgumentException("URI has a fragment component");
/// if (uri.getQuery() != null)     throw new IllegalArgumentException("URI has a query component");
/// ```
///
/// MEASURED before the fix (`probes/L4FileSweep.java`): `new File(new
/// URI("foo/bar"))`, `new File(new URI("http://x/y"))` and `new File((URI)
/// null)` all constructed a `File` rather than refusing. The first two are the
/// dangerous pair -- a relative or non-`file` URI produced a `File` whose path
/// is a fragment of somebody else's namespace, and the caller learns that only
/// when `exists()` answers false somewhere else entirely.
///
/// The scan works on the URI's raw TEXT rather than on `getScheme()` /
/// `getAuthority()`, because the constructor already has to cope with the
/// synthetic 7-slot URIs whose accessors may not be wired -- and a check that
/// cannot be grounded must not invent a refusal. An unreadable text answers
/// `None`, which is the pre-existing behaviour exactly.
///
/// The separator tests use [`u_is_sep_either`] rather than `/` alone, so the
/// mangled `file:\C:\...` spelling this constructor is documented to receive
/// keeps working instead of being newly rejected as opaque — on a Linux host
/// too, where [`u_is_sep`] is `/` only.
pub fn file_uri_reject(raw: &[u16]) -> Option<&'static str> {
    if raw.is_empty() {
        return None;
    }
    let colon = u16::from(b':');
    let hash = u16::from(b'#');
    let quest = u16::from(b'?');
    // A scheme is the text before the first `:`, and only if no separator,
    // query or fragment delimiter comes first -- otherwise the URI is relative.
    let stop = raw
        .iter()
        .position(|&c| u_is_sep_either(c) || c == quest || c == hash);
    let scheme_end = match (raw.iter().position(|&c| c == colon), stop) {
        (Some(i), Some(j)) if i > j => return Some("URI is not absolute"),
        (Some(0), _) | (None, _) => return Some("URI is not absolute"),
        (Some(i), _) => i,
    };
    let scheme = String::from_utf16_lossy(&raw[..scheme_end]);
    if !scheme.eq_ignore_ascii_case("file") {
        return Some("URI scheme is not \"file\"");
    }
    let rest = &raw[scheme_end + 1..];
    // Opaque: the scheme-specific part does not begin with a separator.
    if !rest.first().is_some_and(|&c| u_is_sep_either(c)) {
        return Some("URI is not hierarchical");
    }
    if rest.len() >= 2 && u_is_sep_either(rest[1]) {
        let after = &rest[2..];
        let alen = after
            .iter()
            .position(|&c| u_is_sep_either(c))
            .unwrap_or(after.len());
        // An EMPTY authority (`file:///x`) is `getAuthority() == null`, which
        // is the ordinary spelling and must not be refused.
        if alen > 0 {
            return Some("URI has an authority component");
        }
    }
    if rest.contains(&hash) {
        return Some("URI has a fragment component");
    }
    if rest.contains(&quest) {
        return Some("URI has a query component");
    }
    None
}

/// Allocate a new File synthetic with the given path.
pub fn file_alloc(
    ctx: &mut dyn NativeContext,
    path: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let w = cratonvm_native_api::file_layout::alloc_width(ctx);
    let obj = try_alloc_concurrent_synthetic(ctx, "java/io/File", w)?;
    // Pin across the create_string below — a moving young GC there would
    // relocate the fresh File (native stale-local family).
    let obj_pin = ctx.pin_native_root(obj);
    let s = ctx.create_string(path);
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.set_field(obj, 0, Value::Object(Some(s)));
    cratonvm_native_api::file_layout::write(ctx, obj, s, path);
    ctx.unpin_native_roots(obj_pin);
    Ok(obj)
}

pub fn register_phase57_file(r: &mut NativeMethodRegistry) {
            let file = "java/io/File";

    // <init>(String path)V
    //
    // Real JDK's File.<init>(String) runs `FileSystem.normalize(...)` which
    // on Windows strips a leading `/` before a drive letter — e.g. the
    // URI-style `/C:/Users/foo` produced by `URL.toURI().getSchemeSpecificPart()`
    // round-trips to `C:\Users\foo`. Spring Boot's fat-jar launcher relies
    // on that round-trip in Archive.create(File). Apply the same
    // normalisation so our synthetic File matches HotSpot semantics.
    r.register(file, "<init>", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // UNITS (G70-1 N1): the normalised path is written straight back into
        // a Java field, so this is a round trip, not an inspection.
        //
        // `File(String)` opens `if (pathname == null) throw new
        // NullPointerException();`. Reading a null argument as the empty path
        // instead built `new File("")` -- an object that answers `exists()`
        // false, `getName()` "" and `getAbsolutePath()` the working directory,
        // so the caller's mistake surfaces arbitrarily far from where it was
        // made. MEASURED: HotSpot NPE, this VM no-throw.
        let path = match args.get(1) {
            Some(Value::Object(Some(s))) => ctx.read_string_units(*s).unwrap_or_default(),
            Some(Value::Object(None)) => {
                return Err(RuntimeError::NullPointerException { message: None }.into())
            }
            _ => Vec::new(),
        };
        let normalised = file_normalise_path_units(&path);
        // Pin across the create_string below — a moving young GC there would
        // relocate `this` (native stale-local family).
        let this_pin = ctx.pin_native_root(this);
        let s = ctx.create_string_from_units(&normalised);
        let this = ctx.read_native_pin(this_pin, this);
        ctx.set_field(this, 0, Value::Object(Some(s)));
        ctx.unpin_native_roots(this_pin);
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // <init>(String parent, String child)V
    r.register(
        file,
        "<init>",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // A null PARENT and an empty parent are different, and this body
            // treated them alike. The JDK's two branches are
            //   parent == null  ->  path = fs.normalize(child)
            //   parent == ""    ->  path = fs.resolve(fs.getDefaultParent(), ..)
            // so `new File((String) null, "c")` is "c" while `new File("", "c")`
            // is "/c". MEASURED: this VM answered "/c" to both, i.e. it moved a
            // relative path to the filesystem ROOT.
            let parent = match args.get(1) {
                Some(Value::Object(Some(s))) => Some(ctx.read_string_units(*s).unwrap_or_default()),
                Some(Value::Object(None)) => None,
                _ => Some(Vec::new()),
            };
            // `if (child == null) throw new NullPointerException();`
            let child = match args.get(2) {
                Some(Value::Object(Some(s))) => ctx.read_string_units(*s).unwrap_or_default(),
                Some(Value::Object(None)) => {
                    return Err(RuntimeError::NullPointerException { message: None }.into())
                }
                _ => Vec::new(),
            };
            // JDK `File(String parent, String child)` resolution — NOT `PathBuf::push`,
            // whose semantics differ from Java and broke `File`-based resource lookup
            // (the whole catalina.webresources cluster): `push("")` appends a trailing
            // separator (path no longer denotes the file → exists() false) and
            // `push("/")` treats the child as absolute and discards the parent. Match
            // Java: strip leading/trailing separators from the child, a trailing one
            // from the parent, join with a separator, then normalise (slash
            // conversion + collapse). Empty child → just the normalised parent.
            let path = match &parent {
                Some(p) => file_join_parent_child_units(p, &child),
                None => file_normalise_path_units(&child),
            };
            // Pin across the create_string below — a moving young GC there
            // would relocate `this` (native stale-local family).
            let this_pin = ctx.pin_native_root(this);
            let s = ctx.create_string_from_units(&path);
            let this = ctx.read_native_pin(this_pin, this);
            ctx.set_field(this, 0, Value::Object(Some(s)));
            ctx.unpin_native_roots(this_pin);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // <init>(File parent, String child)V
    r.register(
        file,
        "<init>",
        "(Ljava/io/File;Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Same two branches as the (String, String) form above: a null
            // parent normalises the child, it does not resolve it against the
            // default parent.
            let parent_path = match args.get(1) {
                Some(Value::Object(Some(p))) => Some(file_read_path_units(ctx, *p)),
                Some(Value::Object(None)) => None,
                _ => Some(Vec::new()),
            };
            let child = match args.get(2) {
                Some(Value::Object(Some(s))) => ctx.read_string_units(*s).unwrap_or_default(),
                Some(Value::Object(None)) => {
                    return Err(RuntimeError::NullPointerException { message: None }.into())
                }
                _ => Vec::new(),
            };
            let path = match &parent_path {
                Some(p) => file_join_parent_child_units(p, &child),
                None => file_normalise_path_units(&child),
            };
            // Pin across the create_string below — a moving young GC there
            // would relocate `this` (native stale-local family).
            let this_pin = ctx.pin_native_root(this);
            let s = ctx.create_string_from_units(&path);
            let this = ctx.read_native_pin(this_pin, this);
            ctx.set_field(this, 0, Value::Object(Some(s)));
            ctx.unpin_native_roots(this_pin);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // <init>(URI)V — `new File(file:/path)`.
    //
    // The previous implementation read the URI string from raw slot 5, but
    // that slot is the `port` int in the real `java.net.URI` layout — the
    // result was an empty path. ActiveMQ's launcher locates ACTIVEMQ_HOME
    // via `new File(new URI(jarUrl).resolve(".."))`; an empty path there
    // forced a wrong `../.` fallback and broke lib/*.jar discovery.
    //
    // Read the URI's `path` field *by name* (slot-order safe), falling back
    // to parsing the cached `string` full-text field. Then apply the
    // `WinNTFileSystem.fromURIPath` transform (strip the leading `/` before
    // a drive letter, drop a trailing `/`) and normalise separators.
    r.register(file, "<init>", "(Ljava/net/URI;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let uri = match args.get(1) {
            Some(Value::Object(Some(u))) => *u,
            // `File(URI)` opens with `uri.isAbsolute()`, so a null URI is an
            // NPE from the receiver. Minting an empty-path File instead handed
            // the caller an object that silently denotes the working directory.
            _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
        };
        // Prefer the parsed `path` component — with the slot-collision guard:
        // synthetic 7-slot URIs (URL.toURI) answer the by-name "path" read
        // with the REAL class's field index, which lands on the raw-string
        // slot, so the value equals the ENTIRE URI text ("file:/C:/...").
        // A real hierarchical path never carries the scheme prefix; treat
        // that case as unset and parse from the raw text instead (the
        // mangled "file:\C:\..." Files emptied Gradle's ClasspathUtil walk
        // and with it every ProjectBuilder module classpath).
        // Real `File(URI)` is `String p = uri.getPath();` — and `getPath()`
        // returns the DECODED path, while the `path` FIELD holds the raw,
        // still-percent-encoded one (`getRawPath()`'s value). Reading the field
        // therefore produced `/tmp/dir/resource%23test1.txt` for a file
        // genuinely named `resource#test1.txt`, so `exists()` was false for any
        // path containing a character `File.toURI()` had escaped. Spring's
        // `PathMatchingResourcePatternResolver` round-trips through
        // `new File(url.toURI())` while walking a directory, so a single `#` in
        // a resource name made the whole wildcard scan return nothing
        // (`core.io.support.PathMatchingResourcePatternResolverTests
        // .encodedHashtagInPath`).
        //
        // Ask the URI itself, so the decoding rules stay the JDK's. The field
        // read remains as the fallback for the synthetic 7-slot URIs described
        // below, whose `getPath()` may not be wired.
        // UNITS (G78-1). This is the FOURTH File constructor, and the audit did
        // not reach it: it was invoked zero times across the corpus, so it never
        // entered the live set. G70-1 N2 warns that a sibling sitting next to a
        // fixed method is not thereby fixed — a probe row was added to ask
        // rather than assume, and it diverged. `URI.getPath()` is exact since
        // G75-1 N1, so by this point the ONLY remaining loss was this
        // constructor's own read of the answer.
        let raw_u = crate::net_phase_e::uri_raw_units(ctx, uri);
        if let Some(why) = file_uri_reject(&raw_u) {
            return Err(RuntimeError::IllegalArgumentException {
                message: why.into(),
            }
            .into());
        }
        // gc-common w20-d: `getPath()` is Java. The URI (the field fallback
        // below) and the receiver (written after the String's allocation at
        // the end) were both used through their addresses from before it.
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let this_h = scope.root(this);
        let uri_h = scope.root(uri);
        let ctx = &mut *scope;
        let mut path_u = match ctx.invoke_virtual(uri, "getPath", "()Ljava/lang/String;", &[]) {
            Ok(Some(Value::Object(Some(s)))) => ctx
                .read_string_units(s)
                .filter(|v| !v.is_empty() && *v != raw_u)
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let uri = scope.get(&uri_h);
        let ctx = &mut *scope;
        if path_u.is_empty() {
            path_u = match ctx.get_field_by_name(uri, "path") {
                Value::Object(Some(s)) => ctx
                    .read_string_units(s)
                    .filter(|v| !v.is_empty() && *v != raw_u)
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
        }
        let slash = u16::from(b'/');
        if path_u.is_empty() && !raw_u.is_empty() {
            // Parse from the full URI text:
            //   scheme:[//authority]path[?query][#fragment]
            // Every delimiter here is ASCII, so this is the same scan with a
            // different index type.
            let after_scheme = match raw_u.iter().position(|&c| c == u16::from(b':')) {
                Some(i) => &raw_u[i + 1..],
                None => &raw_u[..],
            };
            let body = if after_scheme.starts_with(&[slash, slash]) {
                let rest = &after_scheme[2..];
                match rest.iter().position(|&c| c == slash) {
                    Some(i) => &rest[i..],
                    None => &[][..],
                }
            } else {
                after_scheme
            };
            let end = body
                .iter()
                .position(|&c| c == u16::from(b'?') || c == u16::from(b'#'))
                .unwrap_or(body.len());
            path_u = body[..end].to_vec();
        }
        // WinNTFileSystem.fromURIPath: `/C:/foo/` -> `C:/foo`. The old spelling
        // measured the drive test in CHARS and the trailing-slash test in BYTES
        // of the same string; units settle both consistently.
        let mut p = path_u;
        if p.len() > 2 && p[0] == slash && p[2] == u16::from(b':') {
            p.remove(0);
            if p.len() > 3 && p.last() == Some(&slash) {
                p.pop();
            }
        } else if p.len() > 1 && p.last() == Some(&slash) {
            p.pop();
        }
        // Normalise to platform separators / collapse `.` `..` segments.
        let normalised = file_normalise_path_units(&p);
        let s = ctx.create_string_from_units(&normalised);
        let this = scope.get(&this_h);
        scope.set_field(this, 0, Value::Object(Some(s)));
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // --- Path accessors ---
    // `getPath()`/`toString()` render the OS-native separator. `File.<init>`
    // already normalises field 0 (so a normally-constructed File holds '\' on
    // Windows), but a File minted from `Path.toFile()` keeps the Path's '/'
    // internal form — normalise on read so both paths agree with HotSpot
    // (no-op on Unix; field 0 holds no jar-FS sentinel).
    r.register(file, "getPath", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string_units(s).unwrap_or_default(),
            other => return Ok(Some(other)),
        };
        let s = ctx.create_string_from_units(&file_normalise_path_units(&raw));
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(file, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string_units(s).unwrap_or_default(),
            other => return Ok(Some(other)),
        };
        let s = ctx.create_string_from_units(&file_normalise_path_units(&raw));
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(file, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // `std::path::Path::file_name` + `to_string_lossy` was a THIRD place the
        // unit was lost, after the constructor and the field read. The basename
        // rule is a split on an ASCII separator, so it needs no `Path` at all.
        let path = file_read_path_units(ctx, this);
        let name = file_basename_units(&path);
        let s = ctx.create_string_from_units(&name);
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(file, "getParent", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path_units(ctx, this);
        match file_parent_units(&path) {
            Some(pu) if !pu.is_empty() => {
                let s = ctx.create_string_from_units(&pu);
                Ok(Some(Value::Object(Some(s))))
            }
            _ => Ok(Some(Value::Object(None))),
        }
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(file, "getParentFile", "()Ljava/io/File;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // The JDK's body is `String p = this.getParent(); if (p == null) return
        // null; return new File(p, this.prefixLength);` — so the two methods
        // cannot disagree. This one used `std::path::Path::parent`, whose
        // COMPONENTS normalise `.` away, and so they did:
        //
        //   new File("a/./b").getParent()      "a/."     getParentFile()  "a"
        //   new File("/.").getParent()         "/"       getParentFile()  null
        //
        // Both MEASURED against HotSpot. The second shape is the one that
        // bites: `new File(".").getAbsoluteFile().getParentFile()` is the
        // ordinary way to name the working directory, and it answered the
        // directory ABOVE it.
        //
        // Going through `file_parent_units` also removes the G78-1 §4 loss —
        // the parent of a surrogate-bearing path no longer round-trips through
        // a lossy `String`.
        let path = file_read_path_units(ctx, this);
        match file_parent_units(&path) {
            Some(pu) if !pu.is_empty() => {
                Ok(Some(Value::Object(Some(file_alloc_units(ctx, &pu)?))))
            }
            _ => Ok(Some(Value::Object(None))),
        }
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        file,
        "getAbsolutePath",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let path = file_read_path(ctx, this);
            // The SAME rule `toURI` uses, and they must not drift: `File.toURI()`
            // is DEFINED in terms of `getAbsoluteFile()`, so a UNC or
            // drive-relative path that absolutizes differently in the two places
            // is a `File` whose own URI names a different file.
            let abs = file_absolutize(&path);
            // java.io.File normalizes a trailing separator on ordinary paths
            // (`new File("/tmp/").getAbsolutePath()` is `/tmp`). Keeping it
            // made Keycloak persist kc.home.dir with a trailing slash while
            // Path-based config resolution returned the normalized form.
            let abs = p57_trim_file_trailing_separator(&abs);
            let s = ctx.create_string(&abs);
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(file, "getAbsoluteFile", "()Ljava/io/File;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        // Same shared rule as `getAbsolutePath` above.
        let abs = file_absolutize(&path);
        let abs = p57_trim_file_trailing_separator(&abs);
        Ok(Some(Value::Object(Some(file_alloc(ctx, &abs)?))))
    }, cratonvm_native_api::NativeKind::Bridge);
    // Strip the Windows `\\?\` extended-length prefix that
    // `std::fs::canonicalize` prepends. The real JDK's `getCanonicalPath`
    // never returns a verbatim/UNC-prefixed path; leaving `\\?\` in place
    // makes a later `File.toURI()` produce `file://?/C:/...`, an invalid
    // URL that breaks Tomcat's `ClassLoaderFactory.buildClassLoaderUrl`.
    r.register(
        file,
        "getCanonicalPath",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let path = file_read_path(ctx, this);
            let canonical = file_canonicalize_path(&path)?;
            let s = ctx.create_string(&canonical);
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(file, "getCanonicalFile", "()Ljava/io/File;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let canonical = file_canonicalize_path(&path)?;
        Ok(Some(Value::Object(Some(file_alloc(ctx, &canonical)?))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(file, "isAbsolute", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        Ok(Some(Value::Int(i32::from(file_is_absolute(&path)))))
    }, cratonvm_native_api::NativeKind::Bridge);

    // --- Metadata (existence, type, permissions) ---
    r.register(file, "exists", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let exists = std::path::Path::new(&path).exists();
        if crate::nbflags().dbg_sbload {
            eprintln!("[DBG_SBLOAD] File.exists() path={:?} -> {}", path, exists);
        }
        Ok(Some(Value::Int(if exists { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(file, "isFile", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let result = std::fs::metadata(&path)
            .map(|m| m.is_file())
            .unwrap_or(false);
        Ok(Some(Value::Int(if result { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(file, "isDirectory", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let result = std::fs::metadata(&path)
            .map(|m| m.is_dir())
            .unwrap_or(false);
        if crate::nbflags().dbg_sbload {
            eprintln!(
                "[DBG_SBLOAD] File.isDirectory() path={:?} -> {}",
                path, result
            );
        }
        Ok(Some(Value::Int(if result { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(file, "isHidden", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        // This used to apply the leading-`.` rule on EVERY platform. On Windows
        // that is wrong in both directions, measured against HotSpot: a
        // `.dotfile` is NOT hidden, and a plain name carrying
        // `FILE_ATTRIBUTE_HIDDEN` IS. `fs_boolean_attributes` is the same
        // platform split `getBooleanAttributes` already encodes — and, since
        // `Files.isHidden` and the `dos:hidden` attribute now read the real
        // Windows bit, sharing it here is what stops two spellings of the same
        // question from disagreeing.
        let hidden = fs_boolean_attributes(&path) & FS_BA_HIDDEN != 0;
        Ok(Some(Value::Int(if hidden { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    // THE THREE `can*` METHODS ASK `access(2)`, via the SAME `fs_check_access`
    // that `UnixFileSystem.checkAccess` has always used. They used to ask three
    // different and weaker questions about the mode BITS:
    //
    //   canRead     `metadata(path).is_ok()`      -- i.e. "does it exist?"
    //   canWrite    `!permissions().readonly()`   -- any write bit, for anyone
    //   canExecute  `mode() & 0o111 != 0`         -- any execute bit, for anyone
    //
    // A permission question is about THIS PROCESS, not about the file. The
    // distance shows up the moment the two disagree, which is exactly what
    // `File.setReadable(false, true)` arranges -- it clears the OWNER read bit
    // and leaves group and other set:
    //
    //   chmod -w-rw-r-- ; canRead()      HotSpot false, this VM true
    //
    // MEASURED with `L4TailSweep2.java`. `canRead` was the visible
    // one because the probe drove it through `setReadable`; `canWrite` and
    // `canExecute` are the same defect one bit over, and are fixed here on the
    // argument, not on a measurement -- a file owned by another user with mode
    // 0o001 answered "executable" to every caller.
    //
    // This ALSO retires a fabricated success one layer down: the fix landed on
    // `setReadable` first, and the setter was never the liar -- the chmod had
    // been correct all along (`getPosixFilePermissions` agreed with HotSpot
    // byte for byte). Reading the state back through a SECOND method is what
    // separated them.
    //
    // `access(2)` costs one syscall, the same as the `stat` it replaces, and it
    // honours ACLs, read-only mounts and root's override -- none of which a
    // mode-bit test can see. `native-io`'s `native_file_can_read` (which opens
    // the file) does not own these slots and is left alone.
    r.register(file, "canRead", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        Ok(Some(Value::Int(i32::from(fs_check_access(
            &path,
            FS_ACCESS_READ,
        )))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(file, "canWrite", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        Ok(Some(Value::Int(i32::from(fs_check_access(
            &path,
            FS_ACCESS_WRITE,
        )))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(file, "canExecute", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        Ok(Some(Value::Int(i32::from(fs_check_access(
            &path,
            FS_ACCESS_EXECUTE,
        )))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(file, "length", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let size = std::fs::metadata(&path)
            .map(|m| m.len() as i64)
            .unwrap_or(0);
        Ok(Some(Value::Long(size)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(file, "lastModified", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let millis = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Ok(Some(Value::Long(millis)))
    }, cratonvm_native_api::NativeKind::Bridge);

    // --- Mutations ---
    r.register(file, "delete", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let p = std::path::Path::new(&path);
        let ok = if p.is_dir() {
            std::fs::remove_dir(p).is_ok()
        } else {
            std::fs::remove_file(p).is_ok()
        };
        Ok(Some(Value::Int(if ok { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(file, "mkdir", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let ok = std::fs::create_dir(&path).is_ok();
        Ok(Some(Value::Int(if ok { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    // `File.mkdirs()` RETURNS FALSE WHEN THE DIRECTORY ALREADY EXISTS.
    //
    // `std::fs::create_dir_all` answers `Ok(())` for a path that is already a
    // directory, so `is_ok()` alone reports "I created it" for a directory this
    // call did nothing to. The JDK's own body opens with `if (exists()) return
    // false;` -- the return value is "did THIS call create the directory", not
    // "does the directory exist now", and callers branch on it (an installer that
    // treats `true` as "first run", a cache that treats it as "I own this dir").
    //
    // MEASURED, `regression-suite/probes/W4File.java`, Linux/JDK 25.0.4, both
    // modes:
    //
    //     new File(dir, "x/y/z").mkdirs()   first call    HotSpot true   CratonVM true
    //     new File(dir, "x/y/z").mkdirs()   second call   HotSpot FALSE  CratonVM TRUE
    //
    // The sibling `mkdir()` was already right -- `create_dir` fails on an existing
    // path -- which is why the single-level case matched the oracle and only the
    // recursive one did not.
    //
    // BOTH COPIES of this native are fixed, deliberately. `--dump-native-registry`
    // shows two registrations of `java/io/File.mkdirs()Z`: this one, `owns_slot=
    // True`, and `native-io/src/lib.rs`'s, `owns_slot=False`. Fixing only the
    // winner leaves the identical defect armed behind it, and trap 4 is that
    // retiring a winner PROMOTES the loser.
    r.register(file, "mkdirs", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        if std::path::Path::new(&path).exists() {
            return Ok(Some(Value::Int(0)));
        }
        let ok = std::fs::create_dir_all(&path).is_ok();
        Ok(Some(Value::Int(if ok { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(file, "createNewFile", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => Ok(Some(Value::Int(1))),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(Some(Value::Int(0))),
            Err(e) => Err(RuntimeError::IOException {
                message: e.to_string(),
            }
            .into()),
        }
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(file, "renameTo", "(Ljava/io/File;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let src = file_read_path(ctx, this);
        // `renameTo(null)` is an NPE in the JDK (`dest.getPath()`), not a
        // false. A false is indistinguishable from "the rename was refused by
        // the filesystem", which is the answer callers retry or log.
        let dst = match args.get(1) {
            Some(Value::Object(Some(f))) => file_read_path(ctx, *f),
            _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
        };
        let ok = std::fs::rename(&src, &dst).is_ok();
        Ok(Some(Value::Int(if ok { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(file, "setLastModified", "(J)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let millis = match args.get(1) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        // `if (time < 0) throw new IllegalArgumentException("Negative time");`
        // — the JDK's first line, and the only argument check the method has.
        if millis < 0 {
            return Err(RuntimeError::IllegalArgumentException {
                message: "Negative time".into(),
            }
            .into());
        }
        Ok(Some(Value::Int(if set_file_mtime_millis(&path, millis) {
            1
        } else {
            0
        })))
    }, cratonvm_native_api::NativeKind::Bridge);
    // THE FOUR PERMISSION SETTERS ROUTE THROUGH `fs_set_permission`, which is
    // the same `access(2)`-shaped helper `UnixFileSystem.setPermission` already
    // uses. Two of them used to be FABRICATED SUCCESSES: `setReadable(boolean)`
    // read its argument into `_readable` and then ignored it, and both
    // two-argument forms ignored BOTH arguments -- all three answered
    // "does this file exist?" and changed nothing.
    //
    //   f.setReadable(false, true)   ->  true, and canRead() still true
    //   f.setExecutable(true, true)  ->  true, and canExecute() still false
    //
    // MEASURED with `L4TailSweep2.java`. The earlier sweep asked
    // these methods and PASSED, because it asserted the RETURN VALUE and not
    // the effect -- an identity-only probe understating a behavioural gap. A
    // permission API that reports success and changes nothing is the worst
    // shape available: a program restricting access to a file believes it did.
    //
    // `ownerOnly` is honoured too, which `setWritable`'s `set_readonly` spelling
    // could not express: the JDK's one-argument forms are defined as the
    // two-argument form with `ownerOnly = true`.
    r.register(file, "setReadable", "(Z)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let readable = args.get(1).and_then(|v| v.as_int()).unwrap_or(1) != 0;
        let ok = fs_set_permission(&path, FS_ACCESS_READ, readable, true);
        Ok(Some(Value::Int(i32::from(ok))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(file, "setReadable", "(ZZ)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let readable = args.get(1).and_then(|v| v.as_int()).unwrap_or(1) != 0;
        let owner_only = args.get(2).and_then(|v| v.as_int()).unwrap_or(1) != 0;
        let ok = fs_set_permission(&path, FS_ACCESS_READ, readable, owner_only);
        Ok(Some(Value::Int(i32::from(ok))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(file, "setWritable", "(Z)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let writable = args.get(1).and_then(|v| v.as_int()).unwrap_or(1) != 0;
        let ok = fs_set_permission(&path, FS_ACCESS_WRITE, writable, true);
        Ok(Some(Value::Int(i32::from(ok))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(file, "setWritable", "(ZZ)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let writable = args.get(1).and_then(|v| v.as_int()).unwrap_or(1) != 0;
        let owner_only = args.get(2).and_then(|v| v.as_int()).unwrap_or(1) != 0;
        let ok = fs_set_permission(&path, FS_ACCESS_WRITE, writable, owner_only);
        Ok(Some(Value::Int(i32::from(ok))))
    }, cratonvm_native_api::NativeKind::Bridge);
    // THE ONE-ARGUMENT FORM WAS THE ONE THIS BLOCK'S OWN COMMENT MISSED.
    //
    // `setReadable(Z)`, `setReadable(ZZ)`, `setWritable(Z)`, `setWritable(ZZ)`
    // and `setExecutable(ZZ)` all delegate to `fs_set_permission`. This one
    // hand-rolled its own body, and it was wrong on BOTH platforms:
    //
    //   * on Windows it answered `std::fs::metadata(path).is_ok()` -- "does
    //     this file exist?" -- so `setExecutable(false)` reported TRUE. That is
    //     exactly the fabricated success the comment above says was removed
    //     from the other three; it survived here because the earlier sweep
    //     never asked the DISABLE direction on Windows. The JDK's
    //     `WinNTFileSystem` cannot revoke execute and reports the request back:
    //     `enable`. Measured against HotSpot 25 by `L4FileSweep.java`,
    //     the row `setExecutable false` -- HotSpot `false`, CratonVM `true`, in
    //     BOTH `--fast-boot` and `--real-jdk`;
    //
    //   * on Unix it toggled `0o111`, all three execute bits, where the
    //     one-argument form is DEFINED as the two-argument form with
    //     `ownerOnly = true` -- so it should touch `0o100` only.
    //     `fs_set_permission` already makes that distinction, which is the
    //     second reason to call it rather than to re-derive it.
    //
    // Delegating fixes both and leaves one implementation of the rule.
    r.register(file, "setExecutable", "(Z)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let exec = args.get(1).and_then(|v| v.as_int()).unwrap_or(1) != 0;
        let ok = fs_set_permission(&path, FS_ACCESS_EXECUTE, exec, true);
        Ok(Some(Value::Int(i32::from(ok))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(file, "setExecutable", "(ZZ)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let exec = args.get(1).and_then(|v| v.as_int()).unwrap_or(1) != 0;
        let owner_only = args.get(2).and_then(|v| v.as_int()).unwrap_or(1) != 0;
        let ok = fs_set_permission(&path, FS_ACCESS_EXECUTE, exec, owner_only);
        Ok(Some(Value::Int(i32::from(ok))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(file, "setReadOnly", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let ok = std::fs::metadata(&path)
            .and_then(|meta| {
                let mut perms = meta.permissions();
                perms.set_readonly(true);
                std::fs::set_permissions(&path, perms)
            })
            .is_ok();
        Ok(Some(Value::Int(if ok { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);

    // --- Listing ---
    r.register(file, "list", "()[Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        match std::fs::read_dir(&path) {
            Ok(entries) => {
                let names: Vec<String> = entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect();
                // A typed component class, not the untyped `Object[]` that
                // `ArrayElementType::Reference` produces — the descriptor's array
                // type is what the caller's assignment checkcasts against.
                let component = string_class_id(ctx);
                let arr = ctx.new_ref_array(component, names.len());
                // Pin across the create_strings below — a moving young GC
                // there would relocate the fresh array (native stale-local
                // family).
                let arr_pin = ctx.pin_native_root(arr);
                for (i, name) in names.iter().enumerate() {
                    let s = ctx.create_string(name);
                    let arr = ctx.read_native_pin(arr_pin, arr);
                    ctx.set_array_element(arr, i, Value::Object(Some(s)));
                }
                let arr = ctx.read_native_pin(arr_pin, arr);
                ctx.unpin_native_roots(arr_pin);
                Ok(Some(Value::Object(Some(arr))))
            }
            Err(_) => Ok(Some(Value::Object(None))),
        }
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(file, "listFiles", "()[Ljava/io/File;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        match std::fs::read_dir(&path) {
            Ok(entries) => {
                let paths: Vec<String> = entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.path().to_string_lossy().into_owned())
                    .collect();
                // A typed component class, not the untyped `Object[]` that
                // `ArrayElementType::Reference` produces — the descriptor's array
                // type is what the caller's assignment checkcasts against.
                let component = file_class_id(ctx);
                let arr = ctx.new_ref_array(component, paths.len());
                // Pin across the File allocs below — a moving young GC there
                // would relocate the fresh array (native stale-local family).
                let arr_pin = ctx.pin_native_root(arr);
                for (i, p) in paths.iter().enumerate() {
                    let f = file_alloc(ctx, p)?;
                    let arr = ctx.read_native_pin(arr_pin, arr);
                    ctx.set_array_element(arr, i, Value::Object(Some(f)));
                }
                let arr = ctx.read_native_pin(arr_pin, arr);
                ctx.unpin_native_roots(arr_pin);
                Ok(Some(Value::Object(Some(arr))))
            }
            Err(_) => Ok(Some(Value::Object(None))),
        }
    }, cratonvm_native_api::NativeKind::Bridge);
    // listFiles(FileFilter) — invoke filter.accept(File) for each entry
    r.register(
        file,
        "listFiles",
        "(Ljava/io/FileFilter;)[Ljava/io/File;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let path = file_read_path(ctx, this);
            let filter = args.get(1).copied().unwrap_or(Value::Object(None));
            let entries: Vec<String> = match std::fs::read_dir(&path) {
                Ok(iter) => iter
                    .filter_map(|e| e.ok())
                    .map(|e| e.path().to_string_lossy().into_owned())
                    .collect(),
                Err(_) => return Ok(Some(Value::Object(None))),
            };
            // Pin across the File allocs / filter callbacks below — a moving
            // young GC there would relocate them (native stale-local family);
            // each fresh File is pinned as it materialises.
            let filter_pin = pinned_object_value(ctx, filter);
            let mut first_pin = filter_pin.map(|(h, _)| h);
            let mut accepted: Vec<(usize, ObjectRef)> = Vec::new();
            for entry_path in &entries {
                let file_obj = file_alloc(ctx, entry_path)?;
                let file_pin = ctx.pin_native_root(file_obj);
                if first_pin.is_none() {
                    first_pin = Some(file_pin);
                }
                let accept = match read_pinned_object_value(ctx, filter_pin, filter) {
                    Value::Object(Some(f)) => {
                        match ctx.invoke_virtual(
                            f,
                            "accept",
                            "(Ljava/io/File;)Z",
                            &[Value::Object(Some(file_obj))],
                        ) {
                            Ok(Some(Value::Int(v))) => v != 0,
                            _ => true, // on failure, include
                        }
                    }
                    _ => true, // null filter accepts everything
                };
                if accept {
                    accepted.push((file_pin, file_obj));
                }
            }
            // A typed component class, not the untyped `Object[]` that
            // `ArrayElementType::Reference` produces — the descriptor's array
            // type is what the caller's assignment checkcasts against.
            let component = file_class_id(ctx);
            let arr = ctx.new_ref_array(component, accepted.len());
            for (i, (pin, orig)) in accepted.iter().enumerate() {
                let f = ctx.read_native_pin(*pin, *orig);
                ctx.set_array_element(arr, i, Value::Object(Some(f)));
            }
            if let Some(h) = first_pin {
                ctx.unpin_native_roots(h);
            }
            Ok(Some(Value::Object(Some(arr))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // listFiles(FilenameFilter) — invoke filter.accept(File dir, String name)
    r.register(
        file,
        "listFiles",
        "(Ljava/io/FilenameFilter;)[Ljava/io/File;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let path = file_read_path(ctx, this);
            let filter = args.get(1).copied().unwrap_or(Value::Object(None));
            let entries: Vec<(String, String)> = match std::fs::read_dir(&path) {
                Ok(iter) => iter
                    .filter_map(|e| e.ok())
                    .map(|e| {
                        let full = e.path().to_string_lossy().into_owned();
                        let name = e.file_name().to_string_lossy().into_owned();
                        (full, name)
                    })
                    .collect(),
                Err(_) => return Ok(Some(Value::Object(None))),
            };
            // Pin across the File allocs / filter callbacks below — a moving
            // young GC there would relocate them (native stale-local family);
            // each fresh File is pinned as it materialises.
            let this_pin = ctx.pin_native_root(this);
            let filter_pin = pinned_object_value(ctx, filter);
            let mut accepted: Vec<(usize, ObjectRef)> = Vec::new();
            for (full, name) in &entries {
                let accept = match read_pinned_object_value(ctx, filter_pin, filter) {
                    Value::Object(Some(f)) => {
                        let name_str = ctx.create_string(name);
                        let this = ctx.read_native_pin(this_pin, this);
                        match ctx.invoke_virtual(
                            f,
                            "accept",
                            "(Ljava/io/File;Ljava/lang/String;)Z",
                            &[Value::Object(Some(this)), Value::Object(Some(name_str))],
                        ) {
                            Ok(Some(Value::Int(v))) => v != 0,
                            _ => true,
                        }
                    }
                    _ => true,
                };
                if accept {
                    let f_obj = file_alloc(ctx, full)?;
                    accepted.push((ctx.pin_native_root(f_obj), f_obj));
                }
            }
            // A typed component class, not the untyped `Object[]` that
            // `ArrayElementType::Reference` produces — the descriptor's array
            // type is what the caller's assignment checkcasts against.
            let component = file_class_id(ctx);
            let arr = ctx.new_ref_array(component, accepted.len());
            for (i, (pin, orig)) in accepted.iter().enumerate() {
                let f = ctx.read_native_pin(*pin, *orig);
                ctx.set_array_element(arr, i, Value::Object(Some(f)));
            }
            ctx.unpin_native_roots(this_pin);
            Ok(Some(Value::Object(Some(arr))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // list(FilenameFilter) — string array variant
    r.register(
        file,
        "list",
        "(Ljava/io/FilenameFilter;)[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let path = file_read_path(ctx, this);
            let filter = args.get(1).copied().unwrap_or(Value::Object(None));
            let entries: Vec<String> = match std::fs::read_dir(&path) {
                Ok(iter) => iter
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect(),
                Err(_) => return Ok(Some(Value::Object(None))),
            };
            // Pin across the filter callbacks below — a moving young GC there
            // would relocate `this`/`filter` (native stale-local family).
            let this_pin = ctx.pin_native_root(this);
            let filter_pin = pinned_object_value(ctx, filter);
            let mut accepted: Vec<String> = Vec::new();
            for name in &entries {
                let accept = match read_pinned_object_value(ctx, filter_pin, filter) {
                    Value::Object(Some(f)) => {
                        let name_str = ctx.create_string(name);
                        let this = ctx.read_native_pin(this_pin, this);
                        match ctx.invoke_virtual(
                            f,
                            "accept",
                            "(Ljava/io/File;Ljava/lang/String;)Z",
                            &[Value::Object(Some(this)), Value::Object(Some(name_str))],
                        ) {
                            Ok(Some(Value::Int(v))) => v != 0,
                            _ => true,
                        }
                    }
                    _ => true,
                };
                if accept {
                    accepted.push(name.clone());
                }
            }
            ctx.unpin_native_roots(this_pin);
            // A typed component class, not the untyped `Object[]` that
            // `ArrayElementType::Reference` produces — the descriptor's array
            // type is what the caller's assignment checkcasts against.
            let component = string_class_id(ctx);
            let arr = ctx.new_ref_array(component, accepted.len());
            let arr_pin = ctx.pin_native_root(arr);
            for (i, name) in accepted.iter().enumerate() {
                let s = ctx.create_string(name);
                let arr = ctx.read_native_pin(arr_pin, arr);
                ctx.set_array_element(arr, i, Value::Object(Some(s)));
            }
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.unpin_native_roots(arr_pin);
            Ok(Some(Value::Object(Some(arr))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- Disk space (real OS query; 0 for a path that does not exist, matching
    // HotSpot's WinNTFileSystem/UnixFileSystem contract) ---
    r.register(file, "getFreeSpace", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let free = file_disk_space_bytes(&path).map_or(0, |(_, free, _)| free);
        Ok(Some(Value::Long(free as i64)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(file, "getTotalSpace", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let total = file_disk_space_bytes(&path).map_or(0, |(total, _, _)| total);
        Ok(Some(Value::Long(total as i64)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(file, "getUsableSpace", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        let usable = file_disk_space_bytes(&path).map_or(0, |(_, _, usable)| usable);
        Ok(Some(Value::Long(usable as i64)))
    }, cratonvm_native_api::NativeKind::Bridge);

    // --- Equality / comparison ---
    r.register(file, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        // Case-INSENSITIVE on Windows (WinNTFileSystem), exact elsewhere. The
        // previous exact comparison answered false for `File("ab")` vs
        // `File("AB")` where HotSpot answers true — measured, not assumed.
        let p1 = file_path_key_units(&file_read_path_units(ctx, this));
        let p2 = file_path_key_units(&file_read_path_units(ctx, other));
        Ok(Some(Value::Int(if p1 == p2 { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(file, "hashCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // `UnixFileSystem.hashCode` is `path.hashCode() ^ 1234321`;
        // `WinNTFileSystem.hashCode` folds case first. Both were missing: the
        // old body hashed UTF-8 BYTES (right only for ASCII) and omitted the
        // mixing constant entirely, so `new File("AB")` answered 2081 where
        // HotSpot answers 1235376.
        let path = file_path_key_units(&file_read_path_units(ctx, this));
        Ok(Some(Value::Int(java_string_hash_units(&path) ^ 1234321)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(file, "compareTo", "(Ljava/io/File;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // `compareTo(null)` reaches `pathname.getPath()` and is an NPE.
        // Answering 0 said "these two files are the same", which is the one
        // answer a sort or a TreeSet acts on destructively.
        let other = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
        };
        let p1 = file_path_key_units(&file_read_path_units(ctx, this));
        let p2 = file_path_key_units(&file_read_path_units(ctx, other));
        Ok(Some(Value::Int(u_compare(&p1, &p2))))
    }, cratonvm_native_api::NativeKind::Bridge);

    // toPath and toURI
    //
    // Validate (AND, on Windows, separator-collapse — `G22-1` §5(a)) the path
    // first — real `File.toPath()` delegates to
    // `FileSystems.getDefault().getPath(...)`, which throws
    // `InvalidPathException` for Windows-illegal syntax (e.g. a bare colon
    // outside a drive specifier, as in Spring's `ping:foo`
    // `ProtocolResolver` probe: `GenericApplicationContextTests.
    // getResourceWithCustomResourceLoader` relies on `FileSystemResource`'s
    // `this.file.toPath()` throwing for exactly this). Delegates to
    // `p57_alloc_path_checked` — the same `Paths.get` uses — rather than
    // re-deriving the check, so the two entry points cannot drift apart the
    // way this one already had (no separator collapsing, an index-less
    // exception message) before that function grew both.
    r.register(file, "toPath", "()Ljava/nio/file/Path;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        p57_alloc_path_checked(ctx, &path)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(file, "toURI", "()Ljava/net/URI;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        // File.toURI() operates on getAbsoluteFile(): a RELATIVE File must be
        // resolved against the working directory first. Skipping that turned
        // `new File("runner").toURI()` into `file:/runner/` — a nonexistent
        // root path that silently dropped relative classpath entries from
        // Gradle's ClasspathUtil walk.
        let path = file_absolutize(&path);
        // Per `File.toURI()`, the result is `new URI("file", null, slashify(absPath, isDir), null)`,
        // which renders as `file:/C:/...` — a SINGLE slash before the path
        // (no `//authority`). Emitting `file://` + `/C:/...` produced the
        // malformed `file:///C:/...` whose `URI.toURL()` returned null and
        // broke every `URLClassLoader` built from `File.toURI().toURL()`.
        // `slashify` IS PLATFORM-SPECIFIC. `File.toURI()` calls
        // `fs.slashify(path)`, which on `WinNTFileSystem` rewrites `\` to `/`
        // and on `UnixFileSystem` does nothing at all — because on Unix a
        // backslash is an ordinary filename character, so rewriting it renames
        // the file. What happens to it instead is that `ParseUtil.encodePath`
        // escapes it, `\` not being a URI path character. MEASURED on Linux,
        // nine rows of `probes/FilePathSweep.java`:
        //
        //   new File("relative\win\path").toURI()
        //     HotSpot   file:/…/relative%5Cwin%5Cpath
        //     CratonVM  file:/…/relative/win/path      <- three directories
        //
        // The CratonVM answer does not merely render differently: round-tripped
        // through `new File(uri)` it names a DIFFERENT FILE, three levels down a
        // tree that does not exist.
        let mut dir_path = if cfg!(windows) {
            path.replace('\\', "/")
        } else {
            path.clone()
        };
        if !dir_path.starts_with('/') {
            dir_path = format!("/{dir_path}");
        }
        let is_dir = std::path::Path::new(&path).is_dir();
        // Directory URIs end with a trailing slash, matching `slashify(...)`.
        if is_dir && !dir_path.ends_with('/') {
            dir_path.push('/');
        }
        // `File.toURI()` has one more line, and it is not decoration:
        //
        //     String sp = slashify(f.getPath(), f.isDirectory());
        //     if (sp.startsWith("//")) sp = "//" + sp;
        //     return new URI("file", null, sp, null);
        //
        // A UNC path slashifies to `//server/share`, and a URI whose
        // scheme-specific part opens with `//` parses the next segment as an
        // AUTHORITY. Without the doubling the host becomes `server` and the
        // path shrinks to `/share`; with it the authority is empty and the
        // whole `//server/share` stays in the path, which is what the JDK
        // means. MEASURED against HotSpot 25.0.3+9:
        //
        //   new File("\\\\server\\share").toURI()
        //     HotSpot   file:////server/share
        //     CratonVM  file://server/share
        //
        // They do not merely render differently: `getHost()` answers `server`
        // on one and `null` on the other, so a round trip through
        // `new File(uri)` reaches a different place.
        if dir_path.starts_with("//") {
            dir_path.insert_str(0, "//");
        }
        // RFC 2396 percent-encoding, matching `sun.net.www.ParseUtil.encodePath`
        // used by `File.toURI()`. Without this, special path chars (`^`, space,
        // `#`, `?`, …) leaked into the URI literally, so `File.toURI()` returned
        // `file:/a b/c` instead of `file:/a%20b/c` (Tomcat UriUtil tests; any
        // `new URI(File.toURI().toString())` round-trip on such paths threw).
        let encoded = encode_file_uri_path(&dir_path);
        let full = format!("file:{encoded}");
        // Allocate a real URI and populate named + positional fields so both
        // `URI` natives and any real-JDK bytecode see consistent state.
        let uri = try_alloc_concurrent_synthetic(ctx, "java/net/URI", 7)?;
        // Pin across the parse/store helpers below (they create strings) — a
        // moving young GC there would relocate the fresh URI (native
        // stale-local family).
        let uri_pin = ctx.pin_native_root(uri);
        crate::url_parse(ctx, uri, &full);
        let uri = ctx.read_native_pin(uri_pin, uri);
        crate::uri_store_named(ctx, uri, &full);
        let uri = ctx.read_native_pin(uri_pin, uri);
        ctx.unpin_native_roots(uri_pin);
        Ok(Some(Value::Object(Some(uri))))
    }, cratonvm_native_api::NativeKind::Bridge);

    // Static helpers
    r.register(file, "listRoots", "()[Ljava/io/File;", |ctx, _args| {
        #[cfg(windows)]
        let roots: Vec<String> = {
            let mut out = Vec::new();
            for letter in b'A'..=b'Z' {
                let root = format!("{}:\\", letter as char);
                if std::path::Path::new(&root).exists() {
                    out.push(root);
                }
            }
            if out.is_empty() {
                out.push("C:\\".to_string());
            }
            out
        };
        #[cfg(not(windows))]
        let roots: Vec<String> = vec!["/".to_string()];
        // A typed component class, not the untyped `Object[]` that
        // `ArrayElementType::Reference` produces — the descriptor's array
        // type is what the caller's assignment checkcasts against.
        let component = file_class_id(ctx);
        let arr = ctx.new_ref_array(component, roots.len());
        // Pin across the File allocs below — a moving young GC there would
        // relocate the fresh array (native stale-local family).
        let arr_pin = ctx.pin_native_root(arr);
        for (i, r) in roots.iter().enumerate() {
            let f = file_alloc(ctx, r)?;
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.set_array_element(arr, i, Value::Object(Some(f)));
        }
        let arr = ctx.read_native_pin(arr_pin, arr);
        ctx.unpin_native_roots(arr_pin);
        Ok(Some(Value::Object(Some(arr))))
    }, cratonvm_native_api::NativeKind::Bridge);
    /// `File.createTempFile`'s prefix contract, transcribed rather than composed.
    ///
    /// MEASURED on both VMs; the oracle refuses where this VM was creating files:
    ///
    /// ```text
    ///   createTempFile("m",   ".t")  HotSpot IllegalArgumentException
    ///                                        Prefix string "m" too short: length must be at least 3
    ///   createTempFile("mm",  ".t")  HotSpot the same, naming "mm"
    ///   createTempFile("mmm", ".t")  HotSpot OK
    ///   createTempFile(null,  ".t")  HotSpot NullPointerException
    ///                                        Cannot invoke "String.length()" because "prefix" is null
    /// ```
    ///
    /// Both `File.createTempFile` overloads defaulted a null prefix to `"tmp"` and
    /// never looked at the length, so all four rows above created a file. This is a
    /// VALIDATION gap, not a message gap: a program that relies on the refusal —
    /// and `File.createTempFile`'s own javadoc documents it — silently got a file.
    ///
    /// The message names the prefix and is quoted exactly as the JDK writes it,
    /// including the double quotes around the offending string. The NPE text is
    /// the helpful-NPE form HotSpot produces for `prefix.length()`, measured, not
    /// guessed.
    ///
    /// A null SUFFIX is legal and means `.tmp` — that is the documented default
    /// and is deliberately not touched here.
    fn jdk_check_temp_prefix(prefix: Option<&str>) -> Result<(), MethodCallFailed> {
        match prefix {
            None => Err(RuntimeError::NullPointerException {
                message: Some(
                    "Cannot invoke \"String.length()\" because \"prefix\" is null".into(),
                ),
            }
            .into()),
            Some(p) if p.chars().count() < 3 => Err(RuntimeError::IllegalArgumentException {
                message: format!("Prefix string \"{p}\" too short: length must be at least 3"),
            }
            .into()),
            Some(_) => Ok(()),
        }
    }

    r.register(
        file,
        "createTempFile",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/io/File;",
        |ctx, args| {
            let prefix_opt = match args.get(0) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s),
                _ => None,
            };
            jdk_check_temp_prefix(prefix_opt.as_deref())?;
            let prefix = prefix_opt.unwrap_or_else(|| "tmp".into());
            let suffix = match args.get(1) {
                Some(Value::Object(Some(s))) => {
                    ctx.read_string(*s).unwrap_or_else(|| ".tmp".into())
                }
                _ => ".tmp".into(),
            };
            let tmp_dir = jdk_temp_dir(crate::lang_system::saved_system_property(
                ctx,
                "java.io.tmpdir",
            ));
            let full = jdk_create_temp_file(&tmp_dir, &prefix, &suffix)?;
            Ok(Some(Value::Object(Some(file_alloc(ctx, &full)?))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        file,
        "createTempFile",
        "(Ljava/lang/String;Ljava/lang/String;Ljava/io/File;)Ljava/io/File;",
        |ctx, args| {
            let prefix_opt = match args.get(0) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s),
                _ => None,
            };
            jdk_check_temp_prefix(prefix_opt.as_deref())?;
            let prefix = prefix_opt.unwrap_or_else(|| "tmp".into());
            let suffix = match args.get(1) {
                Some(Value::Object(Some(s))) => {
                    ctx.read_string(*s).unwrap_or_else(|| ".tmp".into())
                }
                _ => ".tmp".into(),
            };
            let dir_path = match args.get(2) {
                Some(Value::Object(Some(f))) => std::path::PathBuf::from(file_read_path(ctx, *f)),
                _ => jdk_temp_dir(crate::lang_system::saved_system_property(
                    ctx,
                    "java.io.tmpdir",
                )),
            };
            let full = jdk_create_temp_file(&dir_path, &prefix, &suffix)?;
            Ok(Some(Value::Object(Some(file_alloc(ctx, &full)?))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // Was a documented "no actual tracking" no-op: the file was never deleted
    // and nothing told the caller so. Anything relying on it for cleanup (temp
    // files from `File.createTempFile(...).deleteOnExit()`, unpacked native
    // libraries, scratch DB files) leaked on every run. Record the path and
    // delete it from a C-runtime `atexit` handler, which is what
    // `DeleteOnExitHook` does from the JDK's shutdown hook. Deletion is
    // reverse-registration order, matching the JDK.
    r.register(file, "deleteOnExit", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = file_read_path(ctx, this);
        if !path.is_empty() {
            delete_on_exit_register(ctx.vm_identity(), path);        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    // DELETED 2026-08-13 (lane E36): the four `java/io/File` separator
    // constants — `separator` `Ljava/lang/String;`, `separatorChar` `C`,
    // `pathSeparator` `Ljava/lang/String;`, `pathSeparatorChar` `C` — were
    // registered as METHODS carrying a FIELD descriptor. That triple is one no
    // dispatch can produce: every native-registry lookup originates at an
    // INVOKE, whose descriptor starts with '(', and none of this VM's three
    // static-read paths consults the registry —
    // `interpreter/opcodes.rs Instruction::Getstatic`, `jit/helpers.rs
    // jit_getstatic`, and `ir_lower.rs emit_inline_getstatic`, which bakes the
    // statics base as an immediate and emits two `mov`s with no call at all.
    //
    // The comment that stood over `separatorChar` said to KEEP it because it
    // is "a compile-time platform constant in the real JDK too". That is the
    // one claim here that is measurable, and it is FALSE: in the JDK these are
    // `public static final char separatorChar = fs.getSeparator();` — a method
    // call, not a constant expression — so javac does NOT inline them.
    // Measured on this host (Microsoft build 25.0.3+9-LTS), all four compile
    // to a real read:
    //
    //     getstatic java/io/File.separator:Ljava/lang/String;
    //     getstatic java/io/File.separatorChar:C
    //     getstatic java/io/File.pathSeparator:Ljava/lang/String;
    //     getstatic java/io/File.pathSeparatorChar:C
    //
    // So they are genuinely read at runtime, and these rows still could never
    // answer one. The thing that DOES answer is `vm/src/vm/vm_util.rs`'s
    // post-clinit fixup for `java/io/File`, which sets all four statics by
    // name (plus `FS`) from the same host values — so this deletion changes no
    // behaviour, it removes a second, unreachable publisher.
    // `jdk-only-dead-everywhere.tsv` already carries `separatorChar` as
    // `method-nowhere`. No test in the tree calls any of the four.
    }

// ---------------------------------------------------------------------------
// java.nio.channels.FileChannel — 1-field synthetic (fd_id=0 Int)
// Real file operations via fd_table (same API as RandomAccessFile)
// ---------------------------------------------------------------------------

// ===========================================================================
// The SEVEN FileChannel triples both registrars serve — one body each
// ===========================================================================
//
// `size()J`, `position()J`, `position(J)FileChannel`, `close()V`, `isOpen()Z`,
// `read(Ljava/nio/ByteBuffer;)I` and `write(Ljava/nio/ByteBuffer;)I` are
// registered TWICE, and which copy exists depends on the MODE, not on this file:
//
//   * `register_phase57_nio_file` is called DIRECTLY by `vm/src/vm/vm_init.rs`
//     in both SHIPPING arms — `:2217` (a `--features synthetic-jdk` binary run
//     `--fast-boot`) and `:2763` (the default build) — so in `--fast-boot` and
//     `--real-jdk` its copies are the only ones that exist at all.
//   * `register_phase57_file_channel` (below) is reachable from exactly one
//     place: `register_phase57_natives` (line 36 of this file), itself reachable
//     from exactly one place, `lib.rs`'s `register_synthetic_overrides`, which is
//     `#[cfg(feature = "synthetic-jdk")]` and which `vm_init.rs:1839` reaches
//     only through `register_builtins`, inside `if config.use_synthetic_jdk`.
//     Within `register_phase57_natives` it runs AFTER `register_phase57_nio_file`
//     (lines 21 and 36), and last-write-wins, so in `--synthetic-jdk` ITS copies
//     are the live ones.
//
// W7-8 §3.1 hardened the second registrar only — i.e. only the mode that did not
// have the defect. Rows 1, 2 and 19 of that census stayed LIVE in both shipping
// modes for as long as the two copies had separate bodies.
//
// Deleting either registration is the wrong repair: it changes a different mode
// than the one being fixed. So the seven shared triples now point at ONE
// function each and BOTH registrars register that same function pointer.
// Last-write-wins stops being load-bearing for them — whichever copy wins, the
// body is this one. Keep it that way: if you edit one of these, you are editing
// both modes, which is the property that was missing.
//
// `truncate(J)`, the positional `read`/`write`, `transferTo`, `transferFrom` and
// `force(Z)` stay registered by `register_phase57_file_channel` ALONE, and that
// asymmetry is deliberate rather than an oversight. In a shipping mode a real
// `sun.nio.ch.FileChannelImpl` serves those from its own bytecode; registering
// them on the literal `java/nio/channels/FileChannel` there would put a native
// in front of triples nothing currently intercepts, which is a much larger
// change than hardening seven bodies that are already intercepted. `size()` is
// the precedent for why they were ever added on the synthetic side at all
// (Round 63: without it the abstract declaration raised AbstractMethodError).

/// What the private fd slot of a `FileChannel` receiver says. **Three** states,
/// and the third is the whole reason this type exists.
///
/// `synthetic_file_channel::fd_value` answers `Value::Object(None)` — not an
/// `Int` — for any receiver whose class is not literally
/// `java/nio/channels/FileChannel`; the screen lives inside the accessor
/// (`native-api/src/synthetic_file_channel.rs`, "the screen moved INSIDE the
/// accessors"). A real `sun.nio.ch.FileChannelImpl` arriving through a
/// `FileChannel`-typed call site is therefore distinguishable from a synthetic
/// channel that has been closed, and the two must NOT share an arm:
///
/// * `Closed` can only be a synthetic receiver — `-1` is what `close()` below
///   writes into the slot — so it takes the hardened `ClosedChannelException`
///   arm W7-8 §3.1 specifies.
/// * `Foreign` keeps each site's pre-W7-8 answer **verbatim**. Whether a native
///   on `FileChannel` intercepts a real `FileChannelImpl` for these triples is
///   the one thing W7-8 §3.6/§7.1 records as unsettleable from source (`close()V`
///   demonstrably IS intercepted and has carried a class-name screen for it since
///   the H2 file-lock bug; `read(ByteBuffer)` is a different dispatch shape).
///   Turning a `-1` into a throw on that path without a run is exactly the
///   hazard that row names, so this arm changes nothing until someone measures
///   it.
pub enum P57FcFd {
    /// A synthetic channel with a live `fd_table` id.
    Open(u32),
    /// A synthetic channel whose `close()` has run (slot holds a negative `Int`).
    Closed,
    /// Not one of ours — the accessor declined the receiver.
    Foreign,
}

/// Classify `this`'s fd slot for the seven shared bodies below.
pub fn p57_fc_fd(ctx: &mut dyn NativeContext, this: ObjectRef) -> P57FcFd {
    match cratonvm_native_api::synthetic_file_channel::fd_value(ctx, this) {
        Value::Int(v) if v >= 0 => P57FcFd::Open(v as u32),
        Value::Int(_) => P57FcFd::Closed,
        _ => P57FcFd::Foreign,
    }
}

/// `FileChannel.size()J`. Closed answered `0` — a legal size, and the one a
/// caller reads as "empty file" — where the JDK specifies
/// `ClosedChannelException`.
pub fn p57_fc_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let fd_id = match p57_fc_fd(ctx, this) {
        P57FcFd::Open(fd) => fd,
        P57FcFd::Closed => return Err(p57_closed_channel(ctx)),
        P57FcFd::Foreign => return Ok(Some(Value::Long(0))),
    };
    let sz = ctx
        .fd_table()
        .file_size(fd_id)
        .map_err(|e| RuntimeError::IOException {
            message: e.to_string(),
        })?;
    Ok(Some(Value::Long(sz as i64)))
}

/// `FileChannel.position()J`. Closed answered `0`, indistinguishable from a
/// channel positioned at the start of the file.
pub fn p57_fc_position(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let fd_id = match p57_fc_fd(ctx, this) {
        P57FcFd::Open(fd) => fd,
        P57FcFd::Closed => return Err(p57_closed_channel(ctx)),
        P57FcFd::Foreign => return Ok(Some(Value::Long(0))),
    };
    let pos = ctx
        .fd_table()
        .rw_position(fd_id)
        .map_err(|e| RuntimeError::IOException {
            message: e.to_string(),
        })?;
    Ok(Some(Value::Long(pos as i64)))
}

/// `FileChannel.position(J)FileChannel`, which returns `this` — so a refusal has
/// no return value to ride on and must be an exception or nothing.
///
/// "@throws IllegalArgumentException If the new position is negative"; `.max(0)`
/// answered a successful seek to 0 instead. A negative position is how a
/// caller's own arithmetic reports a bug (commons-compress computes seek-from-EOF
/// offsets this way); clamping hides it and then reads or writes the wrong region
/// of the file.
pub fn p57_fc_position_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let pos = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    if pos < 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("Negative position: {pos}"),
        }
        .into());
    }
    let fd_id = match p57_fc_fd(ctx, this) {
        P57FcFd::Open(fd) => fd,
        P57FcFd::Closed => return Err(p57_closed_channel(ctx)),
        P57FcFd::Foreign => return Ok(Some(Value::Object(Some(this)))),
    };
    ctx.fd_table()
        .rw_seek(fd_id, std::io::SeekFrom::Start(pos as u64))
        .map_err(|e| RuntimeError::IOException {
            message: e.to_string(),
        })?;
    Ok(Some(Value::Object(Some(this))))
}

/// `FileChannel.read(ByteBuffer)I`.
///
/// Was `Ok(Int(-1))` on a closed channel. `-1` from a channel read is the
/// end-of-file indication, so a read of a CLOSED channel was reported as a
/// clean, complete end of stream: every copy loop of the form
/// `while ((n = ch.read(buf)) != -1)` terminated normally and its caller wrote
/// out a truncated file believing it had the whole thing.
/// `ReadableByteChannel.read` specifies `ClosedChannelException` here.
pub fn p57_fc_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let fd_id = match p57_fc_fd(ctx, this) {
        P57FcFd::Open(fd) => fd,
        P57FcFd::Closed => return Err(p57_closed_channel(ctx)),
        P57FcFd::Foreign => return Ok(Some(Value::Int(-1))),
    };
    let bb = match args.get(1) {
        Some(Value::Object(Some(b))) => *b,
        // A null destination buffer is a `NullPointerException`, not an
        // end-of-file — same reason as above.
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("FileChannel.read: null destination buffer".into()),
            }
            .into())
        }
    };
    // ByteBuffer: field 0 = backing array, 1 = position, 2 = limit.
    let bb_pos = ctx.get_field(bb, 1).as_int().unwrap_or(0) as usize;
    let bb_lim = ctx.get_field(bb, 2).as_int().unwrap_or(0) as usize;
    let remaining = bb_lim.saturating_sub(bb_pos);
    if remaining == 0 {
        return Ok(Some(Value::Int(0)));
    }
    let mut buf = vec![0u8; remaining];
    match ctx.fd_table().rw_read(fd_id, &mut buf) {
        Ok(0) => Ok(Some(Value::Int(-1))),
        Ok(n) => {
            if let Value::Object(Some(arr)) = ctx.get_field(bb, 0) {
                for (i, b) in buf.iter().take(n).enumerate() {
                    ctx.set_array_element(arr, bb_pos + i, Value::Int(*b as i8 as i32));
                }
            }
            ctx.set_field(bb, 1, Value::Int((bb_pos + n) as i32));
            Ok(Some(Value::Int(n as i32)))
        }
        Err(e) => Err(RuntimeError::IOException {
            message: e.to_string(),
        }
        .into()),
    }
}

/// `FileChannel.write(ByteBuffer)I`.
///
/// The closed arm reported the failure with the WRONG TYPE — a bare
/// `IOException("Channel closed")` — which is W7-8 §3.1 row 19: a
/// `catch (ClosedChannelException)` reopen branch does not match a supertype
/// instance, so the recovery path was deleted rather than taken. That is why
/// `Foreign` shares the arm here and nowhere else: this site already failed for
/// a foreign receiver, so tightening the type cannot turn a success into a
/// failure, and `ClosedChannelException extends IOException`.
pub fn p57_fc_write(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let fd_id = match p57_fc_fd(ctx, this) {
        P57FcFd::Open(fd) => fd,
        P57FcFd::Closed | P57FcFd::Foreign => return Err(p57_closed_channel(ctx)),
    };
    let bb = match args.get(1) {
        Some(Value::Object(Some(b))) => *b,
        // `0` is a legal return from `write` (a buffer with no remaining
        // bytes), so answering it for a null source hid the NPE the JDK
        // raises behind an outcome the caller reads as "nothing to do".
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("FileChannel.write: null source buffer".into()),
            }
            .into())
        }
    };
    let bb_pos = ctx.get_field(bb, 1).as_int().unwrap_or(0) as usize;
    let bb_lim = ctx.get_field(bb, 2).as_int().unwrap_or(0) as usize;
    let remaining = bb_lim.saturating_sub(bb_pos);
    if remaining == 0 {
        return Ok(Some(Value::Int(0)));
    }
    let mut data = vec![0u8; remaining];
    if let Value::Object(Some(arr)) = ctx.get_field(bb, 0) {
        for (i, d) in data.iter_mut().enumerate() {
            *d = ctx.get_array_element(arr, bb_pos + i).as_int().unwrap_or(0) as u8;
        }
    }
    let n = ctx
        .fd_table()
        .rw_write(fd_id, &data)
        .map_err(|e| RuntimeError::IOException {
            message: e.to_string(),
        })?;
    ctx.set_field(bb, 1, Value::Int((bb_pos + n) as i32));
    Ok(Some(Value::Int(n as i32)))
}

/// `FileChannel.close()V`.
///
/// Registered on the literal `java/nio/channels/FileChannel` to service the
/// synthetic single-field channel, but native overrides shadow ALL dispatch for
/// that class name — including a real `sun/nio/ch/FileChannelImpl` reaching an
/// INHERITED method (`close()V` is declared in the grandparent
/// `AbstractInterruptibleChannel`, not on `FileChannel`) through a
/// `FileChannel`-typed call site. That interception is measured, not inferred:
/// docs/known-issues/h2/!bug-h2-testlob-mvstore-chunk-not-found-and-file-lock.md.
/// So a real instance is detected and
/// `AbstractInterruptibleChannel.close()`'s contract replicated by calling the
/// real `implCloseChannel()` bytecode instead of treating the private slot as an
/// fd.
///
/// Idempotent by contract — "Closing a previously closed stream has no effect" —
/// and the descriptor release stays unconditional and unchecked so an error
/// cannot strand a handle.
pub fn p57_fc_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let class_name = ctx.class_name_of_id(ctx.class_id_of_object(this));
    if class_name.as_deref() != Some(cratonvm_native_api::synthetic_file_channel::CLASS) {
        if matches!(ctx.get_field_by_name(this, "closed"), Value::Int(1)) {
            return Ok(None);
        }
        ctx.set_field_by_name(this, "closed", Value::Int(1));
        ctx.invoke_virtual(this, "implCloseChannel", "()V", &[])?;
        return Ok(None);
    }
    if let P57FcFd::Open(fd_id) = p57_fc_fd(ctx, this) {
        let _ = ctx.fd_table().close(fd_id);
    }
    // Mark the REAL `AbstractInterruptibleChannel.closed` field, exactly as the
    // foreign-receiver arm above does. It is the field `isOpen()` — ours and the
    // JDK's — reads, and it is now free to hold the truth: the file position
    // used to occupy it (W7-72-ssc-socket-and-filechannel.md), which is why the
    // old body could not use it and answered a constant instead. A no-op on a
    // fabricated stub that declares no such field, which is the synthetic-JDK
    // arm's previous behaviour unchanged.
    cratonvm_native_api::synthetic_file_channel::set_fd_value(ctx, this, Value::Int(-1));
    ctx.set_field_by_name(this, "closed", Value::Int(1));
    Ok(None)
}

/// `FileChannel.isOpen()Z` — one body for both receiver kinds, deliberately.
///
/// `!closed` is what the real `AbstractInterruptibleChannel.isOpen()` bytecode
/// answers, and now that no native writes the file position into `closed` the
/// synthetic channel can be asked the same question as a real one. The old
/// literal-class arm returned a constant `1`, so `close(); isOpen()` reported the
/// channel open forever.
///
/// Safe in BOTH directions, which is the property to preserve if this is ever
/// touched again: an ordinary open channel has `closed` unset and answers TRUE
/// (nothing writes that field except `close()`), and a closed one answers FALSE.
/// It does NOT consult the fd, so a channel minted without one cannot be
/// reported closed by mistake — and that is also why it needs no [`P57FcFd`]
/// arm.
pub fn p57_fc_is_open(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Int(
        if matches!(ctx.get_field_by_name(this, "closed"), Value::Int(1)) {
            0
        } else {
            1
        },
    )))
}

pub fn register_phase57_file_channel(r: &mut NativeMethodRegistry) {
            let fc = "java/nio/channels/FileChannel";

    // `close()V` below writes -1 into slot 0, so `fd_id < 0` is exactly "this
    // channel is closed" (or was never opened). Every method here is specified
    // to raise `ClosedChannelException` in that state; the arms that instead
    // answered `-1` (read), `0` (transfer) or `this` (position/truncate) were
    // handing back a value the caller cannot tell from a real end-of-file, a
    // real empty transfer, or a real seek. See `p57_closed_channel`.

    // position()J, position(J)FileChannel and size()J — three of the seven
    // triples `register_phase57_nio_file` ALSO registers. One body each, shared
    // with that registrar; see the banner above this function for why sharing
    // rather than deleting is the repair.
    r.register(fc, "position", "()J", p57_fc_position, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        fc,
        "position",
        "(J)Ljava/nio/channels/FileChannel;",
        p57_fc_position_set, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(fc, "size", "()J", p57_fc_size, cratonvm_native_api::NativeKind::Bridge);

    // truncate(J)FileChannel
    r.register(
        fc,
        "truncate",
        "(J)Ljava/nio/channels/FileChannel;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let fd_id = cratonvm_native_api::synthetic_file_channel::fd_value(ctx, this)
                .as_int()
                .unwrap_or(-1);
            let new_len = match args.get(1) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            // "@throws IllegalArgumentException If the new size is negative"
            // (`FileChannel.truncate(long)`). This is the most destructive
            // clamp in the file: `new_len.max(0)` turned `truncate(-1)` — the
            // shape a caller's off-by-one length computation produces — into
            // `rw_set_length(0)`, i.e. it DELETED the file's entire contents
            // and returned `this` as if that had been asked for.
            if new_len < 0 {
                return Err(RuntimeError::IllegalArgumentException {
                    // HotSpot's message is the bare literal, no value echoed
                    // (`apps/probes/L4Diag`, measured against HotSpot 25).
                    message: "Negative size".to_string(),
                }
                .into());
            }
            if fd_id < 0 {
                return Err(p57_closed_channel(ctx));
            }
            ctx.fd_table()
                .rw_set_length(fd_id as u32, new_len as u64)
                .map_err(|e| RuntimeError::IOException {
                    message: e.to_string(),
                })?;
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // read(ByteBuffer)I and write(ByteBuffer)I — shared with
    // `register_phase57_nio_file`; bodies and rationale at [`p57_fc_read`] /
    // [`p57_fc_write`].
    r.register(fc, "read", "(Ljava/nio/ByteBuffer;)I", p57_fc_read, cratonvm_native_api::NativeKind::Bridge);
    r.register(fc, "write", "(Ljava/nio/ByteBuffer;)I", p57_fc_write, cratonvm_native_api::NativeKind::Bridge);

    // read(ByteBuffer, long position)I — positional read
    r.register(fc, "read", "(Ljava/nio/ByteBuffer;J)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = cratonvm_native_api::synthetic_file_channel::fd_value(ctx, this)
            .as_int()
            .unwrap_or(-1);
        let position = match args.get(2) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        // "@throws IllegalArgumentException If the position is negative or the
        // buffer is read-only" (`FileChannel.read(ByteBuffer,long)`). The
        // `position.max(0)` below read from offset 0 instead and reported the
        // bytes as if they had come from the requested offset.
        if position < 0 {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("Negative position: {position}"),
            }
            .into());
        }
        // See the non-positional `read` above: `-1` here is end-of-file.
        if fd_id < 0 {
            return Err(p57_closed_channel(ctx));
        }
        let bb = match args.get(1) {
            Some(Value::Object(Some(b))) => *b,
            _ => {
                return Err(RuntimeError::NullPointerException {
                    message: Some("FileChannel.read: null destination buffer".into()),
                }
                .into())
            }
        };
        let bb_pos = ctx.get_field(bb, 1).as_int().unwrap_or(0) as usize;
        let bb_lim = ctx.get_field(bb, 2).as_int().unwrap_or(0) as usize;
        let remaining = bb_lim.saturating_sub(bb_pos);
        if remaining == 0 {
            return Ok(Some(Value::Int(0)));
        }
        let mut buf = vec![0u8; remaining];
        match ctx
            .fd_table()
            .pread_at(fd_id as u32, &mut buf, position as u64)
        {
            Ok(0) => Ok(Some(Value::Int(-1))),
            Ok(n) => {
                if let Value::Object(Some(arr)) = ctx.get_field(bb, 0) {
                    for i in 0..n {
                        ctx.set_array_element(arr, bb_pos + i, Value::Int(buf[i] as i8 as i32));
                    }
                }
                ctx.set_field(bb, 1, Value::Int((bb_pos + n) as i32));
                Ok(Some(Value::Int(n as i32)))
            }
            Err(e) => Err(RuntimeError::IOException {
                message: e.to_string(),
            }
            .into()),
        }
    }, cratonvm_native_api::NativeKind::Bridge);

    // write(ByteBuffer, long position)I — positional write
    r.register(fc, "write", "(Ljava/nio/ByteBuffer;J)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = cratonvm_native_api::synthetic_file_channel::fd_value(ctx, this)
            .as_int()
            .unwrap_or(-1);
        let position = match args.get(2) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        // "@throws IllegalArgumentException If the position is negative"
        // (`FileChannel.write(ByteBuffer,long)`). `position.max(0)` did not
        // merely lose the refusal: it OVERWROTE the first `remaining` bytes of
        // the file and returned that byte count as a successful write at the
        // offset the caller asked for.
        if position < 0 {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("Negative position: {position}"),
            }
            .into());
        }
        if fd_id < 0 {
            return Err(p57_closed_channel(ctx));
        }
        let bb = match args.get(1) {
            Some(Value::Object(Some(b))) => *b,
            _ => {
                return Err(RuntimeError::NullPointerException {
                    message: Some("FileChannel.write: null source buffer".into()),
                }
                .into())
            }
        };
        let bb_pos = ctx.get_field(bb, 1).as_int().unwrap_or(0) as usize;
        let bb_lim = ctx.get_field(bb, 2).as_int().unwrap_or(0) as usize;
        let remaining = bb_lim.saturating_sub(bb_pos);
        if remaining == 0 {
            return Ok(Some(Value::Int(0)));
        }
        let mut data = vec![0u8; remaining];
        if let Value::Object(Some(arr)) = ctx.get_field(bb, 0) {
            for i in 0..remaining {
                data[i] = ctx.get_array_element(arr, bb_pos + i).as_int().unwrap_or(0) as u8;
            }
        }
        let n = ctx
            .fd_table()
            .pwrite_at(fd_id as u32, &data, position as u64)
            .map_err(|e| RuntimeError::IOException {
                message: e.to_string(),
            })?;
        ctx.set_field(bb, 1, Value::Int((bb_pos + n) as i32));
        Ok(Some(Value::Int(n as i32)))
    }, cratonvm_native_api::NativeKind::Bridge);

    // transferTo(long position, long count, WritableByteChannel target)J
    r.register(
        fc,
        "transferTo",
        "(JJLjava/nio/channels/WritableByteChannel;)J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let fd_id = cratonvm_native_api::synthetic_file_channel::fd_value(ctx, this)
                .as_int()
                .unwrap_or(-1);
            let position = match args.get(1) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let count = match args.get(2) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let target = match args.get(3) {
                Some(Value::Object(Some(t))) => *t,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("FileChannel.transferTo: null target channel".into()),
                    }
                    .into())
                }
            };
            // "@throws IllegalArgumentException If the preconditions on the
            // parameters do not hold", where both `position` and `count` "must
            // be non-negative". `count <= 0` folded the illegal case in with
            // the legal `count == 0`, and both answered `0` — which is also
            // what a successful transfer of an empty region returns, so the
            // caller could not tell a rejected request from a completed one.
            if position < 0 || count < 0 {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!("transferTo: position={position}, count={count}"),
                }
                .into());
            }
            if fd_id < 0 {
                return Err(p57_closed_channel(ctx));
            }
            if count == 0 {
                return Ok(Some(Value::Long(0)));
            }
            // BUG nb-phases-late(2): a huge `count` (up to Long.MAX_VALUE) was fed
            // straight into `vec![0u8; count as usize]` → exabyte allocation abort.
            // The JDK clamps the transfer to the bytes actually available
            // (file_size - position) and streams through a bounded buffer. Clamp
            // `count` to what remains in the source file, then loop in chunks so
            // the per-iteration allocation is bounded regardless of `count`.
            // (`position` is already known non-negative — the range check above
            // replaced the `position.max(0)` that used to sit here.)
            let file_size = ctx.fd_table().file_size(fd_id as u32).unwrap_or(0) as i64;
            let available = (file_size - position).max(0);
            // Clamp to the bytes available in the source file when we have a real
            // size; for non-regular/unknown-size fds (file_size==0) fall back to the
            // caller's `count` and let the chunked read loop stop at EOF — this keeps
            // the per-iteration allocation bounded either way.
            let mut to_transfer = if file_size > 0 {
                count.min(available)
            } else {
                count
            };
            if to_transfer <= 0 {
                return Ok(Some(Value::Long(0)));
            }
            // Bounded streaming buffer (8 MiB) — matches the JDK's chunked fallback
            // when a true zero-copy sendfile is unavailable.
            const FC_XFER_CHUNK: i64 = 8 * 1024 * 1024;
            // Pin across the per-chunk allocs / write callbacks below — a
            // moving young GC there would relocate `target` (native
            // stale-local family).
            let target_pin = ctx.pin_native_root(target);
            let mut total_written: i64 = 0;
            let mut cur_pos = position;
            while to_transfer > 0 {
                let chunk = to_transfer.min(FC_XFER_CHUNK) as usize;
                let mut buf = vec![0u8; chunk];
                // `unwrap_or(0)` turned a failed source read into `n == 0`,
                // which this loop treats as end-of-file: the method then
                // returned the bytes copied so far as a normal short transfer.
                // A short transfer is legal ("the number of bytes, possibly
                // zero, that were actually transferred"), so the caller had no
                // way to see that the rest of the file was never read. The
                // spec's answer for a source-read failure is `IOException`.
                let n = match ctx
                    .fd_table()
                    .pread_at(fd_id as u32, &mut buf, cur_pos as u64)
                {
                    Ok(n) => n,
                    Err(e) => {
                        ctx.unpin_native_roots(target_pin);
                        return Err(RuntimeError::IOException {
                            message: e.to_string(),
                        }
                        .into());
                    }
                };
                if n == 0 {
                    break;
                }
                // Wrap in a ByteBuffer and call target.write(ByteBuffer)
                let byte_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, n);
                for i in 0..n {
                    ctx.set_array_element(byte_arr, i, Value::Int(buf[i] as i8 as i32));
                }
                let byte_arr_pin = ctx.pin_native_root(byte_arr);
                let bb = try_alloc_concurrent_synthetic(ctx, "java/nio/ByteBuffer", 3)?;
                let byte_arr = ctx.read_native_pin(byte_arr_pin, byte_arr);
                ctx.set_field(bb, 0, Value::Object(Some(byte_arr)));
                ctx.set_field(bb, 1, Value::Int(0));
                ctx.set_field(bb, 2, Value::Int(n as i32));
                let target_cur = ctx.read_native_pin(target_pin, target);
                let write_res = ctx.invoke_virtual(
                    target_cur,
                    "write",
                    "(Ljava/nio/ByteBuffer;)I",
                    &[Value::Object(Some(bb))],
                );
                let written = match write_res {
                    Ok(Some(Value::Int(w))) if w >= 0 => w as i64,
                    // The old `_ => n as i64` arm counted the whole chunk as
                    // transferred when the TARGET's `write` threw — so a full
                    // disk or a closed target produced a return value equal to
                    // the bytes requested, i.e. the exact signature of a
                    // complete transfer, with the exception discarded.
                    // `transferTo` propagates whatever the target raised.
                    Err(e) => {
                        ctx.unpin_native_roots(target_pin);
                        return Err(e);
                    }
                    // Anything else is a target that did not answer an `int`;
                    // count nothing and let the short-write break below stop
                    // the loop rather than claiming `n` bytes landed.
                    _ => 0,
                };
                ctx.unpin_native_roots(byte_arr_pin);
                total_written += written;
                cur_pos += n as i64;
                to_transfer -= n as i64;
                // Short write from the target or short read from the source: stop,
                // mirroring the JDK which returns the bytes transferred so far.
                if (written as usize) < n || n < chunk {
                    break;
                }
            }
            ctx.unpin_native_roots(target_pin);
            Ok(Some(Value::Long(total_written)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // transferFrom(ReadableByteChannel src, long position, long count)J
    r.register(
        fc,
        "transferFrom",
        "(Ljava/nio/channels/ReadableByteChannel;JJ)J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let fd_id = cratonvm_native_api::synthetic_file_channel::fd_value(ctx, this)
                .as_int()
                .unwrap_or(-1);
            let src = match args.get(1) {
                Some(Value::Object(Some(s))) => *s,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("FileChannel.transferFrom: null source channel".into()),
                    }
                    .into())
                }
            };
            let position = match args.get(2) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let count = match args.get(3) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            // Same contract as `transferTo`: both parameters "must be
            // non-negative", and violating them is `IllegalArgumentException`.
            // A negative `position` was worse here than on the read side —
            // `position.max(0)` below started WRITING the source's bytes over
            // the front of the destination file and returned that count as a
            // successful transfer to the requested offset.
            if position < 0 || count < 0 {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!("transferFrom: position={position}, count={count}"),
                }
                .into());
            }
            if fd_id < 0 {
                return Err(p57_closed_channel(ctx));
            }
            if count == 0 {
                return Ok(Some(Value::Long(0)));
            }
            // BUG nb-phases-late(2): `count` (a long) was used to size the
            // ByteBuffer via `new_array(count as usize)` (exabyte alloc abort for a
            // huge/Long.MAX_VALUE count) AND passed to the buffer's limit as
            // `Int(count as i32)` (silent 64→32 bit truncation). The JDK streams
            // through a bounded buffer in a loop. Clamp the per-iteration chunk to a
            // sane ceiling and loop until `count` is satisfied or the source is
            // exhausted, so the allocation is bounded and the limit never truncates.
            const FC_XFER_CHUNK: i64 = 8 * 1024 * 1024;
            // Pin across the per-chunk allocs / read callbacks below — a
            // moving young GC there would relocate `src` (native stale-local
            // family).
            let src_pin = ctx.pin_native_root(src);
            let mut remaining = count;
            // `position` is already known non-negative (range-checked above,
            // where the `position.max(0)` that used to live here was).
            let mut cur_pos = position;
            let mut total_written: i64 = 0;
            while remaining > 0 {
                let chunk = remaining.min(FC_XFER_CHUNK) as usize;
                // Allocate a bounded ByteBuffer and call src.read(ByteBuffer)
                let byte_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, chunk);
                let byte_arr_pin = ctx.pin_native_root(byte_arr);
                let bb = try_alloc_concurrent_synthetic(ctx, "java/nio/ByteBuffer", 3)?;
                let bb_pin = ctx.pin_native_root(bb);
                let byte_arr = ctx.read_native_pin(byte_arr_pin, byte_arr);
                ctx.set_field(bb, 0, Value::Object(Some(byte_arr)));
                ctx.set_field(bb, 1, Value::Int(0));
                // `chunk` <= FC_XFER_CHUNK so this Int cast never truncates.
                ctx.set_field(bb, 2, Value::Int(chunk as i32));
                let src_cur = ctx.read_native_pin(src_pin, src);
                let read_res = ctx.invoke_virtual(
                    src_cur,
                    "read",
                    "(Ljava/nio/ByteBuffer;)I",
                    &[Value::Object(Some(bb))],
                );
                let read_n = match read_res {
                    Ok(Some(Value::Int(n))) if n > 0 => n as usize,
                    // The `_ => 0` arm swallowed an exception from the SOURCE's
                    // `read` and fed it into the `read_n == 0` break below,
                    // which this method reports as end-of-source — a legal
                    // short transfer. `transferFrom` propagates it instead.
                    Err(e) => {
                        ctx.unpin_native_roots(src_pin);
                        return Err(e);
                    }
                    _ => 0,
                };
                let bb = ctx.read_native_pin(bb_pin, bb);
                ctx.unpin_native_roots(byte_arr_pin);
                if read_n == 0 {
                    break;
                }
                // Extract bytes from ByteBuffer (position is now read_n)
                let mut data = vec![0u8; read_n];
                if let Value::Object(Some(arr)) = ctx.get_field(bb, 0) {
                    for i in 0..read_n {
                        data[i] = ctx.get_array_element(arr, i).as_int().unwrap_or(0) as u8;
                    }
                }
                // `unwrap_or(0)` here lost the bytes AND the reason: the loop
                // kept consuming the source (`remaining -= read_n`) while
                // `cur_pos` stood still, so a destination that stopped
                // accepting writes produced a silent partial copy whose return
                // value looked like an ordinary short transfer.
                let written = match ctx
                    .fd_table()
                    .pwrite_at(fd_id as u32, &data, cur_pos as u64)
                {
                    Ok(w) => w,
                    Err(e) => {
                        ctx.unpin_native_roots(src_pin);
                        return Err(RuntimeError::IOException {
                            message: e.to_string(),
                        }
                        .into());
                    }
                };
                total_written += written as i64;
                cur_pos += written as i64;
                remaining -= read_n as i64;
                // Short read from the source means EOF: stop, returning bytes so far.
                if read_n < chunk {
                    break;
                }
            }
            ctx.unpin_native_roots(src_pin);
            Ok(Some(Value::Long(total_written)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // force(boolean metadata)V
    r.register(fc, "force", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = cratonvm_native_api::synthetic_file_channel::fd_value(ctx, this)
            .as_int()
            .unwrap_or(-1);
        // FIX (finding 2): honor durability instead of being a silent no-op.
        // `clone_file` flushes any buffered writer for the fd and hands back a
        // std::fs::File referring to the same kernel file; sync_data/sync_all then
        // issue a real fsync (POSIX fsync / Windows FlushFileBuffers under the hood).
        // `metadata == true` → flush file contents AND metadata (sync_all); false →
        // at least the file contents (sync_data). We only fall back to best-effort
        // (no error surfaced) when the fd genuinely isn't a real file (e.g. a pipe).
        //
        // The `if fd_id >= 0` guard used to have no else: `force()` on a CLOSED
        // channel returned normally, which is the one answer a durability
        // barrier must never fabricate — the caller's next act is to record the
        // data as committed. "@throws ClosedChannelException If this channel is
        // closed" (`FileChannel.force`).
        if fd_id < 0 {
            return Err(p57_closed_channel(ctx));
        }
        let metadata = args.get(1).and_then(|v| v.as_int()).unwrap_or(1) != 0;
        if let Ok(file) = ctx.fd_table().clone_file(fd_id as u32) {
            let res = if metadata {
                file.sync_all()
            } else {
                file.sync_data()
            };
            if let Err(e) = res {
                return Err(RuntimeError::IOException {
                    message: format!("FileChannel.force failed: {e}"),
                }
                .into());
            }
        }
        // Non-file-backed fd (pipe/socket/etc.): nothing to sync — best effort.
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // close()V and isOpen()Z — the last two of the seven shared triples.
    //
    // This registrar's copies used to be described as "the LOSING copy … nothing
    // here is reachable", which was true of the two SHIPPING modes and false of
    // `--synthetic-jdk`, where this registrar runs after `register_phase57_nio_file`
    // and therefore wins. Both copies now name the same function, so the
    // last-write-wins outcome for these triples is not load-bearing in any mode —
    // which is the only version of "it does not matter which wins" that is true.
    r.register(fc, "close", "()V", p57_fc_close, cratonvm_native_api::NativeKind::Bridge);
    r.register(fc, "isOpen", "()Z", p57_fc_is_open, cratonvm_native_api::NativeKind::Bridge);
    }

// =============================================================================
// java.nio.file.attribute — BasicFileAttributes, FileTime
// BasicFileAttributes = 5-field (creation=0, lastAccess=1, lastMod=2, isDir=3, size=4)
// FileTime = 1-field (millis=0 Long)
// =============================================================================

pub fn register_p59_file_attributes(r: &mut NativeMethodRegistry) {
            // FileTreeWalker hands `Files.find` the concrete platform attributes
    // implementation, and its predicate invokes `isRegularFile` virtually on
    // that class.  Registering only on the BasicFileAttributes interface is
    // insufficient when the interpreter resolves the concrete override first:
    // every file then appeared non-regular and Files.find produced an empty
    // stream for a mounted embedded JAR.
    for attrs_class in [
        "java/nio/file/attribute/BasicFileAttributes",
        "sun/nio/fs/WindowsFileAttributes",
        "sun/nio/fs/UnixFileAttributes",
    ] {
        r.register(
            attrs_class,
            "creationTime",
            "()Ljava/nio/file/attribute/FileTime;",
            |ctx, args| {
                // SBR wave 2 lane fs: a `UnixFileAttributes` receiver reaching
                // this callback through the `BasicFileAttributes` interface row
                // runs its own (retired) bytecode -- see
                // `yield_to_retired_receiver`. Same guard on all nine below.
                retired_receiver_yield!(
                    ctx,
                    args,
                    "creationTime",
                    "()Ljava/nio/file/attribute/FileTime;"
                );
                let this = obj_arg(args, 0)?;
                let millis = basic_file_attributes_time_millis(ctx, this, "creation");
                Ok(Some(Value::Object(Some(filetime_alloc(ctx, millis)?))))
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            attrs_class,
            "lastAccessTime",
            "()Ljava/nio/file/attribute/FileTime;",
            |ctx, args| {
                retired_receiver_yield!(
                    ctx,
                    args,
                    "lastAccessTime",
                    "()Ljava/nio/file/attribute/FileTime;"
                );
                let this = obj_arg(args, 0)?;
                let millis = basic_file_attributes_time_millis(ctx, this, "access");
                Ok(Some(Value::Object(Some(filetime_alloc(ctx, millis)?))))
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            attrs_class,
            "lastModifiedTime",
            "()Ljava/nio/file/attribute/FileTime;",
            |ctx, args| {
                retired_receiver_yield!(
                    ctx,
                    args,
                    "lastModifiedTime",
                    "()Ljava/nio/file/attribute/FileTime;"
                );
                let this = obj_arg(args, 0)?;
                let millis = basic_file_attributes_time_millis(ctx, this, "modified");
                Ok(Some(Value::Object(Some(filetime_alloc(ctx, millis)?))))
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(attrs_class, "isDirectory", "()Z", |ctx, args| {
            retired_receiver_yield!(ctx, args, "isDirectory", "()Z");
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Int(i32::from(basic_file_attributes_is_dir(
                ctx, this,
            )))))
        }, cratonvm_native_api::NativeKind::Bridge);
        // Was `!isDirectory()`, which called a symlink read under
        // NOFOLLOW_LINKS a regular file. Check the real type bits instead.
        r.register(attrs_class, "isRegularFile", "()Z", |ctx, args| {
            retired_receiver_yield!(ctx, args, "isRegularFile", "()Z");
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Int(i32::from(
                basic_file_attributes_is_regular(ctx, this),
            ))))
        }, cratonvm_native_api::NativeKind::Bridge);
        // Was a hardcoded `false`, so `Files.readAttributes(p, …,
        // NOFOLLOW_LINKS).isSymbolicLink()` could never be true no matter what
        // was on disk — a caller walking a tree had no way to spot a link and
        // would follow it. Read the same `st_mode`/`fileAttrs` bits the other
        // predicates use; `p59_files_read_attributes` now populates them.
        r.register(attrs_class, "isSymbolicLink", "()Z", |ctx, args| {
            retired_receiver_yield!(ctx, args, "isSymbolicLink", "()Z");
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Int(i32::from(
                basic_file_attributes_is_symlink(ctx, this),
            ))))
        }, cratonvm_native_api::NativeKind::Bridge);
        // Was a hardcoded `false` on the claim that nothing this VM's Path
        // surface can name is a device/socket/FIFO — but a `Path` is just a
        // string, and `Files.readAttributes` on `/dev/*`, `/proc/*` or a unix
        // socket lands here. The real bodies (`UnixFileAttributes.isOther()`,
        // `WindowsFileAttributes.isOther()`) are literally
        // `!isRegularFile() && !isDirectory() && !isSymbolicLink()`, and all
        // three of those already read the real `st_mode` / `fileAttrs` bits
        // that `p59_files_read_attributes` populates. Compose them.
        r.register(attrs_class, "isOther", "()Z", |ctx, args| {
            retired_receiver_yield!(ctx, args, "isOther", "()Z");
            let this = obj_arg(args, 0)?;
            let other = !basic_file_attributes_is_regular(ctx, this)
                && !basic_file_attributes_is_dir(ctx, this)
                && !basic_file_attributes_is_symlink(ctx, this);
            Ok(Some(Value::Int(i32::from(other))))
        }, cratonvm_native_api::NativeKind::Bridge);
        r.register(attrs_class, "size", "()J", |ctx, args| {
            retired_receiver_yield!(ctx, args, "size", "()J");
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Long(basic_file_attributes_size(ctx, this))))
        }, cratonvm_native_api::NativeKind::Bridge);
        // Was an unconditional `null` — legal on paper (the JDK returns null on
        // FAT-class volumes / network shares) but it meant NO file this VM ever
        // described had an identity, so `FileTreeWalker.wouldLoop` could never
        // detect a symlink cycle and `Files.isSameFile`-style checks had nothing
        // to compare. `p59_files_read_attributes` now records the real OS
        // identity; see `basic_file_attributes_file_key`.
        r.register(
            attrs_class,
            "fileKey",
            "()Ljava/lang/Object;",
            |ctx, args| {
                retired_receiver_yield!(ctx, args, "fileKey", "()Ljava/lang/Object;");
                let this = obj_arg(args, 0)?;
                // The real key OBJECT -- see
                // `basic_file_attributes_file_key_object` for why a string that
                // prints identically is not good enough.
                Ok(Some(
                    basic_file_attributes_file_key_object(ctx, this).unwrap_or(Value::Object(None)),
                ))
            }, cratonvm_native_api::NativeKind::Bridge,
        );
    }

    // FileTime — see `filetime_alloc` / `filetime_read_millis`. The millis is
    // stored in the real `long value` field (by name) so descriptor coercion
    // doesn't destroy it; slot 0 of a real-JDK-bound FileTime is a *reference*
    // field (`instant`/`unit` cache), so a raw `set_field(ft, 0, Long)` was
    // coerced to null — `toMillis()` returned 0 (vs the real mtime on HotSpot).
    let ft = "java/nio/file/attribute/FileTime";
    r.register(ft, "toMillis", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Long(filetime_read_millis(ctx, this))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        ft,
        "fromMillis",
        "(J)Ljava/nio/file/attribute/FileTime;",
        |ctx, args| {
            let millis = match args.first() {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            let obj = filetime_alloc(ctx, millis)?;
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ft,
        "compareTo",
        "(Ljava/nio/file/attribute/FileTime;)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = obj_arg(args, 1)?;
            let a = filetime_read_millis(ctx, this);
            let b = filetime_read_millis(ctx, other);
            Ok(Some(Value::Int(a.cmp(&b) as i32)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(ft, "toString", "()Ljava/lang/String;", |ctx, args| {
        // Real FileTime.toString is the ISO-8601 instant (was "{millis}ms").
        let this = obj_arg(args, 0)?;
        let millis = filetime_read_millis(ctx, this);
        let inst = match ctx.invoke(
            "java/time/Instant",
            "ofEpochMilli",
            "(J)Ljava/time/Instant;",
            &[Value::Long(millis)],
        )? {
            Some(Value::Object(Some(o))) => o,
            _ => {
                let s = ctx.create_string("");
                return Ok(Some(Value::Object(Some(s))));
            }
        };
        ctx.invoke_virtual(inst, "toString", "()Ljava/lang/String;", &[])
    }, cratonvm_native_api::NativeKind::Bridge);

    // The native bridge supplies the concrete attributes object directly, so
    // it must preserve the provider's requested-type contract itself. In
    // particular, Windows cannot manufacture a `PosixFileAttributes` object:
    // returning `WindowsFileAttributes` makes generic callers observe a
    // successful read (or a later bad cast) instead of the JDK's documented
    // `UnsupportedOperationException` fallback.
    reg_files! { r.register("java/nio/file/Files",
        "readAttributes",
        "(Ljava/nio/file/Path;Ljava/lang/Class;[Ljava/nio/file/LinkOption;)Ljava/nio/file/attribute/BasicFileAttributes;",
        |ctx, args| {
            // `readAttributes(path, null)` is an NPE. It used to fall through
            // to the BASIC view, which is the one answer that cannot be
            // distinguished from a correct call — a caller whose `type`
            // reference is null by mistake got a plausible attributes object
            // back. MEASURED against HotSpot.
            if matches!(args.get(1), Some(Value::Object(None))) {
                return Err(RuntimeError::NullPointerException { message: None }.into());
            }
            #[cfg(windows)]
            {
                // Only a PRESENT `Class` argument states a requested type. A
                // null/absent one carries no request at all, and rejecting it
                // produced the nonsense `File attribute type  is not supported
                // on Windows` (note the empty name) for a caller that never
                // asked for anything unsupported. Fall back to the method's
                // declared return type, `BasicFileAttributes`, which Windows
                // does support.
                if let Some(Value::Object(Some(class))) = args.get(1) {
                    let requested_type =
                        crate::lang_class::mirror_class_name(ctx, *class).unwrap_or_default();
                    if !requested_type.is_empty()
                        && !windows_supports_file_attributes_type(&requested_type)
                    {
                        return Err(RuntimeError::UnsupportedOperationException {
                            message: format!(
                                "File attribute type {requested_type} is not supported on Windows"
                            ),
                        }
                        .into());
                    }
                }
            }
            let path = args.first().copied().unwrap_or(Value::Object(None));
            let options = args.get(2).copied().unwrap_or(Value::Object(None));
            let attrs = p59_files_read_attributes(ctx, &[path, options]);
            // A request for `BasicFileAttributes` gets the BASIC view, not the
            // full platform object — see `narrow_to_basic_view`.
            let requested = args.get(1).copied();
            narrow_to_basic_view(ctx, requested.as_ref(), attrs)
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }
    reg_files! { r.register("java/nio/file/Files",
        "getLastModifiedTime",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Ljava/nio/file/attribute/FileTime;",
        |ctx, args| {
            let path_str = extract_path_string(ctx, args.first());
            // NIO contract: a missing file must raise `NoSuchFileException`
            // (see `newByteChannel` above), not silently answer with a
            // zero/epoch `FileTime` — callers like
            // `FileSystemResource.lastModified()` explicitly catch
            // `NoSuchFileException` and translate it to
            // `FileNotFoundException` (ResourceTests#resourceCreateRelativeUnknown).
            let meta = match std::fs::metadata(&path_str) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(p57_no_such_file(ctx, &path_str)?);
                }
                Err(e) => return Err(p57_io_error(&e)),
            };
            let millis = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let ft = filetime_alloc(ctx, millis)?;
            Ok(Some(Value::Object(Some(ft))))
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }
    }

/// Allocate a `java.nio.file.attribute.FileTime` carrying `millis`.
///
/// In real-JDK mode the object binds to the real `FileTime` class (so a
/// `checkcast FileTime` / `instanceof FileTime` in `BasicFileAttributes`
/// consumers holds). The millis is written into the real `long value` field
/// **by name** so descriptor coercion keeps it a `Long`. A raw slot-0 write is
/// only used as the synthetic-jdk-mode fallback (no named `value` field):
/// slot 0 of a real FileTime is a *reference* field, so a `Long` written there
/// is coerced to null — the overlay-on-real-class bug that made `toMillis()`
/// return 0 instead of the file's mtime.
pub fn filetime_alloc(
    ctx: &mut dyn NativeContext,
    millis: i64,
) -> Result<ObjectRef, MethodCallFailed> {
    // gc-common w20-d: `TimeUnit` is initialised BEFORE the FileTime is
    // allocated. Its first initialisation runs `<clinit>` (Java, allocating
    // the constants), and the fresh FileTime used to be written and returned
    // through its address from before it.
    let tu_cid = ctx.ensure_class_initialized("java/util/concurrent/TimeUnit");
    let ft = try_alloc_concurrent_synthetic(ctx, "java/nio/file/attribute/FileTime", 1)?;
    ctx.set_field_by_name(ft, "value", Value::Long(millis));
    // Set the real `unit` field to TimeUnit.MILLISECONDS. The raw-alloc path
    // skips FileTime's constructor, so `unit` and the cached `instant` are both
    // null. Real FileTime bytecode that has no native override — notably
    // `to(TimeUnit)` (commons-compress's `FileTimes.toUnixTime` → tar entry
    // mtime) — branches on `unit`: when null it dereferences the (null)
    // `instant`, NPEing on `instant.getEpochSecond()`. With `unit` set, all the
    // real value+unit math works (to/toMillis/toInstant/compareTo).
    if let Ok(tu_cid) = tu_cid {
        if let Some(idx) = ctx.static_field_index_by_name(tu_cid, "MILLISECONDS") {
            let ms = ctx.get_static_field(tu_cid, idx);
            if matches!(ms, Value::Object(Some(_))) {
                ctx.set_field_by_name(ft, "unit", ms);
            }
        }
    }
    // Only fall back to slot 0 when the real `value` field is absent
    // (synthetic-jdk stub) — touching slot 0 of a real FileTime would clobber
    // a reference cache field.
    if !matches!(ctx.get_field_by_name(ft, "value"), Value::Long(_)) {
        ctx.set_field(ft, 0, Value::Long(millis));
    }
    Ok(ft)
}

/// Read a `FileTime`'s instant as epoch millis -- from ANY `FileTime`, not only
/// one [`filetime_alloc`] built.
///
/// # `value` is not the time. `(value, unit)` is.
///
/// The real class stores a PAIR, and the unit is `null` in the shape
/// `FileTime.from(Instant)` produces -- `new FileTime(0L, null, instant)`,
/// identically on 17, 21 and 25 (`javap -c`), with the time in `instant` and
/// `value` a literal `lconst_0`. So reading `value` and calling it millis
/// answered **0, the epoch**, for every `FileTime` real bytecode had built, and
/// `BasicFileAttributeView.setTimes` then stamped the file 1970-01-01.
///
/// This was self-consistent for exactly as long as this VM's natives were the
/// only PRODUCER: [`filetime_alloc`] converts to millis before storing and
/// writes `unit = MILLISECONDS`, so writer and reader agreed on a convention
/// that is nowhere in the class. Retiring `from`/`fromMillis` makes real
/// bytecode a producer, and the convention is then whatever the JDK chose. Lane
/// 4 wave 5 -- the corpus vector is `RFileTimes`, which no probe reproduced.
///
/// The unit is honoured rather than assumed, so a `FileTime` in SECONDS or
/// NANOSECONDS reads correctly too; that is the same defect one conversion
/// earlier, and it was live for `FileTime.from(3L, SECONDS)` before this.
pub fn filetime_read_millis(ctx: &dyn NativeContext, ft: ObjectRef) -> i64 {
    // The unit says what `value` MEANS, so it is read first and `value` is
    // never trusted on its own while a unit is present.
    if let Value::Object(Some(unit)) = ctx.get_field_by_name(ft, "unit") {
        if let Value::Long(value) = ctx.get_field_by_name(ft, "value") {
            if let Some(millis) = time_unit_to_millis(ctx, unit, value) {
                return millis;
            }
        }
    }
    // `unit == null` is the `from(Instant)` shape. Read the same two named
    // fields the `from(Ljava/time/Instant;)` native reads, and for the same
    // reason: they are the real `java.time.Instant`'s own.
    if let Value::Object(Some(instant)) = ctx.get_field_by_name(ft, "instant") {
        let secs = match ctx.get_field_by_name(instant, "seconds") {
            Value::Long(v) => v,
            _ => 0,
        };
        let nanos = match ctx.get_field_by_name(instant, "nanos") {
            Value::Int(v) => v,
            _ => 0,
        };
        return secs.saturating_mul(1000) + (nanos as i64) / 1_000_000;    }
    // A `FileTime` with neither -- the legacy shape [`filetime_alloc`] writes
    // when `TimeUnit` could not be initialized, and then the synthetic-jdk
    // stub, which declares no `value` at all.
    if let Value::Long(v) = ctx.get_field_by_name(ft, "value") {
        return v;
    }
    match ctx.get_field(ft, 0) {
        Value::Long(v) => v,
        _ => 0,
    }
}

/// `TimeUnit.toMillis(value)` without a virtual call, and `None` for a receiver
/// that is not a `TimeUnit` constant this understands.
///
/// **It cannot dispatch.** Its only caller is [`filetime_read_millis`], which is
/// itself the body registered for `FileTime.toMillis()`, so going through the VM
/// would re-enter this function whenever that native is the thing that wins --
/// which is every call in compatible mode.
///
/// The constant is identified by its `Enum.name` and not by its ordinal. An
/// ordinal read is a slot read, and this file already carries a defect from
/// assuming the `name`/`ordinal` slot ORDER (see `posix_file_permission_stub_
/// clinit`, which wrote the ordinal where `name()` looks and produced a
/// nameless enum constant).
pub fn time_unit_to_millis(ctx: &dyn NativeContext, unit: ObjectRef, value: i64) -> Option<i64> {
    let name = match ctx.get_field_by_name(unit, "name") {
        Value::Object(Some(s)) => ctx.read_string(s)?,
        _ => return None,
    };
    Some(match name.as_str() {
        "NANOSECONDS" => value / 1_000_000,
        "MICROSECONDS" => value / 1_000,
        "MILLISECONDS" => value,
        "SECONDS" => value.saturating_mul(1_000),
        "MINUTES" => value.saturating_mul(60_000),
        "HOURS" => value.saturating_mul(3_600_000),
        "DAYS" => value.saturating_mul(86_400_000),
        _ => return None,
    })
}

/// Apply the non-null fields passed to `BasicFileAttributeView.setTimes`.
/// `filetime` provides portable access/modified-time updates; Windows also
/// exposes a mutable creation time, which is handled with `SetFileTime`.
pub fn set_file_attribute_times(
    path: &str,
    creation_millis: Option<i64>,
    access_millis: Option<i64>,
    modified_millis: Option<i64>,
) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        return set_file_attribute_times_windows(
            path,
            creation_millis,
            access_millis,
            modified_millis,
        );
    }

    #[cfg(not(windows))]
    {
        let _ = creation_millis;
        if let Some(access_millis) = access_millis {
            filetime::set_file_atime(path, filetime_from_millis(access_millis))?;
        }
        if let Some(modified_millis) = modified_millis {
            filetime::set_file_mtime(path, filetime_from_millis(modified_millis))?;
        }
        Ok(())
    }
}

pub fn filetime_from_millis(millis: i64) -> filetime::FileTime {
    let seconds = millis.div_euclid(1_000);
    let nanos = (millis.rem_euclid(1_000) * 1_000_000) as u32;
    filetime::FileTime::from_unix_time(seconds, nanos)
}

#[cfg(windows)]
pub fn set_file_attribute_times_windows(
    path: &str,
    creation_millis: Option<i64>,
    access_millis: Option<i64>,
    modified_millis: Option<i64>,
) -> std::io::Result<()> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::ptr;

    #[repr(C)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    unsafe extern "system" {
        fn SetFileTime(
            file: *mut std::ffi::c_void,
            creation: *const FileTime,
            access: *const FileTime,
            modified: *const FileTime,
        ) -> i32;
    }

    fn as_filetime(millis: i64) -> FileTime {
        let ticks = (i128::from(millis) + 11_644_473_600_000i128) * 10_000i128;
        let ticks = ticks as u64;
        FileTime {
            low: ticks as u32,
            high: (ticks >> 32) as u32,
        }
    }

    let creation = creation_millis.map(as_filetime);
    let access = access_millis.map(as_filetime);
    let modified = modified_millis.map(as_filetime);
    let file = OpenOptions::new()
        .write(true)
        .custom_flags(0x0200_0000)
        .open(path)?;
    let result = unsafe {
        SetFileTime(
            file.as_raw_handle().cast(),
            creation.as_ref().map_or(ptr::null(), |time| time),
            access.as_ref().map_or(ptr::null(), |time| time),
            modified.as_ref().map_or(ptr::null(), |time| time),
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Allocate a real platform `BasicFileAttributes` implementation.
///
/// Callers perform a JVM `checkcast BasicFileAttributes`, so the carrier must
/// actually implement that interface. The native bridge stores its canonical
/// values in named platform fields and force-dispatches the interface methods;
/// it never relies on the implementation's bytecode layout.
pub fn basic_file_attributes_alloc(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    let class_name = if cfg!(windows) {
        "sun/nio/fs/WindowsFileAttributes"
    } else {
        "sun/nio/fs/UnixFileAttributes"
    };
    if let Ok(class_id) = ctx.ensure_class_initialized(class_name) {
        let fields = ctx.class_num_total_fields(class_id);
        if ctx.class_name_arc_of_id(class_id).as_deref() == Some(class_name) && fields > 0 {
            return Ok(ctx.alloc_object(class_id, fields));
        }
    }
    // Only reached by a synthetic-JDK configuration, where the interface is
    // modelled as a concrete helper with a five-field layout.
    Ok(try_alloc_concurrent_synthetic(
        ctx,
        "java/nio/file/attribute/BasicFileAttributes",
        5,
    )?)
}

/// Narrow a platform attributes object to the BASIC view, when — and only
/// when — the caller asked for `BasicFileAttributes` by name.
///
/// # What the caller can tell, and could not before
///
/// `basic_file_attributes_alloc` hands back a real
/// `sun.nio.fs.UnixFileAttributes`, which implements `PosixFileAttributes`.
/// HotSpot hands back `UnixFileAttributes$UnixAsBasicFileAttributes`, a wrapper
/// that implements ONLY `BasicFileAttributes`. MEASURED,
/// `regression-suite/probes/W4Files.java`, Linux/JDK 25.0.4, both modes:
///
/// ```text
///   Files.readAttributes(f, BasicFileAttributes.class) instanceof PosixFileAttributes
///     HotSpot    false
///     CratonVM   true
///
///   ((PosixFileAttributes) basic).permissions()
///     HotSpot    unreachable — the cast fails
///     CratonVM   reachable:true
/// ```
///
/// So an application could reach POSIX permissions off a view the JDK does not
/// let it reach them from, and — the direction that actually bites — a
/// `basic instanceof PosixFileAttributes` branch guarding a POSIX-only code
/// path silently took the POSIX branch on a request that asked for basic.
/// `WORKER-4-1` N3 recorded the class-name difference and left it as cosmetic;
/// it is not, and `W4Files` is the difference between the two claims.
///
/// # Why the JDK's own factory, and why only here
///
/// `UnixAsBasicFileAttributes.wrap(UnixFileAttributes)` is package-private but
/// it is the JDK's own constructor for exactly this narrowing, and its
/// delegating bodies call `attrs.isDirectory()` and friends — the accessors
/// this bridge already registers on `UnixFileAttributes`. Building the wrapper
/// by hand would be a second implementation of a class that already exists in
/// the image.
///
/// **Only the two Class-taking entry points wrap.** Every INTERNAL consumer —
/// `FileTreeWalker`, the Dos view, `getLastModifiedTime` — reads the carrier
/// through the accessor helpers and wants the full object; wrapping there would
/// hide fields those helpers need for nothing.
///
/// A failure to wrap returns the object unchanged, which is the pre-2026-08-22
/// answer: this narrowing must not become a new way for `readAttributes` to
/// fail.
pub fn narrow_to_basic_view(
    ctx: &mut dyn NativeContext,
    requested: Option<&Value>,
    result: MethodCallResult,
) -> MethodCallResult {
    let Some(Value::Object(Some(class))) = requested else {
        return result;
    };
    let asked = crate::lang_class::mirror_class_name(ctx, *class).unwrap_or_default();
    // Exactly `BasicFileAttributes`. A request for `PosixFileAttributes` or
    // `DosFileAttributes` must keep the full object, and an unnameable Class
    // argument states no request at all.
    if asked != "java/nio/file/attribute/BasicFileAttributes" {
        return result;
    }
    let attrs = match result {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => return other,
    };
    // Only the Unix carrier has this wrapper; the Windows one has no
    // equivalent nested type, and a synthetic-mode carrier is not a
    // `UnixFileAttributes` at all.
    let carrier = ctx
        .class_name_arc_of_id(ctx.class_id_of_object(attrs))
        .unwrap_or_default();
    if &*carrier != "sun/nio/fs/UnixFileAttributes" {
        return Ok(Some(Value::Object(Some(attrs))));
    }
    match ctx.invoke(
        "sun/nio/fs/UnixFileAttributes$UnixAsBasicFileAttributes",
        "wrap",
        "(Lsun/nio/fs/UnixFileAttributes;)Lsun/nio/fs/UnixFileAttributes$UnixAsBasicFileAttributes;",
        &[Value::Object(Some(attrs))],
    ) {
        Ok(Some(v @ Value::Object(Some(_)))) => Ok(Some(v)),
        _ => Ok(Some(Value::Object(Some(attrs)))),
    }
}

pub fn basic_file_attributes_is_windows(ctx: &dyn NativeContext, attrs: ObjectRef) -> bool {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(attrs))
        .as_deref()
        == Some("sun/nio/fs/WindowsFileAttributes")
}

/// True when `attrs` is the synthetic-JDK stub minted by
/// `basic_file_attributes_alloc`'s fallback rather than a real
/// `sun.nio.fs.{Unix,Windows}FileAttributes`.
///
/// `java.nio.file.attribute.BasicFileAttributes` is an INTERFACE in every real
/// JDK, so no genuine instance can ever carry that class id — a hit here is
/// unambiguously our own stub.
///
/// Why this matters: `ensure_synthetic_class` declares a slot COUNT but ZERO
/// named fields, so `set_field_by_name` silently drops the write and
/// `get_field_by_name` answers `Object(None)`. Every attribute
/// `basic_file_attributes_store` wrote by name vanished, and every predicate
/// below read back its default — a plain file reported `isRegularFile() ==
/// false`, `size() == 0` and epoch timestamps. The accessors therefore switch
/// to the fixed slot layout below for this class.
pub fn basic_file_attributes_is_synthetic(
    ctx: &dyn NativeContext,
    attrs: ObjectRef,
) -> bool {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(attrs))
        .as_deref()
        == Some("java/nio/file/attribute/BasicFileAttributes")
}

/// Slot layout of the synthetic `BasicFileAttributes` stub (5 slots — keep in
/// sync with the `try_alloc_concurrent_synthetic(..., 5)?` call in
/// `basic_file_attributes_alloc`). The mode word uses the Unix `st_mode`
/// encoding (type bits | permission bits) on every host so the shared
/// predicates can decode it uniformly.
pub const BFA_SYN_SLOT_MODE: usize = 0;
pub const BFA_SYN_SLOT_SIZE: usize = 1;
pub const BFA_SYN_SLOT_CREATION: usize = 2;
pub const BFA_SYN_SLOT_ACCESS: usize = 3;
pub const BFA_SYN_SLOT_MODIFIED: usize = 4;

/// Read the synthetic stub's `st_mode`-encoded mode word.
pub fn basic_file_attributes_syn_mode(ctx: &dyn NativeContext, attrs: ObjectRef) -> i32 {
    match ctx.get_field(attrs, BFA_SYN_SLOT_MODE) {
        Value::Int(v) => v,
        Value::Long(v) => v as i32,
        _ => 0,
    }
}

/// The real `sun.nio.fs.UnixFileAttributes` stores every timestamp as a
/// SECONDS + NANOS *pair* (`st_mtime_sec`/`st_mtime_nsec`, `st_atime_sec`/
/// `st_atime_nsec`, `st_birthtime_sec`/`st_birthtime_nsec`). It has never had
/// an unsplit `st_mtime`/`st_atime`/`st_birthtime` field on any JDK this VM
/// targets. Returns `(seconds field, nanos field, legacy millis field)`; the
/// third is only used by a synthetic `UnixFileAttributes` shim that declares
/// the unsplit spelling.
///
/// Why this needs a helper rather than a literal: `set_field_by_name` on a
/// name the class does not declare is a SILENT no-op, and `get_field_by_name`
/// answers a non-`Long`. Storing and loading through the same wrong name made
/// this bridge perfectly self-consistent while agreeing with nothing — every
/// `BasicFileAttributeView.readAttributes()` on a Unix host reported
/// 1970-01-01 for all three times regardless of the file's real timestamps,
/// even though `setTimes` had written them to the inode correctly. The
/// Windows carrier's names (`creationTime`/`lastAccessTime`/`lastWriteTime`)
/// happen to be genuine, which is why this only ever showed up on Linux.
/// See jarmode-tools-extract-timestamp-preservation-FIXED.md.
pub fn unix_attr_time_fields(which: &str) -> (&'static str, &'static str, &'static str) {
    match which {
        "creation" => ("st_birthtime_sec", "st_birthtime_nsec", "st_birthtime"),
        "access" => ("st_atime_sec", "st_atime_nsec", "st_atime"),
        "ctime" => ("st_ctime_sec", "st_ctime_nsec", "st_ctime"),
        _ => ("st_mtime_sec", "st_mtime_nsec", "st_mtime"),
    }
}

pub fn attrs_declares_field(ctx: &dyn NativeContext, attrs: ObjectRef, field: &str) -> bool {
    ctx.resolve_field_index_by_class_id(ctx.class_id_of_object(attrs), field)
        .is_some()
}

pub fn attrs_long_field(ctx: &dyn NativeContext, attrs: ObjectRef, field: &str) -> i64 {
    match ctx.get_field_by_name(attrs, field) {
        Value::Long(v) => v,
        Value::Int(v) => v as i64,
        _ => 0,
    }
}

pub fn basic_file_attributes_time_millis(
    ctx: &dyn NativeContext,
    attrs: ObjectRef,
    which: &str,
) -> i64 {
    if basic_file_attributes_is_synthetic(ctx, attrs) {
        let slot = match which {
            "creation" => BFA_SYN_SLOT_CREATION,
            "access" => BFA_SYN_SLOT_ACCESS,
            _ => BFA_SYN_SLOT_MODIFIED,
        };
        return match ctx.get_field(attrs, slot) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
    }
    if basic_file_attributes_is_windows(ctx, attrs) {
        let field = match which {
            "creation" => "creationTime",
            "access" => "lastAccessTime",
            _ => "lastWriteTime",
        };
        // These three fields hold Windows FILETIME, not millis — see
        // `WINDOWS_EPOCH_IN_100NS`. They used to be written AND read as millis,
        // which was self-consistent and agreed with nothing: real
        // `WindowsFileAttributes.lastModifiedTime()` bytecode reading the same
        // field through `toFileTime` reported 1601-01-02T20:42:25.920Z for a
        // file HotSpot dated 2021-01-01 (G90-1 §5, `RFileTimes`).
        return win_millis_from_filetime(attrs_long_field(ctx, attrs, field));
    }
    let (sec_field, nsec_field, legacy_field) = unix_attr_time_fields(which);
    if attrs_declares_field(ctx, attrs, sec_field) {
        let seconds = attrs_long_field(ctx, attrs, sec_field);
        let nanos = attrs_long_field(ctx, attrs, nsec_field);
        return seconds
            .saturating_mul(1_000)
            .saturating_add(nanos.div_euclid(1_000_000));
    }
    attrs_long_field(ctx, attrs, legacy_field)
}

/// Inverse of `basic_file_attributes_time_millis` for the Unix carrier.
pub fn unix_attr_store_time(ctx: &mut dyn NativeContext, attrs: ObjectRef, which: &str, millis: i64) {
    let (sec_field, nsec_field, legacy_field) = unix_attr_time_fields(which);
    if attrs_declares_field(ctx, attrs, sec_field) {
        ctx.set_field_by_name(attrs, sec_field, Value::Long(millis.div_euclid(1_000)));
        ctx.set_field_by_name(
            attrs,
            nsec_field,
            Value::Long(millis.rem_euclid(1_000) * 1_000_000),
        );
        return;
    }
    ctx.set_field_by_name(attrs, legacy_field, Value::Long(millis));
}

pub fn basic_file_attributes_is_dir(ctx: &dyn NativeContext, attrs: ObjectRef) -> bool {
    if basic_file_attributes_is_synthetic(ctx, attrs) {
        return basic_file_attributes_syn_mode(ctx, attrs) & UNIX_S_IFMT == 0o040000;
    }
    if basic_file_attributes_is_windows(ctx, attrs) {
        return matches!(ctx.get_field_by_name(attrs, "fileAttrs"), Value::Int(v) if v & 0x10 != 0);
    }
    matches!(ctx.get_field_by_name(attrs, "st_mode"), Value::Int(v) if v & 0o170000 == 0o040000)
}


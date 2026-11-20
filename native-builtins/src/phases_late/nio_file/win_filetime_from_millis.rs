#[allow(unused_imports)]
use super::*;

/// `st_mode` type bits (`S_IFMT` / `S_IFLNK` / `S_IFREG`) and the Windows
/// `FILE_ATTRIBUTE_REPARSE_POINT` bit, used by the type predicates below.
pub const UNIX_S_IFMT: i32 = 0o170000;
pub const UNIX_S_IFLNK: i32 = 0o120000;
pub const UNIX_S_IFREG: i32 = 0o100000;
pub const WIN_ATTR_DIRECTORY: i32 = 0x10;
pub const WIN_ATTR_REPARSE_POINT: i32 = 0x400;

/// `IO_REPARSE_TAG_SYMLINK`, quoted from JDK 25
/// `java.base/sun/nio/fs/WindowsConstants.java`:
/// `public static final int IO_REPARSE_TAG_SYMLINK = 0xA000000C;`
///
/// Real `WindowsFileAttributes.isSymbolicLink()` is `reparseTag ==
/// IO_REPARSE_TAG_SYMLINK` — it does **not** consult
/// `FILE_ATTRIBUTE_REPARSE_POINT`. A carrier that sets only the attribute bit
/// therefore reads back through real bytecode as `isOther()`, never as a link.
pub const WIN_IO_REPARSE_TAG_SYMLINK: i32 = 0xA000_000Cu32 as i32;

/// `WindowsFileAttributes.WINDOWS_EPOCH_IN_100NS`, quoted from JDK 25
/// `java.base/sun/nio/fs/WindowsFileAttributes.java`:
/// `private static final long WINDOWS_EPOCH_IN_100NS = -116444736000000000L;`
///
/// The real class stores `creationTime` / `lastAccessTime` / `lastWriteTime` as
/// **Windows FILETIME** — 100-nanosecond intervals since 1601-01-01T00:00:00Z —
/// and converts on the way out:
///
/// ```text
///   static FileTime toFileTime(long time) {
///       long adjusted = Math.addExact(time, WINDOWS_EPOCH_IN_100NS);
///       long nanos = Math.multiplyExact(adjusted, 100L);
///       return FileTime.from(nanos, TimeUnit.NANOSECONDS);
///   }
/// ```
pub const WINDOWS_EPOCH_IN_100NS: i64 = -116_444_736_000_000_000;

/// Unix-epoch millis -> Windows FILETIME ticks. The inverse of
/// `WindowsFileAttributes.toFileTime`, and the same arithmetic as its
/// `toWindowsTime` (`adjusted - WINDOWS_EPOCH_IN_100NS`).
pub fn win_filetime_from_millis(millis: i64) -> i64 {
    millis
        .saturating_mul(10_000)
        .saturating_sub(WINDOWS_EPOCH_IN_100NS)
}

/// Windows FILETIME ticks -> Unix-epoch millis. Mirrors
/// `WindowsFileAttributes.toFileTime`'s `time + WINDOWS_EPOCH_IN_100NS`, then
/// scales 100ns to ms.
pub fn win_millis_from_filetime(ticks: i64) -> i64 {
    ticks
        .saturating_add(WINDOWS_EPOCH_IN_100NS)
        .div_euclid(10_000)
}

pub fn basic_file_attributes_is_symlink(ctx: &dyn NativeContext, attrs: ObjectRef) -> bool {
    if basic_file_attributes_is_synthetic(ctx, attrs) {
        return basic_file_attributes_syn_mode(ctx, attrs) & UNIX_S_IFMT == UNIX_S_IFLNK;
    }
    if basic_file_attributes_is_windows(ctx, attrs) {
        return matches!(ctx.get_field_by_name(attrs, "fileAttrs"),
            Value::Int(v) if v & WIN_ATTR_REPARSE_POINT != 0);
    }
    matches!(ctx.get_field_by_name(attrs, "st_mode"),
        Value::Int(v) if v & UNIX_S_IFMT == UNIX_S_IFLNK)
}

pub fn basic_file_attributes_is_regular(ctx: &dyn NativeContext, attrs: ObjectRef) -> bool {
    if basic_file_attributes_is_synthetic(ctx, attrs) {
        return basic_file_attributes_syn_mode(ctx, attrs) & UNIX_S_IFMT == UNIX_S_IFREG;
    }
    if basic_file_attributes_is_windows(ctx, attrs) {
        return matches!(ctx.get_field_by_name(attrs, "fileAttrs"),
            Value::Int(v) if v & (WIN_ATTR_DIRECTORY | WIN_ATTR_REPARSE_POINT) == 0);
    }
    matches!(ctx.get_field_by_name(attrs, "st_mode"),
        Value::Int(v) if v & UNIX_S_IFMT == UNIX_S_IFREG)
}

/// Shared body for `Files.getOwner` / `FileOwnerAttributeView.getOwner`.
///
/// Reads the path's attributes and asks the resulting attributes object for its
/// `owner()`. On a real-JDK Unix build that lands in
/// `UnixFileAttributes.owner()` -> `UnixUserPrincipals.fromUid(st_uid)`, both of
/// which are already wired (`p59_files_read_attributes` fills `st_uid`;
/// `native-io` registers `UnixNativeDispatcher.getpwuid`). Anywhere the owner
/// genuinely cannot be produced we raise the exception the JDK specifies for a
/// provider without a `FileOwnerAttributeView` — returning `null`, which is what
/// this used to do, is not a legal outcome of `getOwner` and only moves the
/// failure to the caller's next dereference.
pub fn nio_owner_principal(
    ctx: &mut dyn NativeContext,
    #[cfg_attr(not(windows), allow(unused_mut))] mut path_value: Value,
) -> MethodCallResult {
    let unsupported = || -> MethodCallFailed {
        RuntimeError::UnsupportedOperationException {
            message: "getOwner: file owner is not available for this path".into(),
        }
        .into()
    };
    // Windows first: `p59_files_read_attributes` produces the DOS/Windows
    // attribute shape there, which has no `owner()` at all, so the Unix
    // delegation below could only ever throw — `Files.getOwner` was
    // unconditionally `UnsupportedOperationException` on Windows while HotSpot
    // answers. `win_file_owner_account` asks the OS for the real owner SID and
    // resolves it to `DOMAIN\account`.
    #[cfg(windows)]
    {
        // The read hands the Path back current: the Unix delegation below
        // still uses it when no text came back (gc-common w21-b).
        let entry_value = path_value;
        let path_text = match entry_value {
            Value::Object(Some(mut p)) => {
                let text = p57_read_path(ctx, &mut p);
                path_value = Value::Object(Some(p));
                text            }
            _ => String::new(),
        };
        if !path_text.is_empty() {
            if std::fs::symlink_metadata(&path_text).is_err() {
                // `p57_no_such_file` allocates the exception object, so building
                // it can itself fail; `?` propagates that, and the success value
                // IS the `MethodCallFailed` this returns.
                return Err(p57_no_such_file(ctx, &path_text)?);
            }
            let Some((account, sid_text, sid_type)) = win_file_owner_account(&path_text) else {
                return Err(RuntimeError::IOException {
                    message: format!("getOwner: cannot read the owner of {path_text}"),
                }
                .into());
            };
            return Ok(Some(Value::Object(Some(alloc_windows_user_principal(
                ctx, &account, &sid_text, sid_type,
            )?))));
        }
    }
    let attrs = match p59_files_read_attributes(ctx, &[path_value])? {
        Some(Value::Object(Some(attrs))) => attrs,
        _ => return Err(unsupported()),
    };
    match ctx.invoke_virtual(
        attrs,
        "owner",
        "()Ljava/nio/file/attribute/UserPrincipal;",
        &[],
    ) {
        Ok(Some(owner @ Value::Object(Some(_)))) => Ok(Some(owner)),
        _ => Err(unsupported()),
    }
}

/// The real-JDK `Files.getOwner` path reaches provider bytecode that is not
/// presently complete in CratonVM.  Reuse the attribute-backed owner bridge,
/// but do not promote the surrounding synthetic Files registrar.
pub fn register_real_jdk_files_owner(r: &mut NativeMethodRegistry) {
    reg_files! { r.register("java/nio/file/Files",
        "getOwner",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Ljava/nio/file/attribute/UserPrincipal;",
        |ctx, args| {
            let path_value = args.first().copied().unwrap_or(Value::Object(None));
            nio_owner_principal(ctx, path_value)
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    ) }}

/// The owner of a file as Windows reports it: `(account, sid, sid_type)`.
///
/// `GetNamedSecurityInfoW(.., OWNER_SECURITY_INFORMATION, ..)` yields the owner
/// SID; `LookupAccountSidW` turns it into `DOMAIN\account` (the rendering
/// `sun.nio.fs.WindowsUserPrincipals` uses), and `ConvertSidToStringSidW` gives
/// the `S-1-5-…` form the JDK keeps for `equals`/`hashCode`. A SID with no
/// resolvable account (an orphaned ACL entry) still gets a principal, named by
/// its SID string — which is exactly what the JDK does in that case.
///
/// `None` means the OS refused the query; the caller raises `IOException`, the
/// outcome `Files.getOwner` specifies for a failed lookup.
#[cfg(windows)]
pub fn win_file_owner_account(path: &str) -> Option<(String, String, i32)> {
    const SE_FILE_OBJECT: i32 = 1;
    const OWNER_SECURITY_INFORMATION: u32 = 0x0000_0001;
    const ERROR_SUCCESS: u32 = 0;
    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
    const ERROR_NONE_MAPPED: u32 = 1332;

    #[link(name = "Advapi32")]
    extern "system" {
        fn GetNamedSecurityInfoW(
            object_name: *const u16,
            object_type: i32,
            security_info: u32,
            owner: *mut *mut std::ffi::c_void,
            group: *mut *mut std::ffi::c_void,
            dacl: *mut *mut std::ffi::c_void,
            sacl: *mut *mut std::ffi::c_void,
            security_descriptor: *mut *mut std::ffi::c_void,
        ) -> u32;
        fn LookupAccountSidW(
            system_name: *const u16,
            sid: *mut std::ffi::c_void,
            name: *mut u16,
            name_len: *mut u32,
            domain: *mut u16,
            domain_len: *mut u32,
            use_: *mut i32,
        ) -> i32;
        fn ConvertSidToStringSidW(sid: *mut std::ffi::c_void, out: *mut *mut u16) -> i32;
    }
    #[link(name = "Kernel32")]
    extern "system" {
        fn LocalFree(mem: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
        fn GetLastError() -> u32;
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }
    fn from_wide(buf: &[u16]) -> String {
        let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    let wpath = wide(path);
    let mut owner: *mut std::ffi::c_void = std::ptr::null_mut();
    let mut descriptor: *mut std::ffi::c_void = std::ptr::null_mut();
    // SAFETY: `wpath` is NUL-terminated and outlives the call; every out-pointer
    // is a live local. On success the descriptor is a single LocalAlloc block
    // that owns `owner`, freed exactly once below and never escaping.
    let rc = unsafe {
        GetNamedSecurityInfoW(
            wpath.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if rc != ERROR_SUCCESS || owner.is_null() {
        if !descriptor.is_null() {
            // SAFETY: allocated by the call above, freed once.
            unsafe { LocalFree(descriptor) };
        }
        return None;
    }

    // SID string form (`S-1-5-21-…`), used as the principal's identity.
    let mut sid_text = String::new();
    let mut sid_wide: *mut u16 = std::ptr::null_mut();
    // SAFETY: `owner` points into the live descriptor; the returned buffer is a
    // LocalAlloc block freed immediately after it is copied out.
    if unsafe { ConvertSidToStringSidW(owner, &mut sid_wide) } != 0 && !sid_wide.is_null() {
        let mut len = 0usize;
        // SAFETY: the OS returns a NUL-terminated UTF-16 string.
        while unsafe { *sid_wide.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `len` stops at the NUL, so the slice is in bounds.
        sid_text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(sid_wide, len) });
        // SAFETY: allocated by ConvertSidToStringSidW, freed once.
        unsafe { LocalFree(sid_wide.cast()) };
    }

    // Account name. Two-call idiom: ask for the required sizes, then fill.
    let mut name_len: u32 = 0;
    let mut domain_len: u32 = 0;
    let mut sid_type: i32 = 0;
    // SAFETY: zero lengths with null buffers is the documented sizing call; it
    // fails with ERROR_INSUFFICIENT_BUFFER and writes the two lengths.
    unsafe {
        LookupAccountSidW(
            std::ptr::null(),
            owner,
            std::ptr::null_mut(),
            &mut name_len,
            std::ptr::null_mut(),
            &mut domain_len,
            &mut sid_type,
        )
    };
    let sizing_error = unsafe { GetLastError() };
    let account = if sizing_error == ERROR_INSUFFICIENT_BUFFER && name_len > 0 {
        let mut name = vec![0u16; name_len as usize];
        let mut domain = vec![0u16; domain_len.max(1) as usize];
        // SAFETY: both buffers are sized by the call above and outlive this one.
        let ok = unsafe {
            LookupAccountSidW(
                std::ptr::null(),
                owner,
                name.as_mut_ptr(),
                &mut name_len,
                domain.as_mut_ptr(),
                &mut domain_len,
                &mut sid_type,
            )
        };
        if ok != 0 {
            let account = from_wide(&name);
            let domain = from_wide(&domain);
            if domain.is_empty() {
                account
            } else {
                format!("{domain}\\{account}")
            }
        } else {
            String::new()
        }
    } else if sizing_error == ERROR_NONE_MAPPED {
        // A SID with no account behind it. The JDK names the principal by its
        // SID string rather than failing, so do the same.
        String::new()
    } else {
        String::new()
    };

    // SAFETY: `descriptor` came from the successful call above and is freed once;
    // `owner` points inside it and is not used after this.
    unsafe { LocalFree(descriptor) };

    if account.is_empty() && sid_text.is_empty() {
        return None;
    }
    let account = if account.is_empty() {
        sid_text.clone()
    } else {
        account
    };
    Some((account, sid_text, sid_type))
}

/// Materialise the `UserPrincipal` `Files.getOwner` hands back on Windows.
///
/// Uses the real JDK carrier `sun.nio.fs.WindowsUserPrincipals$User` and writes
/// its three fields BY NAME, so the object satisfies `instanceof UserPrincipal`
/// and any JDK code that reads those fields directly sees the right values. The
/// accessors below are registered on the same class, so a build where that class
/// is synthesized rather than loaded still answers.
#[cfg(windows)]
pub fn alloc_windows_user_principal(
    ctx: &mut dyn NativeContext,
    account: &str,
    sid_text: &str,
    sid_type: i32,
) -> Result<cratonvm_types::ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "sun/nio/fs/WindowsUserPrincipals$User", 3)?;
    // GC-SAFETY: `create_string` allocates, so pin the carrier and re-read it
    // through the pin after each allocation before writing into it.
    let pin = ctx.pin_native_root(obj);
    let sid_str = ctx.create_string(sid_text);
    let obj = ctx.read_native_pin(pin, obj);
    ctx.set_field_by_name(obj, "sidString", Value::Object(Some(sid_str)));
    let account_str = ctx.create_string(account);
    let obj = ctx.read_native_pin(pin, obj);
    ctx.set_field_by_name(obj, "accountName", Value::Object(Some(account_str)));
    ctx.set_field_by_name(obj, "sidType", Value::Int(sid_type));
    ctx.unpin_native_roots(pin);
    Ok(obj)
}

/// Same identity key as `basic_file_attributes_file_key`, but computed straight
/// from a host path — for attribute objects that record their backing path
/// instead of the raw `stat` fields (`DosFileAttributes`, slot 5).
pub fn file_identity_key_for_path(path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path).ok()?;
        Some(format!(
            // Hex, for the reason given on `basic_file_attributes_file_key`.
            "(dev={:x},ino={})",
            meta.dev() as i64,
            meta.ino() as i64
        ))
    }
    #[cfg(windows)]
    {
        let (volume, high, low) = win_file_identity(path)?;
        let index = (u64::from(high) << 32) | u64::from(low);
        Some(format!("(volume={volume},file={index})"))
    }
}

/// The file-identity string behind `BasicFileAttributes.fileKey()`.
///
/// `fileKey()` used to be a hardcoded `null`, which silently disabled every
/// identity check built on it — most importantly `FileTreeWalker.wouldLoop`,
/// the JDK walker's ONLY symlink-loop guard (`Files.walk(.., FOLLOW_LINKS)`
/// over a directory that links back to an ancestor recursed until it ran out
/// of depth), plus hardlink identity.
///
/// `p59_files_read_attributes` now records the OS identity on the attributes
/// object (`st_dev`/`st_ino` on Unix, the `GetFileInformationByHandle` triple
/// on Windows), so the key is a real answer rather than a fabricated one. We
/// hand back a `String` in the JDK's own `UnixFileKey.toString()` shape: the
/// declared return type is `Object`, and every consumer only ever
/// `equals`/`hashCode`-compares the value, both of which `String` gives us for
/// free (the JDK's own key classes are package-private in `sun.nio.fs`, so no
/// caller can name the concrete type). `None` -> `null`, which stays the
/// documented answer when the identity is unavailable.
/// The `fileKey()` VALUE: a real `sun.nio.fs.UnixFileKey` (or
/// `WindowsFileKey`), not a string that prints like one.
///
/// The string producer below was right about the CONTENT and wrong about the
/// TYPE, and the two doors into this attribute disagreed because of it:
///
/// ```text
///   readAttributes(f, BasicFileAttributes.class).fileKey()
///       HotSpot  sun.nio.fs.UnixFileKey      this VM  sun.nio.fs.UnixFileKey
///   readAttributes(f, PosixFileAttributes.class).fileKey()
///       HotSpot  sun.nio.fs.UnixFileKey      this VM  java.lang.String
/// ```
///
/// One door reaches the real JDK bytecode and mints a real key; the other
/// reaches this native. `Objects.equals` between them is false whatever the
/// two print, so a program that uses `fileKey` to decide whether two paths
/// name the same file -- which is the method's only purpose, and what
/// `FileTreeWalker.wouldLoop` uses to break symlink cycles -- got `false` for
/// one file. MEASURED with `L4BridgeSweep.java`.
///
/// The key classes are package-private with package-private constructors, so
/// this mints them the same way [`p57_closed_channel`] mints its exception:
/// `new_object` plus the real `<init>`. Falling back to the string keeps the
/// previous behaviour when the class is absent (a synthetic image), which is
/// no worse than before and never null.
pub fn basic_file_attributes_file_key_object(
    ctx: &mut dyn NativeContext,
    attrs: ObjectRef,
) -> Option<Value> {
    let field = |ctx: &dyn NativeContext, name: &str| match ctx.get_field_by_name(attrs, name) {
        Value::Long(v) => Some(v),
        Value::Int(v) => Some(i64::from(v)),
        _ => None,
    };
    let (cls, desc, ctor_args) = if basic_file_attributes_is_windows(ctx, attrs) {
        let volume = field(ctx, "volSerialNumber")?;
        let high = field(ctx, "fileIndexHigh")?;
        let low = field(ctx, "fileIndexLow")?;
        if volume == 0 && high == 0 && low == 0 {
            return None;
        }
        let index = ((high as u64) << 32) | (low as u64 & 0xffff_ffff);
        (
            "sun/nio/fs/WindowsFileKey",
            "(IJ)V",
            vec![Value::Int(volume as i32), Value::Long(index as i64)],
        )
    } else {
        let dev = field(ctx, "st_dev")?;
        let ino = field(ctx, "st_ino")?;
        if dev == 0 && ino == 0 {
            return None;
        }
        (
            "sun/nio/fs/UnixFileKey",
            "(JJ)V",
            vec![Value::Long(dev), Value::Long(ino)],
        )
    };
    // gc-common w18-f: `attrs` and the fresh key are rooted across
    // `new_object` (class initialisation) and the real `<init>` (Java), and
    // re-read after them. The key used to be returned, and `attrs` read by the
    // fallback, at their pre-call addresses.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let attrs_h = scope.root(attrs);
    if let Ok(Some(Value::Object(Some(key)))) = scope.new_object(cls) {
        let key_h = scope.root(key);
        let mut call = Vec::with_capacity(ctor_args.len() + 1);
        call.push(Value::Object(Some(key)));
        call.extend(ctor_args);
        if scope.invoke(cls, "<init>", desc, &call).is_ok() {
            return Some(Value::Object(Some(scope.get(&key_h))));
        }
    }
    // The class is not there. Keep the string: it still compares equal to
    // itself across two reads of one file, which is most of the contract.
    let attrs = scope.get(&attrs_h);
    let s = basic_file_attributes_file_key(&*scope, attrs)?;
    let s = scope.create_string(&s);
    Some(Value::Object(Some(s)))
}

pub fn basic_file_attributes_file_key(
    ctx: &dyn NativeContext,
    attrs: ObjectRef,
) -> Option<String> {
    if basic_file_attributes_is_windows(ctx, attrs) {
        let field = |name: &str| match ctx.get_field_by_name(attrs, name) {
            Value::Int(v) => Some(v as u32),
            Value::Long(v) => Some(v as u32),
            _ => None,
        };
        let volume = field("volSerialNumber")?;
        let high = field("fileIndexHigh")?;
        let low = field("fileIndexLow")?;
        if volume == 0 && high == 0 && low == 0 {
            return None;
        }
        let index = (u64::from(high) << 32) | u64::from(low);
        return Some(format!("(volume={volume},file={index})"));
    }
    let field = |name: &str| match ctx.get_field_by_name(attrs, name) {
        Value::Long(v) => Some(v),
        Value::Int(v) => Some(i64::from(v)),
        _ => None,
    };
    let dev = field("st_dev")?;
    let ino = field("st_ino")?;
    // HEX, because `sun.nio.fs.UnixFileKey.toString` is
    // `"(dev=" + Long.toHexString(st_dev) + ",ino=" + st_ino + ")"`. This
    // producer used decimal while the attribute-map producer 900 lines down
    // used `{:x}`, so ONE file had two different identities depending on which
    // door was asked:
    //
    //   readAttributes(f, BasicFileAttributes.class).fileKey()  (dev=10301,...)
    //   readAttributes(f, PosixFileAttributes.class).fileKey()  (dev=66305,...)
    //
    // 0x10301 == 66305. Same device, two spellings, and `equals` between them
    // is false -- which is the whole purpose of a file key. MEASURED with
    // `L4BridgeSweep.java`.
    if dev == 0 && ino == 0 {
        return None;
    }
    Some(format!("(dev={:x},ino={ino})", dev as u64))
}

pub fn basic_file_attributes_size(ctx: &dyn NativeContext, attrs: ObjectRef) -> i64 {
    if basic_file_attributes_is_synthetic(ctx, attrs) {
        return match ctx.get_field(attrs, BFA_SYN_SLOT_SIZE) {
            Value::Long(v) => v,
            Value::Int(v) => v as i64,
            _ => 0,
        };
    }
    let field = if basic_file_attributes_is_windows(ctx, attrs) {
        "size"
    } else {
        "st_size"
    };
    match ctx.get_field_by_name(attrs, field) {
        Value::Long(v) => v,
        _ => 0,
    }
}

pub fn basic_file_attributes_store(
    ctx: &mut dyn NativeContext,
    attrs: ObjectRef,
    is_dir: bool,
    size: i64,
    creation_millis: i64,
    access_millis: i64,
    modified_millis: i64,
    unix_perm_bits: i32,
) {
    if basic_file_attributes_is_synthetic(ctx, attrs) {
        // Synthetic stub: NO named fields exist, so every `set_field_by_name`
        // below would be a silent no-op. Write the fixed slot layout the
        // accessors read (`BFA_SYN_SLOT_*`).
        let type_bits = if is_dir { 0o040000 } else { 0o100000 };
        ctx.set_field(
            attrs,
            BFA_SYN_SLOT_MODE,
            Value::Int(type_bits | (unix_perm_bits & 0o7777)),
        );
        ctx.set_field(attrs, BFA_SYN_SLOT_SIZE, Value::Long(size));
        ctx.set_field(attrs, BFA_SYN_SLOT_CREATION, Value::Long(creation_millis));
        ctx.set_field(attrs, BFA_SYN_SLOT_ACCESS, Value::Long(access_millis));
        ctx.set_field(attrs, BFA_SYN_SLOT_MODIFIED, Value::Long(modified_millis));
        return;
    }
    if basic_file_attributes_is_windows(ctx, attrs) {
        ctx.set_field_by_name(
            attrs,
            "fileAttrs",
            Value::Int(if is_dir { WIN_ATTR_DIRECTORY } else { 0 }),
        );
        // FILETIME, not millis. The real class reads these three through
        // `toFileTime`, which adds `WINDOWS_EPOCH_IN_100NS` and scales by 100ns
        // — a millis value put here is read back as a date two days after
        // 1601-01-01. Every read path in this file goes through
        // `basic_file_attributes_time_millis`, which converts back, so the
        // encoding change is invisible to our own accessors and visible to the
        // real bytecode, which is the entire point.
        ctx.set_field_by_name(
            attrs,
            "creationTime",
            Value::Long(win_filetime_from_millis(creation_millis)),
        );
        ctx.set_field_by_name(
            attrs,
            "lastAccessTime",
            Value::Long(win_filetime_from_millis(access_millis)),
        );
        ctx.set_field_by_name(
            attrs,
            "lastWriteTime",
            Value::Long(win_filetime_from_millis(modified_millis)),
        );
        // Explicit, not inherited from a zeroed allocation: real
        // `isSymbolicLink()` / `isUnixDomainSocket()` compare this word against
        // a tag, so it must be a definite 0 for everything that is not a link.
        // `p59_files_read_attributes` raises it to `IO_REPARSE_TAG_SYMLINK`
        // when the OS says the path is one.
        ctx.set_field_by_name(attrs, "reparseTag", Value::Int(0));
        ctx.set_field_by_name(attrs, "size", Value::Long(size));
    } else {
        // Was always the bare file-type bits with zero permission bits, so
        // `sun/nio/fs/UnixFileAttributes.permissions()` (real JDK bytecode,
        // reads this same `st_mode` field) always answered an empty
        // `Set<PosixFilePermission>` regardless of the file's real mode —
        // harmless while nothing read `permissions()`, but once
        // `PosixFileAttributeView` became reachable (see
        // `getFileAttributeView` above) this fed `FilePathDisk.setReadOnly`'s
        // "keep everything except *_WRITE" recomputation from a permission
        // set that never had bits to keep. OR in the real mode bits when the
        // caller has them (0 for the jar/jrt-FS and directory-walk callers
        // below, which have no real backing inode to query).
        let type_bits = if is_dir { 0o040000 } else { 0o100000 };
        ctx.set_field_by_name(
            attrs,
            "st_mode",
            Value::Int(type_bits | (unix_perm_bits & 0o7777)),
        );
        unix_attr_store_time(ctx, attrs, "creation", creation_millis);
        unix_attr_store_time(ctx, attrs, "access", access_millis);
        unix_attr_store_time(ctx, attrs, "modified", modified_millis);
        // `UnixFileAttributes.creationTime()` (real JDK bytecode) only trusts
        // `st_birthtime_*` when this flag is set, and falls back to `ctime()`
        // otherwise; `ctime()` reads `st_ctime_*`, which `std::fs::Metadata`
        // cannot supply portably. Mirror the modified time there so a caller
        // that reaches the real accessor instead of our override sees a
        // plausible value rather than 1970. Callers with no backing inode
        // (jar-FS / jrt-FS / directory walks) pass 0 and correctly report the
        // birth time as unavailable.
        ctx.set_field_by_name(
            attrs,
            "birthtime_available",
            Value::Int(i32::from(creation_millis != 0)),
        );
        unix_attr_store_time(ctx, attrs, "ctime", modified_millis);
        ctx.set_field_by_name(attrs, "st_size", Value::Long(size));
    }
}

/// Extract path string from a Path argument (field 0 = String)
pub fn extract_path_string(ctx: &mut dyn NativeContext, arg: Option<&Value>) -> String {
    // Resolve the Path slot map BEFORE reading one. `slots()` hands readers the
    // pre-2026-09-10 FALLBACK layout (string in slot 0) until some allocator has
    // initialised it, and a Path minted by REAL JDK bytecode -- exactly what
    // `ZipFile$Source.get` builds with `DefaultFileSystemProvider.theFileSystem()
    // .getPath(..)` -- initialises nothing. As the first file operation of a
    // process, `new ZipFile(file)` / `new JarFile(file)` therefore read that
    // Path's text from the wrong slot, got an empty string, and failed with
    // `NoSuchFileException: ` (empty message) for a file that exists; one
    // earlier `Files.*` call or `File.createTempFile` hid it
    // (`TestServerInfo.testGetJarVersionWithSpecificationVersion`, whose
    // `getJarVersion` swallows the exception and falls back to the file name).
    // Cheap once resolved: an `OnceLock::get`.
    let _ = p57_path_slots_init(ctx);
    // SLOT 0 UNTIL 2026-09-10, and that is what broke first when the Path slot
    // map moved: this helper serves `Files.size`, `readAttributes` and the
    // attribute-view family, so a stale index here empties the path for a whole
    // registrar while `p57_read_path`'s callers stay green. One reader, so the
    // next move cannot split them again.
    let raw = match arg {
        Some(Value::Object(Some(path_obj))) => match p57_path_slot_string(ctx, *path_obj) {
            Some(t) => t,
            None => return String::new(),
        },
        _ => return String::new(),
    };
    // Match `p57_read_path`'s OS-path conversion (strips the leading `/`
    // from a `/C:/...` Windows drive path). Without it, callers of this
    // helper (e.g. `p59_files_read_attributes`) did a filesystem lookup
    // against the raw `/C:/...` string, which Windows does not resolve the
    // same way as `C:/...` — so a `Files.readAttributes`/
    // `getLastModifiedTime` NotFound check could disagree with
    // `Files.exists()` (which already goes through `p57_read_path`) for the
    // exact same logical path.
    p57_to_os_path(&raw)
}

pub fn p59_files_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // A null path is an NPE, not a `NoSuchFileException` for the empty path —
    // the two mean different things to a caller that catches the latter to
    // mean "the file is not there yet".
    obj_arg(args, 0)?;
    let path_str = extract_path_string(ctx, args.first());
    if let Some(bytes) = vfs_read(&path_str) {
        return match bytes {
            Ok(b) => Ok(Some(Value::Long(b.len() as i64))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(p57_no_such_file(ctx, &path_str)?)
            }
            Err(e) => Err(p57_io_error(&e)),
        };
    }

    match std::fs::metadata(&path_str) {
        Ok(meta) => Ok(Some(Value::Long(meta.len() as i64))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(p57_no_such_file(ctx, &path_str)?)
        }
        Err(e) => Err(p57_io_error(&e)),
    }
}

/// Convert a SystemTime to epoch millis
pub fn system_time_to_millis(t: std::time::SystemTime) -> i64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Attribute interfaces backed by the Windows attributes object that this VM
/// can actually construct. Other requested interfaces must fail at the
/// provider boundary, matching the JDK instead of relying on a caller cast.
#[cfg(windows)]
pub fn windows_supports_file_attributes_type(class_name: &str) -> bool {
    matches!(
        class_name,
        "java/nio/file/attribute/BasicFileAttributes" | "java/nio/file/attribute/DosFileAttributes"
    )
}

// ---------------------------------------------------------------------------
// Name-keyed attribute reads — `readAttributes(path, "view:attrs", options)`
// ---------------------------------------------------------------------------

/// Split a `"view:attrs"` attribute spec. No colon means the `basic` view,
/// exactly as `java.nio.file.Files` does it.
pub fn split_attribute_spec(spec: &str) -> (&str, &str) {
    match spec.find(':') {
        Some(i) => (&spec[..i], &spec[i + 1..]),
        None => ("basic", spec),
    }
}

/// The attribute names a given view answers, in the JDK's own vocabulary.
///
/// Kept as one table rather than spread across the reader below so the `*`
/// expansion and the per-name lookup can never disagree about what a view
/// contains — a mismatch there is exactly how "`*` returned it but asking for
/// it by name threw" bugs happen.
pub fn attribute_names_for_view(view: &str) -> Option<&'static [&'static str]> {
    const BASIC: &[&str] = &[
        "lastModifiedTime",
        "lastAccessTime",
        "creationTime",
        "size",
        "isRegularFile",
        "isDirectory",
        "isSymbolicLink",
        "isOther",
        "fileKey",
    ];
    const POSIX: &[&str] = &[
        "lastModifiedTime",
        "lastAccessTime",
        "creationTime",
        "size",
        "isRegularFile",
        "isDirectory",
        "isSymbolicLink",
        "isOther",
        "fileKey",
        "permissions",
        "owner",
        "group",
    ];
    const UNIX: &[&str] = &[
        "lastModifiedTime",
        "lastAccessTime",
        "creationTime",
        "size",
        "isRegularFile",
        "isDirectory",
        "isSymbolicLink",
        "isOther",
        "fileKey",
        "permissions",
        "owner",
        "group",
        "mode",
        "ino",
        "dev",
        "rdev",
        "nlink",
        "uid",
        "gid",
        "ctime",
    ];
    const DOS: &[&str] = &[
        "lastModifiedTime",
        "lastAccessTime",
        "creationTime",
        "size",
        "isRegularFile",
        "isDirectory",
        "isSymbolicLink",
        "isOther",
        "fileKey",
        "readonly",
        "hidden",
        "archive",
        "system",
        // `WindowsFileAttributeViews$Dos` also answers the raw Win32 attribute word.
        "attributes",
    ];
    const OWNER: &[&str] = &["owner"];
    match view {
        "basic" => Some(BASIC),
        "posix" => Some(POSIX),
        "unix" => Some(UNIX),
        "dos" => Some(DOS),
        "owner" => Some(OWNER),
        _ => None,
    }
}

/// Everything one `stat` tells us, in the shapes the attribute names want.
pub struct StatFacts {
    pub is_dir: bool,
    pub is_regular: bool,
    pub is_symlink: bool,
    pub size: i64,
    pub modified_millis: i64,
    pub access_millis: i64,
    pub creation_millis: i64,
    pub mode: i32,
    pub nlink: i32,
    pub uid: i32,
    pub gid: i32,
    pub dev: i64,
    pub ino: i64,
    pub rdev: i64,
    pub ctime_millis: i64,
    pub readonly: bool,
    pub hidden: bool,
    pub system: bool,
    pub archive: bool,
    /// The raw `FILE_ATTRIBUTE_*` word (`dos:attributes`); 0 off Windows.
    pub dos_attributes: i32,
}

/// The attribute views a path's filesystem actually has.
///
/// A mounted archive is not the host filesystem: its entries have a size and a
/// timestamp and nothing else, so `basic` is the only view HotSpot's zipfs
/// offers for them either. Screening the two apart here is what stops
/// `readAttributes(zipEntry, "unix:mode")` being answered out of the host's
/// `stat` of a path that does not exist.
pub fn attribute_view_names_for_path(path: &str) -> &'static [&'static str] {
    if vfs_decode(path).is_some() {
        &["basic"]
    } else {
        supported_attribute_view_names()
    }
}

/// [`StatFacts`] for an entry inside a mounted archive (jar / jrt), or `None`
/// when `p` is an ordinary host path and the caller should `stat` it.
///
/// TIMESTAMPS ARE THE ARCHIVE'S, NOT THE ENTRY'S. The zip central directory
/// does carry a per-entry MS-DOS timestamp, but reading it needs the `zip`
/// crate's `time` feature, which the workspace deliberately does not enable
/// (see the `zip` pin in the root `Cargo.toml`). The container's own mtime is a
/// real timestamp rather than a fabricated one, it is already computed for the
/// index cache key, and it is what every entry of a rebuilt archive would get
/// anyway. Callers that need per-entry precision should say so and the pin can
/// be revisited.
pub fn vfs_stat_facts(p: &str) -> Option<std::io::Result<StatFacts>> {
    let (tag, container, entry) = vfs_decode(p)?;
    let kind = match tag {
        JARFS_TAG => jarfs_classify(&container, &entry),
        _ => jrtfs_classify(&container, &entry),
    };
    if matches!(kind, JarFsKind::Absent) {
        return Some(Err(std::io::Error::from(std::io::ErrorKind::NotFound)));
    }
    let is_dir = matches!(kind, JarFsKind::Dir);
    let size = if is_dir {
        0
    } else {
        let sized = match tag {
            JARFS_TAG => jarfs_entry_size(&container, &entry),
            _ => jrtfs_entry_size(&container, &entry),
        };
        match sized {
            Ok(n) => n,
            Err(e) => return Some(Err(e)),
        }
    };
    // The jrt filesystem's container is `java.home`; the archive it actually
    // reads from is the jimage under it.
    let stamped = match tag {
        JARFS_TAG => container.clone(),
        _ => format!("{container}/lib/modules"),
    };
    let millis = (crate::net_phase_e::archive_stamp(&stamped).0 / 1_000_000) as i64;
    Some(Ok(StatFacts {
        is_dir,
        is_regular: !is_dir,
        is_symlink: false,
        size,
        modified_millis: millis,
        access_millis: millis,
        creation_millis: millis,
        // r-xr-xr-x for a directory, r--r--r-- for an entry: an archive this VM
        // has mounted is readable and never writable through these views.
        mode: if is_dir { 0o040555 } else { 0o100444 },
        nlink: 1,
        uid: 0,
        gid: 0,
        dev: 0,
        ino: 0,
        rdev: 0,
        ctime_millis: millis,
        readonly: true,
        // An archive entry has no DOS attribute word of its own; `false` is
        // what the JDK reports for these on a non-DOS filesystem.
        hidden: false,
        system: false,
        archive: false,
        dos_attributes: 0,
    }))
}

pub fn stat_facts(path: &str, nofollow: bool) -> std::io::Result<StatFacts> {
    let meta = if nofollow {
        std::fs::symlink_metadata(path)?
    } else {
        std::fs::metadata(path)?
    };
    let ft = meta.file_type();
    #[allow(unused_mut)]
    let mut facts = StatFacts {
        is_dir: meta.is_dir(),
        is_regular: meta.is_file(),
        is_symlink: ft.is_symlink(),
        size: meta.len() as i64,
        modified_millis: meta.modified().ok().map(system_time_to_millis).unwrap_or(0),
        access_millis: meta.accessed().ok().map(system_time_to_millis).unwrap_or(0),
        creation_millis: meta.created().ok().map(system_time_to_millis).unwrap_or(0),
        mode: 0,
        nlink: 1,
        uid: 0,
        gid: 0,
        dev: 0,
        ino: 0,
        rdev: 0,
        ctime_millis: 0,
        readonly: meta.permissions().readonly(),
        // The three DOS flags read through the SAME `dos_attr_flag` the
        // `DosFileAttributeView` setters and `setAttribute` write through: real
        // `FILE_ATTRIBUTE_*` bits on Windows, the dot-file convention for
        // `hidden` elsewhere. They used to be derived here from the filename
        // alone (and `system`/`archive` were hardcoded `false` at the read
        // site), so on Windows `setAttribute(p, "dos:hidden", true)` followed by
        // `getAttribute(p, "dos:hidden")` answered `false` — a write that
        // really happened, reported as if it had not.
        hidden: dos_attr_flag(path, DOS_ATTR_HIDDEN),
        system: dos_attr_flag(path, DOS_ATTR_SYSTEM),
        archive: dos_attr_flag(path, DOS_ATTR_ARCHIVE),
        dos_attributes: dos_attributes_word(path),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        facts.mode = meta.mode() as i32;
        facts.nlink = meta.nlink() as i32;
        facts.uid = meta.uid() as i32;
        facts.gid = meta.gid() as i32;
        facts.dev = meta.dev() as i64;
        facts.ino = meta.ino() as i64;
        facts.rdev = meta.rdev() as i64;
        facts.ctime_millis = meta
            .ctime()
            .saturating_mul(1000)
            .saturating_add(meta.ctime_nsec() / 1_000_000);
    }
    Ok(facts)
}

/// `Set<PosixFilePermission>` for a Unix mode word, built from the enum's own
/// nine singleton constants so it compares equal to anything the JDK produced.
pub fn posix_permission_set(ctx: &mut dyn NativeContext, mode: i32) -> Option<ObjectRef> {
    const NAMES: [&str; 9] = [
        "OWNER_READ",
        "OWNER_WRITE",
        "OWNER_EXECUTE",
        "GROUP_READ",
        "GROUP_WRITE",
        "GROUP_EXECUTE",
        "OTHERS_READ",
        "OTHERS_WRITE",
        "OTHERS_EXECUTE",
    ];
    const BITS: [i32; 9] = [
        0o400, 0o200, 0o100, 0o040, 0o020, 0o010, 0o004, 0o002, 0o001,
    ];
    let set = match ctx.new_object_initialized("java/util/HashSet", "()V", &[]) {
        Ok(Some(Value::Object(Some(s)))) => s,
        _ => return None,
    };
    let set_pin = ctx.pin_native_root(set);
    let pfp = POSIX_FILE_PERMISSION;
    let _ = ctx.ensure_class_initialized(pfp);
    let cid = ctx.class_id_by_name(pfp);
    for i in 0..9 {
        if mode & BITS[i] == 0 {
            continue;
        }
        let Some(c) = cid else { break };
        let Some(slot) = ctx.static_field_index_by_name(c, NAMES[i]) else {
            continue;
        };
        let constant = ctx.get_static_field(c, slot);
        if !matches!(constant, Value::Object(Some(_))) {
            continue;
        }
        let set = ctx.read_native_pin(set_pin, set);
        let _ = ctx.invoke_virtual(set, "add", "(Ljava/lang/Object;)Z", &[constant]);
    }
    let set = ctx.read_native_pin(set_pin, set);
    ctx.unpin_native_roots(set_pin);
    Some(set)
}

/// What `setAttribute` does with one attribute name.
///
/// Split out of [`write_named_attribute`] so the classification can be asserted
/// against [`attribute_names_for_view`] in a unit test: that table is what the
/// reader answers, and a name added there without a verdict here is exactly how
/// "`readAttributes` returned it but writing it says it does not exist" appears.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AttributeWrite {
    /// One of the three `FileTime` fields.
    Time,
    /// A DOS boolean flag.
    DosFlag,
    /// A POSIX permission set (`posix:permissions`).
    Permissions,
    /// A raw `unix:mode` int.
    Mode,
    /// Writable in the real JDK; not implemented here, and refused loudly.
    Unimplemented,
    /// The reader answers it, but the JDK lets nobody write it.
    NotWritable,
}

pub fn attribute_write_kind(name: &str) -> AttributeWrite {
    match name {
        "lastModifiedTime" | "lastAccessTime" | "creationTime" => AttributeWrite::Time,
        "readonly" | "hidden" | "archive" | "system" => AttributeWrite::DosFlag,
        "permissions" => AttributeWrite::Permissions,
        "mode" => AttributeWrite::Mode,
        "owner" | "group" | "uid" | "gid" => AttributeWrite::Unimplemented,
        _ => AttributeWrite::NotWritable,
    }
}

/// The write side of `Files.setAttribute(path, "view:name", value)`.
///
/// Screened against exactly the same view/name tables as
/// [`read_named_attributes`], so the two can never disagree about what a view
/// contains — asking for a name the reader answers and being told it does not
/// exist is the confusing half of that class of bug.
///
/// The writes themselves delegate: the DOS flags go through the very
/// `dos_view_set_*` natives `getFileAttributeView(path, DosFileAttributeView)`
/// hands out, and the times/permissions go through the same path-level helpers
/// those views use. Nothing about the filesystem is reimplemented here, so a
/// later fix to any of them reaches this entry point too.
///
/// Names that exist but are read-only (`size`, `isDirectory`, `fileKey`, …)
/// raise `IllegalArgumentException` with the JDK's own wording. Names that are
/// writable in the real JDK but not implemented here (`owner`, `group`, `uid`,
/// `gid`) raise `UnsupportedOperationException` rather than silently doing
/// nothing — a `setAttribute` that returns normally and changes nothing is the
/// failure mode that made Gradle believe it had marked a cache directory
/// read-only.
pub fn write_named_attribute(
    ctx: &mut dyn NativeContext,
    mut path_obj: ObjectRef,
    spec: &str,
    mut value: Option<ObjectRef>,
    nofollow: bool,
) -> MethodCallResult {
    let (view, name) = split_attribute_spec(spec);
    let Some(known) = attribute_names_for_view(view) else {
        return Err(RuntimeError::UnsupportedOperationException {
            message: format!("View '{view}' not available"),
        }
        .into());
    };
    // gc-common w21-b: the Path read can run Java (a foreign Path's
    // `toString()`). The Path is handed back current by the read; the value
    // -- used below as a FileTime, a Boolean, a permission Set -- is pinned
    // across it. Both were used through their entry addresses.
    let value_pin = value.map(|v| ctx.pin_native_root(v));
    let path = p57_read_path(ctx, &mut path_obj);
    if let (Some(h), Some(v)) = (value_pin, value) {
        value = Some(ctx.read_native_pin(h, v));
        ctx.unpin_native_roots(h);
    }
    if !attribute_view_names_for_path(&path).contains(&view) {
        return Err(RuntimeError::UnsupportedOperationException {
            message: format!("View '{view}' not available"),
        }
        .into());
    }
    // Writing INTO a mounted archive means rewriting the archive, which this
    // entry point does not do. Refuse by name rather than fall through to the
    // host writes below, which would otherwise chmod or re-time whatever the
    // sentinel-delimited path string happened to resolve to (nothing, today).
    if vfs_decode(&path).is_some() {
        return Err(RuntimeError::UnsupportedOperationException {
            message: format!(
                "setAttribute('{spec}'): the entries of a mounted archive are read-only"
            ),
        }
        .into());
    }
    if name.is_empty() || !known.iter().any(|k| *k == name) {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("'{view}:{name}' not recognized"),
        }
        .into());
    }

    // The JDK stats the file before it touches any attribute, so a missing path
    // is `NoSuchFileException` — not whichever errno the individual setter would
    // have produced, and not (for the DOS flags, whose view natives only
    // `std::fs::metadata`) a bare `IOException`. Measured against HotSpot. The
    // archive-entry case is already refused above, so this only ever sees a
    // host path.
    if std::fs::symlink_metadata(&path).is_err() {
        return Err(p57_no_such_file(ctx, &path)?);
    }

    // Every write below follows symlinks. Rather than silently write through a
    // link the caller asked us not to follow, refuse — the call sites that
    // matter never pass NOFOLLOW_LINKS, and a wrong target is worse than a
    // refusal.
    if nofollow
        && std::fs::symlink_metadata(&path)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
    {
        return Err(RuntimeError::UnsupportedOperationException {
            message: format!("setAttribute('{spec}'): NOFOLLOW_LINKS on a symbolic link"),
        }
        .into());
    }

    match attribute_write_kind(name) {
        AttributeWrite::Time => {
            let Some(ft) = value else {
                return Err(RuntimeError::NullPointerException {
                    message: Some(format!("setAttribute('{spec}'): null FileTime")),
                }
                .into());
            };
            // Type-check BEFORE reading. `filetime_read_millis` falls back to
            // slot 0 and then to `0`, so handing it a `String` would have set
            // the timestamp to the epoch and reported success — a silent wrong
            // write where the JDK's cast throws.
            require_value_class(ctx, ft, "java/nio/file/attribute/FileTime", spec)?;
            let millis = filetime_read_millis(ctx, ft);
            let (creation, access, modified) = match name {
                "creationTime" => (Some(millis), None, None),
                "lastAccessTime" => (None, Some(millis), None),
                _ => (None, None, Some(millis)),
            };
            set_file_attribute_times(&path, creation, access, modified)
                .map_err(|error| p57_io_error(&error))?;
            Ok(None)
        }
        AttributeWrite::DosFlag => {
            let on = boolean_attribute_value(ctx, spec, value)?;
            // Build the one-field synthetic view the `dos_view_set_*` natives
            // read their path out of — the same shape `getFileAttributeView`
            // hands to Java callers. Pin the Path across the allocation: a
            // moving young GC there would relocate it out from under us.
            let path_pin = ctx.pin_native_root(path_obj);
            let view_obj = try_alloc_concurrent_synthetic(
                ctx,
                "java/nio/file/attribute/DosFileAttributeView",
                1,
            )?;
            let path_obj = ctx.read_native_pin(path_pin, path_obj);
            ctx.set_field(view_obj, 0, Value::Object(Some(path_obj)));
            ctx.unpin_native_roots(path_pin);
            let view_args = [
                Value::Object(Some(view_obj)),
                Value::Int(if on { 1 } else { 0 }),
            ];
            match name {
                "readonly" => dos_view_set_read_only(ctx, &view_args),
                "hidden" => dos_view_set_hidden(ctx, &view_args),
                "archive" => dos_view_set_archive(ctx, &view_args),
                _ => dos_view_set_system(ctx, &view_args),
            }
        }
        AttributeWrite::Permissions => {
            let Some(set) = value else {
                return Err(RuntimeError::NullPointerException {
                    message: Some(format!("setAttribute('{spec}'): null permission set")),
                }
                .into());
            };
            // `posix_permission_bits_from_set` answers `0` for anything it
            // cannot probe, and `0` is also the legitimate answer for an EMPTY
            // set — so a non-collection value would `chmod 000` and report
            // success. `size()` separates the two: a real collection answers it
            // (with `0` when empty), a `String` does not have the method.
            //
            // gc-common w21-b: `size()` is Java, so the Set is pinned across it
            // and read back for the bit scan (it was scanned through its
            // pre-call address).
            let set_pin = ctx.pin_native_root(set);
            let sized = matches!(
                ctx.invoke_virtual(set, "size", "()I", &[]),
                Ok(Some(Value::Int(_)))
            );
            let set = ctx.read_native_pin(set_pin, set);
            ctx.unpin_native_roots(set_pin);
            if !sized {
                return Err(value_class_cast_error(ctx, set, "java/util/Set", spec));
            }
            let mode = posix_permission_bits_from_set(ctx, set);
            set_file_mode(&path, mode)
        }
        AttributeWrite::Mode => {
            let mode = int_attribute_value(ctx, spec, value)?;
            set_file_mode(&path, (mode as u32) & 0o7777)
        }
        AttributeWrite::Unimplemented => Err(RuntimeError::UnsupportedOperationException {
            message: format!("setAttribute('{spec}') is not implemented"),
        }
        .into()),
        // A name the reader answers but the JDK does not let anyone write.
        // The wording is HotSpot's own, verified against it: `BasicFileAttributeView`
        // raises `IllegalArgumentException("'" + name() + ":" + attribute + "'
        // not recognized")`.
        AttributeWrite::NotWritable => Err(RuntimeError::IllegalArgumentException {
            message: format!("'{view}:{name}' not recognized"),
        }
        .into()),
    }
}

/// The `ClassCastException` the JDK's own cast would raise for a `setAttribute`
/// value of the wrong type.
///
/// The `(… are in module java.base of loader 'bootstrap')` clause HotSpot
/// appends is deliberately NOT reproduced: it would mean asserting module and
/// loader facts this call site has not looked up.
pub fn value_class_cast_error(
    ctx: &mut dyn NativeContext,
    value: ObjectRef,
    want: &str,
    spec: &str,
) -> MethodCallFailed {
    let class_id = ctx.class_id_of_object(value);
    let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
    RuntimeError::ClassCastException {
        message: format!(
            "class {} cannot be cast to class {} (setting '{spec}')",
            class_name.replace('/', "."),
            want.replace('/', ".")
        ),
    }
    .into()
}

/// Require a `setAttribute` value to be exactly the class the attribute wants.
///
/// Not merely cosmetic: the reader on the other side of each write arm has a
/// benign fallback (`filetime_read_millis` answers `0` for an object with no
/// `value` field), so without this the mistake becomes a quiet wrong write
/// instead of the `ClassCastException` the JDK raises.
pub fn require_value_class(
    ctx: &mut dyn NativeContext,
    value: ObjectRef,
    want: &str,
    spec: &str,
) -> Result<(), MethodCallFailed> {
    let class_id = ctx.class_id_of_object(value);
    if ctx.class_name_of_id(class_id).unwrap_or_default() == want {
        return Ok(());
    }
    Err(value_class_cast_error(ctx, value, want, spec))
}

/// chmod for the POSIX/unix write paths. A no-op refusal on a host without
/// POSIX modes, rather than a silent success.
pub fn set_file_mode(path: &str, mode: u32) -> MethodCallResult {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|error| p57_io_error(&error))?;
        Ok(None)
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Err(RuntimeError::UnsupportedOperationException {
            message: "POSIX file modes are not supported on this host".to_string(),
        }
        .into())
    }
}

/// Unbox the `Object` value of a boolean-valued attribute. The JDK casts, so a
/// wrong type is a `ClassCastException` there and here.
pub fn boolean_attribute_value(
    ctx: &mut dyn NativeContext,
    spec: &str,
    value: Option<ObjectRef>,
) -> Result<bool, cratonvm_types::error::MethodCallFailed> {
    let Some(value) = value else {
        return Err(RuntimeError::ClassCastException {
            message: format!("setAttribute('{spec}'): value is not a Boolean"),
        }
        .into());
    };
    // Exact class, not "whatever unboxes to an Int". `unbox_value` answers
    // `Int` for Integer/Byte/Short/Character too, and the JDK's cast to
    // `Boolean` rejects every one of them.
    require_value_class(ctx, value, "java/lang/Boolean", spec)?;
    match crate::lang_class::unbox_value(ctx, value) {
        Value::Int(i) => Ok(i != 0),
        _ => Err(RuntimeError::ClassCastException {
            message: format!("setAttribute('{spec}'): value is not a Boolean"),
        }
        .into()),
    }
}

/// Unbox the `Object` value of an int-valued attribute (`unix:mode`).
pub fn int_attribute_value(
    ctx: &mut dyn NativeContext,
    spec: &str,
    value: Option<ObjectRef>,
) -> Result<i32, cratonvm_types::error::MethodCallFailed> {
    match value.map(|v| crate::lang_class::unbox_value(ctx, v)) {
        Some(Value::Int(i)) => Ok(i),
        Some(Value::Long(l)) => Ok(l as i32),
        Some(_) | None => Err(RuntimeError::ClassCastException {
            message: format!("setAttribute('{spec}'): value is not an Integer"),
        }
        .into()),
    }
}

/// Read `path`'s attributes named by `spec` into a `java.util.HashMap`.
///
/// This is the whole of `Files.readAttributes(Path, String, LinkOption...)`,
/// `Files.getAttribute` and `FileSystemProvider.readAttributes(Path, String,
/// LinkOption...)`. It follows the JDK's error contract rather than degrading:
/// an unknown view is `UnsupportedOperationException`, an unknown attribute name
/// is `IllegalArgumentException`, and a missing file is `NoSuchFileException`.
/// An empty map is never a valid answer here — the previous implementation
/// returned one for every call, and every caller reads the map with `get`, so
/// the failure surfaced as a `null` attribute arbitrarily far away.
pub fn read_named_attributes(
    ctx: &mut dyn NativeContext,
    mut path_obj: ObjectRef,
    spec: &str,
    nofollow: bool,
) -> MethodCallResult {
    let (view, requested) = split_attribute_spec(spec);
    let Some(known) = attribute_names_for_view(view) else {
        return Err(RuntimeError::UnsupportedOperationException {
            message: format!("View '{view}' not available"),
        }
        .into());
    };
    // gc-common w21-b: read ONCE. The text was read twice, the second time
    // through `path_obj`'s address from before the first read -- which runs
    // Java for a foreign Path (and ran its `toString()` twice).
    let path = p57_read_path(ctx, &mut path_obj);
    if !attribute_view_names_for_path(&path).contains(&view) {
        return Err(RuntimeError::UnsupportedOperationException {
            message: format!("View '{view}' not available"),
        }
        .into());
    }
    if requested.is_empty() {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("'{spec}' not recognized"),
        }
        .into());
    }
    let wildcard = requested == "*";
    let names: Vec<&str> = if wildcard {
        known.to_vec()
    } else {
        let mut out = Vec::new();
        for name in requested.split(',') {
            let name = name.trim();
            if !known.iter().any(|k| *k == name) {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!("'{view}:{name}' not recognized"),
                }
                .into());
            }
            out.push(name);
        }
        out
    };

    // An entry inside a mounted archive is answered from the archive. Its path
    // string is the sentinel-delimited `\x01JARFS\x01…` form, which names no
    // host file at all, so `stat`ing it produced `NoSuchFileException` for every
    // `Files.readAttributes` / `Files.getAttribute` on a zip or jrt entry.
    let facts = match vfs_stat_facts(&path) {
        Some(Ok(f)) => f,
        Some(Err(_)) => return Err(p57_no_such_file(ctx, &path)?),
        None => match stat_facts(&path, nofollow) {
            Ok(f) => f,
            Err(_) => return Err(p57_no_such_file(ctx, &path)?),
        },
    };

    // gc-common w18-f: `path_obj` is read again below (the `owner`/`group`
    // attribute object) AFTER the map's constructor, which runs Java; it is
    // pinned first, under the map's pin, and every exit below releases both.
    let path_pin = ctx.pin_native_root(path_obj);
    let map = match ctx.new_object_initialized("java/util/LinkedHashMap", "()V", &[]) {
        Ok(Some(Value::Object(Some(m)))) => m,
        // The class library could not give us a Map. Refuse rather than hand
        // back a shape the caller will silently read `null` out of.
        _ => {
            ctx.unpin_native_roots(path_pin);
            return Err(RuntimeError::IOException {
                message: format!("readAttributes({spec}): could not allocate the result map"),
            }
            .into());
        }
    };
    let map_pin = ctx.pin_native_root(map);

    // `owner`/`group` come from the attribute object the rest of this file
    // already builds, so principals stay one implementation.
    let mut owner_group: Option<ObjectRef> = None;
    if names.iter().any(|n| *n == "owner" || *n == "group") {
        let path_now = ctx.read_native_pin(path_pin, path_obj);
        if let Ok(Some(Value::Object(Some(attrs)))) =
            p59_files_read_attributes(ctx, &[Value::Object(Some(path_now))])
        {
            owner_group = Some(attrs);
        }
    }
    let attrs_pin = owner_group.map(|a| ctx.pin_native_root(a));

    for name in names {
        // The key is built and pinned BEFORE the value, and nothing between the
        // value and the `put` allocates. The other order — value, then
        // `create_string(name)` — holds a fresh, unrooted attribute object
        // across an allocation, which is the native stale-local family: a young
        // collection there relocates it and the map gets a dangling entry.
        let key = ctx.create_string(name);
        let key_pin = ctx.pin_native_root(key);
        let value: Value = match name {
            "lastModifiedTime" => Value::Object(Some(filetime_alloc(ctx, facts.modified_millis)?)),
            "lastAccessTime" => Value::Object(Some(filetime_alloc(ctx, facts.access_millis)?)),
            "creationTime" => Value::Object(Some(filetime_alloc(ctx, facts.creation_millis)?)),
            "ctime" => Value::Object(Some(filetime_alloc(ctx, facts.ctime_millis)?)),
            "size" => box_long(ctx, facts.size),
            "isRegularFile" => box_boolean(ctx, facts.is_regular),
            "isDirectory" => box_boolean(ctx, facts.is_dir),
            "isSymbolicLink" => box_boolean(ctx, facts.is_symlink),
            "isOther" => box_boolean(ctx, !facts.is_regular && !facts.is_dir && !facts.is_symlink),
            // The JDK's Unix fileKey is `(dev, ino)`. We have both; render them
            // as the same `String` the JDK's `UnixFileKey.toString` produces so
            // two reads of the same file compare equal.
            "fileKey" => {
                if facts.dev == 0 && facts.ino == 0 {
                    Value::Object(None)
                } else {
                    let s = ctx
                        .create_string(&format!("(dev={:x},ino={})", facts.dev as u64, facts.ino));
                    Value::Object(Some(s))
                }
            }
            "mode" => box_int(ctx, facts.mode),
            "nlink" => box_int(ctx, facts.nlink),
            "uid" => box_int(ctx, facts.uid),
            "gid" => box_int(ctx, facts.gid),
            "dev" => box_long(ctx, facts.dev),
            "ino" => box_long(ctx, facts.ino),
            "rdev" => box_long(ctx, facts.rdev),
            "readonly" => box_boolean(ctx, facts.readonly),
            "hidden" => box_boolean(ctx, facts.hidden),
            // On Windows these are the real `FILE_ATTRIBUTE_*` bits; off
            // Windows `dos_attr_flag` answers `false`, which is what the JDK
            // reports for them on a non-DOS filesystem.
            "system" => box_boolean(ctx, facts.system),
            "archive" => box_boolean(ctx, facts.archive),
            "attributes" => box_int(ctx, facts.dos_attributes),
            "permissions" => match posix_permission_set(ctx, facts.mode) {
                Some(set) => Value::Object(Some(set)),
                None => continue,
            },
            "owner" | "group" => {
                let Some((attrs, pin)) = owner_group.zip(attrs_pin) else {
                    continue;
                };
                let attrs = ctx.read_native_pin(pin, attrs);
                let (method, desc) = if name == "owner" {
                    ("owner", "()Ljava/nio/file/attribute/UserPrincipal;")
                } else {
                    ("group", "()Ljava/nio/file/attribute/GroupPrincipal;")
                };
                match ctx.invoke_virtual(attrs, method, desc, &[]) {
                    Ok(Some(v @ Value::Object(Some(_)))) => v,
                    // Not answerable on this platform/path. Omitting it from a
                    // `*` read matches what the JDK does for a view it cannot
                    // fully serve; an explicit request says so out loud.
                    _ if wildcard => continue,
                    _ => {
                        ctx.unpin_native_roots(path_pin);
                        return Err(RuntimeError::UnsupportedOperationException {
                            message: format!("'{view}:{name}' is not available for {path}"),
                        }
                        .into());
                    }
                }
            }
            _ => continue,
        };
        let key = ctx.read_native_pin(key_pin, key);
        let map = ctx.read_native_pin(map_pin, map);
        let put = ctx.invoke_virtual(
            map,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(key)), value],
        );
        if let Err(e) = put {
            // Release the pin frame before unwinding: `?` here would leave the
            // map (and the attribute object) pinned for the rest of the VM's
            // life.
            ctx.unpin_native_roots(path_pin);
            return Err(e);
        }
    }

    let map = ctx.read_native_pin(map_pin, map);
    ctx.unpin_native_roots(path_pin);
    Ok(Some(Value::Object(Some(map))))
}

/// Extracted from the inline closure `r.register(files, "delete"/"copy"/
/// "move"/"createDirectory", ...)` used to be, so `FileSystemProvider`'s own
/// `delete`/`copy`/`move`/`createDirectory` natives (below) can call the SAME
/// body as a plain Rust function call instead of through `ctx.invoke("java/nio/file/Files", ...)`.
///
/// That distinction is load-bearing under `--real-jdk`: real JDK bytecode for
/// `Files.delete`/`copy`/`move`/`createDirectory` is `provider(path).X(path, ...)`,
/// i.e. it calls back into `FileSystemProvider.X` -- which is registered as
/// exactly the native below it. Going through `ctx.invoke` let strict mode's
/// bytecode-preferred resolution alternate Files-native -> Files-bytecode ->
/// FileSystemProvider-native -> Files-native(invoke) -> Files-bytecode -> ...
/// forever: a stack overflow (SIGSEGV, not a Java exception), reproduced by
/// `AbstractReceiverSweep` under `--real-jdk`. A direct function call cannot
/// re-enter bytecode dispatch, so the cycle cannot form.
pub fn native_files_delete(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut path_obj = obj_arg(args, 0)?;
    let p = p57_read_path(ctx, &mut path_obj);
    // Virtual filesystems (jarfs/memfs sentinels) are not host paths;
    // `symlink_metadata` on the encoded string always ENOENTs, so keep them
    // on the best-effort route rather than inventing a NoSuchFileException.
    if vfs_classify(&p).is_some() {
        p57_delete_path(&p);
        return Ok(None);
    }
    // `p57_delete_error` builds the exception object and so can itself be
    // refused; it cannot sit inside `map_err`, whose closure has to return
    // the error value directly.
    if let Err(e) = p57_delete_path_checked(&p) {
        return Err(p57_delete_error(ctx, &p, &e)?);
    }
    Ok(None)
}

/// `java/nio/file/Files.copy(Path,Path,CopyOption...)`'s real body, factored
/// out so both the `Files` registration and `fsp_delegate_two_path_copy`
/// below can call it DIRECTLY rather than through `ctx.invoke`. Calling back
/// into `Files.copy` by name from the provider-level shim used to be safe
/// only because a registered native unconditionally wins in compatible mode;
/// under `--real-jdk`, `Files.copy` has real bytecode and a `Bridge`
/// registration (not `Intrinsic`), so strict-mode dispatch prefers that real
/// bytecode instead — which calls straight back into
/// `FileSystemProvider.copy`, which called `Files.copy` again: unconditional
/// infinite recursion, `StackOverflowError` on EVERY `Files.copy` call
/// (self-copy or not) under `--real-jdk`, not just the self-copy case this
/// was first measured from. A plain Rust function call has no such mode
/// dependency. `native_files_delete`/`native_files_create_directory` above
/// carry the identical fix for `Files.delete`/`createDirectory`, found the
/// same way: measured immediately after this fix landed, in a plain
/// copy+move+delete sequence.
///
/// The two platforms did NOT fail the same way, which is why this was found
/// on Linux and not before. On Windows the provider halves are retired in the
/// same table, so the loop ends inside the JDK after one turn. On Linux they
/// are not -- the `UnixNativeDispatcher` family the JDK would then need was
/// unimplemented -- so the loop is unbounded: MEASURED 2026-09-20 on
/// linux/x86_64, `java/lang/StackOverflowError` with a stack of nothing but
/// `java/nio/file/Files.copy`, on every `--real-jdk` probe that creates,
/// deletes, copies or moves a file: seven of lane 4's own probes and `L4Diag`
/// among them.
pub fn native_files_copy(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Read the options FIRST: the probe re-enters Java, and a moving
    // young GC there would relocate any `ObjectRef` already lifted out
    // of `args` (the native stale-local family).
    // A null `CopyOption[]` is an NPE (`Set.of(options)` walks it),
    // not "no options". An absent argument is a different thing and is
    // left alone.
    if matches!(args.get(2), Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    }
    //
    // gc-common w20-d: that reasoning was backwards. The `args` slice is NOT
    // remapped by a collection (the caller's frame roots the arguments, but
    // this copy of them is not rewritten), so reading `src` / `dst` out of
    // `args` AFTER the probe is what handed back pre-collection addresses.
    // Both are lifted and rooted first; the target (returned) stays rooted
    // across the probe and both `p57_read_path`s (a foreign Path's
    // `toString()` is Java too), and is read back after them.
    let src = obj_arg(args, 0)?;
    let dst = obj_arg(args, 1)?;
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let src_h = scope.root(src);
    let dst_h = scope.root(dst);
    let replace_existing = copy_options_replace_existing(&mut *scope, args.get(2));
    let mut src = scope.get(&src_h);
    let src_path = p57_read_path(&mut *scope, &mut src);
    let mut dst = scope.get(&dst_h);
    let dst_path = p57_read_path(&mut *scope, &mut dst);
    drop(scope);
    // `Files.copy(p, p)`: real `UnixCopyFile.copy`/`WindowsCopyFile.copy`
    // both open with `if (Util.isSameFile(source, target)) return;` — an
    // unconditional silent no-op, checked before REPLACE_EXISTING or
    // anything else. `std::fs::copy` below has no such guard: it opens the
    // destination with truncate-on-open, so when source and target name the
    // SAME file, by the time the read side runs the file has already been
    // truncated to zero bytes UNDER IT — the call still returns `Ok`, so the
    // caller sees no exception and DATA LOSS (source content replaced by
    // nothing), not merely a wrong-typed error. `paths_name_the_same_file`
    // (identity via st_dev/st_ino on Unix, volume+file-index on Windows —
    // the same helper `FileSystemProvider.isSameFile` above uses) catches
    // this for hard links and differing spellings too, not just an
    // identical path string. Skip for jarfs-encoded paths, which have no
    // inode to compare.
    if jarfs_decode(&src_path).is_none() && jarfs_decode(&dst_path).is_none() {
        if let Ok(true) = cratonvm_native_io::file_channel::paths_name_the_same_file(
            std::path::Path::new(&src_path),
            std::path::Path::new(&dst_path),
        ) {
            return Ok(Some(Value::Object(Some(dst))));
        }
    }
    // Without REPLACE_EXISTING, `Files.copy` onto an existing target
    // must throw `FileAlreadyExistsException` — callers catch it BY
    // TYPE, so a generic IOException will not do. `std::fs::copy`
    // below overwrites unconditionally and the directory arm swallows
    // `AlreadyExists`, so this is the only place the contract can be
    // enforced. The sibling `Files.move` native already does exactly
    // this with the same two helpers. Skip the check for jarfs-encoded
    // targets (not real filesystem paths) and for a self-copy.
    if !replace_existing
        && src_path != dst_path
        && jarfs_decode(&dst_path).is_none()
        && std::fs::symlink_metadata(&dst_path).is_ok()
    {
        return Err(p57_file_already_exists(ctx, &dst_path));
    }
    // Java `Files.copy(Path,Path,CopyOption...)`: copying a DIRECTORY
    // creates an (empty) directory at the target — it does NOT open the
    // source as a file. `std::fs::copy` only handles regular files and on
    // a directory fails ("Access denied / os error 5" on Windows), which
    // broke every `TomcatBaseTest.recursiveCopy` (the whole
    // `catalina.webresources` cluster — `preVisitDirectory` does
    // `Files.copy(dir, …)`). Branch on the source kind; tolerate an
    // already-existing target dir (mirrors the file path's overwrite).
    // Classify the SOURCE too: `src_path` may itself be a jarfs-encoded
    // entry (e.g. a Path from `FileSystems.newFileSystem(zipPath, ...)`,
    // as used by Quarkus's `ZipUtils.unzip`/`copyFromZip` to extract a
    // mounted zip's contents to the real filesystem). Previously this
    // only special-cased a jarfs DESTINATION and always read the source
    // via `std::fs::symlink_metadata`/`std::fs::copy`, which treats the
    // jarfs-encoded source string as a literal OS path — it never is
    // one, so both calls failed with a raw `NotFound` ("No such file or
    // directory (os error 2)"), surfacing as `IllegalStateException:
    // IOException: No such file or directory (os error 2)` even though
    // `Files.isDirectory`/`isRegularFile` on the same Path (which DO
    // classify via `vfs_classify`/`jarfs_classify`) reported correctly.
    let src_jarfs = jarfs_decode(&src_path);
    let src_is_dir = if let Some((ref jar, ref entry)) = src_jarfs {
        matches!(jarfs_classify(jar, entry), JarFsKind::Dir)
    } else {
        std::fs::symlink_metadata(&src_path)
            .map(|m| m.is_dir())
            .unwrap_or(false)
    };
    let result = if let Some((dst_jar, dst_entry)) = jarfs_decode(&dst_path) {
        if src_is_dir {
            jarfs_create_dir_entry(&dst_jar, &dst_entry)
        } else if let Some((src_jar, src_entry)) = &src_jarfs {
            jarfs_read_entry(src_jar, src_entry)
                .and_then(|bytes| jarfs_write_file_entry(&dst_jar, &dst_entry, &bytes))
        } else {
            std::fs::read(&src_path)
                .and_then(|bytes| jarfs_write_file_entry(&dst_jar, &dst_entry, &bytes))
        }
    } else if src_is_dir {
        // REPLACE_EXISTING deletes the target first, and deleting a
        // NON-EMPTY directory is `DirectoryNotEmptyException`. The
        // `AlreadyExists => Ok(())` arm below swallowed that case
        // whole: `Files.copy(dir, someNonEmptyDir, REPLACE_EXISTING)`
        // returned normally having done nothing, so a caller that
        // believes it has just replaced a tree carries on over the old
        // one. MEASURED against HotSpot.
        if std::fs::symlink_metadata(&dst_path).is_ok_and(|m| m.is_dir())
            && std::fs::read_dir(&dst_path).is_ok_and(|mut d| d.next().is_some())
        {
            return Err(match p57_directory_not_empty(ctx, &dst_path) {
                Ok(f) => f,
                Err(f) => f,
            });
        }
        match std::fs::create_dir(&dst_path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(e),
        }
    } else if let Some((src_jar, src_entry)) = &src_jarfs {
        jarfs_read_entry(src_jar, src_entry).and_then(|bytes| std::fs::write(&dst_path, &bytes))
    } else {
        std::fs::copy(&src_path, &dst_path).map(|_| ())
    };
    match result {
        Ok(()) => Ok(Some(Value::Object(Some(dst)))),
        // The failing path is the SOURCE for a missing/undreadable
        // source and the TARGET for a missing parent directory; the
        // error kind tells them apart only when the source is present.
        Err(e) => {
            let blamed = if std::fs::symlink_metadata(&src_path).is_err() {
                src_path.clone()
            } else {
                dst_path.clone()
            };
            Err(p57_fs_error(ctx, &e, &blamed))
        }
    }
}

/// `java/nio/file/Files.move(Path,Path,CopyOption...)`'s real body — see
/// `native_files_copy` just above for why this must be a direct function call
/// rather than a `ctx.invoke("java/nio/file/Files", "move", ...)` bounce.
pub fn native_files_move(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let src = obj_arg(args, 0)?;
    let dst = obj_arg(args, 1)?;
    // gc-common w20-d: the target (returned) and the option array were used
    // through their entry addresses after Java ran -- a foreign Path's
    // `toString()` in `p57_read_path`, each option's `toString()` (the array
    // was re-read from `args`, which is not remapped, after every one), and
    // the exception's `<init>`, whose receiver was held across its message
    // String. All rooted, read back after each call.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let dst_h = scope.root(dst);
    let opts_h = match args.get(2) {
        Some(Value::Object(Some(opts))) => Some(scope.root(*opts)),
        _ => None,
    };
    let mut src = src;
    let src_path = p57_read_path(&mut *scope, &mut src);
    let mut dst = scope.get(&dst_h);
    let dst_path = p57_read_path(&mut *scope, &mut dst);
    // Real Files.move contract: without REPLACE_EXISTING, throw
    // FileAlreadyExistsException if the target already exists. Plain
    // std::fs::rename is POSIX rename(2) semantics, which silently
    // replaces the destination unconditionally -- so H2's
    // FilePathDisk.moveTo(newName, false) (no REPLACE_EXISTING) never
    // saw the FileAlreadyExistsException it catches to translate into
    // DbException(FILE_RENAME_FAILED_2), letting
    // TestFileSystem.testMoveTo's move-onto-existing-file case
    // through instead of rejecting it (
    // bug-h2-files-setposixfilepermissions-FIXED.md residual chain).
    // ATOMIC_MOVE also implies replacement: it is specified as a single
    // filesystem operation, which on every platform CratonVM targets is
    // `rename(2)` / `MoveFileEx(MOVEFILE_REPLACE_EXISTING)` — the target
    // is replaced, not rejected. Treating it as "no replace" made
    // Spring Boot's Kubernetes ConfigMap atomic-swap case
    // (`FileWatcherTests.shouldTriggerOnConfigMapAtomicMoveUpdates`,
    // which moves a fresh `..data` symlink over the live one) fail with
    // `FileAlreadyExistsException`.
    let mut replace_existing = false;
    if let Some(opts_h) = &opts_h {
        let opts = scope.get(opts_h);
        let len = scope.array_length(opts);
        for i in 0..len {
            let opts = scope.get(opts_h);
            if let Value::Object(Some(opt)) = scope.get_array_element(opts, i) {
                if let Ok(Some(Value::Object(Some(s)))) =
                    scope.invoke_virtual(opt, "toString", "()Ljava/lang/String;", &[])
                {
                    let name = scope.read_string(s).unwrap_or_default();
                    if name.contains("REPLACE_EXISTING") || name.contains("ATOMIC_MOVE") {
                        replace_existing = true;
                    }
                }
            }
        }
    }
    if !replace_existing && src_path != dst_path && std::fs::symlink_metadata(&dst_path).is_ok() {
        // Build a REAL java/nio/file/FileAlreadyExistsException via
        // its real single-String constructor (same pattern as
        // throw_unsupported_charset_exception below) rather than a
        // synthetic layout -- this exception is caught by H2's own
        // real FilePathDisk.moveTo bytecode (catch
        // (FileAlreadyExistsException ex)) and its getFile() may be
        // read by real Throwable formatting, so it needs genuine
        // field layout, not a guessed synthetic one.
        //
        // Round 15 wave 4, lane jdkonly4
        // (`r15w3-jdkonly3-residual-new-object-init-builders-patch-FIXED-20260930.md`
        // #4): the shared builder every other `FileAlreadyExistsException`
        // of this file uses. This block built its own: the path unrendered by
        // `p57_exception_path` (forward slashes on Windows, where the others
        // answer `\`), interned, and a throwing constructor ignored.
        return Err(p57_file_already_exists(&mut *scope, &dst_path));
    }
    match std::fs::rename(&src_path, &dst_path) {
        Ok(()) => Ok(Some(Value::Object(Some(scope.get(&dst_h))))),
        Err(e) => {
            let blamed = if std::fs::symlink_metadata(&src_path).is_err() {
                src_path.clone()
            } else {
                dst_path.clone()
            };
            Err(p57_fs_error(&mut *scope, &e, &blamed))
        }
    }
}

/// Root a `Files.createDirector{y,ies}` target and its `FileAttribute[]`
/// argument (when non-null) in `scope`.
///
/// gc-common w20-d: both natives return the target `Path`, and both read it
/// back only after Java had run -- a foreign Path's `toString()` in
/// [`p57_read_path`], and `name()` / `value()` on a real `FileAttribute` in
/// [`posix_mode_from_file_attributes`] -- so the caller got the Path's
/// pre-collection address; the attribute array was read from `args` (not
/// remapped) after the first of those.
pub fn p57_root_path_and_attrs(
    scope: &mut cratonvm_native_api::NativeHandleScope<'_>,
    path_obj: ObjectRef,
    attrs: Option<&Value>,
) -> (
    cratonvm_native_api::NativeHandle,
    Option<cratonvm_native_api::NativeHandle>,
) {
    let path_h = scope.root(path_obj);
    let attrs_h = match attrs {
        Some(Value::Object(Some(a))) => Some(scope.root(*a)),
        _ => None,
    };
    (path_h, attrs_h)}

pub fn native_files_create_directory(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut path_obj = obj_arg(args, 0)?;
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let (path_h, attrs_h) = p57_root_path_and_attrs(&mut scope, path_obj, args.get(1));
    let ctx = &mut *scope;
    let p = p57_read_path(ctx, &mut path_obj);
    if let Some((jar, entry)) = jarfs_decode(&p) {
        return match jarfs_create_dir_entry(&jar, &entry) {
            Ok(()) => Ok(Some(Value::Object(Some(scope.get(&path_h))))),
            Err(e) => Err(p57_io_error(&e)),
        };
    }
    // Honor `PosixFilePermissions.asFileAttribute(...)` (arg 1): the
    // real JDK passes it straight to `mkdir(2)`. Ignoring it left
    // every such directory at the process umask.
    let attrs = attrs_h.as_ref().map(|h| Value::Object(Some(scope.get(h))));
    let mode = posix_mode_from_file_attributes(&mut *scope, attrs.as_ref());
    let path_obj = scope.get(&path_h);
    let ctx = &mut *scope;
    match create_dir_with_mode(&p, mode) {
        Ok(()) => Ok(Some(Value::Object(Some(path_obj)))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(p57_no_such_file(ctx, &p)?),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            Err(p57_access_denied(ctx, &p)?)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(p57_file_already_exists_synthetic(ctx, &p)?)
        }
        Err(e) => Err(p57_io_error(&e)),
    }
}

pub fn fsp_delegate_two_path_copy(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
) -> MethodCallResult {
    let src = obj_arg(args, 1)?;
    let dst = obj_arg(args, 2)?;
    let options = args.get(3).copied().unwrap_or(Value::Object(None));
    let inner_args = [Value::Object(Some(src)), Value::Object(Some(dst)), options];
    // Calls the extracted body directly (not ctx.invoke) -- see the note
    // above `native_files_delete`.
    match method {
        "copy" => native_files_copy(ctx, &inner_args)?,
        "move" => native_files_move(ctx, &inner_args)?,
        _ => unreachable!("fsp_delegate_two_path_copy only dispatches copy/move"),
    };
    Ok(None)
}


/// `Path.of(URI)` for a non-`file` scheme, the way its own bytecode answers
/// it: the first of `FileSystemProvider.installedProviders()` whose
/// `getScheme()` equals `scheme` (ignoring case) resolves the URI with
/// `getPath(URI)`; none is `FileSystemNotFoundException`. Every call is a real
/// dispatch, so the providers' own code decides -- only the loop is here.
pub fn path_of_uri_through_installed_providers(
    ctx: &mut dyn NativeContext,
    uri: ObjectRef,
    scheme: &str,
) -> MethodCallResult {
    let base = ctx.pin_native_root(uri);
    let found = (|| -> Result<Option<Value>, MethodCallFailed> {
        let Some(Value::Object(Some(list))) = ctx.invoke(
            "java/nio/file/spi/FileSystemProvider",
            "installedProviders",
            "()Ljava/util/List;",
            &[],
        )?
        else {
            return Ok(None);
        };
        let list_pin = ctx.pin_native_root(list);
        let n = match ctx.invoke_virtual(list, "size", "()I", &[])? {
            Some(Value::Int(n)) => n,
            _ => 0,
        };
        for i in 0..n {
            let list = ctx.read_native_pin(list_pin, list);
            let Some(Value::Object(Some(provider))) =
                ctx.invoke_virtual(list, "get", "(I)Ljava/lang/Object;", &[Value::Int(i)])?
            else {
                continue;
            };
            let provider_pin = ctx.pin_native_root(provider);
            let claims =
                match ctx.invoke_virtual(provider, "getScheme", "()Ljava/lang/String;", &[])? {
                    Some(Value::Object(Some(s))) => ctx
                        .read_string(s)
                        .is_some_and(|s| s.eq_ignore_ascii_case(scheme)),
                    _ => false,
                };
            let provider = ctx.read_native_pin(provider_pin, provider);
            ctx.unpin_native_roots(provider_pin);
            if claims {
                let uri = ctx.read_native_pin(base, uri);
                let path = ctx.invoke_virtual(
                    provider,
                    "getPath",
                    "(Ljava/net/URI;)Ljava/nio/file/Path;",
                    &[Value::Object(Some(uri))],
                )?;
                return Ok(Some(path.unwrap_or(Value::Object(None))));
            }
        }
        Ok(None)
    })();
    ctx.unpin_native_roots(base);
    match found? {
        Some(path) => Ok(Some(path)),
        None => Err(p57_file_system_not_found(ctx, scheme)),
    }
}

/// `Provider "<scheme>" not installed` — a typed
/// `java.nio.file.FileSystemNotFoundException`.
///
/// The TYPE is what callers key on: a fallback that catches
/// `FileSystemNotFoundException` to try a different resolution strategy does not
/// match a supertype instance, exactly as with [`p57_closed_channel`]. Falls
/// back to `IllegalArgumentException` when the class is not in the image, which
/// is still a refusal rather than a wrong Path.
pub fn p57_file_system_not_found(ctx: &mut dyn NativeContext, scheme: &str) -> MethodCallFailed {
    let msg = format!("Provider \"{scheme}\" not installed");
    // Round 15 wave 3, lane jdkonly2: `new_object_initialized` (rooted across
    // `<init>`, forwarded result). The old body held the `new_object` address
    // raw across the message's allocation and the constructor, and threw it.
    let s = ctx.create_string_uninterned(&msg);
    if let Ok(Some(Value::Object(Some(exc)))) = ctx.new_object_initialized(
        "java/nio/file/FileSystemNotFoundException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(s))],
    ) {
        return MethodCallFailed::ExceptionThrown(exc);
    }
    RuntimeError::IllegalArgumentException { message: msg }.into()
}

/// Refuse `FileSystems.newFileSystem` over a path that is plainly not an
/// archive.
///
/// **Part three recorded this as an unclaimed residual and it was measurable
/// all along** — `L4CensusTail.java` asks HotSpot directly:
///
/// ```text
///   FileSystems.newFileSystem(<a text file>, (ClassLoader) null)
///     HotSpot   ProviderNotFoundException
///     this VM   a jar filesystem over a text file
/// ```
///
/// The reason for deferring was blast radius: these three registrations are on
/// the hot path for every jar this VM opens. That reason survives, and it is why
/// the check is a MAGIC-BYTE test rather than a real probe of the archive — a
/// file whose first two bytes are `PK` still takes exactly the path it took
/// before, and so does anything this cannot read (a jar-FS sentinel, a missing
/// path, a directory). Only a readable regular file that is definitively not a
/// zip is refused.
///
/// Returning a filesystem for a non-archive is worse than it sounds: the
/// failure does not appear here, it appears at the first entry lookup, in a
/// caller with no idea the mount was bogus.
pub fn p57_refuse_non_archive(ctx: &mut dyn NativeContext, path: &str) -> Option<MethodCallFailed> {
    // `jar:` sentinels and anything with an entry separator are this VM's own
    // encoding, not host paths — never second-guess them.
    if path.starts_with("jar:") || path.contains("!/") {
        return None;
    }
    let md = std::fs::metadata(path).ok()?;
    if !md.is_file() {
        return None;
    }
    let mut head = [0u8; 2];
    {
        use std::io::Read;
        let mut f = std::fs::File::open(path).ok()?;
        if f.read_exact(&mut head).is_err() {
            // Shorter than two bytes: cannot be a zip, and cannot be anything
            // else this serves either.
            return Some(p57_provider_not_found(ctx, path));
        }
    }
    if &head == b"PK" {
        return None;
    }
    Some(p57_provider_not_found(ctx, path))
}

/// A typed `java.nio.file.ProviderNotFoundException`, for the same reason
/// [`p57_file_system_not_found`] is typed: callers catch the exact class.
pub fn p57_provider_not_found(ctx: &mut dyn NativeContext, path: &str) -> MethodCallFailed {
    let msg = format!("Provider not found for {path}");
    // Round 15 wave 3, lane jdkonly2: rooted construction, as in
    // `p57_file_system_not_found`.
    let s = ctx.create_string_uninterned(&msg);
    if let Ok(Some(Value::Object(Some(exc)))) = ctx.new_object_initialized(
        "java/nio/file/ProviderNotFoundException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(s))],
    ) {
        return MethodCallFailed::ExceptionThrown(exc);
    }
    RuntimeError::IOException { message: msg }.into()
}

/// `FileStore.getAttribute`'s refusal, with the name the caller asked for.
///
/// The JDK's message is the attribute string itself; a constant "no such
/// attribute" cannot tell a caller which of several names in a loop failed.
pub fn unsupported_file_store_attribute(spec: &str) -> MethodCallFailed {
    RuntimeError::UnsupportedOperationException {
        message: format!("'{spec}' not recognized"),
    }
    .into()
}

pub fn box_long(ctx: &mut dyn NativeContext, v: i64) -> Value {
    crate::lang_class::box_value(ctx, Value::Long(v), "J")
}

pub fn box_int(ctx: &mut dyn NativeContext, v: i32) -> Value {
    crate::lang_class::box_value(ctx, Value::Int(v), "I")
}

pub fn box_boolean(ctx: &mut dyn NativeContext, v: bool) -> Value {
    crate::lang_class::box_value(ctx, Value::Int(i32::from(v)), "Z")
}

pub fn p59_files_read_attributes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Extract path from first argument (Path object, field 0 = string)
    let path_str = extract_path_string(ctx, args.first());

    // jar-FS path — attributes come from the archive listing, not the host
    // filesystem. The real-JDK `Files.walkFileTree` (FileTreeWalker) decides
    // dir-vs-file purely from this BFA; std::fs::metadata on the encoded
    // sentinel string ENOENTs, which made the walker treat every mounted-jar
    // root as a zero-length regular file (JUnit5 jar scanning found nothing).
    if let Some((jar, entry)) = jarfs_decode(&path_str) {
        let bfa = basic_file_attributes_alloc(ctx)?;
        let (is_dir, size) = match jarfs_classify(&jar, &entry) {
            JarFsKind::Dir => (1, 0i64),
            JarFsKind::File => (0, jarfs_entry_size(&jar, &entry).unwrap_or(0)),
            JarFsKind::Absent => {
                // Real readAttributes throws NoSuchFileException (an
                // IOException) for missing files; FileTreeWalker catches it
                // and reports visitFileFailed instead of walking garbage.
                //
                // FileTreeWalker catches `IOException`, so the bare one that
                // used to be thrown here happened to work for the walker --
                // and only for the walker. Treating a missing file as OPTIONAL
                // is written `catch (NoSuchFileException)`, which a supertype
                // instance does not match: that is how SmallRye's optional
                // config load turned into a boot failure elsewhere in this
                // campaign. Same fix as `checkAccess` above.
                return Err(p57_no_such_file(ctx, &path_str)?);
            }
        };
        basic_file_attributes_store(ctx, bfa, is_dir != 0, size, 0, 0, 0, 0);
        return Ok(Some(Value::Object(Some(bfa))));
    }

    // jrt-FS (runtime image) path — attributes come from the jimage, not the
    // host filesystem. javac's platform-class indexing walks `/modules/...`.
    if let Some((java_home, entry)) = jrtfs_decode(&path_str) {
        let bfa = basic_file_attributes_alloc(ctx)?;
        let (is_dir, size) = match jrtfs_classify(&java_home, &entry) {
            JarFsKind::Dir => (1, 0i64),
            JarFsKind::File => (0, jrtfs_entry_size(&java_home, &entry).unwrap_or(0)),
            JarFsKind::Absent => {
                // Typed, for the reason on the jar arm above.
                return Err(p57_no_such_file(ctx, &path_str)?);
            }
        };
        basic_file_attributes_store(ctx, bfa, is_dir != 0, size, 0, 0, 0, 0);
        return Ok(Some(Value::Object(Some(bfa))));
    }

    // NOFOLLOW_LINKS. `java.nio.file.LinkOption` is a single-constant enum, so
    // a non-empty `LinkOption[]` unambiguously means NOFOLLOW_LINKS — no
    // re-entrant `toString()` probe needed. This used to be an unimplemented
    // comment: every read followed links, so `isSymbolicLink()` could never be
    // true and a link's own size/timestamps were never visible.
    let nofollow = matches!(args.get(1), Some(Value::Object(Some(a))) if ctx.array_length(*a) > 0);
    let meta_result = if path_str.is_empty() {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "empty path",
        ))
    } else if nofollow {
        std::fs::symlink_metadata(&path_str)
    } else {
        std::fs::metadata(&path_str)
    };

    let bfa = basic_file_attributes_alloc(ctx)?;

    match meta_result {
        Ok(meta) => {
            // Creation time
            let creation_millis = meta.created().ok().map(system_time_to_millis).unwrap_or(0);
            // Last access time
            let access_millis = meta.accessed().ok().map(system_time_to_millis).unwrap_or(0);
            // Last modified time
            let mod_millis = meta.modified().ok().map(system_time_to_millis).unwrap_or(0);
            #[cfg(unix)]
            let perm_bits = {
                use std::os::unix::fs::PermissionsExt;
                (meta.permissions().mode() & 0o7777) as i32
            };
            #[cfg(not(unix))]
            let perm_bits = 0i32;
            basic_file_attributes_store(
                ctx,
                bfa,
                meta.is_dir(),
                meta.len() as i64,
                creation_millis,
                access_millis,
                mod_millis,
                perm_bits,
            );
            // Windows: the real `sun.nio.fs.WindowsFileAttributes` fields are a
            // verbatim copy of the OS structures, and `std`'s `MetadataExt`
            // hands back those same raw words. Two things
            // `basic_file_attributes_store` cannot supply from
            // `(is_dir, millis, millis, millis)`:
            //
            //  * the DOS attribute word. Real `isReadOnly()` / `isHidden()` /
            //    `isArchive()` / `isSystem()` are single-bit tests on
            //    `fileAttrs`, so a carrier carrying only `FILE_ATTRIBUTE_
            //    DIRECTORY` answers `false` to all four for every file on disk.
            //  * sub-millisecond FILETIME precision, which the millis round
            //    trip truncates.
            //
            // Written by NAME (never by slot): the real layout is the JDK's,
            // and a slot index against it is heap corruption rather than a
            // wrong answer.
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if basic_file_attributes_is_windows(ctx, bfa) {
                    let attr_word = meta.file_attributes();
                    if attr_word != 0 {
                        ctx.set_field_by_name(bfa, "fileAttrs", Value::Int(attr_word as i32));
                    }
                    // A zero FILETIME means "the OS did not supply this one";
                    // leaving the store's converted value in place keeps that
                    // case reading as the Unix epoch rather than as 1601.
                    for (name, raw) in [
                        ("creationTime", meta.creation_time()),
                        ("lastAccessTime", meta.last_access_time()),
                        ("lastWriteTime", meta.last_write_time()),
                    ] {
                        if raw != 0 {
                            ctx.set_field_by_name(bfa, name, Value::Long(raw as i64));
                        }
                    }
                }
            }
            // Record link-ness so `isSymbolicLink()` can stop answering a
            // hardcoded `false`. Only reachable under NOFOLLOW_LINKS, since a
            // following read resolves the target (and the real JDK likewise
            // reports `isSymbolicLink() == false` there).
            if meta.file_type().is_symlink() {
                if basic_file_attributes_is_synthetic(ctx, bfa) {
                    // Synthetic stub has no named fields — patch the slot.
                    let cur = basic_file_attributes_syn_mode(ctx, bfa);
                    ctx.set_field(
                        bfa,
                        BFA_SYN_SLOT_MODE,
                        Value::Int((cur & !UNIX_S_IFMT) | UNIX_S_IFLNK),
                    );
                } else if basic_file_attributes_is_windows(ctx, bfa) {
                    let cur = match ctx.get_field_by_name(bfa, "fileAttrs") {
                        Value::Int(v) => v,
                        _ => 0,
                    };
                    ctx.set_field_by_name(
                        bfa,
                        "fileAttrs",
                        Value::Int(cur | WIN_ATTR_REPARSE_POINT),
                    );
                    // The attribute bit alone is not what the real class reads.
                    // `WindowsFileAttributes.isSymbolicLink()` is
                    // `reparseTag == IO_REPARSE_TAG_SYMLINK`, and `isOther()`
                    // is true for any reparse point that is NOT a link — so a
                    // carrier with the bit and a zero tag makes real bytecode
                    // call every symlink an "other" file.
                    ctx.set_field_by_name(
                        bfa,
                        "reparseTag",
                        Value::Int(WIN_IO_REPARSE_TAG_SYMLINK),
                    );
                } else {
                    let cur = match ctx.get_field_by_name(bfa, "st_mode") {
                        Value::Int(v) => v,
                        _ => 0,
                    };
                    ctx.set_field_by_name(
                        bfa,
                        "st_mode",
                        Value::Int((cur & !UNIX_S_IFMT) | UNIX_S_IFLNK),
                    );
                }
            }
            // `UnixFileAttributes.owner()`/`group()` (real JDK bytecode) read
            // these two fields and hand them to `UnixUserPrincipals.fromUid/
            // fromGid`. They were never populated, so every file reported
            // uid/gid 0 ("root") — wrong for any ownership check, and the
            // reason Spring Boot's `ApplicationTemp` ownership assertion had
            // nothing sensible to compare against. Only meaningful on Unix;
            // the Windows attribute object has no such fields (the setters
            // are by-name and are silently dropped when absent).
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                ctx.set_field_by_name(bfa, "st_uid", Value::Int(meta.uid() as i32));
                ctx.set_field_by_name(bfa, "st_gid", Value::Int(meta.gid() as i32));
                // `st_dev`/`st_ino` ARE the file's identity — both our
                // `fileKey()` native and the real JDK's own
                // `UnixFileAttributes.fileKey()` build a key out of them.
                // Never populated before, which is why `fileKey()` had nothing
                // to report and returned null for every file.
                ctx.set_field_by_name(bfa, "st_dev", Value::Long(meta.dev() as i64));
                ctx.set_field_by_name(bfa, "st_ino", Value::Long(meta.ino() as i64));
                // `--real-jdk`: the WHOLE `stat` record, exactly as the real
                // `UnixNativeDispatcher.stat0` fills it (the same function
                // serves both). The store above keeps only a dir/regular type
                // bit, millisecond times and `ctime = mtime`, which our own
                // accessors never looked past -- but the accessors are real
                // bytecode now (`RETIRED_SHADOW_SBR_FS_TRIPLES` retires
                // `UnixFileAttributes`), and they read the full `st_mode`
                // (`isOther` for a FIFO/device/socket), the nanosecond halves
                // (`FileTime` equality and `toString`), `st_ctime_*` and the
                // birth time. `--compatible` keeps the store's answer.
                if ctx.is_jdk_only() && !basic_file_attributes_is_synthetic(ctx, bfa) {
                    cratonvm_native_io::unix_attrs_fill(ctx, bfa, &meta);
                }
            }
            // Windows equivalent of the `st_dev`/`st_ino` identity above: the
            // `(volume serial, file index)` triple `WindowsFileAttributes`
            // carries. Unlike the Unix fields this needs a second handle open,
            // so restrict it to directories and links — `FileTreeWalker`'s
            // loop check (the reason `fileKey()` has to be real) only ever asks
            // about those, and a tree walk over a large source tree should not
            // pay an extra CreateFile per ordinary file.
            #[cfg(windows)]
            {
                if meta.is_dir() || meta.file_type().is_symlink() {
                    if let Some((volume, high, low)) = win_file_identity(&path_str) {
                        ctx.set_field_by_name(bfa, "volSerialNumber", Value::Int(volume as i32));
                        ctx.set_field_by_name(bfa, "fileIndexHigh", Value::Int(high as i32));
                        ctx.set_field_by_name(bfa, "fileIndexLow", Value::Int(low as i32));
                    }
                }
            }
        }
        // NIO contract: `Files.readAttributes` must raise `IOException`
        // (`NoSuchFileException` when the path is missing) — silently
        // answering with a fake all-zero `BasicFileAttributes` masked every
        // failure, including a missing file. Real bytecode
        // `Files.getLastModifiedTime`/`size`/`isDirectory` etc. are thin
        // wrappers over `readAttributes`, so this silent success also broke
        // `Files.getLastModifiedTime` never throwing for a missing file —
        // `FileSystemResource.lastModified()` catches `NoSuchFileException`
        // and translates it to `FileNotFoundException`
        // (ResourceTests#resourceCreateRelativeUnknown).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(p57_no_such_file(ctx, &path_str)?);
        }
        Err(e) => return Err(p57_io_error(&e)),
    }

    Ok(Some(Value::Object(Some(bfa))))
}

// =============================================================================
// java.nio.file — Files.exists/isDirectory, Path expansion
// =============================================================================

pub fn register_p61_files_path(r: &mut NativeMethodRegistry) {
            let files = "java/nio/file/Files";
    reg_files! { r.register(files,
        "exists",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Z",
        |ctx, args| {
            Ok(Some(Value::Int(i32::from(p57_files_exists_impl(
                ctx, args,
            )))))
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }
    reg_files! { r.register(files,
        "notExists",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Z",
        |ctx, args| {
            Ok(Some(Value::Int(i32::from(!p57_files_exists_impl(
                ctx, args,
            )))))
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }
    reg_files! { r.register(files,
        "isDirectory",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Z",
        |ctx, args| {
            Ok(Some(Value::Int(i32::from(p57_files_is_directory_impl(
                ctx, args,
            )))))
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }
    reg_files! { r.register(files,
        "isRegularFile",
        "(Ljava/nio/file/Path;[Ljava/nio/file/LinkOption;)Z",
        |ctx, args| {
            Ok(Some(Value::Int(i32::from(p57_files_is_regular_file_impl(
                ctx, args,
            )))))
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }
    reg_files! { r.register(files, "size", "(Ljava/nio/file/Path;)J", p59_files_size, cratonvm_native_api::NativeKind::Bridge) }
    reg_files! { r.register(files,
        "isReadable",
        "(Ljava/nio/file/Path;)Z",
        |ctx, args| {
            if let Some(Value::Object(Some(path_ref))) = args.first() {
                let path_str = match ctx.get_field(*path_ref, 0) {
                    Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                    _ => return Ok(Some(Value::Int(0))),
                };
                let readable = match vfs_classify(&path_str) {
                    Some(kind) => !matches!(kind, JarFsKind::Absent),
                    None => std::path::Path::new(&path_str).exists(),
                };
                Ok(Some(Value::Int(if readable { 1 } else { 0 })))
            } else {
                Ok(Some(Value::Int(0)))
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }
    reg_files! { r.register(files,
        "isWritable",
        "(Ljava/nio/file/Path;)Z",
        |ctx, args| {
            if let Some(Value::Object(Some(path_ref))) = args.first() {
                let path_str = match ctx.get_field(*path_ref, 0) {
                    Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                    _ => return Ok(Some(Value::Int(0))),
                };
                let writable = std::fs::metadata(&path_str)
                    .map(|m| !m.permissions().readonly())
                    .unwrap_or(false);
                Ok(Some(Value::Int(if writable { 1 } else { 0 })))
            } else {
                Ok(Some(Value::Int(0)))
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }

    // --- Path expansion ---
    //
    // NO `java/nio/file/Path` REGISTRATIONS BELONG HERE. This function used to
    // re-register six of them — `getFileName`, `getParent`, `toAbsolutePath`,
    // `resolve(String)`, `resolve(Path)`, `getNameCount` — all of which
    // `register_phase57_nio_file` already owns. `NativeMethodRegistry::register`
    // overwrites the slot in place, so the LAST registration wins, and phase 61
    // runs after phase 57 (`register_all_natives`): every one of these silently
    // took over from the phase-57 implementation wherever that path is used
    // (synthetic-JDK mode; the shipping real-JDK VM calls
    // `register_phase57_nio_file` directly from `vm_init.rs` and never gets
    // here, which is the only reason this went unnoticed).
    //
    // They were not equivalent. The `resolve` pair joined with
    // `std::path::Path::join` and wrote the result straight into the object
    // instead of going through `p57_alloc_path`, so they skipped the
    // normalize-at-construction step that `sun.nio.fs.UnixPath`/`WindowsPath`
    // perform — which is exactly the trailing-separator defect
    // `resourcestests-trailing-slash-path-normalization-FIXED-20260804.md`
    // was filed for, re-introduced one phase later. `getFileName` used
    // `jarfs_decode` (missing jrt) and dropped the root/null contract;
    // `getNameCount` and `getParent` had already been hand-synced to their
    // phase-57 twins by earlier sessions, which is a standing invitation to
    // drift.
    //
    // Deleting them is strictly better than keeping them identical: there is
    // one implementation, and phase order stops being load-bearing. See
    // `p61_registers_no_duplicate_path_natives` in `registry_contracts.rs`,
    // which fails if any of them come back.
    }

// URI/URL already fully implemented in earlier phases — no additions needed
pub fn register_p61_net(r: &mut NativeMethodRegistry) {
            // NetworkInterface = 5-field synthetic (name=0, displayName=1, addresses=2, index=3, flags=4)
    // flags: bit 0 = up, bit 1 = loopback, bit 2 = supportsMulticast
    let ni = "java/net/NetworkInterface";

    // NOTE: `getNetworkInterfaces` is intentionally NOT registered as a
    // synthetic native in real-JDK mode. See the long comment in
    // `register_re8_network_interface` (net_phase_e.rs) for details. The
    // real JDK Java method calls native `getAll()` (returns empty) which
    // produces a SocketException that callers handle. Returning synthetic
    // NetworkInterface objects here breaks downstream `getInetAddresses()`
    // because the real JDK reads field slots we don't populate.

    r.register(
        ni,
        "getByName",
        "(Ljava/lang/String;)Ljava/net/NetworkInterface;",
        |ctx, args| {
            let name_ref = obj_arg(args, 1)?;
            let name = ctx.read_string(name_ref).unwrap_or_default();
            let interfaces = p61_build_network_interfaces(ctx)?;
            for iface in &interfaces {
                if let Value::Object(Some(s)) = ctx.get_field(*iface, 0) {
                    if ctx.read_string(s).as_deref() == Some(&name) {
                        return Ok(Some(Value::Object(Some(*iface))));
                    }
                }
            }
            Ok(Some(Value::Object(None)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(
        ni,
        "getByInetAddress",
        "(Ljava/net/InetAddress;)Ljava/net/NetworkInterface;",
        |ctx, args| {
            let addr_ref = obj_arg(args, 1)?;
            let addr_str = match ctx.get_field(addr_ref, 1) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let interfaces = p61_build_network_interfaces(ctx)?;
            for iface in &interfaces {
                if let Value::Object(Some(addrs)) = ctx.get_field(*iface, 2) {
                    let len = ctx.array_length(addrs);
                    for i in 0..len {
                        if let Value::Object(Some(ia)) = ctx.get_array_element(addrs, i) {
                            if let Value::Object(Some(s)) = ctx.get_field(ia, 1) {
                                if ctx.read_string(s).as_deref() == Some(&addr_str) {
                                    return Ok(Some(Value::Object(Some(*iface))));
                                }
                            }
                        }
                    }
                }
            }
            Ok(Some(Value::Object(None)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(ni, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ni, "getDisplayName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        ni,
        "getInetAddresses",
        "()Ljava/util/Enumeration;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // gc-common w18-f: the receiver and the fresh enumeration are
            // rooted across the allocations, and the address array is read
            // from the receiver AFTER the enumeration's allocation. It used to
            // be read first and held raw across it, and so was the
            // enumeration across the empty array's allocation.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let this_h = scope.root(this);
            // Concrete `Enumeration$Impl`, not the bare `Enumeration` interface.
            let enum_obj =
                try_alloc_concurrent_synthetic(&mut *scope, "java/util/Enumeration$Impl", 5)?;
            let enum_h = scope.root(enum_obj);
            let this = scope.get(&this_h);
            let addrs = match scope.get_field(this, 2) {
                Value::Object(Some(a)) => a,
                _ => scope.new_array(cratonvm_types::ArrayElementType::Reference, 0),
            };
            let enum_obj = scope.get(&enum_h);
            scope.set_field(enum_obj, 0, Value::Object(Some(addrs)));
            scope.set_field(enum_obj, 1, Value::Int(0));
            Ok(Some(Value::Object(Some(enum_obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(ni, "getIndex", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 3)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ni, "isUp", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let flags = ctx.get_field(this, 4).as_int().unwrap_or(0);
        Ok(Some(Value::Int(if flags & 1 != 0 { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ni, "isLoopback", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let flags = ctx.get_field(this, 4).as_int().unwrap_or(0);
        Ok(Some(Value::Int(if flags & 2 != 0 { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ni, "supportsMulticast", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let flags = ctx.get_field(this, 4).as_int().unwrap_or(0);
        Ok(Some(Value::Int(if flags & 4 != 0 { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    // Was a hardcoded `false` ("IFF_POINTOPOINT has no portable query"). Linux
    // publishes the interface's raw IFF_* word as a plain file, the same
    // bitmask `SIOCGIFFLAGS` returns and the same one the JDK's
    // `NetworkInterface.isP2P()` tests — read it, exactly as the `getMTU`
    // registration below reads `/sys/class/net/<name>/mtu`. Falls back to
    // `false` where `/sys` is unavailable (Windows/macOS) or the synthesized
    // name has no real interface behind it, which is the same answer as
    // before but now only when we genuinely cannot tell.
    r.register(ni, "isPointToPoint", "()Z", |ctx, args| {
        const IFF_POINTOPOINT: u32 = 0x10;
        let this = obj_arg(args, 0)?;
        let name = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        if !name.is_empty() && !name.contains('/') && !name.contains('\\') {
            if let Ok(text) = std::fs::read_to_string(format!("/sys/class/net/{name}/flags")) {
                let raw = text.trim();
                let digits = raw.strip_prefix("0x").unwrap_or(raw);
                if let Ok(flags) = u32::from_str_radix(digits, 16) {
                    return Ok(Some(Value::Int(i32::from(flags & IFF_POINTOPOINT != 0))));
                }
            }
        }
        Ok(Some(Value::Int(0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    // Was a hardcoded `false`. The JDK defines a "virtual" interface as a
    // sub-interface, which is exactly the `eth0:1` naming convention — read it
    // off the name (slot 0) rather than asserting none exist.
    r.register(ni, "isVirtual", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let name = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        Ok(Some(Value::Int(i32::from(name.contains(':')))))    }, cratonvm_native_api::NativeKind::Bridge);
    // STUB-REMOVAL (wave 3). This used to hand back a FABRICATED all-zero MAC
    // (00:00:00:00:00:00) because "real MAC requires platform APIs". That is
    // strictly worse than admitting we don't know: 00:00:00:00:00:00 is a
    // syntactically valid address, so callers that derive a node ID from it
    // (UUID v1 generators, cluster member identity, licence fingerprints) get
    // a plausible-looking constant instead of a signal to fall back — and
    // every host in a cluster gets the SAME one.
    //
    // `null` is the spec'd answer: getHardwareAddress returns null when the
    // address does not exist or is not accessible. It is also what HotSpot 25
    // returns for the loopback interface, verified on Windows, and loopback is
    // the interface `p61_build_network_interfaces` always synthesizes.
    //
    // WAVE-4: an unconditional `null` is still an answer we do not have to
    // guess at on Linux, which publishes the real MAC as a plain file — the
    // same source `getMTU`/`isPointToPoint` read. Return the real address when
    // `/sys` has one, and keep `null` for the two cases the spec names: no
    // address (loopback publishes the all-zero one, which is exactly "does not
    // exist" and is what HotSpot reports as null) and not accessible.
    r.register(ni, "getHardwareAddress", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let name = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        if name.is_empty() || name.contains('/') || name.contains('\\') {
            return Ok(Some(Value::Object(None)));
        }
        let text = match std::fs::read_to_string(format!("/sys/class/net/{name}/address")) {
            Ok(t) => t,
            Err(_) => return Ok(Some(Value::Object(None))),
        };
        let mut bytes: Vec<u8> = Vec::new();
        for octet in text.trim().split(':') {
            match u8::from_str_radix(octet, 16) {
                Ok(b) => bytes.push(b),
                Err(_) => return Ok(Some(Value::Object(None))),
            }
        }
        // All-zero (loopback) means "no hardware address" -> null, per spec.
        if bytes.is_empty() || bytes.iter().all(|&b| b == 0) {
            return Ok(Some(Value::Object(None)));
        }
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
        for (i, &b) in bytes.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));        }
        Ok(Some(Value::Object(Some(arr))))
    }, cratonvm_native_api::NativeKind::Bridge);
    // Was a flat 1500 for every interface — wrong for the loopback interface on
    // every platform (Linux `lo` is 65536), which is the one interface
    // `p61_build_network_interfaces` ALWAYS synthesizes. Linux publishes the
    // real per-interface MTU as a plain file, the same number `SIOCGIFMTU`
    // returns, so read it; elsewhere fall back to the loopback/Ethernet
    // defaults rather than one constant for both.
    r.register(ni, "getMTU", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let name = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        if !name.is_empty() && !name.contains('/') && !name.contains('\\') {
            if let Ok(text) = std::fs::read_to_string(format!("/sys/class/net/{name}/mtu")) {
                if let Ok(mtu) = text.trim().parse::<i32>() {
                    return Ok(Some(Value::Int(mtu)));
                }
            }
        }
        // Fallback when /sys is unavailable. The loopback MTU is PLATFORM
        // SPECIFIC, not a universal 65536: Linux `lo` is 65536, but Windows
        // loopback reports 1500 and macOS lo0 reports 16384. Measured against
        // HotSpot 25 on Windows, which answers 1500 — so a flat 65536 here
        // would diverge from the reference JVM on that host, which the
        // previous flat 1500 happened to match.
        let loopback = ctx.get_field(this, 4).as_int().unwrap_or(0) & 2 != 0;
        let mtu = if !loopback {
            1500
        } else if cfg!(target_os = "linux") {
            65536
        } else if cfg!(target_os = "macos") {
            16384
        } else {
            1500
        };
        Ok(Some(Value::Int(mtu)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        ni,
        "getSubInterfaces",
        "()Ljava/util/Enumeration;",
        |ctx, _args| {
            // Concrete `Enumeration$Impl`, not the bare `Enumeration` interface.
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
            let enum_obj = crate::classloader::make_snapshot_enumeration(ctx, arr)?;
            Ok(Some(Value::Object(Some(enum_obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // Was an unconditional `null`, justified by "getSubInterfaces() never
    // yields any". That reasons about the wrong direction: `getParent()` is
    // asked of the SUB-interface, and this VM already recognises one — the
    // `isVirtual()` registration above defines a sub-interface exactly as the
    // JDK does, by the `eth0:1` naming convention. So a receiver named
    // `eth0:1` does have a parent (`eth0`), and answering null for it
    // contradicted `isVirtual()`. Resolve it the same way `getByName` does.
    r.register(
        ni,
        "getParent",
        "()Ljava/net/NetworkInterface;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name = match ctx.get_field(this, 0) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            // Not a sub-interface -> no parent (the documented JDK answer).
            let parent_name = match name.split_once(':') {
                Some((base, _)) if !base.is_empty() => base.to_string(),
                _ => return Ok(Some(Value::Object(None))),
            };
            let interfaces = p61_build_network_interfaces(ctx)?;
            for iface in &interfaces {
                if let Value::Object(Some(s)) = ctx.get_field(*iface, 0) {
                    if ctx.read_string(s).as_deref() == Some(&parent_name) {
                        return Ok(Some(Value::Object(Some(*iface))));
                    }
                }
            }
            Ok(Some(Value::Object(None)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(ni, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = obj_arg(args, 1)?;
        let n1 = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let n2 = match ctx.get_field(other, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        Ok(Some(Value::Int(if n1 == n2 { 1 } else { 0 })))    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ni, "hashCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let name = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let mut hash: i32 = 0;
        for b in name.bytes() {
            hash = hash.wrapping_mul(31).wrapping_add(b as i32);
        }
        Ok(Some(Value::Int(hash)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ni, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let name = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let s = ctx.create_string(&format!("name:{}", name));
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Bridge);
    }

/// Build the list of NetworkInterface synthetic objects for this host.
/// Returns loopback + primary interface (resolved via hostname DNS).
///
/// The returned references are CURRENT: every interface is rooted in a
/// [`cratonvm_native_api::NativeHandleScope`] across the allocations that
/// follow it and re-read on the way out. They are unrooted again on return, so
/// a caller must not allocate before it is done with them; the three callers
/// (`getByName`, `getByInetAddress`, `getParent`) only read fields and strings
/// and return one element or null.
///
/// gc-common w18-f: every `NetworkInterface`, name `String`, address array and
/// `InetAddress` used to be held raw across the next allocation, and so was the
/// loopback interface in the returned `Vec` across the whole primary-interface
/// build. That was already wrong under a moving collection; since gc-common
/// w17-a the `InetAddress` side table no longer roots its mirror either, so
/// nothing kept the address alive between its allocation and its store.
pub fn p61_build_network_interfaces(
    ctx: &mut dyn NativeContext,
) -> Result<Vec<ObjectRef>, MethodCallFailed> {
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);

    // 1. Loopback interface. Its InetAddress for 127.0.0.1 comes from
    //    `alloc_inet_address_external`, which records host/IP in the
    //    `net_phase_e` side table AND populates a real-JDK
    //    `InetAddress$InetAddressHolder` (instance slot 0 is the typed `holder`
    //    reference field, never a bare String). This is the object Hazelcast's
    //    `DefaultAddressPicker` enumerates via
    //    `NetworkInterface.getInetAddresses()` then calls `.getHostName()` on.
    let lo = p61_build_network_interface(
        &mut *scope,
        "lo",
        "Loopback Interface",
        "localhost",
        "127.0.0.1",
        1,
        0b111, // up + loopback + multicast
    )?;
    let lo_h = scope.root(lo);

    // 2. Primary interface — resolve hostname to get primary IP. Use the
    //    portable hostname/IP helpers so this code path agrees with what
    //    `InetAddress.getLocalHost` returns and works identically on Linux,
    //    macOS, and Windows. (NEW-2 / C7)
    let hostname = crate::resolve_real_hostname();
    let primary_ip = crate::resolve_primary_ipv4(&hostname);

    let eth = if primary_ip != "127.0.0.1" {
        Some(p61_build_network_interface(
            &mut *scope,
            "eth0",
            "Primary Network Interface",
            &hostname,
            &primary_ip,
            2,
            0b101, // up + multicast (not loopback)
        )?)
    } else {
        None
    };

    // Nothing allocates past this point: `lo` is re-read after `eth`'s build.
    let mut result = vec![scope.get(&lo_h)];
    result.extend(eth);
    Ok(result)
}

/// One synthetic `NetworkInterface` (name=0, displayName=1, addresses=2,
/// index=3, flags=4) with a single `InetAddress`, returned CURRENT. The
/// interface is rooted across every allocation after its own, and each fresh
/// object is stored into it (or into the rooted address array) before the next
/// allocation, so no reference is ever held raw across one (gc-common w18-f).
pub fn p61_build_network_interface(
    ctx: &mut dyn NativeContext,
    name: &str,
    display_name: &str,
    host: &str,
    ip: &str,
    index: i32,
    flags: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let iface = try_alloc_concurrent_synthetic(&mut *scope, "java/net/NetworkInterface", 5)?;
    let iface_h = scope.root(iface);
    let name_s = scope.create_string(name);
    let cur = scope.get(&iface_h);
    scope.set_field(cur, 0, Value::Object(Some(name_s)));
    let display_s = scope.create_string(display_name);
    let cur = scope.get(&iface_h);
    scope.set_field(cur, 1, Value::Object(Some(display_s)));
    let addr = crate::net_phase_e::alloc_inet_address_external(&mut *scope, host, ip)?;
    let addr_h = scope.root(addr);
    let addrs = scope.new_array(cratonvm_types::ArrayElementType::Reference, 1);
    let addr = scope.get(&addr_h);
    scope.set_array_element(addrs, 0, Value::Object(Some(addr)));
    let cur = scope.get(&iface_h);
    scope.set_field(cur, 2, Value::Object(Some(addrs)));
    scope.set_field(cur, 3, Value::Int(index));
    scope.set_field(cur, 4, Value::Int(flags));
    Ok(cur)
}

// =============================================================================
// java.nio.file.FileVisitor and SimpleFileVisitor stubs
// =============================================================================

pub fn register_p66_file_visitor(r: &mut NativeMethodRegistry) {
            let sfv = "java/nio/file/SimpleFileVisitor";
    // SimpleFileVisitor has default methods that return CONTINUE
    r.register(sfv, "preVisitDirectory", "(Ljava/lang/Object;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;", |ctx, _args| {
        let result = p57_alloc_enum(ctx, "java/nio/file/FileVisitResult", "CONTINUE", 0)?;
        Ok(result)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sfv, "visitFile", "(Ljava/lang/Object;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;", |ctx, _args| {
        let result = p57_alloc_enum(ctx, "java/nio/file/FileVisitResult", "CONTINUE", 0)?;
        Ok(result)
    }, cratonvm_native_api::NativeKind::Bridge);
    // THESE TWO RETHROW. `SimpleFileVisitor`'s javadoc is explicit and its
    // body is one line each:
    //
    //   visitFileFailed(T file, IOException exc)      { throw exc; }
    //   postVisitDirectory(T dir, IOException exc)    { if (exc != null) throw exc;
    //                                                    return CONTINUE; }
    //
    // Answering CONTINUE instead made `SimpleFileVisitor` — the class every
    // walk that does not care about errors uses, and the one the javadoc
    // recommends — SWALLOW EVERY I/O FAILURE IN THE TREE. MEASURED:
    // `Files.walkFileTree(<missing directory>, new SimpleFileVisitor<>(){})`
    // returned normally on this VM where HotSpot raises `NoSuchFileException`,
    // and so would an unreadable subdirectory in the middle of a real walk: the
    // caller sees a completed traversal that silently skipped part of the tree.
    //
    // This is the pair the `Files.walkFileTree` start-node repair depends on —
    // that fix calls `visitFileFailed` exactly as the JDK does, and with these
    // bodies the callback fired and threw nothing away.
    r.register(
        sfv,
        "visitFileFailed",
        "(Ljava/lang/Object;Ljava/io/IOException;)Ljava/nio/file/FileVisitResult;",
        // ARGUMENT INDEX. `args[0]` is the RECEIVER, so the `IOException` is
        // `args[2]`, not `args[1]`. The first version of this read `args[1]`
        // and therefore threw the PATH -- a `java/nio/file/Path` handed to the
        // interpreter as a Throwable, which surfaced as
        // `Exception in thread "main" java/nio/file/Path` with no message and
        // no stack frames, and killed the probe 79 rows before its end.
        |ctx, args| match args.get(2) {
            Some(Value::Object(Some(exc))) => Err(MethodCallFailed::ExceptionThrown(*exc)),
            _ => {
                let result = p57_alloc_enum(ctx, "java/nio/file/FileVisitResult", "CONTINUE", 0)?;
                Ok(result)
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sfv,
        "postVisitDirectory",
        "(Ljava/lang/Object;Ljava/io/IOException;)Ljava/nio/file/FileVisitResult;",
        // ARGUMENT INDEX. `args[0]` is the RECEIVER, so the `IOException` is
        // `args[2]`, not `args[1]`. The first version of this read `args[1]`
        // and therefore threw the PATH -- a `java/nio/file/Path` handed to the
        // interpreter as a Throwable, which surfaced as
        // `Exception in thread "main" java/nio/file/Path` with no message and
        // no stack frames, and killed the probe 79 rows before its end.
        |ctx, args| match args.get(2) {
            Some(Value::Object(Some(exc))) => Err(MethodCallFailed::ExceptionThrown(*exc)),
            _ => {
                let result = p57_alloc_enum(ctx, "java/nio/file/FileVisitResult", "CONTINUE", 0)?;
                Ok(result)
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // FileVisitResult enum
    let fvr = "java/nio/file/FileVisitResult";
    r.register(
        fvr,
        "values",
        "()[Ljava/nio/file/FileVisitResult;",
        |ctx, _args| {
            // `FileVisitResult[]`, the declared type (i7-L2), rooted across
            // the per-constant allocations below (it used to be held raw).
            let component =
                crate::lang_class::loading_component_id(ctx, "java/nio/file/FileVisitResult");
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let arr = scope.new_ref_array(component, 4);
            let arr_h = scope.root(arr);
            let names = ["CONTINUE", "TERMINATE", "SKIP_SUBTREE", "SKIP_SIBLINGS"];
            for (i, name) in names.iter().enumerate() {
                if let Ok(Some(e)) =
                    p57_alloc_enum(&mut *scope, "java/nio/file/FileVisitResult", name, i as i32)
                {
                    let arr = scope.get(&arr_h);
                    scope.set_array_element(arr, i, e);
                }
            }
            Ok(Some(Value::Object(Some(scope.get(&arr_h)))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        fvr,
        "valueOf",
        "(Ljava/lang/String;)Ljava/nio/file/FileVisitResult;",
        |ctx, args| {
            // AN UNKNOWN NAME IS A REFUSAL, and `CONTINUE` was the worst
            // possible thing to answer instead: `Enum.valueOf` raises
            // `IllegalArgumentException`, and a walk that asked for TERMINATE
            // through a misspelt or externally-supplied name kept walking. A
            // null name is an NPE, not `CONTINUE` either.
            //
            // MEASURED with `L4CensusTail.java`.
            // `PosixFilePermission.valueOf` — the same shape two files away —
            // already refuses, which is what made this one visible.
            let name = match args.first() {
                Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("Name is null".into()),
                    }
                    .into())
                }
            };
            let ordinal = match name.as_str() {
                "CONTINUE" => 0,
                "TERMINATE" => 1,
                "SKIP_SUBTREE" => 2,
                "SKIP_SIBLINGS" => 3,
                _ => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: format!("No enum constant java.nio.file.FileVisitResult.{name}"),
                    }
                    .into())
                }
            };
            p57_alloc_enum(ctx, "java/nio/file/FileVisitResult", &name, ordinal)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // Files.walkFileTree — real directory walking with visitor callbacks
    reg_files! { r.register("java/nio/file/Files",
        "walkFileTree",
        "(Ljava/nio/file/Path;Ljava/nio/file/FileVisitor;)Ljava/nio/file/Path;",
        // The 2-arg overload is specified as "does not follow symbolic links".
        |ctx, args| p98_walk_file_tree(ctx, args, usize::MAX, false), cratonvm_native_api::NativeKind::Bridge,
    ) }
    reg_files! { r.register("java/nio/file/Files",
        "walkFileTree",
        "(Ljava/nio/file/Path;Ljava/util/Set;ILjava/nio/file/FileVisitor;)Ljava/nio/file/Path;",
        |ctx, args| {
            // THE TWO ARGUMENT CHECKS THE 4-ARG OVERLOAD OWES, both of which it
            // used to skip. `Files.walk`/`Files.find` already refuse a negative
            // depth through `p57_max_depth_refusal` -- the helper was right and
            // simply had no caller here, so `walkFileTree(p, opts, -1, v)`
            // walked the whole tree where HotSpot raises
            // `IllegalArgumentException`. `.max(0)` is what silently turned the
            // refusal into an unbounded walk.
            //
            // A null `Set<FileVisitOption>` is an NPE in the JDK
            // (`Objects.requireNonNull(options)`), not "no options": answering
            // the latter means a caller that passed null by mistake gets a
            // successful walk and never learns.
            //
            // MEASURED with `L4TailSweep2.java`.
            if matches!(args.get(1), Some(Value::Object(None))) {
                return Err(RuntimeError::NullPointerException { message: None }.into());
            }
            if let Some(refused) = p57_max_depth_refusal(args.get(2)) {
                return Err(refused);
            }
            // The `Set<FileVisitOption>` probe calls `isEmpty()` bytecode,
            // which can allocate.
            //
            // gc-common w20-d: this used to read the root path and the
            // visitor out of `args` AFTER the probe, on the belief that
            // `args` is kept current; it is not (a collection does not
            // rewrite a native's argument slice), so a `walkFileTree(p,
            // EnumSet.of(FOLLOW_LINKS), n, v)` walked from, and called back
            // into, pre-collection addresses. Both are lifted and rooted
            // BEFORE the probe and read back after it.
            let path_obj = args.first().copied().unwrap_or(Value::Object(None));
            let visitor = args.get(3).copied().unwrap_or(Value::Object(None));
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let path_h = match path_obj {
                Value::Object(Some(o)) => Some(scope.root(o)),
                _ => None,
            };
            let visitor_h = match visitor {
                Value::Object(Some(o)) => Some(scope.root(o)),
                _ => None,
            };
            let follow = p57_visit_options_follow_links(&mut *scope, args.get(1));
            let path_obj = match &path_h {
                Some(h) => Value::Object(Some(scope.get(h))),
                None => path_obj,
            };
            let visitor = match &visitor_h {
                Some(h) => Value::Object(Some(scope.get(h))),
                None => visitor,
            };
            let requested_depth = args.get(2).and_then(Value::as_int).unwrap_or(i32::MAX);
            let max_depth = if requested_depth == i32::MAX {
                usize::MAX
            } else {
                requested_depth.max(0) as usize
            };
            p98_walk_file_tree(&mut *scope, &[path_obj, visitor], max_depth, follow)
        }, cratonvm_native_api::NativeKind::Bridge,
    ) }
    }

pub fn p98_walk_file_tree(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    max_depth: usize,
    follow_links: bool,
) -> MethodCallResult {
    let path_val = args.first().copied().unwrap_or(Value::Object(None));
    let visitor = if let Some(Value::Object(Some(v))) = args.get(1) {
        *v
    } else {
        return Ok(Some(path_val));
    };
    let path_obj = if let Value::Object(Some(p)) = path_val {
        p
    } else {
        return Ok(Some(path_val));
    };
    let root_str = match p57_path_slot_string(ctx, path_obj) {
        Some(t) => t,
        None => return Ok(Some(path_val)),
    };
    let visitor_class_id = ctx.class_id_of_object(visitor);
    let visitor_class_name = ctx.class_name_of_id(visitor_class_id);
    let skip_file_callbacks = visitor_class_name
        .as_deref()
        .is_some_and(|name| name == "com/sun/tools/javac/file/JavacFileManager$ArchiveContainer$1");
    if crate::nbflags().dbg_visitfile {
        eprintln!(
            "[p98-walkfiletree] visitor_class_id={:?} visitor_class_name={:?} skip_file_callbacks={}",
            visitor_class_id, visitor_class_name, skip_file_callbacks
        );
    }

    // GC-safety: the walk below drives a re-entrant, potentially deep and
    // long-running sequence of Java callbacks (preVisitDirectory/visitFile/
    // postVisitDirectory) for every directory and file under `root`. Any of
    // those calls can allocate and trigger GC (directly, or transitively --
    // e.g. `PathFileObject.forJarPath` inside a compiler's own visitor).
    // `visitor` is a single Rust-local `ObjectRef` that stays alive across
    // the ENTIRE recursive walk; under a moving collector a mid-walk
    // relocation leaves a bare local like this stale, and `invoke_virtual`'s
    // own `load_and_forward` cannot repair it once the old slot has been
    // reused for an unrelated (often array) allocation -- silently
    // redirecting dispatch to `java.lang.Object` and raising a spurious
    // `NoSuchMethodError` on the visitor's real method. Confirmed live: a
    // very large in-memory javac classpath walk (`BeanRegistrationsAot-
    // ContributionTests`, `JavacFileManager$ArchiveContainer.list`'s own
    // `SimpleFileVisitor`) reproduced exactly this signature --
    // `NoSuchMethodError: java/lang/Object.visitFile(...)`.
    //
    // Pin `visitor` and the root `path_obj` (used at both ends of the walk)
    // for the whole traversal via `p98_pin`/`p98_read_pin` (see their doc
    // comment below) and unpin the complete batch -- every pin taken
    // anywhere during the walk, since `visitor_pin` is the first one pushed
    // -- once it returns.
    let visitor_pin = p98_pin(ctx, visitor);
    let path_pin = p98_pin(ctx, path_obj);
    // A START NODE THAT IS NOT THERE. `Files.walkFileTree` reads the start
    // node's attributes first, and when that fails it calls
    // `visitor.visitFileFailed(start, ioe)` — which `SimpleFileVisitor`
    // implements by RETHROWING. MEASURED: HotSpot `NoSuchFileException`, this
    // VM returned the start path having visited nothing, so a caller walking a
    // directory that does not exist saw a successful empty walk.
    //
    // The callback is made rather than the exception thrown directly, because
    // a visitor is entitled to override `visitFileFailed` and continue — which
    // is the whole reason the JDK routes it through the visitor.
    let root_missing = match vfs_classify(&root_str) {
        Some(kind) => matches!(kind, JarFsKind::Absent),
        None => std::fs::symlink_metadata(&root_str).is_err(),
    };
    if root_missing {
        let exc = match p57_no_such_file(ctx, &root_str) {
            Ok(MethodCallFailed::ExceptionThrown(e)) => e,
            Ok(other) | Err(other) => {
                ctx.unpin_native_roots(visitor_pin.0);
                return Err(other);
            }
        };
        let visitor_now = p98_read_pin(ctx, visitor_pin);
        let path_now = p98_read_pin(ctx, path_pin);
        let outcome = p98_invoke_file_visitor(
            ctx,
            visitor_now,
            "visitFileFailed",
            "(Ljava/nio/file/Path;Ljava/io/IOException;)Ljava/nio/file/FileVisitResult;",
            "(Ljava/lang/Object;Ljava/io/IOException;)Ljava/nio/file/FileVisitResult;",
            path_now,
            Value::Object(Some(exc)),
        );
        ctx.unpin_native_roots(visitor_pin.0);
        outcome?;
        return Ok(Some(path_val));
    }
    // JDK 25's ArchiveContainer indexer only records package directories.
    // Serve that exact, already-indexed jar walk without an interpreter
    // callback for every directory; all other visitors use the generic path.
    if skip_file_callbacks
        && max_depth == usize::MAX
        && p98_index_javac_archive_packages(ctx, &root_str, visitor_pin, path_pin)?
    {
        ctx.unpin_native_roots(visitor_pin.0);
        return Ok(Some(path_val));
    }
    // A root that is not a directory (a plain file, or — without FOLLOW_LINKS —
    // a symbolic link, even one pointing at a directory) gets a single
    // `visitFile` callback, not the preVisitDirectory/postVisitDirectory pair.
    // Treating a symlink root as a directory is what made
    // `FileSystemUtils.deleteRecursively(symlink)` recurse into the link's
    // TARGET and then fail to unlink the link itself (`ApplicationTempTests`,
    // whose leftover symlink then poisoned every later test in the class).
    let result = if vfs_or_host_is_walkable_dir(&root_str, follow_links) {
        p98_walk_dir(
            ctx,
            &root_str,
            visitor_pin,
            path_pin,
            skip_file_callbacks,
            max_depth,
            follow_links,
        )
        .map(|_| ())
    } else {
        p98_visit_single_file(ctx, &root_str, visitor_pin, path_pin, skip_file_callbacks)
    };
    ctx.unpin_native_roots(visitor_pin.0);
    result?;
    Ok(Some(path_val))
}

/// `visitFile` for a walk root that is not a directory. `Files.walkFileTree`
/// accepts any path; the JDK reports a non-directory root through exactly one
/// `visitFile` callback and never touches the directory callbacks.
pub fn p98_visit_single_file(
    ctx: &mut dyn NativeContext,
    path: &str,
    visitor_pin: P98Pin,
    path_pin: P98Pin,
    skip_file_callbacks: bool,
) -> Result<(), MethodCallFailed> {
    if skip_file_callbacks {
        return Ok(());
    }
    // A symlink's own size, not its target's — the walk did not follow it.
    let size = std::fs::symlink_metadata(path)
        .map(|m| m.len() as i64)
        .unwrap_or(0);
    let fa = p98_alloc_basic_file_attributes(ctx, false, size);
    let visitor_now = p98_read_pin(ctx, visitor_pin);
    let path_now = p98_read_pin(ctx, path_pin);
    p98_invoke_file_visitor(
        ctx,
        visitor_now,
        "visitFile",
        "(Ljava/nio/file/Path;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;",
        "(Ljava/lang/Object;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;",
        path_now,
        Value::Object(Some(fa?)),
    )?;
    Ok(())
}

pub fn p98_is_missing_visitor_method(
    err: &MethodCallFailed,
    method_name: &str,
    descriptor: &str,
) -> bool {
    matches!(
        err,
        MethodCallFailed::InternalError(VmError::Linkage(LinkageError::NoSuchMethodError {
            method_name: missing_name,
            method_descriptor,
            ..
        })) if missing_name == method_name && method_descriptor == descriptor
    )
}

pub fn p98_invoke_file_visitor(
    ctx: &mut dyn NativeContext,
    visitor: ObjectRef,
    method_name: &str,
    concrete_descriptor: &str,
    erased_descriptor: &str,
    path_obj: ObjectRef,
    second_arg: Value,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    // gc-common w20-d: the erased-descriptor attempt resolves (and can load
    // classes) before it fails with `NoSuchMethodError`; the retry reused the
    // visitor, the path and the second argument at their addresses from
    // before it. Rooted, read back for the retry.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let visitor_h = scope.root(visitor);
    let path_h = scope.root(path_obj);
    let second_h = match second_arg {
        Value::Object(Some(o)) => Some(scope.root(o)),
        _ => None,
    };
    let args = [Value::Object(Some(path_obj)), second_arg];
    let result = scope.invoke_virtual(visitor, method_name, erased_descriptor, &args);
    let value = match result {
        Ok(value) => value,
        Err(err) if p98_is_missing_visitor_method(&err, method_name, erased_descriptor) => {
            let visitor = scope.get(&visitor_h);
            let second_arg = match &second_h {
                Some(h) => Value::Object(Some(scope.get(h))),
                None => second_arg,
            };
            let args = [Value::Object(Some(scope.get(&path_h))), second_arg];
            scope.invoke_virtual(visitor, method_name, concrete_descriptor, &args)?
        }
        Err(err) => return Err(err),
    };
    Ok(match value {
        Some(Value::Object(Some(result))) => Some(result),
        _ => None,
    })
}

/// A native-pinned GC root plus its original (possibly later stale) value,
/// kept as the fallback `read_native_pin` returns for `NativeContext`
/// implementations that don't support pinning (e.g. test mocks -- see
/// [`NativeContext::pin_native_root`]'s doc comment). Create one with
/// `p98_pin` right after allocating/receiving the object and resolve the
/// current, GC-forwarded reference with `p98_read_pin` immediately before
/// each re-entrant use -- never hold the raw `ObjectRef` itself across a
/// call that can allocate.
pub type P98Pin = (usize, ObjectRef);

pub fn p98_pin(ctx: &mut dyn NativeContext, obj: ObjectRef) -> P98Pin {
    (ctx.pin_native_root(obj), obj)
}

pub fn p98_read_pin(ctx: &dyn NativeContext, pin: P98Pin) -> ObjectRef {
    ctx.read_native_pin(pin.0, pin.1)
}

/// Build a concrete platform `BasicFileAttributes` implementation for a
/// `Files.walkFileTree` visitor callback. The named-field bridge makes the
/// representation independent from the platform class's physical field order.
pub fn p98_alloc_basic_file_attributes(
    ctx: &mut dyn NativeContext,
    is_dir: bool,
    size: i64,
) -> Result<ObjectRef, MethodCallFailed> {
    let attrs = basic_file_attributes_alloc(ctx)?;
    basic_file_attributes_store(ctx, attrs, is_dir, size, 0, 0, 0, 0);
    Ok(attrs)
}

/// Fast-path the only callback implemented by javac's archive indexer.
///
/// JDK 25's `JavacFileManager$ArchiveContainer$1` captures an
/// `ArchiveContainer` and its root path, then adds every directory with a
/// Java-identifier basename to `ArchiveContainer.packages`. The archive
/// filesystem already has the complete direct-child tree in [`JarFsIndex`],
/// so reproducing that narrow operation avoids a full interpreter round-trip
/// for every directory of every classpath jar. Return `false` for any shape
/// we do not recognize so generic visitor dispatch remains the fallback.
pub fn p98_index_javac_archive_packages(
    ctx: &mut dyn NativeContext,
    root: &str,
    visitor_pin: P98Pin,
    root_path_pin: P98Pin,
) -> Result<bool, MethodCallFailed> {
    let Some((jar, root_entry)) = jarfs_decode(root) else {
        return Ok(false);
    };
    let Some(index) = jar_index(&jar) else {
        return Ok(false);
    };

    // Keep Unicode package names on the generic path. The direct predicate
    // covers the common ASCII archive layout; falling back rather than
    // approximating Java's Unicode identifier rules preserves correctness for
    // unusual jars.
    if index.children.keys().any(|directory| !directory.is_ascii()) {
        return Ok(false);
    }

    let visitor = p98_read_pin(ctx, visitor_pin);
    let container = match ctx.get_field(visitor, 1) {
        Value::Object(Some(container))
            if ctx
                .class_name_arc_of_id(ctx.class_id_of_object(container))
                .as_deref()
                == Some("com/sun/tools/javac/file/JavacFileManager$ArchiveContainer") =>
        {
            container
        }
        _ => return Ok(false),
    };
    let packages = match ctx.get_field(container, 2) {
        Value::Object(Some(packages)) => packages,
        _ => return Ok(false),
    };
    let packages_pin = p98_pin(ctx, packages);
    let packages_base = packages_pin.0;

    // `(archive entry, path relative to the walk root)`. The visitor adds the
    // root itself under RelativeDirectory("") and prunes a non-identifier
    // directory (for example META-INF) together with its subtree.
    let root_entry = root_entry.trim_start_matches('/').trim_end_matches('/');
    let mut pending = vec![(root_entry.to_string(), String::new(), true)];
    while let Some((entry, relative, is_root)) = pending.pop() {
        if !is_root
            && !entry
                .rsplit('/')
                .next()
                .is_some_and(p98_is_ascii_java_identifier)
        {
            continue;
        }

        let path_pin = if is_root {
            root_path_pin
        } else {
            let encoded = jarfs_encode(&jar, &entry);
            let path = p57_alloc_path_raw(ctx, &encoded)?;
            p98_pin(ctx, path)
        };
        let relative_string = ctx.create_string(&relative);
        let relative_string_pin = p98_pin(ctx, relative_string);
        let relative_directory = match ctx.new_object_initialized(
            "com/sun/tools/javac/file/RelativePath$RelativeDirectory",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(p98_read_pin(ctx, relative_string_pin)))],
        )? {
            Some(Value::Object(Some(directory))) => directory,
            _ => {
                ctx.unpin_native_roots(packages_base);
                return Ok(false);
            }
        };
        let directory_pin = p98_pin(ctx, relative_directory);
        let packages_now = p98_read_pin(ctx, packages_pin);
        let directory_now = p98_read_pin(ctx, directory_pin);
        let path_now = p98_read_pin(ctx, path_pin);
        ctx.invoke_virtual(
            packages_now,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[
                Value::Object(Some(directory_now)),
                Value::Object(Some(path_now)),
            ],
        )?;

        if let Some(children) = index.children.get(&entry) {
            for (child, is_dir) in children.iter().rev() {
                if *is_dir {
                    let leaf = child.rsplit('/').next().unwrap_or_default();
                    let child_relative = if relative.is_empty() {
                        leaf.to_string()
                    } else {
                        format!("{relative}/{leaf}")
                    };
                    pending.push((child.clone(), child_relative, false));
                }
            }
        }
    }
    ctx.unpin_native_roots(packages_base);
    Ok(true)
}

pub fn p98_is_ascii_java_identifier(component: &str) -> bool {
    let mut chars = component.bytes();
    let Some(first) = chars.next() else {
        return false;
    };
    matches!(first, b'A'..=b'Z' | b'a'..=b'z' | b'_' | b'$')
        && chars.all(|byte| matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'$'))
}

pub fn p98_walk_dir(
    ctx: &mut dyn NativeContext,
    dir: &str,
    visitor_pin: P98Pin,
    dir_path_pin: P98Pin,
    skip_file_callbacks: bool,
    remaining_depth: usize,
    follow_links: bool,
) -> Result<bool, MethodCallFailed> {
    // AT THE DEPTH LIMIT A DIRECTORY IS REPORTED AS A FILE. `FileTreeWalker`
    // only opens a directory when it is allowed to descend; an entry sitting AT
    // `maxDepth` is handed to `visitFile` with its real attributes
    // (`attrs.isDirectory()` true) and the `preVisitDirectory`/
    // `postVisitDirectory` pair is never called for it.
    //
    //   walkFileTree(d1, {}, 1, v)   with d1/d2 a directory
    //     HotSpot    pre:d1, file:d1/d2
    //     this VM    pre:d1, pre:d1/d2      <- and no post, since we did not
    //                                          descend either
    //
    // The old code called `preVisitDirectory` unconditionally and then checked
    // the depth before recursing, which is one callback too late. A visitor
    // that counts directories -- the usual reason to bound a walk -- saw the
    // leaf twice in the wrong role, and one that only overrides `visitFile`
    // (the common `SimpleFileVisitor` shape) never saw the leaf at all.
    //
    // This also fixes `maxDepth == 0`, where the ROOT itself is the entry at
    // the limit: the JDK reports the root through a single `visitFile`.
    //
    // MEASURED with `L4TailSweep2.java`.
    if remaining_depth == 0 {
        if skip_file_callbacks {
            return Ok(true);
        }
        let size = std::fs::symlink_metadata(dir)
            .map(|m| m.len() as i64)
            .unwrap_or(0);
        let fa = p98_alloc_basic_file_attributes(ctx, true, size);
        let visitor_now = p98_read_pin(ctx, visitor_pin);
        let dir_path_now = p98_read_pin(ctx, dir_path_pin);
        let vr = p98_invoke_file_visitor(
            ctx,
            visitor_now,
            "visitFile",
            "(Ljava/nio/file/Path;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;",
            "(Ljava/lang/Object;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;",
            dir_path_now,
            Value::Object(Some(fa?)),
        )?;
        if let Some(r) = vr {
            // TERMINATE stops the whole walk; SKIP_SUBTREE has nothing left to
            // skip at a leaf, so it continues like CONTINUE.
            if ctx.get_field(r, 1).as_int().unwrap_or(0) == 1 {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    let attrs = p98_alloc_basic_file_attributes(ctx, true, 0);
    // preVisitDirectory
    let visitor_now = p98_read_pin(ctx, visitor_pin);
    let dir_path_now = p98_read_pin(ctx, dir_path_pin);
    let pre = p98_invoke_file_visitor(
        ctx,
        visitor_now,
        "preVisitDirectory",
        "(Ljava/nio/file/Path;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;",
        "(Ljava/lang/Object;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;",
        dir_path_now,
        Value::Object(Some(attrs?)),
    )?;
    if let Some(r) = pre {
        let ord = ctx.get_field(r, 1).as_int().unwrap_or(0);
        if ord == 1 {
            return Ok(false);
        } // TERMINATE
        if ord == 2 {
            return Ok(true);
        } // SKIP_SUBTREE
    }
    if remaining_depth > 0 {
        if let Some((jar, entry)) = jarfs_decode(dir) {
            // jar-FS directory — children come from the archive listing, not the
            // host filesystem (std::fs::read_dir on the encoded sentinel string
            // would ENOENT and silently visit nothing, so e.g. JUnit5's
            // ClasspathScanner would "discover" an empty jar).
            // The ArchiveContainer indexer only visits directories.  Do not build
            // transient Java paths for every ignored archive file in that mode.
            for (child, is_dir) in jarfs_list_dir_classified(&jar, &entry) {
                let es = jarfs_encode(&jar, &child);
                if is_dir {
                    let epo = p57_alloc_path_raw(ctx, &es)?;
                    let epo_pin = p98_pin(ctx, epo);
                    if !p98_walk_dir(
                        ctx,
                        &es,
                        visitor_pin,
                        epo_pin,
                        skip_file_callbacks,
                        remaining_depth - 1,
                        follow_links,
                    )? {
                        return Ok(false);
                    }
                } else if !skip_file_callbacks {
                    let epo = p57_alloc_path_raw(ctx, &es)?;
                    let epo_pin = p98_pin(ctx, epo);
                    let fa = p98_alloc_basic_file_attributes(
                        ctx,
                        false,
                        jarfs_entry_size(&jar, &child).unwrap_or(0),
                    );
                    let visitor_now = p98_read_pin(ctx, visitor_pin);
                    let epo_now = p98_read_pin(ctx, epo_pin);
                    let vr = p98_invoke_file_visitor(
                    ctx,
                    visitor_now,
                    "visitFile",
                    "(Ljava/nio/file/Path;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;",
                    "(Ljava/lang/Object;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;",
                    epo_now,
                    Value::Object(Some(fa?)),
                )?;
                    if let Some(r) = vr {
                        if ctx.get_field(r, 1).as_int().unwrap_or(0) == 1 {
                            return Ok(false);
                        }
                    }
                }
            }
        } else if let Some((java_home, entry)) = jrtfs_decode(dir) {
            // jrt-FS directory — children come from the jimage listing.
            for (child, is_dir) in jrtfs_list_dir_classified(&java_home, &entry) {
                let es = jrtfs_encode(&java_home, &child);
                if is_dir {
                    let epo = p57_alloc_path_raw(ctx, &es)?;
                    let epo_pin = p98_pin(ctx, epo);
                    if !p98_walk_dir(
                        ctx,
                        &es,
                        visitor_pin,
                        epo_pin,
                        skip_file_callbacks,
                        remaining_depth - 1,
                        follow_links,
                    )? {
                        return Ok(false);
                    }
                } else if !skip_file_callbacks {
                    let epo = p57_alloc_path_raw(ctx, &es)?;
                    let epo_pin = p98_pin(ctx, epo);
                    let fa = p98_alloc_basic_file_attributes(
                        ctx,
                        false,
                        jrtfs_entry_size(&java_home, &child).unwrap_or(0),
                    );
                    let visitor_now = p98_read_pin(ctx, visitor_pin);
                    let epo_now = p98_read_pin(ctx, epo_pin);
                    let vr = p98_invoke_file_visitor(
                    ctx,
                    visitor_now,
                    "visitFile",
                    "(Ljava/nio/file/Path;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;",
                    "(Ljava/lang/Object;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;",
                    epo_now,
                    Value::Object(Some(fa?)),
                )?;
                    if let Some(r) = vr {
                        if ctx.get_field(r, 1).as_int().unwrap_or(0) == 1 {
                            return Ok(false);
                        }
                    }
                }
            }
        } else if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let ep = entry.path();
                let es = ep.to_string_lossy().to_string();
                // `entry.file_type()` is the NOFOLLOW answer (it comes straight
                // off the readdir record), so a symbolic link is a link here
                // whatever it points at. `Path::is_dir()` resolves it — which is
                // exactly the "descend unless FOLLOW_LINKS" decision the walk
                // has to make, so only ask it when following.
                let is_dir = entry
                    .file_type()
                    .map(|t| t.is_dir() || (follow_links && t.is_symlink() && ep.is_dir()))
                    .unwrap_or(false);
                // HOST path (not a virtual-FS sentinel): build it through
                // `p57_alloc_path` so the stored string is the `/`-canonical
                // internal form every other Path native expects. Setting field 0
                // directly left `entry.path()`'s host separators in place, so on
                // Windows a Path handed to a FileVisitor compared unequal to the
                // otherwise-identical Path the caller built, and `startsWith`/
                // `relativize`/`getNameCount` against it all disagreed.
                if is_dir {
                    let epo = p57_alloc_path(ctx, &es);
                    let epo_pin = p98_pin(ctx, epo?);
                    if !p98_walk_dir(
                        ctx,
                        &es,
                        visitor_pin,
                        epo_pin,
                        skip_file_callbacks,
                        remaining_depth - 1,
                        follow_links,
                    )? {
                        return Ok(false);
                    }
                } else if !skip_file_callbacks {
                    let epo = p57_alloc_path(ctx, &es);
                    let epo_pin = p98_pin(ctx, epo?);
                    let fa = p98_alloc_basic_file_attributes(
                        ctx,
                        false,
                        entry
                            .metadata()
                            .map(|metadata| metadata.len() as i64)
                            .unwrap_or(0),
                    );
                    let visitor_now = p98_read_pin(ctx, visitor_pin);
                    let epo_now = p98_read_pin(ctx, epo_pin);
                    let vr = p98_invoke_file_visitor(
                    ctx,
                    visitor_now,
                    "visitFile",
                    "(Ljava/nio/file/Path;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;",
                    "(Ljava/lang/Object;Ljava/nio/file/attribute/BasicFileAttributes;)Ljava/nio/file/FileVisitResult;",
                    epo_now,
                    Value::Object(Some(fa?)),
                )?;
                    if let Some(r) = vr {
                        if ctx.get_field(r, 1).as_int().unwrap_or(0) == 1 {
                            return Ok(false);
                        }
                    }
                }
            }
        }
    }
    let visitor_now = p98_read_pin(ctx, visitor_pin);
    let dir_path_now = p98_read_pin(ctx, dir_path_pin);
    let post = p98_invoke_file_visitor(
        ctx,
        visitor_now,
        "postVisitDirectory",
        "(Ljava/nio/file/Path;Ljava/io/IOException;)Ljava/nio/file/FileVisitResult;",
        "(Ljava/lang/Object;Ljava/io/IOException;)Ljava/nio/file/FileVisitResult;",
        dir_path_now,
        Value::Object(None),
    )?;
    if let Some(r) = post {
        if ctx.get_field(r, 1).as_int().unwrap_or(0) == 1 {
            return Ok(false);
        }
    }
    Ok(true)
}

// =============================================================================
// java.nio.file.WatchService stubs
// =============================================================================

pub fn register_p66_watch_service(r: &mut NativeMethodRegistry) {
        
    // NOTE — this function no longer registers `WatchService.close/poll/take`,
    // `Path.register`, or the `WatchKey` methods.
    //
    // It used to carry a second, competing WatchService implementation built
    // on a 2-field layout (dir_path=0, last_scan_time=1) that answered `poll`
    // by re-`stat`ing the directory and comparing mtimes. The owner of the
    // WatchService surface is `cratonvm-native-io`'s `register_watch_service`,
    // which is backed by a real platform watcher (`notify` → inotify /
    // ReadDirectoryChangesW / FSEvents), reports the actual changed entry
    // through `WatchEvent.context()`, and uses an incompatible 3-field
    // WatchService / 5-field WatchKey layout.
    //
    // Two implementations of the same triples resolved by last-write-wins, so
    // which one ran depended purely on registration order between arms — and
    // the two layouts are not interchangeable, so the losing side's objects
    // are garbage to the winning side's natives. That is exactly how the
    // Spring Boot `FileWatcher` failure arose (a placeholder `newWatchService`
    // displacing the real one).
    //
    // AS OF 2026-08-13 (lane F9) THIS REGISTRAR REGISTERS NOTHING AT ALL, and
    // the four `StandardWatchEventKinds` rows it used to hold are DELETED —
    // E36-1 table row 5's verdict, unblocked by E40-1 §1a deleting the one
    // caller. What was here, and why it is gone rather than converted:
    //
    //   * `ENTRY_CREATE`, `ENTRY_MODIFY`, `ENTRY_DELETE`, `OVERFLOW`, each
    //     registered with `Ljava/nio/file/WatchEvent$Kind;` in the DESCRIPTOR
    //     slot — a field name in the method slot, not a method descriptor.
    //   * No `getstatic` path in this VM consults the native registry. There
    //     are three of them (`interpreter/opcodes.rs Instruction::Getstatic`,
    //     `jit/helpers.rs jit_getstatic`, `ir_lower.rs
    //     emit_inline_getstatic` — the last bakes the statics base as an
    //     immediate and emits two `mov`s with no call at all), and
    //     `StandardWatchEventKinds.ENTRY_CREATE` compiles to
    //     `getstatic ...ENTRY_CREATE:Ljava/nio/file/WatchEvent$Kind;`
    //     (measured on HotSpot 25.0.3+9), so it was a real read no row here
    //     could ever answer.
    //   * Each body returned `ctx.create_string("…")` — a `java/lang/String`
    //     where its own descriptor names a `WatchEvent$Kind`. On the oracle
    //     the constant's class is
    //     `java.nio.file.StandardWatchEventKinds$StdWatchEventKind`, `name()`
    //     is `"ENTRY_CREATE"` and `type()` is `interface java.nio.file.Path`;
    //     a bare String has none of that. `vm/src/vm/tests.rs`'s
    //     `watch_event_kinds_p66` asserted `read_java_string(...) ==
    //     "ENTRY_CREATE"`, i.e. it pinned the wrong TYPE as correct. That test
    //     is deleted (E40-1 §1a) and this deletion is its pair.
    //
    // The behaviour DOES have an owner and now has coverage: `native-io`'s
    // `watch_event_kind_bit` / `watch_event_kind_object` read the class's
    // STATIC (`static_field_index_by_name` + `get_static_field`) and fall back
    // to a synthetic one-field `WatchEvent$Kind` carrying the bit. Do not
    // re-add rows here: a `<clinit>` conversion belongs in the crate that owns
    // the WatchService surface and its layout.
    //
    // The registrar itself is kept (it is called from `phases_late.rs`) so the
    // history above stays attached to the name a future author will search
    // for. If it is ever deleted, move this comment, not just the call.

    }

// =============================================================================
// java.nio.file.attribute extensions — PosixFilePermission, FileTime, UserPrincipal
// =============================================================================

/// Synthetic class backing `PosixFilePermissions.asFileAttribute`'s result.
/// Field 0 = the attribute name (`"posix:permissions"`), field 1 = the
/// `Set<PosixFilePermission>` value — the same two things the interface's
/// `name()`/`value()` return.
pub const FILE_ATTRIBUTE_CLASS: &str = "java/nio/file/attribute/FileAttribute";

/// Unix mode requested by a `[Ljava/nio/file/attribute/FileAttribute;` varargs
/// argument, i.e. by `PosixFilePermissions.asFileAttribute(...)`.
///
/// The real JDK applies this at creation time (the `mkdir(2)`/`open(2)` mode
/// argument); every `Files.create*` native here used to ignore the array
/// outright, so the created file/directory got the process umask instead of
/// the caller's requested mode. Returns `None` when the array is absent,
/// empty, or carries no `posix:permissions` attribute — callers then keep
/// their previous default behaviour.
pub fn posix_mode_from_file_attributes(
    ctx: &mut dyn NativeContext,
    arg: Option<&Value>,
) -> Option<u32> {
    let arr = match arg {
        Some(Value::Object(Some(a))) => *a,
        _ => return None,
    };
    // gc-common w20-d: `name()` and `value()` are Java. The attribute was
    // asked `value()` through its address from before `name()`, and the next
    // iteration read the array through its address from before both. Rooted
    // (one root per attribute: a `FileAttribute<?>...` varargs is a handful
    // at most); each is read back after every call.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let arr_h = scope.root(arr);
    let len = scope.array_length(arr);
    for i in 0..len {
        let arr = scope.get(&arr_h);
        let elem = match scope.get_array_element(arr, i) {
            Value::Object(Some(e)) => e,
            _ => continue,
        };
        let elem_h = scope.root(elem);
        let ctx = &mut *scope;
        // Synthetic `asFileAttribute` result: read the two fields directly.
        // A real JDK `PosixFilePermissions$1` has no such layout, so fall
        // back to the interface methods for it — and check the object's width
        // first, since probing slots it does not have is an out-of-bounds read
        // the heap guard reports.
        let two_field_carrier = ctx.object_num_fields(elem) > 1;
        let mut name = match two_field_carrier.then(|| ctx.get_field(elem, 0)) {
            Some(Value::Object(Some(s))) => ctx.read_string(s),
            _ => None,
        };
        let mut value = if two_field_carrier {
            ctx.get_field(elem, 1)
        } else {
            Value::Object(None)
        };
        if name.as_deref() != Some("posix:permissions") {
            name = match ctx.invoke_virtual(elem, "name", "()Ljava/lang/String;", &[]) {
                Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
                _ => None,
            };
            if name.as_deref() != Some("posix:permissions") {
                continue;
            }
            let elem = scope.get(&elem_h);
            value = match scope.invoke_virtual(elem, "value", "()Ljava/lang/Object;", &[]) {
                Ok(Some(v)) => v,
                _ => continue,
            };
        }
        if let Value::Object(Some(set)) = value {
            return Some(posix_permission_bits_from_set(&mut *scope, set));
        }
    }
    None
}

/// `std::fs::create_dir` with an explicit Unix mode when the caller asked for
/// one (atomic — the mode is passed to `mkdir(2)`, so the directory is never
/// briefly visible with wider permissions).
pub fn create_dir_with_mode(p: &str, mode: Option<u32>) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut b = std::fs::DirBuilder::new();
        if let Some(m) = mode {
            b.mode(m);
        }
        return b.create(p);
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        std::fs::create_dir(p)
    }
}

/// Best-effort `chmod` for paths that were already created (the recursive
/// `create_dir_all` and temp-file paths). No-op off Unix and when no mode was
/// requested.
pub fn apply_unix_mode(p: &str, mode: Option<u32>) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Some(m) = mode {
            let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(m));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (p, mode);
    }
}

/// The one spelling of the class name, so the `<clinit>`, `values()`,
/// `valueOf` and the mode-bit walker cannot drift apart.
pub const POSIX_FILE_PERMISSION: &str = "java/nio/file/attribute/PosixFilePermission";

/// `java.nio.file.attribute.PosixFilePermission`'s constants **in declaration
/// order**.
///
/// The index into this slice IS the ordinal, so the order is load-bearing:
/// `Enum.compareTo` is `this.ordinal - other.ordinal`, `EnumMap`/`EnumSet` key
/// on it, `values()` must hand it back in the same order, and
/// `posix_permission_bits_from_set`'s `BITS` table is positionally paired with
/// it. Measured on the oracle, not assumed —
/// `javap -p java.nio.file.attribute.PosixFilePermission` on this host
/// (Microsoft build 25.0.3+9-LTS) declares them in exactly this order and a
/// run reports `values()[8].ordinal() == 8`. Note it is neither alphabetical
/// nor sorted by mode bit; a plausible-looking reordering silently changes
/// `compareTo`, `EnumSet` iteration and every permission mask this file
/// computes.
pub const POSIX_FILE_PERMISSION_CONSTANTS: &[&str] = &[
    "OWNER_READ",
    "OWNER_WRITE",
    "OWNER_EXECUTE",
    "GROUP_READ",
    "GROUP_WRITE",
    "GROUP_EXECUTE",
    "OTHERS_READ",
    "OTHERS_WRITE",
    "OTHERS_EXECUTE",
];

/// Convert a `Set<PosixFilePermission>` (the 9 canonical singleton constants
/// from `posix_file_permission_stub_clinit`/the real enum) into a Unix
/// permission-bits mode (e.g. for `std::fs::Permissions::from_mode`). Walks
/// the same 9 constants `PosixFilePermissions.toString`/`fromString` use,
/// via `Set.contains` (no set-internals assumption — works for any real
/// `Set` implementation the caller passes in, not just our synthetic
/// `HashSet`).
pub fn posix_permission_bits_from_set(ctx: &mut dyn NativeContext, set: ObjectRef) -> u32 {
    // Positionally paired with `POSIX_FILE_PERMISSION_CONSTANTS`: index i is
    // the mode bit of constant i. The two used to be a private 9-element
    // `NAMES` here plus a second private `NAMES` inside the `<clinit>`, which
    // is two chances for the declaration order to drift.
    const BITS: [u32; 9] = [
        0o400, 0o200, 0o100, 0o040, 0o020, 0o010, 0o004, 0o002, 0o001,
    ];
    let pfp = POSIX_FILE_PERMISSION;
    let _ = ctx.ensure_class_initialized(pfp);
    let cid = ctx.class_id_by_name(pfp);
    let mut mode = 0u32;
    // GC-safety: `Set.contains` is a virtual dispatch into real bytecode
    // (`hashCode`/`equals` on the caller's own set), run once per permission
    // constant. `set` is a bare Rust parameter, stale from the second turn on.
    let set_pin = ctx.pin_native_root(set);
    for i in 0..BITS.len().min(POSIX_FILE_PERMISSION_CONSTANTS.len()) {
        let set = ctx.read_native_pin(set_pin, set);
        let Some(c) = cid else { break };
        let Some(slot) = ctx.static_field_index_by_name(c, POSIX_FILE_PERMISSION_CONSTANTS[i])
        else {
            continue;
        };
        let constant = ctx.get_static_field(c, slot);
        if matches!(
            ctx.invoke_virtual(set, "contains", "(Ljava/lang/Object;)Z", &[constant]),
            Ok(Some(Value::Int(1)))
        ) {
            mode |= BITS[i];
        }
    }
    mode
}

/// `java.nio.file.attribute.PosixFilePermission.<clinit>` — the guarded entry
/// point. The body is [`posix_publish_constants`].
///
/// The split is deliberate: `is_class_synthetic_stub` answers the trait
/// default `false` under `MockNativeContext`, so a test driven through this
/// function measures the guard and never reaches a single field write. With
/// the body factored out, the part that can be wrong is the part that is
/// tested.
pub fn posix_file_permission_stub_clinit(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    if !ctx.is_class_synthetic_stub(POSIX_FILE_PERMISSION) {
        return Ok(None);
    }
    posix_publish_constants(ctx)
}

/// Mint and publish `PosixFilePermission`'s nine constants the way its real
/// `<clinit>` does, then publish `$VALUES`.
///
/// Three obligations, each silent when dropped:
///
/// 1. **`name` and `ordinal` must be written, in that order.** This function
///    used to resolve both against `P` — the RECEIVER class — with the
///    fallbacks INVERTED (`ordinal → 0`, `name → 1`). It was latent only
///    because `class_manager.rs` gives the stub `java/lang/Enum` as its
///    superclass so the lookup succeeds; one missing superclass row, or one
///    enum copied from this model without that row, and every constant is
///    NAMELESS — non-null, `valueOf` matching nothing. Both slots now come
///    from [`enum_name_ordinal_slots`], which resolves against
///    `java/lang/Enum` and cannot be got the wrong way round at one site
///    without being wrong at all of them.
/// 2. **Declaration order is the ordinal.** Measured, not assumed:
///    `javap -p java.nio.file.attribute.PosixFilePermission` on this host
///    (Microsoft build 25.0.3+9-LTS) declares OWNER_{READ,WRITE,EXECUTE},
///    GROUP_{…}, OTHERS_{…}, and a run reports `values()[8].ordinal() == 8`.
///    `compareTo`, `EnumMap` and `EnumSet` all key on it.
/// 3. **`$VALUES` is re-READ out of the statics**, not filled from the refs
///    minted in pass one: `new_ref_array` allocates and can move them. That
///    re-read is also what makes `values()[i] == CONSTANT`, the identity
///    `Enum.valueOf`, `Class.getEnumConstants` and `EnumSet` rely on.
///
/// KNOWN GAP, unfixed here: `class_manager.rs` declares the nine constants for
/// this stub but NOT `$VALUES`, and `set_static_field_by_name` resolves a
/// DECLARED static and is a silent no-op otherwise. So the `$VALUES` publish
/// below currently goes nowhere on the synthetic side and
/// `PosixFilePermission.values()` falls back to reading the nine statics one
/// by one (which works). The declaration is nominated; the publish is written
/// now so it starts working the moment that lands, and `values()` does not
/// depend on it either way.
pub fn posix_publish_constants(ctx: &mut dyn NativeContext) -> MethodCallResult {
    let p = POSIX_FILE_PERMISSION;
    let Some(cid) = ctx.class_id_by_name(p) else {
        return Ok(None);
    };
    // Idempotence: `<clinit>` runs once per class by construction, but a
    // second entry through any path must not replace live constants with
    // fresh objects that fail `==`.
    if let Some(slot) = ctx.static_field_index_by_name(cid, POSIX_FILE_PERMISSION_CONSTANTS[0]) {
        if matches!(ctx.get_static_field(cid, slot), Value::Object(Some(_))) {
            return Ok(None);
        }
    }
    let (name_idx, ord_idx) = enum_name_ordinal_slots(ctx);
    let nfields = ctx.class_num_total_fields(cid).max(2);
    for (ord, &name) in POSIX_FILE_PERMISSION_CONSTANTS.iter().enumerate() {
        let obj = ctx.alloc_object(cid, nfields);
        // Pin across `create_string` — a moving young GC there would relocate
        // the fresh constant while this frame still holds `obj` (the native
        // stale-local family). Nothing allocates between the field writes and
        // the publish to the static, which is a GC root, so the constant is
        // never unreachable-but-live.
        let obj_pin = ctx.pin_native_root(obj);
        let name_obj = ctx.create_string(name);
        let obj = ctx.read_native_pin(obj_pin, obj);
        ctx.unpin_native_roots(obj_pin);
        ctx.set_field(obj, name_idx, Value::Object(Some(name_obj)));
        // Cast: nine constants is far inside `i32`.
        ctx.set_field(obj, ord_idx, Value::Int(ord as i32));
        ctx.set_static_field_by_name(p, name, Value::Object(Some(obj)));
    }
    let values_array = ctx.new_ref_array(cid, POSIX_FILE_PERMISSION_CONSTANTS.len());
    for (idx, &name) in POSIX_FILE_PERMISSION_CONSTANTS.iter().enumerate() {
        let published = match ctx.static_field_index_by_name(cid, name) {
            Some(slot) => ctx.get_static_field(cid, slot),
            None => Value::Object(None),
        };
        // `set_array_element` does not allocate, so `values_array` cannot move
        // underneath this loop.
        ctx.set_array_element(values_array, idx, published);
    }
    ctx.set_static_field_by_name(p, "$VALUES", Value::Object(Some(values_array)));
    // `stack_walker.rs`'s model publishes both spellings and `vm_util.rs`'s
    // post-clinit fixup falls back from one to the other; a write to an
    // undeclared static is a no-op, so writing both costs nothing.
    ctx.set_static_field_by_name(p, "ENUM$VALUES", Value::Object(Some(values_array)));
    Ok(None)
}

/// `PosixFilePermission.values()` — a FRESH array each call, holding the
/// interned constants.
///
/// Freshness is measured, not stylistic: on the oracle
/// `PosixFilePermission.values() != PosixFilePermission.values()` (the real
/// method is `$VALUES.clone()`, javap-confirmed) while
/// `values()[0] == OWNER_READ`. Handing back one shared array would let a
/// single caller's `values()[0] = null` corrupt every later caller.
///
/// Reads through the nine STATICS rather than through `$VALUES` so it answers
/// correctly in both modes and before the nominated `$VALUES` declaration
/// lands: in real-JDK mode the statics hold the JDK's own interned constants,
/// so this returns those.
pub fn posix_file_permission_values(ctx: &mut dyn NativeContext) -> MethodCallResult {
    let cid = ctx.ensure_class_initialized(POSIX_FILE_PERMISSION)?;
    let arr = ctx.new_ref_array(cid, POSIX_FILE_PERMISSION_CONSTANTS.len());
    for (idx, &name) in POSIX_FILE_PERMISSION_CONSTANTS.iter().enumerate() {
        let published = match ctx.static_field_index_by_name(cid, name) {
            Some(slot) => ctx.get_static_field(cid, slot),
            None => Value::Object(None),
        };
        ctx.set_array_element(arr, idx, published);    }
    Ok(Some(Value::Object(Some(arr))))}

/// `PosixFilePermission.valueOf(String)` — resolved THROUGH the static field,
/// so the answer is the same object `GETSTATIC` yields
/// (`valueOf("GROUP_WRITE") == GROUP_WRITE`, measured on the oracle).
///
/// Both failure shapes are the oracle's, quoted from this host:
/// `valueOf(null)` → `NullPointerException: Name is null`;
/// `valueOf("nope")` → `IllegalArgumentException: No enum constant
/// java.nio.file.attribute.PosixFilePermission.nope`. The `$` replacement is
/// carried from the shared idiom for nested enums; this class is top-level so
/// it never fires here.
pub fn posix_file_permission_value_of(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let requested = match args.first() {
        Some(Value::Object(Some(s))) => ctx.read_string(*s),
        _ => None,
    };
    let Some(requested) = requested else {
        return Err(RuntimeError::NullPointerException {
            message: Some("Name is null".to_string()),
        }
        .into());
    };
    let cid = ctx.ensure_class_initialized(POSIX_FILE_PERMISSION)?;
    if POSIX_FILE_PERMISSION_CONSTANTS.contains(&requested.as_str()) {
        if let Some(slot) = ctx.static_field_index_by_name(cid, &requested) {
            let published = ctx.get_static_field(cid, slot);
            if matches!(published, Value::Object(Some(_))) {
                return Ok(Some(published));
            }
        }
    }
    Err(RuntimeError::IllegalArgumentException {
        message: format!(
            "No enum constant {}.{requested}",
            POSIX_FILE_PERMISSION.replace(['/', '$'], ".")
        ),
    }
    .into())
}

pub fn register_posix_file_permission_stub_clinit(r: &mut NativeMethodRegistry) {
            r.register(
        POSIX_FILE_PERMISSION,
        "<clinit>",
        "()V",
        posix_file_permission_stub_clinit, cratonvm_native_api::NativeKind::Bridge,
    );
    // `values()`/`valueOf(String)` are `invokestatic` against the ENUM class,
    // and a synthetic stub declares neither — `PosixFilePermission.values()`
    // was a `NoSuchMethodError`, and `EnumSet.allOf` / `Class
    // .getEnumConstants` had nothing to read. Unlike the `<clinit>` these two
    // are NOT gated on `is_class_synthetic_stub`: both answer by reading the
    // class's own statics, so on a real JDK class they return the JDK's own
    // interned constants and agree with the bytecode they shadow.
    r.register(
        POSIX_FILE_PERMISSION,
        "values",
        "()[Ljava/nio/file/attribute/PosixFilePermission;",
        |ctx, _args| posix_file_permission_values(ctx), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        POSIX_FILE_PERMISSION,
        "valueOf",
        "(Ljava/lang/String;)Ljava/nio/file/attribute/PosixFilePermission;",
        posix_file_permission_value_of, cratonvm_native_api::NativeKind::Bridge,
    );
}


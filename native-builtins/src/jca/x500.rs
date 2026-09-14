// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP6.6 — `javax.security.auth.x500.X500Principal` natives.
//!
//! ## What is here and what is next door
//!
//! This file is the BINDING: it reads the arguments, keeps the object's state,
//! and raises what the JDK raises. The DN grammar — parsing, the four output
//! formats, and the DER — is [`super::x500_name`], and it is a separate module
//! because a DN is not a keyword and a string. Every defect the 2026-09-11
//! wave fixed here came from a model that could not hold an attribute value's
//! ASN.1 STRING TYPE, and 138 of `L6X500Sweep`'s 403 rows differed from
//! HotSpot 25.0.4+7 because of it.
//!
//! There used to be a second parser in this file. There is not any more: two
//! parsers in one file agree until the day they do not, and this one had
//! already drifted — its re-derivation path encoded a `#<hex>` value a second
//! time, so a principal stopped equalling itself.
//!
//! ## Layout
//!
//! `X500Principal` instances are allocated by the VM's `new` bytecode from the
//! *real* JDK 25 class, which declares exactly one instance field —
//! `transient X500Name thisX500Name` (the three `RFC*` constants are
//! `static`). The object therefore has a single slot (index 0), and this VM
//! repurposes it to hold the principal's RFC 2253 name.
//!
//! | Slot | Field |
//! |------|-------|
//! |  0   | the RFC 2253 name string |
//!
//! The DER cannot live there — there is no second slot — so it is kept beside
//! the object in [`x500_der_table`], keyed by identity. That table is the
//! authority for `getEncoded()` because re-deriving an encoding from the name
//! loses the string type of any value whose type its text does not imply.
//! Everything else reads the NAME first: the RFC 2253 form spells such values
//! as `#<DER hex>`, so parsing it back recovers the type where it matters and
//! preserves the attribute ORDER inside a multi-valued RDN, which DER's sorted
//! SET discards.

#![allow(clippy::needless_range_loop)]

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ArrayElementType, ObjectRef, Value};

use super::asn1;
use super::x500_name;

/// The single declared instance slot of `X500Principal` (`thisX500Name`).
/// We repurpose it to hold the canonical RFC-4514 DN string. The real JDK
/// class has no second field for the DER, so the encoding a principal was
/// built from is kept beside the object instead (see `x500_der_table`) —
/// re-deriving it from this string loses the ASN.1 string types.
const FIELD_CANONICAL: usize = 0;

// ---------------------------------------------------------------------------
// Native bindings
// ---------------------------------------------------------------------------

fn read_string(ctx: &mut dyn NativeContext, args: &[Value], idx: usize) -> Option<String> {
    match args.get(idx) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o),
        _ => None,
    }
}

fn read_byte_array(ctx: &mut dyn NativeContext, args: &[Value], idx: usize) -> Option<Vec<u8>> {
    let arr = match args.get(idx) {
        Some(Value::Object(Some(a))) => *a,
        _ => return None,
    };
    let len = ctx.array_length(arr);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        if let Value::Int(b) = ctx.get_array_element(arr, i) {
            out.push(b as u8);
        } else {
            return None;
        }
    }
    Some(out)
}

fn alloc_byte_array(ctx: &mut dyn NativeContext, bytes: &[u8]) -> ObjectRef {
    let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
    for (i, &b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
    }
    arr
}

/// Populate an X500Principal instance from an RFC-4514 string and DER.
///
/// The real-JDK `javax.security.auth.x500.X500Principal` declares exactly
/// one instance field — `transient X500Name thisX500Name` — so the object
/// is allocated with a single slot (index 0).  Writing anything to index 1
/// is an out-of-bounds field write that the heap guard drops.  We therefore
/// store *only* the canonical RFC-4514 string in that single slot; the DER
/// is never persisted as a field — `get_der` re-derives it on demand by
/// re-encoding the canonical string, which is byte-stable for the canonical
/// `Name` form.
fn populate(ctx: &mut dyn NativeContext, this: ObjectRef, canonical: &str, der: &[u8]) {
    remember_der(ctx, this, der);
    let s = ctx.create_string(canonical);
    // Write the canonical string into the one declared instance slot.
    // `set_field_by_name` resolves `thisX500Name` to slot 0; the explicit
    // `set_field(.., FIELD_CANONICAL, ..)` is the same slot and keeps the
    // slot-based readers (`get_canonical`) working without a name lookup.
    ctx.set_field_by_name(this, "thisX500Name", Value::Object(Some(s)));
    ctx.set_field(this, FIELD_CANONICAL, Value::Object(Some(s)));
}

/// Populate from a string DN, or raise the way the JDK raises.
///
/// The JDK's parser VALIDATES. `L6X500Sweep` asked it nineteen malformed
/// names — `CN`, `=Alice`, `CN=Alice,,O=x`, `CN=#0402`, `NoSuchKeyword=x`,
/// `1..2=x`, `CN="unterminated`, … — and every one raised
/// `IllegalArgumentException("improperly specified input name: <dn>")`. This
/// VM built a principal from all nineteen, usually an EMPTY one, which is the
/// dangerous answer: an empty DN equals no certificate subject, so an access
/// check against it fails closed-looking and silently.
fn init_from_string(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    dn: &str,
    keywords: Option<&std::collections::HashMap<String, String>>,
) -> Result<(), MethodCallFailed> {
    let Ok(rdns) = x500_name::parse_dn(dn, keywords) else {
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/lang/IllegalArgumentException",
            &format!("improperly specified input name: {dn}"),
        ));
    };
    let der = x500_name::encode_der(&rdns);
    let name = x500_name::render(&rdns, x500_name::Format::Rfc2253);
    populate(ctx, this, &name, &der);
    Ok(())
}

/// Populate from a DER byte array. A DER that does not decode is
/// `IllegalArgumentException("improperly specified input name")` — with no
/// `: <dn>` suffix, there being no name to name.
fn init_from_der(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    der: &[u8],
) -> Result<(), MethodCallFailed> {
    // ZERO bytes is an empty name, not a malformed one. Measured: `new
    // X500Principal(new byte[0]).getName()` is `""` on HotSpot while
    // `new byte[]{1,2,3,4}` raises — so the encoding is not being validated
    // into existence, it is being read, and there is nothing to read.
    if der.is_empty() {
        populate(ctx, this, "", der);
        return Ok(());
    }
    let Ok(rdns) = x500_name::decode_der(der) else {
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/lang/IllegalArgumentException",
            "improperly specified input name",
        ));
    };
    let name = x500_name::render(&rdns, x500_name::Format::Rfc2253);
    populate(ctx, this, &name, der);
    Ok(())
}

/// The parsed DN of a principal.
///
/// The DER is authoritative because it carries each value's ASN.1 string type
/// and the stored name string does not: `CN=\41lice` and `CN=Alice` render
/// identically and encode differently. The string is the fallback for a
/// principal whose side-table row has been evicted.
fn rdns_of(ctx: &mut dyn NativeContext, this: ObjectRef) -> Vec<x500_name::Rdn> {
    // The STORED NAME first, and the DER only as the fallback.
    //
    // That looks backwards — the DER carries the ASN.1 types and the string
    // does not — but the string is the RFC 2253 form, which spells any value
    // its grammar cannot express as `#<DER hex>`; parsing it back recovers the
    // type for exactly the values whose type is not implied by their text.
    //
    // What the DER cannot recover is ORDER INSIDE a multi-valued RDN. An RDN
    // is a SET and DER sorts a SET by encoding, so `CN=Alice+OU=Eng` comes
    // back as `OU=Eng+CN=Alice` — measured, and wrong: HotSpot prints the
    // attributes in the order the name was written and sorts only when it
    // encodes.
    if let Some(text) = get_canonical(ctx, this) {
        if let Ok(rdns) = x500_name::parse_dn(&text, None) {
            if !rdns.is_empty() {
                return rdns;
            }
        }
    }
    let der = get_der(ctx, this);
    if !der.is_empty() {
        if let Ok(rdns) = x500_name::decode_der(&der) {
            return rdns;
        }
    }
    Vec::new()
}

/// Read a `java.util.Map<String,String>` argument into a Rust map.
///
/// Both of `X500Principal`'s maps are String->String, and they run in OPPOSITE
/// directions: the constructor's is keyword->OID (so a caller can name an
/// attribute the JDK's table lacks) and `getName`'s is OID->keyword (so a
/// caller can spell one it would otherwise print as a dotted OID). Reading is
/// the same; who calls it decides which way round it is read.
fn read_string_map(
    ctx: &mut dyn NativeContext,
    map: ObjectRef,
) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    // Every call below is real Java and therefore a GC point, and the
    // iterator, the entry set and the map itself are all live across the
    // loop — so each one is read back through its pin after every call
    // rather than carried as the address it had when we got it.
    let map_pin = ctx.pin_native_root(map);
    let map = ctx.read_native_pin(map_pin, map);
    let set = match ctx.invoke_virtual(map, "entrySet", "()Ljava/util/Set;", &[]) {
        Ok(Some(Value::Object(Some(set)))) => set,
        _ => {
            ctx.unpin_native_roots(map_pin);
            return out;
        }
    };
    let set_pin = ctx.pin_native_root(set);
    let set = ctx.read_native_pin(set_pin, set);
    let it = match ctx.invoke_virtual(set, "iterator", "()Ljava/util/Iterator;", &[]) {
        Ok(Some(Value::Object(Some(it)))) => it,
        _ => {
            ctx.unpin_native_roots(map_pin);
            return out;
        }
    };
    let it_pin = ctx.pin_native_root(it);
    loop {
        let it_now = ctx.read_native_pin(it_pin, it);
        match ctx.invoke_virtual(it_now, "hasNext", "()Z", &[]) {
            Ok(Some(Value::Int(1))) => {}
            _ => break,
        }
        let it_now = ctx.read_native_pin(it_pin, it);
        let entry = match ctx.invoke_virtual(it_now, "next", "()Ljava/lang/Object;", &[]) {
            Ok(Some(Value::Object(Some(entry)))) => entry,
            _ => break,
        };
        let entry_pin = ctx.pin_native_root(entry);
        let entry_now = ctx.read_native_pin(entry_pin, entry);
        let key = match ctx.invoke_virtual(entry_now, "getKey", "()Ljava/lang/Object;", &[]) {
            Ok(Some(Value::Object(Some(k)))) => ctx.read_string(k),
            _ => None,
        };
        let entry_now = ctx.read_native_pin(entry_pin, entry);
        let value = match ctx.invoke_virtual(entry_now, "getValue", "()Ljava/lang/Object;", &[]) {
            Ok(Some(Value::Object(Some(v)))) => ctx.read_string(v),
            _ => None,
        };
        ctx.unpin_native_roots(entry_pin);
        if let (Some(k), Some(v)) = (key, value) {
            out.insert(k.to_ascii_uppercase(), v);
        }
    }
    ctx.unpin_native_roots(map_pin);
    out
}

/// Map a `getName(String)` format argument onto a renderer.
fn format_of(name: &str) -> Option<x500_name::Format> {
    if name.eq_ignore_ascii_case("RFC2253") {
        Some(x500_name::Format::Rfc2253)
    } else if name.eq_ignore_ascii_case("RFC1779") {
        Some(x500_name::Format::Rfc1779)
    } else if name.eq_ignore_ascii_case("CANONICAL") {
        Some(x500_name::Format::Canonical)
    } else {
        None
    }
}

/// Read the canonical string field from an instance.  Tries the slot-based
/// layout first (synthetic-mode), then falls back to the real-JDK named
/// field (`thisX500Name`) which our `populate` writes to so the value
/// survives even when the JDK class only declares one slot.
fn get_canonical(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<String> {
    if let Value::Object(Some(s)) = ctx.get_field(this, FIELD_CANONICAL) {
        if let Some(text) = ctx.read_string(s) {
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    if let Value::Object(Some(s)) = ctx.get_field_by_name(this, "thisX500Name") {
        return ctx.read_string(s);
    }
    None
}

/// How many principals' encodings to keep. Bounded because the table is keyed
/// by identity hash and nothing tells us when a principal dies.
const X500_DER_MAX_ENTRIES: usize = 4096;

/// The ORIGINAL DER of principals constructed from DER, keyed by identity hash.
///
/// `X500Principal` declares one instance field and `populate` needs it for the
/// canonical string, so there is nowhere on the object to keep the encoding.
/// It nevertheless has to be kept. The canonical RFC-4514 string does not
/// carry the ASN.1 STRING TYPE of each attribute value, so re-encoding it is
/// NOT the identity function — and `get_der` used to do exactly that, under a
/// comment asserting "the canonical `Name` form is byte-stable, so this
/// round-trips exactly". It is not, and the difference is interop-visible,
/// because RFC 5280 name matching is byte equality over the DER.
///
/// Measured (`IdpDbg` probe, jdk-25 as the control): BouncyCastle writes
/// `CN=Root,O=BC` with UTF8String, tag `0c`; re-encoding the canonical string
/// picks PrintableString, tag `13`, because the characters permit it.
///
/// ```text
/// certGn  ...06035504030c04526f6f74...   from the certificate  (0c = UTF8String)
/// expGn   ...0603550403 1304526f6f74...  rebuilt via X500Principal (13 = Printable)
/// equals=false          -- and both print `CN=Root,O=BC,OU=Test+O=Bouncy`
/// ```
///
/// So `X509CRL.getIssuerX500Principal().getEncoded()` disagreed with the CRL's
/// own issuer bytes, and BouncyCastle's
/// `PKIXCRLValidator.checkDistributionPointName` compared two `GeneralName`s
/// that RENDER identically and are not equal. That is `IDPRelativeNameTest`,
/// where the expanded distribution-point name never matched the certificate's;
/// the reported failure named a DIFFERENT distribution point, because
/// `checkCRLs` keeps only the LAST exception and retries with one synthesised
/// from the issuer.
/// LOCK LEVEL (lock-discipline ratchet): `Scratch`. Both callers evaluate
/// `ctx.identity_hash_code` into a local BEFORE acquiring, and the bodies under
/// the guard are map operations on `Vec<u8>` only.
fn x500_der_table(
) -> &'static cratonvm_types::lock_order::OrderedMutex<std::collections::HashMap<i32, Vec<u8>>> {
    static T: std::sync::OnceLock<
        cratonvm_types::lock_order::OrderedMutex<std::collections::HashMap<i32, Vec<u8>>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedMutex::new(
            std::collections::HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// Record the encoding this principal was built from.
fn remember_der(ctx: &mut dyn NativeContext, this: ObjectRef, der: &[u8]) {
    if der.is_empty() {
        return;
    }
    let id = ctx.identity_hash_code(this);
    let mut t = match x500_der_table().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if t.len() >= X500_DER_MAX_ENTRIES {
        let target = X500_DER_MAX_ENTRIES / 2;
        let mut ids: Vec<i32> = t.keys().copied().filter(|&k| k != id).collect();
        ids.sort_unstable();
        let to_remove = t.len().saturating_sub(target);
        for k in ids.into_iter().take(to_remove) {
            t.remove(&k);
        }
    }
    t.insert(id, der.to_vec());
}

/// The recorded encoding, but only if it still describes THIS principal.
///
/// An identity hash is not a handle: two live objects may share one, and the
/// table outlives the principal that filled it. Handing back a stranger's DER
/// from an identity object would be worse than re-encoding, so the entry is
/// only used when decoding it reproduces the canonical name the object
/// currently carries. A miss falls back to the previous behaviour.
fn recall_der(ctx: &mut dyn NativeContext, this: ObjectRef, canonical: &str) -> Option<Vec<u8>> {
    let id = ctx.identity_hash_code(this);
    let der = {
        let t = match x500_der_table().lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        t.get(&id).cloned()?
    };
    // The stored name is the RFC 2253 form, so the check has to render the
    // decoded DER the same way — comparing against a CANONICAL rendering would
    // reject every entry, since the two differ in case for every DN that has a
    // letter in it. (The old helper this replaces was named `render_canonical`
    // and produced the RFC 2253 form, which is how the mismatch stayed
    // invisible.)
    let rdns = x500_name::decode_der(&der).ok()?;
    if x500_name::render(&rdns, x500_name::Format::Rfc2253) == canonical {
        Some(der)
    } else {
        None
    }
}

/// Read the DER encoding of an instance.
///
/// The encoding the principal was BUILT from wins, so `new X500Principal(der)
/// .getEncoded()` returns `der` — see [`x500_der_table`] for why re-deriving it
/// from the canonical string is not the same thing. Re-encoding remains the
/// fallback for principals built from a string (where it is exact, since there
/// was no original) and for any entry that can no longer be trusted.
fn get_der(ctx: &mut dyn NativeContext, this: ObjectRef) -> Vec<u8> {
    if let Some(canon) = get_canonical(ctx, this) {
        if let Some(der) = recall_der(ctx, this, &canon) {
            return der;
        }
        // Re-derive through the same grammar the constructors use. The old
        // `parse_grouped_rdns` + `encode_grouped_rdns_to_der` pair could not
        // read the `#<hex>` value form, so re-deriving the encoding of
        // `1.3.6.1.4.1.99999.1=#130178` produced a UTF8String whose CONTENT was
        // the seven characters `#130178` — the value encoded twice, and a
        // principal that no longer equalled itself.
        if let Ok(rdns) = x500_name::parse_dn(&canon, None) {
            if !rdns.is_empty() {
                return x500_name::encode_der(&rdns);
            }
        }
    }
    Vec::new()
}

fn java_string_hash(s: &str) -> i32 {
    let mut h: i32 = 0;
    for u in s.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(u as i32);
    }
    h
}

/// One AVA rendered with `keyword`'s type map, quoting the value when RFC 1779
/// requires it. Shared by the RFC 1779 form and by `toString`, which differ ONLY
// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register(r: &mut NativeMethodRegistry) {
    let cls = "javax/security/auth/x500/X500Principal";

    // <init>(String) — parse RFC 4514 DN.  The JDK 25 implementation
    // routes through `sun/security/x509/X500Name(String)` which we
    // bypass entirely.
    r.register(cls, "<init>", "(Ljava/lang/String;)V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => {
                return Err(RuntimeError::NullPointerException {
                    message: Some("X500Principal: this is null".into()),
                }
                .into())
            }
        };
        // A null name is a NullPointerException with the JDK's own wording,
        // not an empty principal.
        let Some(dn) = read_string(ctx, args, 1) else {
            return Err(RuntimeError::NullPointerException {
                message: Some("provided null name".into()),
            }
            .into());
        };
        init_from_string(ctx, this, &dn, None)?;
        Ok(None)
    });

    // <init>(String, Map) — the map is an attribute-type keyword map, keyed by
    // KEYWORD and valued by dotted OID (its javadoc's direction). It is the
    // only way a caller can name an attribute the JDK's own table does not
    // have; without it, `MYOID=x` is an unparseable name rather than a
    // principal with an odd attribute.
    r.register(
        cls,
        "<init>",
        "(Ljava/lang/String;Ljava/util/Map;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("X500Principal: this is null".into()),
                    }
                    .into())
                }
            };
            let Some(dn) = read_string(ctx, args, 1) else {
                return Err(RuntimeError::NullPointerException {
                    message: Some("provided null name".into()),
                }
                .into());
            };
            let map = match args.get(2) {
                Some(Value::Object(Some(m))) => read_string_map(ctx, *m),
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("provided null keyword map".into()),
                    }
                    .into())
                }
            };
            init_from_string(ctx, this, &dn, Some(&map))?;
            Ok(None)
        },
    );

    // <init>(byte[]) — parse DER. A null or unparseable array raises: the JDK
    // has nothing to build a name from either way, and answers
    // `IllegalArgumentException` for both.
    r.register(cls, "<init>", "([B)V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => {
                return Err(RuntimeError::NullPointerException {
                    message: Some("X500Principal: this is null".into()),
                }
                .into())
            }
        };
        // A NULL array and an EMPTY one are different answers: null raises,
        // empty is the empty name.
        if !matches!(args.get(1), Some(Value::Object(Some(_)))) {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/lang/IllegalArgumentException",
                "improperly specified input name",
            ));
        }
        let der = read_byte_array(ctx, args, 1).unwrap_or_default();
        init_from_der(ctx, this, &der)?;
        Ok(None)
    });

    // <init>(InputStream) — read all bytes, then parse DER.
    //
    // STUB-REMOVAL (wave 3): this was a no-op whose own comment admitted the
    // consequence — "the caller will see an empty principal and every
    // comparison fails". A principal with no name silently equals nothing and
    // matches nothing, so every downstream identity check against it (KeyStore
    // alias lookup, cert subject/issuer comparison) quietly answers "no" rather
    // than failing loudly. There IS a stream pump: drain the argument with a
    // virtual `readAllBytes()` exactly like `phases_early::
    // scanner_drain_input_stream` and `locale_resources` already do, then hand
    // the bytes to the same `init_from_der` the `([B)V` ctor uses.
    r.register(cls, "<init>", "(Ljava/io/InputStream;)V", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => {
                return Err(RuntimeError::NullPointerException {
                    message: Some("X500Principal: this is null".into()),
                }
                .into())
            }
        };
        let stream = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            // Real JDK dereferences the stream immediately, so a null argument
            // NPEs there too. Do NOT build an empty principal instead.
            _ => {
                return Err(RuntimeError::NullPointerException {
                    message: Some("provided null input stream".into()),
                }
                .into())
            }
        };
        // `readAllBytes` runs arbitrary Java and can move the heap: pin both
        // refs and re-read them afterwards.
        let pin = ctx.pin_native_root(this);
        let _ = ctx.pin_native_root(stream);
        let read = ctx.invoke_virtual(stream, "readAllBytes", "()[B", &[]);
        let this = ctx.read_native_pin(pin, this);
        ctx.unpin_native_roots(pin);
        let der = match read? {
            Some(Value::Object(Some(arr))) => {
                let len = ctx.array_length(arr);
                let mut buf = vec![0u8; len];
                let n = ctx.read_byte_array_into(arr, 0, &mut buf);
                buf.truncate(n);
                buf
            }
            _ => Vec::new(),
        };
        // `X500Principal(InputStream)` is specified to throw
        // IllegalArgumentException when the stream does not hold a valid DER
        // Name encoding. Report that instead of populating a nameless
        // principal — `init_from_der` alone would fall back to an empty
        // canonical string and hide the failure.
        init_from_der(ctx, this, &der)?;
        Ok(None)
    });

    // getEncoded() -> byte[]
    //
    // Two-tier read: first try the slot-based path (works for synthetic
    // allocations with a 2-slot layout).  If that comes back empty (e.g.
    // when the real-JDK X500Principal class is loaded with a single
    // `thisX500Name` slot, so our DER slot 1 was lost on write), fall
    // through to re-deriving the DER from the canonical name field
    // populated via `set_field_by_name`.
    r.register(cls, "getEncoded", "()[B", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let mut der = get_der(ctx, this);
        if der.is_empty() {
            // Recover from the stored name. This loses the ASN.1 string type
            // of any value whose type is not the one the text implies, which
            // is why the side table is consulted first and why it exists.
            der = x500_name::encode_der(&rdns_of(ctx, this));
        }
        let arr = alloc_byte_array(ctx, &der);
        Ok(Some(Value::Object(Some(arr))))
    });

    // getName() -> String. RFC 2253, which is the stored form.
    r.register(cls, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let s = match get_canonical(ctx, this) {
            Some(text) => text,
            None => x500_name::render(&rdns_of(ctx, this), x500_name::Format::Rfc2253),
        };
        let so = ctx.create_string(&s);
        Ok(Some(Value::Object(Some(so))))
    });

    // getName(String format) -> String.
    //
    // The three formats are three different renderings of one name, not one
    // string with its separators swapped: the keyword table, the escaping and
    // the treatment of a value's ASN.1 type all differ. An unrecognised (or
    // null) format is `IllegalArgumentException("invalid format specified")`
    // rather than a silent fall-back to RFC 2253.
    r.register(
        cls,
        "getName",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let fmt = match args.get(1) {
                Some(Value::Object(Some(f))) => ctx.read_string(*f).unwrap_or_default(),
                _ => String::new(),
            };
            let Some(format) = format_of(&fmt) else {
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/lang/IllegalArgumentException",
                    "invalid format specified",
                ));
            };
            let s = x500_name::render(&rdns_of(ctx, this), format);
            let so = ctx.create_string(&s);
            Ok(Some(Value::Object(Some(so))))
        },
    );

    // getName(String format, Map<String,String> oidMap) -> String.
    //
    // This map runs OID -> keyword, the opposite direction to the
    // constructor's, and CANONICAL rejects it outright: the canonical form is
    // defined by the standard and a caller's keyword would make two equal
    // names render differently.
    r.register(
        cls,
        "getName",
        "(Ljava/lang/String;Ljava/util/Map;)Ljava/lang/String;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let fmt = match args.get(1) {
                Some(Value::Object(Some(f))) => ctx.read_string(*f).unwrap_or_default(),
                _ => String::new(),
            };
            let format = match format_of(&fmt) {
                Some(x500_name::Format::Canonical) | None => {
                    return Err(crate::phases_early::throw_jca_exc(
                        ctx,
                        "java/lang/IllegalArgumentException",
                        "invalid format specified",
                    ))
                }
                Some(other) => other,
            };
            let map = match args.get(2) {
                Some(Value::Object(Some(m))) => read_string_map(ctx, *m),
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("provided null OID map".into()),
                    }
                    .into())
                }
            };
            for keyword in map.values() {
                if !keyword.chars().next().is_some_and(|c| c.is_alphabetic()) {
                    return Err(crate::phases_early::throw_jca_exc(
                        ctx,
                        "java/lang/IllegalArgumentException",
                        "keyword does not start with letter",
                    ));
                }
            }
            let s = x500_name::render_with_oid_map(&rdns_of(ctx, this), format, &map);
            let so = ctx.create_string(&s);
            Ok(Some(Value::Object(Some(so))))
        },
    );

    // toString() -> String. `X500Name.toString()`: the `", "` layout with RFC
    // 1779's quoting, and the JDK's FULL keyword table, which is where `DNQ`,
    // `T` and `EMAILADDRESS` come from.
    r.register(cls, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let s = x500_name::render(&rdns_of(ctx, this), x500_name::Format::Display);
        let so = ctx.create_string(&s);
        Ok(Some(Value::Object(Some(so))))
    });

    // hashCode() -> int. The JDK's is the CANONICAL name's `String.hashCode()`
    // (`X500Name.hashCode()`), so it must be that here too — a DER-derived hash
    // disagrees with the new canonical `equals` for exactly the DN pairs equals
    // now (correctly) calls equal, which would file two equal principals in
    // different `HashMap` buckets.
    r.register(cls, "hashCode", "()I", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let canon = x500_name::render(&rdns_of(ctx, this), x500_name::Format::Canonical);
        Ok(Some(Value::Int(java_string_hash(&canon))))
    });

    // equals(Object) -> boolean. `X500Principal.equals` is defined on the
    // CANONICAL form: two DNs are the same name when they differ only in
    // attribute-name case, value case, or runs of whitespace. Comparing the
    // stored RFC 2253 strings (and then the DER) answered `false` for exactly
    // those pairs — see the table above this file's string-form helpers.
    r.register(cls, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let other = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        // A non-`X500Principal` argument has no DN to canonicalise; the JDK
        // answers `false` for it rather than comparing something else.
        let other_is_principal = ctx
            .class_name_of_id(ctx.class_id_of_object(other))
            .is_some_and(|n| n == "javax/security/auth/x500/X500Principal");
        if !other_is_principal {
            return Ok(Some(Value::Int(0)));
        }
        let a = x500_name::render(&rdns_of(ctx, this), x500_name::Format::Canonical);
        let b = x500_name::render(&rdns_of(ctx, other), x500_name::Format::Canonical);
        if !a.is_empty() || !b.is_empty() {
            return Ok(Some(Value::Int(i32::from(a == b))));
        }
        // BOTH canonical forms are empty, which is two different situations:
        // the genuinely empty DN (`new X500Principal("")`, and two of those
        // ARE equal), and a principal whose stored name this VM's other
        // natives wrote directly into the object without going through the
        // parser — `phases_late::ssl_security` mints several that way. For the
        // second, an empty canonical form means "did not parse", and treating
        // two unparseable names as equal would make every one of them equal to
        // every other. Fall back to the stored text.
        let a_text = get_canonical(ctx, this).unwrap_or_default();
        let b_text = get_canonical(ctx, other).unwrap_or_default();
        Ok(Some(Value::Int(i32::from(a_text == b_text))))
    });

    // sun.security.x509.X500Name.asX500Principal() — kcfull #12.
    //
    // The real bytecode routes through
    // `SharedSecrets.getJavaxSecurityAccess().asX500Principal(name)`, but CV
    // never wires `JavaxSecurityAccess` (its registrar, `X500Principal.<clinit>`,
    // doesn't run its `SharedSecrets.setJavaxSecurityAccess(..)` under our native
    // interception), so the access is null → the `invokeinterface` NPEs →
    // `X509CertImpl.getSubjectX500Principal()` / `getIssuerX500Principal()`
    // silently return null (their `catch (Exception)`). That broke keycloak
    // `TruststoreBuilder.setCertificateEntry`, whose
    // `x509.getSubjectX500Principal().getName()` NPE'd while merging a PEM
    // truststore. `getSubjectDN()` worked because it does not go through this
    // path. Build the principal directly from the (real) Name's DER — the
    // `X500Principal([B)` ctor above round-trips the canonical Name form; fall
    // back to the RFC2253 string if the DER is unavailable.
    r.register(
        "sun/security/x509/X500Name",
        "asX500Principal",
        "()Ljavax/security/auth/x500/X500Principal;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            // Primary: DER round-trip from the real X500Name. `this` is read
            // again by the RFC2253 fallback below, after this call.
            let this_pin = ctx.pin_native_root(this);
            if let Ok(Some(Value::Object(Some(arr)))) =
                ctx.invoke_virtual(this, "getEncoded", "()[B", &[])
            {
                if ctx.array_length(arr) > 0 {
                    if let Ok(p) = ctx.new_object_initialized(
                        "javax/security/auth/x500/X500Principal",
                        "([B)V",
                        &[Value::Object(Some(arr))],
                    ) {
                        return Ok(p);
                    }
                }
            }
            // Fallback: RFC2253 name string. `getEncoded()` above allocated a
            // byte array and may have moved `this`.
            let this = ctx.read_native_pin(this_pin, this);
            if let Ok(Some(Value::Object(Some(s)))) =
                ctx.invoke_virtual(this, "getName", "()Ljava/lang/String;", &[])
            {
                if let Some(name) = ctx.read_string(s) {
                    let ns = ctx.create_string(&name);
                    if let Ok(p) = ctx.new_object_initialized(
                        "javax/security/auth/x500/X500Principal",
                        "(Ljava/lang/String;)V",
                        &[Value::Object(Some(ns))],
                    ) {
                        return Ok(p);
                    }
                }
            }
            Ok(Some(Value::Object(None)))
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// The six tests that used to live here exercised a SECOND parser that
    /// this file no longer has: `parse_dn_string`, `parse_grouped_rdns`,
    /// `encode_rdns_to_der`, `decode_rdns` and `render_canonical` were the old
    /// keyword-and-text model, and every claim they made is now made against
    /// the real grammar in `x500_name.rs`'s own tests — where the expected
    /// values are HotSpot 25.0.4+7's rather than this VM's.
    ///
    /// The mapping, so the deletion is auditable rather than a disappearance:
    ///
    /// | old test | now |
    /// |---|---|
    /// | `parse_dn_simple` | `x500_name::tests::a_dotted_oid_attribute_renders_hex_in_rfc2253_and_text_in_rfc1779` and the round-trip test |
    /// | `parse_dn_with_escaped_comma` | `a_quoted_value_keeps_its_comma` |
    /// | `round_trip_probe_input` | `the_der_round_trips_through_decode` |
    /// | `unknown_oid_round_trip` | `the_keyword_map_is_keyed_by_keyword_not_by_oid` + the round-trip test |
    /// | `canonical_uppercases_keys` | `canonical_collapses_spaces_but_not_tabs` (canonical LOWERCASES; the old test asserted the opposite and passed, because the old renderer did that) |
    /// | `grouped_rdn_preserves_each_attribute_in_der` | `a_multi_valued_rdn_keeps_both_attributes_and_sorts_only_in_canonical` |
    ///
    /// The fifth row is the one worth reading twice. `canonical_uppercases_keys`
    /// asserted `canon.starts_with("CN=test")`, and the JDK's CANONICAL form is
    /// `cn=test` — the test was written from the implementation, agreed with
    /// it, and pinned the defect.
    #[test]
    fn the_dn_grammar_is_tested_in_x500_name() {
        // A live assertion rather than a comment: the formats this file's
        // natives hand out come from that module, and this is the seam.
        let rdns = x500_name::parse_dn("CN=Test,O=Acme,C=SE", None).expect("parses");
        assert_eq!(
            x500_name::render(&rdns, x500_name::Format::Rfc2253),
            "CN=Test,O=Acme,C=SE"
        );
        assert_eq!(
            x500_name::render(&rdns, x500_name::Format::Canonical),
            "cn=test,o=acme,c=se"
        );
    }
}

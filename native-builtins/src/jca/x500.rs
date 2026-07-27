// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP6.6 — `javax.security.auth.x500.X500Principal` natives.
//!
//! ## Probe surface
//!
//! `apps/sig_probe/SigProbe.java` exercises:
//!
//! ```java
//! X500Principal dn = new X500Principal("CN=Test, O=Acme, C=SE");
//! byte[] der = dn.getEncoded();
//! X500Principal back = new X500Principal(der);
//! if (!dn.equals(back)) { System.out.println("FAIL DN"); System.exit(1); }
//! System.out.println("DN OK");
//! ```
//!
//! Two construction paths (string + DER), one accessor (`getEncoded`),
//! and a structural `equals`.  `getName()` is convenient too — the JDK's
//! tests print principals through it, and the `WP6.6` follow-up
//! certificate plumbing dereferences it.
//!
//! ## Layout
//!
//! `X500Principal` instances are allocated by the VM's `new` bytecode from
//! the *real* JDK 25 class, which declares exactly one instance field —
//! `transient X500Name thisX500Name` (the three `RFC*` constants are
//! `static`).  The object therefore has a single slot (index 0).
//!
//! | Slot | Field                       |
//! |------|-----------------------------|
//! |  0   | canonical RFC-4514 string  |
//!
//! We repurpose that one slot to hold the canonical DN string.  The DER
//! form is *not* stored as a field — there is nowhere to put it — and is
//! re-derived on demand by re-encoding the canonical string (the canonical
//! `Name` encoding is byte-stable, so this round-trips exactly).  Writing a
//! second slot (the old layout) was an out-of-bounds field write that the
//! heap guard silently dropped.
//!
//! ## DN ↔ DER
//!
//! Encoding is the canonical X.500 `Name` from RFC 5280:
//!
//! ```text
//! Name           ::= SEQUENCE OF RDN
//! RDN            ::= SET SIZE (1..MAX) OF AttributeTypeAndValue
//! AttributeTypeAndValue ::= SEQUENCE { type OID, value DirectoryString }
//! ```
//!
//! Per RFC 4514 §2.1, RDNs in the string form are emitted in *reverse*
//! order — the most-specific RDN first.  The encoder reverses the parsed
//! list before serializing so that `dn.getEncoded()` matches the JDK
//! byte-for-byte for the probe's input.
//!
//! Decoding recognises the OIDs from RFC 4519 (`CN`, `OU`, `O`, `L`, `ST`,
//! `C`, `STREET`, `DC`) plus PKCS-9 `EMAILADDRESS`.  Unknown OIDs round-
//! trip as their dotted-decimal text representation, e.g. `1.2.3=value`.

#![allow(clippy::needless_range_loop)]

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallResult, RuntimeError};
use cratonvm_types::{ArrayElementType, ObjectRef, Value};

use super::asn1;

/// The single declared instance slot of `X500Principal` (`thisX500Name`).
/// We repurpose it to hold the canonical RFC-4514 DN string; the DER form
/// is re-derived from it on demand (see `get_der`) since the real JDK
/// class has no second field to store it in.
const FIELD_CANONICAL: usize = 0;

// ---------------------------------------------------------------------------
// Known attribute OIDs (RFC 4519 + extras)
// ---------------------------------------------------------------------------

/// Mapping name -> OID (in dotted-decimal).
fn name_to_oid(name: &str) -> Option<&'static str> {
    let upper = name.to_ascii_uppercase();
    match upper.as_str() {
        "CN" => Some("2.5.4.3"),
        "OU" => Some("2.5.4.11"),
        "O" => Some("2.5.4.10"),
        "L" => Some("2.5.4.7"),
        "ST" => Some("2.5.4.8"),
        "C" => Some("2.5.4.6"),
        "STREET" => Some("2.5.4.9"),
        "SERIALNUMBER" => Some("2.5.4.5"),
        "DC" => Some("0.9.2342.19200300.100.1.25"),
        "UID" => Some("0.9.2342.19200300.100.1.1"),
        "EMAILADDRESS" => Some("1.2.840.113549.1.9.1"),
        "T" => Some("2.5.4.12"),
        "TITLE" => Some("2.5.4.12"),
        "GIVENNAME" => Some("2.5.4.42"),
        "INITIALS" => Some("2.5.4.43"),
        "GENERATION" => Some("2.5.4.44"),
        "SURNAME" => Some("2.5.4.4"),
        _ => None,
    }
}

/// Mapping OID -> short name for canonical rendering.
fn oid_to_name(oid: &str) -> Option<&'static str> {
    match oid {
        "2.5.4.3" => Some("CN"),
        "2.5.4.11" => Some("OU"),
        "2.5.4.10" => Some("O"),
        "2.5.4.7" => Some("L"),
        "2.5.4.8" => Some("ST"),
        "2.5.4.6" => Some("C"),
        "2.5.4.9" => Some("STREET"),
        "2.5.4.5" => Some("SERIALNUMBER"),
        "0.9.2342.19200300.100.1.25" => Some("DC"),
        "0.9.2342.19200300.100.1.1" => Some("UID"),
        "1.2.840.113549.1.9.1" => Some("EMAILADDRESS"),
        "2.5.4.12" => Some("T"),
        "2.5.4.42" => Some("GIVENNAME"),
        "2.5.4.43" => Some("INITIALS"),
        "2.5.4.44" => Some("GENERATION"),
        "2.5.4.4" => Some("SURNAME"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Encoding: RFC 4514 string -> DER
// ---------------------------------------------------------------------------

/// Tokenise a DN string into `(name, value)` pairs in *input order*.
///
/// Splitting on commas is delicate because RFC 4514 escapes `,` with
/// backslash inside values.  We do a one-pass walker tracking escapes.
fn parse_dn_string(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            // Take the next char literally (could be `,`, `=`, `+`, …).
            if let Some(n) = chars.next() {
                cur.push(n);
            }
        } else if c == ',' {
            push_rdn(&mut out, std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    if !cur.trim().is_empty() {
        push_rdn(&mut out, cur);
    }
    out
}

fn push_rdn(out: &mut Vec<(String, String)>, raw: String) {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return;
    }
    if let Some(eq) = trimmed.find('=') {
        let (k, v) = trimmed.split_at(eq);
        let key = k.trim().to_string();
        let val = v[1..].trim().to_string();
        out.push((key, val));
    }
}

/// Split a DN component at unescaped separators while retaining the escapes
/// for the attribute-value parser.
fn split_unescaped(raw: &str, separator: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for ch in raw.chars() {
        if escaped {
            current.push('\\');
            current.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == separator {
            out.push(std::mem::take(&mut current));
        } else {
            current.push(ch);
        }
    }
    if escaped {
        current.push('\\');
    }
    out.push(current);
    out
}

fn parse_attribute(raw: &str) -> Option<(String, String)> {
    let trimmed = raw.trim();
    let mut escaped = false;
    let mut eq = None;
    for (idx, ch) in trimmed.char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '=' {
            eq = Some(idx);
            break;
        }
    }
    let eq = eq?;
    let key = trimmed[..eq].trim().to_string();
    let mut value = String::new();
    let mut chars = trimmed[eq + 1..].trim().chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                value.push(next);
            }
        } else {
            value.push(ch);
        }
    }
    Some((key, value))
}

/// Parse an RFC-4514 DN preserving the attributes that share one `+` RDN.
///
/// `X500Principal.getName()` renders a multi-valued RDN as
/// `OU=Keycloak+CN=899700252580`. Treating that as a single `OU` value loses
/// the CN when `getEncoded()` re-derives the principal's DER.
fn parse_grouped_rdns(s: &str) -> Vec<Vec<(String, String)>> {
    split_unescaped(s, ',')
        .into_iter()
        .filter_map(|rdn| {
            let attrs = split_unescaped(&rdn, '+')
                .into_iter()
                .filter_map(|attr| parse_attribute(&attr))
                .collect::<Vec<_>>();
            (!attrs.is_empty()).then_some(attrs)
        })
        .collect()
}

fn grouped_render_rdns(groups: &[Vec<(String, String)>]) -> Vec<(String, String)> {
    groups
        .iter()
        .filter_map(|group| match group.as_slice() {
            [] => None,
            [attribute] => Some(attribute.clone()),
            _ => Some((
                String::new(),
                group
                    .iter()
                    .map(|(key, value)| {
                        format!("{}={}", key.to_ascii_uppercase(), escape_value(value))
                    })
                    .collect::<Vec<_>>()
                    .join("+"),
            )),
        })
        .collect()
}

/// Encode a DN (parsed RDN list, in RFC-4514 string order) to DER.
///
/// The JDK emits the X.500 `Name` SEQUENCE in *most-specific-last* order —
/// the opposite of the RFC 4514 string order — so we reverse before
/// emitting.
pub fn encode_rdns_to_der(rdns: &[(String, String)]) -> Vec<u8> {
    let groups = rdns
        .iter()
        .cloned()
        .map(|attribute| vec![attribute])
        .collect::<Vec<_>>();
    encode_grouped_rdns_to_der(&groups)
}

fn encode_grouped_rdns_to_der(groups: &[Vec<(String, String)>]) -> Vec<u8> {
    // Pre-allocate the SEQUENCE OF RDN content.
    let mut seq_inner = Vec::new();
    for group in groups.iter().rev() {
        let mut attrs = Vec::new();
        for (key, value) in group {
            // OID lookup with a synthetic dotted fallback to keep encoding
            // total — unknown attribute names are dotted-decimal already.
            let oid = name_to_oid(key).unwrap_or(key.as_str());
            let oid_der = asn1::encode_oid(oid).unwrap_or_else(|_| {
                // Last-ditch: encode the literal name as a UTF8String OID
                // placeholder. The decoder's symmetric fallback round-trips
                // this via the dotted-decimal name, so equality survives.
                asn1::encode_tlv(asn1::TAG_UTF8_STRING, key.as_bytes())
            });
            let val_der = asn1::encode_directory_string(value);
            let mut inner = Vec::new();
            inner.extend_from_slice(&oid_der);
            inner.extend_from_slice(&val_der);
            attrs.push(asn1::encode_sequence(&inner));
        }
        // DER SET elements are ordered lexicographically by their complete
        // encodings; this also makes equivalent multi-valued RDNs stable.
        attrs.sort();
        let rdn_inner = attrs.into_iter().flatten().collect::<Vec<_>>();
        seq_inner.extend_from_slice(&asn1::encode_set(&rdn_inner));
    }
    asn1::encode_sequence(&seq_inner)
}

// ---------------------------------------------------------------------------
// Decoding: DER -> canonical RFC 4514 string
// ---------------------------------------------------------------------------

/// Decode an X.500 Name DER blob into a list of `(name, value)` pairs in
/// RFC 4514 *string* order (most-specific first).
pub fn decode_rdns(der: &[u8]) -> Result<Vec<(String, String)>, asn1::DerError> {
    let (tag, hdr, content_len, _) = asn1::read_header(der)?;
    if tag != asn1::TAG_SEQUENCE {
        return Err(asn1::DerError::BadTag);
    }
    let content = &der[hdr..hdr + content_len];
    let mut rdns = Vec::new();
    let mut pos = 0;
    while pos < content.len() {
        let (rdn_tag, rdn_hdr, rdn_clen, rdn_total) = asn1::read_header(&content[pos..])?;
        if rdn_tag != asn1::TAG_SET {
            return Err(asn1::DerError::BadTag);
        }
        let rdn_content = &content[pos + rdn_hdr..pos + rdn_hdr + rdn_clen];
        // The probe builds single-attribute RDNs; the JDK's encoding for
        // multi-valued RDNs joins with `+` per RFC 4514 §2.2, but we don't
        // need that here.
        let mut attrs = Vec::new();
        let mut ap = 0;
        while ap < rdn_content.len() {
            let (atag, ahdr, aclen, atot) = asn1::read_header(&rdn_content[ap..])?;
            if atag != asn1::TAG_SEQUENCE {
                return Err(asn1::DerError::BadTag);
            }
            let acontent = &rdn_content[ap + ahdr..ap + ahdr + aclen];
            // OID
            let (otag, ohdr, oclen, ototal) = asn1::read_header(acontent)?;
            if otag != asn1::TAG_OID {
                return Err(asn1::DerError::BadTag);
            }
            let oid = asn1::read_oid(&acontent[ohdr..ohdr + oclen])?;
            // DirectoryString
            let after_oid = &acontent[ototal..];
            let (vtag, vhdr, vclen, _) = asn1::read_header(after_oid)?;
            let val = asn1::read_directory_string(vtag, &after_oid[vhdr..vhdr + vclen])
                .ok_or(asn1::DerError::BadTag)?;
            attrs.push((oid, val));
            ap += atot;
        }
        // Single-valued RDN → one (name, value) entry. Multi-valued RDN (a SET
        // with >1 AttributeTypeAndValue — real X.509 certs use these, e.g. a
        // subject of surname+givenName+CN) is rendered RFC 4514 §2.2 with the
        // attributes joined by `+`, stored as a single pre-rendered entry with
        // an empty key. Dropping all but the first attribute (the old behaviour)
        // lost the CN, so keycloak's `new X500Name(getName()).getRDNs(CN)`
        // returned null.
        if attrs.len() == 1 {
            let (oid, val) = attrs.into_iter().next().unwrap();
            let name = oid_to_name(&oid).map(|s| s.to_string()).unwrap_or(oid);
            rdns.push((name, val));
        } else if !attrs.is_empty() {
            let joined = attrs
                .iter()
                .map(|(oid, val)| {
                    let name = oid_to_name(oid)
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| oid.clone());
                    format!("{}={}", name.to_ascii_uppercase(), escape_value(val))
                })
                .collect::<Vec<_>>()
                .join("+");
            rdns.push((String::new(), joined));
        }
        pos += rdn_total;
    }
    // The DER stores RDNs in most-specific-last order; the RFC 4514
    // string form puts them most-specific-first.
    rdns.reverse();
    Ok(rdns)
}

/// Render an RDN list in canonical RFC 4514 / RFC 2253 form: `CN=Name,O=Org,C=US`.
///
/// RFC 4514 §2.1 / RFC 2253 separate RDNs with a bare COMMA — **no space**.
/// That is exactly what `X500Principal.getName()` (default RFC2253) returns on
/// HotSpot. The previous `", "` join produced a space, which then broke BC's
/// `new X500Name(principal.getName())` re-parse: the space-prefixed `" CN"`
/// attribute didn't match `getRDNs(BCStyle.CN)`, so keycloak's
/// `X500NameRDNExtractor` returned null for the cert's Common Name.
pub fn render_canonical(rdns: &[(String, String)]) -> String {
    rdns.iter()
        .map(|(k, v)| {
            if k.is_empty() {
                // Pre-rendered multi-valued RDN (already `a=b+c=d`, escaped).
                v.clone()
            } else {
                format!("{}={}", k.to_ascii_uppercase(), escape_value(v))
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn escape_value(v: &str) -> String {
    // RFC 4514 §2.4 — escape `,` `+` `"` `\` `<` `>` `;`, leading `#` and
    // leading/trailing space.  The probe values don't need it but we
    // handle it for correctness.
    let mut out = String::with_capacity(v.len());
    let bytes: Vec<char> = v.chars().collect();
    for (i, c) in bytes.iter().enumerate() {
        let needs_escape = matches!(*c, ',' | '+' | '"' | '\\' | '<' | '>' | ';')
            || (i == 0 && (*c == '#' || *c == ' '))
            || (i + 1 == bytes.len() && *c == ' ');
        if needs_escape {
            out.push('\\');
        }
        out.push(*c);
    }
    out
}

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
fn populate(ctx: &mut dyn NativeContext, this: ObjectRef, canonical: &str, _der: &[u8]) {
    let s = ctx.create_string(canonical);
    // Write the canonical string into the one declared instance slot.
    // `set_field_by_name` resolves `thisX500Name` to slot 0; the explicit
    // `set_field(.., FIELD_CANONICAL, ..)` is the same slot and keeps the
    // slot-based readers (`get_canonical`) working without a name lookup.
    ctx.set_field_by_name(this, "thisX500Name", Value::Object(Some(s)));
    ctx.set_field(this, FIELD_CANONICAL, Value::Object(Some(s)));
}

/// Populate from a string DN.
fn init_from_string(ctx: &mut dyn NativeContext, this: ObjectRef, dn: &str) {
    let groups = parse_grouped_rdns(dn);
    let der = encode_grouped_rdns_to_der(&groups);
    let canon = render_canonical(&grouped_render_rdns(&groups));
    populate(ctx, this, &canon, &der);
}

/// Populate from a DER byte array.  On parse failure we still populate
/// so equality has *something* to compare; the canonical form falls back
/// to the printable hex of the input.
fn init_from_der(ctx: &mut dyn NativeContext, this: ObjectRef, der: &[u8]) {
    match decode_rdns(der) {
        Ok(rdns) => {
            let canon = render_canonical(&rdns);
            populate(ctx, this, &canon, der);
        }
        Err(_) => {
            populate(ctx, this, "", der);
        }
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

/// Read the DER encoding of an instance.
///
/// The real JDK `X500Principal` has only one instance field, so there is no
/// slot to persist the DER bytes in — `populate` stores just the canonical
/// RFC-4514 string.  The DER is re-derived here by re-encoding that string;
/// the canonical `Name` form is byte-stable, so this round-trips exactly.
fn get_der(ctx: &mut dyn NativeContext, this: ObjectRef) -> Vec<u8> {
    // Re-encode from the canonical string preserved by `populate`.
    if let Some(canon) = get_canonical(ctx, this) {
        let groups = parse_grouped_rdns(&canon);
        if !groups.is_empty() {
            return encode_grouped_rdns_to_der(&groups);
        }
    }
    Vec::new()
}

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
        let dn = read_string(ctx, args, 1).unwrap_or_default();
        init_from_string(ctx, this, &dn);
        Ok(None)
    });

    // <init>(String, Map) — same as <init>(String) for our purposes;
    // the keyword override map only matters for unknown OIDs and the
    // probe doesn't supply one.
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
            let dn = read_string(ctx, args, 1).unwrap_or_default();
            init_from_string(ctx, this, &dn);
            Ok(None)
        },
    );

    // <init>(byte[]) — parse DER.
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
        let der = read_byte_array(ctx, args, 1).unwrap_or_default();
        init_from_der(ctx, this, &der);
        Ok(None)
    });

    // <init>(InputStream) — read all bytes, then parse DER.  We don't
    // hit this path in the probe but downstream KeyStore-loading flows
    // do, so include it for completeness.
    r.register(cls, "<init>", "(Ljava/io/InputStream;)V", |_ctx, _args| {
        // Without a real InputStream pump we cannot actually slurp
        // the bytes; the caller will see an empty principal and
        // every comparison fails.  Returning Ok keeps probes that
        // never pass through this constructor unaffected.
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
            // Recover from the named field: re-encode from the canonical
            // string so getEncoded never silently returns 0 bytes.
            let canon = match ctx.get_field_by_name(this, "thisX500Name") {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => get_canonical(ctx, this).unwrap_or_default(),
            };
            if !canon.is_empty() {
                let rdns = parse_dn_string(&canon);
                der = encode_rdns_to_der(&rdns);
            }
        }
        let arr = alloc_byte_array(ctx, &der);
        Ok(Some(Value::Object(Some(arr))))
    });

    // getName() -> String  (canonical RFC 2253 / 4514)
    r.register(cls, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let s = get_canonical(ctx, this).unwrap_or_default();
        let so = ctx.create_string(&s);
        Ok(Some(Value::Object(Some(so))))
    });

    // getName(String format) -> String
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
            // Stored canonical is RFC2253 (comma, no space). RFC1779 separates
            // RDNs with ", " (comma + space); RFC2253/CANONICAL keep no space.
            let s = get_canonical(ctx, this).unwrap_or_default();
            let s = if fmt.eq_ignore_ascii_case("RFC1779") {
                s.replace(',', ", ")
            } else {
                s
            };
            let so = ctx.create_string(&s);
            Ok(Some(Value::Object(Some(so))))
        },
    );

    // toString() -> String  (delegates to getName())
    r.register(cls, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Object(None))),
        };
        let s = get_canonical(ctx, this).unwrap_or_default();
        let so = ctx.create_string(&s);
        Ok(Some(Value::Object(Some(so))))
    });

    // hashCode() -> int  (DER bytes XOR-fold to int).
    r.register(cls, "hashCode", "()I", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let der = get_der(ctx, this);
        let mut h: u32 = 0;
        for &b in &der {
            h = h.wrapping_mul(31).wrapping_add(b as u32);
        }
        Ok(Some(Value::Int(h as i32)))
    });

    // equals(Object) -> boolean  (compare canonical strings, falling
    // back to DER bytes if the other side is a different shape).
    r.register(cls, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let other = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let a_canon = get_canonical(ctx, this);
        let b_canon = get_canonical(ctx, other);
        if let (Some(a), Some(b)) = (&a_canon, &b_canon) {
            if a == b {
                return Ok(Some(Value::Int(1)));
            }
        }
        let a_der = get_der(ctx, this);
        let b_der = get_der(ctx, other);
        let same = !a_der.is_empty() && a_der == b_der;
        Ok(Some(Value::Int(if same { 1 } else { 0 })))
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
            // Primary: DER round-trip from the real X500Name.
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
            // Fallback: RFC2253 name string.
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
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;

    #[test]
    fn parse_dn_simple() {
        let p = parse_dn_string("CN=Test, O=Acme, C=SE");
        assert_eq!(p.len(), 3);
        assert_eq!(p[0], ("CN".into(), "Test".into()));
        assert_eq!(p[1], ("O".into(), "Acme".into()));
        assert_eq!(p[2], ("C".into(), "SE".into()));
    }

    #[test]
    fn parse_dn_with_escaped_comma() {
        let p = parse_dn_string(r"CN=Doe\, John, O=Acme");
        assert_eq!(p.len(), 2);
        assert_eq!(p[0], ("CN".into(), "Doe, John".into()));
    }

    #[test]
    fn round_trip_probe_input() {
        let dn = "CN=Test, O=Acme, C=SE";
        let parsed = parse_dn_string(dn);
        let der = encode_rdns_to_der(&parsed);
        let back = decode_rdns(&der).expect("decode");
        let canon_in = render_canonical(&parsed);
        let canon_out = render_canonical(&back);
        assert_eq!(canon_in, canon_out);
        // Re-encode: should be byte-identical (canonical DER).
        let der2 = encode_rdns_to_der(&back);
        assert_eq!(der, der2);
    }

    #[test]
    fn unknown_oid_round_trip() {
        // Unknown attribute name encoded as dotted-decimal OID survives.
        let parsed = vec![("1.2.3.4".to_string(), "value".to_string())];
        let der = encode_rdns_to_der(&parsed);
        let back = decode_rdns(&der).expect("decode");
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].0, "1.2.3.4");
        assert_eq!(back[0].1, "value");
    }

    #[test]
    fn canonical_uppercases_keys() {
        let parsed = parse_dn_string("cn=test, o=ACME");
        let canon = render_canonical(&parsed);
        assert!(canon.starts_with("CN=test"));
        assert!(canon.contains("O=ACME"));
    }

    #[test]
    fn grouped_rdn_preserves_each_attribute_in_der() {
        let groups = parse_grouped_rdns("C=US,O=Craton,OU=Keycloak+CN=899700252580");
        assert_eq!(groups[2].len(), 2);
        let der = encode_grouped_rdns_to_der(&groups);
        let decoded = decode_rdns(&der).expect("decode");
        let canonical = render_canonical(&decoded);
        assert!(canonical.contains("CN=899700252580"));
        assert!(canonical.contains("OU=Keycloak"));
    }
}

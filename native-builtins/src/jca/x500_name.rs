// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The `sun.security.x509.X500Name` / `RDN` / `AVA` grammar, as
//! `javax.security.auth.x500.X500Principal` exposes it.
//!
//! ## Why this is a module rather than a few string helpers
//!
//! `X500Principal` looks like a string wrapper and is not one. A DN is a list
//! of RDNs, each a set of attribute-value assertions, and each assertion
//! carries a **DER string type** that the four output formats treat
//! differently. `x500.rs` used to model a DN as `Vec<(String, String)>` — a
//! keyword and its text — and every defect this module fixes came from the
//! type that model does not have: `L6X500Sweep` measured **138 of 403 rows
//! differing** from HotSpot 25.0.4+7, and the type is why.
//!
//! ## The four formats, and what each does with the same AVA
//!
//! For `1.2.840.113549.1.9.1=alice@example.com` (emailAddress, IA5String):
//!
//! ```text
//!   RFC2253    1.2.840.113549.1.9.1=#1611616c696365406578616d706c652e636f6d
//!   RFC1779    OID.1.2.840.113549.1.9.1=alice@example.com
//!   CANONICAL  1.2.840.113549.1.9.1=#1611616c696365406578616d706c652e636f6d
//!   toString   EMAILADDRESS=alice@example.com
//! ```
//!
//! Three different spellings of one attribute, and the rules behind them are
//! independent of each other:
//!
//! * **the keyword table differs per format.** RFC 2253 defines nine keywords
//!   (§2.3); RFC 1779 defines seven and spells everything else `OID.<dotted>`;
//!   `toString` uses the JDK's own full table, which is where `DNQ`, `T` and
//!   `EMAILADDRESS` come from. A keyword the format does not know forces the
//!   dotted-OID spelling.
//! * **RFC 2253 renders a dotted-OID attribute's value as `#<DER hex>`**,
//!   whatever its type — that is RFC 2253 §2.3, and it is why `SERIALNUMBER`
//!   comes back as `2.5.4.5=#13053132333435` rather than as its text.
//! * **CANONICAL hexes any value that is not a `PrintableString` or a
//!   `UTF8String`**, which is why `DC=example` (an IA5String) is
//!   `dc=#16076578616d706c65` there and plain text in RFC 2253.
//!
//! ## Where the DER string type comes from
//!
//! Measured, per attribute, in this order:
//!
//! 1. a `#`-prefixed value is raw DER and keeps whatever tag it carries
//!    (`CN=#04024869` stays an OCTET STRING);
//! 2. `emailAddress` and `DC` are `IA5String`;
//! 3. text written with a `\XX` hex escape is `UTF8String` — the escape is how
//!    the JDK's parser records "this was not plain text", so `CN=\41lice`
//!    encodes as UTF8String where the identical `CN=Alice` is a
//!    `PrintableString`;
//! 4. otherwise `PrintableString` when every character is in X.680's printable
//!    set, and `UTF8String` when one is not.
//!
//! ## What the parser rejects
//!
//! The JDK's is a validating parser and this VM's was not: nineteen malformed
//! inputs in the sweep produced a principal here and an
//! `IllegalArgumentException("improperly specified input name: <dn>")` on
//! HotSpot. An unparseable DN that silently yields an EMPTY principal is the
//! dangerous shape — it equals nothing, matches no certificate subject, and
//! every access check against it quietly answers "no".

use std::collections::HashMap;

use super::asn1;

/// X.680's `PrintableString` character set.
fn is_printable_string_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || " '()+,-./:=?".contains(c)
}

pub const TAG_UTF8: u8 = 0x0c;
pub const TAG_PRINTABLE: u8 = 0x13;
pub const TAG_IA5: u8 = 0x16;
const TAG_T61: u8 = 0x14;
const TAG_BMP: u8 = 0x1e;
const TAG_UNIVERSAL_STRING: u8 = 0x1c;

/// True for the DER tags that carry text, i.e. the ones RFC 2253 renders as a
/// string rather than as `#<hex>`.
fn is_string_tag(tag: u8) -> bool {
    matches!(
        tag,
        TAG_UTF8 | TAG_PRINTABLE | TAG_IA5 | TAG_T61 | TAG_BMP | TAG_UNIVERSAL_STRING
    )
}

/// One attribute value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AvaValue {
    /// Text plus the DER string tag it encodes with.
    Text { text: String, tag: u8 },
    /// A value given (or decoded) as raw DER: `CN=#04024869`.
    Der(Vec<u8>),
}

/// One attribute-value assertion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ava {
    pub oid: String,
    pub value: AvaValue,
}

/// One relative distinguished name: the attributes joined by `+`.
pub type Rdn = Vec<Ava>;

/// The output formats, in the spelling `X500Principal` uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Rfc2253,
    Rfc1779,
    Canonical,
    /// `X500Name.toString()`, which `X500Principal.toString()` delegates to.
    Display,
}

// ---------------------------------------------------------------------------
// Keyword tables
// ---------------------------------------------------------------------------

/// Keywords the PARSER accepts, with the OID each names.
///
/// `sun.security.x509.AVAKeyword`'s table. A keyword outside it is an error
/// rather than a synthetic attribute — `NoSuchKeyword=x` raises on HotSpot.
const PARSE_KEYWORDS: &[(&str, &str)] = &[
    ("CN", "2.5.4.3"),
    ("SURNAME", "2.5.4.4"),
    ("SERIALNUMBER", "2.5.4.5"),
    ("C", "2.5.4.6"),
    ("L", "2.5.4.7"),
    ("S", "2.5.4.8"),
    ("ST", "2.5.4.8"),
    ("STREET", "2.5.4.9"),
    ("O", "2.5.4.10"),
    ("OU", "2.5.4.11"),
    ("T", "2.5.4.12"),
    ("TITLE", "2.5.4.12"),
    ("GIVENNAME", "2.5.4.42"),
    ("INITIALS", "2.5.4.43"),
    ("GENERATION", "2.5.4.44"),
    ("GENERATIONQUALIFIER", "2.5.4.44"),
    ("DNQ", "2.5.4.46"),
    ("DNQUALIFIER", "2.5.4.46"),
    ("IP", "1.3.6.1.4.1.42.2.11.2.1"),
    ("EMAIL", "1.2.840.113549.1.9.1"),
    ("EMAILADDRESS", "1.2.840.113549.1.9.1"),
    ("UID", "0.9.2342.19200300.100.1.1"),
    ("DC", "0.9.2342.19200300.100.1.25"),
];

/// RFC 2253 §2.3's table — the only keywords that format spells as words.
const RFC2253_KEYWORDS: &[(&str, &str)] = &[
    ("CN", "2.5.4.3"),
    ("L", "2.5.4.7"),
    ("ST", "2.5.4.8"),
    ("O", "2.5.4.10"),
    ("OU", "2.5.4.11"),
    ("C", "2.5.4.6"),
    ("STREET", "2.5.4.9"),
    ("DC", "0.9.2342.19200300.100.1.25"),
    ("UID", "0.9.2342.19200300.100.1.1"),
];

/// RFC 1779's table. Everything else is `OID.<dotted>`, INCLUDING `DC` and
/// `UID` — measured: `getName(RFC1779)` of `DC=example` is
/// `OID.0.9.2342.19200300.100.1.25=example`.
const RFC1779_KEYWORDS: &[(&str, &str)] = &[
    ("CN", "2.5.4.3"),
    ("L", "2.5.4.7"),
    ("ST", "2.5.4.8"),
    ("O", "2.5.4.10"),
    ("OU", "2.5.4.11"),
    ("C", "2.5.4.6"),
    ("STREET", "2.5.4.9"),
];

/// The spelling `toString()` uses for each OID, where it has one.
const DISPLAY_KEYWORDS: &[(&str, &str)] = &[
    ("CN", "2.5.4.3"),
    ("SURNAME", "2.5.4.4"),
    ("SERIALNUMBER", "2.5.4.5"),
    ("C", "2.5.4.6"),
    ("L", "2.5.4.7"),
    ("ST", "2.5.4.8"),
    ("STREET", "2.5.4.9"),
    ("O", "2.5.4.10"),
    ("OU", "2.5.4.11"),
    ("T", "2.5.4.12"),
    ("GIVENNAME", "2.5.4.42"),
    ("INITIALS", "2.5.4.43"),
    ("GENERATION", "2.5.4.44"),
    ("DNQ", "2.5.4.46"),
    ("IP", "1.3.6.1.4.1.42.2.11.2.1"),
    ("EMAILADDRESS", "1.2.840.113549.1.9.1"),
    ("UID", "0.9.2342.19200300.100.1.1"),
    ("DC", "0.9.2342.19200300.100.1.25"),
];

fn keyword_for(oid: &str, table: &[(&'static str, &'static str)]) -> Option<&'static str> {
    table
        .iter()
        .find(|(_, o)| *o == oid)
        .map(|(keyword, _)| *keyword)
}

/// True for text shaped like a dotted-decimal OID: at least two arcs, every
/// arc a non-empty run of digits. `1.2.3.` and `1..2` are not OIDs, and the
/// JDK rejects a DN that uses one as an attribute type.
fn is_oid_text(s: &str) -> bool {
    let mut arcs = 0;
    for arc in s.split('.') {
        if arc.is_empty() || !arc.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        arcs += 1;
    }
    arcs >= 2
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse an RFC 2253 / RFC 1779 distinguished name.
///
/// `extra_keywords` is `X500Principal(String, Map)`'s keyword map, keyed by
/// KEYWORD and valued by dotted OID — the direction its javadoc specifies.
///
/// Returns the RDNs in string order (most specific first).
pub fn parse_dn(
    input: &str,
    extra_keywords: Option<&HashMap<String, String>>,
) -> Result<Vec<Rdn>, ()> {
    let chars: Vec<char> = input.chars().collect();
    let mut rdns: Vec<Rdn> = Vec::new();
    let mut current: Rdn = Vec::new();
    let mut pos = 0usize;

    // An empty (or all-whitespace) DN is an empty name, not an error... but
    // only when it is EMPTY: `"   "` raises on HotSpot, and `""` does not.
    if input.is_empty() {
        return Ok(Vec::new());
    }

    loop {
        let (ava, next, terminator) = parse_ava(&chars, pos, extra_keywords)?;
        current.push(ava);
        pos = next;
        match terminator {
            Terminator::Plus => {}
            Terminator::Comma => {
                rdns.push(std::mem::take(&mut current));
            }
            Terminator::End => {
                rdns.push(std::mem::take(&mut current));
                break;
            }
        }
    }
    Ok(rdns)
}

enum Terminator {
    Comma,
    Plus,
    End,
}

/// Parse one attribute-value assertion starting at `pos`.
fn parse_ava(
    chars: &[char],
    mut pos: usize,
    extra_keywords: Option<&HashMap<String, String>>,
) -> Result<(Ava, usize, Terminator), ()> {
    // ---- keyword
    while pos < chars.len() && chars[pos] == ' ' {
        pos += 1;
    }
    let start = pos;
    while pos < chars.len() && chars[pos] != '=' {
        // A separator before any `=` means an assertion with no value at all
        // (`CN=Alice,CN`), which is malformed rather than an empty value.
        if chars[pos] == ',' || chars[pos] == ';' || chars[pos] == '+' {
            return Err(());
        }
        pos += 1;
    }
    if pos >= chars.len() {
        return Err(());
    }
    let keyword: String = chars[start..pos]
        .iter()
        .collect::<String>()
        .trim()
        .to_string();
    pos += 1; // consume '='
    if keyword.is_empty() {
        return Err(());
    }
    let oid = resolve_keyword(&keyword, extra_keywords)?;

    // ---- value
    while pos < chars.len() && chars[pos] == ' ' {
        pos += 1;
    }
    if pos < chars.len() && chars[pos] == '#' {
        let (der, term, next) = parse_hex_value(chars, pos + 1)?;
        return Ok((
            Ava {
                oid,
                value: AvaValue::Der(der),
            },
            next,
            term,
        ));
    }
    let (text, forced_utf8, term, next) = parse_text_value(chars, pos)?;
    let tag = value_tag(&oid, &text, forced_utf8);
    Ok((
        Ava {
            oid,
            value: AvaValue::Text { text, tag },
        },
        next,
        term,
    ))
}

/// What follows a value: a separator, the end of the input, or an error.
///
/// Unescaped spaces may precede a separator; anything else after a value has
/// ended is malformed.
fn finish(chars: &[char], pos: usize) -> Result<(Terminator, usize), ()> {
    let mut cursor = pos;
    while chars.get(cursor) == Some(&' ') {
        cursor += 1;
    }
    match chars.get(cursor) {
        None => Ok((Terminator::End, cursor)),
        Some(',') | Some(';') => Ok((Terminator::Comma, cursor + 1)),
        Some('+') => Ok((Terminator::Plus, cursor + 1)),
        Some(_) => Err(()),
    }
}

fn resolve_keyword(
    keyword: &str,
    extra_keywords: Option<&HashMap<String, String>>,
) -> Result<String, ()> {
    let upper = keyword.to_ascii_uppercase();
    if let Some(map) = extra_keywords {
        if let Some(oid) = map.get(&upper).or_else(|| map.get(keyword)) {
            if !is_oid_text(oid) {
                return Err(());
            }
            return Ok(oid.clone());
        }
    }
    if let Some((_, oid)) = PARSE_KEYWORDS.iter().find(|(k, _)| *k == upper) {
        return Ok((*oid).to_string());
    }
    // A dotted-decimal attribute type is legal, and `1.2.3.` / `1..2` are not
    // dotted-decimal.
    if is_oid_text(keyword) {
        return Ok(keyword.to_string());
    }
    Err(())
}

/// The `#`-prefixed form: an even run of hex digits that is a complete,
/// well-formed DER value and nothing more.
fn parse_hex_value(chars: &[char], mut pos: usize) -> Result<(Vec<u8>, Terminator, usize), ()> {
    let start = pos;
    while pos < chars.len() && chars[pos].is_ascii_hexdigit() {
        pos += 1;
    }
    let digits: String = chars[start..pos].iter().collect();
    let (term, next) = finish(chars, pos)?;
    if digits.is_empty() || digits.len() % 2 != 0 {
        return Err(());
    }
    let mut bytes = Vec::with_capacity(digits.len() / 2);
    let raw: Vec<char> = digits.chars().collect();
    for pair in raw.chunks(2) {
        let hi = pair[0].to_digit(16).ok_or(())?;
        let lo = pair[1].to_digit(16).ok_or(())?;
        bytes.push(((hi << 4) | lo) as u8);
    }
    // `CN=#0402` announces two content bytes and supplies none: the JDK's
    // parser reads the DER and rejects a truncated one.
    let (_, header, content_len, total) = asn1::read_header(&bytes).map_err(|_| ())?;
    if header + content_len != bytes.len() || total != bytes.len() {
        return Err(());
    }
    Ok((bytes, term, next))
}

/// Read to the end of an attribute value, resolving escapes.
///
/// Returns the text, whether a `\XX` hex escape appeared (which pins the DER
/// type to `UTF8String`), the position after the terminator, and which
/// terminator it was.
fn parse_text_value(
    chars: &[char],
    mut pos: usize,
) -> Result<(String, bool, Terminator, usize), ()> {
    let mut out = String::new();
    let mut forced_utf8 = false;
    let mut pending_utf8: Vec<u8> = Vec::new();
    let mut quoted = false;
    let mut closed_quote = false;
    // Track where the last non-space, non-escaped character ended so an
    // unescaped trailing run of spaces can be dropped the way the JDK's
    // parser drops it.
    let mut trailing_spaces = 0usize;

    if pos < chars.len() && chars[pos] == '"' {
        quoted = true;
        pos += 1;
    }

    while pos < chars.len() {
        let c = chars[pos];
        if c == '\\' {
            let h1 = chars.get(pos + 1).copied().ok_or(())?;
            if let Some(d1) = h1.to_digit(16) {
                // Either a hex pair, or a single-character escape of a digit
                // (`\3` is not legal; the JDK requires two hex digits).
                let h2 = chars.get(pos + 2).copied().ok_or(())?;
                let d2 = h2.to_digit(16).ok_or(())?;
                pending_utf8.push(((d1 << 4) | d2) as u8);
                forced_utf8 = true;
                pos += 3;
                trailing_spaces = 0;
                continue;
            }
            flush_utf8(&mut out, &mut pending_utf8)?;
            // A single-character escape. The JDK accepts the RFC 2253 special
            // set and a space; `\q` is malformed.
            if !",+=\"\\<>;# ".contains(h1) {
                return Err(());
            }
            out.push(h1);
            trailing_spaces = 0;
            pos += 2;
            continue;
        }
        flush_utf8(&mut out, &mut pending_utf8)?;
        if quoted && !closed_quote {
            if c == '"' {
                closed_quote = true;
                pos += 1;
                continue;
            }
            out.push(c);
            pos += 1;
            continue;
        }
        if c == ',' || c == ';' || c == '+' {
            break;
        }
        if c == '"' {
            // A bare quote inside an unquoted value is malformed.
            return Err(());
        }
        if c == ' ' {
            trailing_spaces += 1;
        } else {
            trailing_spaces = 0;
        }
        out.push(c);
        pos += 1;
    }
    flush_utf8(&mut out, &mut pending_utf8)?;
    if quoted && !closed_quote {
        return Err(());
    }
    for _ in 0..trailing_spaces {
        out.pop();
    }
    let (term, next) = finish(chars, pos)?;
    Ok((out, forced_utf8, term, next))
}

/// Decode the bytes a run of `\XX` escapes accumulated. They are UTF-8: the
/// sweep's `CN=Ren\C3\A9` is one two-byte character, not two.
fn flush_utf8(out: &mut String, pending: &mut Vec<u8>) -> Result<(), ()> {
    if pending.is_empty() {
        return Ok(());
    }
    let text = String::from_utf8(std::mem::take(pending)).map_err(|_| ())?;
    out.push_str(&text);
    Ok(())
}

/// The DER string type for a parsed text value.
fn value_tag(oid: &str, text: &str, forced_utf8: bool) -> u8 {
    if oid == "1.2.840.113549.1.9.1" || oid == "0.9.2342.19200300.100.1.25" {
        return TAG_IA5;
    }
    if forced_utf8 || !text.chars().all(is_printable_string_char) {
        return TAG_UTF8;
    }
    TAG_PRINTABLE
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn hex_of(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2 + 1);
    s.push('#');
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// The DER a text AVA value encodes to, which `#<hex>` renders.
fn der_of_value(value: &AvaValue) -> Vec<u8> {
    match value {
        AvaValue::Der(bytes) => bytes.clone(),
        AvaValue::Text { text, tag } => asn1::encode_tlv(*tag, text.as_bytes()),
    }
}

/// RFC 2253 §2.4 escaping.
fn escape_2253(value: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = value.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        let last = i + 1 == chars.len();
        match c {
            ',' | '+' | '=' | '"' | '\\' | '<' | '>' | ';' => {
                out.push('\\');
                out.push(*c);
            }
            '#' if i == 0 => {
                out.push('\\');
                out.push('#');
            }
            ' ' if i == 0 || last => {
                out.push('\\');
                out.push(' ');
            }
            _ => out.push(*c),
        }
    }
    out
}

/// RFC 1779 quotes a value rather than escaping it, and `toString()` follows
/// the same rule. Measured: a tab does NOT trigger quoting and two adjacent
/// spaces do.
fn quote_1779(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut needs_quotes = chars.iter().any(|c| ",=\n+<>#;\\\"".contains(*c));
    if !needs_quotes {
        if matches!(chars.first(), Some(' ')) || matches!(chars.last(), Some(' ')) {
            needs_quotes = true;
        }
    }
    if !needs_quotes {
        needs_quotes = chars.windows(2).any(|w| w[0] == ' ' && w[1] == ' ');
    }
    if !needs_quotes {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in chars {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// CANONICAL folds case, trims, and collapses runs of SPACES (not of other
/// whitespace: a tab survives intact).
fn canonical_text(value: &str) -> String {
    use unicode_normalization::UnicodeNormalization;

    let lowered = value.to_lowercase();
    let trimmed = lowered.trim_matches(' ');
    let mut out = String::with_capacity(trimmed.len());
    let mut previous_space = false;
    for c in trimmed.chars() {
        if c == ' ' {
            if !previous_space {
                out.push(' ');
            }
            previous_space = true;
        } else {
            previous_space = false;
            out.push(c);
        }
    }
    // NFKD, the last step of `AVA.toRFC2253CanonicalString`. It is invisible
    // in ASCII and decides equality outside it: HotSpot's canonical form of
    // `CN=René` ends `65 cc 81` (`e` + COMBINING ACUTE) where the composed
    // input is `c3 a9`. Two principals spelling the same name in NFC and NFD
    // are the same principal, and without this they were neither equal nor
    // equal-hashed.
    out.nfkd().collect()
}

fn render_ava(ava: &Ava, format: Format) -> String {
    let table = match format {
        Format::Rfc2253 | Format::Canonical => RFC2253_KEYWORDS,
        Format::Rfc1779 => RFC1779_KEYWORDS,
        Format::Display => DISPLAY_KEYWORDS,
    };
    let keyword = keyword_for(&ava.oid, table);
    let text = match &ava.value {
        AvaValue::Text { text, tag } => Some((text.clone(), *tag)),
        AvaValue::Der(bytes) => match asn1::read_header(bytes) {
            Ok((tag, header, len, _)) if is_string_tag(tag) => {
                asn1::read_directory_string(tag, &bytes[header..header + len]).map(|s| (s, tag))
            }
            _ => None,
        },
    };

    match format {
        Format::Rfc2253 => match (keyword, &text) {
            // RFC 2253 §2.3: a dotted-OID type takes the hex form regardless
            // of the value's own type.
            (None, _) => format!("{}={}", ava.oid, hex_of(&der_of_value(&ava.value))),
            (Some(k), Some((t, _))) => format!("{k}={}", escape_2253(t)),
            (Some(k), None) => format!("{k}={}", hex_of(&der_of_value(&ava.value))),
        },
        Format::Canonical => {
            let hexed = match &text {
                // CANONICAL keeps text only for the two types RFC 2253 §2.4
                // names; an IA5String `DC` is hex here and text in RFC2253.
                Some((_, tag)) if *tag == TAG_PRINTABLE || *tag == TAG_UTF8 => false,
                Some(_) => true,
                None => true,
            };
            match (keyword, &text) {
                (Some(k), Some((t, _))) if !hexed => {
                    format!("{}={}", k.to_lowercase(), escape_2253(&canonical_text(t)))
                }
                (Some(k), _) => {
                    format!("{}={}", k.to_lowercase(), hex_of(&der_of_value(&ava.value)))
                }
                (None, _) => format!("{}={}", ava.oid, hex_of(&der_of_value(&ava.value))),
            }
        }
        Format::Rfc1779 | Format::Display => {
            let name = match keyword {
                Some(k) => k.to_string(),
                None if format == Format::Rfc1779 => format!("OID.{}", ava.oid),
                None => format!("OID.{}", ava.oid),
            };
            match &text {
                Some((t, _)) => format!("{name}={}", quote_1779(t)),
                None => format!("{name}={}", hex_of(&der_of_value(&ava.value))),
            }
        }
    }
}

/// Render with `getName(String, Map)`'s caller-supplied OID -> keyword map.
///
/// A mapped attribute stops being "an OID this format has no word for", so it
/// renders as `KEYWORD=text` where it would otherwise have been
/// `<dotted>=#<hex>`. Measured: `getName(RFC2253, {"1.3.6.1.4.1.99999.1":
/// "MYOID"})` of `1.3.6.1.4.1.99999.1=x` is `MYOID=x`, and the same call with
/// an empty map is `1.3.6.1.4.1.99999.1=#130178`.
pub fn render_with_oid_map(
    rdns: &[Rdn],
    format: Format,
    oid_map: &HashMap<String, String>,
) -> String {
    let mapped: Vec<Rdn> = rdns
        .iter()
        .map(|rdn| {
            rdn.iter()
                .map(|ava| Ava {
                    oid: ava.oid.clone(),
                    value: ava.value.clone(),
                })
                .collect()
        })
        .collect();
    let (rdn_sep, ava_sep) = match format {
        Format::Rfc2253 | Format::Canonical => (",", "+"),
        Format::Rfc1779 | Format::Display => (", ", " + "),
    };
    let mut parts: Vec<String> = Vec::with_capacity(mapped.len());
    for rdn in &mapped {
        let avas: Vec<String> = rdn
            .iter()
            .map(|ava| match oid_map.get(&ava.oid.to_ascii_uppercase()) {
                Some(keyword) => {
                    let text = match &ava.value {
                        AvaValue::Text { text, .. } => Some(text.clone()),
                        AvaValue::Der(bytes) => match asn1::read_header(bytes) {
                            Ok((tag, header, len, _)) if is_string_tag(tag) => {
                                asn1::read_directory_string(tag, &bytes[header..header + len])
                            }
                            _ => None,
                        },
                    };
                    match text {
                        Some(t) if format == Format::Rfc2253 => {
                            format!("{keyword}={}", escape_2253(&t))
                        }
                        Some(t) => format!("{keyword}={}", quote_1779(&t)),
                        None => format!("{keyword}={}", hex_of(&der_of_value(&ava.value))),
                    }
                }
                None => render_ava(ava, format),
            })
            .collect();
        parts.push(avas.join(ava_sep));
    }
    parts.join(rdn_sep)
}

/// Render a whole DN.
pub fn render(rdns: &[Rdn], format: Format) -> String {
    let (rdn_sep, ava_sep) = match format {
        Format::Rfc2253 | Format::Canonical => (",", "+"),
        Format::Rfc1779 | Format::Display => (", ", " + "),
    };
    let mut parts: Vec<String> = Vec::with_capacity(rdns.len());
    for rdn in rdns {
        let mut avas: Vec<String> = rdn.iter().map(|a| render_ava(a, format)).collect();
        if format == Format::Canonical {
            // Equality is defined on this string, and a multi-valued RDN is a
            // SET: `CN=Alice+OU=Eng` and `OU=Eng+CN=Alice` are the same name.
            avas.sort();
        }
        parts.push(avas.join(ava_sep));
    }
    parts.join(rdn_sep)
}

// ---------------------------------------------------------------------------
// DER
// ---------------------------------------------------------------------------

/// Encode to an X.500 `Name`. The DER lists RDNs most-specific-LAST, which is
/// the reverse of the string order.
pub fn encode_der(rdns: &[Rdn]) -> Vec<u8> {
    let mut seq_inner = Vec::new();
    for rdn in rdns.iter().rev() {
        let mut attrs: Vec<Vec<u8>> = Vec::with_capacity(rdn.len());
        for ava in rdn {
            let mut inner = Vec::new();
            match asn1::encode_oid(&ava.oid) {
                Ok(oid_der) => inner.extend_from_slice(&oid_der),
                Err(()) => continue,
            }
            inner.extend_from_slice(&der_of_value(&ava.value));
            attrs.push(asn1::encode_sequence(&inner));
        }
        // DER orders SET elements by their encodings.
        attrs.sort();
        let rdn_inner: Vec<u8> = attrs.into_iter().flatten().collect();
        seq_inner.extend_from_slice(&asn1::encode_set(&rdn_inner));
    }
    asn1::encode_sequence(&seq_inner)
}

/// Decode an X.500 `Name`, in string order (most specific first).
pub fn decode_der(der: &[u8]) -> Result<Vec<Rdn>, ()> {
    let (tag, header, content_len, total) = asn1::read_header(der).map_err(|_| ())?;
    if tag != asn1::TAG_SEQUENCE || total != der.len() {
        return Err(());
    }
    let content = &der[header..header + content_len];
    let mut rdns: Vec<Rdn> = Vec::new();
    let mut pos = 0usize;
    while pos < content.len() {
        let (set_tag, set_header, set_len, set_total) =
            asn1::read_header(&content[pos..]).map_err(|_| ())?;
        if set_tag != asn1::TAG_SET {
            return Err(());
        }
        let set_content = &content[pos + set_header..pos + set_header + set_len];
        let mut rdn: Rdn = Vec::new();
        let mut inner_pos = 0usize;
        while inner_pos < set_content.len() {
            let (seq_tag, seq_header, seq_len, seq_total) =
                asn1::read_header(&set_content[inner_pos..]).map_err(|_| ())?;
            if seq_tag != asn1::TAG_SEQUENCE {
                return Err(());
            }
            let body = &set_content[inner_pos + seq_header..inner_pos + seq_header + seq_len];
            let (oid_tag, oid_header, oid_len, oid_total) =
                asn1::read_header(body).map_err(|_| ())?;
            if oid_tag != asn1::TAG_OID {
                return Err(());
            }
            let oid = asn1::read_oid(&body[oid_header..oid_header + oid_len]).map_err(|_| ())?;
            let value_bytes = &body[oid_total..];
            let (value_tag, value_header, value_len, _) =
                asn1::read_header(value_bytes).map_err(|_| ())?;
            let value = if is_string_tag(value_tag) {
                match asn1::read_directory_string(
                    value_tag,
                    &value_bytes[value_header..value_header + value_len],
                ) {
                    Some(text) => AvaValue::Text {
                        text,
                        tag: value_tag,
                    },
                    None => AvaValue::Der(value_bytes.to_vec()),
                }
            } else {
                AvaValue::Der(value_bytes.to_vec())
            };
            rdn.push(Ava { oid, value });
            inner_pos += seq_total;
        }
        rdns.push(rdn);
        pos += set_total;
    }
    rdns.reverse();
    Ok(rdns)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn dn(input: &str) -> Vec<Rdn> {
        parse_dn(input, None).expect("parses")
    }

    #[test]
    fn a_dotted_oid_attribute_renders_hex_in_rfc2253_and_text_in_rfc1779() {
        // MEASURED on HotSpot 25.0.4+7, `L6X500Sweep` rows 244-248.
        let name = dn("SERIALNUMBER=12345,CN=Alice");
        assert_eq!(
            render(&name, Format::Rfc2253),
            "2.5.4.5=#13053132333435,CN=Alice"
        );
        assert_eq!(
            render(&name, Format::Rfc1779),
            "OID.2.5.4.5=12345, CN=Alice"
        );
        assert_eq!(
            render(&name, Format::Display),
            "SERIALNUMBER=12345, CN=Alice"
        );
    }

    #[test]
    fn an_ia5string_is_text_in_rfc2253_and_hex_in_canonical() {
        // `DC` is the case that separates the two rules: rows 218 and 220.
        let name = dn("DC=example,DC=com");
        assert_eq!(render(&name, Format::Rfc2253), "DC=example,DC=com");
        assert_eq!(
            render(&name, Format::Canonical),
            "dc=#16076578616d706c65,dc=#1603636f6d"
        );
    }

    #[test]
    fn a_hex_escape_pins_the_value_to_utf8string() {
        // `CN=\41lice` and `CN=Alice` are the same TEXT and not the same DER:
        // rows 163-169.
        let escaped = dn("CN=\\41lice");
        let plain = dn("CN=Alice");
        assert_eq!(render(&escaped, Format::Rfc2253), "CN=Alice");
        assert_eq!(
            hex(&encode_der(&escaped)),
            "3010310e300c06035504030c05416c696365"
        );
        assert_eq!(
            hex(&encode_der(&plain)),
            "3010310e300c06035504031305416c696365"
        );
    }

    #[test]
    fn utf8_hex_escapes_join_into_one_character() {
        let name = dn("CN=Ren\\C3\\A9");
        assert_eq!(render(&name, Format::Rfc2253), "CN=Ren\u{e9}");
        assert_eq!(
            hex(&encode_der(&name)),
            "3010310e300c06035504030c0552656ec3a9"
        );
    }

    #[test]
    fn a_hash_value_is_raw_der_and_stays_raw() {
        let name = dn("CN=#04024869");
        assert_eq!(render(&name, Format::Rfc2253), "CN=#04024869");
        assert_eq!(render(&name, Format::Rfc1779), "CN=#04024869");
        assert_eq!(hex(&encode_der(&name)), "300d310b3009060355040304024869");
    }

    #[test]
    fn a_quoted_value_keeps_its_comma() {
        let name = dn("CN=\"Smith, Alice\",O=Example");
        assert_eq!(
            render(&name, Format::Rfc2253),
            "CN=Smith\\, Alice,O=Example"
        );
        assert_eq!(
            render(&name, Format::Rfc1779),
            "CN=\"Smith, Alice\", O=Example"
        );
    }

    #[test]
    fn a_semicolon_separates_rdns() {
        let name = dn("CN=Alice;O=Example");
        assert_eq!(render(&name, Format::Rfc2253), "CN=Alice,O=Example");
    }

    #[test]
    fn an_escaped_trailing_space_survives_every_format_but_canonical() {
        let name = dn("CN=Alice\\ ");
        assert_eq!(render(&name, Format::Rfc2253), "CN=Alice\\ ");
        assert_eq!(render(&name, Format::Rfc1779), "CN=\"Alice \"");
        assert_eq!(render(&name, Format::Canonical), "cn=alice");
        assert_eq!(
            hex(&encode_der(&name)),
            "3011310f300d06035504031306416c69636520"
        );
    }

    #[test]
    fn canonical_collapses_spaces_but_not_tabs() {
        assert_eq!(render(&dn("CN=a    b"), Format::Canonical), "cn=a b");
        assert_eq!(render(&dn("CN=a\tb"), Format::Canonical), "cn=a\tb");
        assert_eq!(render(&dn("CN=a\tb"), Format::Rfc1779), "CN=a\tb");
    }

    #[test]
    fn the_malformed_names_the_jdk_rejects_are_rejected() {
        for bad in [
            "CN",
            "=Alice",
            "CN=Alice,",
            ",CN=Alice",
            "CN=Alice,,O=x",
            "CN=Alice+",
            "+CN=Alice",
            "CN=a\\",
            "CN=#0",
            "CN=#zz",
            "CN=#0402",
            "NoSuchKeyword=x",
            "1.2.3.=x",
            "1..2=x",
            "CN=\"unterminated",
            "CN=a\\q",
            "CN=Alice,CN",
            "   ",
        ] {
            assert!(parse_dn(bad, None).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn an_equals_inside_a_value_is_kept_and_escaped_on_the_way_out() {
        // `CN=Alice O=x` is NOT malformed: the JDK reads the whole tail as the
        // value and escapes the `=` when it renders it (row 344). This one was
        // in the sweep's "bad" list and is the one member of it that parses.
        let name = dn("CN=Alice O=x");
        assert_eq!(render(&name, Format::Rfc2253), "CN=Alice O\\=x");
    }

    #[test]
    fn an_empty_name_is_legal_and_encodes_to_an_empty_sequence() {
        let name = parse_dn("", None).expect("empty parses");
        assert!(name.is_empty());
        assert_eq!(render(&name, Format::Rfc2253), "");
        assert_eq!(hex(&encode_der(&name)), "3000");
    }

    #[test]
    fn a_multi_valued_rdn_keeps_both_attributes_and_sorts_only_in_canonical() {
        let one = dn("CN=Alice+OU=Eng,O=Example");
        let other = dn("OU=Eng+CN=Alice,O=Example");
        assert_eq!(render(&one, Format::Rfc2253), "CN=Alice+OU=Eng,O=Example");
        assert_eq!(
            render(&one, Format::Rfc1779),
            "CN=Alice + OU=Eng, O=Example"
        );
        assert_eq!(
            render(&one, Format::Canonical),
            render(&other, Format::Canonical)
        );
        assert_eq!(hex(&encode_der(&one)), "302e3110300e060355040a13074578616d706c65311a300a060355040b1303456e67300c06035504031305416c696365");
    }

    #[test]
    fn the_der_round_trips_through_decode() {
        for input in [
            "CN=Alice",
            "CN=Alice,OU=Eng,O=Example,C=US",
            "1.2.840.113549.1.9.1=alice@example.com,CN=Alice",
            "CN=#04024869",
            "CN=Smith\\, Alice,O=Example",
            "DC=example,DC=com",
        ] {
            let parsed = dn(input);
            let der = encode_der(&parsed);
            let back = decode_der(&der).expect("decodes");
            assert_eq!(
                render(&parsed, Format::Rfc2253),
                render(&back, Format::Rfc2253),
                "round trip of {input:?}"
            );
            assert_eq!(hex(&der), hex(&encode_der(&back)));
        }
    }

    #[test]
    fn the_keyword_map_is_keyed_by_keyword_not_by_oid() {
        // `X500Principal(String, Map)`'s javadoc: keyword -> OID. The sweep
        // passes it the other way round and HotSpot raises, which is the row
        // this asserts.
        let mut backwards = HashMap::new();
        backwards.insert("1.3.6.1.4.1.99999.1".to_string(), "MYOID".to_string());
        assert!(parse_dn("MYOID=x", Some(&backwards)).is_err());

        let mut correct = HashMap::new();
        correct.insert("MYOID".to_string(), "1.3.6.1.4.1.99999.1".to_string());
        let name = parse_dn("MYOID=x", Some(&correct)).expect("parses");
        assert_eq!(
            render(&name, Format::Rfc2253),
            "1.3.6.1.4.1.99999.1=#130178"
        );
    }
}

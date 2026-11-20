use super::*;

/// Round 19 (nio19): a request of two or more chars never ends on the high
/// half of a pair whose low half is next; a one-char request may.
#[test]
fn r19_stream_decoder_does_not_split_a_pair_for_a_multi_char_read() {
    let (hi, lo) = (0xD83Du16, 0xDE00u16);
    assert!(ends_inside_a_pair(
        &[b'l' as u16, b'o' as u16, hi],
        Some(lo),
        3
    ));
    assert!(!ends_inside_a_pair(
        &[b'l' as u16, b'o' as u16, hi],
        Some(b'x' as u16),
        3
    ));
    assert!(!ends_inside_a_pair(
        &[b'l' as u16, b'o' as u16, hi],
        None,
        3
    ));
    assert!(
        !ends_inside_a_pair(&[hi], Some(lo), 1),
        "one char: read0 splits"
    );
    assert!(
        !ends_inside_a_pair(&[hi, lo], Some(hi), 2),
        "ends on a whole pair"
    );
}
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn utf8_complete_prefix_ascii() {
    assert_eq!(split_complete_prefix("UTF-8", b"abc"), 3);
}

#[test]
fn utf8_complete_prefix_truncates_partial_2byte() {
    // 0xC2 begins a 2-byte sequence; on its own, incomplete.
    assert_eq!(split_complete_prefix("UTF-8", &[b'a', 0xC2]), 1);
}

#[test]
fn utf8_complete_prefix_truncates_partial_4byte() {
    // 0xF0 0x9F = first two bytes of a 4-byte sequence.
    let data = &[b'a', 0xF0, 0x9F];
    assert_eq!(split_complete_prefix("UTF-8", data), 1);
}

#[test]
fn utf8_complete_prefix_full_multibyte() {
    // Full 3-byte sequence: 0xE4 0xB8 0xAD = 中.
    let data = &[b'a', 0xE4, 0xB8, 0xAD];
    assert_eq!(split_complete_prefix("UTF-8", data), 4);
}

#[test]
fn utf16_split_even() {
    assert_eq!(split_complete_prefix("UTF-16BE", &[0, 0x41, 0, 0x42]), 4);
    assert_eq!(split_complete_prefix("UTF-16BE", &[0, 0x41, 0]), 2);
}

#[test]
fn utf32_split_quad() {
    assert_eq!(split_complete_prefix("UTF-32BE", &[0, 0, 0, 0x41, 0, 0]), 4);
}

#[test]
fn normalize_supported_accepts_known_aliases() {
    assert_eq!(normalize_supported("utf-8").as_deref(), Some("UTF-8"));
    assert_eq!(normalize_supported("Latin1").as_deref(), Some("ISO-8859-1"));
    assert_eq!(
        normalize_supported("Cp1252").as_deref(),
        Some("windows-1252")
    );
}

#[test]
fn normalize_supported_rejects_unknown_name() {
    // Genuinely unknown name → None (caller throws UnsupportedEncodingException)
    // instead of the old silent UTF-8 fallback.
    assert_eq!(normalize_supported("NoSuchCharset-42"), None);
}

#[test]
fn normalize_supported_accepts_engine_supported_charsets() {
    // These were rejected by the old stale private alias table even
    // though the engine decodes them (IBM850 hand-written, the CJK
    // families via encoding_rs) — the shared table accepts them.
    assert_eq!(
        normalize_supported("Shift_JIS").as_deref(),
        Some("Shift_JIS")
    );
    assert_eq!(normalize_supported("EUC-JP").as_deref(), Some("EUC-JP"));
    assert_eq!(normalize_supported("ibm850").as_deref(), Some("IBM850"));
}

#[test]
fn normalize_supported_rejects_canonical_but_codecless_charsets() {
    // KOI8-U canonicalizes in the shared table but the engine has no
    // codec for it — the engine probe must still reject the name.
    assert_eq!(normalize_supported("KOI8-U"), None);
}

// --- PropertyResourceBundleCharset decoder (Bug #19A) ---

fn fresh() -> PropState {
    PropState {
        strict: false,
        fell_back: false,
    }
}

#[test]
fn prop_decoder_falls_back_to_iso_on_invalid_utf8() {
    // "Umlaut: äöü" stored as ISO-8859-1: the umlaut bytes E4 F6 FC are not
    // valid UTF-8, so the JDK falls back to ISO-8859-1 → äöü (not '?').
    let mut bytes = b"Umlaut: ".to_vec();
    bytes.extend_from_slice(&[0xE4, 0xF6, 0xFC]);
    let (chars, rest, p) = decode_prop(&bytes, true, fresh());
    assert_eq!(
        String::from_utf16(&chars).unwrap(),
        "Umlaut: \u{00e4}\u{00f6}\u{00fc}"
    );
    assert!(rest.is_empty());
    assert!(p.unwrap().fell_back, "stream should be sticky-ISO now");
}

#[test]
fn prop_decoder_keeps_valid_utf8() {
    // The same text stored as UTF-8 must stay UTF-8 (no spurious fallback).
    let bytes = "Umlaut: \u{00e4}\u{00f6}\u{00fc}".as_bytes().to_vec();
    let (chars, rest, p) = decode_prop(&bytes, true, fresh());
    assert_eq!(
        String::from_utf16(&chars).unwrap(),
        "Umlaut: \u{00e4}\u{00f6}\u{00fc}"
    );
    assert!(rest.is_empty());
    assert!(!p.unwrap().fell_back);
}

#[test]
fn prop_decoder_carries_truncated_utf8_midstream() {
    // "ab" + the first two bytes of the 3-byte sequence for 中 (E4 B8): a
    // truncated tail mid-stream is carried, NOT treated as an error/fallback.
    let bytes = vec![b'a', b'b', 0xE4, 0xB8];
    let (chars, rest, p) = decode_prop(&bytes, false, fresh());
    assert_eq!(String::from_utf16(&chars).unwrap(), "ab");
    assert_eq!(rest, vec![0xE4, 0xB8]);
    assert!(!p.unwrap().fell_back);
}

#[test]
fn prop_decoder_truncated_utf8_at_eof_falls_back() {
    // The same truncated tail at EOF cannot complete → malformed → ISO.
    let bytes = vec![b'a', b'b', 0xE4, 0xB8];
    let (chars, rest, p) = decode_prop(&bytes, true, fresh());
    // ISO-8859-1: 4 bytes -> 4 chars.
    assert_eq!(chars.len(), 4);
    assert_eq!(chars[0], b'a' as u16);
    assert_eq!(chars[2], 0xE4);
    assert!(rest.is_empty());
    assert!(p.unwrap().fell_back);
}

#[test]
fn prop_decoder_sticky_iso_after_fallback() {
    // Once fallen back, valid-UTF-8 bytes still decode as ISO-8859-1.
    let mut p = fresh();
    p.fell_back = true;
    let bytes = "中".as_bytes().to_vec(); // E4 B8 AD (valid UTF-8)
    let (chars, rest, p2) = decode_prop(&bytes, false, p);
    assert_eq!(chars.len(), 3, "ISO decodes each byte separately");
    assert!(rest.is_empty());
    assert!(p2.unwrap().fell_back);
}

// --- AUDIT 2026-07-26 (native-io-audit) regressions ---
//
// These drive the real natives through `MockNativeContext`, whose
// `read([BII)I` is scripted to behave like a genuine `InputStream`
// (`script_input_stream`). `sd_table` is a process-global keyed by
// identity hash, and the mock hands out low, reusable pointers, so these
// tests serialize on `confine_test_lock` and clear their own key first.

use crate::test_support::{confine_test_lock, MockNativeContext};

/// Build a StreamDecoder-shaped mock object over `bytes`, with a fresh
/// `sd_table` entry for `charset`.
fn decoder_over(ctx: &mut MockNativeContext, bytes: &[u8], charset: &str) -> ObjectRef {
    let is = ctx.alloc_object(1);
    let this = ctx.alloc_object(4);
    ctx.set_field_by_name(this, "in", Value::Object(Some(is)));
    ctx.script_input_stream(bytes);
    let key = sd_key(ctx, this);
    let mut t = sd_table().lock();
    t.remove(&key);
    t.insert(key, SdState::empty(charset, crate::io_side_owner(this)));
    drop(t);
    this
}

fn drain_reader(ctx: &mut MockNativeContext, this: ObjectRef) -> String {
    let mut out = String::new();
    for _ in 0..64 {
        match native_sd_read(ctx, &[Value::Object(Some(this))]) {
            Ok(Some(Value::Int(-1))) => break,
            Ok(Some(Value::Int(c))) => out.push(char::from_u32(c as u32).unwrap_or('\u{fffd}')),
            _ => break,
        }
    }
    out
}

/// The headline defect: `decode_into` truncated its decoded chars to
/// `len` and dropped the surplus, so single-char `Reader.read()` over a
/// stream containing a multi-byte character silently lost every character
/// decoded alongside it. "éabcd" used to read back as "éd".
#[test]
fn audit_read_single_char_does_not_drop_surplus_after_multibyte() {
    let _g = confine_test_lock().lock();
    let mut ctx = MockNativeContext::new();
    // C3 A9 = 'é', then plain ASCII.
    let this = decoder_over(&mut ctx, "éabcd".as_bytes(), "UTF-8");
    let got = drain_reader(&mut ctx, this);
    assert_eq!(
        got, "éabcd",
        "every character must survive; the surplus decoded alongside \
         the multi-byte char used to be discarded"
    );
    sd_table().lock().remove(&sd_key(&ctx, this));
}

/// Two multi-byte characters back to back — exercises the carry AND the
/// surplus path together.
#[test]
fn audit_read_single_char_handles_consecutive_multibyte() {
    let _g = confine_test_lock().lock();
    let mut ctx = MockNativeContext::new();
    let this = decoder_over(&mut ctx, "中文ok".as_bytes(), "UTF-8");
    let got = drain_reader(&mut ctx, this);
    assert_eq!(got, "中文ok");
    sd_table().lock().remove(&sd_key(&ctx, this));
}

/// `read()` must consume the read-ahead queue that `read(char[],int,int)`
/// fills, rather than pulling fresh bytes past it.
#[test]
fn audit_read_single_char_drains_pending_before_refilling() {
    let _g = confine_test_lock().lock();
    let mut ctx = MockNativeContext::new();
    let this = decoder_over(&mut ctx, b"abcdef", "UTF-8");
    // Ask for 2 chars; the bulk path reads ahead and parks the rest.
    let out = ctx.new_array(ArrayElementType::Char, 2);
    let n = native_sd_read_chars(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(out)),
            Value::Int(0),
            Value::Int(2),
        ],
    );
    assert!(matches!(n, Ok(Some(Value::Int(2)))));
    let queued = sd_table()
        .lock()
        .get(&sd_key(&ctx, this))
        .map(|s| s.pending.len())
        .unwrap_or(0);
    assert!(queued > 0, "bulk read should have parked read-ahead chars");
    // The single-char reader must continue from 'c', not skip the queue.
    let rest = drain_reader(&mut ctx, this);
    assert_eq!(rest, "cdef");
    sd_table().lock().remove(&sd_key(&ctx, this));
}

/// `ready()` must report true while fully decoded chars sit in `pending`,
/// even though `carry` is empty and the stream has nothing left.
#[test]
fn audit_ready_accounts_for_decoded_read_ahead() {
    let _g = confine_test_lock().lock();
    let mut ctx = MockNativeContext::new();
    let this = decoder_over(&mut ctx, b"abcdef", "UTF-8");
    let out = ctx.new_array(ArrayElementType::Char, 1);
    let _ = native_sd_read_chars(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(out)),
            Value::Int(0),
            Value::Int(1),
        ],
    );
    // Stream is drained; only `pending` holds the remaining chars.
    let r = native_sd_ready(&mut ctx, &[Value::Object(Some(this))]);
    assert!(
        matches!(r, Ok(Some(Value::Int(1)))),
        "ready() must be true with decoded chars buffered"
    );
    sd_table().lock().remove(&sd_key(&ctx, this));
}

/// gc-common w13-e: the `ch` (channel) refill's buffer is caller-sized and
/// now reclaims before its OOME. The reclaim MOVES every object
/// (`relocate_on_reclaim`): `this`, the destination and the channel, so a
/// stale local panics "invalid ObjectRef". The first `try_new_array` (the
/// 4096-char read-ahead scratch) is let through; the channel buffer's
/// backing array is refused once. Runs at its own address base because
/// `sd_table` is process-wide and keyed by identity hash.
#[test]
fn w13e_channel_refill_reclaims_and_decodes_after_the_move() {
    let _g = confine_test_lock().lock();
    let mut ctx = MockNativeContext::with_address_base(0x5E15_0000);
    ctx.relocate_on_reclaim();
    ctx.script_channel(b"chan");
    let ch = ctx.alloc_object(0);
    let this = ctx.alloc_object(4);
    ctx.set_field_by_name(this, "ch", Value::Object(Some(ch)));
    let key = sd_key(&ctx, this);
    sd_table().lock().remove(&key);
    let out = ctx.new_array(ArrayElementType::Char, 4);
    let out_root = ctx.add_global_root(out);
    let this_root = ctx.add_global_root(this);
    ctx.refuse_try_arrays_after(1, 1);
    ctx.set_reclaim_succeeds(true);
    let r = native_sd_read_chars(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(out)),
            Value::Int(0),
            Value::Int(4),
        ],
    );
    // gc-common w16-c: the carry row persisted after the refill names the
    // decoder where it is AFTER the move, or the next sweep would judge a
    // live decoder dead. Since w29-c it is also FILED there (the mock's
    // move preserves the identity hash, as the heap does).
    let now = ctx.resolve_global_root(this_root).expect("rooted");
    let now_key = sd_key(&ctx, now);
    let owner = sd_table().lock().get(&now_key).map(|s| s.owner);
    assert_eq!(owner, Some(now.as_ptr() as usize));
    assert!(
        sd_table().lock().get(&key).is_none(),
        "nothing at the old address"
    );
    sd_table().lock().remove(&now_key);
    assert!(matches!(r, Ok(Some(Value::Int(4)))), "got {r:?}");
    let out = ctx.resolve_global_root(out_root).expect("rooted");
    let got: String = (0..4)
        .map(|i| match ctx.get_array_element(out, i) {
            Value::Int(c) => char::from_u32(c as u32).unwrap_or('\u{fffd}'),
            other => panic!("char[] element {i} is {other:?}"),
        })
        .collect();
    assert_eq!(got, "chan");
    assert_eq!(
        ctx.alloc_ladder_counts(),
        (3, 1),
        "scratch, refused channel buffer, one retry"
    );
    assert_eq!(ctx.relocation_count(), 1);
    assert_eq!(ctx.pinned_count(), 0);
}

#[test]
fn prop_decoder_strict_does_not_fall_back() {
    // strict=true (system property = UTF-8): invalid UTF-8 is REPLACE-decoded
    // and the decoder stays in UTF-8 mode (no ISO fallback).
    let mut p = fresh();
    p.strict = true;
    let bytes = vec![0xE4, 0xF6, 0xFC];
    let (chars, rest, p2) = decode_prop(&bytes, true, p);
    assert!(
        chars.iter().any(|&c| c == 0xFFFD),
        "REPLACE substitutes U+FFFD"
    );
    assert!(rest.is_empty());
    assert!(!p2.unwrap().fell_back);
}

// --- gc-common w16-c (`common-w15c-native-io-identity-keyed-tables-outside-the-weak-sweep` row 1) ---

fn sd_name(key: crate::IoRowKey) -> Option<String> {
    sd_table().lock().get(&key).map(|s| s.name.clone())
}

fn sd_owner(key: crate::IoRowKey) -> Option<usize> {
    sd_table().lock().get(&key).map(|s| s.owner)
}

/// The retire criterion for row 1: two VMs whose decoders have EQUAL
/// identity hashes (one address base) keep their own rows. The bare-hash
/// key made B's `forInputStreamReader` replace A's charset, and B's
/// `close()` drop A's row.
#[test]
fn w16c_two_vms_with_equal_identity_hashes_keep_their_own_decoder_rows() {
    let mut a = MockNativeContext::with_address_base(0x5E21_0000);
    let mut b = MockNativeContext::with_address_base(0x5E21_0000);
    let in_a = a.alloc_object(1);
    let in_b = b.alloc_object(1);
    let da = alloc_stream_decoder(&mut a, in_a, "ISO-8859-1", None).expect("decoder A");
    let db = alloc_stream_decoder(&mut b, in_b, "UTF-16", None).expect("decoder B");
    assert_eq!(a.identity_hash_code(da), b.identity_hash_code(db));
    let (key_a, key_b) = (sd_key(&a, da), sd_key(&b, db));
    assert_ne!(key_a, key_b);
    assert_eq!(sd_name(key_a).as_deref(), Some("ISO-8859-1"));
    assert_eq!(sd_name(key_b).as_deref(), Some("UTF-16"));
    assert_eq!(sd_owner(key_a), Some(da.as_ptr() as usize));

    let r = native_sd_close(&mut b, &[Value::Object(Some(db))]);
    assert!(r.is_ok(), "B's close gave {r:?}");
    assert_eq!(sd_name(key_b), None);
    assert_eq!(
        sd_name(key_a).as_deref(),
        Some("ISO-8859-1"),
        "A's row stays A's"
    );
    crate::forget_vm_io_side_tables(a.vm_identity());
    crate::forget_vm_io_side_tables(b.vm_identity());
}

/// The sweep, through the crate hook: a moved decoder's row follows it, a
/// dead decoder's goes, another VM's is never judged; teardown drops the
/// rest.
#[test]
fn w16c_sweep_follows_a_moved_decoder_and_drops_a_dead_one() {
    let mut ctx = MockNativeContext::new();
    let mut other = MockNativeContext::new();
    let (i1, i2, i3) = (
        ctx.alloc_object(1),
        ctx.alloc_object(1),
        other.alloc_object(1),
    );
    let moved = alloc_stream_decoder(&mut ctx, i1, "UTF-8", None).expect("moved");
    let dead = alloc_stream_decoder(&mut ctx, i2, "UTF-8", None).expect("dead");
    let foreign = alloc_stream_decoder(&mut other, i3, "UTF-8", None).expect("foreign");
    let (moved_key, dead_key) = (sd_key(&ctx, moved), sd_key(&ctx, dead));
    let foreign_key = sd_key(&other, foreign);

    // A real move keeps the identity hash (the mock's is the low 32 bits
    // of the address); the row is re-filed under the new address
    // (gc-common w29-c).
    let to = moved.as_ptr() as usize + (1usize << 32);
    let to_key = crate::io_row_key(crate::io_side_key(&ctx, moved), to);
    let mut map = cratonvm_types::PointerMap::new();
    map.insert(moved.as_ptr() as usize, to);
    assert_eq!(
        crate::gc_sweep_io_side_tables(ctx.vm_identity(), &map, &|_| false),
        1
    );
    assert_eq!(sd_owner(to_key), Some(to));
    assert_eq!(sd_owner(moved_key), None, "re-filed, not copied");
    assert_eq!(sd_owner(dead_key), None);
    assert!(
        sd_owner(foreign_key).is_some(),
        "another VM's row is untouched"
    );

    assert_eq!(crate::forget_vm_io_side_tables(ctx.vm_identity()), 1);
    assert_eq!(sd_owner(to_key), None);
    crate::forget_vm_io_side_tables(other.vm_identity());
}

/// `read(char[], int, int)` checks like the JDK: a null array NPEs, and a
/// negative length is an `IndexOutOfBoundsException`. The negative length
/// used to be cast to `usize` and PANIC in `Vec::with_capacity`.
fn read_chars_call(
    ctx: &mut MockNativeContext,
    this: ObjectRef,
    cbuf: Value,
    off: i32,
    len: i32,
) -> MethodCallResult {
    native_sd_read_chars(
        ctx,
        &[
            Value::Object(Some(this)),
            cbuf,
            Value::Int(off),
            Value::Int(len),
        ],
    )
}

fn is_npe(r: &MethodCallResult) -> bool {
    use cratonvm_types::error::{RuntimeError, VmError};
    matches!(
        r,
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::NullPointerException { .. }
        )))
    )
}

fn is_ioobe(r: &MethodCallResult) -> bool {
    use cratonvm_types::error::{RuntimeError, VmError};
    matches!(
        r,
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::IndexOutOfBoundsException { .. }
        )))
    )
}

#[test]
fn w16c_read_chars_checks_its_arguments_like_the_jdk() {
    let mut ctx = MockNativeContext::with_address_base(0x5E22_0000);
    let this = decoder_over(&mut ctx, b"abc", "UTF-8");
    let out = ctx.new_array(ArrayElementType::Char, 2);
    let npe = read_chars_call(&mut ctx, this, Value::Object(None), 0, 1);
    assert!(is_npe(&npe), "null cbuf gave {npe:?}");
    for (off, len) in [(0, -1), (-1, 1), (1, 2), (3, 0)] {
        let r = read_chars_call(&mut ctx, this, Value::Object(Some(out)), off, len);
        assert!(is_ioobe(&r), "read(cbuf, {off}, {len}) gave {r:?}");
    }
    let zero = read_chars_call(&mut ctx, this, Value::Object(Some(out)), 2, 0);
    assert!(
        matches!(zero, Ok(Some(Value::Int(0)))),
        "len 0 gave {zero:?}"
    );
    crate::forget_vm_io_side_tables(ctx.vm_identity());
}

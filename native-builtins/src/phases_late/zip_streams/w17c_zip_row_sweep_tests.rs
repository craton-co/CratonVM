use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::error::MethodCallResult;

thread_local! {
    /// `(old, new)` address of the stream the drain hook moves.
    static MOVE: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

fn addr(obj: ObjectRef) -> usize {
    obj.as_ptr() as usize
}

fn at(addr: usize) -> ObjectRef {
    // SAFETY: never dereferenced; the mock hashes the address and the
    // lock-key registry only records it.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

/// Drops the listed VMs' lock keys and zip rows, even when a test fails.
struct Teardown(&'static [usize]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for &vm in self.0 {
            crate::forget_vm_lock_keys(vm);
            forget_vm_zip_side_tables(vm);
        }
    }
}

/// A dead object's rows go from EVERY table they were filed in, a moved
/// object keeps its rows (found through its NEW address once the epilogue
/// re-addressed its key), and another VM's rows are never judged. Before
/// w17-c nothing but teardown removed a row whose stream was never closed.
#[test]
fn the_sweep_drops_a_dead_owners_rows_everywhere_and_follows_a_moved_one() {
    const VM: usize = 0xE17_0301;
    const OTHER: usize = 0xE17_0302;
    let _teardown = Teardown(&[VM, OTHER]);
    let ctx = mock_ctx();
    ctx.set_vm_identity(VM);
    let other = mock_ctx();
    other.set_vm_identity(OTHER);
    let (dead, moved) = (at(0x1_E170_3010), at(0x1_E170_3020));
    let foreign = at(0x1_E170_3030);
    let (dead_key, moved_key) = (zo_buf_key(&ctx, dead), zo_buf_key(&ctx, moved));
    let foreign_key = zo_buf_key(&other, foreign);

    inflater_fast_states().lock().unwrap().insert(
        dead_key,
        InflaterFastState {
            bytes: vec![1, 2, 3],
            pos: 0,
        },
    );
    sb_file_data_cache()
        .lock()
        .unwrap()
        .insert(dead_key, std::sync::Arc::new(vec![0; 16]));
    dos_state().lock().unwrap().insert(
        dead_key,
        DosState {
            pending: vec![9],
            finished: false,
        },
    );
    deflater_states()
        .lock()
        .unwrap()
        .insert(dead_key, DeflaterState::new(-1, false));
    iis_state().lock().unwrap().insert(
        moved_key,
        IisState {
            data: vec![7],
            pos: 0,
        },
    );
    inflater_states()
        .lock()
        .unwrap()
        .insert(foreign_key, InflaterState::new(false));

    // A relocation keeps the identity hash; the mock's hash is the
    // address's low 32 bits, so the new address keeps them.
    let to = addr(moved) + (2usize << 32);
    assert_eq!(
        ctx.identity_hash_code(moved),
        ctx.identity_hash_code(at(to)),
        "premise: a hash-preserving move"
    );
    let mut map = cratonvm_types::PointerMap::new();
    map.insert(addr(moved), to);
    let dead_addr = addr(dead);
    assert_eq!(
        crate::gc_sweep_and_remap_lock_keys(VM, &|a| a != dead_addr, &map),
        1,
        "the dead stream's key, nothing else"
    );
    assert_eq!(zo_existing_key(&ctx, dead), None, "its key was freed");
    assert!(!inflater_fast_states()
        .lock()
        .unwrap()
        .contains_key(&dead_key));
    assert!(!sb_file_data_cache().lock().unwrap().contains_key(&dead_key));
    assert!(!dos_state().lock().unwrap().contains_key(&dead_key));
    assert!(!deflater_states().lock().unwrap().contains_key(&dead_key));
    assert_eq!(
        zo_existing_key(&ctx, at(to)),
        Some(moved_key),
        "re-addressed: the moved stream still finds its key"
    );
    assert_eq!(
        zo_existing_key(&ctx, moved),
        None,
        "the vacated address has none"
    );
    assert!(iis_state().lock().unwrap().contains_key(&moved_key), "kept");
    assert_eq!(
        zo_existing_key(&other, foreign),
        Some(foreign_key),
        "never judged"
    );
    assert!(inflater_states().lock().unwrap().contains_key(&foreign_key));

    // The next collection finds the moved stream alive where it now is.
    let none = cratonvm_types::PointerMap::new();
    assert_eq!(
        crate::gc_sweep_and_remap_lock_keys(VM, &|a| a == to, &none),
        0
    );
    assert!(iis_state().lock().unwrap().contains_key(&moved_key));

    // Teardown drops the survivor's row, and only VM's.
    assert!(forget_vm_zip_side_tables(VM) >= 1);
    assert!(!iis_state().lock().unwrap().contains_key(&moved_key));
    assert!(inflater_states().lock().unwrap().contains_key(&foreign_key));
    // The lock-key teardown drops the other VM's rows through the hook.
    crate::forget_vm_lock_keys(OTHER);
    assert!(!inflater_states().lock().unwrap().contains_key(&foreign_key));
}

/// `Deflater.<init>` / `Inflater.<init>` (and the lazy `setInput`
/// creation) key their row by the object's weak lock key, so an
/// abandoned, never-`end()`ed zlib stream goes at the collection that
/// frees its object -- HotSpot's `Deflater` / `Inflater` cleaners.
#[test]
fn an_abandoned_deflater_and_inflater_go_with_their_objects() {
    const VM: usize = 0xE17_0304;
    let _teardown = Teardown(&[VM]);
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(VM);
    let (d, i, lazy) = (
        ctx.fresh_object_ref(),
        ctx.fresh_object_ref(),
        ctx.fresh_object_ref(),
    );
    if is_real_layout(&ctx, d, "zsRef") {
        eprintln!("SKIP: the mock resolves `zsRef`, so the synthetic path is not taken");
        return;
    }
    deflater_init(&mut ctx, &[Value::Object(Some(d))], "()V").expect("init");
    inflater_init(&mut ctx, &[Value::Object(Some(i))], "()V").expect("init");
    let src = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 2);
    deflater_set_input(
        &mut ctx,
        &[Value::Object(Some(lazy)), Value::Object(Some(src))],
        "([B)V",
    )
    .expect("setInput");
    let (dk, ik, lk) = (
        zo_existing_key(&ctx, d).expect("init keys the Deflater"),
        zo_existing_key(&ctx, i).expect("init keys the Inflater"),
        zo_existing_key(&ctx, lazy).expect("the lazy creation keys too"),
    );
    assert!(deflater_states().lock().unwrap().contains_key(&dk));
    assert!(inflater_states().lock().unwrap().contains_key(&ik));
    assert!(deflater_states().lock().unwrap().contains_key(&lk));

    let live = addr(i);
    assert_eq!(
        crate::gc_sweep_lock_keys(VM, &|a| a == live),
        2,
        "the dead Deflaters' keys go; the live Inflater's stays"
    );
    assert!(!deflater_states().lock().unwrap().contains_key(&dk));
    assert!(!deflater_states().lock().unwrap().contains_key(&lk));
    assert!(inflater_states().lock().unwrap().contains_key(&ik));
    forget_vm_zip_side_tables(VM);
    assert!(!inflater_states().lock().unwrap().contains_key(&ik));
}

fn move_stream_on_read(
    ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "read" {
        return None;
    }
    let (old, new) = MOVE.with(std::cell::Cell::get);
    ctx.remap_native_pin_addr_for_test(old, new);
    Some(Ok(Some(Value::Int(-1))))
}

/// The `InflaterInputStream` row is filed after the drain, which runs
/// Java and here moves the stream. gc-common w29-c: it is filed under the
/// key minted from the receiver BEFORE the drain (current there), which
/// stays the stream's across the move, and `iis_fill` hands that key back
/// so the caller reads the row without re-keying. The row is weak: the
/// sweep that frees the key drops it.
#[test]
fn an_inflater_input_stream_row_is_filed_under_the_key_minted_before_the_drain() {
    const VM: usize = 0xE17_0305;
    let _teardown = Teardown(&[VM]);
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(VM);
    let this = ctx.fresh_object_ref();
    let moved = ctx.fresh_object_ref();
    let src = ctx.fresh_object_ref();
    ctx.set_field(this, 0, Value::Object(Some(src)));
    ctx.set_field_by_name(this, "in", Value::Object(Some(src)));
    let key = zo_buf_key(&ctx, this);
    MOVE.with(|c| c.set((addr(this), addr(moved))));
    ctx.set_invoke_virtual_hook(move_stream_on_read);

    let mut cur = this;
    let filed = iis_fill(&mut ctx, &mut cur).expect("an empty stream inflates to nothing");

    assert_eq!(cur, moved, "the caller gets the moved reference back");
    assert_eq!(filed, key, "filed under the key minted before the drain");
    assert!(iis_state().lock().unwrap().contains_key(&key));
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every root released");

    // The stream dies: its key is freed and the row goes with it.
    crate::gc_sweep_lock_keys(VM, &|_| false);
    assert!(!iis_state().lock().unwrap().contains_key(&key));
}

thread_local! {
    /// How many `read`s [`feed_once_and_move_dst`] has answered.
    static READS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// The first `read` of the drain moves the caller's destination array
/// (`MOVE`) and hands back a zlib-wrapped "zip"; every later one is EOF.
fn feed_once_and_move_dst(
    ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "read" {
        return None;
    }
    let first = READS.with(|c| {
        let n = c.get();
        c.set(n + 1);
        n == 0
    });
    let Some(Value::Object(Some(buf))) = args.first().copied() else {
        return Some(Ok(Some(Value::Int(-1))));
    };
    if !first {
        return Some(Ok(Some(Value::Int(-1))));
    }
    let (old, new) = MOVE.with(std::cell::Cell::get);
    ctx.remap_native_pin_addr_for_test(old, new);
    let payload = {
        use std::io::Write;
        let mut enc =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(b"zip").expect("in-memory write");
        enc.finish().expect("in-memory finish")
    };
    ctx.write_byte_array_from(buf, 0, &payload);
    Some(Ok(Some(Value::Int(payload.len() as i32))))
}

/// `InflaterInputStream.read([BII)`'s first call drains the wrapped
/// stream through Java, which here moves the destination array. The
/// inflated bytes must land in the array where it is NOW; they used to be
/// written through the address read before the drain.
#[test]
fn an_iis_read_writes_into_the_destination_where_it_is_after_the_drain() {
    const VM: usize = 0xE17_0306;
    let _teardown = Teardown(&[VM]);
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(VM);
    let this = ctx.fresh_object_ref();
    let src = ctx.fresh_object_ref();
    ctx.set_field(this, 0, Value::Object(Some(src)));
    ctx.set_field_by_name(this, "in", Value::Object(Some(src)));
    let dst = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 8);
    let moved_dst = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 8);
    MOVE.with(|c| c.set((addr(dst), addr(moved_dst))));
    READS.with(|c| c.set(0));
    ctx.set_invoke_virtual_hook(feed_once_and_move_dst);

    let got = iis_read_bytes(
        &mut ctx,
        &[
            Value::Object(Some(this)),
            Value::Object(Some(dst)),
            Value::Int(0),
            Value::Int(8),
        ],
    )
    .expect("the read succeeds");

    assert_eq!(got, Some(Value::Int(3)));
    let mut now = [0u8; 3];
    assert_eq!(ctx.read_byte_array_into(moved_dst, 0, &mut now), 3);
    assert_eq!(&now, b"zip", "written into the moved array");
    let mut before = [0u8; 3];
    ctx.read_byte_array_into(dst, 0, &mut before);
    assert_eq!(before, [0u8; 3], "the vacated address is not written");
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every root released");
    forget_vm_zip_side_tables(VM);
}

use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};
use std::cell::Cell;

thread_local! {
    /// (old, new) address pairs the `toExternalForm` hook moves.
    static W16F_MOVES: Cell<[(usize, usize); 2]> = const { Cell::new([(0, 0); 2]) };
    /// The receiver `openStream` was dispatched on.
    static W16F_OPEN_STREAM_RECEIVER: Cell<usize> = const { Cell::new(0) };
}

fn addr(o: ObjectRef) -> usize {
    o.as_ptr() as usize
}

/// `toExternalForm` moves both registered objects and answers null (not a
/// "scheme://..." URL); `openStream` records its receiver.
fn w16f_move_hook(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    match method_name {
        "toExternalForm" => {
            for (old, new) in W16F_MOVES.with(|m| m.get()) {
                if old != 0 {
                    ctx.remap_native_pin_addr_for_test(old, new);
                }
            }
            Some(Ok(None))
        }
        "openStream" => {
            W16F_OPEN_STREAM_RECEIVER.with(|r| r.set(addr(receiver)));
            Some(Ok(Some(Value::Object(None))))
        }
        _ => None,
    }
}

/// `huc_init` with a URL whose `toExternalForm()` is not "scheme://...":
/// the synthetic arm used to write every slot -- and read the URL's
/// field 0 -- through the addresses from before that Java call.
#[test]
fn w16f_huc_init_synthetic_arm_writes_the_moved_receiver() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0xF16_0101);
    let this_old = ctx.fresh_object_ref();
    let this_new = ctx.fresh_object_ref();
    let url_old = ctx.fresh_object_ref();
    let url_new = ctx.fresh_object_ref();
    let spec_old = ctx.create_string("old-spec");
    let spec_new = ctx.create_string("new-spec");
    ctx.set_field(url_old, 0, Value::Object(Some(spec_old)));
    ctx.set_field(url_new, 0, Value::Object(Some(spec_new)));
    W16F_MOVES.with(|m| {
        m.set([
            (addr(this_old), addr(this_new)),
            (addr(url_old), addr(url_new)),
        ])
    });
    ctx.set_invoke_virtual_hook(w16f_move_hook);

    let out = huc_init(
        &mut ctx,
        &[Value::Object(Some(this_old)), Value::Object(Some(url_old))],
    );
    W16F_MOVES.with(|m| m.set([(0, 0); 2]));

    assert!(matches!(out, Ok(None)));
    assert!(matches!(
        ctx.get_field(this_new, HUC_CONN_ID),
        Value::Int(-1)
    ));
    assert!(matches!(
        ctx.get_field(this_new, HUC_METHOD),
        Value::Object(Some(_))
    ));
    assert!(matches!(
        ctx.get_field(this_new, HUC_INSTANCE_FOLLOW_REDIRECTS),
        Value::Int(1)
    ));
    assert!(
        matches!(ctx.get_field(this_new, HUC_URL_STR), Value::Object(Some(s)) if s == spec_new),
        "the URL string must be read from the URL's current address"
    );
    assert!(
        matches!(ctx.get_field(this_old, HUC_METHOD), Value::Int(0)),
        "nothing may be written through the vacated address"
    );
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}

/// `getInputStream` on a real carrier whose `toExternalForm()` answers
/// nothing: the fallback peeked at, and dispatched `openStream` on, the URL
/// copy read before that Java call.
#[test]
fn w16f_get_input_stream_fallback_opens_the_moved_url() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0xF16_0102);
    let this_old = ctx.fresh_object_ref();
    let this_new = ctx.fresh_object_ref();
    let url_old = ctx.fresh_object_ref();
    let url_new = ctx.fresh_object_ref();
    // The collector copied the carrier: each copy names "its" URL.
    ctx.set_field(this_old, HUC_CONN_ID, Value::Object(Some(url_old)));
    ctx.set_field(this_new, HUC_CONN_ID, Value::Object(Some(url_new)));
    let spec = ctx.create_string("file:/tmp/w16f.xml");
    ctx.set_field(url_new, 5, Value::Object(Some(spec)));
    W16F_MOVES.with(|m| m.set([(addr(this_old), addr(this_new)), (0, 0)]));
    W16F_OPEN_STREAM_RECEIVER.with(|r| r.set(0));
    ctx.set_invoke_virtual_hook(w16f_move_hook);

    let out = huc_get_input_stream(&mut ctx, &[Value::Object(Some(this_old))]);
    W16F_MOVES.with(|m| m.set([(0, 0); 2]));

    assert!(
        out.is_ok(),
        "the file: URL must be opened, not sent as HTTP"
    );
    assert_eq!(
        W16F_OPEN_STREAM_RECEIVER.with(|r| r.get()),
        addr(url_new),
        "openStream must be dispatched on the URL the moved carrier holds"
    );
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}

fn header_lines(ctx: &MockNativeContext, this: ObjectRef) -> Vec<String> {
    let Value::Object(Some(arr)) = ctx.get_field(this, HUC_REQ_HEADERS) else {
        return Vec::new();
    };
    (0..ctx.array_length(arr))
        .filter_map(|i| match ctx.get_array_element(arr, i) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        })
        .collect()
}

fn w16f_header_call(ctx: &mut MockNativeContext, this: ObjectRef, add: bool, k: &str, v: &str) {
    let k = ctx.create_string(k);
    let v = ctx.create_string(v);
    let args = [
        Value::Object(Some(this)),
        Value::Object(Some(k)),
        Value::Object(Some(v)),
    ];
    let out = if add {
        huc_add_request_property(ctx, &args)
    } else {
        huc_set_request_property(ctx, &args)
    };
    assert!(matches!(out, Ok(None)));
}

/// The handshake table is per VM, and a dead carrier's row goes with its
/// other real-carrier rows. VM B's carrier is given VM A's carrier's
/// address (two mocks do not mint the same addresses, and the mock's
/// identity hash is the address), so the two carriers have equal identity
/// hashes -- the collision the bare-hash key could not tell apart.
#[test]
fn w16f_https_peer_rows_are_per_vm_and_die_with_the_carrier() {
    const VM_A: usize = 0xF16_0301;
    const VM_B: usize = 0xF16_0302;
    struct Teardown;
    impl Drop for Teardown {
        fn drop(&mut self) {
            for vm in [VM_A, VM_B] {
                forget_vm_http_url_connection_state(vm);
                crate::forget_vm_lock_keys(vm);
            }
        }
    }
    let _teardown = Teardown;
    let mut a = MockNativeContext::new();
    a.set_vm_identity(VM_A);
    let mut b = MockNativeContext::new();
    b.set_vm_identity(VM_B);
    let ca = a.fresh_object_ref();
    // Never dereferenced by `b`: the rows key on the identity hash and
    // the VM (through the weak lock key since gc-common w27-b).
    let cb = ca;
    assert_eq!(
        a.identity_hash_code(ca),
        b.identity_hash_code(cb),
        "precondition: the two carriers collide on identity hash"
    );
    let has = |key: Option<usize>| {
        key.is_some_and(|k| https_peer_info().lock().unwrap().contains_key(&k))
    };
    let chain = [vec![0x30u8, 0x01, 0x02]];

    record_https_peer_info(&a, Some(ca), &chain, "TLS_AES_128_GCM_SHA256");
    let key_a = huc_existing_obj_key(&a, ca);
    assert!(has(key_a));
    assert!(
        !has(huc_existing_obj_key(&b, cb)),
        "VM B's carrier must not find VM A's handshake"
    );

    // A dies: the lock-key sweep frees its key and drops its rows.
    crate::gc_sweep_lock_keys(VM_A, &|_| false);
    assert!(!has(key_a), "a dead carrier's handshake row goes");
    assert!(huc_existing_obj_key(&a, ca).is_none());

    // Teardown drops only the torn-down VM's rows.
    record_https_peer_info(&a, Some(ca), &chain, "TLS_AES_128_GCM_SHA256");
    record_https_peer_info(&b, Some(cb), &chain, "TLS_AES_128_GCM_SHA256");
    let (key_a, key_b) = (huc_existing_obj_key(&a, ca), huc_existing_obj_key(&b, cb));
    assert_ne!(key_a, key_b);
    forget_vm_http_url_connection_state(VM_A);
    assert!(!has(key_a));
    assert!(has(key_b), "another VM's row survives");
    forget_vm_http_url_connection_state(VM_B);
    assert!(!has(key_b));
}

/// The shared `huc_synthetic_store_header` keeps both setters' semantics:
/// `set` replaces a key's line, `add` appends another, and neither leaves
/// a root behind.
#[test]
fn w16f_synthetic_header_setters_replace_and_append() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0xF16_0103);
    let this = ctx.fresh_object_ref();
    ctx.set_field(this, HUC_CONN_ID, Value::Int(-1));
    // The mock's `new_array(Reference, n)` fills with `Int(0)`, not null
    // (production fills with null), so the line array is seeded with a
    // null-filled one rather than left to the helper's lazy allocation.
    let lines = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 32);
    ctx.set_field(this, HUC_REQ_HEADERS, Value::Object(Some(lines)));
    w16f_header_call(&mut ctx, this, false, "X-A", "1");
    w16f_header_call(&mut ctx, this, false, "x-a", "2");
    assert_eq!(header_lines(&ctx, this), vec!["x-a: 2".to_string()]);
    w16f_header_call(&mut ctx, this, true, "X-A", "3");
    assert_eq!(
        header_lines(&ctx, this),
        vec!["x-a: 2".to_string(), "X-A: 3".to_string()]
    );
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}

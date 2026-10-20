// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 18 wave 11, lane backlogfix (R16B-194): source witnesses that
//! the `--compatible` `ArrayList$ListItr` natives keep their receivers in a
//! pin frame across the list funnels they call, and re-read them before the
//! field writes that follow. A stale receiver under a moving collector writes
//! the cursor into a vacated copy and fails silently, which no mock in this
//! crate can stage; the runtime witness is `R18BacklogfixListItr.java` under
//! `CRATONVM_DBG_GC_STRESS` + `CRATONVM_DBG_STALE_OBJREF=1`.

fn body<'a>(src: &'a str, signature: &str) -> &'a str {
    let start = src
        .find(signature)
        .unwrap_or_else(|| panic!("`{signature}` exists"));
    let rest = &src[start..];
    let end = rest.find("\n}\n").map_or(rest.len(), |e| e + 3);
    &rest[..end]
}

/// `pin` comes before `call`, and `reread` comes after it and before `use_at`.
fn pinned_across(b: &str, what: &str, pin: &str, call: &str, reread: &str, use_at: &str) {
    let p = b.find(pin).unwrap_or_else(|| panic!("{what}: `{pin}`"));
    let c = b.find(call).unwrap_or_else(|| panic!("{what}: `{call}`"));
    let r = b
        .find(reread)
        .unwrap_or_else(|| panic!("{what}: `{reread}`"));
    let u = b
        .find(use_at)
        .unwrap_or_else(|| panic!("{what}: `{use_at}`"));
    assert!(p < c, "{what}: pinned before the call");
    assert!(c < r, "{what}: re-read after the call");
    assert!(r < u, "{what}: re-read before the first use");
}

#[test]
fn list_itr_remove_rereads_this_and_list_after_the_removal() {
    let src = include_str!("../src/lib.rs").replace("\r\n", "\n");
    let b = body(&src, "\nfn native_arraylist_list_itr_remove(");
    pinned_across(
        b,
        "remove/this",
        "let this_pin = ctx.pin_native_root(this);",
        "native_al_remove_at(",
        "let this = ctx.read_native_pin(this_pin, this);",
        "ctx.set_field(this, cursor_slot",
    );
    pinned_across(
        b,
        "remove/list",
        "let list_pin = ctx.pin_native_root(list);",
        "native_al_remove_at(",
        "let list = ctx.read_native_pin(list_pin, list);",
        "al_list_itr_list_mod_count(&*ctx, list)",
    );
    // The frame is released before the error propagates.
    let unpin = b
        .find("ctx.unpin_native_roots(this_pin);")
        .expect("unpinned");
    let q = b
        .find("let _ = removed?;")
        .expect("error propagated after the unpin");
    assert!(unpin < q);
}

#[test]
fn list_itr_next_and_previous_reread_this_after_the_get() {
    let src = include_str!("../src/lib.rs").replace("\r\n", "\n");
    for (name, sig) in [
        ("next", "\nfn native_snapshot_itr_next("),
        ("previous", "\nfn native_snapshot_list_itr_previous("),
    ] {
        let b = body(&src, sig);
        pinned_across(
            b,
            name,
            "let this_pin = ctx.pin_native_root(this);",
            "native_al_get(",
            "let this = ctx.read_native_pin(this_pin, this);",
            "ctx.set_field(this, cursor_slot",
        );
        let unpin = b
            .find("ctx.unpin_native_roots(this_pin);")
            .expect("unpinned");
        let q = b
            .find("let value = value?;")
            .expect("error propagated after the unpin");
        assert!(unpin < q, "{name}: unpinned before the `?`");
    }
}

#[test]
fn stream_spliterator_rereads_this_after_the_jdk_spliterator_call() {
    let src = include_str!("../../native-collections/src/lib.rs").replace("\r\n", "\n");
    let b = body(&src, "\nfn native_stream_spliterator(");
    pinned_across(
        b,
        "spliterator",
        "let this_pin = ctx.pin_native_root(this);",
        "\"java/util/stream/AbstractPipeline\",",
        "this = ctx.read_native_pin(this_pin, this);",
        "stream_is_reference_carrier(&*ctx, this)",
    );
}

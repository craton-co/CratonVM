/// gci i9/hj (`gci-i8hd-jvmti-tag-functions-and-oome-dump-outside-a-pause-FIXED-20261017`
/// item 3): the OOME dump retries a refused pause with GC-safe slices when
/// the caller is this OS thread's own `JvmThread`, the single request
/// otherwise; the slice is a blocked region (or a sleep for a thread
/// already in one). Source pin: a competing pause and a counted OOME
/// thread cannot be staged together in a unit test; the retry loop itself
/// is exercised by `hprof` `gci_i9hj_oom_dump_retries_its_pause_gc_safe`.
#[test]
fn gci_i9hj_oom_dump_waits_refusals_out_gc_safe() {
    let src = super::GC_AND_ALLOC_SRC.replace("\r\n", "\n");
    let body = |name: &str| -> String {
        let start = src.find(name).expect(name);
        let rest = &src[start..];
        rest[..rest.find("\n}\n").expect("end")].to_string()
    };
    let w = body("fn write_heap_dump_now(");
    // gci i10/al: "own" is the registry's thread for its id, read by
    // address (`oom_dump_thread_is_registered`), no longer a pointer from
    // `current_jvm_thread_of` compared with `std::ptr::eq`.
    let own = w
        .find("oom_dump_thread_is_registered(shared, thread)")
        .expect("own thread");
    let retry = w
        .find("dump_heap_on_oom(&arc, &path, tid, gzip, &mut slice)")
        .expect("retrying dump");
    let single = w
        .find("crate::runtime::hprof::dump_heap_framed(&arc, &path, tid, gzip)")
        .expect("single request");
    assert!(own < retry && retry < single);
    let s = body("fn oom_dump_gc_safe_slice(");
    let blocked = s.find(".in_blocked_region").expect("already blocked");
    // Split so the wait-site ratchet's text scan does not count this needle.
    let begin = s
        .find(concat!("ctx.begin_", "blocking_region();"))
        .expect("region");
    let end = s.find("ctx.end_blocking_region();").expect("leave");
    assert!(blocked < begin && begin < end);
    assert!(s.contains("ctx.begin_vm_wait_region();"));
}

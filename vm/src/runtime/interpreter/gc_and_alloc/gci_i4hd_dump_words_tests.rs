use super::*;

#[test]
fn gci_i4hd_dump_words_are_sorted_and_deduplicated() {
    let mut buf = [0u64; 4];
    let base = buf.as_mut_ptr() as *mut u8;
    // SAFETY: non-null, 8-aligned addresses inside `buf`; never dereferenced.
    let at = |i: usize| unsafe { ObjectRef::from_raw(base.add(8 * i)) };
    let words = vec![at(2), at(0), at(2), at(1), at(0)];
    let got: Vec<usize> = dedup_dump_words(words)
        .iter()
        .map(|o| o.as_ptr() as usize)
        .collect();
    let b = base as usize;
    assert_eq!(got, vec![b, b + 8, b + 16]);
}

/// The dump mode is a no-roots mode (never a root-gathering take-over),
/// thread-local, and lowered on every path out.
#[test]
fn gci_i4hd_dump_words_mode_is_no_roots_thread_local_and_lowered() {
    assert!(!dump_peer_words_wanted());
    assert!(takeover_gathers_roots());
    {
        let _g = NoRootsPause::enter();
        assert!(
            !dump_peer_words_wanted(),
            "a plain no-roots pause keeps no words"
        );
        assert!(!takeover_gathers_roots());
    }
    {
        let _g = NoRootsPause::enter_mode(NO_ROOTS_DUMP_WORDS);
        assert!(dump_peer_words_wanted());
        assert!(
            !takeover_gathers_roots(),
            "the dump's pause gathers no roots"
        );
        let other = std::thread::spawn(dump_peer_words_wanted).join().unwrap();
        assert!(
            !other,
            "another thread (a collection's initiator) never sees it"
        );
    }
    assert!(!dump_peer_words_wanted());
    assert!(takeover_gathers_roots());
}

/// Source pin: the dump probe is an arm of the no-roots branch only, and
/// only `request_dump_words` asks for it.
#[test]
fn gci_i4hd_takeover_probes_dump_words_only_without_roots() {
    let src = super::GC_AND_ALLOC_SRC.replace("\r\n", "\n");
    let body = &src[src
        .find("fn run_takeover_pass(")
        .expect("run_takeover_pass")..];
    let body = &body[..body.find("\n}\n").expect("end of run_takeover_pass")];
    let gathers = body
        .find("let n = if gathers_roots {")
        .expect("gathers arm");
    let dump = body
        .find("} else if dump_peer_words_wanted() {")
        .expect("dump arm");
    let decline = body.find("None::<ObjectRef>").expect("decline arm");
    assert!(gathers < dump && dump < decline);
    let req = &src[src
        .find("pub(crate) fn request_dump_words(")
        .expect("request_dump_words")..];
    assert!(req[..300].contains("NonCollectionPause::HeapDump, None, true)"));
}

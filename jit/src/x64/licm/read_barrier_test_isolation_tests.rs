#[test]
fn overrides_are_thread_local_and_restore_nested_state() {
    let ambient = super::zgc_read_barrier_blocks_inline_fields();
    let peer;
    {
        let scope = super::ZgcReadBarrierTestOverride::new(!ambient);
        assert_eq!(super::zgc_read_barrier_blocks_inline_fields(), !ambient);
        peer = std::thread::spawn(super::zgc_read_barrier_blocks_inline_fields)
            .join()
            .unwrap();
        {
            let _nested = super::ZgcReadBarrierTestOverride::new(ambient);
            assert_eq!(super::zgc_read_barrier_blocks_inline_fields(), ambient);
        }
        assert_eq!(super::zgc_read_barrier_blocks_inline_fields(), !ambient);
        scope.set(ambient);
        assert_eq!(super::zgc_read_barrier_blocks_inline_fields(), ambient);
    }
    assert_eq!(super::zgc_read_barrier_blocks_inline_fields(), ambient);
    assert_eq!(
        peer, ambient,
        "another test thread retains its own barrier policy"
    );
}

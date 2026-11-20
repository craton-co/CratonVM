use super::*;

#[test]
fn the_shim_block_is_one_offset_on_every_thread_and_per_thread() {
    let delta = load_shim().expect("glibc loads the in-memory TLS shim into static TLS");
    assert!(delta < 0);
    let write = move |v: usize| {
        shim_cell(delta, 1).set(v);
        shim_cell(delta, 1).get()
    };
    assert_eq!(write(7), 7);
    let other = std::thread::spawn(move || {
        assert_eq!(shim_cell(delta, 1).get(), 0, "a new thread starts zeroed");
        write(9)
    })
    .join()
    .unwrap();
    assert_eq!((other, shim_cell(delta, 1).get()), (9, 7));
}

#[test]
fn the_scheme_resolves_in_a_static_tls_test_binary() {
    assert_ne!(Mirror(1).disp(), 0);
    Mirror(2).with(|c| c.set(5));
    assert_eq!(Mirror(2).try_with(|c| c.get()), Ok(5));
}

use super::*;

#[test]
fn disp32_bound_rejects_high_and_unaligned_bases() {
    assert!(!base_fits_disp32(0));
    assert!(!base_fits_disp32(0x1000_0001));
    assert!(!base_fits_disp32(0x8000_0000));
    assert!(!base_fits_disp32(LOW_LIMIT - PAGE));
    assert!(base_fits_disp32(0x1000_0000));
    assert!(base_fits_disp32(LOW_LIMIT - PAIR_BYTES));
}

/// The OS half without the switch: a low pair maps under 2 GiB, its stop
/// page reads and toggles, and a registered pair is recognised by the
/// fault-address test across both pages and nowhere else. The armed page
/// is never touched here (no handler in a unit-test process).
#[test]
fn a_low_pair_maps_toggles_and_is_recognised() {
    let Some(base) = os::map_low_pair() else {
        // A platform without a low mapping: the switch then stays off.
        return;
    };
    assert!(base_fits_disp32(base), "pair at {base:#x} out of disp32 reach");
    // SAFETY: the stop page is readable after mapping.
    let v = unsafe { core::ptr::read_volatile(base as *const u32) };
    assert_eq!(v, 0, "fresh anonymous memory");
    assert!(os::protect_page(base, false));
    assert!(os::protect_page(base, true));
    // SAFETY: readable again.
    let _ = unsafe { core::ptr::read_volatile(base as *const u32) };
    assert!(!is_poll_page_addr(base));
    assert!(register_pair(base));
    assert!(is_poll_page_addr(base));
    assert!(is_poll_page_addr(base + ARMED_OFFSET + 8));
    assert!(!is_poll_page_addr(base + PAIR_BYTES));
    assert!(!is_poll_page_addr(base.wrapping_sub(1)));
    // Leave it registered and mapped: the registry has no removal, as in
    // production (a pair lives as long as its VM, and VMs are few).

    // The handler's question: a poll site registered in the implicit-null
    // table recovers for a fault inside the pair (both pages), and not for
    // an address past it; the null page still recovers as before.
    let fake_code: &'static [u8; 64] = Box::leak(Box::new([0u8; 64]));
    let site = fake_code.as_ptr() as usize + 0x10;
    let slow = fake_code.as_ptr() as usize + 0x20;
    assert!(crate::implicit_null::register(site, slow));
    assert_eq!(crate::implicit_null::recover(site, base), Some(slow));
    assert_eq!(crate::implicit_null::recover(site, base + ARMED_OFFSET), Some(slow));
    assert_eq!(crate::implicit_null::recover(site, base + PAIR_BYTES), None);
    assert_eq!(crate::implicit_null::recover(site, 0x8), Some(slow));
    crate::implicit_null::unregister_range(fake_code.as_ptr() as usize, 64);
    assert_eq!(crate::implicit_null::recover(site, base), None);
}

#[test]
fn switch_off_maps_nothing_and_has_no_cell() {
    if enabled() {
        // The switch is on for this process (on Linux by default since j27;
        // the answer is cached per process, so no thread override reaches it).
        return;
    }
    assert_eq!(map_pair(), None);
    assert_eq!(this_threads_cell_addr(), 0);
    assert_eq!(this_threads_cell_value(), None);
    assert!(!emit_enabled());
    assert!(!set_stop(0, true));
}

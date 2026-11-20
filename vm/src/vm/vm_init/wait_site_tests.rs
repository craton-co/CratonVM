use super::*;

/// JIT round 16 (lane monitor, MB1): a lazily noted wait renders, on
/// demand, exactly the text an eager one stored; clearing forgets both;
/// nothing is reported when no wait was noted.
#[test]
fn r16_mb1_a_lazy_wait_site_renders_what_the_eager_one_stored() {
    let thread = JvmThread::new(ThreadId(1641), "r16-mb1-waiter");
    clear_wait_site_snapshot();
    assert_eq!(wait_site_dump_text(), None);
    note_wait_site(&thread, false);
    let eager = wait_site_dump_text().expect("an eager wait-site dump");
    assert!(eager.contains("tid=1641"), "{eager}");
    assert!(eager.contains("(wait-site)"), "{eager}");
    clear_wait_site_snapshot();
    note_wait_site(&thread, true);
    assert_eq!(wait_site_dump_text().as_deref(), Some(eager.as_str()));
    clear_wait_site_snapshot();
    assert_eq!(wait_site_dump_text(), None);
}

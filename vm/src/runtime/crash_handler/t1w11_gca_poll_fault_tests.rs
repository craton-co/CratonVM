use cratonvm_jit::poll_page::{self, ARMED_OFFSET};
use std::time::Instant;

/// `poll(page)`: `0` when `[page]` is readable; `1` when the read faulted
/// and the handler resumed at the registered slow label. The site is the
/// instruction a page-form poll faults on (`TEST [reg], EAX`), registered
/// as the single-pass driver registers it (the implicit-null table: exact
/// PC, recovery label, fault address inside a registered pair).
struct PollStub {
    // Owns the mapping; dropping it retires the registered site.
    _buf: cratonvm_jit::ExecutableBuffer,
    poll: extern "C" fn(usize) -> u32,
}

fn poll_stub() -> PollStub {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
    let site = buf.pos();
    #[cfg(windows)]
    buf.emit(&[0x85, 0x01]); // TEST [RCX], EAX (Win64: argument 0 in RCX)
    #[cfg(not(windows))]
    buf.emit(&[0x85, 0x07]); // TEST [RDI], EAX (SysV: argument 0 in RDI)
    buf.emit(&[0x31, 0xC0, 0xC3]); // XOR EAX, EAX ; RET
    let slow = buf.pos();
    buf.emit(&[0xB8, 0x01, 0x00, 0x00, 0x00, 0xC3]); // MOV EAX, 1 ; RET
    buf.finalize();
    let entry = buf.as_ptr() as usize;
    assert!(
        cratonvm_jit::implicit_null::register(entry + site, entry + slow),
        "the implicit-null table took the poll site"
    );
    // SAFETY: `entry` is the finalized (RX) code above: a leaf that takes
    // one integer argument in the platform's C convention and returns a
    // `u32` in EAX, touching nothing but the argument's target.
    let poll = unsafe { std::mem::transmute::<usize, extern "C" fn(usize) -> u32>(entry) };
    PollStub { _buf: buf, poll }
}

fn ns_per_op(n: u32, f: impl Fn() -> u32, want: u32) -> f64 {
    let t = Instant::now();
    for _ in 0..n {
        assert_eq!(f(), want);
    }
    t.elapsed().as_nanos() as f64 / f64::from(n)
}

#[test]
#[ignore = "measurement: installs the process fault handler; run with --ignored --nocapture"]
fn t1w11_gca_poll_fault_round_trip() {
    super::install_hardware_fault_handler();
    let Some(base) = poll_page::map_pair_for_measurement() else {
        eprintln!("[t1w11-gca] no low page pair on this host: nothing measured");
        return;
    };
    let stub = poll_stub();
    let poll = stub.poll;

    // Warm both paths (page tables, the handler's first-touch costs).
    assert_eq!(poll(base), 0, "the stop page reads while no pause is up");
    assert_eq!(poll(base + ARMED_OFFSET), 1, "the armed page faults into the slow label");

    let fast = ns_per_op(1_000_000, || poll(base), 0);
    let armed = ns_per_op(20_000, || poll(base + ARMED_OFFSET), 1);
    // A pause: the stop page protected, every poll of it faults; then the
    // lowering unprotects it and the same poll reads again.
    assert!(poll_page::set_stop(base, true), "protect the stop page");
    let stopped = ns_per_op(20_000, || poll(base), 1);
    assert!(poll_page::set_stop(base, false), "unprotect the stop page");
    assert_eq!(poll(base), 0, "readable again after the pause");
    // The two protection changes a pause adds (raise + lower).
    let n_toggle = 2_000u32;
    let t = Instant::now();
    for _ in 0..n_toggle {
        assert!(poll_page::set_stop(base, true));
        assert!(poll_page::set_stop(base, false));
    }
    let toggle = t.elapsed().as_nanos() as f64 / f64::from(n_toggle);

    println!("time poll-page-fast {fast:.1} ns/op");
    println!("time poll-page-fault-armed {armed:.0} ns/op");
    println!("time poll-page-fault-stop {stopped:.0} ns/op");
    println!("time poll-page-stop-protect-unprotect {toggle:.0} ns/op");
    // The book's refutation bar: a round trip above ~50 us.
    println!(
        "[t1w11-gca] fault round trip {} the book's 50 us refutation bar",
        if armed.max(stopped) > 50_000.0 { "EXCEEDS" } else { "is under" }
    );
}

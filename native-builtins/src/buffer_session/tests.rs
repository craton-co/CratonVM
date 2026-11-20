use super::*;

/// `SERVED` is process-global, and `overflow_degrades_to_unserved` fills
/// every one of its slots on purpose. Cargo runs the two tests below on
/// separate threads of one process, so before this guard existed the
/// outcome depended on which ran first: with the overflow test ahead,
/// `note_served(0xDEAD_0001)` found no free slot and the claim test failed
/// its positive assertion. Picking ids that do not collide -- which the
/// comment below used to rely on -- cannot help, because the problem is
/// capacity, not collision.
///
/// Each test takes this lock and starts from an empty table. The lock is
/// test-only; the production table is never cleared.
static TABLE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Empty `SERVED`. Test-only: the shim's table is append-only in
/// production, and a class is only ever removed from it by process exit.
fn reset_table() {
    for cell in SERVED.iter() {
        cell.store(0, Ordering::Relaxed);
    }
}

/// **An unserved class is never claimed, and a served one is** — the whole
/// contract, and the reason the JIT helper may answer at all.
#[test]
fn only_a_served_class_is_claimed() {
    let _guard = TABLE.lock().unwrap_or_else(|e| e.into_inner());
    reset_table();
    assert!(!class_is_served(0), "class id 0 is never served");
    assert!(!class_is_served(0xDEAD_0001));
    note_served(0xDEAD_0001);
    assert!(class_is_served(0xDEAD_0001));
    // A different class stays unserved: the table is per-class, not a flag.
    assert!(!class_is_served(0xDEAD_0002));
    // Re-noting is idempotent and must not consume a second slot.
    note_served(0xDEAD_0001);
    assert!(class_is_served(0xDEAD_0001));
}

/// **A full table refuses rather than wraps.** The capacity is an
/// optimisation bound, not a correctness one, so overflow must degrade to
/// "not served" instead of evicting an entry and claiming a class the
/// funnel never proved.
#[test]
fn overflow_degrades_to_unserved() {
    let _guard = TABLE.lock().unwrap_or_else(|e| e.into_inner());
    reset_table();
    for i in 0..(CAPACITY as u32 + 4) {
        note_served(0xBEEF_0000 + i + 1);
    }
    // Whatever fits is served; nothing outside the table is claimed.
    let claimed = (0..(CAPACITY as u32 + 4))
        .filter(|i| class_is_served(0xBEEF_0000 + i + 1))
        .count();
    assert!(
        claimed <= CAPACITY,
        "claimed {claimed} classes from a {CAPACITY}-slot table"
    );
}

use super::*;


fn gate() -> crate::classloading::resolution::RedefineGate {
    crate::classloading::resolution::RedefineGate::snapshot(Arc::new(
        std::sync::atomic::AtomicU32::new(0),
    ))
}

fn slot(addr: usize) -> GenericIndySlot {
    GenericIndySlot {
        // SAFETY: never dereferenced; the row is only published and probed.
        link: GenericIndyLink::Linked(unsafe { ObjectRef::from_raw(addr as *mut u8) }),
        shape: Arc::new(GenericIndyShape::new("()Ljava/lang/Object;")),
        gate: gate(),
    }
}

fn addr_of(link: &GenericIndyLink) -> Option<usize> {
    match link {
        GenericIndyLink::Linked(cs) => Some(cs.as_ptr() as usize),
        _ => None,
    }
}

/// Fill the process table past the cap with rows of the fake VM `vm`.
fn fill_past_the_cap(vm: usize) {
    let mut table = generic_indy_sites().write();
    let mut i = 0u64;
    while table.len() < GENERIC_INDY_SITE_CAP + 64 {
        table.insert((vm, ClassId::new(0x7_100), 1, i, 3), slot(0x10_000));
        i += 1;
    }
    drop(table);
    ANY_GENERIC_INDY_SITE.store(true, std::sync::atomic::Ordering::Release);    }
use super::register_implicit_null_sites;

#[test]
fn a_declined_site_retires_every_site_the_artifact_registered() {
    // `register` refuses a fault PC equal to a table sentinel
    // (`usize::MAX - 1`), which is how this test makes the SECOND site
    // decline without filling the process-wide table.
    let base = usize::MAX - 0x31;
    let first_fault = base + 0x10;
    let sites = [(0x10usize, 0x20usize), (0x30, 0x28)];
    assert_eq!(
        register_implicit_null_sites(base, 0x31, &sites),
        Err(0x30),
        "the sentinel-keyed site must decline"
    );
    assert_eq!(
        crate::implicit_null::recover(first_fault, 0x10),
        None,
        "the site registered before the decline must have been retired"
    );
}

#[test]
fn an_accepted_artifact_registers_every_site() {
    let base = 0x4f00_0000_0000usize;
    let sites = [(0x10usize, 0x40usize), (0x18, 0x48)];
    assert_eq!(register_implicit_null_sites(base, 0x100, &sites), Ok(()));
    assert_eq!(
        crate::implicit_null::recover(base + 0x10, 0x8),
        Some(base + 0x40)
    );
    assert_eq!(
        crate::implicit_null::recover(base + 0x18, 0x8),
        Some(base + 0x48)
    );
    crate::implicit_null::unregister_range(base, 0x100);
    assert_eq!(crate::implicit_null::recover(base + 0x10, 0x8), None);
}

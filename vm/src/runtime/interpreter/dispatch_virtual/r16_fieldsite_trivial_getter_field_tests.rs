use super::trivial_getter_field;
use cratonvm_classloading::resolution::ResolvedField;
use cratonvm_types::ClassId;

fn field(index: usize, is_static: bool, is_volatile: bool) -> ResolvedField {
    ResolvedField {
        declaring_class_id: ClassId::new(7),
        field_index: index,
        is_static,
        is_volatile,
        is_final: false,
        is_reference: false,
        desc_byte: b'I',
    }
}

#[test]
fn a_site_hit_wins_and_the_shared_cache_is_not_read() {
    let mut raw_reads = 0;
    let got = trivial_getter_field(Some(field(3, false, false)), || {
        raw_reads += 1;
        Some(field(9, false, false))
    });
    let (f, from_site) = got.expect("a plain instance field answers");
    assert_eq!((f.field_index, from_site), (3, true));
    assert_eq!(raw_reads, 0, "a site hit must not take the shared lock");
}

#[test]
fn a_site_miss_reads_the_shared_cache_as_before() {
    let got = trivial_getter_field(None, || Some(field(9, false, false)));
    let (f, from_site) = got.expect("raw hit");
    assert_eq!((f.field_index, from_site), (9, false));
    assert!(
        trivial_getter_field(None, || None).is_none(),
        "cold: run the body"
    );
}

#[test]
fn static_or_volatile_declines_without_falling_back() {
    let raw = || Some(field(9, false, false));
    assert!(trivial_getter_field(Some(field(3, false, true)), raw).is_none());
    assert!(trivial_getter_field(Some(field(3, true, false)), raw).is_none());
    assert!(trivial_getter_field(None, || Some(field(9, false, true))).is_none());
}

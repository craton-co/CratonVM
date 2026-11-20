use super::*;

/// Every embedded class is a Java 25 class file under the shared prefix,
/// and every field descriptor maps to its variable type's pair.
#[test]
fn every_embedded_form_class_is_a_class_file_and_every_type_maps() {
    for c in &VH_REAL_VARFORM_CLASSES {
        for (name, bytes) in [(c.ro_name, c.ro_bytes), (c.rw_name, c.rw_bytes)] {
            assert!(name.starts_with(VH_REAL_VARFORM_PREFIX), "{name}");
            assert_eq!(&bytes[..4], &[0xCA, 0xFE, 0xBA, 0xBE], "{name}");
            assert_eq!(u16::from_be_bytes([bytes[6], bytes[7]]), 69, "{name}");
        }
        assert_eq!(c.primitive.is_none(), c.letter == b'L');
    }
    for (desc, rw) in [
        ("I", "cratonvm/internal/VhFormInt"),
        ("Z", "cratonvm/internal/VhFormBoolean"),
        ("Ljava/lang/String;", "cratonvm/internal/VhFormReference"),
        ("[I", "cratonvm/internal/VhFormReference"),
        ("D", "cratonvm/internal/VhFormDouble"),
    ] {
        assert_eq!(vh_real_varform_class(desc).map(|c| c.rw_name), Some(rw), "{desc}");
    }
    assert!(vh_real_varform_class("").is_none());
    assert!(vh_real_varform_class("V").is_none());
}

/// Interpreter round i7 wave 136, lane L4: every class pair has its own
/// row in the per-VM record, and a fresh record holds no id and no form.
#[test]
fn i136_l4_every_form_class_has_a_record_row() {
    let rows: Vec<usize> = VH_REAL_VARFORM_CLASSES
        .iter()
        .map(|c| vh_real_varform_row(c).expect("row"))
        .collect();
    assert_eq!(rows, (0..VH_REAL_VARFORM_CLASSES.len()).collect::<Vec<_>>());
    let record = VhRealVarFormRecord::default();
    assert_eq!(record.ids.len(), VH_REAL_VARFORM_CLASSES.len());
    assert_eq!(record.forms.len(), VH_REAL_VARFORM_CLASSES.len());
    use std::sync::atomic::Ordering;
    assert!(record.ids.iter().flatten().all(|id| id.load(Ordering::Relaxed) == 0));
    assert!(record
        .forms
        .iter()
        .flatten()
        .flatten()
        .all(|f| f.load(Ordering::Relaxed) == 0));
    assert!(!record.defining.load(Ordering::Relaxed));
}

use super::*;

#[test]
fn gci_i40fg_every_read_made_is_no_miss() {
    assert_eq!(mn_static_field_miss(true, true, true, true), None);
}

#[test]
fn gci_i40fg_the_first_failing_step_is_named() {
    // A null slot before an unreadable one, `clazz` before `name`.
    assert_eq!(
        mn_static_field_miss(false, false, false, false),
        Some(MnStaticFieldMiss::NoClazz)
    );
    assert_eq!(
        mn_static_field_miss(true, false, false, false),
        Some(MnStaticFieldMiss::NoName)
    );
    assert_eq!(
        mn_static_field_miss(true, false, true, false),
        Some(MnStaticFieldMiss::MirrorUnmapped)
    );
    assert_eq!(
        mn_static_field_miss(true, false, true, true),
        Some(MnStaticFieldMiss::MirrorUnmapped)
    );
    assert_eq!(
        mn_static_field_miss(true, true, true, false),
        Some(MnStaticFieldMiss::NameUnreadable)
    );
}

/// gci i41/rv: `mn_static_field` decodes `name` only after the mirror
/// resolved, which is exact because the rule never asks `name_read` while
/// `class_known` is false (or a slot is null): every such answer is the
/// same with the name read or not.
#[test]
fn gci_i41rv_name_read_is_not_asked_before_the_mirror() {
    for has_clazz in [false, true] {
        for has_name in [false, true] {
            for class_known in [false, true] {
                if has_clazz && has_name && class_known {
                    continue;
                }
                assert_eq!(
                    mn_static_field_miss(has_clazz, class_known, has_name, false),
                    mn_static_field_miss(has_clazz, class_known, has_name, true),
                    "{has_clazz} {class_known} {has_name}"
                );
            }
        }
    }
    let src = LANG_INVOKE_SRC.replace("\r\n", "\n");
    assert!(src.contains(concat!(
        "    let name = if class_id.is_some() \u{7b}\n",
        "        name_obj.and_then(|n| ctx.read_string(n))\n"
    )));
}

#[test]
fn gci_i40fg_each_miss_has_its_own_token() {
    let tokens = [
        MnStaticFieldMiss::NoClazz.token(),
        MnStaticFieldMiss::NoName.token(),
        MnStaticFieldMiss::MirrorUnmapped.token(),
        MnStaticFieldMiss::NameUnreadable.token(),
    ];
    for (i, a) in tokens.iter().enumerate() {
        for b in &tokens[i + 1..] {
            assert_ne!(a, b);
        }
    }
    assert_eq!(MnStaticFieldMiss::NameUnreadable.token(), "name-unreadable");
    assert_eq!(MnStaticFieldMiss::MirrorUnmapped.token(), "mirror-unmapped");
}

use super::{label_names_method, split_frame_label};

fn split_reference(label: &str) -> Option<(&str, &str)> {
    let (owner_and_method, _descriptor) = label.rsplit_once(':')?;
    let (class_name, method_name) = owner_and_method.rsplit_once('.')?;
    if class_name.is_empty() || method_name.is_empty() {
        return None;
    }
    Some((class_name, method_name))
}

fn names_reference(label: &str, class: &str, method: &str, descriptor: &str) -> bool {
    let Some((owner_and_method, desc)) = label.rsplit_once(':') else {
        return false;
    };
    let Some((cls, m)) = owner_and_method.rsplit_once('.') else {
        return false;
    };
    m == method && desc == descriptor && cls == class    }

const LABELS: &[&str] = &[
    "java/lang/Throwable.fillInStackTrace:()Ljava/lang/Throwable;",
    "java/lang/RuntimeException.<init>:(Ljava/lang/String;)V",
    "p/Outer$Inner.run:()V",
    "Top.main:([Ljava/lang/String;)V",
    "p/Odd:Name.m:()V",
    "p/C.m:(Lq/Has:Colon;)V",
    "p/C.m:",
    ".m:()V",
    "p/C.:()V",
    "noseparators",
    "p/C.m",
    "",
    ":",
    "a.b:c.d:e",
    "été/Clässe.méthode:()V",
];

#[test]
fn the_byte_split_is_the_rsplit_once_split() {
    for label in LABELS {
        assert_eq!(
            split_frame_label(label),
            split_reference(label),
            "{label:?}"
        );
    }
}

#[test]
fn the_scan_free_match_is_the_split_and_compare_match() {
    let names: &[(&str, &str, &str)] = &[
        (
            "java/lang/Throwable",
            "fillInStackTrace",
            "()Ljava/lang/Throwable;",
        ),
        (
            "java/lang/RuntimeException",
            "<init>",
            "(Ljava/lang/String;)V",
        ),
        ("java/lang/Throwable", "<init>", "(Ljava/lang/String;)V"),
        ("p/Odd:Name", "m", "()V"),
        ("p/C", "m", "(Lq/Has:Colon;)V"),
        ("p/C", "m", "()V"),
        ("a.b:c", "d", "e"),
        ("a", "b:c.d", "e"),
        ("", "m", "()V"),
        ("p/C", "", "()V"),
        ("été/Clässe", "méthode", "()V"),
    ];
    for label in LABELS {
        for (class, method, descriptor) in names {
            assert_eq!(
                label_names_method(label, class, method, descriptor),
                names_reference(label, class, method, descriptor),
                "{label:?} vs {class}.{method}:{descriptor}"
            );
        }
    }
}

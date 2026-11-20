use super::{p57_note_open_option_name, p57_open_option_names_of_set_text, P57OpenFlags};

fn scan(text: &str) -> P57OpenFlags {
    let mut flags = P57OpenFlags::default();
    for name in p57_open_option_names_of_set_text(text) {
        p57_note_open_option_name(&mut flags, name);
    }
    flags
}

#[test]
fn create_new_is_not_create() {
    let f = scan("[WRITE, CREATE_NEW]");
    assert!(f.write && f.create_new);
    assert!(!f.create, "the substring scan read CREATE_NEW as CREATE too");
}

#[test]
fn a_user_option_named_like_a_standard_one_in_part_is_ignored() {
    let f = scan("[READ_ONLY_MIRROR, MY_WRITE]");
    assert_eq!(f, P57OpenFlags::default());
}

#[test]
fn every_standard_name_is_read() {
    let f = scan("[APPEND, CREATE, TRUNCATE_EXISTING, NOFOLLOW_LINKS, READ, WRITE]");
    assert!(f.append && f.create && f.truncate_existing && f.nofollow && f.read && f.write);
    assert!(!f.create_new);
    assert_eq!(scan("[]"), P57OpenFlags::default());
}

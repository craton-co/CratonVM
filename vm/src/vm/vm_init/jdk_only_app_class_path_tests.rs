use super::jdk_only_app_class_path_entry as entry;
use std::path::Path;

#[test]
fn a_relative_element_becomes_absolute_under_the_working_directory() {
    let cwd = std::env::current_dir().expect("cwd");
    let missing = entry("no-such-dir-fju/app.jar");
    assert!(Path::new(&missing).is_absolute(), "{missing}");
    assert!(missing.ends_with("app.jar"), "{missing}");
    let here = entry(".");
    assert!(Path::new(&here).is_absolute(), "{here}");
    assert_eq!(
        std::fs::canonicalize(&here).expect("canonical"),
        std::fs::canonicalize(&cwd).expect("canonical"),
        "`.` names the working directory"
    );
}

#[test]
fn elements_the_class_path_resolves_itself_pass_through() {
    for raw in ["", "lib/*", "*", "app.war!/WEB-INF/classes/"] {
        assert_eq!(entry(raw), raw);
    }
}

#[test]
fn an_absolute_element_stays_absolute() {
    let abs = std::env::temp_dir().join("fju-missing.jar");
    let out = entry(&abs.to_string_lossy());
    assert!(Path::new(&out).is_absolute(), "{out}");
    assert!(out.ends_with("fju-missing.jar"), "{out}");
}

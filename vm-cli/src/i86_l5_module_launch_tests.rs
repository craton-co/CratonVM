use super::{normalize_java_launcher_argv, rewrite_module_launch};

fn argv(tokens: &[&str]) -> Vec<String> {
    tokens.iter().map(|t| t.to_string()).collect()
}

/// Interpreter round i4 wave 86, lane L5: no module launch, argv as given
/// (a bare main class, `-jar`, or a `-m` past the program selector).
#[test]
fn argv_without_a_module_launch_is_unchanged() {
    for case in [
        argv(&["java", "-cp", "out", "Main", "-m", "x"]),
        argv(&["java", "-jar", "app.jar", "-m", "x"]),
        argv(&["java", "--", "-m", "x"]),
        argv(&["java", "-version"]),
    ] {
        assert_eq!(rewrite_module_launch(case.clone()).as_ref(), Ok(&case));
    }
}

/// An explicit `<module>/<class>` over a module-path module: the root
/// module, the class as the selector, the rest as program arguments.
#[test]
fn an_explicit_main_class_becomes_the_selector() {
    let dir = std::env::temp_dir().join(format!("i86-l5-launch-{}", std::process::id()));
    let root = dir.join("m.x");
    std::fs::create_dir_all(root.join("p")).unwrap();
    // module m.x { exports p; } with ModulePackages [p], hand-assembled.
    let module_info: Vec<u8> = {
        let mut b: Vec<u8> = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 69];
        // cp: 1 Utf8 module-info, 2 Class #1, 3 Utf8 m.x, 4 Module #3,
        // 5 Utf8 java.base, 6 Module #5, 7 Utf8 p, 8 Package #7,
        // 9 Utf8 Module, 10 Utf8 ModulePackages
        b.extend_from_slice(&[0, 11]);
        let utf = |b: &mut Vec<u8>, s: &str| {
            b.push(1);
            b.extend_from_slice(&(s.len() as u16).to_be_bytes());
            b.extend_from_slice(s.as_bytes());
        };
        utf(&mut b, "module-info");
        b.extend_from_slice(&[7, 0, 1]);
        utf(&mut b, "m.x");
        b.extend_from_slice(&[19, 0, 3]);
        utf(&mut b, "java.base");
        b.extend_from_slice(&[19, 0, 5]);
        utf(&mut b, "p");
        b.extend_from_slice(&[20, 0, 7]);
        utf(&mut b, "Module");
        utf(&mut b, "ModulePackages");
        // access ACC_MODULE, this #2, super 0, interfaces 0, fields 0,
        // methods 0, attributes 2.
        b.extend_from_slice(&[0x80, 0x00, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        // Module attribute: name 4, flags 0, version 0; requires 1:
        // (6, MANDATED, 0); exports 1: (8, 0, 0); opens 0; uses 0; provides 0.
        let body: Vec<u8> = vec![
            0, 4, 0, 0, 0, 0, 0, 1, 0, 6, 0x80, 0, 0, 0, 0, 1, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0,
        ];
        b.extend_from_slice(&[0, 9]);
        b.extend_from_slice(&(body.len() as u32).to_be_bytes());
        b.extend_from_slice(&body);
        b.extend_from_slice(&[0, 10, 0, 0, 0, 4, 0, 1, 0, 8]);
        b
    };
    std::fs::write(root.join("module-info.class"), module_info).unwrap();
    std::fs::write(root.join("p").join("C.class"), b"not read").unwrap();
    let path = dir.to_string_lossy().into_owned();
    let out = rewrite_module_launch(argv(&["java", "-p", &path, "-m", "m.x/p.C", "a", "b"]))
        .expect("a module-path module with the class");
    assert_eq!(
        out,
        argv(&[
            "java",
            "-p",
            &path,
            "-Djdk.module.main=m.x",
            "--add-modules",
            "m.x",
            "p.C",
            "--",
            "a",
            "b"
        ])
    );
    let refused = rewrite_module_launch(argv(&["java", "-p", &path, "-m", "m.x/p.Nope"]));
    assert_eq!(
        refused,
        Err("Error: Could not find or load main class p.Nope in module m.x".to_string())
    );
    let no_main = rewrite_module_launch(argv(&["java", "-p", &path, "-m", "m.x"]));
    assert_eq!(
        no_main,
        Err("module m.x does not have a ModuleMainClass attribute, use -m \
             <module>/<main-class>"
            .to_string())
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Interpreter round i6 wave 116, lane L5: the launcher's boot-layer
/// checks take the JDK the VM resolves, an explicit `--java-home` first
/// and authoritative (a missing explicit directory resolves to nothing,
/// not to another JDK).
#[test]
fn i116_the_launcher_checks_resolve_the_vms_java_home() {
    let dir = std::env::temp_dir().join(format!("i116-l5-home-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let explicit = dir.to_string_lossy().into_owned();
    assert_eq!(super::launcher_java_home(Some(explicit.clone())), Some(explicit));
    let missing = dir.join("absent").to_string_lossy().into_owned();
    assert_eq!(super::launcher_java_home(Some(missing)), None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// HotSpot's short `-p` reaches clap as `--module-path`.
#[test]
fn short_p_is_the_module_path() {
    let out = normalize_java_launcher_argv(argv(&["java", "-p", "mods", "Main"]));
    assert_eq!(out, argv(&["java", "--module-path", "mods", "Main"]));
}

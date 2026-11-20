use super::*;
use std::io::Write;

/// A JDK home with `jmods/java.base.jmod` and `lib/modules` whose
/// `java.base` differs the way jlink's plugins make it differ: a class
/// regenerated (`p/Holder`), a class only the image has (`p/Species_X`),
/// a class both have alike (`p/Same`), and a different `module-info`.
fn fake_jdk(tag: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!(
        "cratonvm_zs4rp_{}_{}_{tag}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(home.join("jmods")).unwrap();
    fs::create_dir_all(home.join("lib")).unwrap();
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in [
        (
            "classes/p/Holder.class",
            b"\xCA\xFE\xBA\xBE_jmod_holder".as_ref(),
        ),
        ("classes/p/Same.class", b"\xCA\xFE\xBA\xBE_same".as_ref()),
        (
            "classes/q/OnlyJmod.class",
            b"\xCA\xFE\xBA\xBE_only_jmod".as_ref(),
        ),
        (
            "classes/module-info.class",
            b"\xCA\xFE\xBA\xBE_jmod_module_info".as_ref(),
        ),
    ] {
        zip.start_file(name, options).unwrap();
        zip.write_all(bytes).unwrap();
    }
    let mut jmod = vec![0x4A, 0x4D, 0x01, 0x00];
    jmod.extend_from_slice(&zip.finish().unwrap().into_inner());
    fs::write(home.join("jmods").join("java.base.jmod"), jmod).unwrap();
    let image = cratonvm_reader::jimage::test_builder::build_simple(&[
        (
            "java.base",
            "p",
            "Holder",
            "class",
            b"\xCA\xFE\xBA\xBE_image_holder".as_ref(),
        ),
        (
            "java.base",
            "p",
            "Species_X",
            "class",
            b"\xCA\xFE\xBA\xBE_image_species".as_ref(),
        ),
        (
            "java.base",
            "p",
            "Same",
            "class",
            b"\xCA\xFE\xBA\xBE_same".as_ref(),
        ),
        (
            "java.base",
            "",
            "module-info",
            "class",
            b"\xCA\xFE\xBA\xBE_image_module_info".as_ref(),
        ),
    ]);
    fs::write(home.join("lib").join("modules"), image).unwrap();
    home
}

fn boot_path(home: &Path, on: bool) -> ClassPath {
    let jmod = home.join("jmods").join("java.base.jmod");
    let mut cp = ClassPath::new(&[jmod.to_string_lossy().into_owned()]);
    cp.set_runtime_image_classes(on);
    cp
}

fn bytes(cp: &ClassPath, name: &str) -> Option<Vec<u8>> {
    cp.find_class(name).ok().map(|b| b.as_ref().to_vec())
}

#[test]
fn zs4_rp_jmod_boot_path_serves_the_runtime_images_classes() {
    let home = fake_jdk("on");
    let cp = boot_path(&home, true);
    // The image's regenerated class wins over the JMOD's.
    assert_eq!(
        bytes(&cp, "p/Holder").unwrap(),
        b"\xCA\xFE\xBA\xBE_image_holder"
    );
    // A class only the image has resolves (its package's module).
    assert_eq!(
        bytes(&cp, "p/Species_X").unwrap(),
        b"\xCA\xFE\xBA\xBE_image_species"
    );
    assert_eq!(bytes(&cp, "p/Same").unwrap(), b"\xCA\xFE\xBA\xBE_same");
    // A class only the JMOD has still resolves from the JMOD.
    assert_eq!(
        bytes(&cp, "q/OnlyJmod").unwrap(),
        b"\xCA\xFE\xBA\xBE_only_jmod"
    );
    // Nothing invents a class neither has, in a known package or not.
    assert!(matches!(
        cp.find_class("p/Missing"),
        Err(ClassFileError::ClassNotFound { .. })
    ));
    assert!(cp.find_class("r/Missing").is_err());
    // Module descriptors stay the JMOD's.
    assert_eq!(
        bytes(&cp, "module-info").unwrap(),
        b"\xCA\xFE\xBA\xBE_jmod_module_info"
    );
    assert_eq!(
        cp.scan_module_infos(),
        vec![b"\xCA\xFE\xBA\xBE_jmod_module_info".to_vec()]
    );
    let _ = fs::remove_dir_all(&home);
}

/// The kill switch (`=0`) is the JMOD-only class path it replaced: the
/// positive control for the rows above.
#[test]
fn zs4_rp_kill_switch_serves_the_jmods_only() {
    let home = fake_jdk("off");
    let cp = boot_path(&home, false);
    assert_eq!(
        bytes(&cp, "p/Holder").unwrap(),
        b"\xCA\xFE\xBA\xBE_jmod_holder"
    );
    assert!(matches!(
        cp.find_class("p/Species_X"),
        Err(ClassFileError::ClassNotFound { .. })
    ));
    let _ = fs::remove_dir_all(&home);
}

/// A JMOD with no runtime image beside it is served as before with the
/// switch on.
#[test]
fn zs4_rp_a_jmod_without_an_image_is_unchanged() {
    let home = fake_jdk("noimage");
    fs::remove_file(home.join("lib").join("modules")).unwrap();
    let cp = boot_path(&home, true);
    assert_eq!(
        bytes(&cp, "p/Holder").unwrap(),
        b"\xCA\xFE\xBA\xBE_jmod_holder"
    );
    assert!(cp.find_class("p/Species_X").is_err());
    let _ = fs::remove_dir_all(&home);
}

/// The host JDK's home, when it has both `jmods/` and `lib/modules`.
fn host_jdk_with_jmods_and_image() -> Option<PathBuf> {
    let output = std::process::Command::new("java")
        .args(["-XshowSettings:properties", "-version"])
        .output()
        .ok()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    let home = stderr.lines().find_map(|line| {
        let value = line.trim().strip_prefix("java.home")?.trim();
        Some(PathBuf::from(value.strip_prefix('=')?.trim()))
    })?;
    (home.join("jmods").join("java.base.jmod").is_file()
        && home.join("lib").join("modules").is_file())
    .then_some(home)
}

/// The real JDK: the jlink-generated `java.base` classes answer the
/// image's bytes (what HotSpot runs), the others are unchanged. Skips
/// (with a line on stderr) on a host JDK without `jmods/`.
#[test]
fn zs4_rp_real_jdk_jlink_generated_classes_come_from_the_image() {
    let Some(home) = host_jdk_with_jmods_and_image() else {
        eprintln!("zs4_rp: SKIP, no host JDK with jmods/ and lib/modules");
        return;
    };
    let on = boot_path(&home, true);
    let off = boot_path(&home, false);
    let species = "java/lang/invoke/BoundMethodHandle$Species_LL";
    assert!(
        on.find_class(species).is_ok(),
        "{species} with the switch on"
    );
    assert!(
        off.find_class(species).is_err(),
        "{species} with the switch off"
    );
    let holder = "java/lang/invoke/DirectMethodHandle$Holder";
    let image = cratonvm_reader::JImageReader::open(home.join("lib").join("modules"))
        .unwrap()
        .find_class("java.base", holder)
        .unwrap()
        .unwrap();
    assert_eq!(bytes(&on, holder).unwrap(), image);
    assert_ne!(bytes(&off, holder).unwrap(), image);
    assert_eq!(
        bytes(&on, "java/lang/Object"),
        bytes(&off, "java/lang/Object")
    );
    // s4 follow-up: `Invokers` and its regenerated holder, the outer whose
    // `getDeclaredClasses` the host row reads.
    let reader = cratonvm_reader::JImageReader::open(home.join("lib").join("modules")).unwrap();
    for name in [
        "java/lang/invoke/Invokers",
        "java/lang/invoke/Invokers$Holder",
    ] {
        let image = reader.find_class("java.base", name).unwrap().unwrap();
        assert_eq!(
            bytes(&on, name).unwrap(),
            image,
            "{name} with the switch on"
        );
    }
}

/// The launcher's two `java.home` shapes (`discover_boot_classpath`): a
/// home WITH `jmods/` boots from the JMODs (java.base first, then the
/// others), and the image serves their classes under the switch; a home
/// WITHOUT `jmods/` (a JDK linked without them, JEP 493, or a JRE) boots
/// from `lib/modules` itself, so it has the image's bytes whatever the
/// switch says -- the switch has nothing to do there.
#[test]
fn zs4_rp_both_launcher_java_home_shapes_serve_the_image_holder() {
    let home = fake_jdk("shapes");
    let jmods = home.join("jmods").join("java.base.jmod");
    let modules = home.join("lib").join("modules");
    for on in [true, false] {
        let mut cp = ClassPath::new(&[jmods.to_string_lossy().into_owned()]);
        cp.set_runtime_image_classes(on);
        let want: &[u8] = if on {
            b"\xCA\xFE\xBA\xBE_image_holder"
        } else {
            b"\xCA\xFE\xBA\xBE_jmod_holder"
        };
        assert_eq!(bytes(&cp, "p/Holder").unwrap(), want, "jmods home, on={on}");
        let mut cp = ClassPath::new(&[modules.to_string_lossy().into_owned()]);
        cp.set_runtime_image_classes(on);
        assert_eq!(
            bytes(&cp, "p/Holder").unwrap(),
            b"\xCA\xFE\xBA\xBE_image_holder",
            "image-only home, on={on}"
        );
        assert!(cp.find_class("p/Species_X").is_ok());
    }
    let _ = fs::remove_dir_all(&home);
}

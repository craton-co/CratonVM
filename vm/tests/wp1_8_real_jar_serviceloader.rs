// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP1.8-finish — exercise the **real JAR-classpath path** of
//! `ServiceLoader.load(java.sql.Driver.class).iterator()`.
//!
//! Roadmap reference: `gaps/wildfly-ejbca-roadmap.md` Wave 1 §4 (WP1.8).
//!
//! The companion `wp1_8_serviceloader_e2e.rs` covers the iterator chain
//! end-to-end but stages its `META-INF/services/java.sql.Driver`
//! descriptor in a **directory** classpath entry (`tests/resources/...`
//! plus a temp dir). That never runs through the JAR-decoding branch of
//! `ClassPath::find_all_resource_bytes` and therefore never proves the
//! WP1.8 acceptance bar verbatim:
//!
//! > `ServiceLoader.load(java.sql.Driver.class)` finds **H2 (or any
//! > driver JAR)** via `META-INF/services` on classpath.
//!
//! Strategy:
//!   1. At test setup, synthesise a real `.jar` (via the `zip` crate so
//!      this works without `jar` on PATH) containing
//!         - `META-INF/services/java.sql.Driver` listing the fixture's
//!           FakeDriver FQN, and
//!         - the compiled `Wp18ServiceLoaderE2E.class` +
//!           `Wp18ServiceLoaderE2E$FakeDriver.class` files.
//!   2. Boot the VM with **only that JAR** on the classpath — no
//!      directory entries — so every class-load AND every
//!      `find_all_resource_bytes` call must traverse the JarFile
//!      classpath flavour.
//!   3. Invoke `Wp18ServiceLoaderE2E.serviceLoaderIteratorCount()` and
//!      assert it returns `>0`.
//!
//! Reuses the existing `Wp18ServiceLoaderE2E` fixture (so we share its
//! `FakeDriver` and the `serviceLoaderIteratorCount` entry point); only
//! the JAR-staging strategy is new.

use std::io::Write;

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

const FIXTURE_CLASS: &str = "cratonvm/Wp18ServiceLoaderE2E";

fn test_resources_dir() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/tests/resources")
}

fn fixture_class_bytes() -> Option<(Vec<u8>, Vec<u8>)> {
    let outer = format!(
        "{}/cratonvm/Wp18ServiceLoaderE2E.class",
        test_resources_dir()
    );
    let inner = format!(
        "{}/cratonvm/Wp18ServiceLoaderE2E$FakeDriver.class",
        test_resources_dir()
    );
    let outer_bytes = std::fs::read(&outer).ok()?;
    let inner_bytes = std::fs::read(&inner).ok()?;
    Some((outer_bytes, inner_bytes))
}

/// Materialize a real `.jar` file on disk containing both the
/// `META-INF/services/java.sql.Driver` descriptor AND the fixture
/// classes. Returns the path to the JAR; the surrounding tempdir is
/// leaked via `mem::forget` so the JAR survives until process exit.
///
/// Uses `zip::ZipWriter` (already a `vm` crate dep — see
/// `vm/src/runtime/agent_loader.rs` for the established pattern) so the
/// test does NOT depend on a `jar` binary being on PATH.
fn make_spi_classpath_jar(outer_class: &[u8], inner_class: &[u8]) -> std::path::PathBuf {
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    let dir = tempfile::TempDir::new().expect("create temp dir for SPI jar");
    let jar_path = dir.path().join("wp18-fake-driver.jar");

    let file = std::fs::File::create(&jar_path).expect("create jar file");
    let mut zip = ZipWriter::new(file);
    let opts: SimpleFileOptions =
        SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);

    // 1. The SPI descriptor — load-bearing for ServiceLoader discovery.
    zip.start_file("META-INF/services/java.sql.Driver", opts)
        .expect("start META-INF/services/java.sql.Driver");
    zip.write_all(b"cratonvm.Wp18ServiceLoaderE2E$FakeDriver\n")
        .expect("write SPI descriptor");

    // 2. The fixture's outer class — needed so vm.invoke can resolve
    //    `cratonvm/Wp18ServiceLoaderE2E` purely from the JAR classpath.
    zip.start_file("cratonvm/Wp18ServiceLoaderE2E.class", opts)
        .expect("start outer class");
    zip.write_all(outer_class).expect("write outer class");

    // 3. The FakeDriver inner class — needed so the iterator's
    //    Class.forName("cratonvm.Wp18ServiceLoaderE2E$FakeDriver")
    //    resolves from JAR bytes.
    zip.start_file("cratonvm/Wp18ServiceLoaderE2E$FakeDriver.class", opts)
        .expect("start inner class");
    zip.write_all(inner_class).expect("write inner class");

    zip.finish().expect("finish jar");

    // Leak the TempDir so the JAR file survives the function return.
    std::mem::forget(dir);
    jar_path
}

/// Can this build run the JAR probe at all?
///
/// `VmConfig::new()` is deliberately `JdkMode::Synthetic` on the embedding/test
/// path, and synthetic mode is only usable in a build carrying the
/// `synthetic-jdk` Cargo feature. Without it the VM boots a shim class library
/// and the fixture's very first statement — `ServiceLoader.load(Driver.class)`
/// — cannot resolve, so the probe reports `-99` and its panic message sends the
/// reader to `Class.forName` / `BufferedReader` resolution, neither of which
/// ever ran.
///
/// Same guard, and the same reason, as
/// `wp7_2_jdbc_core_types_reachable::synthetic_library_available`.
///
/// Observed while diagnosing this, NOT filed as a defect because it has only
/// been seen in this unsupported configuration: the `NoSuchMethodError` for the
/// unresolvable static named `java.lang.Class` as the owner —
/// `'java.util.ServiceLoader java.lang.Class.load(java.lang.Class)'` — where
/// the bytecode's owner is `java/util/ServiceLoader` and `java.lang.Class` is
/// the PARAMETER's type. Worth re-checking against a `--features synthetic-jdk`
/// build before treating it as real.
fn synthetic_library_available() -> bool {
    if !cratonvm_vm::config::SYNTHETIC_JDK_COMPILED_IN {
        eprintln!(
            "Skipping WP1.8 JAR probe: this build has no `synthetic-jdk`              feature, and `VmConfig::new()` boots JdkMode::Synthetic — the              probe would measure a shim class library, not the JAR walk"
        );
        return false;
    }
    true
}

fn vm_with_jar(jar: &std::path::Path) -> Vm {
    // Note: NO directory classpath entries — the JAR is the entire
    // classpath surface for the fixture and its SPI descriptor. This
    // forces every resource and class lookup through the JarFile
    // flavour of `ClassPath::find_all_resource_bytes` /
    // `find_class_bytes`, which is the WP1.8 acceptance bar.
    let cp = vec![jar.to_string_lossy().into_owned()];
    let config = VmConfig::new().with_classpath(cp);
    Vm::new(config)
}

// ---------------------------------------------------------------------------
// Acceptance — JAR-classpath end-to-end
// ---------------------------------------------------------------------------

/// WP1.8 acceptance bar (verbatim): a driver advertised in
/// `META-INF/services/java.sql.Driver` inside a **real `.jar`** on the
/// classpath is reported by
/// `ServiceLoader.load(java.sql.Driver.class).iterator()`.
///
/// This proves the JAR-decoding path of `find_all_resource_bytes`
/// (called from `service_loader.rs::discover_providers`) works against
/// a real ZIP container — the directory-classpath equivalent in
/// `wp1_8_serviceloader_e2e.rs` does not exercise that path.
#[test]
fn driver_discovered_from_jar_on_classpath() {
    if !synthetic_library_available() {
        return;
    }
    let (outer, inner) = match fixture_class_bytes() {
        Some(v) => v,
        None => {
            eprintln!(
                "Skipping: Wp18ServiceLoaderE2E.class not available \
                 (javac not on PATH at build time?)"
            );
            return;
        }
    };

    let jar = make_spi_classpath_jar(&outer, &inner);

    // Sanity: the file we just wrote is on disk and has content.
    let meta = std::fs::metadata(&jar).expect("jar metadata");
    assert!(
        meta.len() > 0,
        "synthesised jar at {} is empty",
        jar.display()
    );

    let mut vm = vm_with_jar(&jar);

    let result = vm.invoke(FIXTURE_CLASS, "serviceLoaderIteratorCount", "()I", &[]);
    match result {
        Ok(Some(Value::Int(n))) if n > 0 => {
            // Acceptance satisfied: at least one provider was discovered
            // by walking META-INF/services from inside a JAR file.
        }
        Ok(Some(Value::Int(0))) => panic!(
            "ServiceLoader.iterator() returned 0 — JAR-side \
             META-INF/services/java.sql.Driver was not walked. Check \
             that ClassPath::find_all_resource_bytes traverses \
             ClassPathEntry::JarFile entries for the JAR at {}.",
            jar.display(),
        ),
        Ok(Some(Value::Int(-99))) => panic!(
            "ServiceLoader fixture caught a Throwable — \
             check Class.forName / BufferedReader.<init> resolution \
             when the fixture classes themselves are loaded from the JAR.",
        ),
        other => panic!("serviceLoaderIteratorCount expected Ok(Some(Int(n>0))), got: {other:?}",),
    }
}

/// Anchor: the synthesised JAR must contain the SPI descriptor at the
/// correct path. Catches a regression where the test scaffolding
/// changes the entry name and silently no-ops the JAR-side path.
#[test]
fn synthesised_jar_contains_spi_descriptor() {
    let (outer, inner) = match fixture_class_bytes() {
        Some(v) => v,
        None => {
            eprintln!(
                "Skipping: Wp18ServiceLoaderE2E.class not available \
                 (javac not on PATH at build time?)"
            );
            return;
        }
    };

    let jar = make_spi_classpath_jar(&outer, &inner);
    let bytes = std::fs::read(&jar).expect("read synthesised jar");
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor).expect("open jar as zip");

    // The SPI descriptor entry must exist.
    let mut entry = archive
        .by_name("META-INF/services/java.sql.Driver")
        .expect("META-INF/services/java.sql.Driver entry present in jar");
    let mut content = String::new();
    use std::io::Read;
    entry
        .read_to_string(&mut content)
        .expect("read SPI descriptor entry");
    assert!(
        content.contains("cratonvm.Wp18ServiceLoaderE2E$FakeDriver"),
        "SPI descriptor missing FakeDriver FQN, got: {content:?}",
    );
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression for Spring Boot Jetty's `LoaderHidingResourceTests` fixture.
//!
//! Its WAR contains only explicit, zero-content `JarOutputStream` entries.
//! Mounting that freshly-written archive through `jar:` must still enumerate
//! its directories and find the zero-length asset, in both execution modes.

use std::path::{Path, PathBuf};
use std::process::Command;

const CLASS_NAME: &str = "JarFileSystemZeroEntryListingProbe";

const SOURCE: &str = r#"
import java.io.FileOutputStream;
import java.net.URI;
import java.nio.file.FileSystem;
import java.nio.file.FileSystems;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.Collections;
import java.util.Set;
import java.util.TreeSet;
import java.util.jar.JarOutputStream;
import java.util.stream.Collectors;
import java.util.zip.ZipEntry;

public class JarFileSystemZeroEntryListingProbe {
    private static void entry(JarOutputStream out, String name) throws Exception {
        out.putNextEntry(new ZipEntry(name));
        out.closeEntry();
    }

    private static Set<String> names(Path dir) throws Exception {
        try (var stream = Files.list(dir)) {
            return stream.map(path -> path.getFileName().toString())
                .collect(Collectors.toCollection(TreeSet::new));
        }
    }

    public static void main(String[] args) throws Exception {
        Path war = Files.createTempFile("cratonvm-loader-hiding-", ".war");
        try {
            try (JarOutputStream out = new JarOutputStream(new FileOutputStream(war.toFile()))) {
                entry(out, "org/");
                entry(out, "org/springframework/");
                entry(out, "org/springframework/boot/");
                entry(out, "org/springframework/boot/Loader.class");
                entry(out, "assets/");
                entry(out, "assets/image.jpg");
            }
            URI uri = URI.create("jar:" + war.toUri() + "!/");
            try (FileSystem fs = FileSystems.newFileSystem(uri, Collections.emptyMap())) {
                Path uriRoot = Paths.get(uri);
                if (!uriRoot.toAbsolutePath().toString().equals("/")) {
                    throw new AssertionError("uriRoot display=" + uriRoot.toAbsolutePath());
                }
                if (!names(uriRoot).equals(Set.of("assets", "org"))) {
                    throw new AssertionError("uriRoot=" + names(uriRoot));
                }
                if (!names(fs.getPath("/")).equals(Set.of("assets", "org"))) {
                    throw new AssertionError("root=" + names(fs.getPath("/")));
                }
                if (!names(fs.getPath("/assets")).equals(Set.of("image.jpg"))) {
                    throw new AssertionError("assets=" + names(fs.getPath("/assets")));
                }
                if (!Files.exists(fs.getPath("/assets/image.jpg"))) {
                    throw new AssertionError("missing zero-length image");
                }
                if (!fs.getPath("/assets/image.jpg").toAbsolutePath().toString().equals("/assets/image.jpg")) {
                    throw new AssertionError("image display=" + fs.getPath("/assets/image.jpg").toAbsolutePath());
                }
                if (Files.exists(fs.getPath("/assets/non-existent.jpg"))) {
                    throw new AssertionError("unexpected missing asset");
                }
            }
            System.out.println("JAR_FILESYSTEM_ZERO_ENTRY_LISTING_PASS");
        } finally {
            Files.deleteIfExists(war);
        }
    }
}
"#;

fn java_home() -> Option<PathBuf> {
    for variable in ["CRATONVM_TEST_JAVA_HOME", "CRATONVM_JAVA_HOME", "JAVA_HOME"] {
        if let Ok(value) = std::env::var(variable) {
            let path = PathBuf::from(value);
            if path
                .join("bin")
                .join(if cfg!(windows) { "java.exe" } else { "java" })
                .exists()
            {
                return Some(path);
            }
        }
    }
    [
        "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot",
        "C:/Program Files/Java/jdk-25",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|path| {
        path.join("bin")
            .join(if cfg!(windows) { "java.exe" } else { "java" })
            .exists()
    })
}

fn compile_probe(java_home: &Path) -> Option<PathBuf> {
    let dir =
        std::env::temp_dir().join(format!("cratonvm-jarfs-zero-entry-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;
    let source = dir.join(format!("{CLASS_NAME}.java"));
    std::fs::write(&source, SOURCE).expect("write probe source");
    let out = match Command::new(java_home.join("bin").join(if cfg!(windows) {
        "javac.exe"
    } else {
        "javac"
    }))
    .arg("--release")
    .arg("21")
    .arg("-d")
    .arg(&dir)
    .arg(&source)
    .output()
    {
        Ok(o) => o,
        // javac cannot be launched at all — the one legitimate skip.
        Err(e) => {
            eprintln!("[jarfs-zero-entry] javac could not be executed: {e}; skipping");
            return None;
        }
    };
    // javac REJECTED THE ARGUMENTS, not the source: an unsupported `--release`
    // means this javac is older than the level this probe compiles at, so it never
    // opened the file. That is a missing-toolchain condition — the same one the
    // `Err(e)` arm above skips for — not a broken probe. Reporting it as "fix the
    // probe source" sends the next reader to edit a correct `.java` file.
    //
    // Narrowly keyed on javac's own wording for an unsupported release, so a
    // genuine source error still reaches the assertion below and still fails loudly
    // (see `probe_compile_guard.rs` for why that must never become a skip).
    if !out.status.success() {
        let stderr_probe = String::from_utf8_lossy(&out.stderr);
        if stderr_probe.contains("release version") && stderr_probe.contains("not supported") {
            eprintln!(
                "[jar_filesystem_zero_entry_listing] javac cannot target --release 21 ({}); skipping. Point \
                 JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    // javac RAN and rejected the source: the probe is broken, and skipping here
    // would make this test a permanent vacuous pass.
    assert!(
        out.status.success(),
        "[jarfs-zero-entry] the embedded probe failed to compile — fix the probe \
         source. javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

#[test]
fn jar_filesystem_lists_fresh_zero_content_entries_in_jit_and_nojit() {
    let Some(java_home) = java_home() else {
        eprintln!("[jarfs-zero-entry] JDK unavailable; skipping");
        return;
    };
    let Some(classes) = compile_probe(&java_home) else {
        eprintln!("[jarfs-zero-entry] javac failed; skipping");
        return;
    };
    let Some(binary) = std::env::var("CRATONVM_BIN").ok().map(PathBuf::from) else {
        eprintln!("[jarfs-zero-entry] CRATONVM_BIN is required; skipping");
        return;
    };
    for nojit in [false, true] {
        let mut command = Command::new(&binary);
        command.arg("--java-home").arg(&java_home);
        if nojit {
            command.arg("--nojit");
        }
        let output = command
            .arg("-cp")
            .arg(&classes)
            .arg(CLASS_NAME)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "nojit={nojit} stdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(
            stdout.contains("JAR_FILESYSTEM_ZERO_ENTRY_LISTING_PASS"),
            "nojit={nojit} stdout:\n{stdout}"
        );
    }
}

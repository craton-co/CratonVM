use super::*;

pub(super) fn detect_java_home_from_path() -> Option<PathBuf> {
    // Resolve what the spawn would actually execute, then refuse if it is
    // this process. `cratonvm` installed (or built) as `java.exe` is a
    // supported configuration — Maven Surefire needs it — so this is a
    // routine case, not a defensive one, and getting it wrong recurses
    // without bound. See `resolve_java_executable`.
    let java = resolve_java_executable()?;
    if let Ok(this) = std::env::current_exe().and_then(std::fs::canonicalize) {
        let resolved = std::fs::canonicalize(&java).unwrap_or_else(|_| java.clone());
        if this == resolved {
            tracing::debug!(
                "skipping java.home auto-detection: `java` resolves to this \
                 executable ({}). Set JAVA_HOME or CRATONVM_JAVA_HOME, or pass \
                 --java-home, to point at a real JDK.",
                resolved.display()
            );
            return None;
        }
    }

    if let Some(home) = java_home_from_executable(&java) {
        tracing::info!("Auto-detected JAVA_HOME from PATH: {}", home.display());
        return Some(home);
    }

    // Try the flag variants in order of specificity
    let flag_variants = [
        "-XshowSettings:properties", // JDK 25+
        "-XshowSettings:property",   // JDK 9-24
        "-XshowSettings:all",        // universal fallback
    ];

    for flag in &flag_variants {
        // The resolved absolute path, never the bare name: a bare name is
        // re-resolved by `CreateProcessW` at spawn time, which is exactly
        // the step the guard above cannot see through.
        let output = std::process::Command::new(&java)
            .args([flag, "-version"])
            .output()
            .ok();

        let Some(output) = output else { continue };

        // JDK prints property settings to stderr
        let stderr = String::from_utf8_lossy(&output.stderr);

        // Skip if this variant wasn't recognized (JDK prints "Unrecognized")
        if stderr.contains("Unrecognized") {
            continue;
        }

        for line in stderr.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("java.home") {
                // Format: "java.home = /path/to/jdk"
                if let Some(value) = rest.trim().strip_prefix('=') {
                    let path = PathBuf::from(value.trim());
                    if path.is_dir() {
                        tracing::info!("Auto-detected JAVA_HOME from PATH: {}", path.display());
                        return Some(path);
                    }
                }
            }
        }
    }

    None
}

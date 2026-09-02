// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

#[allow(dead_code)]
mod build_script {
    include!("../build.rs");

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::fs;
        use std::path::{Path, PathBuf};
        use std::time::{SystemTime, UNIX_EPOCH};

        struct TempDir {
            path: PathBuf,
        }

        impl TempDir {
            fn new(label: &str) -> Self {
                let nonce = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("system clock is after Unix epoch")
                    .as_nanos();
                let path = std::env::temp_dir().join(format!(
                    "craton-gpu-build-script-{label}-{}-{nonce}",
                    std::process::id()
                ));
                fs::create_dir_all(&path).expect("create temp dir");
                Self { path }
            }

            fn path(&self) -> &Path {
                &self.path
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.path);
            }
        }

        fn manifest_dir(root: &Path) -> PathBuf {
            root.join("CratonVM").join("craton-gpu")
        }

        /// The pre-aggregator layout: `<checkout>/src/main/java`.
        ///
        /// Still one of the two the resolver accepts, and the one it picks
        /// when that directory actually exists — which is why the tests
        /// that CREATE it expect this.
        fn documented_java_src(root: &Path) -> PathBuf {
            root.join("craton-gpu-java")
                .join("src")
                .join("main")
                .join("java")
        }

        /// The layout gpu-java has had since it became a Maven aggregator
        /// on 2026-08-28: `<checkout>/craton-gpu/src/main/java`.
        ///
        /// `first_existing_layout` returns this one when NEITHER layout is
        /// present, deliberately, so that the `cargo:warning` names the
        /// path a current checkout would use rather than a path no
        /// checkout has had for months. A test for the nothing-exists case
        /// therefore expects this, not [`documented_java_src`].
        fn aggregator_java_src(root: &Path) -> PathBuf {
            root.join("craton-gpu-java")
                .join("craton-gpu")
                .join("src")
                .join("main")
                .join("java")
        }

        #[test]
        fn prepare_clean_dir_removes_stale_classes() {
            let temp = TempDir::new("clean");
            let classes_dir = temp.path().join("classes");
            let stale_class = classes_dir.join("craton").join("gpu").join("Stale.class");
            fs::create_dir_all(stale_class.parent().expect("stale class has parent"))
                .expect("create stale class parent");
            fs::write(&stale_class, b"stale").expect("write stale class");

            prepare_clean_dir(&classes_dir).expect("clean classes dir");

            assert!(classes_dir.is_dir());
            assert!(!stale_class.exists());
            assert_eq!(fs::read_dir(&classes_dir).expect("read classes").count(), 0);
        }

        #[test]
        fn resolves_documented_sibling_checkout_beside_workspace() {
            let temp = TempDir::new("sibling");
            let manifest = manifest_dir(temp.path());
            let expected = documented_java_src(temp.path());
            let old_in_workspace = temp
                .path()
                .join("CratonVM")
                .join("craton-gpu-java")
                .join("src")
                .join("main")
                .join("java");
            fs::create_dir_all(&manifest).expect("create manifest dir");
            fs::create_dir_all(&expected).expect("create documented source dir");
            fs::create_dir_all(old_in_workspace).expect("create old source dir");

            let resolution = resolve_java_root_from(None, &manifest, false);

            assert_eq!(
                resolution,
                JavaRootResolution {
                    path: expected,
                    invalid_override: None,
                    used_absolute_fallback: false,
                }
            );
        }

        #[test]
        fn invalid_override_is_reported_and_then_ignored() {
            let temp = TempDir::new("override");
            let manifest = manifest_dir(temp.path());
            let expected = documented_java_src(temp.path());
            let invalid_override = temp.path().join("missing-src");
            fs::create_dir_all(&manifest).expect("create manifest dir");
            fs::create_dir_all(&expected).expect("create documented source dir");

            let resolution = resolve_java_root_from(
                Some(invalid_override.clone().into_os_string()),
                &manifest,
                false,
            );

            assert_eq!(
                resolution,
                JavaRootResolution {
                    path: expected,
                    invalid_override: Some(invalid_override),
                    used_absolute_fallback: false,
                }
            );
        }

        /// The resolution the build now reports on.
        ///
        /// On Windows with no override and no sibling checkout, the
        /// resolver reaches an absolute install path that is a
        /// convention of one machine — see `platform_fallback_java_root`,
        /// whose own comment names the box. A build that lands here
        /// produces annotation classes from a directory this repository
        /// does not record, at a revision it does not pin, and said
        /// nothing about it. The flag is what lets `resolve_java_root`
        /// emit exactly one line of warning for that case and stay quiet
        /// for the two reproducible ones.
        #[test]
        fn windows_absolute_fallback_is_flagged_as_non_reproducible() {
            let temp = TempDir::new("winfallback");
            let manifest = manifest_dir(temp.path());
            fs::create_dir_all(&manifest).expect("create manifest dir");

            // Nothing beside the workspace, no override: the only
            // candidate left is the absolute one.
            let resolution = resolve_java_root_from(None, &manifest, true);

            assert!(
                resolution.used_absolute_fallback,
                "a Windows resolution that reached the absolute install path \
                 must be flagged so the build can say so: {resolution:?}"
            );
            assert_eq!(resolution.invalid_override, None);

            // …and the same inputs on a platform with no absolute
            // candidate must NOT be flagged, or the warning would fire on
            // every Linux build that simply has no sources.
            let elsewhere = resolve_java_root_from(None, &manifest, false);
            assert!(
                !elsewhere.used_absolute_fallback,
                "there is no machine-specific path to warn about off Windows: \
                 {elsewhere:?}"
            );
        }

        #[test]
        fn missing_non_windows_sources_fall_back_to_documented_sibling_path() {
            let temp = TempDir::new("missing");
            let manifest = manifest_dir(temp.path());
            // Nothing is created, so neither layout exists and the resolver
            // names the current one. See `aggregator_java_src`.
            let expected = aggregator_java_src(temp.path());
            fs::create_dir_all(&manifest).expect("create manifest dir");

            let resolution = resolve_java_root_from(None, &manifest, false);

            assert_eq!(
                resolution,
                JavaRootResolution {
                    path: expected,
                    invalid_override: None,
                    used_absolute_fallback: false,
                }
            );
        }
    }
}

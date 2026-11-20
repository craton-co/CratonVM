use cratonvm_native_api::{NativeKind, NativeMethodRegistry};
use cratonvm_types::compat::CompatibilityMode;

const URL_RESOURCE: (&str, &str, &str) = (
    "org/springframework/core/io/UrlResource",
    "getInputStream",
    "()Ljava/io/InputStream;",
);
const OPEN_STREAM: (&str, &str, &str) =
    ("java/net/URL", "openStream", "()Ljava/io/InputStream;");
/// A row of the same registrar that no table retires: the control.
const JAR_FILE_URL: (&str, &str, &str) = (
    "java/net/JarURLConnection",
    "getJarFileURL",
    "()Ljava/net/URL;",
);

/// The registry `register_re4_url_http` fills under `mode`, in the `Bridge`
/// scope its production caller gives it: under the ambient `SyntheticStub`
/// default `--real-jdk` would refuse every row, and the absence below would
/// prove nothing about the guard.
fn registry(mode: CompatibilityMode) -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    r.set_compatibility_mode(mode);
            super::register_re4_url_http(&mut r);
    r
}

/// `--real-jdk` runs Spring's own `UrlResource.getInputStream`, which asks
/// the URL's handler; `--compatible` keeps the native.
/// `JarURLConnection.getJarFileURL` is the control: it stays registered in
/// both modes. `URL.openStream` used to be the control; SBR wave 2 lane
/// `netaddr` retired it (`RETIRED_SHADOW_SBR_NETADDR_URL_TRIPLES`), so
/// `--real-jdk` now drops it too and `--compatible` keeps it.
#[test]
fn url_resource_get_input_stream_registers_in_compatible_mode_only() {
    let (c, m, d) = URL_RESOURCE;
    let strict = registry(CompatibilityMode::JdkOnly);
    assert!(
        strict.find(c, m, d).is_none(),
        "--real-jdk must leave {c}.{m}{d} to Spring's bytecode"
    );
    let (jc, jm, jd) = JAR_FILE_URL;
    assert!(
        strict.find(jc, jm, jd).is_some(),
        "control: {jc}.{jm}{jd} must still register under --real-jdk"
    );
    let (oc, om, od) = OPEN_STREAM;
    assert!(
        strict.find(oc, om, od).is_none(),
        "{oc}.{om}{od} is retired: --real-jdk runs the JDK's own body"
    );
    let compatible = cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_COMPAT_URL_APP_HANDLER", Some("0"))],
        || registry(CompatibilityMode::Compatible),
    );
    assert!(
        compatible.find(c, m, d).is_some(),
        "--compatible with CRATONVM_COMPAT_URL_APP_HANDLER=0 must keep {c}.{m}{d}"
    );
}

/// `CRATONVM_COMPAT_URL_APP_HANDLER` (interpreter round i1 wave 49, lane
/// L6) retires the shadow in `--compatible` as well, and the registry
/// reports the application handler authoritative there.
#[test]
fn url_resource_get_input_stream_retires_in_compatible_mode_under_the_switch() {
    let (c, m, d) = URL_RESOURCE;
    let compatible = cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_COMPAT_URL_APP_HANDLER", Some("1"))],
        || registry(CompatibilityMode::Compatible),
    );
    assert!(compatible.url_app_handler_is_authoritative());
    assert!(
        compatible.find(c, m, d).is_none(),
        "--compatible under the switch must leave {c}.{m}{d} to Spring's bytecode"
    );
    let (oc, om, od) = OPEN_STREAM;
    assert!(compatible.find(oc, om, od).is_some());
    let off = cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_COMPAT_URL_APP_HANDLER", Some("0"))],
        || registry(CompatibilityMode::Compatible),
    );
    assert!(!off.url_app_handler_is_authoritative());
    assert!(registry(CompatibilityMode::JdkOnly).url_app_handler_is_authoritative());
}

/// The `jar:nested:` text reader's spelling: a nested JAR stays two-level,
/// a nested DIRECTORY (`BOOT-INF/classes/`) becomes a single-level lookup
/// in the outer jar.
#[test]
fn nested_url_directory_entry_flattens_to_the_outer_jar() {
    assert_eq!(
        super::nested_url_as_jar_file(
            "/data/app.jar/!BOOT-INF/classes/!/smoketest/simple/ExitException.class"
        ),
        "jar:file:/data/app.jar!/BOOT-INF/classes/smoketest/simple/ExitException.class"
    );
    assert_eq!(
        super::nested_url_as_jar_file(
            "/data/app.jar/!BOOT-INF/lib/x.jar!/META-INF/spring.factories"
        ),
        "jar:file:/data/app.jar!/BOOT-INF/lib/x.jar!/META-INF/spring.factories"
    );
    assert_eq!(
        super::nested_url_as_jar_file("/data/app.jar/!BOOT-INF/lib/x.jar"),
        "jar:file:/data/app.jar!/BOOT-INF/lib/x.jar"
    );
}

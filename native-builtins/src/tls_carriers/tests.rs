use super::*;

/// Every method the Impl classes declare with `Code` has a row on the Impl: a copy of the row the old
/// (abstract) carrier reached, or a forwarder to the public class. A method without one runs the
/// Impl's own bytecode on a carrier that has no `conContext`.
#[test]
fn every_declared_impl_method_has_a_row() {
    let mut r = NativeMethodRegistry::new();
    crate::register_essential_natives(&mut r);
    let mut missing = Vec::new();
    for (imp, tables) in [
        (
            SSL_SOCKET_IMPL,
            vec![SSL_SOCKET_IMPL_METHODS, BASE_SSL_SOCKET_IMPL_METHODS],
        ),
        (SSL_SERVER_SOCKET_IMPL, vec![SSL_SERVER_SOCKET_IMPL_METHODS]),
        (
            SSL_SOCKET_FACTORY_IMPL,
            vec![SSL_SOCKET_FACTORY_IMPL_METHODS],
        ),
        (
            SSL_SERVER_SOCKET_FACTORY_IMPL,
            vec![SSL_SERVER_SOCKET_FACTORY_IMPL_METHODS],
        ),
    ] {
        for table in tables {
            for (m, d) in table {
                if r.resolve_id(imp, m, d).is_none() {
                    missing.push(format!("{imp}.{m}{d}"));
                }
            }
        }
    }
    assert!(
        missing.is_empty(),
        "declared methods with no row: {missing:#?}"
    );
}

/// Every row the module adds is a `SyntheticStub`, so `--real-jdk` drops it and the JDK's own object
/// runs its own bytecode.
#[test]
fn mirrored_rows_are_synthetic_stubs() {
    let mut r = NativeMethodRegistry::new();
    crate::register_essential_natives(&mut r);
    for (c, m, d, k) in r.dump_registrations() {
        if matches!(
            c,
            SSL_SOCKET_IMPL
                | SSL_SERVER_SOCKET_IMPL
                | SSL_SOCKET_FACTORY_IMPL
                | SSL_SERVER_SOCKET_FACTORY_IMPL
        ) {
            assert_eq!(k, NativeKind::Intrinsic, "{c}.{m}{d}");
        }
    }
}

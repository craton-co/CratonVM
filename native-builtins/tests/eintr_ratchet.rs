// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Every socket-backed `read_tls`/`write_tls` must be EINTR-transparent.
//!
//! A blocking `recv` on a socket carrying `SO_RCVTIMEO` is not restarted by
//! `SA_RESTART` (Linux `signal(7)`), and CratonVM signals its own threads —
//! `jit::xt_root_scan` `SIGUSR2`s every one of them for a cross-thread
//! stop-the-world root scan. So a TLS handshake parked in `read_tls` gets
//! `EINTR`, and until 2026-08-06 that reached Java as
//! `IOException: TLS handshake read: Interrupted system call (os error 4)`
//! (`JdkClientHttpRequestFactoryBuilderTests.connectWithSslBundle`). The fix is
//! `cratonvm_native_io::eintr::EintrIo` around the socket at every such site.
//!
//! This guard is deliberately narrow and it says so: it recognises the two
//! spellings the codebase actually uses, `*_tls(&mut stream.sock)` and
//! `*_tls(&mut tls.sock)`. A brand new handshake loop that names its socket
//! something else slips past it. What it does catch — and what actually
//! happened here — is a site being added by copying an existing loop, or an
//! existing wrapper being unwrapped. The expected counts are exact, with no
//! slack, so *removing* a site fails too rather than quietly shrinking
//! coverage.

/// `(file label, source, exact number of EINTR-wrapped socket-backed sites)`
const SOURCES: &[(&str, &str, usize)] = &[
    ("t27_tls.rs", include_str!("../src/t27_tls.rs"), 29),
    ("net_phase_e.rs", include_str!("../src/net_phase_e.rs"), 2),
    ("http_client.rs", include_str!("../src/http_client.rs"), 2),
    (
        "http_url_connection.rs",
        include_str!("../src/http_url_connection.rs"),
        3,
    ),
];

/// The socket receivers the handshake loops use.
const SOCKETS: &[&str] = &["stream.sock", "tls.sock"];

#[test]
fn no_socket_backed_tls_io_bypasses_the_eintr_wrapper() {
    for (label, source, _) in SOURCES {
        for socket in SOCKETS {
            for op in ["read_tls", "write_tls"] {
                let bare = format!("{op}(&mut {socket})");
                assert!(
                    !source.contains(&bare),
                    "{label}: `{bare}` touches the socket directly, so a signal \
                     delivered while it is parked surfaces as `IOException: \
                     Interrupted system call`. Wrap it: \
                     `{op}(&mut EintrIo::new(&mut {socket}))`."
                );
            }
        }
    }
}

#[test]
fn the_wrapped_site_count_is_exact() {
    for (label, source, expected) in SOURCES {
        let mut found = 0usize;
        for socket in SOCKETS {
            for op in ["read_tls", "write_tls"] {
                found += source
                    .matches(&format!("{op}(&mut EintrIo::new(&mut {socket}))"))
                    .count();
            }
        }
        assert_eq!(
            found, *expected,
            "{label}: expected exactly {expected} EINTR-wrapped socket-backed TLS \
             sites, found {found}. If you added or removed a handshake loop, update \
             this count deliberately — the point of the exact number is that neither \
             direction passes silently."
        );
    }
}

/// The three remaining `read_tls`/`write_tls` calls in the tree feed rustls from
/// an in-memory buffer (the `SSLEngine` lane: `wrap`/`unwrap` hand it a
/// `Cursor`, never a socket), so they cannot be interrupted and must NOT be
/// wrapped. Freeze that, so the two groups stay distinguishable and a future
/// reader does not "fix" them too.
#[test]
fn in_memory_tls_io_is_left_alone() {
    let source = include_str!("../src/t27_tls.rs");
    for marker in [
        "write_tls(&mut out)",
        "read_tls(&mut cursor)",
        "read_tls(&mut cur)",
    ] {
        assert!(
            source.contains(marker),
            "t27_tls.rs: `{marker}` disappeared. It is the SSLEngine lane's \
             in-memory feed; if it became socket-backed it needs `EintrIo`, and if \
             it was renamed this guard needs updating."
        );
    }
}

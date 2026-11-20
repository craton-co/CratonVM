use super::fixtures::*;
use super::*;
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess, NativeMethodRegistry};
use std::sync::Mutex;

/// `engine*` methods the delegating arm reached, most recent last.
static DELEGATED: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn record(
    _ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    DELEGATED.lock().unwrap().push(method.to_string());
    Some(Ok(None))
}

fn take_delegated() -> Vec<String> {
    std::mem::take(&mut *DELEGATED.lock().unwrap())
}

fn registries() -> Vec<(&'static str, NativeMethodRegistry)> {
    let mut out = Vec::new();
    for (name, f) in [
        (
            "net_phase_e::register_re6_ssl_context",
            crate::net_phase_e::register_re6_ssl_context as fn(&mut NativeMethodRegistry),
        ),
        (
            "phases_late::ssl_security::register_p68_ssl",
            crate::phases_late::ssl_security::register_p68_ssl,
        ),
        (
            "tls::register_tls_natives",
            crate::tls::register_tls_natives,
        ),
    ] {
        let mut r = NativeMethodRegistry::new();
        f(&mut r);
        out.push((name, r));
    }
    out
}

/// Argument vector for `descriptor`, receiver first, everything else null
/// or zero. Only the arity and the receiver matter to the guard.
fn call_args(this: ObjectRef, descriptor: &str) -> Vec<Value> {
    let mut args = vec![Value::Object(Some(this))];
    let params = descriptor
        .split_once(')')
        .map(|(head, _)| head.trim_start_matches('('))
        .unwrap_or("");
    let mut chars = params.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            'L' => {
                for c in chars.by_ref() {
                    if c == ';' {
                        break;
                    }
                }
                args.push(Value::Object(None));
            }
            // Element type follows on the next iteration and supplies the
            // one slot an array argument occupies.
            '[' => continue,
            'I' | 'Z' | 'B' | 'S' | 'C' => args.push(Value::Int(0)),
            'J' => args.push(Value::Long(0)),
            'F' => args.push(Value::Float(0.0)),
            'D' => args.push(Value::Double(0.0)),
            _ => {}
        }
    }
    args
}

/// ALL THREE registration sets, not just the one that wins today.
///
/// The `SSLContext` natives are registered three times over (`tls.rs`,
/// `net_phase_e.rs`, `phases_late/ssl_security.rs`) and boot order decides
/// which answers. A guard on only the current winner is one re-ordering
/// away from being dead, so this asserts the boundary holds in each of
/// them independently.
///
/// One `#[test]`, not nine: the recorder is a process-wide static (the
/// mock's hook is a plain `fn` pointer and cannot capture), so separate
/// tests would race under the default parallel harness.
#[test]
fn all_three_registration_sets_route_a_foreign_spi() {
    let _ = take_delegated();
    let mut checked = 0usize;
    for (set, r) in registries() {
        // --- MUST DELEGATE ---
        for (method, descriptor, engine_method) in DELEGATED_SURFACE {
            let f = match r.find("javax/net/ssl/SSLContext", method, descriptor) {
                Some(f) => f,
                // A set that does not register this method at all cannot
                // answer for a foreign context, so there is nothing to
                // guard. (`p68` and `tls.rs` each register a subset.)
                None => continue,
            };
            let mut ctx = MockNativeContext::new();
            ctx.set_invoke_virtual_hook(record);
            let (obj, _) = make_real_context(
                &mut ctx,
                "org/bouncycastle/jsse/provider/ProvSSLContextSpi",
                "TLSv1.3",
            );
            let args = call_args(obj, descriptor);
            let _ = f(&mut ctx, &args);
            assert_eq!(
                take_delegated(),
                vec![engine_method.to_string()],
                "{set}: SSLContext.{method}{descriptor} must reach \
                 SSLContextSpi.{engine_method} for a third-party SPI"
            );
            checked += 1;
        }

        // --- MUST NOT DELEGATE (the twin) ---
        //
        // Without this half the test would be satisfied by a guard that
        // delegates unconditionally — which is precisely the fix the page
        // rejected, because a real context around
        // `sun.security.ssl.SSLContextImpl` works today only BECAUSE the
        // natives answer for it.
        for (method, descriptor, _) in DELEGATED_SURFACE {
            let f = match r.find("javax/net/ssl/SSLContext", method, descriptor) {
                Some(f) => f,
                None => continue,
            };
            for spi in ["sun/security/ssl/SSLContextImpl", ""] {
                let mut ctx = MockNativeContext::new();
                ctx.set_invoke_virtual_hook(record);
                let obj = if spi.is_empty() {
                    make_synthetic_context(&mut ctx, "TLSv1.3")
                } else {
                    make_real_context(&mut ctx, spi, "TLSv1.3").0
                };
                let args = call_args(obj, descriptor);
                let _ = f(&mut ctx, &args);
                let kind = if spi.is_empty() { "a synthetic" } else { spi };
                assert!(
                    take_delegated().is_empty(),
                    "{set}: SSLContext.{method}{descriptor} must NOT delegate \
                     for {kind} context"
                );
            }
        }

        // --- getProtocol: the LAYOUT half, which is not a delegation ---
        for spi in [
            "org/bouncycastle/jsse/provider/ProvSSLContextSpi",
            "sun/security/ssl/SSLContextImpl",
        ] {
            let f = match r.find(
                "javax/net/ssl/SSLContext",
                "getProtocol",
                "()Ljava/lang/String;",
            ) {
                Some(f) => f,
                None => continue,
            };
            let mut ctx = MockNativeContext::new();
            let (obj, _) = make_real_context(&mut ctx, spi, "TLSv1.3");
            let got = match f(&mut ctx, &[Value::Object(Some(obj))]) {
                Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
                other => panic!("{set}: getProtocol answered {other:?}"),
            };
            assert_eq!(
                got, "TLSv1.3",
                "{set}: getProtocol on a real SSLContext read the wrong slot \
                 (slot 0 is `provider`, not `protocol`)"
            );
            checked += 1;
        }

        // --- getProvider: the same slot-0 confusion, the other way up ---
        //
        // `SSLContext.getProvider()` is `return provider;` — slot 0 on the
        // real layout, and the PROTOCOL on every synthetic one. Before
        // this was registered the real bytecode answered it and handed
        // back a `java.lang.String` where a `java.security.Provider` is
        // contracted.
        if let Some(f) = r.find(
            "javax/net/ssl/SSLContext",
            "getProvider",
            "()Ljava/security/Provider;",
        ) {
            // A REAL context answers its own `provider` field, BY IDENTITY
            // — `getInstance(algo, p).getProvider() == p`.
            let mut ctx = MockNativeContext::new();
            let (obj, _) =
                make_real_context(&mut ctx, "sun/security/ssl/SSLContextImpl", "TLSv1.3");
            let want = match ctx.get_field(obj, 0) {
                Value::Object(Some(o)) => o,
                other => panic!("{set}: fixture slot 0 was {other:?}"),
            };
            match f(&mut ctx, &[Value::Object(Some(obj))]) {
                Ok(Some(Value::Object(Some(got)))) => assert_eq!(
                    got, want,
                    "{set}: getProvider on a real SSLContext must answer its \
                     own `provider` field, by identity"
                ),
                other => panic!("{set}: getProvider answered {other:?}"),
            }

            // A SYNTHETIC context answers a Provider — specifically NOT
            // the protocol String sitting in its slot 0, which is what the
            // real bytecode returned.
            let mut ctx = MockNativeContext::new();
            let obj = make_synthetic_context(&mut ctx, "TLSv1.3");
            let slot0 = ctx.get_field(obj, 0);
            match f(&mut ctx, &[Value::Object(Some(obj))]) {
                Ok(Some(Value::Object(Some(got)))) => {
                    assert_ne!(
                        Value::Object(Some(got)),
                        slot0,
                        "{set}: getProvider on a synthetic SSLContext answered \
                         its slot 0 — that is the protocol, not a Provider"
                    );
                    let cls = ctx
                        .class_name_of_id(ctx.class_id_of_object(got))
                        .unwrap_or_default();
                    assert_eq!(
                        cls, "java/security/Provider",
                        "{set}: getProvider must answer a Provider"
                    );
                }
                other => panic!("{set}: getProvider answered {other:?}"),
            }
            checked += 1;
        }
    }
    // A `find` that stopped resolving would silently turn every loop above
    // into a no-op and the test would still pass.
    assert!(
        checked >= 12,
        "only {checked} guarded registrations were exercised — the \
         registration lookups have drifted"
    );
}

use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess as _, NativeHeapAccess as _};

const VM_ENGINE: usize = 0x16A0_0501;
const VM_NO_IDENTIFY: usize = 0x16A0_0502;
const VM_NO_CONTEXT: usize = 0x16A0_0503;

struct Teardown(usize);
impl Drop for Teardown {
    fn drop(&mut self) {
        crate::t27_tls::forget_vm_tls_state(self.0);
        crate::forget_vm_lock_keys(self.0);
    }
}

thread_local! {
    /// What the gate asked of the mocked Java objects, in order.
    static LOG: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn log(entry: String) {
    LOG.with(|l| l.borrow_mut().push(entry));
}

fn logged() -> Vec<String> {
    LOG.with(|l| l.borrow().clone())
}

fn string_arg(ctx: &MockNativeContext, args: &[Value], i: usize) -> String {
    match args.get(i) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    }
}

/// The `SSLContext`, its engine and parameters, and an engine-only trust
/// manager: accepts in `checkServerTrusted(chain, authType, SSLEngine)`
/// and throws `UnsupportedOperationException` from the two-argument form
/// (netty's `SniClientJava8TestUtil` idiom).
fn hook(
    ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    name: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    match name {
        "createSSLEngine" => {
            let host = string_arg(ctx, args, 0);
            let port = args.get(1).and_then(|v| v.as_int()).unwrap_or(-1);
            log(format!("engine {host}:{port}"));
            Some(ctx.new_object("sun/security/ssl/SSLEngineImpl"))
        }
        "setUseClientMode" | "setSSLParameters" => Some(Ok(None)),
        "getSSLParameters" => Some(ctx.new_object("javax/net/ssl/SSLParameters")),
        "setEndpointIdentificationAlgorithm" => {
            log(format!("algorithm {}", string_arg(ctx, args, 0)));
            Some(Ok(None))
        }
        "checkServerTrusted" => {
            if descriptor.contains("SSLEngine") {
                log("checkServerTrusted/3".to_string());
                Some(Ok(None))
            } else {
                log("checkServerTrusted/2".to_string());
                match ctx.new_object("java/lang/UnsupportedOperationException") {
                    Ok(Some(Value::Object(Some(e)))) => {
                        Some(Err(MethodCallFailed::ExceptionThrown(e)))
                    }
                    other => Some(other),
                }
            }
        }
        _ => None,
    }
}

/// An `SSLContext` initialised with the engine-only manager, and its
/// trust key.
fn context_with_engine_only_manager(ctx: &mut MockNativeContext) -> (ObjectRef, u64) {
    let extended = ctx
        .ensure_class_initialized("javax/net/ssl/X509ExtendedTrustManager")
        .unwrap();
    let tm_class = ctx
        .ensure_class_initialized("w16a/EngineOnlyTrustManager")
        .unwrap();
    ctx.set_superclass(tm_class, extended);
    let tm = match ctx.new_object("w16a/EngineOnlyTrustManager") {
        Ok(Some(Value::Object(Some(tm)))) => tm,
        other => panic!("expected the trust manager, got {other:?}"),
    };
    let tms = ctx.new_ref_array(ClassId::new(0), 1);
    ctx.set_array_element(tms, 0, Value::Object(Some(tm)));
    let sslctx = match ctx.new_object("javax/net/ssl/SSLContext") {
        Ok(Some(Value::Object(Some(c)))) => c,
        other => panic!("expected the context, got {other:?}"),
    };
    crate::t27_tls::attach_trust_managers_to_ctx(ctx, sslctx, Some(tms))
        .expect("the managers attach");
    let key = crate::t27_tls::ctx_trust_managers_key_if_attached(ctx, sslctx)
        .expect("key lookup")
        .expect("the context carries application managers");
    (sslctx, key)
}

fn chain() -> Vec<Vec<u8>> {
    vec![include_bytes!("../t27_certs/server.crt").to_vec()]
}

#[test]
fn an_extended_manager_gets_the_engine_overload_with_the_dialled_endpoint() {
    let _teardown = Teardown(VM_ENGINE);
    LOG.with(|l| l.borrow_mut().clear());
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM_ENGINE);
    ctx.set_invoke_virtual_hook(hook);
    let (sslctx, key) = context_with_engine_only_manager(&mut ctx);
    let mut failure = None;
    let result = re5_server_trust_gate(
        &mut ctx,
        Some(key),
        Some(sslctx),
        "w16a.example",
        8443,
        true,
        &chain(),
        &mut failure,
    );
    assert!(
        result.is_ok(),
        "an engine-only manager accepts through the engine overload, as on HotSpot: {result:?}"
    );
    assert!(failure.is_none());
    assert_eq!(
        logged(),
        vec![
            "engine w16a.example:8443".to_string(),
            "algorithm HTTPS".to_string(),
            "checkServerTrusted/3".to_string(),
        ],
        "the engine reports the dialled host and port and carries HTTPS, and \
         the two-argument form is never called"
    );
}

#[test]
fn disabled_hostname_verification_leaves_the_engine_without_an_algorithm() {
    let _teardown = Teardown(VM_NO_IDENTIFY);
    LOG.with(|l| l.borrow_mut().clear());
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM_NO_IDENTIFY);
    ctx.set_invoke_virtual_hook(hook);
    let (sslctx, key) = context_with_engine_only_manager(&mut ctx);
    let mut failure = None;
    let result = re5_server_trust_gate(
        &mut ctx,
        Some(key),
        Some(sslctx),
        "[::1]",
        443,
        false,
        &chain(),
        &mut failure,
    );
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        logged(),
        vec![
            "engine ::1:443".to_string(),
            "checkServerTrusted/3".to_string()
        ],
        "the IPv6 brackets are stripped and no algorithm is set"
    );
}

/// The contrast: with no `SSLContext` to mint an engine from, the same
/// manager gets the two-argument form and rejects, which is what every
/// `HttpClient` request over such a context did before.
#[test]
fn without_an_engine_the_same_manager_gets_the_two_argument_form() {
    let _teardown = Teardown(VM_NO_CONTEXT);
    LOG.with(|l| l.borrow_mut().clear());
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM_NO_CONTEXT);
    ctx.set_invoke_virtual_hook(hook);
    let (_sslctx, key) = context_with_engine_only_manager(&mut ctx);
    let mut failure = None;
    let error = re5_server_trust_gate(
        &mut ctx,
        Some(key),
        None,
        "w16a.example",
        8443,
        true,
        &chain(),
        &mut failure,
    )
    .expect_err("the two-argument form rejects");
    assert!(
        error
            .to_string()
            .starts_with("TLS handshake: TrustManager rejected the peer certificate chain"),
        "{error}"
    );
    assert!(failure.is_some());
    assert_eq!(logged(), vec!["checkServerTrusted/2".to_string()]);
}

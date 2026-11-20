#[allow(unused_imports)]
use super::*;

/// Append one `"name: value"` line to a builder's header array, growing it if
/// the fixed-size backing array is full.
///
/// The array was allocated at a fixed 32 entries and the old `header()` simply
/// stopped writing when it ran out — the 33rd header vanished with no error at
/// all. Grow instead: a silently dropped header is exactly the class of defect
/// this directory exists to remove.
pub fn re5_builder_append_header(ctx: &mut dyn NativeContext, this: ObjectRef, line: &str) {
    // `create_string` and `new_array` allocate, so the builder is read back
    // through a pin after each of them rather than carried as a bare local.
    let this_pin = ctx.pin_native_root(this);
    let line_obj = ctx.create_string(line);
    let line_pin = ctx.pin_native_root(line_obj);
    let this = ctx.read_native_pin(this_pin, this);
    let arr = match ctx.get_field(this, RE5_REQUEST_HEADERS) {
        Value::Object(Some(a)) => a,
        _ => {
            let a = ctx.new_array(ArrayElementType::Reference, 32);
            let this = ctx.read_native_pin(this_pin, this);
            ctx.set_field(this, RE5_REQUEST_HEADERS, Value::Object(Some(a)));
            a
        }
    };
    let line_obj = ctx.read_native_pin(line_pin, line_obj);
    let len = ctx.array_length(arr);
    for i in 0..len {
        // "Free" is anything that is NOT a live reference, not specifically
        // `Object(None)`. A reference array's cells are null on a real heap, so
        // the two agree there — but they do not agree everywhere, and a
        // free-slot scan that only recognises one spelling of empty walks off
        // the end of a full array and doubles it on every single append.
        if !matches!(ctx.get_array_element(arr, i), Value::Object(Some(_))) {
            ctx.set_array_element(arr, i, Value::Object(Some(line_obj)));
            ctx.unpin_native_roots(this_pin);
            return;
        }
    }
    // Full: copy into a doubled array and append there.
    let arr_pin = ctx.pin_native_root(arr);
    let grown = ctx.new_array(ArrayElementType::Reference, (len * 2).max(32));
    let arr = ctx.read_native_pin(arr_pin, arr);
    let line_obj = ctx.read_native_pin(line_pin, line_obj);
    for i in 0..len {
        let v = ctx.get_array_element(arr, i);
        ctx.set_array_element(grown, i, v);
    }
    ctx.set_array_element(grown, len, Value::Object(Some(line_obj)));
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, RE5_REQUEST_HEADERS, Value::Object(Some(grown)));
    ctx.unpin_native_roots(this_pin);
}

/// `setHeader(name, value)` — drop every existing line whose name matches
/// case-insensitively, then append. HotSpot, MEASURED:
/// `header("Accept","a").setHeader("Accept","z")` -> `{Accept=[z]}`.
pub fn re5_builder_set_header(ctx: &mut dyn NativeContext, this: ObjectRef, name: &str, value: &str) {
    if let Value::Object(Some(arr)) = ctx.get_field(this, RE5_REQUEST_HEADERS) {
        let len = ctx.array_length(arr);
        let mut kept: Vec<Value> = Vec::with_capacity(len);
        for i in 0..len {
            let cell = ctx.get_array_element(arr, i);
            let drop = match cell {
                Value::Object(Some(s)) => ctx
                    .read_string(s)
                    .and_then(|line| line.find(':').map(|c| line[..c].trim().to_string()))
                    .is_some_and(|k| k.eq_ignore_ascii_case(name)),
                _ => false,
            };
            if !drop {
                kept.push(cell);
            }
        }
        for (i, v) in kept.iter().enumerate() {
            ctx.set_array_element(arr, i, *v);
        }
        for i in kept.len()..len {
            ctx.set_array_element(arr, i, Value::Object(None));
        }
    }
    re5_builder_append_header(ctx, this, &format!("{name}: {value}"));
}

/// The name/value validation `HttpRequest.Builder.header` and `setHeader`
/// share. MEASURED on HotSpot 25.0.3+9-LTS:
///
/// ```text
/// header(null, "v") -> NullPointerException: name
/// header("k", null) -> NullPointerException: value
/// header("",   "v") -> IllegalArgumentException: invalid header name: ""
/// ```
pub fn re5_check_header_pair(
    ctx: &dyn NativeContext,
    args: &[Value],
) -> Result<(String, String), MethodCallFailed> {
    let name = match args.get(1).copied() {
        Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
        _ => return Err(npe("name")),
    };
    let value = match args.get(2).copied() {
        Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
        _ => return Err(npe("value")),
    };
    if name.is_empty() {
        return Err(iae("invalid header name: \"\""));
    }
    // `HttpRequestBuilderImpl.checkNameAndValue` refuses the five restricted
    // names (`Utils.DISALLOWED_HEADERS_SET`, case-insensitive) unless
    // `jdk.httpclient.allowRestrictedHeaders` lists them: `header("Host", ..)`
    // is `IllegalArgumentException: restricted header name: "Host"`. This
    // builder accepted every name (`NioConf19b` row `httpheaders` under
    // `--compatible`, JIT round 19).
    if re5_header_is_restricted(ctx, &name) {
        return Err(iae(format!("restricted header name: \"{name}\"")));
    }
    Ok((name, value))
}

/// Is `name` one of JDK 25's restricted request headers (`connection`,
/// `content-length`, `expect`, `host`, `upgrade`) and not re-allowed by the
/// `jdk.httpclient.allowRestrictedHeaders` system property?
pub fn re5_header_is_restricted(ctx: &dyn NativeContext, name: &str) -> bool {
    const DISALLOWED: [&str; 5] = ["connection", "content-length", "expect", "host", "upgrade"];
    let lower = name.to_ascii_lowercase();
    if !DISALLOWED.contains(&lower.as_str()) {
        return false;
    }
    let allowed = ctx
        .get_system_property("jdk.httpclient.allowRestrictedHeaders")
        .unwrap_or_default();
    !allowed
        .split(',')
        .any(|h| h.trim().eq_ignore_ascii_case(&lower))
}

/// The `name` argument the four `HttpHeaders` readers share.
/// MEASURED: `firstValue(null)` is a message-less NPE on HotSpot
/// (`Objects.requireNonNull(name)`), not an empty Optional.
pub fn re5_header_name_arg(
    ctx: &dyn NativeContext,
    args: &[Value],
) -> Result<String, MethodCallFailed> {
    match args.get(1).copied() {
        Some(Value::Object(Some(s))) => Ok(ctx.read_string(s).unwrap_or_default()),
        _ => Err(npe_no_message()),
    }
}

/// The `"key: value"` lines a request carries, grouped case-insensitively in
/// first-seen order. Shared by `HttpHeaders.map`/`firstValue`/`allValues`/
/// `firstValueAsLong`/`toString` so the five cannot come to disagree about
/// what a header name means.
pub fn re5_header_groups(ctx: &dyn NativeContext, headers: ObjectRef) -> Vec<(String, Vec<String>)> {
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    if let Value::Object(Some(arr)) = ctx.get_field(headers, 0) {
        let n = ctx.array_length(arr);
        for i in 0..n {
            if let Value::Object(Some(s)) = ctx.get_array_element(arr, i) {
                if let Some(line) = ctx.read_string(s) {
                    if let Some(c) = line.find(':') {
                        let k = line[..c].trim().to_string();
                        let v = line[c + 1..].trim().to_string();
                        if k.is_empty() {
                            continue;
                        }
                        if let Some(g) = groups
                            .iter_mut()
                            .find(|(gk, _)| gk.eq_ignore_ascii_case(&k))
                        {
                            g.1.push(v);
                        } else {
                            groups.push((k, vec![v]));
                        }
                    }
                }
            }
        }
    }
    groups
}

pub fn re5_request_timeout(
    ctx: &mut dyn NativeContext,
    request: ObjectRef,
) -> Result<Duration, MethodCallFailed> {
    let duration = match ctx.get_field(request, RE5_REQUEST_TIMEOUT_FIELD) {
        Value::Object(Some(duration)) => duration,
        _ => return Ok(Duration::from_secs(30)),
    };
    // Read the Duration through its public method instead of assuming the
    // real-JDK object's field layout.
    let millis = match ctx.invoke_virtual(duration, "toMillis", "()J", &[])? {
        Some(Value::Long(millis)) => millis,
        Some(Value::Int(millis)) => millis as i64,
        _ => 0,
    };
    // A positive sub-millisecond Duration has a toMillis() value of zero, so
    // use the smallest timeout the operating-system socket API can represent.
    Ok(Duration::from_millis(millis.max(1) as u64))
}

/// Read the real `SSLParameters` cipher list instead of assuming its private
/// field layout.  In real-JDK mode the object originates in Spring/JDK code,
/// so its public accessor is the stable contract.
pub fn re5_ssl_parameter_ciphers(
    ctx: &mut dyn NativeContext,
    parameters: ObjectRef,
) -> Result<Vec<String>, MethodCallFailed> {
    let array =
        match ctx.invoke_virtual(parameters, "getCipherSuites", "()[Ljava/lang/String;", &[])? {
            Some(Value::Object(Some(array))) => array,
            _ => return Ok(Vec::new()),
        };
    let mut ciphers = Vec::with_capacity(ctx.array_length(array));
    for index in 0..ctx.array_length(array) {
        if let Value::Object(Some(cipher)) = ctx.get_array_element(array, index) {
            if let Some(name) = ctx.read_string(cipher) {
                ciphers.push(name);
            }
        }
    }
    Ok(ciphers)
}

/// Shared request driver for `HttpClient.send` / `sendAsync`. `args[0]` is the
/// `HttpClient`, `args[1]` the `HttpRequest`, `args[2]` the `BodyHandler`.
/// Start `sendAsync`'s request on its own thread, returning the INCOMPLETE
/// future the caller gets back — or `None` when that could not be arranged, in
/// which case the caller falls back to performing the request inline.
///
/// Every step can fail on a VM that has not loaded these classes, and each
/// failure answers `None` rather than half-starting: a future handed out with
/// no thread to complete it would hang the first `get()`.
///
/// gc-common w14-a: `args` is [`Re5SendArgs`], pinned by the caller, because
/// every step here allocates and the caller's inline fallback needs the
/// arguments afterwards too. The client, request and handler used to be
/// copied into the task raw from `args` AFTER three allocations (the future,
/// the task, and possibly a class definition), so a moving collection there
/// handed the worker thread vacated addresses. And the future and task
/// global roots were never removed, so every `sendAsync` kept its future —
/// with the finished response and its body — and its task, i.e. the client,
/// request and handler, alive until the VM went away.
pub fn re5_start_async_send(
    ctx: &mut dyn NativeContext,
    args: &Re5SendArgs,
) -> Option<cratonvm_types::ObjectRef> {
    let future = match ctx.new_object_initialized(
        "jdk/internal/net/http/common/MinimalFuture",
        "()V",
        &[],
    ) {
        Ok(Some(Value::Object(Some(f)))) => f,
        _ => match ctx.new_object_initialized("java/util/concurrent/CompletableFuture", "()V", &[])
        {
            Ok(Some(Value::Object(Some(f)))) => f,
            _ => return None,
        },
    };
    let future_root = ctx.add_global_root(future);
    let started = re5_start_async_send_task(ctx, args, future, future_root);
    let future = ctx.resolve_global_root(future_root);
    ctx.remove_global_root(future_root);
    // Nothing between the resolve above and the caller's return allocates.
    started.and(future)
}

/// The client, request and handler of an `HttpClient.send`/`sendAsync` call,
/// pinned so they survive the allocations `sendAsync` makes before either
/// handing them to a worker or performing the request inline. See
/// [`re5_start_async_send`].
pub struct Re5SendArgs {
    pub values: [Value; 3],
    pub pins: [Option<usize>; 3],
}

impl Re5SendArgs {
    pub fn pin(ctx: &mut dyn NativeContext, args: &[Value]) -> Self {
        let values = [0, 1, 2].map(|i| args.get(i).copied().unwrap_or(Value::Object(None)));
        let pins = values.map(|v| match v {
            Value::Object(Some(o)) => Some(ctx.pin_native_root(o)),
            _ => None,
        });
        Self { values, pins }
    }

    /// The three values as they are NOW.
    pub fn current(&self, ctx: &dyn NativeContext) -> [Value; 3] {
        [0, 1, 2].map(|i| match (self.values[i], self.pins[i]) {
            (Value::Object(Some(o)), Some(pin)) => Value::Object(Some(ctx.read_native_pin(pin, o))),
            (v, _) => v,
        })
    }

    /// Release these pins and every pin taken after them.
    pub fn unpin(&self, ctx: &mut dyn NativeContext) {
        if let Some(first) = self.pins.iter().flatten().next() {
            ctx.unpin_native_roots(*first);
        }
    }
}

/// The part of [`re5_start_async_send`] that runs with the future held by
/// `future_root`; `Some(())` once the worker thread has started.
pub fn re5_start_async_send_task(
    ctx: &mut dyn NativeContext,
    args: &Re5SendArgs,
    future: cratonvm_types::ObjectRef,
    future_root: usize,
) -> Option<()> {
    // Under `--real-jdk` the fabricated carrier is refused, so the request used
    // to run INLINE on the caller's thread (the `None` below): `sendAsync` then
    // blocked until the response arrived, and a caller that starts its server
    // AFTER `sendAsync` never got there. A real class, defined from shipped
    // bytes, takes its place.
    let task = if ctx.is_jdk_only() {
        if ctx.class_id_by_name(RE5_SEND_TASK).is_none()
            && ctx
                .define_class_from_bytes(RE5_SEND_TASK, RE5_REAL_SEND_TASK_BYTES)
                .is_none()
        {
            return None;
        }
        match ctx.new_object_initialized(RE5_SEND_TASK, "()V", &[]) {
            Ok(Some(Value::Object(Some(t)))) => t,
            _ => return None,
        }
    } else {
        match try_alloc_concurrent_synthetic(ctx, RE5_SEND_TASK, RE5_TASK_FIELDS) {
            Ok(t) => t,
            Err(_) => return None,
        }
    };
    // No allocation from here to the last `set_field`: the future and the
    // three arguments are read through their roots right before the stores.
    let future = ctx.resolve_global_root(future_root).unwrap_or(future);
    let [client, request, handler] = args.current(ctx);
    ctx.set_field(task, RE5_TASK_FUTURE, Value::Object(Some(future)));
    ctx.set_field(task, RE5_TASK_CLIENT, client);
    ctx.set_field(task, RE5_TASK_REQUEST, request);
    ctx.set_field(task, RE5_TASK_HANDLER, handler);

    let task_root = ctx.add_global_root(task);
    let started = (|| {
        let thread = match ctx.new_object_initialized(
            "java/lang/Thread",
            "(Ljava/lang/Runnable;)V",
            &[Value::Object(Some(task))],
        ) {
            Ok(Some(Value::Object(Some(t)))) => t,
            _ => return None,
        };
        // `setDaemon` runs Java, so the thread is re-read for `thread_start`.
        let thread_pin = ctx.pin_native_root(thread);
        // Daemon, so an in-flight request cannot hold the VM open at exit — the
        // JDK's own HttpClient threads are daemons for the same reason.
        let _ = ctx.invoke_virtual(thread, "setDaemon", "(Z)V", &[Value::Int(1)]);
        let thread = ctx.read_native_pin(thread_pin, thread);
        let ok = ctx.thread_start(thread).is_ok();
        ctx.unpin_native_roots(thread_pin);
        ok.then_some(())
    })();
    // A started thread holds its task (and through it the future); an
    // unstarted one leaves both to the collector, as they should be.
    ctx.remove_global_root(task_root);
    started
}

/// The class of the task object `sendAsync` hands to a `Thread`.
pub const RE5_SEND_TASK: &str = "cratonvm/internal/HttpSendTask";
/// [`RE5_SEND_TASK`] as a REAL class, compiled from
/// `class_bytes/HttpSendTask.java` (`javac --release 17`): four `Object` fields
/// in the slot order above and a native `run()` ([`re5_send_task_run`]).
pub const RE5_REAL_SEND_TASK_BYTES: &[u8] = include_bytes!("../class_bytes/HttpSendTask.class");
pub const RE5_TASK_FUTURE: usize = 0;
pub const RE5_TASK_CLIENT: usize = 1;
pub const RE5_TASK_REQUEST: usize = 2;
pub const RE5_TASK_HANDLER: usize = 3;
pub const RE5_TASK_FIELDS: usize = 4;

/// `Runnable.run()` for the task above: perform the request that `sendAsync`
/// used to perform inline, then complete the future it already handed back.
///
/// Completion mirrors `native-io`'s async-channel path exactly, including the
/// `postComplete` call: `completeThrowable` and `complete` are the real JDK
/// primitives, and `postComplete` is bytecode, so it needs the bytecode-only
/// resolver or the by-name native lookup misses it and waiters never wake.
pub fn re5_send_task_run(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(None);
    };
    let future = match ctx.get_field(this, RE5_TASK_FUTURE) {
        Value::Object(Some(f)) => f,
        _ => return Ok(None),
    };
    let client = ctx.get_field(this, RE5_TASK_CLIENT);
    let request = ctx.get_field(this, RE5_TASK_REQUEST);
    let handler = ctx.get_field(this, RE5_TASK_HANDLER);

    let future_root = ctx.add_global_root(future);
    let outcome = re5_do_request(ctx, &[client, request, handler], Re5Caller::AsyncTask);
    let future = ctx.resolve_global_root(future_root).unwrap_or(future);

    match outcome {
        Ok(resp) => {
            let _ = ctx.invoke_virtual(
                future,
                "complete",
                "(Ljava/lang/Object;)Z",
                &[resp.unwrap_or(Value::Object(None))],
            );
        }
        Err(failure) => {
            let thrown = re5_throwable_for(ctx, failure);
            let future = ctx.resolve_global_root(future_root).unwrap_or(future);
            if let Some(t) = thrown {
                let _ = ctx.invoke_virtual(
                    future,
                    "completeThrowable",
                    "(Ljava/lang/Throwable;)Z",
                    &[Value::Object(Some(t))],
                );
            } else {
                // Nothing to complete it WITH is still better than a future no
                // one ever completes: a caller blocked in `get()` would hang.
                let _ = ctx.invoke_virtual(
                    future,
                    "complete",
                    "(Ljava/lang/Object;)Z",
                    &[Value::Object(None)],
                );
            }
        }
    }
    let future = ctx.resolve_global_root(future_root).unwrap_or(future);
    let _ = ctx.invoke_virtual_bytecode_only(future, "postComplete", "()V", &[]);
    // gc-common w14-a: this root was never removed, so every `sendAsync`
    // left its completed future (response, body and all) rooted for the life
    // of the VM. The caller holds the future it was handed; nothing else
    // needs this one.
    ctx.remove_global_root(future_root);
    Ok(None)
}

/// Turn a native failure into a Java `Throwable`, or `None` when it is a VM
/// fault rather than something a `catch` could name.
///
/// The common shape is `InternalError(VmError::Runtime(..))`, NOT an
/// already-materialised `ExceptionThrown` — reading that wrong is why the first
/// version of the `sendAsync` repair changed nothing observable.
pub fn re5_throwable_for(
    ctx: &mut dyn NativeContext,
    failure: cratonvm_types::error::MethodCallFailed,
) -> Option<cratonvm_types::ObjectRef> {
    use cratonvm_types::error::{MethodCallFailed, VmError};
    match failure {
        MethodCallFailed::ExceptionThrown(t) => Some(t),
        MethodCallFailed::InternalError(VmError::Runtime(re)) => {
            let (class_name, message) = re.as_java_throwable()?;
            let message = message.map(|m| m.into_owned());
            let built = match &message {
                Some(m) => {
                    let msg_obj = ctx.create_string(m);
                    ctx.new_object_initialized(
                        class_name,
                        "(Ljava/lang/String;)V",
                        &[Value::Object(Some(msg_obj))],
                    )
                }
                None => ctx.new_object_initialized(class_name, "()V", &[]),
            };
            match built {
                Ok(Some(Value::Object(Some(t)))) => Some(t),
                _ => None,
            }
        }
        MethodCallFailed::InternalError(_) => None,
    }
}

/// `CompletableFuture.failedFuture(t)` — the future `sendAsync` hands back when
/// the request failed.
///
/// MEASURED: `failedFuture` behaves identically to HotSpot on this VM
/// (`probes/CfFailedProbe.java`) — `isCompletedExceptionally()` is `true` and
/// `join()` raises `CompletionException` with the original cause — so the
/// failure reaches the caller through the same channel it does on HotSpot.
pub fn re5_failed_future(
    ctx: &mut dyn NativeContext,
    thrown: cratonvm_types::ObjectRef,
) -> MethodCallResult {
    let failed = ctx
        .invoke(
            "jdk/internal/net/http/common/MinimalFuture",
            "failedFuture",
            "(Ljava/lang/Throwable;)Ljava/util/concurrent/CompletableFuture;",
            &[Value::Object(Some(thrown))],
        )
        .or_else(|_| {
            ctx.invoke(
                "java/util/concurrent/CompletableFuture",
                "failedFuture",
                "(Ljava/lang/Throwable;)Ljava/util/concurrent/CompletableFuture;",
                &[Value::Object(Some(thrown))],
            )
        });
    match failed {
        // If `failedFuture` itself cannot be reached, handing back the original
        // throwable is better than inventing a successful future over no
        // response — the caller at least still sees the failure.
        Err(_) => Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
            thrown,
        )),
        ok => ok,
    }
}

/// The `HttpClient` server-trust gate (gc-common w14-a): consult the
/// application `TrustManager`s of the client's `SSLContext` on the chain a
/// finished handshake produced, before the request is written. Runs from
/// inside the exchange, between socket syscalls, with `ctx` published as the
/// active native context and the thread NOT GC-blocked — the same window
/// `JavaKeyManagerResolver` already runs Java in on this path.
///
/// `trust_key` is `None` when the context has no application managers. Its
/// config then verifies the chain itself (rustls against the context's
/// roots), so there is nothing to add here.
///
/// `endpoint_host` (gc-common w15-a,
/// `common-w14a-httpclient-custom-trustmanager-skips-endpoint-identification`)
/// is the host the hop dialled, or `None` when the client switched hostname
/// verification off (`jdk.internal.httpclient.disableHostnameVerification`,
/// see [`re5_hostname_verification_disabled`]). HotSpot's `HttpClient` sets
/// the `HTTPS` endpoint-identification algorithm on every connection's
/// `SSLParameters` unless that property is set, and JSSE then applies it the
/// way `t27_tls::jsse_owns_endpoint_identification` encodes: a plain
/// `X509TrustManager` is wrapped and JSSE checks the leaf against the host
/// after the manager accepts; JSSE's own `X509TrustManagerImpl` checks it
/// itself; an application `X509ExtendedTrustManager` owns the decision and
/// JSSE adds nothing. `run_client_trust_check_for_chain_with_engine` (since
/// gc-common w16-a; `_with_identity` before) runs the managers and then
/// applies exactly that rule.
///
/// On a rejection the Java exception is kept in `failure` for the caller to
/// rethrow, and the returned error aborts the exchange. Its `TLS handshake: `
/// prefix is what `re5_do_request` maps to `SSLHandshakeException` should the
/// kept exception ever be absent.
///
/// gc-common w16-a
/// (`common-w15a-httpclient-extended-trustmanager-gets-the-two-argument-check`):
/// `host` / `port` are the hop's dialled endpoint and `verify_hostname` says
/// whether identification applies (it replaces the old `endpoint_host`
/// option). When `ssl_context` is given (the client's `SSLContext`, current),
/// the gate mints the `SSLEngine` HotSpot's `HttpClient` would have
/// (`re5_trust_gate_engine`) and the managers are consulted with it, so an
/// application `X509ExtendedTrustManager` gets its THREE-argument
/// `checkServerTrusted(chain, authType, SSLEngine)`, with an engine that reports
/// the dialled host and port and carries the `HTTPS` algorithm unless
/// hostname verification is off. It used to get the two-argument form, which
/// an engine-only manager rejects. A plain `X509TrustManager` still gets the
/// two-argument form (`t27_tls::tm_is_extended` decides per manager). If the
/// engine cannot be minted the gate consults the managers as before.
#[allow(clippy::too_many_arguments)]
pub fn re5_server_trust_gate(
    ctx: &mut dyn NativeContext,
    trust_key: Option<u64>,
    ssl_context: Option<ObjectRef>,
    host: &str,
    port: u16,
    verify_hostname: bool,
    peer_chain: &[Vec<u8>],
    failure: &mut Option<MethodCallFailed>,
) -> std::io::Result<()> {
    let Some(key) = trust_key else {
        return Ok(());
    };
    if peer_chain.is_empty() {
        return Err(std::io::Error::other(
            "TLS handshake: no peer certificate available for TrustManager verification",
        ));
    }
    // An IPv6 literal reaches here as the URL spelled it; the identity check
    // compares an address, not its bracketed URL form.
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let endpoint_identity = verify_hostname.then(|| ("HTTPS".to_string(), host.to_string()));
    // The context's pin is the base of every pin the engine mint takes; one
    // release after the check drops them all.
    let (pin_base, engine) = match ssl_context {
        Some(ssl_context) => {
            let base = ctx.pin_native_root(ssl_context);
            let engine = re5_trust_gate_engine(ctx, base, ssl_context, host, port, verify_hostname);
            (Some(base), engine)        }
        None => (None, None),
    };
    let engine_now = engine.map(|(pin, e)| ctx.read_native_pin(pin, e));
    let checked = crate::t27_tls::run_client_trust_check_for_chain_with_engine(
        ctx,
        key,
        peer_chain.to_vec(),
        endpoint_identity,
        engine_now,
    );
    if let Some(base) = pin_base {
        ctx.unpin_native_roots(base);
    }
    match checked {
        Ok(()) => Ok(()),
        Err(e) => {
            let detail = crate::t27_tls::take_last_trust_rejection_detail()
                .unwrap_or_else(|| "no exception detail available".to_string());
            *failure = Some(e);
            // An identity failure is not the manager's rejection: its detail
            // already reads `endpoint identification (HTTPS) failed for host ..`.
            if detail.starts_with("endpoint identification") {
                return Err(std::io::Error::other(format!("TLS handshake: {detail}")));
            }
            Err(std::io::Error::other(format!(
                "TLS handshake: TrustManager rejected the peer certificate chain: {detail}"
            )))
        }
    }
}

/// The `SSLEngine` [`re5_server_trust_gate`] hands the context's managers
/// (gc-common w16-a): the one HotSpot's `HttpClient` builds per connection
/// (`sslContext.createSSLEngine(host, port)`, client mode, and the `HTTPS`
/// endpoint-identification algorithm on its `SSLParameters` unless
/// `jdk.internal.httpclient.disableHostnameVerification` is set). The engine
/// is this VM's own (or a third-party provider's, through the context's SPI),
/// so its `getPeerHost()` / `getPeerPort()` / `getSSLParameters()` answer
/// what a trust manager asks; it never handshakes (the exchange's rustls
/// connection did), and its rows go with it (`t27_tls::gc_sweep_tls_rows`).
///
/// `ctx_pin` pins `ssl_context0`. Returns the engine's pin and value, pinned
/// ABOVE `ctx_pin`, so the caller's release of `ctx_pin` drops both. `None`
/// when any step fails; the gate then consults the managers without an engine,
/// as it did before.
pub fn re5_trust_gate_engine(
    ctx: &mut dyn NativeContext,
    ctx_pin: usize,
    ssl_context0: ObjectRef,
    host: &str,
    port: u16,
    identify: bool,
) -> Option<(usize, ObjectRef)> {
    let host_str = ctx.create_string(host);
    let ssl_context = ctx.read_native_pin(ctx_pin, ssl_context0);
    let engine = match ctx.invoke_virtual(
        ssl_context,
        "createSSLEngine",
        "(Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;",
        &[Value::Object(Some(host_str)), Value::Int(i32::from(port))],
    ) {
        Ok(Some(Value::Object(Some(engine)))) => engine,
        _ => return None,
    };
    let engine_pin = ctx.pin_native_root(engine);
    let engine = ctx.read_native_pin(engine_pin, engine);
    if ctx
        .invoke_virtual(engine, "setUseClientMode", "(Z)V", &[Value::Int(1)])
        .is_err()
    {
        ctx.unpin_native_roots(engine_pin);
        return None;
    }
    if identify {
        let engine = ctx.read_native_pin(engine_pin, engine);
        let params = match ctx.invoke_virtual(
            engine,
            "getSSLParameters",
            "()Ljavax/net/ssl/SSLParameters;",
            &[],
        ) {
            Ok(Some(Value::Object(Some(params)))) => params,
            _ => {
                ctx.unpin_native_roots(engine_pin);
                return None;
            }
        };
        let params_pin = ctx.pin_native_root(params);
        let algorithm = ctx.create_string("HTTPS");
        let params = ctx.read_native_pin(params_pin, params);
        let set = ctx.invoke_virtual(
            params,
            "setEndpointIdentificationAlgorithm",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(algorithm))],
        );
        let applied = set.is_ok() && {
            let params = ctx.read_native_pin(params_pin, params);
            let engine = ctx.read_native_pin(engine_pin, engine);
            ctx.invoke_virtual(
                engine,
                "setSSLParameters",
                "(Ljavax/net/ssl/SSLParameters;)V",
                &[Value::Object(Some(params))],
            )
            .is_ok()
        };
        // `params_pin` sits above `engine_pin`: this keeps the engine pinned.
        ctx.unpin_native_roots(params_pin);
        if !applied {
            // An engine that does not carry the algorithm would tell an
            // extended manager that identification is off, which HotSpot's
            // would not: no engine at all is the closer answer.
            ctx.unpin_native_roots(engine_pin);
            return None;
        }
    }
    Some((engine_pin, ctx.read_native_pin(engine_pin, engine)))
}

/// `jdk.internal.net.http.common.Utils.isHostnameVerificationDisabled`: the
/// property `jdk.internal.httpclient.disableHostnameVerification` switches
/// the `HttpClient`'s endpoint identification off when it is present and
/// either empty or `Boolean.parseBoolean`-true (case-insensitive `true`).
/// Absent, or any other value, leaves it on.
///
/// HotSpot reads it once, into a `static final`, when `Utils` initialises;
/// this reads it per request. The two differ only for a program that changes
/// the property after its first `HttpClient` request, and then this one
/// follows the change.
pub fn re5_hostname_verification_disabled(ctx: &mut dyn NativeContext) -> bool {
    re5_hostname_verification_disabled_value(
        crate::system_property_read(ctx, "jdk.internal.httpclient.disableHostnameVerification")
            .as_deref(),
    )
}

/// The value rule of [`re5_hostname_verification_disabled`], without `ctx`.
pub fn re5_hostname_verification_disabled_value(prop: Option<&str>) -> bool {
    match prop {
        None => false,
        Some(v) => v.is_empty() || v.eq_ignore_ascii_case("true"),
    }
}

pub fn re5_do_request(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    caller: Re5Caller,
) -> MethodCallResult {
    let client0 = obj_arg(args, 0)?;
    let req0 = obj_arg(args, 1)?;
    let handler_val = args.get(2).copied();
    let handler_tag = re5_handler_tag(ctx, handler_val);
    // gc-common w14-a: the client, the request and a real BodyHandler are
    // pinned HERE, before the first call below that runs Java
    // (`Duration.toMillis`, the body publisher, `ProxySelector.select`,
    // `Redirect.name`, `SSLParameters.getCipherSuites`), and re-read through
    // their pins at each use. They used to be read raw from `args` — a copy
    // a moving collection does not rewrite — after those calls, and the
    // handler was pinned only after all of them, i.e. from a possibly
    // already-vacated address.
    let client_pin = ctx.pin_native_root(client0);
    let req_pin = ctx.pin_native_root(req0);
    let real_handler = match (handler_tag.as_deref(), handler_val) {
        (None, Some(Value::Object(Some(h)))) => Some((ctx.pin_native_root(h), h)),
        _ => None,
    };
    let method = read_field_string_or(ctx, req0, 0, "GET");
    let uri = read_field_string_or(ctx, req0, 1, "");
    let request_timeout = re5_request_timeout(ctx, req0)?;
    if re5_dbg() {
        eprintln!("[RE5-DBG] re5_do_request ENTER method={method} uri={uri}");
    }
    // Asked BEFORE the body is collected (collection subscribes to the
    // publisher): an unknown length is framed chunked, as the JDK's
    // `Http1Request` does (nio20 wave 3, L1; `NioConf19b` `httpstream`).
    let req = ctx.read_native_pin(req_pin, req0);
    let chunked = re5_body_length_unknown(ctx, req)?;
    let req = ctx.read_native_pin(req_pin, req0);
    let body_val = ctx.get_field(req, 2);
    let body = re5_request_body_bytes(ctx, body_val, caller)?;
    let req = ctx.read_native_pin(req_pin, req0);
    let hdrs_val = ctx.get_field(req, 3);
    let mut headers = huc_extract_req_headers(ctx, hdrs_val);
    let body = if chunked
        && !headers.iter().any(|(k, _)| {
            k.eq_ignore_ascii_case("transfer-encoding") || k.eq_ignore_ascii_case("content-length")
        }) {
        headers.push(("Transfer-Encoding".to_string(), "chunked".to_string()));
        http_chunk_encode(&body)
    } else {
        body
    };
    if uri.is_empty() {
        return Err(ioex("HttpRequest.uri is empty"));
    }
    // Spring's filtered ProxySelector is the policy boundary for JDK-client
    // requests. Run the real selector before opening a socket so its exception
    // (not a later, unrelated connection error) reaches the caller.
    let client = ctx.read_native_pin(client_pin, client0);
    if let Value::Object(Some(proxy)) = ctx.get_field(client, RE5_CLIENT_PROXY) {
        let proxy_pin = ctx.pin_native_root(proxy);
        let text = ctx.create_string(&uri);
        let uri_obj = match ctx.invoke(
            "java/net/URI",
            "create",
            "(Ljava/lang/String;)Ljava/net/URI;",
            &[Value::Object(Some(text))],
        )? {
            Some(Value::Object(Some(uri_obj))) => uri_obj,
            _ => return Err(ioex("URI.create returned null for HttpClient request")),
        };
        let proxy = ctx.read_native_pin(proxy_pin, proxy);
        ctx.invoke_virtual(
            proxy,
            "select",
            "(Ljava/net/URI;)Ljava/util/List;",
            &[Value::Object(Some(uri_obj))],
        )?;
        ctx.unpin_native_roots(proxy_pin);
    }
    let client = ctx.read_native_pin(client_pin, client0);
    let redirect = ctx.get_field(client, RE5_CLIENT_REDIRECT);
    let max_redirects = match re5_enum_name(ctx, redirect) {
        Some(name) if name == "NEVER" => 0,
        // JDK's default is NEVER. Spring explicitly configures NORMAL for its
        // request factory, and BOTH NORMAL/ALWAYS are allowed to follow the
        // local HTTP redirects used by these integration tests.
        Some(_) => 10,
        None => 0,
    };
    let client = ctx.read_native_pin(client_pin, client0);
    let has_ssl_context = matches!(
        ctx.get_field(client, RE5_CLIENT_SSL_CONTEXT),
        Value::Object(Some(_))
    );
    // `(config, TrustManager key)`. The key is `Some` only when the context
    // carries application `TrustManager`s, which is exactly when its config
    // installs the pass-through verifier and they must be consulted below.
    let tls = if has_ssl_context {
        // `getCipherSuites` runs Java, so the context is read after it.
        let ciphers = match ctx.get_field(client, RE5_CLIENT_SSL_PARAMETERS) {
            Value::Object(Some(parameters)) => re5_ssl_parameter_ciphers(ctx, parameters)?,
            _ => Vec::new(),
        };
        let client = ctx.read_native_pin(client_pin, client0);
        match ctx.get_field(client, RE5_CLIENT_SSL_CONTEXT) {
            Value::Object(Some(ssl_context)) => {
                // Neither call allocates (`ctx_obj_key` is an identity-hash
                // registry lookup), so `ssl_context` is current for both.
                let trust_key =
                    crate::t27_tls::ctx_trust_managers_key_if_attached(ctx, ssl_context)?;
                let config = crate::t27_tls::client_config_for_ssl_context_with_ciphers(
                    ctx,
                    ssl_context,
                    &ciphers,
                )
                .map_err(|e| ioex(format!("HttpClient SSLContext configuration failed: {e}")))?;
                Some((config, trust_key))
            }
            _ => None,
        }
    } else {
        None
    };
    if re5_dbg() {
        eprintln!(
            "[RE5-DBG] re5_do_request method={method} uri={uri} body_len={} calling http_perform_request...",
            body.len()
        );
    }
    // STW cross-thread JIT-takeover deadlock fix (found investigating
    // reactive ClientHttpConnectorTests intermittent hangs, 2026-07-15): the
    // raw TcpStream connect/write/read cycle inside `http_perform_request`
    // blocks this thread in a genuine OS syscall for up to 30s (its own
    // socket-level read timeout) without cooperating with a concurrent
    // Stop-The-World pause -- unlike every OTHER blocking native I/O call in
    // this file (see `re1_socket_read_stream`/`re1_socket_write_stream`
    // above), which correctly brackets the syscall with
    // `begin_blocking_region`/`end_blocking_region` so the GC barrier
    // excludes this thread from `expected` while it cannot reach a
    // safepoint. Without that, a concurrent STW request (e.g. a JIT
    // recompile or GC pause triggered by unrelated activity in the SAME
    // process) waits up to its full round budget for this thread to
    // cooperate -- while the STW pause is simultaneously what freezes the
    // real Java thread on the OTHER end of the socket (MockWebServer's own
    // response-writing dispatcher, ordinary bytecode running in this same
    // JVM process) that this thread is blocked waiting to hear from. Live
    // capture: `CRATONVM_DBG_RE5=1` showed a `DELETE` request's `write_all`
    // succeed in under 200us, then `read_response` block for the full 30s
    // socket timeout and fail with `WouldBlock`, with a
    // "STW cross-thread JIT takeover is still waiting for cooperative
    // mutators rounds=64 pending=1 taken=0" warning firing mid-block --
    // confirmed HotSpot-only-divergent (8/8 clean runs of the identical
    // 32-request sequential-MockWebServer-cycle probe on HotSpot; CratonVM
    // hit it on ~2/13 attempts, always on this JDK-connector code path,
    // never on the Reactor-Netty/Jetty/HttpComponents connectors that don't
    // route through this raw-socket implementation).
    // The `SSLHandshakeException` a rejecting `TrustManager` produced (its
    // message names the manager's exception class and message), kept so the
    // caller gets THAT object rather than one re-minted from a string.
    let mut trust_failure: Option<MethodCallFailed> = None;
    // gc-common w15-a: read only when the gate will run (a context with
    // application `TrustManager`s). It may run Java (`System.getProperties`'
    // real map), and nothing below reads an `ObjectRef` that was not re-read
    // through its pin.
    let hostname_verification_disabled =
        matches!(tls, Some((_, Some(_)))) && re5_hostname_verification_disabled(ctx);
    let perform_result = match tls {
        Some((config, trust_key)) => {
            // A context-scoped config may resolve Java KeyManager/TrustManager
            // callbacks during the rustls handshake. Keep this native context
            // active and do not mark the thread as GC-blocked while that happens.
            let _active_context = crate::t27_tls::set_active_native_context(ctx);
            // gc-common w14-a (common-w13a-httpclient-custom-trustmanager-is-
            // never-consulted): the context's `TrustManager`s decide, as
            // JSSE's do. Before this gate the config's pass-through verifier
            // accepted every server certificate and nothing asked the
            // managers, so a pinning or rejecting manager was ignored.
            //
            // gc-common w15-a: and with the dialled host, so a plain
            // `X509TrustManager` gets JSSE's endpoint identification on top.
            //
            // gc-common w16-a: and with the client's `SSLContext`, from which
            // the gate mints the `SSLEngine` an `X509ExtendedTrustManager` is
            // handed. Read through the client's pin at each hop, so it is
            // current; nothing allocates between here and the gate's own pin.
            let mut server_trust =
                |host: &str, port: u16, peer_chain: &[Vec<u8>]| -> std::io::Result<()> {
                    let ssl_context = match trust_key {
                        Some(_) => {
                            let client = ctx.read_native_pin(client_pin, client0);
                            match ctx.get_field(client, RE5_CLIENT_SSL_CONTEXT) {
                                Value::Object(Some(ssl_context)) => Some(ssl_context),
                                _ => None,
                            }
                        }
                        None => None,
                    };
                    re5_server_trust_gate(
                        ctx,
                        trust_key,
                        ssl_context,
                        host,
                        port,
                        !hostname_verification_disabled,
                        peer_chain,
                        &mut trust_failure,
                    )
                };
            http_perform_request_with_timeout(
                &method,
                &uri,
                &headers,
                &body,
                request_timeout,
                max_redirects,
                Some(config),
                &mut server_trust,
            )
        }
        None => {
            // Wave 83 (lane L6): the state of the caller's HotSpot thread
            // (`Re5Caller`), as `http_client.rs` `do_send`.
            re5_begin_exchange_region(ctx, caller);
            let result = http_perform_request_with_timeout(
                &method,
                &uri,
                &headers,
                &body,
                request_timeout,
                max_redirects,
                None,
                &mut http_no_server_trust_gate,
            );
            ctx.end_blocking_region();
            result
        }
    };
    if let Some(failure) = trust_failure {
        if re5_dbg() {
            eprintln!("[RE5-DBG] re5_do_request method={method} uri={uri} TrustManager rejected the server");
        }
        return Err(failure);
    }
    let resp = perform_result.map_err(|e| {
        let message = e.to_string();
        if re5_dbg() {
            eprintln!("[RE5-DBG] re5_do_request method={method} uri={uri} http_perform_request FAILED: {message}");
        }
        if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) {
            return ioex("HttpClient request timed out");
        }
        // JSSE surfaces a rejected TLS negotiation as SSLHandshakeException.
        // Preserve that Java contract rather than wrapping the transport's
        // platform-specific certificate error in a generic IOException.
        if let Some(detail) = message.strip_prefix("TLS handshake: ") {
            return crate::phases_early::throw_jca_exc(
                ctx,
                "javax/net/ssl/SSLHandshakeException",
                detail,
            );
        }
        ioex(format!("HttpClient request failed: {message}"))
    })?;
    if re5_dbg() {
        eprintln!(
            "[RE5-DBG] re5_do_request method={method} uri={uri} http_perform_request OK status={}",
            resp.status
        );
    }
    let out = match real_handler {
        None => {
            let tag = handler_tag.unwrap_or_else(|| "inputstream".to_string());
            let out = re5_build_response(ctx, resp.status, &resp.headers, &resp.body, &tag)?;
            // Fills in uri()/request()/previousResponse()/sslSession() — see
            // docs/feature-designs/native-dispatch-architecture.md.
            re5_finish_response(ctx, out, &resp)
        }
        Some((handler_pin, handler)) => {
            // Drive the user's BodyHandler protocol against the wire bytes;
            // body() then returns whatever value the handler produced.
            let handler_now = ctx.read_native_pin(handler_pin, handler);
            let body_obj =
                re5_drive_body_handler(ctx, handler_now, resp.status, &resp.headers, &resp.body)?;
            let body_obj_pin = match body_obj {
                Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
                _ => None,
            };
            let out =
                re5_build_response(ctx, resp.status, &resp.headers, &resp.body, RE5_TAG_HANDLED)?;
            let body_obj_now = match body_obj_pin {
                Some((pin, o)) => Value::Object(Some(ctx.read_native_pin(pin, o))),
                None => Value::Object(None),
            };
            ctx.set_field(out, RE5_RESP_BODY_OBJ, body_obj_now);
            ctx.unpin_native_roots(handler_pin);
            re5_finish_response(ctx, out, &resp)
        }
    };
    // Releases the client and request pins (and anything pinned after them).
    ctx.unpin_native_roots(client_pin);
    Ok(Some(Value::Object(Some(out?))))
}

/// Extract the full external-form string from a `java.net.URI` and return it as
/// a heap String. `java.net.URI`'s slot 0 is the `scheme` ("http"), not the
/// whole URL, so reading the field directly is wrong; `uri_raw_string` is the
/// module's slot-order-safe reconstruction (reads the `string` cache field by
/// name, the same path every URI getter uses) and works for both a real JDK
/// `URI` and a `make_uri`-built one. Falls back to `toString()` only if that
/// yields nothing.
pub fn re5_uri_string(ctx: &mut dyn NativeContext, uri: ObjectRef) -> ObjectRef {
    // 1. Cached external form (the `string` field, slot-order-safe).
    let raw = uri_raw_string(ctx, uri);
    if !raw.is_empty() {
        return ctx.create_string(&raw);
    }
    // 2. Reconstruct from the URI's own getters (proven natives that read named
    //    components), so we don't depend on the `string` cache being populated.
    //
    // gc-common w20-d: each getter is Java; the URI was asked the next one
    // through its address from before the previous. Pinned (released when the
    // native returns), read back before every call.
    let uri_pin = ctx.pin_native_root(uri);
    let getter = |ctx: &mut dyn NativeContext, m: &str, d: &str| -> Option<String> {
        let uri = ctx.read_native_pin(uri_pin, uri);
        match ctx.invoke_virtual(uri, m, d, &[]) {
            Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).filter(|v| !v.is_empty()),
            _ => None,
        }
    };
    if let Some(scheme) = getter(ctx, "getScheme", "()Ljava/lang/String;") {
        let mut url = format!("{scheme}://");
        if let Some(host) = getter(ctx, "getHost", "()Ljava/lang/String;") {
            url.push_str(&host);
            let uri = ctx.read_native_pin(uri_pin, uri);
            if let Ok(Some(Value::Int(port))) = ctx.invoke_virtual(uri, "getPort", "()I", &[]) {
                if port > 0 {
                    url.push_str(&format!(":{port}"));
                }
            }
        }
        if let Some(path) = getter(ctx, "getRawPath", "()Ljava/lang/String;") {
            url.push_str(&path);
        }
        if let Some(query) = getter(ctx, "getRawQuery", "()Ljava/lang/String;") {
            url.push('?');
            url.push_str(&query);
        }
        if url.contains("://") && url.len() > scheme.len() + 3 {
            return ctx.create_string(&url);
        }
    }
    // 3. Last resort: toString().
    let uri = ctx.read_native_pin(uri_pin, uri);
    if let Ok(Some(Value::Object(Some(s)))) =
        ctx.invoke_virtual(uri, "toString", "()Ljava/lang/String;", &[])
    {
        if let Some(text) = ctx.read_string(s) {
            if text.contains("://") || text.starts_with('/') {
                return s;
            }
        }
    }
    ctx.create_string("")
}

pub fn register_re5_http_client(r: &mut NativeMethodRegistry) {
    r.register(
        RE5_BODY_COLLECTOR_SUBSCRIBER,
        "onSubscribe",
        "(Ljava/util/concurrent/Flow$Subscription;)V",
        re5_body_collector_on_subscribe, NativeKind::SyntheticStub,
    );
    r.register(
        RE5_BODY_COLLECTOR_SUBSCRIBER,
        "onNext",
        "(Ljava/lang/Object;)V",
        re5_body_collector_on_next, NativeKind::SyntheticStub,
    );
    r.register(
        RE5_BODY_COLLECTOR_SUBSCRIBER,
        "onError",
        "(Ljava/lang/Throwable;)V",
        re5_body_collector_on_error, NativeKind::SyntheticStub,
    );
    r.register(
        RE5_BODY_COLLECTOR_SUBSCRIBER,
        "onComplete",
        "()V",
        re5_body_collector_on_complete, NativeKind::SyntheticStub,
    );

    let hc = "java/net/http/HttpClient";
    r.register(
        hc,
        "newHttpClient",
        "()Ljava/net/http/HttpClient;",
        |ctx, _args| {
            let obj = re5_alloc_client(ctx)?;
            Ok(Some(Value::Object(Some(obj))))
        }, NativeKind::SyntheticStub,
    );
    r.register(
        hc,
        "newBuilder",
        "()Ljava/net/http/HttpClient$Builder;",
        |ctx, _args| {
            let obj = re5_alloc_client_builder(ctx)?;
            Ok(Some(Value::Object(Some(obj))))
        }, NativeKind::SyntheticStub,
    );

    let bld = "java/net/http/HttpClient$Builder";
    r.register(bld, "build", "()Ljava/net/http/HttpClient;", |ctx, args| {
        let builder = obj_arg(args, 0)?;
        // gc-common w21-b: the builder is rooted across the client's
        // allocation and its fields are read through its current address.
        // They were read -- and their references copied into the client --
        // through the builder's entry address.
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let builder_h = scope.root(builder);
        let obj = re5_alloc_client(&mut *scope)?;
        let builder = scope.get(&builder_h);
        let ctx = &mut *scope;
        for (from, to) in [
            (RE5_BUILDER_VERSION, RE5_CLIENT_VERSION),
            (RE5_BUILDER_REDIRECT, RE5_CLIENT_REDIRECT),
            (RE5_BUILDER_CONNECT_TIMEOUT, RE5_CLIENT_CONNECT_TIMEOUT),
            (RE5_BUILDER_SSL_CONTEXT, RE5_CLIENT_SSL_CONTEXT),
            (RE5_BUILDER_EXECUTOR, RE5_CLIENT_EXECUTOR),
            (RE5_BUILDER_PROXY, RE5_CLIENT_PROXY),
            (RE5_BUILDER_AUTHENTICATOR, RE5_CLIENT_AUTHENTICATOR),
            (RE5_BUILDER_COOKIE_HANDLER, RE5_CLIENT_COOKIE_HANDLER),
            (RE5_BUILDER_SSL_PARAMETERS, RE5_CLIENT_SSL_PARAMETERS),
        ] {
            ctx.set_field(obj, to, ctx.get_field(builder, from));
        }
        Ok(Some(Value::Object(Some(obj))))
    }, NativeKind::SyntheticStub);
    r.register(
        bld,
        "connectTimeout",
        "(Ljava/time/Duration;)Ljava/net/http/HttpClient$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(
                this,
                RE5_BUILDER_CONNECT_TIMEOUT,
                args.get(1).copied().unwrap_or(Value::Object(None)),
            );
            Ok(Some(Value::Object(Some(this))))
        }, NativeKind::SyntheticStub,
    );
    r.register(
        bld,
        "followRedirects",
        "(Ljava/net/http/HttpClient$Redirect;)Ljava/net/http/HttpClient$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(
                this,
                RE5_BUILDER_REDIRECT,
                args.get(1).copied().unwrap_or(Value::Object(None)),
            );
            Ok(Some(Value::Object(Some(this))))
        }, NativeKind::SyntheticStub,
    );

    // These are abstract interface methods. In the real-JDK build `newBuilder`
    // creates this synthetic directly, so every fluent method must retain its
    // argument here rather than relying on the synthetic-only HTTP/2 registrar.
    macro_rules! register_builder_value {
        ($method:literal, $descriptor:literal, $field:expr) => {
            r.register(bld, $method, $descriptor, |ctx, args| {
                let this = obj_arg(args, 0)?;
                ctx.set_field(
                    this,
                    $field,
                    args.get(1).copied().unwrap_or(Value::Object(None)),
                );
                Ok(Some(Value::Object(Some(this))))
            }, NativeKind::SyntheticStub);
        };
    }
    register_builder_value!(
        "version",
        "(Ljava/net/http/HttpClient$Version;)Ljava/net/http/HttpClient$Builder;",
        RE5_BUILDER_VERSION
    );
    r.register(
        bld,
        "priority",
        "(I)Ljava/net/http/HttpClient$Builder;",
        |_ctx, args| Ok(Some(args[0])), NativeKind::SyntheticStub,
    );
    register_builder_value!(
        "executor",
        "(Ljava/util/concurrent/Executor;)Ljava/net/http/HttpClient$Builder;",
        RE5_BUILDER_EXECUTOR
    );
    register_builder_value!(
        "cookieHandler",
        "(Ljava/net/CookieHandler;)Ljava/net/http/HttpClient$Builder;",
        RE5_BUILDER_COOKIE_HANDLER
    );
    register_builder_value!(
        "proxy",
        "(Ljava/net/ProxySelector;)Ljava/net/http/HttpClient$Builder;",
        RE5_BUILDER_PROXY
    );
    register_builder_value!(
        "authenticator",
        "(Ljava/net/Authenticator;)Ljava/net/http/HttpClient$Builder;",
        RE5_BUILDER_AUTHENTICATOR
    );
    register_builder_value!(
        "sslContext",
        "(Ljavax/net/ssl/SSLContext;)Ljava/net/http/HttpClient$Builder;",
        RE5_BUILDER_SSL_CONTEXT
    );
    register_builder_value!(
        "sslParameters",
        "(Ljavax/net/ssl/SSLParameters;)Ljava/net/http/HttpClient$Builder;",
        RE5_BUILDER_SSL_PARAMETERS
    );

    r.register(
        hc,
        "send",
        "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/net/http/HttpResponse;",
        |ctx, args| re5_do_request(ctx, args, Re5Caller::Send), NativeKind::SyntheticStub,
    );

    // sendAsync(HttpRequest, BodyHandler) -> CompletableFuture<HttpResponse>.
    // Spring's `JdkClientHttpRequest` (RestClient / JdkClientHttpRequestFactory)
    // drives requests exclusively through sendAsync(...).get(). We perform the
    // request synchronously and hand back an already-completed real
    // CompletableFuture so the caller's `.get()` returns immediately.
    let send_async: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult = |ctx, args| {
        // A FAILURE COMPLETES THE FUTURE; it does not come out of the call.
        //
        // `CompletableFuture<HttpResponse<T>> sendAsync(...)` declares no
        // checked exception, so an `IOException` escaping it is a throwable no
        // Java implementation of this method could ever produce, and no caller
        // can catch without `catch (Throwable)`. `re5_do_request(...)?` did
        // exactly that: this VM performs the request synchronously, and the
        // `?` handed the synchronous failure to a caller expecting a future.
        //
        // MEASURED (`probes/ResidualProbe.java`), request to a refused port:
        //   HotSpot 25   sendAsync returns jdk.internal.net.http.common.MinimalFuture
        //   CratonVM     sendAsync threw java.io.IOException
        //
        // The failure now lands where the contract puts it — in the returned
        // future — so `.get()`/`.join()` raise `ExecutionException`/
        // `CompletionException` as they do on HotSpot, and a caller that only
        // holds the future is no longer skipped past.
        //
        // NOT fixed here, and named rather than hidden: the request is still
        // performed SYNCHRONOUSLY, so the future is already complete when it is
        // handed back (`isDone()` is `true` where HotSpot's is `false`) and its
        // class is `CompletableFuture` where HotSpot's is `MinimalFuture`. A
        // caller that chains on it sees the continuation run on the calling
        // thread. Making the send genuinely asynchronous is a different change.
        // ASYNCHRONOUS when a thread can be started for it, synchronous when
        // one cannot. The request runs on its own thread and completes the
        // future the caller already holds, so N calls overlap instead of
        // serialising and a continuation runs off the caller's thread — which
        // is what `sendAsync` means. If the thread cannot be created the old
        // synchronous path below runs unchanged, so the failure mode is the
        // previous behaviour rather than a future nobody completes.
        // gc-common w14-a: pinned before `re5_start_async_send` allocates, so
        // the inline fallback below does not run on stale `args` copies.
        let send_args = Re5SendArgs::pin(ctx, args);
        if let Some(future) = re5_start_async_send(ctx, &send_args) {
            send_args.unpin(ctx);
            return Ok(Some(Value::Object(Some(future))));
        }
        let args_now = send_args.current(ctx);
        let outcome = re5_do_request(ctx, &args_now, Re5Caller::SendAsyncInline);
        send_args.unpin(ctx);
        match outcome {
            Ok(resp) => {
                let r_val = resp.unwrap_or(Value::Object(None));
                ctx.invoke(
                    "jdk/internal/net/http/common/MinimalFuture",
                    "completedFuture",
                    "(Ljava/lang/Object;)Ljdk/internal/net/http/common/MinimalFuture;",
                    &[r_val],
                )
                .or_else(|_| {
                    ctx.invoke(
                        "java/util/concurrent/CompletableFuture",
                        "completedFuture",
                        "(Ljava/lang/Object;)Ljava/util/concurrent/CompletableFuture;",
                        &[r_val],
                    )
                })
            }
            Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(thrown)) => {
                re5_failed_future(ctx, thrown)
            }
            // The common shape here is NOT an already-materialised throwable:
            // `re5_do_request` reports its failures as `RuntimeError`s, which
            // reach this point as `InternalError(VmError::Runtime(..))` and are
            // turned into Java objects later, at the interpreter's own throw
            // site. Completing a future needs the object NOW, so build it from
            // the same table that site uses — `RuntimeError::as_java_throwable`
            // — exactly as `lang_class.rs` does when it has to wrap a callee's
            // failure in an `InvocationTargetException`. Reading the variant
            // wrong is why the first version of this fix changed nothing: the
            // `ExceptionThrown` arm above never matched.
            Err(cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(re),
            )) => {
                let Some((class_name, message)) = re.as_java_throwable() else {
                    // Not a Java exception at all (a VM gap). Keep propagating.
                    return Err(cratonvm_types::error::MethodCallFailed::InternalError(
                        cratonvm_types::error::VmError::Runtime(re),
                    ));
                };
                // Own the message so `re` is free again for the failure path.
                let message = message.map(|m| m.into_owned());
                let built = match &message {
                    Some(m) => {
                        let msg_obj = ctx.create_string(m);
                        ctx.new_object_initialized(
                            class_name,
                            "(Ljava/lang/String;)V",
                            &[Value::Object(Some(msg_obj))],
                        )
                    }
                    // `None` means "no detail message": the no-arg ctor, so
                    // `getMessage()` is null rather than "".
                    None => ctx.new_object_initialized(class_name, "()V", &[]),
                };
                match built {
                    Ok(Some(Value::Object(Some(thrown)))) => re5_failed_future(ctx, thrown),
                    // Could not build the throwable — propagate the original
                    // rather than invent a successful future over no response.
                    _ => Err(cratonvm_types::error::MethodCallFailed::InternalError(
                        cratonvm_types::error::VmError::Runtime(re),
                    )),
                }
            }
            // Not a Java throwable, so it cannot complete a future. It is a VM
            // fault and keeps propagating as one.
            Err(internal) => Err(internal),
        }
    };
    // The task carrier's `run()`. Registered on its own class so dispatch —
    // which keys on the receiver's runtime class — finds it when `Thread.run()`
    // calls `target.run()`.
    r.register(RE5_SEND_TASK, "run", "()V", re5_send_task_run, NativeKind::SyntheticStub);
    r.register(
        hc,
        "sendAsync",
        "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/util/concurrent/CompletableFuture;",
        send_async, NativeKind::SyntheticStub,
    );
    r.register(
        hc,
        "sendAsync",
        "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;Ljava/net/http/HttpResponse$PushPromiseHandler;)Ljava/util/concurrent/CompletableFuture;",
        send_async, NativeKind::SyntheticStub,
    );

    // --- HttpClient instance accessors (RE.5 audit) ---------------------------
    // `java.net.http.HttpClient` declares these as abstract; on the synthetic
    // bare-client object they have neither real bytecode nor a native, so any
    // call throws `AbstractMethodError: ... has no Code attribute`. Spring's
    // `JdkClientHttpRequestFactory` ctor calls `executor()`; the rest are filled
    // for parity so they degrade to JDK-default values instead of crashing.
    macro_rules! register_client_optional {
        ($method:literal, $field:expr) => {
            r.register(hc, $method, "()Ljava/util/Optional;", |ctx, args| {
                let this = obj_arg(args, 0)?;
                let value = ctx.get_field(this, $field);
                re5_optional(ctx, value)
            }, NativeKind::SyntheticStub);
        };
    }
    register_client_optional!("executor", RE5_CLIENT_EXECUTOR);
    register_client_optional!("connectTimeout", RE5_CLIENT_CONNECT_TIMEOUT);
    register_client_optional!("proxy", RE5_CLIENT_PROXY);
    register_client_optional!("authenticator", RE5_CLIENT_AUTHENTICATOR);
    register_client_optional!("cookieHandler", RE5_CLIENT_COOKIE_HANDLER);
    // version() -> HttpClient.Version (default HTTP_2); fetch the real enum
    // constant so the returned object is a genuine Version, not an int proxy.
    r.register(
        hc,
        "version",
        "()Ljava/net/http/HttpClient$Version;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            match ctx.get_field(this, RE5_CLIENT_VERSION) {
                configured @ Value::Object(Some(_)) => Ok(Some(configured)),
                _ => {
                    let name = ctx.create_string("HTTP_2");
                    ctx.invoke(
                        "java/net/http/HttpClient$Version",
                        "valueOf",
                        "(Ljava/lang/String;)Ljava/net/http/HttpClient$Version;",
                        &[Value::Object(Some(name))],
                    )
                }
            }
        }, NativeKind::SyntheticStub,
    );
    // followRedirects() -> HttpClient.Redirect (newHttpClient() default: NEVER).
    r.register(
        hc,
        "followRedirects",
        "()Ljava/net/http/HttpClient$Redirect;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            match ctx.get_field(this, RE5_CLIENT_REDIRECT) {
                configured @ Value::Object(Some(_)) => Ok(Some(configured)),
                _ => {
                    let name = ctx.create_string("NEVER");
                    ctx.invoke(
                        "java/net/http/HttpClient$Redirect",
                        "valueOf",
                        "(Ljava/lang/String;)Ljava/net/http/HttpClient$Redirect;",
                        &[Value::Object(Some(name))],
                    )
                }
            }
        }, NativeKind::SyntheticStub,
    );
    // sslContext() -> the JVM default SSLContext.
    r.register(
        hc,
        "sslContext",
        "()Ljavax/net/ssl/SSLContext;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            match ctx.get_field(this, RE5_CLIENT_SSL_CONTEXT) {
                configured @ Value::Object(Some(_)) => Ok(Some(configured)),
                _ => ctx.invoke(
                    "javax/net/ssl/SSLContext",
                    "getDefault",
                    "()Ljavax/net/ssl/SSLContext;",
                    &[],
                ),
            }
        }, NativeKind::SyntheticStub,
    );
    r.register(
        hc,
        "sslParameters",
        "()Ljavax/net/ssl/SSLParameters;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            match ctx.get_field(this, RE5_CLIENT_SSL_PARAMETERS) {
                configured @ Value::Object(Some(_)) => Ok(Some(configured)),
                _ => ctx.new_object_initialized("javax/net/ssl/SSLParameters", "()V", &[]),
            }
        }, NativeKind::SyntheticStub,
    );
    // close()/shutdown()/shutdownNow() (JDK 21+ AutoCloseable surface). Our
    // synthetic client owns no background selector or worker threads, so there
    // is nothing to interrupt — but the request still has to be RECORDED,
    // because `isTerminated()`/`awaitTermination()` below are defined purely in
    // terms of "a shutdown was requested AND all operations have completed".
    for (m, d) in [
        ("close", "()V"),
        ("shutdown", "()V"),
        ("shutdownNow", "()V"),
    ] {
        r.register(hc, m, d, |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = mint_native_obj_key(&*ctx, this);
            // gc-common w16-a: the receiver at entry is current; its address
            // is the row's weak owner (and, gc-common w28-a, where its key is
            // minted).
            re5_client_shutdown_table()
                .lock()
                .insert(id, this.as_ptr() as usize);
            Ok(None)
        }, NativeKind::SyntheticStub);
    }
    // A constant `true` here claimed the client had already finished
    // terminating before anyone asked it to stop — the opposite of the spec'd
    // answer for a live client, and a `while (!client.isTerminated())` drain
    // loop exits on the first iteration having shut nothing down. Report the
    // real state: false until a shutdown is requested, and — since no request
    // is ever outstanding on this client — terminated immediately after.
    r.register(hc, "isTerminated", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Looked up, never minted: a client never shut down has no key.
        let done = existing_native_obj_key(&*ctx, this)
            .is_some_and(|id| re5_client_shutdown_table().lock().contains_key(&id));
        Ok(Some(Value::Int(i32::from(done))))
    }, NativeKind::SyntheticStub);
    // `awaitTermination(Duration)` returns true iff the client terminated
    // before the timeout elapsed. Nothing can complete a shutdown that was
    // never requested, so a still-running client answers false rather than
    // pretending the wait succeeded.
    r.register(
        hc,
        "awaitTermination",
        "(Ljava/time/Duration;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let done = existing_native_obj_key(&*ctx, this)
                .is_some_and(|id| re5_client_shutdown_table().lock().contains_key(&id));
            Ok(Some(Value::Int(i32::from(done))))
        }, NativeKind::SyntheticStub,
    );

    let req = "java/net/http/HttpRequest";
    r.register(
        req,
        "newBuilder",
        "()Ljava/net/http/HttpRequest$Builder;",
        |ctx, _args| re5_new_request_builder(ctx, None), NativeKind::SyntheticStub,
    );
    r.register(
        req,
        "newBuilder",
        "(Ljava/net/URI;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            // MEASURED on HotSpot 25.0.3+9-LTS:
            //   HttpRequest.newBuilder((URI) null)
            //     -> NullPointerException: uri must be non-null
            // `obj_arg` would raise an NPE too, but with the generic
            // "null object argument" text that no differential can match.
            let uri = match args.first().copied() {
                Some(Value::Object(Some(u))) => u,
                _ => return Err(npe("uri must be non-null")),
            };
            re5_new_request_builder(ctx, Some(uri))
        }, NativeKind::SyntheticStub,
    );
    r.register(req, "timeout", "()Ljava/util/Optional;", |ctx, args| {
        let request = obj_arg(args, 0)?;
        match ctx.get_field(request, RE5_REQUEST_TIMEOUT_FIELD) {
            Value::Object(Some(timeout)) => ctx.invoke(
                "java/util/Optional",
                "of",
                "(Ljava/lang/Object;)Ljava/util/Optional;",
                &[Value::Object(Some(timeout))],
            ),
            _ => ctx.invoke("java/util/Optional", "empty", "()Ljava/util/Optional;", &[]),
        }
    }, NativeKind::SyntheticStub);
    // method()/uri() — public HttpRequest getters. `build()` above allocates
    // the returned object directly as class `java/net/http/HttpRequest`
    // (the abstract JDK class itself, not a concrete subclass), so any real
    // Java bytecode invoking these instance methods resolves against that
    // abstract declaration (no Code attribute) unless a native is registered
    // on this exact class name. Only field-0 (method) and field-1 (uri, a
    // plain String — see `newBuilder`/`uri` above) were previously
    // read/written internally by this file's own Rust helpers
    // (`re5_do_request` et al.); nothing exposed them back to Java callers.
    // Real-world callers building a request via this builder and then
    // inspecting it as a genuine `HttpRequest` (not just handing it to
    // `HttpClient.send`) hit `AbstractMethodError: method
    // java/net/http/HttpRequest.method()Ljava/lang/String; has no Code
    // attribute` — see
    // cacheautoconfigurationtests-hazelcast-httprequest-abstractmethoderror-FIXED.md
    // (Hazelcast's `RestClient.call` calls `request.method()` purely for its
    // own logging/retry bookkeeping after building the request).
    r.register(req, "method", "()Ljava/lang/String;", |ctx, args| {
        let request = obj_arg(args, 0)?;
        match ctx.get_field(request, RE5_REQUEST_METHOD) {
            m @ Value::Object(Some(_)) => Ok(Some(m)),
            _ => Ok(Some(Value::Object(Some(ctx.create_string("GET"))))),
        }
    }, NativeKind::SyntheticStub);
    r.register(req, "uri", "()Ljava/net/URI;", |ctx, args| {
        let request = obj_arg(args, 0)?;
        let uri_str = match ctx.get_field(request, RE5_REQUEST_URI) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let uri_string_obj = ctx.create_string(&uri_str);
        ctx.invoke(
            "java/net/URI",
            "create",
            "(Ljava/lang/String;)Ljava/net/URI;",
            &[Value::Object(Some(uri_string_obj))],
        )
    }, NativeKind::SyntheticStub);
    // G29-1. THE OTHER FOUR. `java.net.http.HttpRequest` declares seven
    // abstract instance methods; `method`/`uri`/`timeout` above had natives and
    // `version`/`bodyPublisher`/`expectContinue`/`headers` did not, so a request
    // this VM had just minted answered four of its own seven accessors with
    // `AbstractMethodError: ... has no Code attribute` (MEASURED, RJdkOptionalShape
    // and G29-1's `HttpProbe` on every builder shape). Nothing about the object
    // was missing — the builder had recorded the state for all four — the
    // registrations simply stopped after three. `HttpClient` a few hundred lines
    // above is the control: 7 of 7, and 7 of 7 measured correct.
    //
    // MEASURED on HotSpot 25.0.3+9-LTS for a request built with nothing set:
    // `version()` and `bodyPublisher()` are `Optional.empty`, `expectContinue()`
    // is `false`, `headers()` is an empty `HttpHeaders` — never null, and never
    // an exception.
    r.register(req, "version", "()Ljava/util/Optional;", |ctx, args| {
        let request = obj_arg(args, 0)?;
        let stored = ctx.get_field(request, RE5_REQUEST_VERSION);
        re5_optional(ctx, stored)
    }, NativeKind::SyntheticStub);
    r.register(
        req,
        "bodyPublisher",
        "()Ljava/util/Optional;",
        |ctx, args| {
            let request = obj_arg(args, 0)?;
            let stored = ctx.get_field(request, RE5_REQUEST_BODY_PUBLISHER);
            re5_optional(ctx, stored)
        }, NativeKind::SyntheticStub,
    );
    // A (Z)-returning accessor: the int encoding is CORRECT here, unlike the
    // reference-shaped setters. `RJdkOptionalShape.httpmint` calls this out
    // explicitly as the family's negative control.
    r.register(req, "expectContinue", "()Z", |ctx, args| {
        let request = obj_arg(args, 0)?;
        let flag = ctx
            .get_field(request, RE5_REQUEST_EXPECT_CONTINUE)
            .as_int()
            .unwrap_or(0);
        Ok(Some(Value::Int(i32::from(flag != 0))))
    }, NativeKind::SyntheticStub);
    r.register(
        req,
        "headers",
        "()Ljava/net/http/HttpHeaders;",
        |ctx, args| {
            let request = obj_arg(args, 0)?;
            let arr = ctx.get_field(request, RE5_REQUEST_HEADERS);
            let headers = re5_make_http_headers(ctx, arr)?;
            Ok(Some(Value::Object(Some(headers))))
        }, NativeKind::SyntheticStub,
    );
    // MEASURED: `HttpRequest.newBuilder(u).build().toString()` is
    // `http://example.com/x GET` — `jdk.internal.net.http.ImmutableHttpRequest`
    // overrides `toString`. Our object is stamped with the abstract class,
    // which declares no `toString`, so it inherited `Object`'s
    // `java.net.http.HttpRequest@1b6d3586`.
    r.register(req, "toString", "()Ljava/lang/String;", |ctx, args| {
        let request = obj_arg(args, 0)?;
        let uri = read_field_string_or(ctx, request, RE5_REQUEST_URI, "");
        let method = read_field_string_or(ctx, request, RE5_REQUEST_METHOD, "GET");
        let s = ctx.create_string(&format!("{uri} {method}"));
        Ok(Some(Value::Object(Some(s))))
    }, NativeKind::SyntheticStub);

    let bl = "java/net/http/HttpRequest$Builder";
    r.register(
        bl,
        "uri",
        "(Ljava/net/URI;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // MEASURED: HotSpot answers `NullPointerException: uri must be
            // non-null`, the same text as the static `newBuilder(null)`.
            let uri = match args.get(1).copied() {
                Some(Value::Object(Some(u))) => u,
                _ => return Err(npe("uri must be non-null")),
            };
            let uri_s = re5_uri_string(ctx, uri);
            ctx.set_field(this, RE5_REQUEST_URI, Value::Object(Some(uri_s)));
            Ok(Some(Value::Object(Some(this))))
        }, NativeKind::SyntheticStub,
    );
    // GET()/DELETE() take no publisher, and MEASURED on HotSpot they CLEAR any
    // publisher a previous POST/PUT put on the builder:
    // `newBuilder(u).GET().build().bodyPublisher()` is `Optional.empty`, while
    // `method("GET", BodyPublishers.noBody())` is `present:contentLength=0`.
    // The two are not the same request, and a builder that only ever adds
    // would report the first as the second.
    r.register(
        bl,
        "GET",
        "()Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            re5_builder_set_publisher(ctx, this, Value::Object(None));
            let this = re5_builder_set_method(ctx, this, "GET");
            Ok(Some(Value::Object(Some(this))))
        }, NativeKind::SyntheticStub,
    );
    r.register(
        bl,
        "DELETE",
        "()Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            re5_builder_set_publisher(ctx, this, Value::Object(None));
            let this = re5_builder_set_method(ctx, this, "DELETE");
            Ok(Some(Value::Object(Some(this))))
        }, NativeKind::SyntheticStub,
    );
    // HEAD() is a DEFAULT interface method — it has real bytecode, and that
    // bytecode is `method("HEAD", BodyPublishers.noBody())`, which would leave
    // the built request reporting `bodyPublisher() == present:0`. MEASURED on
    // HotSpot, `newBuilder(u).HEAD().build().bodyPublisher()` is **empty**:
    // `HttpRequestBuilderImpl` overrides the default and passes no publisher at
    // all, exactly as it does for `GET()`/`DELETE()`. Register it so the
    // default body cannot run and invent one.
    r.register(
        bl,
        "HEAD",
        "()Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            re5_builder_set_publisher(ctx, this, Value::Object(None));
            let this = re5_builder_set_method(ctx, this, "HEAD");
            Ok(Some(Value::Object(Some(this))))
        }, NativeKind::SyntheticStub,
    );
    // `register` takes a plain `fn` pointer, so these two cannot be folded into
    // a loop over the verb name without capturing it — hence one body each,
    // both delegating to the same helper so they cannot drift.
    r.register(
        bl,
        "POST",
        "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| re5_builder_verb_with_body(ctx, args, "POST"), NativeKind::SyntheticStub,
    );
    r.register(
        bl,
        "PUT",
        "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| re5_builder_verb_with_body(ctx, args, "PUT"), NativeKind::SyntheticStub,
    );
    r.register(
        bl,
        "header",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (k, v) = re5_check_header_pair(ctx, args)?;
            re5_builder_append_header(ctx, this, &format!("{k}: {v}"));
            Ok(Some(Value::Object(Some(this))))
        }, NativeKind::SyntheticStub,
    );
    // setHeader(name, value) — REPLACES every existing value for `name`.
    // Unregistered until G29-1: `HttpRequest$Builder` is an interface and the
    // builder is minted as an instance of it, so calling this threw
    // `AbstractMethodError: ... setHeader ... has no Code attribute` (MEASURED).
    r.register(
        bl,
        "setHeader",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (k, v) = re5_check_header_pair(ctx, args)?;
            re5_builder_set_header(ctx, this, &k, &v);
            Ok(Some(Value::Object(Some(this))))
        }, NativeKind::SyntheticStub,
    );
    // headers(String...) — name/value PAIRS, appended like `header`.
    // MEASURED: an odd count is `IllegalArgumentException: wrong number, 1, of
    // parameters`; `headers()` with zero arguments is accepted and is a no-op.
    r.register(
        bl,
        "headers",
        "([Ljava/lang/String;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let arr = match args.get(1).copied() {
                Some(Value::Object(Some(a))) => a,
                _ => return Err(npe_no_message()),
            };
            let n = ctx.array_length(arr);
            if n % 2 != 0 {
                return Err(iae(format!("wrong number, {n}, of parameters")));
            }
            let mut pairs: Vec<(String, String)> = Vec::with_capacity(n / 2);
            let mut i = 0;
            while i < n {
                let name = match ctx.get_array_element(arr, i) {
                    Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                    _ => return Err(npe_no_message()),
                };
                let value = match ctx.get_array_element(arr, i + 1) {
                    Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                    _ => return Err(npe_no_message()),
                };
                if name.is_empty() {
                    return Err(iae("invalid header name: \"\""));
                }
                if re5_header_is_restricted(ctx, &name) {
                    return Err(iae(format!("restricted header name: \"{name}\"")));
                }
                pairs.push((name, value));
                i += 2;
            }
            // GC-safety: `re5_builder_append_header` allocates the header
            // string it appends, so `this` is a pre-GC address from the second
            // header on.
            let this_pin = ctx.pin_native_root(this);
            for (name, value) in pairs {
                let this = ctx.read_native_pin(this_pin, this);
                re5_builder_append_header(ctx, this, &format!("{name}: {value}"));
            }
            let this = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            Ok(Some(Value::Object(Some(this))))
        }, NativeKind::SyntheticStub,
    );
    // copy() — an independent builder carrying the same state. The header
    // array must be COPIED, not shared: two builders that alias one array are
    // not two builders.
    r.register(
        bl,
        "copy",
        "()Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Two allocations happen below (the new builder, then the duplicate
            // header array), so BOTH the source and the copy have to survive a
            // collection that runs between them. One pin batch covers both:
            // `unpin_native_roots(this_pin)` releases everything pinned from
            // that point on, which is why the early-return path takes it too.
            let this_pin = ctx.pin_native_root(this);
            let copy = match re5_new_request_builder(ctx, None)? {
                Some(Value::Object(Some(c))) => c,
                other => {
                    ctx.unpin_native_roots(this_pin);
                    return Ok(other);
                }
            };
            let copy_pin = ctx.pin_native_root(copy);
            let src = ctx.read_native_pin(this_pin, this);
            let copy_now = ctx.read_native_pin(copy_pin, copy);
            for slot in 0..RE5_REQUEST_NUM_FIELDS {
                if slot == RE5_REQUEST_HEADERS {
                    continue;
                }
                let v = ctx.get_field(src, slot);
                ctx.set_field(copy_now, slot, v);
            }
            if let Value::Object(Some(arr)) = ctx.get_field(src, RE5_REQUEST_HEADERS) {
                let len = ctx.array_length(arr);
                let dup = ctx.new_array(ArrayElementType::Reference, len);
                let src = ctx.read_native_pin(this_pin, this);
                let arr = match ctx.get_field(src, RE5_REQUEST_HEADERS) {
                    Value::Object(Some(a)) => a,
                    _ => arr,
                };
                for i in 0..len {
                    let v = ctx.get_array_element(arr, i);
                    ctx.set_array_element(dup, i, v);
                }
                let copy_now = ctx.read_native_pin(copy_pin, copy);
                ctx.set_field(copy_now, RE5_REQUEST_HEADERS, Value::Object(Some(dup)));
            }
            let copy_now = ctx.read_native_pin(copy_pin, copy);
            ctx.unpin_native_roots(this_pin);
            Ok(Some(Value::Object(Some(copy_now))))
        }, NativeKind::SyntheticStub,
    );
    // method(String, BodyPublisher) — the generic verb setter Spring uses for
    // POST/PUT/PATCH (and any custom verb).
    r.register(
        bl,
        "method",
        "(Ljava/lang/String;Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // MEASURED on HotSpot:
            //   method(null, p)  -> NullPointerException (null message)
            //   method("",   p)  -> IllegalArgumentException: illegal method <empty string>
            //   method("POST", null) -> NullPointerException (null message)
            // The old body accepted all three: a null name left the previous
            // verb in place and an empty name was stored verbatim, so a
            // request could go on the wire with a method nobody chose.
            let name_obj = match args.get(1).copied() {
                Some(v @ Value::Object(Some(_))) => v,
                _ => return Err(npe_no_message()),
            };
            let name = value_or_string(ctx, name_obj, "");
            if name.is_empty() {
                return Err(iae("illegal method <empty string>"));
            }
            let publisher = match args.get(2).copied() {
                Some(v @ Value::Object(Some(_))) => v,
                _ => return Err(npe_no_message()),
            };
            ctx.set_field(this, RE5_REQUEST_METHOD, name_obj);
            re5_builder_set_publisher(ctx, this, publisher);
            Ok(Some(Value::Object(Some(this))))
        }, NativeKind::SyntheticStub,
    );
    r.register(
        bl,
        "timeout",
        "(Ljava/time/Duration;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // MEASURED: `timeout(null)` is a message-less NPE, and a
            // non-positive duration is
            // `IllegalArgumentException: Invalid duration: PT-1S` — the
            // Duration's own `toString`. The message this raised until G29-1
            // ("HttpRequest timeout must be positive") was the right refusal
            // with the wrong words, which a differential vector compares.
            let timeout = match args.get(1).copied() {
                Some(Value::Object(Some(d))) => d,
                _ => return Err(npe_no_message()),
            };
            // gc-common w20-d: `isZero` / `isNegative` / `toString` are Java
            // (a real `Duration`); the builder and the Duration were stored
            // and returned through their addresses from before them. Rooted.
            let mut scope = NativeHandleScope::new(ctx);
            let this_h = scope.root(this);
            let timeout_h = scope.root(timeout);
            let is_zero = matches!(
                scope.invoke_virtual(timeout, "isZero", "()Z", &[])?,
                Some(Value::Int(value)) if value != 0
            );
            let timeout = scope.get(&timeout_h);
            let is_negative = matches!(
                scope.invoke_virtual(timeout, "isNegative", "()Z", &[])?,
                Some(Value::Int(value)) if value != 0
            );
            let timeout = scope.get(&timeout_h);
            if is_zero || is_negative {
                let text =
                    match scope.invoke_virtual(timeout, "toString", "()Ljava/lang/String;", &[])? {
                        Some(Value::Object(Some(s))) => scope.read_string(s).unwrap_or_default(),
                        _ => String::new(),
                    };
                return Err(iae(format!("Invalid duration: {text}")));
            }
            let this = scope.get(&this_h);
            scope.set_field(
                this,
                RE5_REQUEST_TIMEOUT_FIELD,
                Value::Object(Some(timeout)),
            );
            Ok(Some(Value::Object(Some(this))))
        }, NativeKind::SyntheticStub,
    );
    // expectContinue(boolean) / version(Version) — both were `Ok(Some(args[0]))`
    // until G29-1: they returned the builder for the fluent chain and DISCARDED
    // the argument, so the two matching accessors on the built request had
    // nothing to read even once they existed. (This is the second half of the
    // same defect: `HttpRequest.version()` had no native AND no state.)
    r.register(
        bl,
        "expectContinue",
        "(Z)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let flag = args.get(1).copied().and_then(|v| v.as_int()).unwrap_or(0);
            ctx.set_field(
                this,
                RE5_REQUEST_EXPECT_CONTINUE,
                Value::Int(i32::from(flag != 0)),
            );
            Ok(Some(Value::Object(Some(this))))
        }, NativeKind::SyntheticStub,
    );
    r.register(
        bl,
        "version",
        "(Ljava/net/http/HttpClient$Version;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // MEASURED: `version(null)` is a message-less NPE on HotSpot.
            let version = match args.get(1).copied() {
                Some(v @ Value::Object(Some(_))) => v,
                _ => return Err(npe_no_message()),
            };
            ctx.set_field(this, RE5_REQUEST_VERSION, version);
            Ok(Some(Value::Object(Some(this))))
        }, NativeKind::SyntheticStub,
    );

    r.register(bl, "build", "()Ljava/net/http/HttpRequest;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // MEASURED on HotSpot, in this order:
        //   newBuilder().build()                    -> IllegalStateException: uri is null
        //   newBuilder(URI.create("/relative"))     -> IllegalArgumentException: URI with undefined scheme
        //   newBuilder(URI.create("ftp://h/x"))     -> IllegalArgumentException: invalid URI scheme ftp
        // The refusals live on `build()` and not on `uri()` because that is
        // where HotSpot puts them: `newBuilder(URI.create("ftp://..."))` alone
        // does not throw.
        let uri_text = read_field_string(ctx, this, RE5_REQUEST_URI);
        match uri_text.as_deref() {
            None => return Err(ise("uri is null")),
            Some(text) => match text.find(':') {
                None => return Err(iae("URI with undefined scheme")),
                Some(colon) => {
                    let scheme = text[..colon].to_ascii_lowercase();
                    if scheme != "http" && scheme != "https" {
                        return Err(iae(format!("invalid URI scheme {scheme}")));
                    }
                }
            },
        }
        // gc-common w20-d: the builder is rooted across the request's
        // allocation and read back (its slots were copied through its
        // pre-allocation address).
        let mut scope = NativeHandleScope::new(ctx);
        let this_h = scope.root(this);
        let req = try_alloc_concurrent_synthetic(
            &mut *scope,
            "java/net/http/HttpRequest",
            RE5_REQUEST_NUM_FIELDS,
        )?;
        let this = scope.get(&this_h);
        let ctx = &mut *scope;
        // Slot-for-slot, over the WHOLE range. This loop read `0..5` while the
        // builder carried five slots; every slot added after that would have
        // been silently dropped at `build()` — which is how `expectContinue`
        // and `version` could round-trip on the builder and vanish on the
        // request. `RE5_REQUEST_NUM_FIELDS` is the single name both ends use.
        for i in 0..RE5_REQUEST_NUM_FIELDS {
            let v = ctx.get_field(this, i);
            ctx.set_field(req, i, v);
        }
        Ok(Some(Value::Object(Some(req))))
    }, NativeKind::SyntheticStub);

    let bps = "java/net/http/HttpRequest$BodyPublishers";
    r.register(
        bps,
        "ofString",
        "(Ljava/lang/String;)Ljava/net/http/HttpRequest$BodyPublisher;",
        |ctx, args| {
            let payload = args.first().copied().unwrap_or(Value::Object(None));
            re5_new_body_publisher(ctx, payload, None)
        }, NativeKind::SyntheticStub,
    );
    r.register(
        bps,
        "noBody",
        "()Ljava/net/http/HttpRequest$BodyPublisher;",
        |ctx, _args| {
            let empty = ctx.create_string("");
            re5_new_body_publisher(ctx, Value::Object(Some(empty)), None)
        }, NativeKind::SyntheticStub,
    );
    r.register(
        bps,
        "ofByteArray",
        "([B)Ljava/net/http/HttpRequest$BodyPublisher;",
        |ctx, args| {
            // Keep the original byte[] rather than round-tripping it through a
            // Java String.  Request builders copy this literal value into their
            // request body slot, and `re5_request_body_bytes` already knows how
            // to materialise byte[] verbatim.  String::from_utf8_lossy changed
            // every non-UTF-8 octet into U+FFFD, corrupting compressed Zipkin
            // payloads (notably gzip's 0x8b and 0xff bytes) on the wire.
            let payload = args.first().copied().unwrap_or(Value::Object(None));
            re5_new_body_publisher(ctx, payload, None)
        }, NativeKind::SyntheticStub,
    );
    // fromPublisher(Flow.Publisher[, contentLength]) — Spring's streaming
    // POST/PUT path. Keep the publisher object in slot 0; `re5_do_request`
    // subscribes a native collector, requests demand, and assembles the emitted
    // ByteBuffers into the wire body before opening the socket. The one-arg
    // overload has NO length until then, which is exactly what a declared
    // length of -1 means; the two-arg overload carries the caller's.
    for desc in [
        "(Ljava/util/concurrent/Flow$Publisher;)Ljava/net/http/HttpRequest$BodyPublisher;",
        "(Ljava/util/concurrent/Flow$Publisher;J)Ljava/net/http/HttpRequest$BodyPublisher;",
    ] {
        r.register(bps, "fromPublisher", desc, |ctx, args| {
            let payload = args.first().copied().unwrap_or(Value::Object(None));
            let declared = match args.get(1).copied() {
                Some(Value::Long(n)) => Some(n),
                Some(Value::Int(n)) => Some(i64::from(n)),
                _ => Some(-1),
            };
            re5_new_body_publisher(ctx, payload, declared)
        }, NativeKind::SyntheticStub);
    }
    // G29-1. `HttpRequest$BodyPublisher` is an INTERFACE declaring exactly one
    // method, `contentLength()J`, and it had no native at all — the same 0-of-1
    // ratio `PathMatcher.matches` had. Every publisher the five factories above
    // mint is stamped with that interface, so
    // `BodyPublishers.ofString("hi").contentLength()` threw
    // `AbstractMethodError: ... has no Code attribute` (MEASURED), and
    // `RJdkOptionalShape.httpmint` asks for it twice.
    //
    // MEASURED on HotSpot 25.0.3+9-LTS:
    //   noBody()                  -> 0
    //   ofString("hello")         -> 5   (UTF-8 bytes, not chars)
    //   ofByteArray(new byte[5])  -> 5
    //   fromPublisher(p)          -> -1
    //   fromPublisher(p, 12)      -> 12
    r.register(
        "java/net/http/HttpRequest$BodyPublisher",
        "contentLength",
        "()J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Value::Long(declared) = ctx.get_field(this, RE5_BP_DECLARED_LENGTH) {
                return Ok(Some(Value::Long(declared)));
            }
            let payload = ctx.get_field(this, RE5_BP_PAYLOAD);
            let bytes = re5_request_body_bytes(ctx, payload, Re5Caller::ContentLength)?;
            Ok(Some(Value::Long(bytes.len() as i64)))
        }, NativeKind::SyntheticStub,
    );

    let bhs = "java/net/http/HttpResponse$BodyHandlers";
    r.register(
        bhs,
        "ofString",
        "()Ljava/net/http/HttpResponse$BodyHandler;",
        |ctx, _args| re5_tagged_body_handler(ctx, "string"), NativeKind::SyntheticStub,
    );
    r.register(
        bhs,
        "discarding",
        "()Ljava/net/http/HttpResponse$BodyHandler;",
        |ctx, _args| re5_tagged_body_handler(ctx, "discarding"), NativeKind::SyntheticStub,
    );
    // ofInputStream() / ofByteArray() — the body shapes Spring's
    // `JdkClientHttpRequest` consumes (it always reads `response.body()` as an
    // InputStream). The tag drives `HttpResponse.body()` materialisation.
    r.register(
        bhs,
        "ofInputStream",
        "()Ljava/net/http/HttpResponse$BodyHandler;",
        |ctx, _args| re5_tagged_body_handler(ctx, "inputstream"), NativeKind::SyntheticStub,
    );
    r.register(
        bhs,
        "ofByteArray",
        "()Ljava/net/http/HttpResponse$BodyHandler;",
        |ctx, _args| re5_tagged_body_handler(ctx, "bytearray"), NativeKind::SyntheticStub,
    );

    let resp = "java/net/http/HttpResponse";
    r.register(resp, "statusCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, RE5_RESP_STATUS)))
    }, NativeKind::SyntheticStub);
    // body() materialises the stored byte[] per the BodyHandler tag captured at
    // send time: an InputStream (default / ofInputStream), a String (ofString),
    // the raw byte[] (ofByteArray), null (discarding), or -- for a real
    // user-supplied BodyHandler -- the exact value the driven handler's
    // BodySubscriber produced (RE5_TAG_HANDLED; same instance every call,
    // matching the real client).
    r.register(resp, "body", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let tag = read_field_string_or(ctx, this, RE5_RESP_HANDLER_TAG, "inputstream");
        let body_val = ctx.get_field(this, RE5_RESP_BODY_BYTES);
        match tag.as_str() {
            RE5_TAG_HANDLED => Ok(Some(ctx.get_field(this, RE5_RESP_BODY_OBJ))),
            "discarding" => Ok(Some(Value::Object(None))),
            "bytearray" => Ok(Some(body_val)),
            "string" => {
                let bytes = match body_val {
                    Value::Object(Some(a)) => re5_read_byte_array(ctx, a),
                    _ => Vec::new(),
                };
                let s = ctx.create_string(&String::from_utf8_lossy(&bytes));
                Ok(Some(Value::Object(Some(s))))
            }
            _ => {
                // InputStream: wrap the byte[] in a real ByteArrayInputStream.
                let arr = match body_val {
                    Value::Object(Some(_)) => body_val,
                    _ => Value::Object(Some(ctx.new_array(ArrayElementType::Byte, 0))),
                };
                ctx.new_object_initialized("java/io/ByteArrayInputStream", "([B)V", &[arr])
            }
        }
    }, NativeKind::SyntheticStub);
    // headers() -> HttpHeaders backed by the stored "key: value" String[].
    r.register(
        resp,
        "headers",
        "()Ljava/net/http/HttpHeaders;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let arr = ctx.get_field(this, RE5_RESP_HEADERS);
            let headers = re5_make_http_headers(ctx, arr)?;
            Ok(Some(Value::Object(Some(headers))))
        }, NativeKind::SyntheticStub,
    );
    // The five HttpResponse registered this carrier was missing entirely --
    // uri()/request()/version()/previousResponse()/sslSession() all threw
    // AbstractMethodError on a live send() call because nothing here answered
    // for them (http2.rs's own registrations for the same five names exist
    // but are inert outside --synthetic-jdk; see run.sh lines 150-167, lane
    // E2). See
    // docs/feature-designs/native-dispatch-architecture.md
    // for the full measurement and
    // native-builtins/src/net_phase_e.rs's `re5_finish_response` for where
    // RE5_RESP_URI/REQUEST/PREV/SSL_SESSION are actually populated -- this
    // block only reads what that function already wrote.
    r.register(resp, "uri", "()Ljava/net/URI;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, RE5_RESP_URI)))
    }, NativeKind::SyntheticStub);
    r.register(
        resp,
        "request",
        "()Ljava/net/http/HttpRequest;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, RE5_RESP_REQUEST)))
        }, NativeKind::SyntheticStub,
    );
    // version() -> HttpClient.Version, NOT Optional -- unlike HttpRequest's
    // accessor of the same name. This client only ever speaks HTTP/1.1 on the
    // wire (`http_build_request`), so it is a constant rather than a stored
    // field: fetch the real enum constant, matching the pattern
    // `HttpClient.version()` a few hundred lines above already uses, so the
    // returned object is a genuine Version and not an int proxy.
    r.register(
        resp,
        "version",
        "()Ljava/net/http/HttpClient$Version;",
        |ctx, _args| {
            let name = ctx.create_string("HTTP_1_1");
            ctx.invoke(
                "java/net/http/HttpClient$Version",
                "valueOf",
                "(Ljava/lang/String;)Ljava/net/http/HttpClient$Version;",
                &[Value::Object(Some(name))],
            )
        }, NativeKind::SyntheticStub,
    );
    r.register(
        resp,
        "previousResponse",
        "()Ljava/util/Optional;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let prev = ctx.get_field(this, RE5_RESP_PREV);
            re5_optional(ctx, prev)
        }, NativeKind::SyntheticStub,
    );
    r.register(resp, "sslSession", "()Ljava/util/Optional;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let session = ctx.get_field(this, RE5_RESP_SSL_SESSION);
        re5_optional(ctx, session)
    }, NativeKind::SyntheticStub);

    // HttpResponse.ResponseInfo -- the argument re5_drive_body_handler hands
    // to a real BodyHandler's apply(). Same synthetic-receiver pattern as the
    // HttpResponse natives above.
    r.register(RE5_RESPONSE_INFO, "statusCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, RE5_RI_STATUS)))
    }, NativeKind::SyntheticStub);
    r.register(
        RE5_RESPONSE_INFO,
        "headers",
        "()Ljava/net/http/HttpHeaders;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let arr = ctx.get_field(this, RE5_RI_HEADERS);
            let headers = re5_make_http_headers(ctx, arr)?;
            Ok(Some(Value::Object(Some(headers))))
        }, NativeKind::SyntheticStub,
    );
    r.register(
        RE5_RESPONSE_INFO,
        "version",
        "()Ljava/net/http/HttpClient$Version;",
        |ctx, _args| {
            // The bare client speaks HTTP/1.1.
            let name = ctx.create_string("HTTP_1_1");
            ctx.invoke(
                "java/net/http/HttpClient$Version",
                "valueOf",
                "(Ljava/lang/String;)Ljava/net/http/HttpClient$Version;",
                &[Value::Object(Some(name))],
            )
        }, NativeKind::SyntheticStub,
    );

    // One-shot replay Flow.Subscription (see re5_drive_body_handler /
    // re5_replay_subscription_request). Registered on its own wrapper class
    // name; abstract Flow$Subscription dispatch finds receiver-class natives.
    r.register(
        RE5_REPLAY_SUBSCRIPTION,
        "request",
        "(J)V",
        re5_replay_subscription_request, NativeKind::SyntheticStub,
    );
    r.register(
        RE5_REPLAY_SUBSCRIPTION,
        "cancel",
        "()V",
        re5_replay_subscription_cancel, NativeKind::SyntheticStub,
    );

    // HttpHeaders.map() -> Map<String, List<String>>. Spring's
    // `JdkClientHttpResponse.adaptHeaders` calls `response.headers().map()` and
    // iterates it, so this must be a real Map of real Lists. Header names are
    // grouped case-insensitively, preserving first-seen order.
    let hh = "java/net/http/HttpHeaders";
    r.register(hh, "map", "()Ljava/util/Map;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Grouping is shared with `firstValue`/`allValues`/`firstValueAsLong`/
        // `toString` (`re5_header_groups`) so the five cannot come to disagree
        // about what a header name means. It was open-coded here.
        //
        // MEASURED on HotSpot 25.0.3+9-LTS: the map a real `HttpHeaders`
        // returns is sorted CASE-INSENSITIVELY by name, not kept in first-seen
        // order — `header("Z-Last",…).header("a-mid",…).header("A-First",…)`
        // gives `{A-First=[3], a-mid=[2], Z-Last=[1]}`. (The real backing
        // store is a `TreeMap<>(String.CASE_INSENSITIVE_ORDER)` made
        // unmodifiable.) Values WITHIN one name stay in insertion order.
        let mut groups = re5_header_groups(ctx, this);
        groups.sort_by(|a, b| {
            a.0.to_ascii_lowercase()
                .cmp(&b.0.to_ascii_lowercase())
                .then_with(|| a.0.cmp(&b.0))
        });
        let map_val = ctx.new_object_initialized("java/util/LinkedHashMap", "()V", &[])?;
        let map = match map_val {
            Some(Value::Object(Some(m))) => m,
            _ => return Ok(map_val),
        };
        // Pin the map across the (allocating) per-entry construction so a moving
        // collector can't invalidate the reference we keep `put`-ing into.
        let map_pin = ctx.pin_native_root(map);
        for (k, vals) in groups {
            let list_val = ctx.new_object_initialized("java/util/ArrayList", "()V", &[])?;
            let list = match list_val {
                Some(Value::Object(Some(l))) => l,
                _ => continue,
            };
            let list_pin = ctx.pin_native_root(list);
            for v in vals {
                let vs = ctx.create_string(&v);
                let list_now = ctx.read_native_pin(list_pin, list);
                ctx.invoke_virtual(
                    list_now,
                    "add",
                    "(Ljava/lang/Object;)Z",
                    &[Value::Object(Some(vs))],
                )?;
            }
            let ks = ctx.create_string(&k);
            let list_now = ctx.read_native_pin(list_pin, list);
            let map_now = ctx.read_native_pin(map_pin, map);
            ctx.invoke_virtual(
                map_now,
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                &[Value::Object(Some(ks)), Value::Object(Some(list_now))],
            )?;
        }
        let map_now = ctx.read_native_pin(map_pin, map);
        ctx.unpin_native_roots(map_pin);
        Ok(Some(Value::Object(Some(map_now))))
    }, NativeKind::SyntheticStub);
    // G29-1. `headers()` is one of the four accessors that had no native, so
    // until now nothing could reach an `HttpHeaders` from a REQUEST at all and
    // `map()` was the only method this class carried. `java.net.http.HttpHeaders`
    // declares four public readers; the other three were reachable only from a
    // response and answered by real JDK bytecode reading slot 0 as the `Map` it
    // declares — but slot 0 holds a `String[]` of "key: value" lines. Register
    // all four against the shape this VM actually mints.
    //
    // MEASURED on HotSpot 25.0.3+9-LTS, headers {Accept: text/plain,
    // Accept: text/html, X-Num: 42}:
    //   firstValue("Accept")     -> Optional[text/plain]
    //   firstValue("accept")     -> Optional[text/plain]   (case-insensitive)
    //   firstValue("Nope")       -> Optional.empty
    //   allValues("Accept")      -> [text/plain, text/html]
    //   allValues("Nope")        -> []                     (empty list, not null)
    //   firstValueAsLong("X-Num")-> OptionalLong[42]
    //   firstValueAsLong("Nope") -> OptionalLong.empty
    //   firstValueAsLong("Accept") -> NumberFormatException: For input string: "text/plain"
    //   firstValue(null)         -> NullPointerException (null message)
    r.register(
        hh,
        "firstValue",
        "(Ljava/lang/String;)Ljava/util/Optional;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name = re5_header_name_arg(ctx, args)?;
            let first = re5_header_groups(ctx, this)
                .into_iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(&name))
                .and_then(|(_, v)| v.into_iter().next());
            match first {
                Some(v) => {
                    let s = ctx.create_string(&v);
                    ctx.invoke(
                        "java/util/Optional",
                        "of",
                        "(Ljava/lang/Object;)Ljava/util/Optional;",
                        &[Value::Object(Some(s))],
                    )
                }
                None => ctx.invoke("java/util/Optional", "empty", "()Ljava/util/Optional;", &[]),
            }
        }, NativeKind::SyntheticStub,
    );
    r.register(
        hh,
        "allValues",
        "(Ljava/lang/String;)Ljava/util/List;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name = re5_header_name_arg(ctx, args)?;
            let values = re5_header_groups(ctx, this)
                .into_iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(&name))
                .map(|(_, v)| v)
                .unwrap_or_default();
            let list_val = ctx.new_object_initialized("java/util/ArrayList", "()V", &[])?;
            let list = match list_val {
                Some(Value::Object(Some(l))) => l,
                _ => return Ok(list_val),
            };
            let list_pin = ctx.pin_native_root(list);
            for v in values {
                let vs = ctx.create_string(&v);
                let list_now = ctx.read_native_pin(list_pin, list);
                ctx.invoke_virtual(
                    list_now,
                    "add",
                    "(Ljava/lang/Object;)Z",
                    &[Value::Object(Some(vs))],
                )?;
            }
            let list_now = ctx.read_native_pin(list_pin, list);
            ctx.unpin_native_roots(list_pin);
            Ok(Some(Value::Object(Some(list_now))))
        }, NativeKind::SyntheticStub,
    );
    r.register(
        hh,
        "firstValueAsLong",
        "(Ljava/lang/String;)Ljava/util/OptionalLong;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name = re5_header_name_arg(ctx, args)?;
            let first = re5_header_groups(ctx, this)
                .into_iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(&name))
                .and_then(|(_, v)| v.into_iter().next());
            match first {
                None => ctx.invoke(
                    "java/util/OptionalLong",
                    "empty",
                    "()Ljava/util/OptionalLong;",
                    &[],
                ),
                // A header that is present but not a number is an ERROR, not
                // an empty Optional: HotSpot lets `Long.parseLong`'s
                // NumberFormatException out, message included. Answering
                // `empty` here would make an unparsable Content-Length look
                // like an absent one.
                Some(text) => match text.trim().parse::<i64>() {
                    Ok(n) => ctx.invoke(
                        "java/util/OptionalLong",
                        "of",
                        "(J)Ljava/util/OptionalLong;",
                        &[Value::Long(n)],
                    ),
                    Err(_) => Err(RuntimeError::NumberFormatException {
                        message: format!("For input string: \"{text}\""),
                    }
                    .into()),
                },
            }
        }, NativeKind::SyntheticStub,
    );
    // MEASURED: `java.net.http.HttpHeaders@361e4cfc { {A-First=[3], …} }` —
    // the Object-style identity prefix, then the map in braces.
    r.register(hh, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let mut groups = re5_header_groups(ctx, this);
        groups.sort_by(|a, b| {
            a.0.to_ascii_lowercase()
                .cmp(&b.0.to_ascii_lowercase())
                .then_with(|| a.0.cmp(&b.0))
        });
        let body = groups
            .iter()
            .map(|(k, v)| format!("{k}=[{}]", v.join(", ")))
            .collect::<Vec<_>>()
            .join(", ");
        let hash = ctx.identity_hash_code(this);
        let text = format!("java.net.http.HttpHeaders@{hash:x} {{ {{{body}}} }}");
        let s = ctx.create_string(&text);
        Ok(Some(Value::Object(Some(s))))
    }, NativeKind::SyntheticStub);
}

// ===========================================================================
// RE.6 — javax.net.ssl.SSLContext
// ===========================================================================

/// A synthetic 2-slot `SSLContext` whose slot 0 is the protocol name.
///
/// gc-common w20-d: the name String is allocated first and rooted across the
/// context's allocation; `getInstance` and `getDefault` held the context raw
/// across the String's allocation and stored the name through it.
pub fn re6_alloc_named_ssl_context(
    ctx: &mut dyn NativeContext,
    protocol: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let name = ctx.create_string(protocol);
    let mut scope = NativeHandleScope::new(ctx);
    let name_h = scope.root(name);
    let obj = try_alloc_concurrent_synthetic(&mut *scope, "javax/net/ssl/SSLContext", 2)?;
    let name = scope.get(&name_h);
    scope.set_field(obj, 0, Value::Object(Some(name)));
    Ok(obj)
}

/// `new SSLParameters(cipherSuites, protocols)` over two fresh `String[]`s.
///
/// gc-common w20-d: the one body `getSupportedSSLParameters` and
/// `getDefaultSSLParameters` carried inline. Each array was held raw across
/// one `create_string` per element (a few dozen cipher-suite names), and the
/// cipher array across the whole protocol array's construction. The arrays
/// are built by `t27_tls::materialize_java_string_array` (rooted across its
/// own element allocations, typed `String[]` as `SSLParameters`' setters
/// expect), and the first is rooted across the second.
pub fn re6_new_ssl_parameters(
    ctx: &mut dyn NativeContext,
    ciphers: &[&str],
    protocols: &[&str],
) -> MethodCallResult {
    let carr = crate::t27_tls::materialize_java_string_array(ctx, ciphers);
    let mut scope = NativeHandleScope::new(ctx);
    let carr_h = scope.root(carr);
    let parr = crate::t27_tls::materialize_java_string_array(&mut *scope, protocols);
    let carr = scope.get(&carr_h);
    // SSLParameters(String[] cipherSuites, String[] protocols)
    scope.new_object_initialized(
        "javax/net/ssl/SSLParameters",
        "([Ljava/lang/String;[Ljava/lang/String;)V",
        &[Value::Object(Some(carr)), Value::Object(Some(parr))],
    )
}

/// `pub` since 2026-08-12 (W7-61) so `tls.rs`'s registrar-ordering
/// ratchet can name this exact registrar rather than the whole
/// `register_phase_e_networking` umbrella. It is the LAST writer of
/// `javax/net/ssl/SSLContext.createSSLEngine` on the Compatible/strict boot
/// path (lib.rs 18191 `register_p68_ssl`, then 18214 this), and that is what
/// makes `phases_late::ssl_security::ssleng_alloc` — a 7-slot allocation on a
/// class declaring 2 — unreachable in that mode. Reordering the two silently
/// re-arms the over-allocation, so the fact is pinned by a test rather than
/// left to this comment.
pub fn register_re6_ssl_context(r: &mut NativeMethodRegistry) {
    let ctx_cls = "javax/net/ssl/SSLContext";
    r.register(
        ctx_cls,
        "getInstance",
        "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;",
        |ctx, args| {
            if crate::nbflags().dbg_tls_auth_ok {
                eprintln!("[dbg-tls-auth] re6 SSLContext.getInstance");
            }
            let proto_val = args.first().copied().unwrap_or(Value::Object(None));
            // MEASURED on HotSpot 25.0.4+7: `SSLContext.getInstance(null)` is
            // `NullPointerException: null protocol name`, from
            // `Objects.requireNonNull(protocol, "null protocol name")` at the
            // top of the method. `value_or_string`'s default turned it into a
            // working "TLS" context, so a caller who computed a null protocol
            // got TLS and no signal.
            if matches!(proto_val, Value::Object(None)) {
                return Err(npe("null protocol name"));
            }
            let proto = value_or_string(ctx, proto_val, "TLS");
            // W3-7 (RJdkSecurity.tls:291). TWO defects on this line.
            //
            // (1) WRONG TYPE. `ioex` builds a `java.io.IOException`, so
            //     `getInstance("NO-SUCH-TLS")` threw
            //     `IOException: NoSuchAlgorithmException: NO-SUCH-TLS` — the
            //     right words in the message, the wrong class on the wire.
            //     `java.io.IOException` and `java.security.NoSuchAlgorithm-
            //     Exception` (via GeneralSecurityException) are disjoint below
            //     `Exception`, so the caller's `catch (NoSuchAlgorithmException)`
            //     did not match and the refusal escaped to main.
            //
            // (2) TOO NARROW. The five-name list refused `TLSv1`, `TLSv1.1`,
            //     `SSLv3` and all three DTLS protocols — every one of which
            //     SunJSSE really registers on JDK 25 (measured). A too-narrow
            //     accept list is the more dangerous half: it turns a valid
            //     `getInstance` into a refusal and takes every HTTPS-using
            //     suite with it. The decision now lives in ONE place shared
            //     with `phases_late::ssl_security::register_p68_ssl` and both
            //     `tls.rs` registrations, so the four cannot drift again.
            if !crate::jca::provider_chain::ssl_context_protocol_supported_in(
                ctx.vm_identity(),
                &proto,
            ) {
                return Err(crate::jca::provider_chain::throw_no_such_algorithm_public(
                    ctx,
                    &format!("{proto} SSLContext not available"),
                ));
            }
            // gc-common w20-d: the name first, rooted across the context's
            // allocation (the context was held raw across the String's).
            let obj = re6_alloc_named_ssl_context(ctx, &proto)?;
            // The raw `ctx.set_field(obj, 1, ...)` this line used to carry
            // (mirroring the pre-side-table "initialized" bookkeeping) is
            // gone: `mark_context_uninitialized`/`mark_context_initialized`
            // below is the only initialized-state this file ever reads back
            // (`require_context_initialized`, not `tls.rs`'s field-based
            // checker), so the field write was pure dead weight -- and
            // harmful dead weight under real-JDK mode, where the real
            // `javax.net.ssl.SSLContext` class's field 1 is `contextSpi :
            // SSLContextSpi` (reference-typed), so writing an `Int` there hit
            // the descriptor-aware field guard and silently destroyed
            // whatever `contextSpi` held (see
            // `docs/feature-designs/native-dispatch-architecture.md`,
            // itself `G30-1`'s already-catalogued `N4` finding for this exact
            // `(class, slot)`).
            crate::jca::ssl_context_spi::mark_context_uninitialized(ctx, obj);
            Ok(Some(Value::Object(Some(obj))))
        }, NativeKind::SyntheticStub,
    );
    r.register(
        ctx_cls,
        "getDefault",
        "()Ljavax/net/ssl/SSLContext;",
        |ctx, _args| {
            // FIX (es-restclientbuilder-ssl-default-context-20260710): if
            // `SSLContext.setDefault(ctx)` installed a context (see the new
            // `setDefault` registration below), return that SAME object --
            // not a fresh, unconfigured one -- so callers that rely on the
            // JDK's documented getDefault()/setDefault() contract (e.g.
            // RestClientBuilder.build() -> SSLContext.getDefault() at
            // RestClientBuilder.java:330) see the trust/key managers that
            // were attached to it at `init()` time. This is the LIVE
            // getDefault()/init() registration in real-JDK mode: it is
            // registered here, in `register_re6_ssl_context` (called from
            // `register_essential_natives`, unconditionally), AFTER
            // `phases_late::register_p68_ssl`'s own getDefault/init (called a
            // few lines earlier in the same function) -- so this
            // implementation wins via last-registered-wins. The THIRD and
            // FOURTH registrations of the same (class, method, descriptor)
            // triple, in `tls.rs::register_ssl_context` and a second call to
            // `phases_late::register_p68_ssl`, both live inside
            // `register_synthetic_overrides`, which is
            // `#[cfg(feature = "synthetic-jdk")]`-gated to a no-op shim in
            // the default real-JDK build -- so they never run here and are
            // not live competitors for last-write-wins.
            if let Some(ctx_obj) =
                crate::t27_tls::get_runtime_default_ssl_context(ctx.vm_identity())
            {
                return Ok(Some(Value::Object(Some(ctx_obj))));
            }
            // No explicit setDefault() call yet: mirror the JDK's lazy-init
            // contract (SSLContext.getDefault() javadoc — the default is
            // "created if not yet created") by allocating ONE context and
            // caching it in the same slot setDefault() writes to, so a
            // second getDefault() call returns this SAME object instead of
            // a fresh one each time (e.g. OtlpMetricsExportAutoConfigurationTests
            // .whenNoSslBundleDefaultHttpSenderHasDefaultSslContext asserts
            // `httpClient.sslContext()).isSameAs(SSLContext.getDefault())`).
            // The lazily-created default's protocol is the literal string
            // "Default", not "TLS". MEASURED on jdk-25.0.3.9-hotspot:
            // `SSLContext.getDefault().getProtocol()` answers `Default`, which
            // is `SSLContext.getInstance("Default")` -- the name the JDK
            // registers its pre-initialised context under. "TLS" is the name of
            // a context you asked for by protocol and then have to `init()`
            // yourself, so the two are not interchangeable: code that switches
            // on `getProtocol()` to decide whether a context is ready to use
            // reads this one as unconfigured.
            //
            // gc-common w20-d: the name first (see `getInstance`).
            let obj = re6_alloc_named_ssl_context(ctx, "Default")?;
            // See the `getInstance` registration above: the raw field write
            // this line used to carry is dead-weight-turned-harmful under
            // real-JDK mode (G30-1's N4, `SSLContext#1 contextSpi`) and is
            // gone; `mark_context_initialized` is the only bookkeeping this
            // file's own checker (`require_context_initialized`) reads.
            crate::jca::ssl_context_spi::mark_context_initialized(ctx, obj);
            // nio21 wave 13 (L4): SunJSSE's default context trusts the default
            // trust store (`javax.net.ssl.trustStore`, else `cacerts`).
            let obj = crate::t27_tls::scope_default_context_trust(ctx, obj)?;
            crate::t27_tls::set_runtime_default_ssl_context(ctx.vm_identity(), obj);
            Ok(Some(Value::Object(Some(obj))))
        }, NativeKind::SyntheticStub,
    );
    // `SSLContext.setDefault(SSLContext)` -- previously missing entirely
    // (confirmed by grep across phases_late.rs/net_phase_e.rs/tls.rs), which
    // made `getDefault()` above always return a fresh, unconfigured context
    // even after a caller called `setDefault` with a fully `.init()`'d one.
    // Static method: args[0] is the SSLContext parameter, no receiver.
    r.register(
        ctx_cls,
        "setDefault",
        "(Ljavax/net/ssl/SSLContext;)V",
        |ctx, args| match args.first().copied() {
            Some(Value::Object(Some(ctx_obj))) => {
                crate::t27_tls::set_runtime_default_ssl_context(ctx.vm_identity(), ctx_obj);
                Ok(None)
            }
            // `SSLContext.setDefault(null)` is `Objects.requireNonNull(context)`
            // — the one-argument overload, so the NPE carries NO message.
            // MEASURED: HotSpot `msg=null`, this VM `msg=context`.
            _ => Err(npe_no_message()),
        }, NativeKind::SyntheticStub,
    );
    r.register(
        ctx_cls,
        "init",
        "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V",
        |ctx, args| {
            // A third party's SPI owns its own init. `SSLContext.init` is
            // `final` and its entire body is
            // `contextSpi.engineInit(km, tm, sr)`; everything below records the
            // managers in CratonVM's side tables for the rustls engine, which
            // such a context will never use. See `jca::ssl_context_spi`.
            if let Some(r) = crate::jca::ssl_context_spi::spi_delegate(
                ctx,
                args,
                "engineInit",
                "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V",
            ) {
                return r;
            }
            let this = obj_arg(args, 0)?;
            if crate::nbflags().dbg_tls_auth_ok {
                eprintln!(
                    "[dbg-tls-auth] re6 SSLContext.init key={}",
                    ctx.identity_hash_code(this)
                );
            }
            // See `getInstance`/`getDefault` above: the raw field write this
            // line used to carry (G30-1's N4, `SSLContext#1 contextSpi`) is
            // gone; `mark_context_initialized` is the only bookkeeping
            // `require_context_initialized` reads.
            crate::jca::ssl_context_spi::mark_context_initialized(ctx, this);
            // Stash the actual KeyManager objects too (may include a test
            // wrapper like Tomcat's `TrackingKeyManager`). rustls's own
            // client-cert path otherwise only ever presents one fixed
            // (cert_pem, key_pem) pair — it never consults a KeyManager's
            // `chooseClientAlias`, so a server-requested-issuer-specific
            // selection (or a wrapper that must observe the call) was
            // silently skipped. Consulted synchronously mid-handshake by
            // `t27_tls::JavaKeyManagerResolver` (`getSocketFactory()` reads
            // this back out via `ctx_key_managers_table`'s key).
            let kms_arr = match args.get(1) {
                Some(Value::Object(Some(a))) => Some(*a),
                _ => None,
            };
            let tms_arr = match args.get(2) {
                Some(Value::Object(Some(a))) => Some(*a),
                _ => None,
            };
            // Install the long-lived manager roots before resolving identity
            // or transferring pending context state.  Those helpers can
            // allocate/re-enter Java; native-call argument pins survive that
            // collection, but the copied ObjectRefs above do not get rewritten
            // afterwards.  Retaining them later could publish a recycled
            // receiver into the TLS manager table.
            //
            // FIX (jdkclienthttprequestfactory-certificaterequired-alert):
            // that ordering alone was NOT enough, because the FIRST helper
            // re-enters Java — `attach_trust_managers_to_ctx` calls
            // `getAcceptedIssuers()` on every manager (and pins the managers,
            // but nothing else). A young collection landing in there leaves
            // `this` and `kms_arr` naming vacated slots. Measured on Windows
            // with `CRATONVM_DBG_GC_STRESS=1048576`, inside ONE `init` call:
            //
            //   attach_trust_managers_to_ctx  key=4754528796672
            //   attach_key_managers_to_ctx    key=40643275522048 count=0
            //
            // Two different side-table keys for the same `SSLContext`, and a
            // `KeyManager[]` whose length read back as 0 — so the key managers
            // and the mTLS identity were filed under a recycled object's key
            // (and, at count=0, `attach_key_managers_to_ctx` REMOVED the real
            // entry) while every later lookup — `ctx_identity`,
            // `ctx_key_managers_table` — used the true key and missed. A
            // client built from such a context presents NO certificate, and a
            // server that demands one answers `CertificateRequired`.
            //
            // Root all three through a handle scope and re-read them before
            // each use; the scope closes on early return and on unwind.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let this_h = scope.root(this);
            let kms_h = kms_arr.map(|a| scope.root(a));
            let tms_h = tms_arr.map(|a| scope.root(a));
            let this_now = scope.get(&this_h);
            let tms_now = tms_h.as_ref().map(|h| scope.get(h));
            crate::t27_tls::attach_trust_managers_to_ctx(&mut *scope, this_now, tms_now)?;
            let this_now = scope.get(&this_h);
            let kms_now = kms_h.as_ref().map(|h| scope.get(h));
            crate::t27_tls::attach_key_managers_to_ctx(&mut *scope, this_now, kms_now)?;
            let ctx = &mut scope;
            let kms_arr = kms_h.as_ref().map(|h| ctx.get(h));
            // Per-SSLContext mTLS identity: prefer resolving it DIRECTLY from
            // the KeyManager[] this call actually received (immune to an
            // intervening, unrelated SSLContext.init draining the
            // thread-local first — see pemcertificates-clientauth-rustls-
            // decrypterror); fall back to the thread-local "staged by the
            // most recent KeyManagerFactory.init on this thread" mechanism
            // when the KeyManager objects don't carry a recognizable id (e.g.
            // a test wrapper). createSSLEngine / createSocket then use THIS
            // context's cert+key rather than the process-global slot, so an
            // in-process server and client don't clobber each other.
            // nio20 wave 4 (L4): `init(null, ..)` / an empty `KeyManager[]` is
            // an EMPTY key manager in SunJSSE, never the identity a preceding
            // `KeyStore.load` staged (`attach_pending_identity_to_ctx`).
            let km_supplied = kms_arr.is_some_and(|a| ctx.array_length(a) > 0);
            let resolved_identity =
                crate::x509_manager::resolved_identity_pem_for_key_manager_array(&mut **ctx, kms_arr);
            // Same fix, trust side (jdk-only/P4A-TOMCAT-20260812-RETIRED-20260921.md defect 4): resolve
            // roots DIRECTLY from the TrustManager[] this call received
            // instead of the thread-local `PENDING_TM_TRUST_ROOTS`, which is
            // only "whichever TrustManagerFactory.init ran most recently on
            // this thread" and gets drained by an unrelated SSLContext.init
            // that happens to run first.
            let tms_arr = tms_h.as_ref().map(|h| ctx.get(h));
            // nio20 wave 5 (L5): `init(.., null, ..)` / an empty
            // `TrustManager[]` is SunJSSE's DEFAULT trust manager, never the
            // roots a preceding `KeyStore.load` staged (`tm_supplied`).
            let tm_supplied = tms_arr.is_some_and(|a| ctx.array_length(a) > 0);
            let resolved_trust_roots =
                crate::x509_manager::resolved_trust_roots_for_trust_manager_array(&mut **ctx, tms_arr);
            // Re-read through the handle rather than reusing the copy from the
            // top of the method: the two attaches above allocate.
            let this = ctx.get(&this_h);
            crate::t27_tls::attach_pending_identity_to_ctx(
                &mut **ctx,
                this,
                resolved_identity,
                resolved_trust_roots,
                km_supplied,
                tm_supplied,
            )?;
            // Stash the actual TrustManager objects passed here (may include a
            // revocation-aware PKIXRevocationChecker attached by
            // Tomcat's SSLUtilBase.getTrustManagers, or a fully custom
            // X509TrustManager). rustls's own verifier only checks the
            // certificate chain against a trust anchor — it never consults
            // these — so without this, custom/OCSP/CRL trust managers are
            // silently never invoked. Consulted post-handshake by
            // `t27_tls::engine_run_trust_check`.

            // args[3] — the caller's `SecureRandom`. This handler used to
            // ignore it completely, which is a divergence from
            // `sun.security.ssl.SSLContextImpl.engineInit` regardless of where
            // the handshake's entropy comes from: that method ends with
            //
            //     secureRandom = Objects.requireNonNullElseGet(sr, SecureRandom::new);
            //     /* The initial delay of seeding the random number generator
            //      * could be long enough to cause the initial handshake on our
            //      * first connection to time out and fail. Make sure it is
            //      * primed and ready by getting some initial output from it. */
            //     secureRandom.nextInt();
            //
            // (verified against this host's JDK 25 `src.zip`). The draw is part
            // of the method's observable behaviour, not an implementation
            // detail — netty's `SslContextBuilderTest.
            // {testServerContextWithSecureRandom,testClientContextWithSecureRandom}`
            // hand in a counting `SecureRandom` subclass and assert it was
            // drawn from; both failed `expected: <true> but was: <false>`.
            //
            // NOTE, deliberately: this primes the caller's generator, it does
            // NOT re-source the TLS handshake from it. CratonVM's engine is
            // rustls-backed and its record/key randomness comes from `ring`'s
            // CSPRNG through the `CryptoProvider`; there is no supported way to
            // drive that from a Java object mid-handshake, and routing it
            // through one would mean a caller's weak or fixed-seed
            // `SecureRandom` silently weakened real key material. See the
            // retirement note for this batch.
            if let Some(Value::Object(Some(sr))) = args.get(3) {
                // Rooted through the same handle scope everything else in this
                // handler uses — `nextInt()` runs Java and can move `sr`, and a
                // raw pin nested inside the scope would have to be unwound in
                // exactly the right order on every exit path.
                let sr_h = ctx.root(*sr);
                let sr_now = ctx.get(&sr_h);
                // A generator that throws propagates, exactly as it does out of
                // the real `engineInit`.
                ctx.invoke_virtual(sr_now, "nextInt", "()I", &[])?;            }
            Ok(None)
        }, NativeKind::SyntheticStub,
    );
    r.register(
        ctx_cls,
        "getSocketFactory",
        "()Ljavax/net/ssl/SSLSocketFactory;",
        |ctx, args| {
            if let Some(r) = crate::jca::ssl_context_spi::spi_delegate(
                ctx,
                args,
                "engineGetSocketFactory",
                "()Ljavax/net/ssl/SSLSocketFactory;",
            ) {
                return r;
            }
            let this = obj_arg(args, 0)?;
            crate::jca::ssl_context_spi::require_context_initialized(ctx, this)?;
            if crate::nbflags().dbg_tls_auth_ok {
                eprintln!(
                    "[dbg-tls-auth] re6 SSLContext.getSocketFactory key={}",
                    ctx.identity_hash_code(this)
                );
            }
            // gc-common w14-a: the context is pinned across the allocation and
            // re-read. The factory's slot 0 used to get the `args` copy, i.e.
            // a vacated address after a moving collection in the allocation,
            // and every socket from that factory then read its TLS material
            // (trust managers, identity) off whatever occupied that address.
            let this_pin = ctx.pin_native_root(this);
            let f = match crate::tls_carriers::alloc_tls_carrier(
                ctx,
                crate::tls_carriers::SSL_SOCKET_FACTORY_PUBLIC,
                crate::tls_carriers::SSL_SOCKET_FACTORY_IMPL,
                1,
            ) {
                Ok(f) => f,
                Err(e) => {
                    ctx.unpin_native_roots(this_pin);
                    return Err(e);
                }
            };
            let this = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            ctx.set_field(f, 0, Value::Object(Some(this)));
            // Obtaining a factory has no connection scope. The HttpsURLConnection
            // setter captures it later, either as an instance-specific config
            // or as the JDK process default; doing that here leaked an
            // instance's permissive TrustManager into later connections.
            Ok(Some(Value::Object(Some(f))))
        }, NativeKind::SyntheticStub,
    );
    r.register(
        ctx_cls,
        "getServerSocketFactory",
        "()Ljavax/net/ssl/SSLServerSocketFactory;",
        |ctx, args| {
            if let Some(r) = crate::jca::ssl_context_spi::spi_delegate(
                ctx,
                args,
                "engineGetServerSocketFactory",
                "()Ljavax/net/ssl/SSLServerSocketFactory;",
            ) {
                return r;
            }
            let this = obj_arg(args, 0)?;
            crate::jca::ssl_context_spi::require_context_initialized(ctx, this)?;
            // gc-common w14-a: pinned across the allocation, as in
            // `getSocketFactory` above.
            let this_pin = ctx.pin_native_root(this);
            let f = match crate::tls_carriers::alloc_tls_carrier(
                ctx,
                crate::tls_carriers::SSL_SERVER_SOCKET_FACTORY_PUBLIC,
                crate::tls_carriers::SSL_SERVER_SOCKET_FACTORY_IMPL,
                1,
            ) {
                Ok(f) => f,
                Err(e) => {
                    ctx.unpin_native_roots(this_pin);
                    return Err(e);
                }
            };
            let this = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            ctx.set_field(f, 0, Value::Object(Some(this)));
            Ok(Some(Value::Object(Some(f))))
        }, NativeKind::SyntheticStub,
    );
    r.register(
        ctx_cls,
        "getProtocol",
        "()Ljava/lang/String;",
        |ctx, args| {
            // A REAL `javax.net.ssl.SSLContext` — one JDK bytecode built around
            // a provider's SPI, ours or a third party's — has `provider` in
            // slot 0 and `protocol` in slot 2 (`javap -p --module java.base
            // javax.net.ssl.SSLContext`, JDK 25). Reading slot 0 on one of
            // those echoed the PROVIDER back as a protocol name:
            // `getInstance("TLSv1.3", new BouncyCastleJsseProvider())` answered
            // "BCJSSE version 1.0023" where HotSpot answers "TLSv1.3". The
            // slot-0 read below is right only for the synthetic shape this
            // registration's own `getInstance` allocates.
            if let Some(v) = crate::jca::ssl_context_spi::real_context_protocol(ctx, args) {
                return Ok(Some(v));
            }
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        }, NativeKind::SyntheticStub,
    );
    // getProvider() — see `jca::ssl_context_spi::ssl_context_provider`. Not
    // registered at all until 2026-08-20, so the real bytecode answered it:
    // `return provider;`, which is slot 0 — the PROTOCOL on every synthetic
    // layout. `getProvider()` handed back a `java.lang.String`.
    r.register(
        ctx_cls,
        "getProvider",
        "()Ljava/security/Provider;",
        |ctx, args| crate::jca::ssl_context_spi::ssl_context_provider(ctx, args), NativeKind::SyntheticStub,
    );
    // getSupportedSSLParameters() — Tomcat's JSSEUtil.initialise() reads the
    // supported protocols + cipher suites here. The synthetic SSLContext has no
    // contextSpi, so the inherited javax bytecode NPEs; return a REAL
    // javax.net.ssl.SSLParameters (plain data holder) with the protocol/cipher
    // lists accepted by the JSSE configuration surface. TLSv1.1 remains
    // disabled by default below, but must be present here so an explicitly
    // requested legacy protocol survives Tomcat's configuration intersection.
    r.register(
        ctx_cls,
        "getSupportedSSLParameters",
        "()Ljavax/net/ssl/SSLParameters;",
        |ctx, _args| {
            if let Some(r) = crate::jca::ssl_context_spi::spi_delegate(
                ctx,
                _args,
                "engineGetSupportedSSLParameters",
                "()Ljavax/net/ssl/SSLParameters;",
            ) {
                return r;
            }
            // Single source of truth — see `t27_tls::SUPPORTED_PROTOCOL_NAMES`.
            let protocols = crate::t27_tls::SUPPORTED_PROTOCOL_NAMES;
            // Single source of truth — see `t27_tls::SUPPORTED_CIPHER_SUITE_NAMES`.
            // Tomcat's `JSSEUtil.initialise()` reads this list and
            // `SSLUtilBase.getEnabled` silently DROPS any configured suite
            // missing from it, so a name absent here can never be enforced
            // by a connector no matter what the handshake code does.
            let ciphers = crate::t27_tls::SUPPORTED_CIPHER_SUITE_NAMES;
            re6_new_ssl_parameters(ctx, ciphers, protocols)
        }, NativeKind::SyntheticStub,
    );
    // getDefaultSSLParameters() — BUG-08: Jetty's `SslContextFactory.load()`
    // (jetty-util) calls this on the SSLContext it just `getInstance`'d to seed
    // the connector's enabled protocols/cipher suites. Like
    // getSupportedSSLParameters above, the synthetic SSLContext carries no real
    // `contextSpi`, so the un-intercepted `javax.net.ssl.SSLContext`
    // .getDefaultSSLParameters() bytecode (`return contextSpi.engineGet…()`)
    // dereferenced null → NPE (5× org.springframework.http.client.
    // JettyClientHttpRequestFactoryTests). Return a REAL SSLParameters whose
    // *default-enabled* protocol/cipher lists match what the rustls-backed
    // engine negotiates — on JDK 25 the modern TLSv1.3/1.2 suites are
    // enabled-by-default, so the default set mirrors the supported set here.
    r.register(
        ctx_cls,
        "getDefaultSSLParameters",
        "()Ljavax/net/ssl/SSLParameters;",
        |ctx, _args| {
            if let Some(r) = crate::jca::ssl_context_spi::spi_delegate(
                ctx,
                _args,
                "engineGetDefaultSSLParameters",
                "()Ljavax/net/ssl/SSLParameters;",
            ) {
                return r;
            }
            let protocols = ["TLSv1.3", "TLSv1.2"];
            // Single source of truth — see `t27_tls::SUPPORTED_CIPHER_SUITE_NAMES`.
            // Tomcat's `JSSEUtil.initialise()` reads this list and
            // `SSLUtilBase.getEnabled` silently DROPS any configured suite
            // missing from it, so a name absent here can never be enforced
            // by a connector no matter what the handshake code does.
            let ciphers = crate::t27_tls::SUPPORTED_CIPHER_SUITE_NAMES;
            re6_new_ssl_parameters(ctx, ciphers, &protocols)
        }, NativeKind::SyntheticStub,
    );
    // createSSLEngine() — return a rustls-backed sun.security.ssl.SSLEngineImpl
    // (its wrap/unwrap/handshake natives live in t27_tls::register_sslengine_real,
    // keyed by ObjectRef via engine_id_or_alloc).  It is intentionally a
    // synthetic allocation, but it still participates in real JDK bytecode:
    // Netty configures ALPN through SSLEngineImpl's
    // setHandshakeApplicationProtocolSelector(), which takes engineLock.  A
    // bare allocation leaves that final constructor field null and turns a
    // normal TLS setup into an NPE.  Supply the one JDK-visible invariant that
    // method needs without running SSLEngineImpl's full JSSE constructor (the
    // rustls-backed native state owns the rest of the engine lifecycle).
    for desc in [
        "()Ljavax/net/ssl/SSLEngine;",
        "(Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;",
    ] {
        r.register(ctx_cls, "createSSLEngine", desc, |ctx, args| {
            // A BouncyCastle (or any third-party) context must hand back the
            // engine ITS provider builds. Allocating CratonVM's rustls-backed
            // `sun.security.ssl.SSLEngineImpl` unconditionally gave such a
            // caller a SunJSSE-named engine BC never built and never
            // initialised — `engine.toString()` alone threw
            // `NullPointerException: Cannot read field "conSession"`.
            // The no-arg and (host, port) overloads share this closure, hence
            // the arity test rather than a per-descriptor guard.
            let spi_desc = if args.len() >= 3 {
                "(Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;"
            } else {
                "()Ljavax/net/ssl/SSLEngine;"
            };
            if let Some(r) = crate::jca::ssl_context_spi::spi_delegate(
                ctx,
                args,
                "engineCreateSSLEngine",
                spi_desc,
            ) {
                return r;
            }
            // `SSLContextImpl.engineCreateSSLEngine` is guarded by
            // `checkInitialized` exactly like the two factory getters.
            //
            // THIS registration is the live one, not the pair in
            // `phases_late::ssl_security` — a `for desc in [..]` loop hides
            // them from the grep that finds every other `"createSSLEngine"`,
            // which is how a trial binary with the gate on the other two
            // still answered `L6TlsParamSweep` row 65 with an engine.
            let sslctx0 = obj_arg(args, 0)?;
            crate::jca::ssl_context_spi::require_context_initialized(ctx, sslctx0)?;
            // gc-common w14-a: read the `(String host, int port)` overload's
            // host NOW, before anything below allocates. `args` is a copy that
            // a moving collection does not rewrite, so the String read after
            // `new ReentrantLock()` was from a possibly vacated slot.
            let peer_host: Option<(String, i32)> = if args.len() >= 3 {
                let host = match args.get(1) {
                    Some(Value::Object(Some(h))) => ctx.read_string(*h),
                    _ => None,
                };
                let port = args.get(2).and_then(|v| v.as_int()).unwrap_or(-1);
                host.map(|h| (h, port))
            } else {
                None
            };
            // gc-common w14-a (handoff-w13a-create-ssl-engine-reads-its-
            // context-after-allocating): the context is pinned together with
            // the engine and re-read through the pin. It used to be re-read
            // from `args` after the two allocations below, so an engine built
            // across a moving collection got the identity, trust roots and
            // `TrustManager` key of whatever the vacated slot held. Pins are
            // released as a stack: unpinning `sslctx_pin` also releases
            // `eng_pin`, taken after it.
            let sslctx_pin = ctx.pin_native_root(sslctx0);
            let eng0 =
                match try_alloc_concurrent_synthetic(ctx, "sun/security/ssl/SSLEngineImpl", 4) {
                    Ok(e) => e,
                    Err(e) => {
                        ctx.unpin_native_roots(sslctx_pin);
                        return Err(e);
                    }
                };
            // Everything below this point allocates (a ReentrantLock, and the
            // peer-host String further down), so `eng` must be pinned and
            // re-read rather than held raw across those calls.
            let eng_pin = ctx.pin_native_root(eng0);
            let lock = match ctx.new_object_initialized(
                "java/util/concurrent/locks/ReentrantLock",
                "()V",
                &[],
            ) {
                Ok(Some(Value::Object(Some(lock)))) => lock,
                Ok(_) => {
                    ctx.unpin_native_roots(sslctx_pin);
                    return Err(npe("ReentrantLock <init> failed"));
                }
                Err(e) => {
                    ctx.unpin_native_roots(sslctx_pin);
                    return Err(e);
                }
            };
            let eng = ctx.read_native_pin(eng_pin, eng0);
            ctx.set_field_by_name(eng, "engineLock", Value::Object(Some(lock)));
            // Copy this SSLContext's per-context identity (its keystore cert+key)
            // onto the engine, so the rustls handshake presents THIS context's
            // cert (server cert, or client cert for mTLS) instead of the global.
            {
                let sslctx = ctx.read_native_pin(sslctx_pin, sslctx0);
                let identity = crate::t27_tls::ctx_identity(ctx, sslctx);
                let eng = ctx.read_native_pin(eng_pin, eng0);
                // A client context commonly has only trust material. Capture
                // its roots before the next context creation can replace the
                // thread-local selection used by the rustls engine.
                crate::t27_tls::set_engine_trust_roots_override(ctx, eng);
                if let Ok(Some((cert, key))) = identity {
                    crate::t27_tls::set_engine_identity_override(ctx, eng, cert, key);
                }
                // Remember which SSLContext created this engine so the
                // post-handshake trust check can find its TrustManager[]
                // (independent of whether a KMF identity was also present).
                let sslctx = ctx.read_native_pin(sslctx_pin, sslctx0);
                let eng = ctx.read_native_pin(eng_pin, eng0);
                if let Err(e) = crate::t27_tls::set_engine_trust_ctx_key(ctx, eng, sslctx) {
                    ctx.unpin_native_roots(sslctx_pin);
                    return Err(e);
                }
            }
            // `createSSLEngine(String host, int port)` — record the host the
            // caller intends to reach. Dropping it silently made this engine
            // announce `localhost` as its SNI whatever host was dialled, and
            // left RFC 2818 endpoint identification with nothing to verify
            // against (see `t27_tls::set_engine_peer_host`). The no-arg
            // overload shares this closure, hence the arity test rather than a
            // per-descriptor body.
            if args.len() >= 3 {
                if let Some((host, port)) = peer_host.filter(|(h, _)| !h.is_empty()) {
                    let eng = ctx.read_native_pin(eng_pin, eng0);
                    crate::t27_tls::set_engine_peer_host(ctx, eng, host, port);
                }
            } else {
                // `javax.net.ssl.SSLEngine` declares `private int peerPort = -1`
                // and the no-arg `SSLEngine()` leaves it there, so HotSpot's
                // `createSSLEngine()` engine reports port -1. A synthetic
                // allocation leaves the slot at its untagged 0, which is a
                // legal-looking port number that no caller asked for and which
                // `SSLEngine.toString()` prints. `peerHost` needs no such seed:
                // its default IS null.
                let eng = ctx.read_native_pin(eng_pin, eng0);
                ctx.set_field_by_name(eng, "peerPort", Value::Int(-1));
            }
            let eng = ctx.read_native_pin(eng_pin, eng0);
            ctx.unpin_native_roots(sslctx_pin);
            Ok(Some(Value::Object(Some(eng))))
        }, NativeKind::SyntheticStub);
    }
    // Netty's JdkSslClientContext reads this immediately after SSLContext.init.
    // Our bridged real-JDK SSLContext has no contextSpi, so the Java method
    // would otherwise dereference null despite the usable native TLS context.
    // The synthetic facade matches the server session-context contract below.
    r.register(
        ctx_cls,
        "getClientSessionContext",
        "()Ljavax/net/ssl/SSLSessionContext;",
        |ctx, args| {
            if let Some(r) = crate::jca::ssl_context_spi::spi_delegate(
                ctx,
                args,
                "engineGetClientSessionContext",
                "()Ljavax/net/ssl/SSLSessionContext;",
            ) {
                return r;
            }
            // One carrier per (SSLContext, side), not one per call — see
            // `ssc_carrier` for the measured identity row this restores. The
            // GC-safety that used to live here moved in there with it.
            let this = obj_arg(args, 0)?;
            let carrier = ssc_carrier(ctx, this, SSC_TAG_CLIENT)?;
            Ok(Some(Value::Object(Some(carrier))))
        }, NativeKind::SyntheticStub,
    );

    // getServerSessionContext() — Tomcat caches it and sets cache size /
    // timeout on it (see the setters below).
    r.register(
        ctx_cls,
        "getServerSessionContext",
        "()Ljavax/net/ssl/SSLSessionContext;",
        |ctx, args| {
            if let Some(r) = crate::jca::ssl_context_spi::spi_delegate(
                ctx,
                args,
                "engineGetServerSessionContext",
                "()Ljavax/net/ssl/SSLSessionContext;",
            ) {
                return r;
            }
            let this = obj_arg(args, 0)?;
            let carrier = ssc_carrier(ctx, this, SSC_TAG_SERVER)?;
            Ok(Some(Value::Object(Some(carrier))))
        }, NativeKind::SyntheticStub,
    );
    // SSLSessionContext cache tuning, as driven by Tomcat's `SSLHostConfig`.
    // rustls owns its own session cache and exposes no resize/expiry hook we
    // can drive from here, so the values genuinely do not reach the TLS stack
    // — that part stays a documented no-op.
    //
    // What is NOT acceptable is the getters contradicting the setters. They
    // used to answer a constant 0 whatever was configured, and 0 has a defined
    // meaning in this API — "unlimited cache" / "sessions never time out" —
    // so a caller that set a bound and read it back was told its bound had
    // been replaced by no bound at all. Tomcat's `SSLHostConfig`/JMX round-trip
    // does exactly that read-back. Store the configured values so the pair is
    // self-consistent. A side table, not instance slots: this carrier is
    // allocated against `javax/net/ssl/SSLSessionContext`, which is an
    // INTERFACE in the real JDK and declares zero fields, so widening the
    // allocation would produce an undersized-layout object whose field
    // accesses the GC bounds guard rejects (see `alloc_concurrent_synthetic`).
    let ssc = "javax/net/ssl/SSLSessionContext";
    r.register(ssc, "setSessionCacheSize", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let size = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if size < 0 {
            // MEASURED: HotSpot throws `IllegalArgumentException` with a NULL
            // message here, not a described one. Empty string is this crate's
            // marker for a null `getMessage()` (see `types/src/error.rs`), and
            // a message that cannot be derived can only be transcribed --
            // here the transcription is "no message at all".
            return Err(iae(String::new()));
        }
        ssc_set(ctx, this, |s| s.cache_size = size);
        Ok(None)
    }, NativeKind::SyntheticStub);
    r.register(ssc, "setSessionTimeout", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let secs = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if secs < 0 {
            // Same null message as `setSessionCacheSize` above, measured on the
            // same run.
            return Err(iae(String::new()));
        }
        ssc_set(ctx, this, |s| s.timeout_secs = secs);
        Ok(None)
    }, NativeKind::SyntheticStub);
    r.register(ssc, "getSessionCacheSize", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(ssc_get(ctx, this).cache_size)))
    }, NativeKind::SyntheticStub);
    r.register(ssc, "getSessionTimeout", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(ssc_get(ctx, this).timeout_secs)))
    }, NativeKind::SyntheticStub);
    // getIds() / getSession(byte[]) — the two LOOKUP methods on this
    // interface. They had no registration at all, and `SSLSessionContext` is a
    // real JDK interface with no bodies, so every call threw
    // `AbstractMethodError: method javax/net/ssl/SSLSessionContext.getIds()
    // Ljava/util/Enumeration; has no Code attribute`. That is 36 of netty's
    // `JdkSslEngineTest` failures on its own (`SSLEngineTest
    // .currentSessionCacheSize`, which every session-resumption test calls
    // before it does anything else).
    //
    // NOM-1 (G7-1 §8). These used to answer honestly-empty — rustls owns the
    // TLS session cache and exposes no enumeration of it — which was correct
    // for every state this VM could observe UNTIL `getSessionContext()`
    // itself started handing out a real, populated carrier for a completed
    // handshake (G7-1 §2). From that point an always-empty `getIds()` was a
    // second door contradicting the first: "the context a session points at
    // does not contain that session". `t27_tls::session_id_registry` is the
    // side table that closes it — keyed by the same 32 bytes `getId()`
    // reports, populated at the same handshake-completed epoch
    // `getSessionContext()` itself gates on, and erased at `invalidate()`.
    //
    // It is still not the real JDK's cache: no eviction at `SscSide`'s
    // `cache_size`/`timeout_secs`, and no partition by which `SSLContext`
    // (client/server) negotiated which session — a session appears in every
    // context's `getIds()`. That is an honest simplification, not a silent
    // one: this VM has exactly one TLS session set (per VM since gc-common
    // w9-b, which made `getIds()` stop listing other VMs' sessions), not one per
    // `SSLContext`, and the alternative (an accurate per-context cache) needs
    // the cache-tuning setters above to mean something first.
    r.register(ssc, "getIds", "()Ljava/util/Enumeration;", |ctx, _args| {
        let ids = crate::t27_tls::session_id_registry_ids(ctx.vm_identity());
        let arr = ctx.new_array(ArrayElementType::Reference, ids.len());
        for (i, id) in ids.iter().enumerate() {
            let id_arr = new_java_byte_array(ctx, id);
            ctx.set_array_element(arr, i, Value::Object(Some(id_arr)));
        }
        let en = crate::classloader::make_snapshot_enumeration(ctx, arr)?;
        Ok(Some(Value::Object(Some(en))))
    }, NativeKind::SyntheticStub);
    r.register(
        ssc,
        "getSession",
        "([B)Ljavax/net/ssl/SSLSession;",
        |ctx, args| {
            // MEASURED (probe `G16Ctx`, HotSpot 25.0.3+9-LTS):
            // `getSession(null)` throws `NullPointerException("session id
            // cannot be null")`. It is the ONLY row of this family where a
            // null argument is not just "nothing cached under that id": the
            // empty array and an unknown id both answer null, and both already
            // agreed. Returning null for a null id told a caller its lookup
            // had MISSED when the JDK would have refused the call.
            let id_arr = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => return Err(npe("session id cannot be null")),
            };
            let id = re5_read_byte_array(ctx, id_arr);
            // NOM-1 (G7-1 §8). `getIds()`'s side above, from the other end:
            // an id this VM actually cached answers the SAME session object
            // (MEASURED, §1b), everything else — an unknown id, `byte[0]` —
            // answers `null` exactly as the always-null lookup used to,
            // honestly, for those two cases.
            Ok(Some(Value::Object(crate::t27_tls::session_by_id(
                ctx.vm_identity(),
                &id,
            ))))
        }, NativeKind::SyntheticStub,
    );

    let sf = "javax/net/ssl/SSLSocketFactory";
    r.register(
        sf,
        "createSocket",
        "(Ljava/lang/String;I)Ljava/net/Socket;",
        |ctx, args| {
            let this_factory = obj_arg(args, 0)?;
            let host_val = args.get(1).copied().unwrap_or(Value::Object(None));
            let host = value_or_string(ctx, host_val, "");
            let port = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
            if host.is_empty() || !(1..=65535).contains(&port) {
                return Err(iae(format!("bad host/port: {host}:{port}")));
            }
            // FIX (tls-handshake-enforcement-gap, doc 21): probe mode — hand
            // back an UNCONNECTED carrier so the calling factory's own Java
            // code can configure it (see `set_huc_factory_probe_mode`). No
            // TCP connect and no handshake happen here, so this stays a
            // cheap, purely in-VM call even though it runs re-entrantly from
            // inside another native.
            if huc_factory_probe_mode() {
                // gcd d1/e: plus the handshake-listener slot
                // (`ssl_security::alloc_ssl_socket`).
                let sock = crate::phases_late::ssl_security::alloc_ssl_socket(ctx, 5)?;
                let pin_base = ctx.pin_native_root(sock);
                let sock = ctx.read_native_pin(pin_base, sock);
                // gc-common w16-a: owned (re-read through its pin just above).
                sock_set_owned(ctx, sock, |s| {
                    s.host = host.clone();
                    s.port = port;
                    s.local_port = 0;
                    s.closed = 0;
                    s.stream_id = -1;
                });
                {
                    // Minted before the guard (`sock` is current: re-read
                    // through its pin, nothing allocated since).
                    let key = mint_native_obj_key(ctx, sock);
                    let mut table = probe_restrictions().lock();
                    // A factory whose `createSocket` returns some OTHER
                    // socket than the one we handed it leaves its entry
                    // behind, so bound the table rather than trust every
                    // entry to be claimed by `take_probe_restrictions`.
                    // (Since gc-common w17-a the owner-death sweep also drops
                    // the entry of a probe socket that died unclaimed.)
                    //
                    // gc-common w17-a: bound THIS VM's entries only. The
                    // bound used to `clear()` the whole process-wide table,
                    // so one VM's overflow erased another VM's in-flight probe
                    // (and with it the cipher / protocol restrictions its
                    // factory had just applied, which its connection then
                    // silently ignored).
                    if table.keys().filter(|k| k.vm == key.vm).count() >= 64 {
                        table.retain(|k, _| k.vm != key.vm);
                    }
                    table.insert(key, (Vec::new(), Vec::new()));
                }
                let sock = ctx.read_native_pin(pin_base, sock);
                ctx.unpin_native_roots(pin_base);
                return Ok(Some(Value::Object(Some(sock))));
            }
            // Per-context client identity (mTLS): the SSLContext stashed on the
            // factory by getSocketFactory (field 0) may carry a client cert+key.
            //
            // FIX (client-cipher-restriction): field 0 is only the owning
            // SSLContext for the bare placeholder `getSocketFactory()`
            // itself returns. When this `createSocket` is reached via an
            // up-call on a real, user-defined `SSLSocketFactory` SUBCLASS
            // (e.g. Tomcat's `TesterSupport.ClientSSLSocketFactory`, wrapping
            // the placeholder — see `http_url_connection
            // ::huc_upcall_create_socket_if_custom_factory`), field 0 is
            // whatever THAT class declares first (for `ClientSSLSocketFactory`,
            // its own `delegate` field, a *different* SSLSocketFactory
            // object) — `ctx_identity` then looks up identity keyed by the
            // wrong object's identity and always misses, so the up-called
            // path silently dropped client-cert presentation entirely. Fall
            // back to this VM's default `huc_default_client_identity_in_vm` that
            // `http_url_connection::perform`'s own (non-up-called) connect
            // already relies on — populated by the same `getSocketFactory()`
            // call, just read through a path that isn't sensitive to which
            // object ends up at field 0.
            let configured_ident = match ctx.get_field(this_factory, 0) {
                Value::Object(Some(sslctx)) => crate::t27_tls::ctx_identity(ctx, sslctx)?,
                _ => None,
            };
            let client_ident = match configured_ident {
                Some(identity) => Some(identity),
                None => crate::t27_tls::huc_default_client_identity_in_vm(ctx.vm_identity()),
            };
            #[cfg(unix)]
            let legacy_dsa_roots = crate::t27_tls::selected_context_trust_root_ders();
            #[cfg(unix)]
            let legacy_dsa_client = client_ident
                .as_ref()
                .is_some_and(|(_, key_pem)| crate::t27_tls::is_dsa_private_key_pem(key_pem))
                || legacy_dsa_roots
                    .iter()
                    .any(|der| crate::t27_tls::is_dsa_certificate_der(der));
            // Use the rustls client path rather than a default native-tls
            // connector: (1) trust the gathered test/truststore roots (the
            // native-tls default trusts only the OS root store, so it cannot
            // validate a test CA — which broke every loopback HTTPS client),
            // and (2) present the client certificate for mTLS when present.
            // `km_ctx_key: None` here — this `SSLSocketFactory.createSocket`
            // path (`rustls_client_connect`'s own handshake loop) doesn't yet
            // publish an active native context around its handshake, so it
            // keeps presenting the fixed pre-captured identity rather than
            // consulting a live KeyManager. Only `http_url_connection::
            // perform` (the path `HttpsURLConnection`/`TestClientCert` uses)
            // does the latter today — see
            // tls-ocsp-clientcert-validation-not-enforced-FIXED.md.
            // FIX (TestSsl.testClientInitiatedRenegotiation[JSSE]): honour a
            // version-pinned `SSLContext.getInstance(...)` on THIS path.
            // `phases_late::ssl_security` registers the same
            // (SSLSocketFactory, createSocket, (String,I)) triple and applies
            // the ceiling to its native-tls connector, but this registration
            // runs later and therefore wins in the real-JDK build — so a
            // socket from a `TLSv1.2` context still negotiated TLS 1.3, and
            // the pin appeared to be honoured nowhere. Feeding
            // `enabled_protocols` into the rustls client config is the
            // equivalent knob here; an empty slice keeps rustls's defaults,
            // which is the pre-existing behaviour for an unpinned context.
            let pinned_protocols: Vec<String> =
                crate::phases_late::ssl_security::p68_factory_pinned_protocol_name(ctx, args)
                    .into_iter()
                    .collect();
            let vm = ctx.vm_identity();
            let cfg = crate::t27_tls::build_socket_client_config_with_identity_ciphers_in_vm(
                vm,
                &["http/1.1"],
                client_ident.as_ref().map(|(c, k)| (c.as_str(), k.as_str())),
                None,
                None,
                &[],
                &pinned_protocols,
            )
            .map_err(|e| ioex(format!("client TLS config: {e}")))?;
            // T19.H1: TCP connect + full TLS handshake blocks for real, and this
            // native can now be reached via an up-call from
            // `http_url_connection::huc_upcall_create_socket_if_custom_factory`
            // that runs BEFORE the caller's own blocking region (it needs `ctx`,
            // which a blocking region must not hold) — without announcing our
            // own blocking region here, a concurrent stop-the-world GC would wait
            // forever for this thread to reach a safepoint it can't reach until
            // the (now-deadlocked-behind-the-GC) network call returns.
            ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
            #[cfg(unix)]
            let connect_result = if legacy_dsa_client {
                crate::servlet::s2_legacy_dsa_tls_connect(&host, port as u16, &legacy_dsa_roots)
                    .map_err(|e| e.to_string())
            } else {
                crate::t27_tls::rustls_client_connect(cfg, &host, port as u16)
                    .map(|rid| crate::servlet::RUSTLS_SOCK_ID_BASE + rid)
                    .map_err(|e| e.to_string())
            };
            #[cfg(not(unix))]
            let connect_result = crate::t27_tls::rustls_client_connect(cfg, &host, port as u16)
                .map(|rid| crate::servlet::RUSTLS_SOCK_ID_BASE + rid)
                .map_err(|e| e.to_string());
            ctx.end_blocking_region();
            let id = connect_result.map_err(|e| ioex(format!("TLS connect: {e}")))?;
            // gc-common w14-a: name this connection's VM so that VM's teardown
            // closes it. Safe on both arms: `s2_claim_for_vm` files a
            // legacy-DSA `servlet` row, and decides itself what a rustls id
            // (>= RUSTLS_SOCK_ID_BASE) means.
            crate::servlet::s2_claim_for_vm(ctx.vm_identity(), id);
            // gcd d1/e: plus the handshake-listener slot
            // (`ssl_security::alloc_ssl_socket`).
            let sock = crate::phases_late::ssl_security::alloc_ssl_socket(ctx, 5)?;
            let pin_base = ctx.pin_native_root(sock);
            let sock = ctx.read_native_pin(pin_base, sock);
            // gc-common w16-a: owned (re-read through its pin just above).
            // The row goes with the socket; the TLS stream is not this row's
            // to close (`owns_stream` stays false).
            let local_port = re1_with_raw_stream(id, |tcp| tcp.local_addr().map(|a| i32::from(a.port())).unwrap_or(0)).unwrap_or(0);
            sock_set_owned(ctx, sock, |s| {
                s.host = host.clone();
                s.port = port;
                s.local_port = local_port;
                s.enabled_protocols = Some(if pinned_protocols.is_empty() { vec!["TLSv1.3".into(), "TLSv1.2".into()] } else { pinned_protocols.clone() });
                s.closed = 0;
                s.stream_id = id;
            });
            let sock = ctx.read_native_pin(pin_base, sock);
            ctx.unpin_native_roots(pin_base);
            if crate::nbflags().dbg_tls_sock {
                eprintln!(
                    "[dbg-tls-sock] thread={:?} net_phase_e createSocket(String,int) built sock={:?} stream_id={}",
                    std::thread::current().id(),
                    sock,
                    id
                );
            }
            Ok(Some(Value::Object(Some(sock))))
        }, NativeKind::SyntheticStub,
    );
    // FIX (client-cipher-restriction): a socket from `createSocket` above
    // already completed its handshake unrestricted. The JDK contract
    // callers rely on (e.g. Tomcat's `TesterSupport.ClientSSLSocketFactory`,
    // which calls this immediately after `createSocket` returns, before any
    // I/O) is that restricting to a suite the server doesn't support makes
    // the connection fail — so the only way to honor it is to tear down and
    // reconnect under the restriction.
    //
    // Only do this when at least one requested suite maps to a real rustls
    // `CipherSuite` (`any_cipher_mappable`) — classic TLS 1.2 `TLS_DHE_RSA_*`
    // names never do, because rustls has no finite-field DHE support in any
    // crypto provider (a real upstream limitation, not a gap in this
    // mapping). For an unmappable list, leave the existing (unrestricted)
    // connection alone rather than reconnect into a config that would
    // silently fall back to unrestricted anyway (see `cipher_provider_for`'s
    // own empty-`wanted` fallback) — that would tear down a working
    // connection for no enforcement benefit. Known, currently unclosable gap
    // for TLS 1.2 DHE-suite restriction specifically; see
    // tls-ocsp-clientcert-validation-not-enforced-FIXED.md.
    r.register(
        "javax/net/ssl/SSLSocket",
        "setEnabledCipherSuites",
        "([Ljava/lang/String;)V",
        ssl_socket_set_enabled_cipher_suites, NativeKind::SyntheticStub,
    );
    // FIX (tls-handshake-enforcement-gap, doc 21): `SSLSocket
    // .setEnabledProtocols` was registered only in `phases_late/
    // ssl_security.rs` as an unconditional no-op, so a caller narrowing a
    // socket to one TLS version (Tomcat's `TesterSupport
    // .ClientSSLSocketFactory.setProtocols`, used by
    // `TestSSLHostConfigProtocol`) changed nothing at all. Registered HERE
    // (net_phase_e runs after phases_late, last-writer-wins) so the probe
    // socket records it; a socket that already handshaked keeps the old
    // accept-and-discard behaviour, since its protocol version is settled.
    r.register(
        "javax/net/ssl/SSLSocket",
        "setEnabledProtocols",
        "([Ljava/lang/String;)V",
        ssl_socket_set_enabled_protocols, NativeKind::SyntheticStub,
    );
    // `SSLSocketFactory.getDefault()` is deliberately NOT registered here.
    //
    // REGRESSION 2026-08-04: this spot carried a second registration of the
    // exact triple (`javax/net/ssl/SSLSocketFactory`, `getDefault`,
    // `()Ljavax/net/SocketFactory;`) that `phases_late::ssl_security`'s
    // `register_p68_ssl` already owns. `register()` is documented
    // last-registration-wins on the exact triple (see
    // `NativeMethodRegistry::register`), and `register_p68_ssl` runs FIRST in
    // `register_essential_natives_with_shims` — so this later, stale copy,
    // which set field 0 to `Value::Object(None)`, silently overwrote the
    // fixed one and every caller got a factory with no owning `SSLContext`.
    // The layered `createSocket(Socket,String,int,boolean)` overload then
    // threw `IllegalStateException: SSLSocketFactory has no owning
    // SSLContext` before any network I/O, which is how a fix that "looked
    // present and correct" in `ssl_security.rs` had no runtime effect: it
    // took out every Spring Boot test going through
    // `ModifiedClassPathClassLoader` (Aether/Apache HttpClient resolving
    // `@ClassPathOverrides` coordinates against Maven Central over HTTPS).
    //
    // The sibling `SSLContext.getDefault()` duplicate in this same function
    // IS intentional and documented (see `register_re6_ssl_context`) — that
    // one deliberately relies on the ordering to win. This one never did; it
    // was simply never updated when the 2026-07-23 fix landed. Same bug shape
    // as the `TimeZone.getDefault()` duplicate removed 2026-08-03 (see
    // `native-builtins/src/lib.rs`). Guarded by
    // `native-builtins/tests/registry_contracts.rs::
    // ssl_default_factory_and_context_have_the_documented_single_owner`.
    // See `sslsocketfactory-getdefault-aether-resolution-regression-20260804-FIXED.md`.
}

// ===========================================================================
// RE.6b - the `java.net.ServerSocket` surface `javax.net.ssl.SSLServerSocket`
//         INHERITS
// ===========================================================================
//
// MEASURED 2026-08-17 on HotSpot 25.0.3+9-LTS vs the `--real-jdk` binary from
// `d87dff06a`+2 (probe `G16Sweep`, record
// `G16-1-the-server-socket-impl-and-how-far-RSslLiveSession-got-20260817.md`).
//
// `t27_tls::create_ssl_server_socket` hands back an instance of the REAL
// `javax.net.ssl.SSLServerSocket`, then stores its listener id / local port /
// closed flag in object slots 0/1/2. Those are not free slots: `javap -p
// java.net.ServerSocket` (JDK 25) declares
//
//     0 impl (SocketImpl)   1 created (boolean)   2 bound (boolean)
//     3 closed (boolean)    4 socketLock (Object) 5 options (Set)
//
// so slot 0 is the reference-typed `impl`. Writing a `Value::Int` there is
// DROPPED by the field-layout guard (the same drop `SockSide`/`SsSide` were
// introduced for), `impl` stays null, and every inherited method whose real
// bytecode reads `getImpl()` throws
//
//     NullPointerException: Cannot invoke "java.net.SocketImpl.setOption(int,
//     Object)" because the return value of "java.net.ServerSocket.getImpl()"
//     is null
//
// which is where `RSslLiveSession` died on its FIRST statement,
// `ss.setSoTimeout(SO_TIMEOUT_MS)`, before any TLS work.
//
// The repair here does not transcribe the JDK's answers - it borrows them.
// Each of these nine methods is forwarded to a real, **unbound**
// `java.net.ServerSocket` created on first use and remembered per receiver.
// That is not an approximation: the sweep measured all four lifecycle points
// on both VMs and every option row of a BOUND `sun.security.ssl.
// SSLServerSocketImpl` is byte-identical to the same row on an UNBOUND plain
// `ServerSocket` - value, exception class and message alike, including
// `SO_REUSEPORT` -> `UnsupportedOperationException("'SO_REUSEPORT' not
// supported")`, `setSoTimeout(-1)` -> `IllegalArgumentException("timeout <
// 0")`, `setReceiveBufferSize(0)` -> `IllegalArgumentException("negative
// receive size")` and `getOption(null)` -> a message-less NPE. The two
// argument-check orderings that differ between siblings (`setSoTimeout` tests
// closed-ness FIRST, `setReceiveBufferSize` tests the argument first) come
// out right for free, because they are the real bytecode's orderings.
//
// Deliberately NOT registered here, and nominated to `t27_tls.rs` instead:
// `isBound`, `getInetAddress`, `getLocalSocketAddress` and `toString`. They
// need the bind ADDRESS, which only `create_ssl_server_socket` sees, and they
// have to move together: `ServerSocket.toString()` returns the constant
// `"ServerSocket[unbound]"` while `isBound()` is false, so making `isBound()`
// answer the oracle's `true` on its own converts a row that currently AGREES
// with HotSpot into an NPE on `impl.getInetAddress()`.

/// The real, unbound `java.net.ServerSocket` standing behind one
/// `javax.net.ssl.SSLServerSocket`'s inherited option surface.
///
/// Held as a global-root HANDLE (`NativeContext::add_global_root`), never a
/// bare `ObjectRef`: the collector moves objects and a stale reference
/// resolves to whatever now occupies the slot. Same rule as `lang_class`'s
/// package cache and `jca::provider_chain`'s provider table.
#[derive(Clone, Copy, Debug)]
pub struct SssOptionDelegate {
    /// `add_global_root` handle of the delegate `java.net.ServerSocket`.
    pub root: usize,
    /// Whether the SSL socket's `close()` has already been mirrored onto the
    /// delegate. One-way — a `ServerSocket` never reopens.
    pub closed: bool,
    /// The `SSLServerSocket`'s current address (gc-common w16-a), its weak
    /// owner: [`gc_sweep_net_socket_rows`] drops the row of a socket dropped
    /// without `close()` and queues the root for release.
    pub owner: usize,
}

/// Receiver key -> its delegate. Only a `usize` handle and a `bool` are
/// stored, so the table needs no GC roots of its own and survives object
/// relocation (the receiver's `NativeObjKey` -- its weak lock key since
/// gc-common w28-a -- is stable across a move).
/// LOCK LEVEL (lock-discipline ratchet): `Scratch`. Each of the three
/// acquisitions is one statement — a `get().copied()` or an `insert()` of a
/// `Copy` row — and the delegate construction (`new_object_initialized`,
/// `add_global_root`) happens between them rather than under them.
pub fn sss_option_delegates(
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<HashMap<NativeObjKey, SssOptionDelegate>> {
    static T: OnceLock<
        cratonvm_types::lock_order::OrderedPlMutex<HashMap<NativeObjKey, SssOptionDelegate>>,
    > = OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// Resolve — creating on first use — the delegate handle for `this`, and bring
/// its closed-ness up to date with the SSL socket's.
///
/// `isClosed()` is asked of the SSL socket itself rather than read out of any
/// field: `t27_tls` owns that native, its side table is the authority, and the
/// object slot it also writes (`SSS_CLOSED = 2`) is really `ServerSocket.bound`
/// (see the layout above), so the field says the opposite thing.
///
/// gc-common w15-a: `this` is pinned for the whole call. Creating the
/// delegate runs a Java constructor, and `isClosed()` used to be asked of the
/// address `this` had BEFORE that allocation, i.e. possibly of whatever a
/// moving collection left in the vacated slot (the caller's pin kept the
/// object alive, but the value it passed in is not rewritten).
///
/// Answers the delegate's root and whether the CALLER must release that root
/// once it has resolved the delegate (gc-common w16-a): a closed delegate is
/// no longer remembered. A `ServerSocket` never reopens, and every inherited
/// call on a closed one is answered just as well by a fresh delegate that the
/// sync below closes at once, so keeping the closed one rooted until VM
/// teardown bought nothing and cost one rooted `java.net.ServerSocket` per
/// `SSLServerSocket` ever configured.
pub fn sss_option_delegate_root(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<(usize, bool), MethodCallFailed> {
    // gc-common w16-a: release the roots the last sweep queued for this VM.
    // Allocates nothing, so `this` stays current.
    net_release_deferred(ctx);
    let this_pin = ctx.pin_native_root(this);
    let result = sss_option_delegate_root_pinned(ctx, this_pin, this);
    ctx.unpin_native_roots(this_pin);
    result
}

/// The body of [`sss_option_delegate_root`]; `this` is re-read through
/// `this_pin` after anything that can allocate.
pub fn sss_option_delegate_root_pinned(
    ctx: &mut dyn NativeContext,
    this_pin: usize,
    this: ObjectRef,
) -> Result<(usize, bool), MethodCallFailed> {
    // Minted (gc-common w28-a): the miss path files a row under it after the
    // constructor below runs Java, and a lock key follows `this` across a move.
    // `this` is current here (the wrapper allocated nothing before pinning it).
    let key = mint_native_obj_key(&*ctx, this);
    let known = sss_option_delegates().lock().get(&key).copied();

    let mut entry = match known {
        Some(e) if ctx.resolve_global_root(e.root).is_some() => e,
        _ => {
            let created = match ctx.new_object_initialized("java/net/ServerSocket", "()V", &[])? {
                Some(Value::Object(Some(obj))) => obj,
                _ => {
                    return Err(ioex(
                        "SSLServerSocket: could not create the delegate java.net.ServerSocket",
                    ))
                }
            };
            let root = ctx.add_global_root(created);
            if root == 0 {
                return Err(ioex(
                    "SSLServerSocket: no global root for the delegate java.net.ServerSocket",
                ));
            }
            // A FRESH delegate is open, whatever the previous one was, so the
            // flag starts false and the sync below re-derives it from the SSL
            // socket and closes this one if it has to. Inheriting the old
            // `closed: true` would skip that sync and leave an OPEN delegate
            // answering for a CLOSED socket -- the one state where every row of
            // this surface is supposed to throw.
            //
            // gc-common w16-a: the owner address is read through the pin,
            // AFTER the constructor above ran Java.
            let fresh = SssOptionDelegate {
                root,
                closed: false,
                owner: ctx.read_native_pin(this_pin, this).as_ptr() as usize,
            };
            sss_option_delegates().lock().insert(key, fresh);
            fresh
        }
    };

    if !entry.closed {
        let this = ctx.read_native_pin(this_pin, this);
        let closed = ctx
            .invoke_virtual(this, "isClosed", "()Z", &[])?
            .and_then(|v| v.as_int())
            .unwrap_or(0)
            != 0;
        if closed {
            let delegate = ctx.resolve_global_root(entry.root).ok_or_else(|| {
                ioex("SSLServerSocket: delegate java.net.ServerSocket was collected")
            })?;
            ctx.invoke_virtual(delegate, "close", "()V", &[])?;
            entry.closed = true;
        }
    }
    if entry.closed {
        // gc-common w16-a: forget the closed delegate (see the wrapper's doc);
        // the caller releases its root once it holds the object. Only this
        // row: a concurrent call may have filed a newer one.
        let mut table = sss_option_delegates().lock();
        if table.get(&key).is_some_and(|e| e.root == entry.root) {
            table.remove(&key);
        }
        return Ok((entry.root, true));
    }
    Ok((entry.root, false))
}

/// Forward one inherited call to the delegate, unchanged.
///
/// Every reference argument is pinned across the delegate lookup: creating the
/// delegate runs a Java constructor and `isClosed()` runs a registered native,
/// and either can move objects under the collector.
pub fn sss_option_call(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    name: &str,
    descriptor: &str,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let pin_base = ctx.pin_native_root(this);
    let mut pinned: Vec<(usize, ObjectRef)> = Vec::new();
    for value in args.iter().skip(1) {
        if let Value::Object(Some(obj)) = value {
            pinned.push((ctx.pin_native_root(*obj), *obj));
        }
    }
    let receiver = ctx.read_native_pin(pin_base, this);
    let (root, release_root) = match sss_option_delegate_root(ctx, receiver) {
        Ok(answer) => answer,
        Err(error) => {
            ctx.unpin_native_roots(pin_base);
            return Err(error);
        }
    };
    let mut rest: Vec<Value> = Vec::with_capacity(args.len().saturating_sub(1));
    let mut next = 0usize;
    for value in args.iter().skip(1) {
        match value {
            Value::Object(Some(_)) => {
                let (handle, fallback) = pinned[next];
                next += 1;
                rest.push(Value::Object(Some(ctx.read_native_pin(handle, fallback))));
            }
            other => rest.push(*other),
        }
    }
    let delegate = ctx.resolve_global_root(root);
    if release_root {
        // gc-common w16-a: a closed delegate is no longer remembered. Nothing
        // between here and the call allocates, and the call roots its
        // receiver.
        ctx.remove_global_root(root);
    }
    ctx.unpin_native_roots(pin_base);
    let delegate = delegate
        .ok_or_else(|| ioex("SSLServerSocket: delegate java.net.ServerSocket was collected"))?;
    ctx.invoke_virtual(delegate, name, descriptor, &rest)
}

/// Descriptor of `ServerSocket.setOption`, spelled once so the registration and
/// the forwarded call cannot drift apart.
pub const SSS_SET_OPTION_DESC: &str =
    "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/net/ServerSocket;";

/// Register the nine inherited `java.net.ServerSocket` methods that a
/// `javax.net.ssl.SSLServerSocket` receiver cannot answer for itself.
///
/// Every name here is disjoint from the set `t27_tls::register_sslserversocket`
/// claims on the same class (`accept`, `bind`, `close`, `isClosed`,
/// `getLocalPort`, and the protocol/client-auth pairs). That matters: this
/// registrar runs from `register_phase_e_networking` (lib.rs:18688) and
/// `register_sslserversocket` runs later, from `register_t27_natives`
/// (lib.rs:18731), so a name in both files would be silently taken over by
/// t27's body — the shadowing shape that has already cost this codebase a
/// working fix more than once. `ssl_server_socket_option_registrar_*` below
/// fails if a t27-owned name is ever added here.
pub fn register_ssl_server_socket_options(r: &mut NativeMethodRegistry) {
    let sss = "javax/net/ssl/SSLServerSocket";

    r.register(sss, "getSoTimeout", "()I", |ctx, args| {
        sss_option_call(ctx, args, "getSoTimeout", "()I")
    }, NativeKind::SyntheticStub);
    r.register(sss, "setSoTimeout", "(I)V", |ctx, args| {
        sss_option_call(ctx, args, "setSoTimeout", "(I)V")
    }, NativeKind::SyntheticStub);
    r.register(sss, "getReuseAddress", "()Z", |ctx, args| {
        sss_option_call(ctx, args, "getReuseAddress", "()Z")
    }, NativeKind::SyntheticStub);
    r.register(sss, "setReuseAddress", "(Z)V", |ctx, args| {
        sss_option_call(ctx, args, "setReuseAddress", "(Z)V")
    }, NativeKind::SyntheticStub);
    r.register(sss, "getReceiveBufferSize", "()I", |ctx, args| {
        sss_option_call(ctx, args, "getReceiveBufferSize", "()I")
    }, NativeKind::SyntheticStub);
    r.register(sss, "setReceiveBufferSize", "(I)V", |ctx, args| {
        sss_option_call(ctx, args, "setReceiveBufferSize", "(I)V")
    }, NativeKind::SyntheticStub);
    r.register(sss, "supportedOptions", "()Ljava/util/Set;", |ctx, args| {
        sss_option_call(ctx, args, "supportedOptions", "()Ljava/util/Set;")
    }, NativeKind::SyntheticStub);
    r.register(
        sss,
        "getOption",
        "(Ljava/net/SocketOption;)Ljava/lang/Object;",
        |ctx, args| {
            sss_option_call(
                ctx,
                args,
                "getOption",
                "(Ljava/net/SocketOption;)Ljava/lang/Object;",
            )
        }, NativeKind::SyntheticStub,
    );
    // `setOption` returns the socket for chaining, and it must be the SSL
    // socket the caller passed in — handing back the delegate would let
    // `ss.setOption(..).accept()` accept on the plain listener instead.
    r.register(sss, "setOption", SSS_SET_OPTION_DESC, |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pin = ctx.pin_native_root(this);
        let result = sss_option_call(ctx, args, "setOption", SSS_SET_OPTION_DESC);
        let this = ctx.read_native_pin(pin, this);
        ctx.unpin_native_roots(pin);
        result?;
        Ok(Some(Value::Object(Some(this))))
    }, NativeKind::SyntheticStub);
}

// ===========================================================================
// RE.7 — java.net.DatagramSocket
// ===========================================================================

// DatagramSocket state now lives in the `ds_side_table()` (see `DsSide`),
// keyed by ObjectRef — the old DS_PORT/DS_CLOSED/DS_TIMEOUT/DS_FD object-slot
// layout collided with the real-JDK single-field `DatagramSocket`.

// `java.net.DatagramPacket` slots, SYNTHETIC-JDK layout only — the five
// fabricated `_f0.._f4` slots that `classloading`'s
// `synthetic_stub_fields("java/net/DatagramPacket") => instance_fields(5)`
// allocates and that `phases_late::net_channels::register_p72_datagram` reads
// and writes.
//
// They are NOT the real-JDK layout, and this registrar runs in BOTH builds
// (`register_re7_datagram_socket` is called from `register_phase_e_networking`,
// which is not feature-gated), so every use below has to go through
// [`dp_layout`] rather than reach for these directly. Reaching for them
// directly is exactly what `send`/`receive` used to do, and against a real
// `java.net.DatagramPacket` — `buf, offset, length, bufLength, address, port`,
// in that declared order — it read `length` as the address (an `int` slot, so
// never an object: empty host) and `bufLength` as the port. For
// `new DatagramPacket(payload, 8, lo, ephemeralPort)` that is the message
// `DatagramPacket: bad addr :8`, with the `8` being `payload.length` echoed
// through `bufLength`, and RJdkNet died on it at `loopbackUdp`'s first `send`.
pub const DP_DATA: usize = 0;
pub const DP_LENGTH: usize = 1;
pub const DP_ADDR: usize = 2;
pub const DP_PORT: usize = 3;

/// Where one `java.net.DatagramPacket`'s fields actually live.
///
/// `offset`/`buf_length` are `Option` because the synthetic layout's slot 4 is
/// written by only one of three constructors and the other two leave it
/// uninitialised — see [`dp_layout`] for why the synthetic arm reports `None`
/// for both.
pub struct DpLayout {
    pub buf: usize,
    pub offset: Option<usize>,
    pub length: usize,
    pub buf_length: Option<usize>,
    pub address: usize,
    pub port: usize,
}

/// Resolve [`DpLayout`] for `pkt` by FIELD NAME, falling back to the synthetic
/// `DP_*` slots.
///
/// The name lookup is the discriminator between the two builds and needs no
/// mode flag: a real `java.net.DatagramPacket` declares `buf`/`length`/
/// `address`/`port`, while the fabricated stub's slots are named `_f0.._f4`
/// (`class_manager::synthetic_stub_fields`'s `instance_fields` helper), so the
/// lookup misses and the fallback arm — today's behaviour, unchanged — applies.
///
/// The synthetic arm deliberately reports `offset: None` / `buf_length: None`
/// even though slot 4 is the synthetic offset: `send`/`receive` have always
/// treated that layout as offset-0 whole-buffer, and widening them here would
/// change synthetic-jdk behaviour on a change whose whole purpose is the
/// real-JDK layout. Slot 4 stays `register_p72_datagram`'s business.
pub fn dp_layout(ctx: &dyn NativeContext, pkt: ObjectRef) -> DpLayout {
    let cid = ctx.class_id_of_object(pkt);
    let named = |name: &str| ctx.resolve_field_index_by_class_id(cid, name);
    match (
        named("buf"),
        named("length"),
        named("address"),
        named("port"),
    ) {
        (Some(buf), Some(length), Some(address), Some(port)) => DpLayout {
            buf,
            offset: named("offset"),
            length,
            buf_length: named("bufLength"),
            address,
            port,
        },
        _ => DpLayout {
            buf: DP_DATA,
            offset: None,
            length: DP_LENGTH,
            buf_length: None,
            address: DP_ADDR,
            port: DP_PORT,
        },
    }
}

/// Split the `ip:port` text `FileDescriptorTable::udp_recv` reports for a
/// datagram's origin.
///
/// Parsing it as a `SocketAddr` first is what keeps an IPv6 peer usable: the
/// wire form is `[::1]:54321`, and the `rsplit_once(':')` this replaces
/// answered the host as the *bracketed* `[::1]`, which no downstream
/// `InetAddress` mirror can parse. The bare-`rsplit` arm survives only as the
/// fallback for a hypothetical non-`SocketAddr` spelling.
pub fn udp_origin_split(origin: &str) -> Option<(String, i32)> {
    if let Ok(sa) = origin.parse::<std::net::SocketAddr>() {
        return Some((sa.ip().to_string(), i32::from(sa.port())));
    }
    let (host, port) = origin.rsplit_once(':')?;
    Some((host.to_string(), port.parse::<i32>().ok()?))
}

/// The raw OS descriptor of a `java.net.Socket`'s connected stream.
///
/// The stream lives in `servlet::s2_registry().streams`, keyed by the id in
/// `SockSide::stream_id`. `native-io`'s extended-option bridge cannot reach it
/// (wrong crate, and it has no `NativeContext`), which is why
/// `Socket.setOption(TCP_KEEPIDLE, ..)` resolved to nothing there and fell
/// through to a UDP-only accessor.
pub fn sock_raw_descriptor(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<SockRawFd> {
    let id = sock_peek(ctx, this, |s| s.stream_id);
    if id < 0 {
        return None;
    }
    let stream = crate::servlet::s2_registry()
        .lock()
        .streams
        .get(&id)
        .cloned()?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        Some(stream.as_raw_fd())
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        Some(stream.as_raw_socket() as usize)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = stream;
        None
    }
}

#[cfg(unix)]
pub type SockRawFd = std::os::fd::RawFd;
#[cfg(windows)]
pub type SockRawFd = usize;
#[cfg(not(any(unix, windows)))]
pub type SockRawFd = i32;

/// `(level, optname)` for the socket options this registrar can serve, or `None`
/// for one it cannot.
///
/// Numbering is per-platform on purpose: `TCP_KEEPIDLE` is 4 on Linux and 3 on
/// Windows (where it is an alias of the older `TCP_KEEPALIVE`), and getting that
/// wrong writes a real value into a different option.
pub fn sock_option_level_and_name(name: &str) -> Option<(i32, i32)> {
    #[cfg(target_os = "linux")]
    {
        const SOL_SOCKET: i32 = 1;
        const IPPROTO_TCP: i32 = 6;
        const SO_KEEPALIVE: i32 = 9;
        const TCP_KEEPIDLE: i32 = 4;
        const TCP_KEEPINTVL: i32 = 5;
        const TCP_KEEPCNT: i32 = 6;
        return Some(match name {
            "SO_KEEPALIVE" => (SOL_SOCKET, SO_KEEPALIVE),
            "TCP_KEEPIDLE" => (IPPROTO_TCP, TCP_KEEPIDLE),
            "TCP_KEEPINTERVAL" => (IPPROTO_TCP, TCP_KEEPINTVL),
            "TCP_KEEPCOUNT" => (IPPROTO_TCP, TCP_KEEPCNT),
            _ => return None,
        });
    }
    #[cfg(windows)]
    {
        const SOL_SOCKET: i32 = 0xffff;
        const IPPROTO_TCP: i32 = 6;
        const SO_KEEPALIVE: i32 = 0x0008;
        const TCP_KEEPIDLE: i32 = 3;
        const TCP_KEEPCNT: i32 = 16;
        const TCP_KEEPINTVL: i32 = 17;
        return Some(match name {
            "SO_KEEPALIVE" => (SOL_SOCKET, SO_KEEPALIVE),
            "TCP_KEEPIDLE" => (IPPROTO_TCP, TCP_KEEPIDLE),
            "TCP_KEEPINTERVAL" => (IPPROTO_TCP, TCP_KEEPINTVL),
            "TCP_KEEPCOUNT" => (IPPROTO_TCP, TCP_KEEPCNT),
            _ => return None,
        });
    }
    #[allow(unreachable_code)]
    {
        let _ = name;
        None
    }
}

#[allow(unused_variables)]
pub fn sock_set_option_int(fd: SockRawFd, level: i32, name: i32, value: i32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let value: libc::c_int = value;
        // SAFETY: `value` outlives the call and `fd` is a live descriptor owned
        // by the stream the registry still holds.
        let rc = unsafe {
            libc::setsockopt(
                fd,
                level,
                name,
                (&value as *const libc::c_int).cast::<libc::c_void>(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    #[cfg(windows)]
    {
        ws2_set_int(fd, level, name, value)
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "socket options",
        ))
    }
}

#[allow(unused_variables)]
pub fn sock_get_option_int(fd: SockRawFd, level: i32, name: i32) -> Option<i32> {
    #[cfg(unix)]
    {
        let mut value: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: valid out-pointer pair for an int-sized option on a live fd.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                level,
                name,
                (&mut value as *mut libc::c_int).cast::<libc::c_void>(),
                &mut len,
            )
        };
        (rc == 0).then_some(value)    }
    #[cfg(windows)]
    {
        ws2_get_int(fd, level, name)
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// The `name()` of a `java.net.SocketOption` argument.
///
/// Every `StandardSocketOptions` / `ExtendedSocketOptions` constant carries its
/// JDK name, so dispatching on it serves whichever constant object a caller
/// passes without this file needing to know their identities.
///
/// gc-common w20-d: `name()` is Java. The field fallback read the option
/// through its pre-call address, and all four callers then used their socket
/// (`this`: a side-table key, the returned receiver) through its address from
/// before the call. Both are pinned; `this` is `&mut` and CURRENT on return.
pub fn ds_socket_option_name(
    ctx: &mut dyn NativeContext,
    this: &mut ObjectRef,
    arg: Option<Value>,
) -> String {
    let Some(Value::Object(Some(opt))) = arg else {
        return String::new();
    };
    let this_pin = ctx.pin_native_root(*this);
    let opt_pin = ctx.pin_native_root(opt);
    let named = ctx.invoke_virtual(opt, "name", "()Ljava/lang/String;", &[]);
    let opt = ctx.read_native_pin(opt_pin, opt);
    *this = ctx.read_native_pin(this_pin, *this);
    ctx.unpin_native_roots(this_pin);
    match named {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        _ => match ctx.get_field_by_name(opt, "name") {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        },
    }
}

/// Unwrap a `setOption` value that arrives either as a raw int or as a boxed
/// `Boolean`/`Integer`.
pub fn ds_unbox_bool(ctx: &mut dyn NativeContext, v: Value) -> bool {
    match v {
        Value::Int(i) => i != 0,
        Value::Object(Some(o)) => {
            matches!(ctx.get_field_by_name(o, "value"), Value::Int(i) if i != 0)
        }
        _ => false,
    }
}

pub fn ds_unbox_int(ctx: &mut dyn NativeContext, v: Value) -> i32 {
    match v {
        Value::Int(i) => i,
        Value::Object(Some(o)) => ctx.get_field_by_name(o, "value").as_int().unwrap_or(0),
        _ => 0,
    }
}

pub fn ds_box_bool(ctx: &mut dyn NativeContext, b: bool) -> MethodCallResult {
    ctx.invoke(
        "java/lang/Boolean",
        "valueOf",
        "(Z)Ljava/lang/Boolean;",
        &[Value::Int(i32::from(b))],
    )
}

pub fn ds_box_int(ctx: &mut dyn NativeContext, n: i32) -> MethodCallResult {
    ctx.invoke(
        "java/lang/Integer",
        "valueOf",
        "(I)Ljava/lang/Integer;",
        &[Value::Int(n)],
    )
}

/// `IP_DONTFRAGMENT` on a UDP fd. Windows has a boolean option; Linux expresses
/// the same thing as the tri-state `IP_MTU_DISCOVER` -- the identical mapping
/// the `jdk/net/*SocketOptions` bridge in `native-io` applies for the fd form.
#[allow(unused_variables)]
pub fn ds_set_dont_fragment(
    ctx: &mut dyn NativeContext,
    fd: i32,
    on: bool,
) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        let Some(raw) = ctx.fd_table().udp_raw_fd(fd as u32) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no raw fd",
            ));
        };
        // linux/in.h: IP_MTU_DISCOVER = 10, IP_PMTUDISC_DO = 2, _DONT = 0.
        let value: libc::c_int = if on { 2 } else { 0 };
        // SAFETY: `value` outlives the call and `raw` is a live descriptor
        // owned by the fd table for its duration.
        let rc = unsafe {
            libc::setsockopt(
                raw,
                libc::IPPROTO_IP,
                10,
                (&value as *const libc::c_int).cast::<libc::c_void>(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    #[cfg(windows)]
    {
        let Some(raw) = ctx.fd_table().udp_raw_socket(fd as u32) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no raw socket",
            ));
        };
        // ws2ipdef.h: IPPROTO_IP = 0, IP_DONTFRAGMENT = 14.
        ws2_set_int(raw as usize, 0, 14, i32::from(on))
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "IP_DONTFRAGMENT",
        ))
    }
}

#[allow(unused_variables)]
pub fn ds_get_dont_fragment(ctx: &mut dyn NativeContext, fd: i32) -> bool {
    #[cfg(unix)]
    {
        let Some(raw) = ctx.fd_table().udp_raw_fd(fd as u32) else {
            return false;
        };
        let mut value: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: valid out-pointer pair for an int-sized option on a live fd.
        let rc = unsafe {
            libc::getsockopt(
                raw,
                libc::IPPROTO_IP,
                10,
                (&mut value as *mut libc::c_int).cast::<libc::c_void>(),
                &mut len,
            )
        };
        rc == 0 && value == 2    }
    #[cfg(windows)]
    {
        let Some(raw) = ctx.fd_table().udp_raw_socket(fd as u32) else {
            return false;
        };
        ws2_get_int(raw as usize, 0, 14)
            .map(|v| v != 0)
            .unwrap_or(false)
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

/// Winsock `setsockopt` for an int option, declared by hand for the same reason
/// `fd_table.rs` and `servlet.rs` do it: `libc` is `cfg(unix)`-shaped for these
/// calls and this crate carries no Windows-specific crate.
#[cfg(windows)]
pub fn ws2_set_int(s: usize, level: i32, name: i32, value: i32) -> Result<(), std::io::Error> {
    #[link(name = "ws2_32")]
    extern "system" {
        fn setsockopt(s: usize, level: i32, optname: i32, optval: *const u8, optlen: i32) -> i32;
        fn WSAGetLastError() -> i32;
    }
    // SAFETY: `value` outlives the call and `s` is a live SOCKET owned by the
    // fd table for its duration.
    let rc = unsafe {
        setsockopt(
            s,
            level,
            name,
            (&value as *const i32).cast::<u8>(),
            std::mem::size_of::<i32>() as i32,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        // SAFETY: plain Winsock error read, no pointers involved.
        Err(std::io::Error::from_raw_os_error(unsafe {
            WSAGetLastError()
        }))
    }
}

#[cfg(windows)]
pub fn ws2_get_int(s: usize, level: i32, name: i32) -> Option<i32> {
    #[link(name = "ws2_32")]
    extern "system" {
        fn getsockopt(s: usize, level: i32, optname: i32, optval: *mut u8, optlen: *mut i32)
            -> i32;
    }
    let mut value: i32 = 0;
    let mut len = std::mem::size_of::<i32>() as i32;
    // SAFETY: valid out-pointer pair for an int-sized option on a live SOCKET.
    let rc = unsafe {
        getsockopt(
            s,
            level,
            name,
            (&mut value as *mut i32).cast::<u8>(),
            &mut len,
        )
    };
    (rc == 0).then_some(value)}

/// Read a `SocketAddress` as **`numeric-host:port`**, preferring the address
/// the JVM already resolved over any hostname the object also carries.
///
/// [`read_inet_socket_address`] answers the hostname first, which is right for
/// callers that need the NAME (an HTTP `Host:` header, TLS SNI) and wrong for
/// callers that hand the result to `bind`/`connect`: those pass the string to
/// the platform resolver, which answers the same question a SECOND time and
/// may order the answers differently. `localhost` is where the two disagree —
/// Windows `getaddrinfo` puts `::1` first, glibc with the stock `/etc/hosts`
/// puts `127.0.0.1` first — so `bind(new InetSocketAddress("localhost", 0))`
/// bound a different family on the two platforms from identical bytes, while
/// HotSpot (which uses `isa.getAddress()` and never looks at the name) bound
/// the loopback v4 address on both. Same rule, same reason, as
/// `native-io`'s `inet_addr_literal`.
///
/// Falls back to [`read_inet_socket_address`] for an UNRESOLVED address
/// (`createUnresolved`, or a constructor whose lookup failed): there is no
/// resolved address to prefer, and the name is all the caller has.
pub fn read_socket_address_numeric(
    ctx: &dyn NativeContext,
    sa: ObjectRef,
) -> Result<(String, i32), cratonvm_types::error::MethodCallFailed> {
    let (name_host, port) = read_inet_socket_address(ctx, sa)?;
    // Real-JDK layout only: `InetSocketAddress.holder.addr`.
    if let Value::Object(Some(holder)) = ctx.get_field(sa, ISA_HOST) {
        if ctx.read_string(holder).is_none() {
            if let Value::Object(Some(ia)) = ctx.get_field(holder, 1) {
                if let Some(literal) = inet_address_numeric_literal(ctx, ia) {
                    return Ok((literal, port));
                }
            }
        }
    }
    Ok((name_host, port))
}


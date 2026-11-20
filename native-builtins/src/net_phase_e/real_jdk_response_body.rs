#[allow(unused_imports)]
use super::*;

/// `HttpExchange.getResponseBody()`'s real-JDK stand-in, built **only** when the
/// policy has already refused the VM-minted `HttpExchange$ResponseBody`.
///
/// W7-24. `com/sun/net/httpserver/HttpExchange$ResponseBody` is a *behaviour
/// carrier* in W7-17's classification, not a door defect, and the distinction
/// decides the fix. `javap com.sun.net.httpserver.HttpExchange$ResponseBody`
/// against the JDK 25 image answers "class not found", so it passes the
/// guardrail — but `--dump-native-registry` taken once per mode reports its five
/// natives (`write(I)`, `write([B)`, `write([BII)`, `flush`, `close`, all
/// registered a few lines below) as `synthetic-stub` in Compatible and **absent**
/// under `--real-jdk`. `CompatibilityStub` is therefore the CORRECT origin: the
/// class exists only to carry an implementation strict mode is deliberately
/// retiring, and the class and its natives are refused together on purpose.
/// Re-minting it through `ensure_vm_internal_class` — the fix
/// `CratonVM$HttpServerLoop` needed, three functions up — would put back a class
/// strict mode has no implementation for and buy an `UnsatisfiedLinkError` at
/// the handler's first `write` instead of the `NoClassDefFoundError` at the
/// mint. What is missing here is not a door but a **fallback at the refusal**,
/// which is the shape `craton_alloc_system_logger` already implements for
/// `cratonvm/internal/SystemLogger`.
///
/// **Why `java.io.ByteArrayOutputStream`, and not what HotSpot returns.**
/// Measured on this host, HotSpot 25's `getResponseBody()` hands back a
/// `sun.net.httpserver.PlaceholderOutputStream`; that class cannot serve — it is
/// package-private, its only constructor takes the `OutputStream` it wraps, and
/// every write goes through a `checkWrap()` that throws until a real
/// `ExchangeImpl` has called `setWrappedStream`. There is no `ExchangeImpl` on
/// this path. `java.io.ByteArrayOutputStream` can: it is a real, public
/// `java.base` class; every method this stream needs is registered `bridge` in
/// BOTH modes (`native-io/src/lib.rs` — `<init>()V`, the three `write`s,
/// `flush`, `close`, `toByteArray`), so nothing about it is dropped by the
/// strict policy; and it has exactly the semantics the carrier had — accumulate
/// now, hand the bytes to the dispatcher after `handle()` returns. It is not the
/// name HotSpot reports, and this VM's Compatible answer
/// (`com.sun.net.httpserver.HttpExchange$ResponseBody`) is not that name either,
/// so the fallback costs no fidelity that was there to lose and buys a working
/// response body where strict mode had none.
///
/// **Where it is parked.** In the exchange's own chunk array (slot 6), not in a
/// Rust side table: the array is GC-traced from the exchange for exactly as long
/// as the exchange lives, and `re10_dispatch_pending`'s drain already looks
/// there, in order, for the response bytes. A side table keyed on an `ObjectRef`
/// would need pinning across the whole handler call and would inherit a
/// recycled address's state.
pub fn re10_real_jdk_response_body(
    ctx: &mut dyn NativeContext,
    ex_pin: usize,
    ex0: ObjectRef,
    refusal: MethodCallFailed,
) -> Result<ObjectRef, MethodCallFailed> {
    // Keep the refusal itself alive across the allocations below.
    // `MethodCallFailed::ExceptionThrown` carries a raw `ObjectRef`, and
    // re-raising one that a young collection moved in the meantime is the same
    // stale-local family every other site in this function's neighbourhood pins
    // against. The pin batch belongs to the caller's `this_pin`, so the caller's
    // single `unpin_native_roots` releases this one too — do not unpin here.
    let refusal_pin = match &refusal {
        MethodCallFailed::ExceptionThrown(exc) => Some((ctx.pin_native_root(*exc), *exc)),
        _ => None,
    };
    // Same order as `real_jdk_system_logger`: ask whether the real class is
    // there BEFORE trying to build it, so a missing image class leaves the
    // refusal standing rather than turning it into a second, less informative
    // failure.
    let have_real = ctx
        .class_id_by_name("java/io/ByteArrayOutputStream")
        .is_some()
        || ctx
            .ensure_class_initialized("java/io/ByteArrayOutputStream")
            .is_ok();
    let built = if have_real {
        ctx.new_object_initialized("java/io/ByteArrayOutputStream", "()V", &[])    } else {
        Ok(None)
    };
    let sink = match built {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => return Err(re10_reraise(ctx, refusal, refusal_pin)),
    };
    // No allocation between the create above and the park below — `get_field`,
    // `array_length`, `get_array_element` and `set_array_element` do not
    // allocate — so `sink` needs no pin of its own; the exchange does, because
    // `new_object_initialized` ran a constructor.
    let ex = ctx.read_native_pin(ex_pin, ex0);
    let chunks = match ctx.get_field(ex, 6) {
        Value::Object(Some(c)) => c,
        _ => return Err(re10_reraise(ctx, refusal, refusal_pin)),
    };
    let cap = ctx.array_length(chunks);
    for i in 0..cap {
        if let Value::Object(None) = ctx.get_array_element(chunks, i) {
            ctx.set_array_element(chunks, i, Value::Object(Some(sink)));
            return Ok(sink);
        }
    }
    // Chunk array full. The three `write` natives below silently drop a chunk in
    // this case; a fallback stream that the drain will never read must NOT be
    // silent about it — a response body dropped without a word is the
    // "refusal laundered into a wrong answer" failure W7-17 §8 names as worse
    // than either gate.
    Err(ioex(
        "HttpExchange.getResponseBody: no free response chunk slot for the \
         real-JDK response body",
    ))
}

/// Re-raise a refusal that has been held across allocations, reading it back
/// from its pin first. Split out because the fallback above has three exits that
/// must all do it and a copy that forgets the `read_native_pin` is invisible
/// until a young collection happens to move the exception.
pub fn re10_reraise(
    ctx: &dyn NativeContext,
    refusal: MethodCallFailed,
    refusal_pin: Option<(usize, ObjectRef)>,
) -> MethodCallFailed {
    match refusal_pin {
        Some((handle, exc)) => MethodCallFailed::ExceptionThrown(ctx.read_native_pin(handle, exc)),
        None => refusal,
    }
}

pub fn register_re10_http_server(r: &mut NativeMethodRegistry) {
    let hs = "com/sun/net/httpserver/HttpServer";
    // The JDK factory contract returns this concrete implementation, not the
    // abstract public API class. Native bridges are aliased to it after all
    // registrations below so virtual dispatch retains the concrete receiver.

    // VM-thread dispatch loop runner (see re10_spawn_dispatcher).
    r.register(HS_LOOP_CLASS, "run", "()V", re10_serve_loop_run, NativeKind::SyntheticStub);

    r.register(
        hs,
        "create",
        "(Ljava/net/InetSocketAddress;I)Lcom/sun/net/httpserver/HttpServer;",
        // The public class is abstract, but this native-backed server owns the
        // concrete implementation, so the factory hands back `HS_IMPL_CLASS`
        // (aliased at the end of this registrar).
        |ctx, args| re10_create_server(ctx, args, HS_IMPL_CLASS), NativeKind::Bridge,
    );

    // No-arg factory. Previously registered ONLY by phase 72, which minted a
    // 3-slot object with no `server_registry` entry — `start()` on it reported
    // "server not registered", and in real-JDK mode (where phase 72 does not
    // run at all) `HttpServer.create()` resolved to the abstract declaration.
    r.register(
        hs,
        "create",
        "()Lcom/sun/net/httpserver/HttpServer;",
        |ctx, _args| re10_create_unbound_server(ctx), NativeKind::SyntheticStub,
    );

    // Real bind. Phase 72's version only wrote the address into a slot, so the
    // documented `create(); bind(addr, backlog); start();` sequence never
    // opened a socket.
    r.register(
        hs,
        "bind",
        "(Ljava/net/InetSocketAddress;I)V",
        |ctx, args| re10_bind_server(ctx, args), NativeKind::SyntheticStub,
    );

    // `HttpServer` declares these methods abstract. `create` returns the
    // native-backed receiver above, so both calls must be registered here;
    // otherwise virtual dispatch resolves the abstract declaration and throws
    // `AbstractMethodError: ... has no Code attribute` before a server can
    // start (Keycloak's shared startHttpServer helper exercises this path).
    r.register(
        hs,
        "setExecutor",
        "(Ljava/util/concurrent/Executor;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if ctx.get_field(this, HS_STARTED).as_int().unwrap_or(0) != 0 {
                return Err(RuntimeError::IllegalStateException {
                    message: "server already started".to_string(),
                }
                .into());
            }
            ctx.set_field(
                this,
                HS_EXECUTOR,
                args.get(1).copied().unwrap_or(Value::Object(None)),
            );
            Ok(None)
        }, NativeKind::SyntheticStub,
    );
    r.register(
        hs,
        "getExecutor",
        "()Ljava/util/concurrent/Executor;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, HS_EXECUTOR)))
        }, NativeKind::SyntheticStub,
    );

    r.register(hs, "start", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = ctx.get_field(this, HS_SERVER_ID).as_int().unwrap_or(-1);
        if id < 0 {
            return Err(ioex("HttpServer not initialised"));
        }
        // `ServerImpl.start()` throws IllegalStateException("server in wrong
        // state") for a server that was never bound. It matters that this is
        // NOT an IOException: `HttpServer.start()` declares no checked
        // exception, so an IOException here is undeclared — and the condition
        // is the ordinary "created with `create()` and forgot to `bind()`"
        // programming mistake, not an I/O failure.
        let state = {
            let reg = server_registry().lock();
            reg.get(&id).cloned()
        };
        let bound = match state {
            Some(state) => state.listener.lock().is_some(),
            None => false,
        };
        if !bound {
            return Err(RuntimeError::IllegalStateException {
                message: "server in wrong state".to_string(),
            }
            .into());
        }
        re10_start_server(id).map_err(|e| ioex(format!("HttpServer start: {e}")))?;
        ctx.set_field(this, HS_STARTED, Value::Int(1));
        // The OS accept thread (re10_start_server) only parses requests into the
        // queue — it has no VM context and cannot invoke the Java `HttpHandler`.
        // Spawn a dedicated VM thread whose `run()` drains that queue and
        // dispatches each request on a thread that CAN run Java bytecode. Without
        // this, requests were only dispatched on `start()`/`stop()`, so a client
        // that blocks waiting for a response (e.g. the ES `RestClient*IntegTests`)
        // deadlocks against the embedded server. See ES-HANG-02.
        re10_spawn_dispatcher(ctx, id)?;
        Ok(None)
    }, NativeKind::SyntheticStub);

    r.register(hs, "stop", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = ctx.get_field(this, HS_SERVER_ID).as_int().unwrap_or(-1);
        // gc-common w28-a: `re10_dispatch_pending` below runs the pending
        // requests' Java `HttpHandler`s, which can move the server; the
        // `HS_STARTED` write after it used the pre-dispatch address (a store
        // into a vacated slot). Pinned for the whole native, read back last.
        let this_pin = ctx.pin_native_root(this);
        if id >= 0 {
            // Clone the Arc out from under the registry lock before taking the
            // per-server `listener` lock — the two must never be held at once
            // (see `gc_scan_re10_handler_roots`).
            let state = {
                let reg = server_registry().lock();
                reg.get(&id).cloned()
            };
            if let Some(state) = &state {
                state.running.store(false, Ordering::SeqCst);
                // Close the OS listener NOW (drop it) so the port immediately
                // refuses connections — a round-robin client must see a stopped
                // host fail fast and retry, not connect into a dead server in the
                // window before the accept thread notices `running`.
                state.listener.lock().take();
            }
            re10_dispatch_pending(ctx, id)?;
            // Anything still queued is discarded under the queue lock, after
            // `running` went false, so `re10_enqueue` cannot re-park a request
            // behind the exiting dispatchers. Dropping those requests drops their
            // reply senders, which releases their connection threads; the
            // notify wakes idle dispatchers to see `running` and exit.
            request_queue().lock().remove(&id);
            request_queue_ready().notify_all();
            if let Some(state) = &state {
                // Kept-alive connections are parked in a read on their own
                // threads. Shutting the socket ends that read; the thread exits.
                for (_, conn) in state.conns.lock().drain() {
                    let _ = conn.shutdown(std::net::Shutdown::Both);
                }
            }
            server_registry().lock().remove(&id);
        }
        let this = ctx.read_native_pin(this_pin, this);
        ctx.set_field(this, HS_STARTED, Value::Int(0));
        ctx.unpin_native_roots(this_pin);
        Ok(None)
    }, NativeKind::SyntheticStub);

    r.register(
        hs,
        "createContext",
        "(Ljava/lang/String;Lcom/sun/net/httpserver/HttpHandler;)Lcom/sun/net/httpserver/HttpContext;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let path_val = args.get(1).copied().unwrap_or(Value::Object(None));
            let path = value_or_string(ctx, path_val, "/");
            let handler0 = obj_arg(args, 2).map_err(|_| ioex("createContext: null handler"))?;
            let id = ctx.get_field(this, HS_SERVER_ID).as_int().unwrap_or(-1);
            // The registry entry now carries the context object too, so the
            // context has to exist before the entry is pushed. That puts the
            // HttpContext/String allocations BEFORE the only thing that roots
            // `handler` — pin it across them (native stale-local family) so the
            // entry, and the context's slot 1, record the live address rather
            // than a vacated from-space slot.
            //
            // gc-common w28-a: the server (`this`) too. It is handed to
            // `http_link_set` below, which roots it for
            // `HttpContext.getServer()`, AFTER both allocations; the pre-
            // allocation address it used to be handed could name a vacated
            // slot. Pinned first, so the one release below frees the batch.
            let this_pin = ctx.pin_native_root(this);
            let h_pin = ctx.pin_native_root(handler0);
            let hctx0 = try_alloc_concurrent_synthetic(ctx, "com/sun/net/httpserver/HttpContext", 2)?;
            let hctx_pin = ctx.pin_native_root(hctx0);
            let path_s = ctx.create_string(&path);
            let hctx = ctx.read_native_pin(hctx_pin, hctx0);
            ctx.set_field(hctx, 0, Value::Object(Some(path_s)));
            let handler = ctx.read_native_pin(h_pin, handler0);
            ctx.set_field(hctx, 1, Value::Object(Some(handler)));
            if id >= 0 {
                if let Some(state) = server_registry().lock().get(&id) {
                    state.handlers.lock().push(HttpHandlerEntry {
                        path_prefix: path.clone(),
                        handler,
                        context: hctx,
                    });
                }
            }
            // Record the owning server so `HttpContext.getServer()` can answer
            // it: the 2-slot context layout has no room for a back-pointer, so
            // the link table carries it. Phase 72's ONE-arg `createContext` has
            // always done this; this TWO-arg one -- the overload javac emits for
            // `createContext(path, handler)`, and the only one registered in a
            // real-JDK build -- never did, so the documented
            // `context.getServer().getExecutor()` idiom saw a null server in
            // every mode.
            let this = ctx.read_native_pin(this_pin, this);
            crate::phases_late::net_channels::http_link_set(
                ctx,
                crate::phases_late::net_channels::HTTP_LINK_CONTEXT_SERVER,
                hctx,
                Some(this),
            );
            let hctx = ctx.read_native_pin(hctx_pin, hctx0);
            // Releases the whole batch (server + handler + context).
            ctx.unpin_native_roots(this_pin);
            Ok(Some(Value::Object(Some(hctx))))
        }, NativeKind::SyntheticStub,
    );

    // removeContext(String path) — drop the handler registered for that exact
    // path (ES MultipleHosts resetWaitHandlers swaps the "/wait" handler).
    r.register(hs, "removeContext", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = value_or_string(ctx, args.get(1).copied().unwrap_or(Value::Object(None)), "");
        let id = ctx.get_field(this, HS_SERVER_ID).as_int().unwrap_or(-1);
        if id >= 0 {
            if let Some(state) = server_registry().lock().get(&id) {
                state.handlers.lock().retain(|e| e.path_prefix != path);            }        }
        Ok(None)
    }, NativeKind::SyntheticStub);
    // removeContext(HttpContext ctx) — same, resolving the path from the context's
    // slot 0 (set by createContext).
    r.register(
        hs,
        "removeContext",
        "(Lcom/sun/net/httpserver/HttpContext;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = ctx.get_field(this, HS_SERVER_ID).as_int().unwrap_or(-1);
            if let Some(Value::Object(Some(hctx))) = args.get(1).copied() {
                let path = value_or_string(ctx, ctx.get_field(hctx, 0), "");
                if id >= 0 {
                    if let Some(state) = server_registry().lock().get(&id) {
                        state.handlers.lock().retain(|e| e.path_prefix != path);                    }                }            }
            Ok(None)
        }, NativeKind::SyntheticStub,
    );

    r.register(
        hs,
        "getAddress",
        "()Ljava/net/InetSocketAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, HS_ADDRESS)))
        }, NativeKind::SyntheticStub,
    );

    let hex = "com/sun/net/httpserver/HttpExchange";
    // These abstract HttpExchange getters must be live in the default
    // real-JDK registry as well as in the synthetic phase-72 overlay.  The
    // dispatch path owns the endpoint slots and records them from the accepted
    // TcpStream; return null only for an exchange minted by an older/foreign
    // path that does not carry those slots.
    r.register(
        hex,
        "getLocalAddress",
        "()Ljava/net/InetSocketAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if ctx.object_num_fields(this) <= HEX_REMOTE_ADDR {
                return Ok(Some(Value::Object(None)));
            }
            Ok(Some(ctx.get_field(this, HEX_LOCAL_ADDR)))
        }, NativeKind::SyntheticStub,
    );
    r.register(
        hex,
        "getRemoteAddress",
        "()Ljava/net/InetSocketAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if ctx.object_num_fields(this) <= HEX_REMOTE_ADDR {
                return Ok(Some(Value::Object(None)));
            }
            Ok(Some(ctx.get_field(this, HEX_REMOTE_ADDR)))
        }, NativeKind::SyntheticStub,
    );
    r.register(
        hex,
        "getRequestMethod",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        }, NativeKind::SyntheticStub,
    );
    r.register(hex, "getRequestURI", "()Ljava/net/URI;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // The exchange stores the request target as a String. Do not place it
        // in a guessed URI field slot: in the real JDK layout slot 0 is the
        // scheme, not the full external form, which made toString()/getPath()
        // observe an empty URI. `make_uri` writes the canonical URI fields by
        // name and therefore works for both synthetic and real-JDK layouts.
        let raw = match ctx.get_field(this, 1) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let uri = make_uri(ctx, &raw)?;
        Ok(Some(Value::Object(Some(uri))))
    }, NativeKind::SyntheticStub);
    // Request/response headers are stored on the exchange as REAL
    // `com.sun.net.httpserver.Headers` (see re10_dispatch_pending) — return them
    // directly so the handler operates on a live Map.
    r.register(
        hex,
        "getResponseHeaders",
        "()Lcom/sun/net/httpserver/Headers;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 3)))
        }, NativeKind::SyntheticStub,
    );
    r.register(
        hex,
        "getRequestHeaders",
        "()Lcom/sun/net/httpserver/Headers;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 2)))
        }, NativeKind::SyntheticStub,
    );
    r.register(
        hex,
        "getRequestBody",
        "()Ljava/io/InputStream;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let body0 = match ctx.get_field(this, 4) {
                Value::Object(Some(a)) => a,
                _ => ctx.new_array(ArrayElementType::Byte, 0),
            };
            let len = ctx.array_length(body0) as i32;
            // GC-safety: `body0` is read here but used only AFTER the
            // `ByteArrayInputStream` allocation below, which can trigger a moving
            // GC. A stale `body` would set `ByteArrayInputStream.buf` to a vacated
            // from-space slot, so the handler reads garbage request bytes and
            // typically throws while decoding — aborting `handle()` BEFORE it
            // calls `sendResponseHeaders(status)`, which the dispatcher then
            // reports as the default 200 (observed as ES auth-test
            // `expected:<403> but was:<200>` under -Xmx1g GC pressure). Pin it
            // across the alloc and read the forwarded address back.
            let body_pin = ctx.pin_native_root(body0);
            let stream = try_alloc_concurrent_synthetic(ctx, "java/io/ByteArrayInputStream", 4)?;
            let body = ctx.read_native_pin(body_pin, body0);
            ctx.set_field(stream, 0, Value::Object(Some(body))); // buf
            ctx.set_field(stream, 1, Value::Int(0)); // pos
            ctx.set_field(stream, 2, Value::Int(0)); // mark
            ctx.set_field(stream, 3, Value::Int(len)); // count
            ctx.unpin_native_roots(body_pin);
            Ok(Some(Value::Object(Some(stream))))
        }, NativeKind::SyntheticStub,
    );
    r.register(hex, "sendResponseHeaders", "(IJ)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let code = args.get(1).and_then(|v| v.as_int()).unwrap_or(200);
        ctx.set_field(this, 5, Value::Int(code));
        // Stash the response-length argument (field 7): per the
        // com.sun.net.httpserver contract, -1 means "no response body and NO
        // Content-Length header" (HEAD / 204 / 304); >0 is the body length; 0
        // means chunked. We honour -1 so HEAD responses don't carry a spurious
        // Content-Length (ES testHeaders). The Long arrives at args[2].
        let len = match args.get(2) {
            Some(Value::Long(l)) => *l,
            Some(v) => v.as_int().map(|i| i as i64).unwrap_or(0),
            None => 0,
        };
        ctx.set_field(
            this,
            7,
            Value::Int(len.clamp(i32::MIN as i64, i32::MAX as i64) as i32),
        );
        Ok(None)
    }, NativeKind::SyntheticStub);
    r.register(
        hex,
        "getResponseBody",
        "()Ljava/io/OutputStream;",
        |ctx, args| {
            // GC-safety: `this` (the exchange) is used AFTER the ResponseBody
            // allocation, which can move it. A stale `this` would set
            // `ResponseBody.owner` (slot 0) to a vacated exchange address, so a
            // later `out.write(...)` reads the wrong/garbage owner and drops the
            // response body. Pin it across the alloc and read it back forwarded.
            let this0 = obj_arg(args, 0)?;
            let this_pin = ctx.pin_native_root(this0);
            let out = match try_alloc_concurrent_synthetic(
                ctx,
                "com/sun/net/httpserver/HttpExchange$ResponseBody",
                2,
            ) {
                Ok(out) => {
                    let this = ctx.read_native_pin(this_pin, this0);
                    ctx.set_field(out, 0, Value::Object(Some(this)));
                    ctx.set_field(out, 1, Value::Int(0));
                    out
                }
                // W7-24 — the real-JDK fallback, taken ONLY at the refusal.
                // `Compatible` never refuses this mint (measured: the class is
                // fabricated once per run in every Compatible HttpServer run,
                // and the `--real-jdk-report` row for it is the only
                // `net_phase_e` row on the serve path besides the dispatcher's),
                // so this arm cannot run there and Compatible is unchanged.
                Err(refusal) => match re10_real_jdk_response_body(ctx, this_pin, this0, refusal) {
                    Ok(out) => out,
                    Err(err) => {
                        // Unpin before unwinding: an early return past
                        // `unpin_native_roots` leaks the frame.
                        ctx.unpin_native_roots(this_pin);
                        return Err(err);
                    }
                },
            };
            ctx.unpin_native_roots(this_pin);
            Ok(Some(Value::Object(Some(out))))
        }, NativeKind::SyntheticStub,
    );
    // KEEP as a no-op. `HttpExchange.close()` closes the request/response
    // streams; here neither owns an OS resource — the request body is a
    // `ByteArrayInputStream` over an already-materialised array and the
    // response body is a chunk array the dispatcher serialises AFTER
    // `handle()` returns (see `rb` below). Closing the connection here would
    // be actively wrong: the bytes have not been written yet.
    r.register(hex, "close", "()V", |_ctx, _args| Ok(None), NativeKind::SyntheticStub);
    // `getPrincipal()` is the observable half of the authentication gate: the
    // dispatcher writes the `HttpPrincipal` carried by an
    // `Authenticator.Success` into `HEX_PRINCIPAL` before it calls the handler,
    // and the handler reads it back through here. Null for an unauthenticated
    // context, matching the real server (whose `HttpExchangeImpl.principal` is
    // likewise only set by the auth filter).
    r.register(
        hex,
        "getPrincipal",
        "()Lcom/sun/net/httpserver/HttpPrincipal;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if ctx.object_num_fields(this) <= HEX_PRINCIPAL {
                return Ok(Some(Value::Object(None)));
            }
            Ok(Some(ctx.get_field(this, HEX_PRINCIPAL)))
        }, NativeKind::SyntheticStub,
    );
    // `getHttpContext()` and `getProtocol()` are both `abstract` on the real
    // `HttpExchange` and, like the §3 `HttpContext` accessors this file
    // already fixed, had NO registration at all in any mode — every call
    // resolved to the abstract declaration and threw AbstractMethodError.
    // `the-httpserver-family-four-defects-20260902-RETIRED-20260920.md`
    // §6 named this exact gap ("the census reports only what a run reaches, so
    // its silence about a class is not a clearance"); `HttpExchangeRequestProbe`
    // is what finally served a request through it. `HEX_CONTEXT`/`HEX_PROTOCOL`
    // are written once, at exchange-mint time, in `re10_dispatch_pending`.
    r.register(
        hex,
        "getHttpContext",
        "()Lcom/sun/net/httpserver/HttpContext;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if ctx.object_num_fields(this) <= HEX_CONTEXT {
                return Ok(Some(Value::Object(None)));
            }
            Ok(Some(ctx.get_field(this, HEX_CONTEXT)))
        }, NativeKind::SyntheticStub,
    );
    r.register(hex, "getProtocol", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) <= HEX_PROTOCOL {
            return Ok(Some(Value::Object(None)));
        }
        Ok(Some(ctx.get_field(this, HEX_PROTOCOL)))
    }, NativeKind::SyntheticStub);

    let rb = "com/sun/net/httpserver/HttpExchange$ResponseBody";
    r.register(rb, "write", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let owner0 = match ctx.get_field(this, 0) {
            Value::Object(Some(o)) => o,
            _ => return Err(ioex("ResponseBody has no exchange")),
        };
        let buf = obj_arg(args, 1)?;
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
        let data = java_byte_array_to_vec(ctx, buf, off, len)?;
        // GC-safety: `owner` (the exchange) is used after `new_java_byte_array`,
        // which can relocate it. Pin across the alloc and read it back forwarded
        // so the chunk lands in the live exchange's chunk array, not a vacated
        // from-space slot (which would silently drop the response body).
        let owner_pin = ctx.pin_native_root(owner0);
        let chunk = new_java_byte_array(ctx, &data);
        let owner = ctx.read_native_pin(owner_pin, owner0);
        if let Value::Object(Some(chunks)) = ctx.get_field(owner, 6) {
            let cap = ctx.array_length(chunks);
            for i in 0..cap {
                if let Value::Object(None) = ctx.get_array_element(chunks, i) {
                    ctx.set_array_element(chunks, i, Value::Object(Some(chunk)));
                    break;
                }
            }
        }
        ctx.unpin_native_roots(owner_pin);
        Ok(None)
    }, NativeKind::SyntheticStub);
    // `OutputStream.write(byte[])` — the synthetic ResponseBody does not inherit
    // the real OutputStream default (no superclass bytecode), so register the
    // overload explicitly. Delegates to the same append-a-chunk logic as
    // write([BII) over the whole array.
    r.register(rb, "write", "([B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let owner0 = match ctx.get_field(this, 0) {
            Value::Object(Some(o)) => o,
            _ => return Err(ioex("ResponseBody has no exchange")),
        };
        let buf = obj_arg(args, 1)?;
        let len = ctx.array_length(buf) as i32;
        let data = java_byte_array_to_vec(ctx, buf, 0, len)?;
        // GC-safety: see write([BII) — pin `owner` across the chunk allocation.
        let owner_pin = ctx.pin_native_root(owner0);
        let chunk = new_java_byte_array(ctx, &data);
        let owner = ctx.read_native_pin(owner_pin, owner0);
        if let Value::Object(Some(chunks)) = ctx.get_field(owner, 6) {
            let cap = ctx.array_length(chunks);
            for i in 0..cap {
                if let Value::Object(None) = ctx.get_array_element(chunks, i) {
                    ctx.set_array_element(chunks, i, Value::Object(Some(chunk)));
                    break;
                }
            }
        }
        ctx.unpin_native_roots(owner_pin);
        Ok(None)
    }, NativeKind::SyntheticStub);
    r.register(rb, "write", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let owner0 = match ctx.get_field(this, 0) {
            Value::Object(Some(o)) => o,
            _ => return Err(ioex("ResponseBody has no exchange")),
        };
        let b = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) & 0xff;
        // GC-safety: see write([BII) — pin `owner` across the 1-byte chunk alloc.
        let owner_pin = ctx.pin_native_root(owner0);
        let chunk = ctx.new_array(ArrayElementType::Byte, 1);
        ctx.set_array_element(chunk, 0, Value::Int(b as i8 as i32));
        let owner = ctx.read_native_pin(owner_pin, owner0);
        if let Value::Object(Some(chunks)) = ctx.get_field(owner, 6) {
            let cap = ctx.array_length(chunks);
            for i in 0..cap {
                if let Value::Object(None) = ctx.get_array_element(chunks, i) {
                    ctx.set_array_element(chunks, i, Value::Object(Some(chunk)));
                    break;
                }
            }
        }
        ctx.unpin_native_roots(owner_pin);
        Ok(None)
    }, NativeKind::SyntheticStub);
    // KEEP as no-ops. This stream is not buffered over a socket: every
    // `write` appends a chunk to the exchange's `chunks` array (field 6),
    // which the serve loop drains and sends once the handler returns. There
    // is nothing to push out on `flush`, and nothing to release on `close` —
    // the array is ordinary heap that the exchange owns. Making either of
    // them send would emit the response body before its status line.
    r.register(rb, "flush", "()V", |_ctx, _args| Ok(None), NativeKind::SyntheticStub);
    r.register(rb, "close", "()V", |_ctx, _args| Ok(None), NativeKind::SyntheticStub);

    // Native dispatch is keyed by the receiver class rather than Java
    // inheritance. Mirror the complete public HttpServer bridge surface onto
    // the concrete class returned by the factory, including set/getExecutor.
    // The HttpContext accessors, in EVERY mode. They lived only in phase 72
    // (reachable solely from `register_synthetic_overrides`) while
    // `createContext` minted the carrier in every mode, so on the shipping
    // arms `getPath`/`getServer`/`getHandler`/`getAttributes` all resolved to
    // the abstract declaration and threw AbstractMethodError.
    crate::phases_late::net_channels::register_http_context_surface(r);

    r.alias_class(hs, HS_IMPL_CLASS);
}


/// `SSLSocket.setEnabledCipherSuites` -- the ONE implementation, registered by both
/// `register_phase_e_networking` and `t27_tls::register_client_socket_mode_accessors`.
///
/// REGRESSION 2026-09-11 (`ee264ea33`): `t27_tls` registers this triple AFTER
/// this file and gave it a body of its own that validated the names and did
/// nothing else, so it won by last-writer-wins and every restriction below --
/// the `HttpsURLConnection` probe recording, the deferred layered-socket
/// entry, the eager-socket reconnect -- stopped running. Tomcat's
/// `TestSSLHostConfigCipher`, `TestSSLHostConfigProtocol` and
/// `TestSSLHostConfigCompat` connected unrestricted where JSSE refuses the
/// handshake. Both registrars now name this function, so which one wins no
/// longer decides the behaviour.
pub fn ssl_socket_set_enabled_cipher_suites(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Validate before anything else, with HotSpot's messages: null first,
    // then membership (the order is observable -- `setEnabledCipherSuites(null)`
    // reports "CipherSuites cannot be null", not a null suite).
    let Some(Value::Object(Some(arr))) = args.get(1).copied() else {
        return Err(RuntimeError::IllegalArgumentException {
            message: "CipherSuites cannot be null".into(),
        }
        .into());
    };
    for i in 0..ctx.array_length(arr) {
        let name = match ctx.get_array_element(arr, i) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        }
        .unwrap_or_default();
        if !crate::t27_tls::SUPPORTED_CIPHER_SUITE_NAMES.contains(&name.as_str()) {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("Unsupported CipherSuite: {name}"),
            }
            .into());
        }
    }
    let this = obj_arg(args, 0)?;
    let mut ciphers: Vec<String> = Vec::new();
    if let Some(Value::Object(Some(arr))) = args.get(1) {
        let len = ctx.array_length(*arr);
        for i in 0..len {
            if let Value::Object(Some(s)) = ctx.get_array_element(*arr, i) {
                if let Some(t) = ctx.read_string(s) {
                    ciphers.push(t);
                }
            }
        }
    }
    // FIX (tls-handshake-enforcement-gap, doc 21): on a probe socket
    // (`set_huc_factory_probe_mode`) there is no connection to
    // reconnect — record the restriction verbatim, INCLUDING names
    // this rustls build cannot map, and let
    // `http_url_connection::perform` decide what it can enforce when
    // it builds the one real connection.
    if crate::nbflags().dbg_tls_auth_ok {
        eprintln!(
            "[dbg-tls-auth] SSLSocket.setEnabledCipherSuites n={} probe={} tls_id={}",
            ciphers.len(),
            is_probe_socket(ctx, this),
            crate::phases_late::new13_resolve_tls_id(ctx, this)
        );
    }
    if is_probe_socket(ctx, this) || super::huc_factory_probe_mode() {
        record_probe_ciphers(ctx, this, ciphers);
        return Ok(None);
    }
    // A socket from `createSocket(Socket wrapped, ...)` handshakes
    // lazily, so a restriction set beforehand still counts. THIS
    // registration overrides the one in `phases_late/ssl_security.rs`
    // (net_phase_e registers later, last-writer-wins) which was the
    // only place that handled the deferred case — so overriding it
    // silently dropped the restriction for every layered socket,
    // which is the shape the real JDK `HttpsURLConnection` uses.
    let tls_id = crate::phases_late::new13_resolve_tls_id(ctx, this);
    if tls_id >= crate::servlet::PENDING_LAYERED_SOCK_ID_BASE
        && tls_id < crate::servlet::RUSTLS_SOCK_ID_BASE
    {
        let pending_id = tls_id - crate::servlet::PENDING_LAYERED_SOCK_ID_BASE;
        crate::t27_tls::set_pending_layered_socket_ciphers(pending_id, ciphers);
        return Ok(None);
    }
    // A connect-path socket has not handshaked yet (its `connect`
    // parked the endpoint -- see `PENDING_CONNECT_SOCK_ID_BASE`), so
    // there is no established connection to tear down and redo. Accept
    // and discard, which is what this registration already did for an
    // already-handshaked socket.
    if tls_id >= crate::servlet::PENDING_CONNECT_SOCK_ID_BASE
        && tls_id < crate::servlet::PENDING_LAYERED_SOCK_ID_BASE
    {
        return Ok(None);
    }
    if ciphers.is_empty() || !crate::t27_tls::any_cipher_mappable(&ciphers) {
        return Ok(None);
    }
    // FIX (netty-client-socket-write-after-close residual): don't
    // reconnect when `ciphers` covers this socket's ENTIRE supported
    // set (`ssl_sock_supported_cipher_suites` in phases_late.rs, the
    // list `SSLSocket.getSupportedCipherSuites()` returns) — that's
    // not a real restriction, just a caller re-asserting "use
    // everything you support" (the common case: e.g. Apache
    // HttpClient5's `SSLConnectionSocketFactory` calls
    // `setEnabledCipherSuites(getSupportedCipherSuites())` as part of
    // its normal connection setup, unconditionally, for every socket
    // it creates — not only ones under an actual cipher policy).
    // Reconnecting anyway tears down a connection that may have been
    // established under semantics THIS rustls-only path cannot
    // reproduce (a Java TrustManager accepting a self-signed/test
    // certificate — see `new13_do_create_socket`'s post-connect
    // `checkServerTrusted` delegation, which this reconnect has no
    // way to redo), turning a harmless no-op call into a hard
    // failure. Only reconnect for a GENUINE restriction: a proper
    // subset of the supported suites (e.g. Tomcat's
    // `TesterSupport.ClientSSLSocketFactory`, which this reconnect
    // exists for in the first place — see the FIX comment above).
    let requested: std::collections::HashSet<&str> = ciphers.iter().map(String::as_str).collect();
    let is_full_supported_set = CLIENT_SUPPORTED_CIPHER_SUITES
        .iter()
        .all(|c| requested.contains(c));
    if is_full_supported_set {
        return Ok(None);
    }
    // gc-common w16-a: the key is taken HERE, before the blocking reconnect
    // below. A collection may run inside that region and move `this` (a raw
    // copy it does not rewrite), and the stream-id write after it used to
    // compute the key from that possibly vacated address.
    // Looked up, never minted (gc-common w28-a): a socket never keyed has no
    // host, and the default row returns just below.
    let Some(this_key) = existing_native_obj_key(ctx, this) else {
        return Ok(None);
    };
    let side = sock_get_key(this_key);
    let host = side.host.clone();
    if host.is_empty() || side.port <= 0 {
        return Ok(None);
    }
    let vm = ctx.vm_identity();
    let client_ident = crate::t27_tls::huc_default_client_identity_in_vm(vm);
    let cfg = match crate::t27_tls::build_socket_client_config_with_identity_ciphers_in_vm(
        vm,
        &["http/1.1"],
        client_ident.as_ref().map(|(c, k)| (c.as_str(), k.as_str())),
        None,
        None,
        &ciphers,
        &[],
    ) {
        Ok(cfg) => cfg,
        Err(msg) => {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "javax/net/ssl/SSLHandshakeException",
                &msg,
            ));
        }
    };
    // T19.H1: see the matching comment on `createSocket` above — this
    // reconnect blocks on real network I/O too and must announce it.
    ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
    let connect_result = crate::t27_tls::rustls_client_connect(cfg, &host, side.port as u16);
    ctx.end_blocking_region();
    match connect_result {
        Ok(rid) => {
            if side.stream_id >= 0 {
                let _ = crate::servlet::s2_tls_close(side.stream_id);
            }
            let new_id = crate::servlet::RUSTLS_SOCK_ID_BASE + rid;
            // gc-common w14-a (handoff-w14b-net-phase-e-rustls-reconnects-
            // claim-their-vm): the reconnected rustls stream belongs to this VM.
            crate::servlet::s2_claim_for_vm(ctx.vm_identity(), new_id);
            // By the key taken before the blocking region (gc-common w16-a).
            sock_set_key(this_key, |s| {
                s.stream_id = new_id;
            });
            Ok(None)
        }
        Err(msg) => Err(crate::phases_early::throw_jca_exc(
            ctx,
            "javax/net/ssl/SSLHandshakeException",
            &msg,
        )),
    }
}

/// `SSLSocket.setEnabledProtocols` -- the ONE implementation, registered by both
/// `register_phase_e_networking` and `t27_tls::register_client_socket_mode_accessors`.
///
/// REGRESSION 2026-09-11 (`ee264ea33`): `t27_tls` registers this triple AFTER
/// this file and gave it a body of its own that validated the names and did
/// nothing else, so it won by last-writer-wins and every restriction below --
/// the `HttpsURLConnection` probe recording, the deferred layered-socket
/// entry, the eager-socket reconnect -- stopped running. Tomcat's
/// `TestSSLHostConfigCipher`, `TestSSLHostConfigProtocol` and
/// `TestSSLHostConfigCompat` connected unrestricted where JSSE refuses the
/// handshake. Both registrars now name this function, so which one wins no
/// longer decides the behaviour.
pub fn ssl_socket_set_enabled_protocols(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Validate before anything else, with HotSpot's messages. The accepted
    // names are the protocols this stack reports through
    // `getSupportedProtocols` plus the legacy spellings JSSE still names;
    // anything else is a caller's typo, and HotSpot says so.
    let Some(Value::Object(Some(arr))) = args.get(1).copied() else {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Protocols cannot be null".into(),
        }
        .into());
    };
    const KNOWN: &[&str] = &[
        "TLSv1.3",
        "TLSv1.2",
        "TLSv1.1",
        "TLSv1",
        "SSLv3",
        "SSLv2Hello",
    ];
    for i in 0..ctx.array_length(arr) {
        let name = match ctx.get_array_element(arr, i) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        }
        .unwrap_or_default();
        if !KNOWN.contains(&name.as_str()) {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("Unsupported protocol: {name}"),
            }
            .into());
        }
    }
    let this = obj_arg(args, 0)?;
    let mut protocols: Vec<String> = Vec::new();
    if let Some(Value::Object(Some(arr))) = args.get(1) {
        let len = ctx.array_length(*arr);
        for i in 0..len {
            if let Value::Object(Some(s)) = ctx.get_array_element(*arr, i) {
                if let Some(t) = ctx.read_string(s) {
                    protocols.push(t);
                }
            }
        }
    }
    if crate::nbflags().dbg_tls_auth_ok {
        eprintln!(
            "[dbg-tls-auth] SSLSocket.setEnabledProtocols protocols={:?} probe={} \
             tls_id={} host={:?} port={}",
            protocols,
            is_probe_socket(ctx, this),
            crate::phases_late::new13_resolve_tls_id(ctx, this),
            sock_get(ctx, this).host,
            sock_get(ctx, this).port
        );
    }
    sock_set(ctx, this, |s| s.enabled_protocols = Some(protocols.clone()));
    // Probe socket (`set_huc_factory_probe_mode`): report the
    // restriction back to `http_url_connection`, which applies it to
    // the one real connection it makes.
    if is_probe_socket(ctx, this) || super::huc_factory_probe_mode() {
        record_probe_protocols(ctx, this, protocols);
        return Ok(None);
    }
    // A socket from `createSocket(Socket wrapped, ...)` has a DEFERRED
    // handshake, so a restriction set before the first I/O still
    // counts — the cipher sibling in `phases_late/ssl_security.rs`
    // already did this and the protocol setter did not.
    let tls_id = crate::phases_late::new13_resolve_tls_id(ctx, this);
    if tls_id >= crate::servlet::PENDING_LAYERED_SOCK_ID_BASE
        && tls_id < crate::servlet::RUSTLS_SOCK_ID_BASE
    {
        let pending_id = tls_id - crate::servlet::PENDING_LAYERED_SOCK_ID_BASE;
        crate::t27_tls::set_pending_layered_socket_protocols(pending_id, protocols);
        return Ok(None);
    }
    // A connect-path socket has not handshaked yet (its `connect`
    // parked the endpoint -- see `PENDING_CONNECT_SOCK_ID_BASE`), so
    // there is no established connection to tear down and redo. Accept
    // and discard, which is what this registration already did for an
    // already-handshaked socket.
    if tls_id >= crate::servlet::PENDING_CONNECT_SOCK_ID_BASE
        && tls_id < crate::servlet::PENDING_LAYERED_SOCK_ID_BASE
    {
        return Ok(None);
    }
    // Otherwise the socket came from `createSocket(host, port)`, which
    // handshakes EAGERLY — so, exactly like the `setEnabledCipherSuites`
    // sibling above, the only way to honour the JDK contract ("a
    // restriction the peer cannot satisfy makes the connection fail")
    // is to tear the connection down and redo it under the
    // restriction. That is the path `TestSSLHostConfigProtocol`'s
    // `testTlsVersionMismatchServerTls12ClientTls13` takes:
    // `TesterSupport.ClientSSLSocketFactory.reconfigureSocket` calls
    // this immediately after `createSocket` returns, and simply
    // accepting-and-discarding left the client offering both TLS
    // versions, so a deliberate version mismatch negotiated the other
    // version and SUCCEEDED where real JSSE refuses.
    let restricted = crate::t27_tls::protocol_restriction_is_real(&protocols);
    if !restricted {
        // Empty, unrecognised, or "everything we support" — the
        // caller is not actually narrowing anything, so leave the
        // established connection alone rather than pay a reconnect
        // (and risk losing semantics this reconnect cannot redo).
        return Ok(None);
    }
    // gc-common w16-a: keyed before the blocking reconnect, as in the cipher
    // sibling above.
    // Looked up, never minted (gc-common w28-a): a socket never keyed has no
    // host, and the default row returns just below.
    let Some(this_key) = existing_native_obj_key(ctx, this) else {
        return Ok(None);
    };
    let side = sock_get_key(this_key);
    let host = side.host.clone();
    if host.is_empty() || side.port <= 0 {
        return Ok(None);
    }
    let vm = ctx.vm_identity();
    let client_ident = crate::t27_tls::huc_default_client_identity_in_vm(vm);
    let cfg = match crate::t27_tls::build_socket_client_config_with_identity_ciphers_in_vm(
        vm,
        &["http/1.1"],
        client_ident.as_ref().map(|(c, k)| (c.as_str(), k.as_str())),
        None,
        None,
        &[],
        &protocols,
    ) {
        Ok(cfg) => cfg,
        Err(msg) => {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "javax/net/ssl/SSLHandshakeException",
                &msg,
            ));
        }
    };
    // T19.H1: same blocking-region requirement as the cipher
    // reconnect above — this performs real network I/O.
    ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
    let connect_result = crate::t27_tls::rustls_client_connect(cfg, &host, side.port as u16);
    ctx.end_blocking_region();
    match connect_result {
        Ok(rid) => {
            if side.stream_id >= 0 {
                let _ = crate::servlet::s2_tls_close(side.stream_id);
            }
            let new_id = crate::servlet::RUSTLS_SOCK_ID_BASE + rid;
            // gc-common w14-a (handoff-w14b-net-phase-e-rustls-reconnects-
            // claim-their-vm): the reconnected rustls stream belongs to this VM.
            crate::servlet::s2_claim_for_vm(ctx.vm_identity(), new_id);
            sock_set_key(this_key, |s| {
                s.stream_id = new_id;
            });
            Ok(None)
        }
        Err(msg) => Err(crate::phases_early::throw_jca_exc(
            ctx,
            "javax/net/ssl/SSLHandshakeException",
            &msg,
        )),
    }
}


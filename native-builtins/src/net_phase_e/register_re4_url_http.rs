#[allow(unused_imports)]
use super::*;

pub fn register_re4_url_http(r: &mut NativeMethodRegistry) {
    let url = "java/net/URL";
    register_https_session_accessors(r);

    // ---- java.net.URL(String) ------------------------------------------
    //
    // `register_net_natives` dropped this constructor when the synthetic URL
    // substitute was removed ("defers to real JDK bytecode") — but its own
    // call site in lib.rs still documents `URL.<init>(String)` as one of the
    // entry points it must keep alive, and nothing re-registered it. In
    // synthetic-JDK mode (no real `java.net.URL` bytecode) `new URL(spec)`
    // therefore died with `NoSuchMethodError`, and so did every native that
    // builds a URL through `invoke_special` (e.g.
    // `JarURLConnection.getJarFileURL`).
    //
    // A registered native ALWAYS wins over bytecode at every dispatch site, so
    // this registration must NOT shadow a real `java.net.URL`: when the class
    // has genuine bytecode we hand the call straight back to it via
    // `invoke_special_bytecode_only` (the "just run this bytecode, no native
    // check" primitive) and only parse ourselves when the class is a fabricated
    // stub.
    r.register(url, "<init>", "(Ljava/lang/String;)V", |ctx, args| {
        if !ctx.is_class_synthetic_stub("java/net/URL")
            && ctx.method_exists("java/net/URL", "<init>", "(Ljava/lang/String;)V")
        {
            return ctx.invoke_special_bytecode_only(
                "java/net/URL",
                "<init>",
                "(Ljava/lang/String;)V",
                args,
            );
        }
        let this = obj_arg(args, 0)?;
        let spec = match args.get(1) {
            Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
            _ => String::new(),
        };
        // `java.net.URL` rejects a spec with no scheme
        // (`MalformedURLException: no protocol: <spec>`); callers such as
        // Spring's `ResourceUtils.isUrl` rely on that throw to fall back to a
        // classpath/file lookup, so a silently-accepted garbage URL is worse
        // than none.
        if url_components(&spec).0.is_empty() {
            let msg = ctx.create_string(&format!("no protocol: {spec}"));
            match cratonvm_native_api::throwable_builder::construct_new(
                ctx,
                "java/net/MalformedURLException",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(msg))],
            ) {
                Ok(Some(exc)) => return Err(MethodCallFailed::ExceptionThrown(exc)),
                Err(thrown) => return Err(thrown),
                Ok(None) => {}
            }
        }
        crate::net_uri_inet::url_parse(ctx, this, &spec);
        Ok(None)
    }, NativeKind::SyntheticStub);

    // ---- component getters ---------------------------------------------
    //
    // These re-register the versions from `lib.rs::register_essential_natives`
    // (this function runs last, so it wins) with the missing slot/full-string
    // fallbacks, and add `getPort`/`getQuery`/`getRef`, which were never
    // registered at all. The by-name step and the final defaults are unchanged,
    // so a real `java.net.URL` behaves exactly as before; only the synthetic
    // layouts — whose fields have no names, making every by-name read answer
    // `Object(None)` — take the new paths.
    r.register(url, "getProtocol", "()Ljava/lang/String;", |ctx, args| {
        let Some(Value::Object(Some(this))) = args.first().copied() else {
            return Ok(Some(Value::Object(None)));
        };
        if let Value::Object(Some(s)) = ctx.get_field_by_name(this, "protocol") {
            return Ok(Some(Value::Object(Some(s))));
        }
        // `url_parse`'s synthetic layout: slot 0 is the bare scheme. The legacy
        // layout puts the WHOLE url there, which is not a scheme token — that
        // case falls through to the full-string parse below.
        if ctx.object_num_fields(this) > URLS_PROTOCOL {
            if let Value::Object(Some(s)) = ctx.get_field(this, URLS_PROTOCOL) {
                if ctx.read_string(s).is_some_and(|t| url_is_scheme_token(&t)) {
                    return Ok(Some(Value::Object(Some(s))));
                }
            }
        }
        if let Some(full) = url_raw_full_string(ctx, this) {
            let scheme = url_components(&full).0;
            if !scheme.is_empty() {
                let s = ctx.create_string(&scheme);
                return Ok(Some(Value::Object(Some(s))));
            }
        }
        Ok(Some(Value::Object(Some(ctx.create_string("file")))))
    }, NativeKind::SyntheticStub);
    r.register(url, "getHost", "()Ljava/lang/String;", |ctx, args| {
        let Some(Value::Object(Some(this))) = args.first().copied() else {
            return Ok(Some(Value::Object(None)));
        };
        if let Some(s) = url_component_ref(ctx, this, "host", URLS_HOST) {
            return Ok(Some(Value::Object(Some(s))));
        }
        // `URL.getHost()` is "" (never null) for a host-less URL such as
        // `file:/tmp/x`, matching the real JDK.
        let host = url_raw_full_string(ctx, this)
            .map(|full| url_components(&full).1)
            .unwrap_or_default();
        Ok(Some(Value::Object(Some(ctx.create_string(&host)))))
    }, NativeKind::SyntheticStub);
    r.register(url, "getPort", "()I", |ctx, args| {
        let Some(Value::Object(Some(this))) = args.first().copied() else {
            return Ok(Some(Value::Int(-1)));
        };
        if let Value::Int(p) = ctx.get_field_by_name(this, "port") {
            return Ok(Some(Value::Int(p)));
        }
        if ctx.object_num_fields(this) > URLS_PORT {
            if let Value::Int(p) = ctx.get_field(this, URLS_PORT) {
                return Ok(Some(Value::Int(p)));
            }
        }
        let port = url_raw_full_string(ctx, this)
            .map(|full| url_components(&full).2)
            .unwrap_or(-1);
        Ok(Some(Value::Int(port)))
    }, NativeKind::SyntheticStub);
    r.register(url, "getQuery", "()Ljava/lang/String;", |ctx, args| {
        let Some(Value::Object(Some(this))) = args.first().copied() else {
            return Ok(Some(Value::Object(None)));
        };
        if let Some(s) = url_component_ref(ctx, this, "query", URLS_QUERY) {
            return Ok(Some(Value::Object(Some(s))));
        }
        // Absent query is `null`, per `java.net.URL.getQuery`.
        match url_raw_full_string(ctx, this).and_then(|full| url_components(&full).4) {
            Some(q) => Ok(Some(Value::Object(Some(ctx.create_string(&q))))),
            None => Ok(Some(Value::Object(None))),
        }
    }, NativeKind::SyntheticStub);
    r.register(url, "getRef", "()Ljava/lang/String;", |ctx, args| {
        let Some(Value::Object(Some(this))) = args.first().copied() else {
            return Ok(Some(Value::Object(None)));
        };
        if let Value::Object(Some(s)) = ctx.get_field_by_name(this, "ref") {
            return Ok(Some(Value::Object(Some(s))));
        }
        // No synthetic slot carries the fragment; recover it from the cached
        // full URL string.
        match url_raw_full_string(ctx, this).and_then(|full| url_components(&full).5) {
            Some(f) => Ok(Some(Value::Object(Some(ctx.create_string(&f))))),
            None => Ok(Some(Value::Object(None))),
        }
    }, NativeKind::SyntheticStub);

    // Our getResources / Class.getProtectionDomain overrides return URL
    // objects that never run java.net.URL.<init>, so the real-JDK
    // toString()/toExternalForm() NPE when they invoke the (null)
    // URLStreamHandler. We build the external form ourselves.
    //
    // Two layouts must be supported:
    //   * Synthetic-mode 6-field URL where field 5 holds the full URL string
    //     (populated by our `url_parse` constructor).
    //   * Real-JDK 13-field URL where field 0=protocol, 1=host, 2=port (int),
    //     3=file, 4=query, 5=authority, 6=path, 8=ref, 10=handler. The full
    //     external form is `protocol:[//host[:port]]file[#ref]` per
    //     java.net.URL.toString.
    //
    // We try the synthetic field-5 fast path first (works whenever url_parse
    // ran), then the real-JDK reconstruction (matches HotSpot output for
    // synthetic URLs allocated by Class.getProtectionDomain).
    let url_to_string = |ctx: &mut dyn NativeContext, args: &[Value]| -> MethodCallResult {
        let this = obj_arg(args, 0)?;
        // Synthetic fast path: a non-empty field-5 string is the cached
        // full URL written by url_parse.  We treat any string containing
        // ":" as a full URL so we don't mistake the real-JDK `authority`
        // slot for a synthetic full-URL cache.
        // G75-1 N2: this whole reconstruction used to end in
        // `create_string(&out)` over a Rust `String`, which cannot hold an
        // unpaired UTF-16 surrogate — so `new URL(s).toString()` could not give
        // back the `s` it was handed, while `getPath()`/`getFile()` were exact
        // (under `--real-jdk` the real JDK constructor fills the component
        // fields and only this cache goes through us). The STRUCTURE below is
        // unchanged and still reasons over host text — scheme, host and port
        // are ASCII by JVMS and by URI syntax. Only the OUTPUT is assembled in
        // units, and only `file` and `ref` are read as units, because they are
        // the two components that can carry arbitrary text.
        let synth_full_obj = match ctx.get_field(this, 5) {
            Value::Object(Some(o)) => Some(o),
            _ => None,
        };
        let synth_full = synth_full_obj.and_then(|o| ctx.read_string(o));
        if let Some(ref s) = synth_full {
            // Only treat field 5 as a full-URL cache when it actually carries a
            // scheme — a real java.net.URL's field 5 is the `authority`
            // (`host:port`), which must fall through to the reconstruction below
            // instead of being returned as the whole URL. (BUG: openStream then
            // saw `unsupported scheme: localhost:<port>`.)
            if field5_is_full_url(s) {
                // Hand back the STORED OBJECT, not a re-encoding of it: exact
                // by construction, and one allocation cheaper.
                if let Some(o) = synth_full_obj {
                    return Ok(Some(Value::Object(Some(o))));
                }
            }
        }
        // Real-JDK reconstruction: protocol:[//host[:port]]file[#ref].
        let proto = match ctx.get_field(this, 0) {
            Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
            _ => String::new(),
        };
        let host = match ctx.get_field(this, 1) {
            Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
            _ => String::new(),
        };
        let port = match ctx.get_field(this, 2) {
            Value::Int(i) => i,
            _ => -1,
        };
        let file = match ctx.get_field(this, 3) {
            Value::Object(Some(o)) => ctx.read_string_units(o).unwrap_or_default(),
            _ => Vec::new(),
        };
        let ref_str = match ctx.get_field(this, 8) {
            Value::Object(Some(o)) => ctx.read_string_units(o),
            _ => None,
        };
        /// Append an ASCII/BMP literal to a UTF-16 buffer.
        fn pu(buf: &mut Vec<u16>, text: &str) {
            buf.extend(text.encode_utf16());
        }
        let mut out: Vec<u16> = Vec::new();
        if !proto.is_empty() {
            pu(&mut out, &proto);
            out.push(u16::from(b':'));
        }
        // Real java.net.URL.toExternalForm emits `protocol://authority` where
        // the authority (field 5) may carry user-info
        // (`alice:secret@localhost:8080`). Prefer it over the bare host:port
        // reconstruction so user-info survives (ResourceTests.
        // useUserInfoToSetBasicAuth needs the native HTTP client to see it and
        // send preemptive Basic auth). Guard: only when the parsed host is
        // non-empty and embedded in the field-5 string, so a synthetic 6-field
        // URL's unrelated slot never leaks in. Reaching here already implies
        // `field5_is_full_url(field 5)` was false (the fast path above
        // returned otherwise), i.e. field 5 is authority-shaped or empty.
        let authority_from_field5 = match &synth_full {
            Some(s) if !host.is_empty() && s.contains(&host) => Some(s.as_str()),
            _ => None,
        };
        if let Some(auth) = authority_from_field5 {
            pu(&mut out, "//");
            pu(&mut out, auth);
        } else if !host.is_empty() || port >= 0 {
            pu(&mut out, "//");
            pu(&mut out, &host);
            if port >= 0 {
                out.push(u16::from(b':'));
                pu(&mut out, &port.to_string());
            }
        }
        out.extend_from_slice(&file);
        if let Some(r) = ref_str {
            out.push(u16::from(b'#'));
            out.extend_from_slice(&r);
        }
        // Ultimate fallback: if we built nothing useful, fall back to the
        // legacy field-0 read for synthetic-mode URLs that stored the
        // full path at slot 0.
        if out.is_empty() || out == [u16::from(b':')] {
            if synth_full.is_some() {
                if let Some(o) = synth_full_obj {
                    return Ok(Some(Value::Object(Some(o))));
                }
            }
        }
        Ok(Some(Value::Object(Some(
            ctx.create_string_from_units(&out),
        ))))
    };
    r.register(url, "toString", "()Ljava/lang/String;", url_to_string, NativeKind::SyntheticStub);
    r.register(url, "toExternalForm", "()Ljava/lang/String;", url_to_string, NativeKind::SyntheticStub);

    // URL.toURI() — JDK 21+ URLClassPath calls url.toURI() when setting up
    // the classpath for LaunchedURLClassLoader. The method is absent from
    // CratonVM's synthetic URL type, so we provide it here. Strategy:
    //   1. Get the full URL string via our toString native.
    //   2. Allocate a real java/net/URI and run its (String) constructor so
    //      getScheme/getPath/toString all return correct answers via JDK bytecode.
    //   3. If the real constructor fails (e.g. URISyntaxException from a
    //      jar:file:…!/… URL), fall back to a 7-field synthetic URI whose
    //      raw-string field is set so our uri_string helper can reconstruct
    //      the string, and whose scheme field holds the scheme prefix.
    r.register(url, "toURI", "()Ljava/net/URI;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Obtain the full URL string.
        let url_str_obj = match ctx.invoke(
            "java/net/URL",
            "toString",
            "()Ljava/lang/String;",
            &[Value::Object(Some(this))],
        ) {
            Ok(Some(Value::Object(Some(s)))) => s,
            _ => return Ok(Some(Value::Object(None))),
        };
        let url_str = ctx.read_string(url_str_obj).unwrap_or_default();

        // `URL.toURI()` is `new URI(toString())`, so it owes that
        // constructor's refusals — and this door PUBLISHED the text into a
        // URI carrier without ever parsing it, so `new URL("http://h/a b")
        // .toURI()` handed back a URI whose own constructor rejects its text
        // (`Illegal character in path at index 10`). Three `L6UrlSweep` rows,
        // and the same door Spring reaches when it catches
        // `URISyntaxException` to fall back to a string path.
        //
        // Only the CHECK is shared with the constructor; the publish below
        // stays, because the `jar:`/`nested:` fallback it exists for is
        // load-bearing for Spring and Tomcat (see the retirement record's
        // item 3).
        if let Some(fail) = crate::uri_parse_fail(&url_str) {
            if let Some(exc) = crate::uri_syntax_exception_pub(ctx, &url_str, &fail) {
                return Err(exc);
            }
        }

        // `--real-jdk`: `new URI(toString())` for real, over the very String
        // object `toString()` returned, which is the JDK body's first line.
        // The publish below wrote `decodedPath` from a lossy `read_string`
        // and then re-derived only `path` from units, so once the `URI`
        // readers retired, `url.toURI().getPath()` with a lone surrogate read
        // U+FFFD (`RJdkBridge1` `surrog`, under the SBR wave-1 dial).
        if let Some(uri) = jdk_only_constructed_uri(ctx, url_str_obj) {
            return Ok(Some(Value::Object(Some(uri))));
        }

        // Always build a synthetic URI. Returning a real-JDK URI here leaves
        // our `URI.getSchemeSpecificPart()` override without access to the raw
        // string (layout differs), which can degrade to empty SSP and break
        // Spring Boot's `new File(url.toURI().getSchemeSpecificPart())` path.
        // Layout per http2.rs:
        //   scheme=0, host=1, port=2, path=3, query=4, fragment=5, raw=6
        //
        // gc-common w19-d handoff: the URL's `toString()` result and the URI
        // are rooted across the URI's allocation, every component String and
        // `uri_publish_named` (which pins the URI only inside itself), and
        // every store and the return go through their current addresses. Both
        // used to be held raw across all of them.
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let str_h = scope.root(url_str_obj);
        let uri = try_alloc_concurrent_synthetic(&mut *scope, "java/net/URI", 7)?;
        let uri_h = scope.root(uri);
        // JDK-ONLY-LAYOUT: raw slots only on OUR layout. On a real
        // `java.net.URI` slot 6 is `path` and slot 3 is `userInfo`, so the full
        // text went into the path and the path into the user information.
        if uri_has_synthetic_layout(&*scope, uri) {
            // G75-1 N1: store the STRING OBJECT `toString()` returned, not a
            // re-encoding of it. `url_str` came through `read_string` and
            // cannot hold an unpaired surrogate, so `url.toURI().getPath()`
            // lost the unit that `url.toString()` had just preserved. Slot 6 is
            // the raw-string slot every URI accessor reads first, so getting
            // this one reference right fixes the whole derived family at once.
            let url_str_now = scope.get(&str_h);
            scope.set_field(uri, 6, Value::Object(Some(url_str_now))); // raw
            if let Some(colon) = url_str.find(':') {
                let scheme = &url_str[..colon];
                let scheme_s = scope.create_string(scheme);
                let uri_now = scope.get(&uri_h);
                scope.set_field(uri_now, 0, Value::Object(Some(scheme_s)));
                // For file: URIs, the path is everything after "file:".
                if scheme == "file" {
                    let path = &url_str[colon + 1..];
                    let path_s = scope.create_string(path);
                    let uri_now = scope.get(&uri_h);
                    scope.set_field(uri_now, 3, Value::Object(Some(path_s)));
                }
            }
        }
        // The file: path override preserves this site's own rule: everything
        // after `file:` is the path, including a Windows `/C:/…` form that the
        // generic split would treat as an opaque scheme-specific part.
        let file_path = url_str
            .strip_prefix("file:")
            .filter(|p| !p.is_empty())
            .map(str::to_string);
        let uri_now = scope.get(&uri_h);
        uri_publish_named(&mut *scope, uri_now, &url_str, file_path.as_deref());
        // G75-1 N1: `uri_publish_named` takes `&str`, so every field it wrote
        // came from a `read_string` of the URL's own `toString()` — which had
        // just been fixed to preserve an unpaired surrogate, only for this to
        // drop it again one call later. Restore the raw STRING OBJECT and
        // re-derive the three text-carrying components from its units, with the
        // same splitter the accessors and the URI constructor use.
        //
        // Skipped when a `path_override` was supplied: that argument exists to
        // overrule the parse (the `file:` cases above), and this correction
        // must not overrule it back.
        if file_path.is_none() {
            let url_str_now = scope.get(&str_h);
            let raw_units = scope.read_string_units(url_str_now).unwrap_or_default();
            if !raw_units.is_empty() {
                let uri_now = scope.get(&uri_h);
                scope.set_field_by_name(uri_now, "string", Value::Object(Some(url_str_now)));
                if let Some(pu) = uri_select_raw_path_units(&raw_units) {
                    let o = scope.create_string_from_units(&pu);
                    let uri_now = scope.get(&uri_h);
                    scope.set_field_by_name(uri_now, "path", Value::Object(Some(o)));
                }
                if let Some(q) = uri_query_units(&raw_units) {
                    let o = scope.create_string_from_units(&q);
                    let uri_now = scope.get(&uri_h);
                    scope.set_field_by_name(uri_now, "query", Value::Object(Some(o)));
                }
                if let Some(f) = uri_fragment_units(&raw_units) {
                    let o = scope.create_string_from_units(&f);
                    let uri_now = scope.get(&uri_h);
                    scope.set_field_by_name(uri_now, "fragment", Value::Object(Some(o)));
                }
            }
        }
        Ok(Some(Value::Object(Some(scope.get(&uri_h)))))
    }, NativeKind::SyntheticStub);

    // S111r18: URL.getDefaultPort() — JDK 25 reads `handler.getDefaultPort()`
    // (URL.java:1015) which NPEs because synthetic URLs (alloc_url, real-JDK
    // URL ctors) never populate the `handler` field. Per-protocol defaults
    // match what URLStreamHandler subclasses report; this bypasses the null
    // handler deref entirely. Spring's CandidateComponentsIndexLoader →
    // LaunchedURLClassLoader$UseFastConnectionExceptionsEnumeration →
    // URLClassPath.getLoader → URLUtil.urlNoFragString → getDefaultPort
    // chain (scag-auth + sister jars) hit this NPE.
    r.register(url, "getDefaultPort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Read the protocol field (slot 0 in our synthetic URL layout).
        let protocol = read_field_string_or(ctx, this, 0, "");
        // file/jar/jrt/classpath/nested/etc. → -1 per JDK URLStreamHandler.
        // One table, shared with `URL.equals`/`sameFile`/`hashCode`, which
        // substitute the same default for an absent port.
        Ok(Some(Value::Int(crate::url_default_port(&protocol))))
    }, NativeKind::SyntheticStub);

    r.register(url, "openStream", "()Ljava/io/InputStream;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // `--real-jdk`: a URL that carries an APPLICATION handler is served by
        // the JDK's own body, `openConnection().getInputStream()`, and so by
        // that handler, exactly as on HotSpot. Everything below reads the
        // URL's TEXT and ignores its handler, which is only a stand-in for the
        // JDK's built-in handlers. For Spring Boot's
        // `org.springframework.boot.loader.net.protocol.jar.Handler` it
        // misread `jar:nested:/app.jar/!BOOT-INF/classes/!/x.class`, whose
        // `BOOT-INF/classes/` is a DIRECTORY of the outer jar, as a nested jar
        // ("Could not find EOCD"), so `java -jar` of a Spring Boot 3 fat jar
        // died at its first component scan. See
        // `fatjar-nested-classes-directory-url-openstream-FIXED-20260924.md`.
        //
        // Only application handlers: the `openConnection` native returns the
        // handler's own connection for them, so the real body cannot come
        // back here. For a built-in or absent handler it hands out a carrier
        // whose `getInputStream` calls `openStream` again.
        //
        // `--compatible` takes the application-handler arm too, under
        // `CRATONVM_COMPAT_URL_APP_HANDLER` (`url_app_handler_is_authoritative`;
        // interpreter round i1 wave 49, lane L6): the old "compatible keeps
        // the text reader" policy is gone, and the same fat jar died there
        // (`interpreter-L6-compatible-fat-jar-reads-boot-inf-classes-as-a-nested-jar-FIXED-20261013.md`).
        //
        // A `file:` URL with a handler takes the same road under `--real-jdk`:
        // `openConnection` hands it the JDK's real `FileURLConnection`
        // (below), never a carrier, so HotSpot's bytes come back, including a
        // directory's `Collator`-sorted listing, which the text reader only
        // approximated.
        if ctx.url_app_handler_is_authoritative()
            && (url_app_handler(ctx, this).is_some()
                || (ctx.jdk_url_class_loaders() && url_is_handled_file(ctx, this)))
        {
            // gci i18/vp: a tail call (`continuation_pin::tail_call`) of the
            // same descriptor. The body runs the application handler's
            // connection, which may park (a lock, a socket), as it unmounts
            // a virtual thread on HotSpot.
            return cratonvm_native_api::continuation_pin::tail_call(|| {
                ctx.invoke_special_bytecode_only(
                    "java/net/URL",
                    "openStream",
                    "()Ljava/io/InputStream;",
                    args,
                )
            });
        }
        // A `jar:file:` URL with the JDK's own `jar` handler: the JDK body,
        // whose `openConnection` (the native above) answers the handler's
        // `JarURLConnection`, so the stream is its `JarURLInputStream`
        // (`jdk_jar_file_handler`; the `JdkJarUrlConnections` row).
        if jdk_jar_file_handler(ctx, this).is_some() {
            ctx.note_alignment_divergence(cratonvm_types::HotSpotAlignment::JdkJarUrlConnections);
            // gci i18/vp: a tail call, as above (a `jar:` URL over a remote
            // jar downloads it).
            return cratonvm_native_api::continuation_pin::tail_call(|| {
                ctx.invoke_special_bytecode_only(
                    "java/net/URL",
                    "openStream",
                    "()Ljava/io/InputStream;",
                    args,
                )
            });
        }
        // Prefer our synthetic "full URL" slot (field 5). For real-JDK URLs
        // field 0 holds only the protocol (e.g. "jar") — a value too short
        // to be a usable full URL — so fall through to toExternalForm which
        // reconstructs the full string (protocol:[//host[:port]]file[#ref]).
        // Round 76: SportMe's SpringFactoriesLoader builds UrlResource around
        // real-JDK URL instances whose slot 5 is empty; reading slot 0 alone
        // yielded "jar" and tripped the unsupported-scheme branch below.
        let mut url_str = read_field_string_or(ctx, this, 5, "");
        if !field5_is_full_url(&url_str) {
            // Empty, just a protocol, or a real-JDK `authority` (host:port) in
            // field 5 — ask the URL for its full form (toExternalForm now
            // reconstructs protocol://authority/path for real URLs).
            if let Ok(Some(Value::Object(Some(s)))) = ctx.invoke(
                "java/net/URL",
                "toExternalForm",
                "()Ljava/lang/String;",
                &[Value::Object(Some(this))],
            ) {
                let ext = ctx.read_string(s).unwrap_or_default();
                if ext.contains(':') {
                    url_str = ext;
                }
            }
        }
        if !url_str.contains(':') {
            // Last-resort legacy synthetic fallback to slot 0.
            let s0 = read_field_string_or(ctx, this, 0, "");
            if s0.contains(':') {
                url_str = s0;
            }
        }
        if url_str.is_empty() {
            return Err(ioex("URL.openStream: empty URL"));
        }

        // S111r23-DBG: log spring.factories URL openStream calls
        let is_sf = url_str.contains("spring.factories");
        if is_sf && spring_dbg_enabled() {
            eprintln!("[OSTR-DBG] URL.openStream: {}", url_str);
        }

        // `JarUrl` exposes Spring Boot nested archives as
        // `jar:nested:/outer.jar/!inner.jar!/entry`. Preserve that external
        // spelling for URL identity, but route byte access through the proven
        // `jar:file:` nested-archive reader below: after the protocol prefix,
        // both forms carry the identical outer-path / inner-entry grammar.
        // Spring Boot spells only the outer boundary as `/!inner`; the generic
        // reader spells that same boundary `!/inner`, while preserving further
        // `!/` archive/resource separators unchanged.
        //
        // The `nested:` entry may also be a DIRECTORY of the outer jar
        // (`BOOT-INF/classes/`); `nested_url_as_jar_file` flattens that case
        // to a single-level lookup instead of opening the directory as a jar.
        if let Some(rest) = url_str.strip_prefix("jar:nested:") {
            url_str = nested_url_as_jar_file(rest);
        }

        // Resolve the URL to raw bytes. Handles file:, jar:file:!/, normalized
        // jar:nested:!/, and
        // classpath: schemes locally; http(s): goes through the HTTP client.
        let bytes: Vec<u8> = if let Some(rest) = url_str.strip_prefix("jar:file:") {
            // jar:file:/path/to.jar!/entry  (single-level)
            // jar:file:/fat.jar!/BOOT-INF/lib/inner.jar!/entry  (double-nested)
            //   — Spring Boot 2.x fat JARs store nested JARs as BOOT-INF/lib/*.jar;
            //     ClassLoader.getResources() produces double-nested URLs. We must
            //     read the outer JAR, extract the inner JAR bytes, then look up
            //     the entry in the inner archive.
            // Bug 2 fix: cache outer JAR bytes + nested JAR bytes so the
            // O(N) autoconfig walk doesn't become O(N²) on disk reads.
            //
            // `file:` URLs prefix a leading `/` before a Windows drive letter
            // (`/C:/…`), which must be stripped for `std::fs`/`Path` to resolve
            // the path on Windows. On POSIX that same leading `/` is the path's
            // own root and must be KEPT — unconditionally trimming every
            // leading slash turned `/home/user/foo.jar` into the relative
            // `home/user/foo.jar`, which resolved (if at all) against the
            // wrong CWD and failed with ENOENT on every Linux/macOS host. Try
            // the trimmed form first, then the raw form — mirrors the
            // existence-check fallback already used by `jar_url_entry_size`
            // and `JarURLConnection.getJarFile` below for the same ambiguity.
            //
            // The existence probe must run on the PERCENT-DECODED jar path.
            // Probing the raw URL form made every percent-escaped path fail the
            // check — a directory literally named `dir with spaces` appears in
            // the URL as `dir%20with%20spaces`, which never `exists()` — so the
            // code fell through to `raw_rest` and handed Windows the
            // drive-letter path with its `file:` leading slash still attached
            // (`/C:/…`), i.e. `os error 123` ("The filename, directory name, or
            // volume label syntax is incorrect"). That is the residual half of
            // the `%20` decoding fix: decoding was added below at
            // `outer_jar_raw`, but the slash-trimming decision above still
            // looked at the encoded string. (`TestDeployTask.bug58086a`.)
            let raw_rest = rest;
            let trimmed_rest = rest.trim_start_matches('/');
            let exists_decoded = |p: &str| {
                let jar_part = p.split("!/").next().unwrap_or(p);
                std::path::Path::new(uri_percent_decode(jar_part).as_str()).exists()
            };
            let rest = if exists_decoded(trimmed_rest) {
                trimmed_rest
            } else if exists_decoded(raw_rest) {
                raw_rest
            } else if is_windows_drive_path(trimmed_rest) {
                // Neither probe found the file (it may legitimately not exist
                // yet, or live inside a WAR). A `X:/…` path is unusable on
                // Windows with the leading slash still on it, so prefer the
                // trimmed form and let the real open surface a proper
                // FileNotFound rather than a syntax error.
                trimmed_rest
            } else {
                raw_rest
            };
            let (outer_jar_raw, inner_path) = match rest.find("!/") {
                Some(i) => (&rest[..i], &rest[i + 2..]),
                None => {
                    return Err(ioex(format!(
                        "URL.openStream: malformed jar URL: {url_str}"
                    )))
                }
            };
            // HotSpot's JarURLConnection percent-decodes the outer jar's own
            // URL component (`ParseUtil.decode`) before touching disk, so a
            // `%20` resolves back to a literal space — e.g. a directory
            // literally named "dir with spaces" is addressed as
            // `dir%20with%20spaces` in the URL. `outer_jar` previously stayed
            // encoded, so `std::fs`/`zip` lookups for any percent-escaped jar
            // path always missed with ENOENT even though the file visibly
            // exists (TestDeployTask.bug58086a). The entry name after `!/` is
            // a raw zip entry name, not a URL component, and must NOT be
            // decoded.
            let outer_jar_owned = uri_percent_decode(outer_jar_raw);
            let outer_jar = outer_jar_owned.as_str();
            // Check if the inner_path itself is a nested jar entry (double !/):
            // e.g. "BOOT-INF/lib/spring-boot-2.7.12.jar!/META-INF/spring.factories"
            let buf = if let Some(second_sep) = inner_path.find("!/") {
                let nested_jar_entry = &inner_path[..second_sep];
                let resource_entry = &inner_path[second_sep + 2..];
                use std::io::Read;
                let nested_jar_bytes =
                    cached_nested_jar(outer_jar, nested_jar_entry).map_err(|e| {
                        ioex(format!(
                            "URL.openStream: nested jar {nested_jar_entry} in {outer_jar}: {e}"
                        ))
                    })?;
                let inner_cursor = std::io::Cursor::new(nested_jar_bytes.as_slice());
                let mut inner_zip = zip::ZipArchive::new(inner_cursor).map_err(|e| {
                    ioex(format!(
                        "URL.openStream: open inner jar {nested_jar_entry}: {e}"
                    ))
                })?;
                let mut entry_file = inner_zip
                    .by_name(resource_entry)
                    .map_err(|e| zip_entry_err(resource_entry, nested_jar_entry, e))?;
                let mut buf = Vec::with_capacity(entry_file.size().min(1 << 27) as usize);
                entry_file.read_to_end(&mut buf).map_err(|e| {
                    ioex(format!("URL.openStream: read entry {resource_entry}: {e}"))
                })?;
                buf
            } else {
                // Single-level: jar:file:/path/to.jar!/entry
                use std::io::Read;
                let jar_bytes = cached_outer_jar(outer_jar)
                    .map_err(|e| ioex(format!("URL.openStream: read jar {outer_jar}: {e}")))?;
                let cursor = std::io::Cursor::new(jar_bytes.as_slice());
                let mut zip = zip::ZipArchive::new(cursor)
                    .map_err(|e| ioex(format!("URL.openStream: open jar {outer_jar}: {e}")))?;
                // A `.jmod` archive stores its class/resource entries under a
                // `classes/` prefix, but a `getResource` URL into a jmod omits
                // it (e.g. `…/java.base.jmod!/java/lang/Object.class`). Resolve
                // to the prefixed entry, mirroring `find_resource`'s jmod path.
                // Without this, Hibernate's Jandex indexer
                // (`SourceModelTestHelper.buildJandexIndex`, which reads JDK
                // baseline types out of `java.base.jmod`) failed with
                // "entry java/lang/Object.class … not found in archive"; it
                // also made `ClassLoader.getResourceAsStream("java/lang/...")`
                // null, blocking ecj/Jasper JSP type resolution (bug 10).
                let lookup: std::borrow::Cow<str> =
                    if outer_jar.ends_with(".jmod") && !inner_path.starts_with("classes/") {
                        std::borrow::Cow::Owned(format!("classes/{inner_path}"))
                    } else {
                        std::borrow::Cow::Borrowed(inner_path)
                    };
                let mut entry_file = zip
                    .by_name(&lookup)
                    .map_err(|e| zip_entry_err(inner_path, outer_jar, e))?;
                let mut buf = Vec::with_capacity(entry_file.size().min(1 << 27) as usize);
                entry_file
                    .read_to_end(&mut buf)
                    .map_err(|e| ioex(format!("URL.openStream: read entry {inner_path}: {e}")))?;
                buf
            };
            buf
        } else if let Some(raw_rest) = url_str.strip_prefix("file:") {
            // HotSpot's `sun.net.www.protocol.file.FileURLConnection` decodes
            // the URL's raw (percent-escaped) file component via
            // `ParseUtil.decode(url.getFile())` before touching the
            // filesystem, so a `file:` URL like
            // `file:/…/foo%20with%20spaces.css` (produced by
            // `UrlResource`/`UriUtils.encodePath`) resolves to the file
            // literally named "foo with spaces.css". We previously used the
            // still-encoded `raw_rest`/`path` verbatim, so `std::fs::read`
            // looked for a path containing a literal "%20" — always missing
            // — and any caller expecting content (e.g.
            // `ResourceHttpRequestHandlerIntegrationTests
            // .classpathLocationWithEncodedPath`, `pathPrefix="/url"`) got an
            // empty/404 response instead of the file's bytes. Decode once,
            // up front, the same way URI handling elsewhere in this file
            // does (`uri_percent_decode`).
            let rest_owned = uri_percent_decode(raw_rest);
            let rest = rest_owned.as_str();
            let path = rest.trim_start_matches('/');
            // On Windows, MSYS/Cygwin-style `/c/...` (URL `file:/c/...`)
            // needs the drive-letter colon reinjected: `c/foo` → `c:/foo`.
            // ActiveMQ's XBeanBrokerFactory builds the activemq.xml URL
            // from `-Dactivemq.conf=/c/.../conf`; without this rewrite the
            // broker start fails with "Системе не удается найти указанный
            // путь" (cannot find the specified path) at config load.
            #[cfg(windows)]
            let win_path = {
                let bytes = path.as_bytes();
                if bytes.len() >= 2
                    && bytes[0].is_ascii_alphabetic()
                    && (bytes[1] == b'/' || bytes[1] == b'\\')
                {
                    let mut s = String::with_capacity(path.len() + 1);
                    s.push(bytes[0] as char);
                    s.push(':');
                    s.push_str(&path[1..]);
                    Some(s)
                } else {
                    None
                }
            };
            #[cfg(not(windows))]
            let win_path: Option<String> = None;
            let try_paths: Vec<String> = match &win_path {
                Some(w) => vec![w.clone(), path.to_string(), rest.to_string()],
                None => vec![path.to_string(), rest.to_string()],
            };
            // Keep the FIRST (most-canonical) path's error: `try_paths` is
            // ordered [win_path?, path, rest], and the least-canonical `rest`
            // form (`/C:/...` on Windows) yields a spurious ERROR_INVALID_NAME
            // (os error 123) that would otherwise mask the real not-found error
            // from the canonical `C:/...` path.
            let mut first_err: Option<std::io::Error> = None;
            let mut result: Option<Vec<u8>> = None;
            for p in &try_paths {
                match std::fs::read(p) {
                    Ok(b) => {
                        result = Some(b);
                        break;
                    }
                    Err(e) => {
                        if first_err.is_none() {
                            first_err = Some(e);
                        }
                    }
                }
            }
            match result {
                Some(b) => b,
                None => {
                    // A `file:` URL pointing at a DIRECTORY: HotSpot's
                    // `sun.net.www.protocol.file.FileURLConnection.getInputStream`
                    // does NOT fail — it returns a directory LISTING (the entry
                    // names, Collator-sorted, one per `\n`-terminated line) as a
                    // `ByteArrayInputStream`. `std::fs::read` on a directory
                    // errors instead (Windows: os error 5 "access denied"), so
                    // detect the directory and synthesize the same listing.
                    // (TestConfigFileLoader.test03: file: URL to
                    // test/webresources/dir1 expects a non-null stream.)
                    if let Some(d) = try_paths.iter().find(|p| std::path::Path::new(p).is_dir()) {
                        match std::fs::read_dir(d) {
                            Ok(entries) => {
                                let mut names: Vec<String> = entries
                                    .filter_map(|e| e.ok())
                                    .filter_map(|e| e.file_name().into_string().ok())
                                    .collect();
                                // HotSpot uses `Collator.getInstance()` (default
                                // locale, case-insensitive primary order). Mirror
                                // it closely with a case-insensitive sort, ties
                                // broken by the raw name for determinism.
                                names.sort_by(|a, b| {
                                    a.to_lowercase()
                                        .cmp(&b.to_lowercase())
                                        .then_with(|| a.cmp(b))
                                });
                                let mut listing = String::new();
                                for n in &names {
                                    listing.push_str(n);
                                    listing.push('\n');
                                }
                                listing.into_bytes()
                            }
                            Err(e) => return Err(fnfex(format!("{path} ({e})"))),
                        }
                    } else {
                        let e = first_err.unwrap_or_else(|| std::io::Error::other("no path tried"));
                        // The JDK's `file:` URL stream (sun.net.www.protocol.file
                        // FileURLConnection → FileInputStream) raises
                        // FileNotFoundException — not a bare IOException — when the
                        // target can't be opened (missing file or permission).
                        // Callers assert on it specifically, e.g. Spring Boot's
                        // PluginXmlParser via
                        // `withCauseInstanceOf(FileNotFoundException.class)`. See
                        // `apps/spring-boot/cratonvm-bug-reports/SB-12`.
                        return Err(fnfex(format!("{path} ({e})")));
                    }
                }
            }
        } else if let Some(name) = url_str.strip_prefix("classpath:") {
            let name = name.trim_start_matches('/');
            // Static app/boot/ext classpath first — preserves every case that
            // already worked (e.g. catalina.jar's mbeans-descriptors.xml).
            if let Some(b) = ctx.find_resource(name) {
                b
            } else if let Some(stream) = classpath_resource_via_context_loader(ctx, name) {
                // Resource lives behind the thread context classloader (a
                // deployed webapp's WEB-INF/classes), which `find_resource`
                // can't see. The real `ClasspathURLStreamHandler` resolves it
                // via `TCCL.getResource`; return the loader's own InputStream
                // directly. (TestPropertiesRoleMappingListener
                // testFileFromClasspath*: classpath:com/example/role-mapping.properties.)
                return Ok(Some(Value::Object(Some(stream))));
            } else {
                // Tomcat's real `ClasspathURLStreamHandler.openConnection`
                // throws `FileNotFoundException` (a subclass of IOException)
                // when neither the TCCL nor the handler's own loader resolves
                // the resource. Callers assert on that specific type — e.g.
                // `TestConfigFileLoader.test02` is `@Test(expected =
                // FileNotFoundException.class)` for `classpath:.../foo`. Mirror
                // the `file:` arm above (which already uses `fnfex`) so a
                // missing classpath resource raises FNFE, not a bare IOException.
                return Err(fnfex(format!(
                    "URL.openStream: classpath resource not found: {name}"
                )));
            }
        } else if let Some(name) = url_str.strip_prefix("resource:") {
            // Synthetic `resource:/<name>` URLs are produced by
            // `Class.getResource(String)` in `lang_class.rs` for resources
            // that the classloader knows about but for which we don't have
            // a real `file:` or `jar:` URL. Felix's
            // `Util.loadDefaultProperties` does `Util.class.getResource(...).
            // openConnection().getInputStream()`, which arrives here via
            // `URLConnection.getInputStream` → `URL.openStream` and used to
            // hit the unsupported-scheme arm, leaving Felix's static
            // `DEFAULTS` field null and tripping a downstream NPE on
            // `DEFAULTS.isEmpty()`.
            //
            // Spring's in-memory compiler deliberately uses the same
            // `resource:` spelling for URLs backed by an application-provided
            // URLStreamHandler. Prefer that handler when present: generated
            // annotation-processor outputs live only in its heap-resident
            // DynamicResourceFileObject and cannot be found through the VM's
            // static classpath resource index. Craton-synthesized resource
            // URLs have no handler and continue through the existing lookup.
            if let Some(conn) = url_custom_handler_connection(ctx, this)? {
                match conn {
                    Value::Object(Some(conn)) => {
                        let stream = ctx.invoke_virtual(
                            conn,
                            "getInputStream",
                            "()Ljava/io/InputStream;",
                            &[],
                        )?;
                        return Ok(Some(stream.unwrap_or(Value::Object(None))));
                    }
                    _ => return Ok(Some(Value::Object(None))),
                }
            }
            let name = name.trim_start_matches('/');
            ctx.find_resource(name)
                .ok_or_else(|| ioex(format!("URL.openStream: resource not found: {name}")))?
        } else if let Some(rest) = url_str.strip_prefix("jrt:") {
            // JEP 220 runtime-image URL: `jrt:/<module>/<resource>`. CratonVM
            // emits these from `find_all_resource_urls` when the boot classpath
            // is a jimage (`lib/modules`) rather than exploded `.jmod`s (HotSpot
            // always uses this form). `find_resource` keys on the module-relative
            // resource name, so strip the leading `/<module>/` before looking up.
            let path = rest.trim_start_matches('/');
            let resource = match path.split_once('/') {
                Some((_module, r)) => r,
                None => path,
            };
            ctx.find_resource(resource)
                .ok_or_else(|| ioex(format!("URL.openStream: jrt resource not found: {url_str}")))?
        } else if url_str.starts_with("http://") || url_str.starts_with("https://") {
            // GC-safe blocking region: the exchange blocks in native socket
            // reads, and the peer may be an in-process Java server (e.g.
            // MockWebServer in Spring's ResourceTests). Without parking this
            // thread, a concurrent STW request freezes the server's worker
            // threads while we sit in recv() — the 30s SO_RCVTIMEO then fires
            // as `Resource temporarily unavailable (os error 11)`
            // (ResourceTests.canCustomizeHttpUrlConnectionForRead). Pure OS
            // I/O, no ctx interaction inside → safe to park. Mirrors the
            // blocking-region use in http_url_connection.rs::perform.
            ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
            let resp = http_perform_request("GET", &url_str, &[], &[], 10, None);
            ctx.end_blocking_region();
            let resp = resp.map_err(|e| ioex(format!("URL.openStream failed: {e}")))?;
            resp.body
        } else {
            // Application-provided `URLStreamHandler` (e.g. ShrinkWrap's
            // in-memory `archive:` handler). The scheme isn't one we resolve
            // natively, but the URL may carry a custom handler whose connection
            // reads from a heap-resident / custom source. Mirror the real
            // `URL.openStream()` = `openConnection().getInputStream()` by
            // delegating to it. A thrown `FileNotFoundException` propagates via
            // `?` (matching the JDK's missing-resource contract).
            match url_custom_handler_connection(ctx, this)? {
                Some(Value::Object(Some(conn))) => {
                    let stream =
                        ctx.invoke_virtual(conn, "getInputStream", "()Ljava/io/InputStream;", &[])?;
                    return Ok(Some(stream.unwrap_or(Value::Object(None))));
                }
                // Custom handler present but `openConnection` returned null:
                // surface a null stream rather than a bogus "unsupported scheme".
                Some(_) => return Ok(Some(Value::Object(None))),
                // No custom handler — genuinely unsupported.
                None => {}
            }
            return Err(ioex(format!(
                "URL.openStream: unsupported scheme: {url_str}"
            )));
        };

        if is_sf && spring_dbg_enabled() {
            eprintln!("[OSTR-DBG] URL.openStream bytes={}", bytes.len());
        }
        // Use the shared real-JDK-safe ByteArrayInputStream allocator rather
        // than writing presumed field slots. The latter made parser-style
        // single-byte reads observe an empty stream even though bulk reads
        // appeared to work, which broke XML resources inherited through a
        // filtered URLClassLoader.
        let stream = crate::lang_class::t19_h10_alloc_byte_array_input_stream(ctx, &bytes)?;
        Ok(Some(Value::Object(Some(stream))))
    }, NativeKind::SyntheticStub);

    // URL.openConnection() — return a synthetic URLConnection that defers
    // its `getInputStream()` to the URL's `openStream()` resolver above.
    //
    // The real-JDK path is `handler.openConnection(this)`, but the
    // URL objects we hand out for resources synthesised by
    // `ClassLoader.getResource(s)` and `Class.getProtectionDomain` were
    // never run through the real `URL.<init>` (which would have populated
    // the package-private `handler` field via `URL.getURLStreamHandler`),
    // so the JDK call NPEs at line 1209. Spring's `UrlResource.getInputStream`
    // and `JarFile.getInputStream` go through `URL.openConnection().
    // getInputStream()`, so a working override is the difference between
    // SB3's `SpringFactoriesLoader` finding `META-INF/spring.factories`
    // entries and the launcher dying with `InvocationTargetException`
    // wrapping a `NullPointerException` at `URL.openConnection`.
    //
    // We pick `java/net/HttpURLConnection` as the carrier class. It is
    // concrete (so allocation succeeds), it is a subclass of URLConnection
    // (so `URLConnection`-typed locals accept it), and the existing
    // HttpURLConnection natives below cover `connect`, `getResponseCode`,
    // `getInputStream`, `setRequestMethod`, etc. for the http(s) case.
    // For non-http schemes we override `getInputStream` here to delegate
    // back to URL.openStream so file:/jar:/classpath:/nested:/jrt: all
    // serve bytes consistently with the resource resolver.
    r.register(
        url,
        "openConnection",
        "()Ljava/net/URLConnection;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // PIN THE RECEIVER, and re-read it after every allocation below.
            //
            // `this` is a raw `ObjectRef` -- a bare address. Everything this
            // native does afterwards can allocate (`create_string`,
            // `new_object`, `try_alloc_concurrent_synthetic`) or run bytecode
            // (`invoke_virtual`, `invoke_special`), and a moving young
            // collection at any of those points relocates the URL and leaves
            // `this` naming the address it moved away from.
            //
            // The write that matters is `set_field_by_name(conn, "url", this)`
            // on the carrier below: it stores that dead address into a LIVE
            // object's field, where no frame remap will ever reach it, and the
            // allocator then re-serves the address to something else.
            //
            // Observed exactly that way on `BindableTests` under
            // `CRATONVM_DBG_GC_STRESS=262144`:
            // `ServiceLoader$LazyClassPathLookupIterator.parse` dispatching
            // `openStream()` on a `java.util.Hashtable`, while the frame's own
            // `local[1]` still held the correct, relocated `java.net.URL`. The
            // stale address was in the `JarURLConnection.url` field written
            // here. See
            // `docs/feature-designs/default-moving-young-gen.md`.
            let p_this = ctx.pin_native_root(this);
            // Application-provided `URLStreamHandler` (e.g. ShrinkWrap
            // `archive:`): the real `URL.openConnection()` is
            // `handler.openConnection(this)`. Delegate so the app's own
            // `URLConnection` (which reads from its custom backing store) is
            // returned instead of the synthetic http/jar carrier below. Only
            // non-null, non-`sun.net.*` handlers reach this — `file:`/`jar:`/
            // synthetic URLs fall through to the carrier path unchanged.
            if let Some(conn @ Value::Object(Some(_))) = url_custom_handler_connection(ctx, this)? {
                return Ok(Some(conn));
            }
            // The JDK's own `jar` handler opens a `jar:file:` URL it carries:
            // its `JarURLConnection`, not the carrier below
            // (`jdk_jar_file_handler`; the `JdkJarUrlConnections` row).
            let this = ctx.read_native_pin(p_this, this);
            if let Some(handler) = jdk_jar_file_handler(ctx, this) {
                ctx.note_alignment_divergence(
                    cratonvm_types::HotSpotAlignment::JdkJarUrlConnections,
                );
                let this = ctx.read_native_pin(p_this, this);
                let conn = ctx.invoke_virtual(
                    handler,
                    "openConnection",
                    "(Ljava/net/URL;)Ljava/net/URLConnection;",
                    &[Value::Object(Some(this))],
                );
                ctx.unpin_native_roots(p_this);
                return Ok(Some(conn?.unwrap_or(Value::Object(None))));
            }
            // Determine the URL's external form so we can pick a carrier
            // class whose type matches what JDK callers cast the result to.
            // ActiveMQ's Main.getActiveMQHome() does
            //   `(JarURLConnection) url.openConnection()` on the
            //   `jar:file:…!/…` URL returned by ClassLoader.getResource() —
            // returning an HttpURLConnection there throws ClassCastException,
            // which is swallowed and forces a wrong `../.` home fallback.
            let this = ctx.read_native_pin(p_this, this);
            let ext = {
                let s5 = read_field_string_or(ctx, this, 5, "");
                // `s5.contains(':')` alone false-positives on a real-JDK
                // URL's field 5 (`authority`, e.g. "localhost:8080" — always
                // has a colon), skipping the toExternalForm() fallback below
                // and leaving `ext` as the bare authority. That misses both
                // the `jar:` and `http(s)://` prefix checks, so every real
                // http(s) URL fell to the generic URLConnection carrier and
                // `(HttpURLConnection) url.openConnection()` threw
                // ClassCastException everywhere (e.g. Tomcat's
                // TomcatBaseTest.methodUrl). Use the same synthetic-vs-real
                // discriminator as the openStream/toExternalForm paths above.
                let s = if field5_is_full_url(&s5) {
                    s5
                } else {
                    let this = ctx.read_native_pin(p_this, this);
                    match ctx.invoke_virtual(this, "toExternalForm", "()Ljava/lang/String;", &[]) {
                        Ok(Some(Value::Object(Some(o)))) => ctx.read_string(o).unwrap_or_default(),
                        _ => {
                            let this = ctx.read_native_pin(p_this, this);
                            let s0 = read_field_string_or(ctx, this, 0, "");
                            if s0.contains(':') {
                                s0
                            } else {
                                String::new()
                            }
                        }
                    }
                };
                s
            };
            // S111r23-DBG: log openConnection calls for spring.factories
            if ext.contains("spring.factories") && spring_dbg_enabled() {
                eprintln!("[CONN-DBG] URL.openConnection: {}", ext);
            }
            // Let the real JDK FileURLConnection own `file:` resources. Its
            // getInputStream body is precisely what URLClassLoader expects;
            // routing a file URL through the synthetic HTTP carrier leaves the
            // concrete HTTP connection without response state and returns EOF.
            if let Some(raw_path) = ext.strip_prefix("file:") {
                let decoded = uri_percent_decode(raw_path);
                // POSIX: the URL's decoded path (e.g. `/data/data/...`) IS
                // the absolute filesystem path already -- keep it intact.
                // Windows: strip the leading `/` and, for the MSYS/Cygwin-
                // style `/c/...` form (no colon), reinject the drive-letter
                // colon (`c/foo` -> `c:/foo`) so `new File(path)` resolves.
                // A prior version unconditionally stripped every leading
                // `/` before this cfg split existed, which silently broke
                // POSIX absolute-path resolution (see
                // docs/feature-designs/gpu-offload.md). That fix was itself silently reverted by a
                // stale-branch merge (b90ecea19, 2026-07-20) that carried
                // an older, pre-fix copy of this function back into dev --
                // restoring it here, same shape as the original fix.
                #[cfg(windows)]
                let path = {
                    let mut path = decoded.trim_start_matches('/').to_string();
                    let bytes = path.as_bytes();
                    if bytes.len() >= 2
                        && bytes[0].is_ascii_alphabetic()
                        && (bytes[1] == b'/' || bytes[1] == b'\\')
                    {
                        path.insert(1, ':');
                    }
                    path
                };
                #[cfg(not(windows))]
                let path = decoded.clone();
                let file = match ctx.new_object("java/io/File")? {
                    Some(Value::Object(Some(o))) => o,
                    _ => return Err(ioex("URL.openConnection: allocate File")),
                };
                let p_file = ctx.pin_native_root(file);
                let path_string = ctx.create_string(&path);
                let file = ctx.read_native_pin(p_file, file);
                ctx.invoke_special(
                    "java/io/File",
                    "<init>",
                    "(Ljava/lang/String;)V",
                    &[Value::Object(Some(file)), Value::Object(Some(path_string))],
                )?;
                let conn = match ctx.new_object("sun/net/www/protocol/file/FileURLConnection")? {
                    Some(Value::Object(Some(o))) => o,
                    _ => return Err(ioex("URL.openConnection: allocate FileURLConnection")),
                };
                let this = ctx.read_native_pin(p_this, this);
                let file = ctx.read_native_pin(p_file, file);
                ctx.invoke_special(
                    "sun/net/www/protocol/file/FileURLConnection",
                    "<init>",
                    "(Ljava/net/URL;Ljava/io/File;)V",
                    &[
                        Value::Object(Some(conn)),
                        Value::Object(Some(this)),
                        Value::Object(Some(file)),
                    ],
                )?;
                ctx.unpin_native_roots(p_this);
                return Ok(Some(Value::Object(Some(conn))));
            }
            // `jrt:` (JEP 220 runtime image) URLs get their own carrier, exactly
            // as the real JDK does (`sun.net.www.protocol.jrt.JavaRuntimeURLConnection`).
            // Handing these the generic HttpURLConnection carrier made
            // `con instanceof HttpURLConnection` true for a runtime-image
            // resource, so Spring's `AbstractFileResolvingResource.isReadable`
            // took its HTTP branch and fired a HEAD request at a jrt URL --
            // `ClassPathResource("java/beans/Introspector.class").isReadable()`
            // was false even though `openStream()` served the right 23755
            // bytes (core.io.ModuleResourceTests.existingClassFileResource).
            if ext.starts_with("jrt:") {
                let conn = try_alloc_concurrent_synthetic(ctx, JRT_URL_CONNECTION, 16)?;
                let this = ctx.read_native_pin(p_this, this);
                ctx.set_field(conn, HUC_URL, Value::Object(Some(this)));
                ctx.set_field(conn, HUC_DO_INPUT, Value::Int(1));
                ctx.set_field(conn, HUC_CONNECTED, Value::Int(0));
                ctx.unpin_native_roots(p_this);
                return Ok(Some(Value::Object(Some(conn))));
            }
            // For `jar:` URLs, retain the JarURLConnection carrier so callers
            // that cast it continue to work. All other schemes need the
            // concrete HttpURLConnection carrier, including `file:`. The
            // abstract URLConnection base has real-JDK bytecode for
            // getInputStream() which throws UnknownServiceException, and that
            // bytecode wins over a native registered on the base class. The
            // HttpURLConnection-specific native below instead dispatches to
            // URL.openStream() for every non-http scheme.
            let carrier = if ext.starts_with("jar:") {
                "java/net/JarURLConnection"
            } else if ext.starts_with("https:") {
                // Spring's SkipSslVerificationHttpRequestFactory first tests
                // the carrier with `instanceof HttpsURLConnection`.  Returning
                // the plain HTTP base here skipped that whole configuration
                // branch, so its permissive TrustManager was never created.
                //
                // The CONCRETE subclass is what the real JDK returns and it
                // still satisfies that `instanceof` -- `javap` on HotSpot 25
                // confirms
                //   sun.net.www.protocol.https.HttpsURLConnectionImpl
                //       extends javax.net.ssl.HttpsURLConnection
                // -- so the Spring branch above is preserved. Handing back the
                // ABSTRACT base instead made getCipherSuite(),
                // getLocalCertificates() and getServerCertificates(), all three
                // DECLARED ABSTRACT on it, throw
                // `AbstractMethodError: ... has no Code attribute`. See
                // `https_session_accessors` below, which is the other half of
                // this change and must not be separated from it: the Impl's
                // own bytecode is `getfield delegate; invokevirtual ...` for
                // every one of those methods, and this carrier is allocated
                // rather than constructed, so `delegate` is null and the swap
                // alone would only trade the AbstractMethodError for an NPE.
                "sun/net/www/protocol/https/HttpsURLConnectionImpl"
            } else {
                "java/net/HttpURLConnection"
            };
            let conn = try_alloc_concurrent_synthetic(ctx, carrier, 16)?;
            let p_conn = ctx.pin_native_root(conn);
            // BY NAME, NOT BY SLOT — and this is not a style preference.
            //
            // Both carriers selected above are REAL JDK classes with the
            // JDK's own field layout, and this file's `HUC_*` constants are a
            // synthetic map that does not match it. MEASURED at `e7e840264`
            // on `RSslLiveSession`, with `CRATONVM_DBG_COERCION=1` naming
            // this closure as the writer and `CRATONVM_DBG_LAYOUT=1`
            // resolving the class (`cid=735 ... refs=7 fields=20`, which is
            // exactly `javap`'s flattened instance-field list):
            //
            // ```text
            //  slot  this file meant   the real field it hit        outcome
            //  ----  ---------------   --------------------------  --------------------
            //   0    HUC_URL           URLConnection.url      (L)   right by luck
            //   1    HUC_METHOD        URLConnection.doInput  (Z)   String -> DESTROYED
            //   7    HUC_DO_INPUT      URLConnection.connectTimeout (I)  silently = 1ms
            //   9    HUC_CONNECTED     URLConnection.requests (L)   Int -> DESTROYED, null
            // ```
            //
            // The guard fired on two of the four and said nothing about the
            // other two, because `connectTimeout` is an `I` and takes an
            // `Int(1)` without complaint. So the visible half was a warning
            // and the invisible half was a 1-millisecond connect timeout on
            // every connection this path hands out.
            //
            // `sun.net.www.MessageHeader requests` is the field that holds
            // every request header the JDK's own code path would send, and it
            // was being set to null on construction.
            //
            // Writing by name asks the class where its field is, so the four
            // values land where they mean something: this is also the ONLY
            // reason `HUC_URL` looked correct — `url` genuinely is slot 0.
            // See `http_url_connection.rs`, which states the same rule for
            // itself ("never write synthetic slots (they alias real fields on
            // a real-JDK object)") and keeps its state in an identity-keyed
            // side table.
            let this = ctx.read_native_pin(p_this, this);
            ctx.set_field_by_name(conn, "url", Value::Object(Some(this)));
            // Everything else `java.net.URLConnection` and
            // `java.net.HttpURLConnection` declare an initialiser for —
            // `method`, `doInput`, `useCaches`, `instanceFollowRedirects`,
            // `connected`, and the four `-1` sentinels a zeroed carrier reads
            // as ALREADY SET. One list, shared with `huc_init`, because a
            // user subclass reaching `super(u)` has the identical hole; see
            // `huc_write_declared_field_defaults` for what each one costs.
            let conn = ctx.read_native_pin(p_conn, conn);
            let conn = crate::http_url_connection::huc_write_declared_field_defaults(ctx, conn);
            // A carrier this call just minted inherits nothing. See
            // `http_url_connection::real_forget`: its side tables are keyed by
            // the carrier's weak lock key since gc-common w27-b, so a fresh
            // carrier normally has no row at all; the call stays as the
            // mint-time guarantee.
            crate::http_url_connection::real_forget(ctx, conn);
            ctx.unpin_native_roots(p_this);
            Ok(Some(Value::Object(Some(conn))))
        }, NativeKind::SyntheticStub,
    );
    // java/net/JarURLConnection.getJarFileURL() — return the `file:` URL of
    // the enclosing JAR. The originating `jar:file:…!/entry` URL is stored in
    // HUC_URL; strip the `jar:` prefix and the `!/entry` suffix to recover
    // the bare jar URL, then construct a fresh java.net.URL for it.
    r.register(
        "java/net/JarURLConnection",
        "getJarFileURL",
        "()Ljava/net/URL;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Some(url) = constructed_jar_connection_field(ctx, this, "jarFileURL") {
                return Ok(Some(url));
            }
            let url_obj = match ctx.get_field(this, HUC_URL) {
                Value::Object(Some(o)) => o,
                _ => return Err(ioex("JarURLConnection.getJarFileURL: no URL")),
            };
            // Recover the full `jar:file:…!/…` external form.
            let mut ext = read_field_string_or(ctx, url_obj, 5, "");
            if !ext.starts_with("jar:") {
                if let Ok(Some(Value::Object(Some(o)))) =
                    ctx.invoke_virtual(url_obj, "toExternalForm", "()Ljava/lang/String;", &[])
                {
                    ext = ctx.read_string(o).unwrap_or_default();
                }
            }
            // jar:<jarurl>!/<entry>  →  <jarurl>
            let jar_part = ext
                .strip_prefix("jar:")
                .unwrap_or(&ext)
                .split("!/")
                .next()
                .unwrap_or("")
                .to_string();
            if jar_part.is_empty() {
                return Err(ioex("JarURLConnection.getJarFileURL: malformed URL"));
            }
            let spec = ctx.create_string(&jar_part);
            // Same rule as `URL.openConnection` above: `new_object` allocates,
            // so `spec` has to be re-read from a pin before it is handed to
            // the constructor, or the URL is built from a dead address.
            let p_spec = ctx.pin_native_root(spec);
            // Construct via the regular `new URL(String)` path so the
            // returned object is a fully-initialised java.net.URL.
            let new_url = match ctx.new_object("java/net/URL")? {
                Some(Value::Object(Some(o))) => o,
                _ => return Err(ioex("JarURLConnection.getJarFileURL: alloc URL")),
            };
            let spec = ctx.read_native_pin(p_spec, spec);
            // gc-common w20-d: `<init>` is Java; the URL is pinned across it
            // and returned at its current address, not its allocation one.
            let p_url = ctx.pin_native_root(new_url);
            ctx.invoke_special(
                "java/net/URL",
                "<init>",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(new_url)), Value::Object(Some(spec))],
            )?;
            let new_url = ctx.read_native_pin(p_url, new_url);
            ctx.unpin_native_roots(p_spec);
            Ok(Some(Value::Object(Some(new_url))))
        }, NativeKind::Bridge,
    );
    // java/net/JarURLConnection.getJarFile() — return a `java/util/jar/JarFile`
    // opened on the enclosing JAR.
    //
    // `JarURLConnection` is ABSTRACT and `getJarFile()` is an abstract method
    // (no Code attribute). The concrete implementation lives in
    // `sun.net.www.protocol.jar.JarURLConnection` (or Spring Boot's own
    // nested-jar `URLConnection`). Our `URL.openConnection()` above hands out
    // a synthetic object whose *runtime* class is the abstract
    // `java/net/JarURLConnection`, so a virtual `getJarFile()` dispatch lands
    // on the abstract declaration and throws
    //   `AbstractMethodError: java/net/JarURLConnection.getJarFile() has no
    //    Code attribute`
    // which aborts Spring Boot 4's `JarLauncher` before it can read the fat
    // jar. Registering the native here completes the synthetic carrier the
    // same way `getJarFileURL()` / `getInputStream()` already do.
    //
    // The originating `jar:file:…!/entry` URL is stored in HUC_URL; strip the
    // `jar:file:` prefix and the `!/entry` suffix to recover the bare on-disk
    // jar path, then construct a `java/util/jar/JarFile` via its
    // `<init>(Ljava/lang/String;)V` native (`p59_jar_file_init`).
    r.register(
        "java/net/JarURLConnection",
        "getJarFile",
        "()Ljava/util/jar/JarFile;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // A JarURLConnection owns one JarFile for its lifetime. In
            // particular, callers that disable caches close that instance;
            // a later getJarFile() must return the closed object rather than
            // silently allocating a fresh archive.
            if let Value::Object(Some(jar_file)) = ctx.get_field(this, HUC_JAR_FILE) {
                return Ok(Some(Value::Object(Some(jar_file))));
            }
            let url_obj = match ctx.get_field(this, HUC_URL) {
                Value::Object(Some(o)) => o,
                _ => return Err(ioex("JarURLConnection.getJarFile: no URL")),
            };
            // gc-common w20-d: the connection is rooted from here to the end.
            // `toExternalForm`, the JarFile's and the path's allocations and
            // `JarFile.<init>` all ran before the JarFile was cached through
            // it; the JarFile is rooted across the String and `<init>` too.
            let mut scope = NativeHandleScope::new(ctx);
            let this_h = scope.root(this);
            let ctx = &mut *scope;
            // Recover the full `jar:file:…!/…` external form.
            let mut ext = read_field_string_or(ctx, url_obj, 5, "");
            if !ext.starts_with("jar:") {
                if let Ok(Some(Value::Object(Some(o)))) =
                    ctx.invoke_virtual(url_obj, "toExternalForm", "()Ljava/lang/String;", &[])
                {
                    ext = ctx.read_string(o).unwrap_or_default();
                }
            }
            // jar:file:<path>!/<entry>  →  <path>  (the outer jar on disk).
            // For a double-nested Spring Boot 2.x URL
            //   jar:file:/fat.jar!/BOOT-INF/lib/inner.jar!/entry
            // the *first* `!/`-delimited segment is still the outer jar.
            let after_scheme = ext
                .strip_prefix("jar:file:")
                .or_else(|| ext.strip_prefix("jar:"))
                .unwrap_or(&ext);
            let jar_part_enc = after_scheme
                .split("!/")
                .next()
                .unwrap_or("")
                .trim_start_matches("file:");
            if jar_part_enc.is_empty() {
                return Err(ioex("JarURLConnection.getJarFile: malformed URL"));
            }
            // Percent-decode the jar file's own URL component — see the
            // matching fix in URL.openStream's jar:file: handler
            // (TestDeployTask.bug58086a) for why.
            let jar_part = uri_percent_decode(jar_part_enc);
            // Resolve to a real on-disk path. `file:` URLs use a leading `/`
            // before a Windows drive letter (`/C:/…`); try the trimmed form
            // first, then the raw form for POSIX absolute paths — mirrors the
            // `URL.openStream` resolution above.
            let trimmed = jar_part.trim_start_matches('/');
            let disk_path = if std::path::Path::new(trimmed).exists() {
                trimmed.to_string()
            } else if std::path::Path::new(&jar_part).exists() {
                jar_part.clone()
            } else {
                // The enclosing jar genuinely does not exist on disk. The real
                // `JarURLConnection.getJarFile()` throws `FileNotFoundException`
                // here; our `JarFile.<init>` did NOT, so the connection handed
                // back an empty JarFile and Spring's
                // `AbstractFileResolvingResource.exists()` (which treats
                // `entryName == null` — i.e. a `…!/` jar-root URL — as existing
                // once `getJarFile()` returns) reported a NON-EXISTENT jar as
                // present (PMRPR `javaDashJarFinds…` `writeAssetJar`
                // `jar:file:X<path>!/` assertion). Surface the missing jar so
                // the `catch (IOException)` turns it into `false`.
                return Err(fnfex(format!(
                    "JarURLConnection.getJarFile: no such jar file: {jar_part}"
                )));
            };
            // Allocate the JarFile and run its <init>(String) native so the
            // `path` (field 0) and parsed `manifest` (field 1) are populated.
            let jar_file = match ctx.new_object("java/util/jar/JarFile")? {
                Some(Value::Object(Some(o))) => o,
                _ => return Err(ioex("JarURLConnection.getJarFile: alloc JarFile")),
            };
            let jar_h = scope.root(jar_file);
            let path_str = scope.create_string(&disk_path);
            let jar_file = scope.get(&jar_h);
            scope.invoke_special(
                "java/util/jar/JarFile",
                "<init>",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(jar_file)), Value::Object(Some(path_str))],
            )?;
            let jar_file = scope.get(&jar_h);
            let this = scope.get(&this_h);
            scope.set_field(this, HUC_JAR_FILE, Value::Object(Some(jar_file)));
            Ok(Some(Value::Object(Some(jar_file))))
        }, NativeKind::SyntheticStub,
    );
    // java/net/JarURLConnection.getContentLengthLong() / getContentLength() —
    // return the jar ENTRY's uncompressed size. Our synthetic carrier has no
    // JDK URLConnection header machinery, so the inherited URLConnection
    // implementation returns -1. Spring's
    // `AbstractFileResolvingResource.isReadable()` treats `contentLength <= 0`
    // (with no exception) as NOT readable, so a perfectly good jar class
    // resource reported `isReadable=false`, `contentLength=-1`, and
    // `classpath*:…/*.class` scanning matched 0 resources (SB-07 / S10_Resources).
    // Recover the entry from the `jar:file:…!/entry` URL in HUC_URL and read
    // its size straight from the zip central directory. The lookup walks the
    // superclass chain starting at the receiver's JarURLConnection class, so it
    // wins over the inherited URLConnection/HttpURLConnection registrations.
    r.register(
        "java/net/JarURLConnection",
        "getContentLengthLong",
        "()J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if is_constructed_jar_connection(ctx, this) {
                // gci i18/vp: a tail call (`continuation_pin::tail_call`) of the
                // same descriptor; the body connects (a remote jar downloads).
                return cratonvm_native_api::continuation_pin::tail_call(|| {
                    ctx.invoke_special_bytecode_only(
                        "java/net/URLConnection",
                        "getContentLengthLong",
                        "()J",
                        args,
                    )
                });
            }
            Ok(Some(Value::Long(jar_url_conn_entry_size(ctx, this))))
        }, NativeKind::SyntheticStub,
    );
    r.register(
        "java/net/JarURLConnection",
        "getContentLength",
        "()I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if is_constructed_jar_connection(ctx, this) {
                // gci i18/vp: a tail call, as `getContentLengthLong` above.
                return cratonvm_native_api::continuation_pin::tail_call(|| {
                    ctx.invoke_special_bytecode_only(
                        "java/net/URLConnection",
                        "getContentLength",
                        "()I",
                        args,
                    )
                });
            }
            let sz = jar_url_conn_entry_size(ctx, this);
            // URLConnection.getContentLength() narrows to int, -1 when unknown
            // or larger than Integer.MAX_VALUE.
            let v = if sz < 0 || sz > i32::MAX as i64 {
                -1
            } else {
                sz as i32
            };
            Ok(Some(Value::Int(v)))
        }, NativeKind::SyntheticStub,
    );
    // java/net/JarURLConnection.getJarEntry() — the no-arg accessor that
    // Spring's checkReadable() and PathMatchingResourcePatternResolver use to
    // probe a jar resource. Our synthetic carrier's runtime class is the
    // ABSTRACT java/net/JarURLConnection, so the virtual dispatch would land on
    // the abstract declaration (AbstractMethodError) without this native.
    r.register(
        "java/net/JarURLConnection",
        "getJarEntry",
        "()Ljava/util/jar/JarEntry;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if is_constructed_jar_connection(ctx, this) {
                // gci i18/vp: a tail call, as `getContentLengthLong` above.
                return cratonvm_native_api::continuation_pin::tail_call(|| {
                    ctx.invoke_special_bytecode_only(
                        "java/net/JarURLConnection",
                        "getJarEntry",
                        "()Ljava/util/jar/JarEntry;",
                        args,
                    )
                });
            }
            let ext = jar_url_conn_ext(ctx, this);
            Ok(Some(jar_url_lookup_entry(ctx, &ext)?))
        }, NativeKind::SyntheticStub,
    );
    // java/net/JarURLConnection.getEntryName() — the entry path inside the jar
    // (the part after `!/`), or null for a bare `jar:file:…!/` (jar root).
    //
    // Without this native the inherited real-JDK accessor reads the synthetic
    // carrier's never-populated `entryName` field and returns null even for
    // `jar:file:…!/some/entry`. Spring's `AbstractFileResolvingResource.exists()`
    // does `entryName == null || getJarEntry() != null`, so a null entryName
    // made a NON-EXISTENT jar entry report as existing (PMRPR
    // `javaDashJarFinds…` `writeAssetJar` asserting `…!/assets/none.txt` is
    // absent). Parse it the same way `jar_url_lookup_entry` does so the two stay
    // consistent.
    r.register(
        "java/net/JarURLConnection",
        "getEntryName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Some(name) = constructed_jar_connection_field(ctx, this, "entryName") {
                return Ok(Some(name));
            }
            let ext = jar_url_conn_ext(ctx, this);
            let entry = ext
                .strip_prefix("jar:file:")
                .or_else(|| ext.strip_prefix("jar:"))
                .and_then(|a| a.splitn(2, "!/").nth(1))
                .filter(|e| !e.is_empty());
            match entry {
                Some(e) => {
                    let s = ctx.create_string(e);
                    Ok(Some(Value::Object(Some(s))))                }
                None => Ok(Some(Value::Object(None))),
            }
        }, NativeKind::SyntheticStub,
    );

    // URLConnection.setUseCaches / setDefaultUseCaches / connect — Spring's
    // `ResourceUtils.useCachesIfNecessary` calls setUseCaches(false) on
    // file: URLs. Avoid the real-JDK setter's uninitialised-connected check,
    // but retain the requested per-connection value: Spring Boot's nested
    // JarUrlConnection uses it to select its cached-empty-stream and
    // close-on-close paths.
    // file: URLs; without these no-op natives the call would fall through
    // to the real-JDK setter, which probes the (uninitialised) connected
    // field and throws IllegalStateException. Make them no-ops on both
    // URLConnection and HttpURLConnection (registered separately).
    // The `jrt:` carrier handed out by `URL.openConnection` above. Its
    // methods are the same URLConnection bodies registered just below, but
    // native lookup is by EXACT class, so the base-class registrations never
    // reach a subclass carrier -- they have to be repeated here (the same
    // reason `setUseCaches` is registered on both URLConnection and
    // HttpURLConnection).
    // KEEP the no-op `connect()`. The `jrt:` carrier holds no OS connection to
    // establish — `getInputStream()` below opens the module resource lazily —
    // so "connected" is already true, and `connect()` on an already-connected
    // URLConnection is a no-op by contract.
    r.register(JRT_URL_CONNECTION, "connect", "()V", |_ctx, _args| Ok(None), NativeKind::SyntheticStub);
    // Retain the requested value rather than dropping it, exactly as the
    // `java/net/URLConnection` registration below does — Spring Boot's nested
    // `JarUrlConnection` reads `useCaches` back to pick its cached-empty-stream
    // and close-on-close paths.
    r.register(JRT_URL_CONNECTION, "setUseCaches", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let use_caches = args.get(1).and_then(Value::as_int).unwrap_or(1);
        ctx.set_field_by_name(this, "useCaches", Value::Int(use_caches));
        Ok(None)
    }, NativeKind::SyntheticStub);
    // Same body as the `java/net/URLConnection` registration below: the JDK
    // setter writes the class-wide `defaultUseCaches` static, and the carrier
    // needs its own copy because native lookup on a synthetic carrier class is
    // by exact class (see the note above).
    r.register(
        JRT_URL_CONNECTION,
        "setDefaultUseCaches",
        "(Z)V",
        |ctx, args| {
            let default_use_caches = args.get(1).and_then(Value::as_int).unwrap_or(1);
            let _ = ctx.ensure_class_initialized("java/net/URLConnection");
            ctx.set_static_field_by_name(
                "java/net/URLConnection",
                "defaultUseCaches",
                Value::Int(default_use_caches),
            );
            Ok(None)
        }, NativeKind::SyntheticStub,
    );
    r.register(
        JRT_URL_CONNECTION,
        "getContentLength",
        "()I",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            let url = huc_url_string(ctx, &mut this);
            let len = synthetic_resource_url_content_len(ctx, &url);
            let v = if len < 0 || len > i32::MAX as i64 {
                -1
            } else {
                len as i32
            };
            Ok(Some(Value::Int(v)))
        }, NativeKind::SyntheticStub,
    );
    r.register(
        JRT_URL_CONNECTION,
        "getContentLengthLong",
        "()J",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            let url = huc_url_string(ctx, &mut this);
            Ok(Some(Value::Long(synthetic_resource_url_content_len(
                ctx, &url,
            ))))
        }, NativeKind::SyntheticStub,
    );
    r.register(JRT_URL_CONNECTION, "getLastModified", "()J", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let url = huc_url_string(ctx, &mut this);
        Ok(Some(Value::Long(synthetic_resource_url_last_modified(
            &url,
        ))))
    }, NativeKind::SyntheticStub);
    r.register(
        JRT_URL_CONNECTION,
        "getInputStream",
        "()Ljava/io/InputStream;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let url_obj = match ctx.get_field(this, HUC_URL) {
                Value::Object(Some(o)) => o,
                _ => return Err(ioex("JavaRuntimeURLConnection.getInputStream: no URL")),
            };
            // SBR wave 2 (lane `netaddr`): under `--real-jdk` `URL.openStream`
            // and `URL.openConnection` are the JDK's own bodies, so this
            // connection is the REAL one `jrt.Handler.openConnection` built,
            // and `url.openStream()` is `openConnection().getInputStream()`
            // -- i.e. this native again, forever (`StackOverflowError`, probe
            // row `jrt(Object).conn`). Serve the module resource's bytes here
            // instead, from the same resolver the retired `openStream` native
            // used for `jrt:`. `--compatible` keeps the delegation.
            if ctx.is_jdk_only() {
                let text = match ctx.invoke_virtual(
                    url_obj,
                    "toExternalForm",
                    "()Ljava/lang/String;",
                    &[],
                )? {
                    Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
                    _ => String::new(),
                };
                let path = text
                    .strip_prefix("jrt:")
                    .unwrap_or(&text)
                    .trim_start_matches('/');
                let resource = match path.split_once('/') {
                    Some((_module, r)) => r,
                    None => path,
                };
                let bytes = ctx
                    .find_resource(resource)
                    .ok_or_else(|| RuntimeError::FileNotFoundException { path: text.clone() })?;
                let stream = crate::lang_class::t19_h10_alloc_byte_array_input_stream(ctx, &bytes)?;
                return Ok(Some(Value::Object(Some(stream))));
            }
            ctx.invoke_virtual(url_obj, "openStream", "()Ljava/io/InputStream;", &[])
        }, NativeKind::SyntheticStub,
    );
    r.register(
        "java/net/URLConnection",
        "setUseCaches",
        "(Z)V",
        |ctx, args| {
            uc_subclass_guard!(ctx, args, "setUseCaches", "(Z)V");
            let this = obj_arg(args, 0)?;
            // `URLConnection.setUseCaches` opens with `checkConnected()`:
            // "if already connected". Writing the field regardless let a
            // caller change caching on a connection whose request had already
            // gone out and reported success.
            if matches!(ctx.get_field_by_name(this, "connected"), Value::Int(v) if v != 0) {
                return Err(RuntimeError::IllegalStateException {
                    message: "Already connected".to_string(),
                }
                .into());
            }
            let use_caches = args.get(1).and_then(Value::as_int).unwrap_or(1);
            ctx.set_field_by_name(this, "useCaches", Value::Int(use_caches));
            Ok(None)
        }, NativeKind::SyntheticStub,
    );
    // JDK: `public void setDefaultUseCaches(boolean b) { defaultUseCaches = b; }`
    // — an instance method that writes the class-wide static. NO subclass
    // overrides it, so this registration intercepts every URLConnection in the
    // VM and the former no-op silently discarded the process-wide default
    // (`getDefaultUseCaches()` then contradicted every caller who turned it
    // off). Write the static the real setter writes.
    r.register(
        "java/net/URLConnection",
        "setDefaultUseCaches",
        "(Z)V",
        |ctx, args| {
            let default_use_caches = args.get(1).and_then(Value::as_int).unwrap_or(1);
            let _ = ctx.ensure_class_initialized("java/net/URLConnection");
            ctx.set_static_field_by_name(
                "java/net/URLConnection",
                "defaultUseCaches",
                Value::Int(default_use_caches),
            );
            Ok(None)
        }, NativeKind::SyntheticStub,
    );
    // KEEP the no-op. `URLConnection.connect()` is ABSTRACT in the real JDK, so
    // every concrete subclass declares its own override and native lookup —
    // which keys on the declaring class of the RESOLVED method — never lands
    // here for one of them. The only receivers that reach it are CratonVM's own
    // URLConnection carriers, which hold no OS connection and open lazily in
    // `getInputStream()`; for them "already connected" is the truthful answer.
    r.register("java/net/URLConnection", "connect", "()V", |_ctx, _args| {
        Ok(None)
    }, NativeKind::SyntheticStub);
    r.register(
        "java/net/URLConnection",
        "getContentLength",
        "()I",
        |ctx, args| {
            uc_subclass_guard!(ctx, args, "getContentLength", "()I");
            let mut this = obj_arg(args, 0)?;
            let url = huc_url_string(ctx, &mut this);
            let len = synthetic_resource_url_content_len(ctx, &url);
            let v = if len < 0 || len > i32::MAX as i64 {
                -1
            } else {
                len as i32
            };
            Ok(Some(Value::Int(v)))
        }, NativeKind::SyntheticStub,
    );
    r.register(
        "java/net/URLConnection",
        "getContentLengthLong",
        "()J",
        |ctx, args| {
            uc_subclass_guard!(ctx, args, "getContentLengthLong", "()J");
            let mut this = obj_arg(args, 0)?;
            let url = huc_url_string(ctx, &mut this);
            Ok(Some(Value::Long(synthetic_resource_url_content_len(
                ctx, &url,
            ))))
        }, NativeKind::SyntheticStub,
    );
    r.register(
        "java/net/URLConnection",
        "getLastModified",
        "()J",
        |ctx, args| {
            uc_subclass_guard!(ctx, args, "getLastModified", "()J");
            let mut this = obj_arg(args, 0)?;
            let url = huc_url_string(ctx, &mut this);
            Ok(Some(Value::Long(synthetic_resource_url_last_modified(
                &url,
            ))))
        }, NativeKind::SyntheticStub,
    );
    // URLConnection.getInputStream — defer to URL.openStream by reading
    // the URL stored in HUC_URL during openConnection above.
    r.register(
        "java/net/URLConnection",
        "getInputStream",
        "()Ljava/io/InputStream;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let url_obj = match ctx.get_field(this, HUC_URL) {
                Value::Object(Some(o)) => o,
                _ => {
                    return Err(ioex("URLConnection.getInputStream: no URL"));
                }
            };
            ctx.invoke_virtual(url_obj, "openStream", "()Ljava/io/InputStream;", &[])
        }, NativeKind::SyntheticStub,
    );

    // -----------------------------------------------------------------------
    // S111r25 — UrlResource.getInputStream() override.
    //
    // Spring's UrlResource.getInputStream() uses the pattern:
    //   URLConnection con = this.url.openConnection();
    //   customizeConnection(con);
    //   try { return con.getInputStream(); }
    //   catch (IOException ex) { ... disconnect ... throw; }
    //
    // The final `con.getInputStream()` call dispatches as
    // `invokevirtual java/net/URLConnection.getInputStream()` on our
    // synthetic `java/net/HttpURLConnection` object.  In real-JDK mode,
    // the VM finds the JDK's URLConnection bytecode (which throws
    // `UnknownServiceException extends IOException`) in preference to our
    // native registration, because the declaring class (`URLConnection`)
    // has a JDK class file present.  The IOException is caught by
    // UrlResource's handler (offset 18), disconnect() is called, and the
    // exception re-propagates to loadSpringFactories which skips the URL —
    // leaving the factory map empty and causing DefaultApplicationContextFactory
    // to fail with IllegalArgumentException.
    //
    // Fix: intercept `UrlResource.getInputStream()` directly and delegate
    // straight to URL.openStream(), completely skipping openConnection /
    // customizeConnection / URLConnection.getInputStream.  This is
    // semantically equivalent but entirely inside our VM infrastructure.
    // -----------------------------------------------------------------------
    // -----------------------------------------------------------------------
    // (REMOVED) Spring `ConfigurationClassEnhancer.enhance(Class, ClassLoader)`
    // identity-bypass shim.
    //
    // FIX(net-phase-e #2): this used to register an identity-bypass that
    // returned the @Configuration class unchanged, disabling CGLIB @Bean
    // interception (a silent-wrong-result stub: inter-@Bean-method calls
    // returned fresh instances instead of the shared singleton).
    //
    // The REAL CGLIB @Configuration enhancement is now implemented in
    // `cglib_enhancer.rs` (`cce_enhance` / `register_cglib_enhancer`), which
    // emits a genuine EnhancedConfiguration subclass and intercepts @Bean
    // methods. That registration runs in `lib.rs::register_net_natives`
    // AFTER `net_phase_e::register_phase_e_networking` and re-registers the
    // identical method triple
    //   org/springframework/context/annotation/ConfigurationClassEnhancer
    //   .enhance(Ljava/lang/Class;Ljava/lang/ClassLoader;)Ljava/lang/Class;
    // Registry semantics silently overwrite on duplicate triples
    // (native-api registry), so this bypass was already DEAD CODE —
    // unconditionally clobbered by the real enhancer. Removing it eliminates
    // the misleading stub; the live behavior is unchanged (real path wins).
    // Do not re-add a bypass here: if the real enhancer needs work, fix it in
    // `cglib_enhancer.rs`.
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // (REMOVED) AbstractBeanDefinition.getResolvedAutowireMode() override.
    //
    // A former "Bug 3" workaround hardcoded getResolvedAutowireMode() to
    // return 0 (AUTOWIRE_NO) for AbstractBeanDefinition + Root/Generic/Child
    // subclasses, to mask a field-layout/slot bug where the `autowireMode`
    // field misread as a spurious non-zero value (driving an unwanted
    // autowireByType setter pass that crashed ConfigurationClassPostProcessor).
    //
    // That underlying field bug is FIXED: getAutowireMode() (which reads the
    // same `autowireMode` slot) now returns the real value (0/2/3) identically
    // to HotSpot for default / setAutowireMode(BY_TYPE) / setAutowireMode(
    // CONSTRUCTOR) bean definitions. The constant-0 stub had become stale and
    // actively wrong: it forced AUTOWIRE_NO onto @Bean factory methods, whose
    // definitions are AUTOWIRE_CONSTRUCTOR (3). That sent
    // ConstructorResolver.createArgumentArray down the `autowiring == false`
    // branch, so a @Qualifier'd @Bean method parameter could not be autowired
    // and Spring threw "Ambiguous argument values for parameter of type ..."
    // (SB-05 / S02_Inject). Removing the stub restores the real bytecode:
    // getResolvedAutowireMode() reads the (correct) field and returns it,
    // matching HotSpot. Daemon apps using the default AUTOWIRE_NO still get 0.
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Spring Data Redis `RedisAccessor.afterPropertiesSet()`
    //
    // RedisAccessor.afterPropertiesSet() asserts that the
    // RedisConnectionFactory has been wired in:
    //   Assert.state(getConnectionFactory() != null,
    //                "RedisConnectionFactory is required");
    //
    // In real-JDK mode under CratonVM the `@Autowired` setter on
    // `RedisHttpSessionConfiguration.setRedisConnectionFactory(ObjectProvider,
    // ObjectProvider)` is not being invoked (multi-arg ObjectProvider setter
    // injection on a @Configuration class — the CGLIB enhancement is now
    // performed by the real enhancer in `cglib_enhancer.rs`; the former
    // identity-bypass shim here was removed, see the "(REMOVED)
    // ConfigurationClassEnhancer.enhance" note above).  The
    // RedisOperationsSessionRepository @Bean factory method then ends up
    // calling `RedisTemplate.afterPropertiesSet()` with a null connection
    // factory and Spring throws `IllegalStateException:
    // RedisConnectionFactory is required` during context refresh — which
    // aborts SportMe startup before it can reach the embedded Tomcat /
    // controller-registration phase we want to exercise next.
    //
    // Pragmatic workaround: turn `RedisAccessor.afterPropertiesSet()` into a
    // no-op so the RedisTemplate constructed by
    // `RedisHttpSessionConfiguration.sessionRepository()` does not blow up on
    // a missing factory.  Any actual session lookup at runtime would still
    // NPE, but bootstrap can advance past this gate.  This is the same kind
    // of bypass we apply to Tomcat lifecycle classes for the same goal.
    //
    // 2026-07-28 (wave 4): NARROWED, not left as blanket no-ops. Each of these
    // exists to dodge one documented CratonVM autowiring gap (multi-arg
    // `ObjectProvider` setter injection on a @Configuration class), whose only
    // observable symptom is a NULL collaborator. So test for that null and run
    // the REAL bytecode whenever the collaborator is present — same shape as
    // the `RedisMessageListenerContainer.setConnectionFactory` shim below,
    // which wave 3 had already narrowed for exactly this reason (the blanket
    // version was swallowing legitimate injection in Spring Data Redis's own
    // tests). The bypass now applies only in the state it was written for.
    //
    // Note on dispatch: `RedisTemplate` DOES override `afterPropertiesSet`, so
    // this registration is not reached by resolution on a RedisTemplate
    // receiver — it is reached through `RedisTemplate.afterPropertiesSet`'s
    // `super.afterPropertiesSet()` invokespecial, which resolves to
    // `RedisAccessor`.
    // -----------------------------------------------------------------------
    r.register(
        "org/springframework/data/redis/core/RedisAccessor",
        "afterPropertiesSet",
        "()V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // The real body is exactly `Assert.state(getConnectionFactory() !=
            // null, "RedisConnectionFactory is required")`, so with a factory
            // present, delegating is equivalent — and it keeps any subclass
            // state the real method would establish in future versions.
            if let Value::Object(Some(_)) = ctx.get_field_by_name(this, "connectionFactory") {
                return ctx.invoke_special_bytecode_only(
                    "org/springframework/data/redis/core/RedisAccessor",
                    "afterPropertiesSet",
                    "()V",
                    args,
                );
            }
            Ok(None)
        }, NativeKind::SyntheticStub,
    );

    // Same chain: RedisOperationsSessionRepository.setApplicationEventPublisher
    // does `Assert.notNull(applicationEventPublisher, "applicationEventPublisher cannot be null")`.
    // Because `@Autowired` setter injection on `RedisHttpSessionConfiguration` is
    // not running under our shim, the publisher is null when
    // `sessionRepository()` invokes the setter.
    //
    // NARROWED (wave 4): the unconditional no-op also discarded a VALID
    // publisher, so the repository never published session-created/deleted
    // events even when injection worked. Run the real setter when the argument
    // is non-null; only the null case (the assertion this exists to dodge) is
    // still swallowed.
    r.register(
        "org/springframework/session/data/redis/RedisOperationsSessionRepository",
        "setApplicationEventPublisher",
        "(Lorg/springframework/context/ApplicationEventPublisher;)V",
        |ctx, args| match args.get(1) {
            Some(Value::Object(Some(_))) => ctx.invoke_special_bytecode_only(
                "org/springframework/session/data/redis/RedisOperationsSessionRepository",
                "setApplicationEventPublisher",
                "(Lorg/springframework/context/ApplicationEventPublisher;)V",
                args,
            ),
            _ => Ok(None),
        }, NativeKind::SyntheticStub,
    );

    // Same chain: RedisHttpSessionConfiguration.redisMessageListenerContainer()
    // can call this setter with a null factory while its broken multi-argument
    // autowiring path is being bypassed.  Keep that narrow bootstrap escape,
    // but execute the real setter for a valid factory.  The former unconditional
    // no-op swallowed legitimate injection in Spring Data Redis's own
    // DataRedisAnnotationDrivenConfiguration tests, leaving the container
    // unusable at afterPropertiesSet().
    r.register(
        "org/springframework/data/redis/listener/RedisMessageListenerContainer",
        "setConnectionFactory",
        "(Lorg/springframework/data/redis/connection/RedisConnectionFactory;)V",
        |ctx, args| match args.get(1) {
            Some(Value::Object(Some(_))) => ctx.invoke_special_bytecode_only(
                "org/springframework/data/redis/listener/RedisMessageListenerContainer",
                "setConnectionFactory",
                "(Lorg/springframework/data/redis/connection/RedisConnectionFactory;)V",
                args,
            ),
            _ => Ok(None),
        }, NativeKind::SyntheticStub,
    );

    // Same chain: the `enableRedisKeyspaceNotificationsInitializer` bean's
    // afterPropertiesSet calls `connectionFactory.getConnection()` on its
    // null factory and NPEs.  Bypassing the init is equivalent to having
    // `ConfigureRedisAction.NO_OP` selected, which is the early-return path
    // already supported by Spring.
    //
    // NARROWED (wave 4): only bypass when the factory really is null. With a
    // factory present the real body configures Redis keyspace notifications —
    // which is the whole point of the bean, and which the blanket no-op
    // silently disabled even in a fully-wired context (expired-session events
    // then never fire).
    r.register(
        "org/springframework/session/data/redis/config/annotation/web/http/RedisHttpSessionConfiguration$EnableRedisKeyspaceNotificationsInitializer",
        "afterPropertiesSet",
        "()V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Value::Object(Some(_)) = ctx.get_field_by_name(this, "connectionFactory") {
                // gci i18/vp: a tail call (`continuation_pin::tail_call`) of the
                // same descriptor; the body talks to Redis (a socket wait).
                return cratonvm_native_api::continuation_pin::tail_call(|| {
                    ctx.invoke_special_bytecode_only(
                        "org/springframework/session/data/redis/config/annotation/web/http/RedisHttpSessionConfiguration$EnableRedisKeyspaceNotificationsInitializer",
                        "afterPropertiesSet",
                        "()V",
                        args,
                    )
                });
            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // -----------------------------------------------------------------------
    // RedisOperationsSessionRepository.cleanupExpiredSessions
    //
    // The repository registers a @Scheduled(cron = "0 * * * * *") method
    // which Spring's ScheduledAnnotationBeanPostProcessor wires onto the
    // TaskScheduler.  Because the @Autowired RedisConnectionFactory setter
    // never fires under CratonVM (see RedisAccessor.afterPropertiesSet no-op
    // above), the first cron firing executes
    //   this.expirationPolicy.cleanExpiredSessions()
    // which calls into a RedisTemplate with a null factory and throws
    //   java.lang.IllegalStateException: RedisConnectionFactory is required
    // The TaskUtils$LoggingErrorHandler logs it; SpringApplication then sees
    // the failed cleanup, marks "Application run failed", and tries to
    // cancel the cron task — at which point ScheduledTask.cancel() asserts
    //   Assert.notNull(this.future, "No scheduled future")
    // failing because the registrar's future map was torn down already.
    //
    // Pragmatic bypass: turn cleanupExpiredSessions into a no-op so the
    // cron firing succeeds silently, no error is reported, no shutdown is
    // triggered, and the application stays up.  Same shape as the other
    // session/redis bypasses above.
    //
    // NARROWED (wave 4): gate the bypass on the condition that produces the
    // failure — the repository's `RedisOperations` having no connection
    // factory. When one IS wired, run the real sweep; suppressing it there
    // leaks expired sessions in the Redis keyspace forever, which is a data
    // bug, not a bootstrap concession. `sessionRedisOperations` is the
    // `RedisTemplate` the repository was built with, and `connectionFactory`
    // is the `RedisAccessor` field the template inherits.
    // -----------------------------------------------------------------------
    r.register(
        "org/springframework/session/data/redis/RedisOperationsSessionRepository",
        "cleanupExpiredSessions",
        "()V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let wired = match ctx.get_field_by_name(this, "sessionRedisOperations") {
                Value::Object(Some(ops)) => {
                    matches!(
                        ctx.get_field_by_name(ops, "connectionFactory"),
                        Value::Object(Some(_))
                    )
                }
                _ => false,
            };
            if wired {
                // gci i18/vp: a tail call (`continuation_pin::tail_call`) of the
                // same descriptor; the sweep talks to Redis (a socket wait).
                return cratonvm_native_api::continuation_pin::tail_call(|| {
                    ctx.invoke_special_bytecode_only(
                        "org/springframework/session/data/redis/RedisOperationsSessionRepository",
                        "cleanupExpiredSessions",
                        "()V",
                        args,
                    )
                });
            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // -----------------------------------------------------------------------
    // Round 76 (REVERTED): do NOT stub ScheduledTaskRegistrar.schedule*Task.
    //
    // A previous workaround registered scheduleCronTask/scheduleFixedRateTask/
    // scheduleFixedDelayTask/scheduleTriggerTask as no-ops returning null to
    // dodge a "No scheduled future" assertion in one Redis-session app whose
    // cron fired synchronously through the DelegatedScheduledExecutorService
    // path.  That stub is far too broad: shadowing these methods bypasses the
    // real bytecode, so the registrar's cronTasks/fixedRateTasks/
    // fixedDelayTasks/triggerTasks lists are never populated (they are filled
    // by the addCronTask()/addFixedRateTask()/... calls in the taskScheduler==
    // null branch), and getScheduledTasks() collects a null ScheduledTask.
    // This breaks all @Scheduled processing — ScheduledAnnotationBeanPostProcessor
    // tests saw null task lists and NPEs (scheduleOneTimeTask, which was never
    // stubbed, was the only path that worked).  The real fix for the Redis app
    // belongs in the ConcurrentTaskScheduler/ReschedulingRunnable execution
    // path, not here; letting the real methods run matches HotSpot.

    // `--real-jdk`: RETIRED. Spring's own `UrlResource.getInputStream()` runs,
    // and it asks the URL's handler through `openConnection()`. This native
    // sent every non-http URL to the VM's `URL.openStream` text reader
    // instead, which cannot read a Spring Boot `jar:nested:` URL into
    // `BOOT-INF/classes/`, so `java -jar` of a fat jar died at its first
    // component scan. See
    // `fatjar-nested-classes-directory-url-openstream-FIXED-20260924.md`.
    // `--compatible` retires it too under `CRATONVM_COMPAT_URL_APP_HANDLER`
    // (`url_app_handler_is_authoritative`; interpreter round i1 wave 49, lane
    // L6): its one remaining reason, the http 3xx arm, is gone -- the
    // `HttpURLConnection` carrier's `huc_perform` follows up to 10 redirects
    // itself -- and Spring's body then runs `customizeConnection(con)`, which
    // this native skipped for every non-http URL.
    // `interpreter-L6-compatible-fat-jar-reads-boot-inf-classes-as-a-nested-jar-FIXED-20261013.md`.
    if !r.url_app_handler_is_authoritative() {
        r.register(
            "org/springframework/core/io/UrlResource",
            "getInputStream",
            "()Ljava/io/InputStream;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                if spring_dbg_enabled() {
                    eprintln!("[URLRES-DBG] UrlResource.getInputStream intercepted");
                }
                // Obtain the URL via UrlResource.getURL() — the public accessor
                // that simply returns the private `url` field.  Using a Java
                // call lets us avoid hard-coding the field index (which depends
                // on the number of fields in the AbstractResource/AbstractFile
                // ResolvingResource superclasses).
                let url_val = ctx.invoke_virtual(this, "getURL", "()Ljava/net/URL;", &[])?;
                let url_obj = match url_val {
                    Some(Value::Object(Some(o))) => o,
                    _ => return Err(ioex("UrlResource.getInputStream: getURL() returned null")),
                };
                // http(s) URLs: mirror Spring's REAL bytecode
                //   openConnection() → customizeConnection(con) → con.getInputStream()
                // instead of short-circuiting to URL.openStream(). Two reasons
                // (ResourceTests):
                //   * `customizeConnection(HttpURLConnection)` subclass overrides
                //     stage request headers (canCustomizeHttpUrlConnectionForRead
                //     asserts its Framework-Name header reaches the server) — the
                //     openStream shortcut never ran them;
                //   * the connection path performs the exchange inside GC-safe
                //     blocking regions (http_url_connection.rs::perform), so an
                //     in-process MockWebServer keeps serving during the read
                //     (openStream's exchange previously EAGAIN-timed-out under a
                //     concurrent STW).
                // `this`/`url_obj`/`con` are pinned across the up-calls below:
                // toExternalForm/openConnection/customizeConnection run arbitrary
                // Java (allocations can move objects).
                let this_pin = ctx.pin_native_root(this);
                let url_pin = ctx.pin_native_root(url_obj);
                let ext_val = ctx.invoke(
                    "java/net/URL",
                    "toExternalForm",
                    "()Ljava/lang/String;",
                    &[Value::Object(Some(url_obj))],
                );
                let this = ctx.read_native_pin(this_pin, this);
                let url_obj = ctx.read_native_pin(url_pin, url_obj);
                let ext = match ext_val {
                    Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
                    _ => String::new(),
                };
                if ext.starts_with("http://") || ext.starts_with("https://") {
                    let con_val = ctx.invoke_virtual(
                        url_obj,
                        "openConnection",
                        "()Ljava/net/URLConnection;",
                        &[],
                    );
                    let this = ctx.read_native_pin(this_pin, this);
                    let con = match con_val {
                        Ok(Some(Value::Object(Some(c)))) => c,
                        Err(e) => {
                            ctx.unpin_native_roots(this_pin);
                            return Err(e);
                        }
                        _ => {
                            ctx.unpin_native_roots(this_pin);
                            return Err(ioex(
                                "UrlResource.getInputStream: openConnection returned null",
                            ));
                        }
                    };
                    let con_pin = ctx.pin_native_root(con);
                    // Protected on AbstractFileResolvingResource; virtual dispatch
                    // reaches subclass overrides (its real bytecode no-ops the
                    // useCaches hook and forwards to the HttpURLConnection overload).
                    let cc = ctx.invoke_virtual(
                        this,
                        "customizeConnection",
                        "(Ljava/net/URLConnection;)V",
                        &[Value::Object(Some(con))],
                    );
                    let con = ctx.read_native_pin(con_pin, con);
                    if let Err(e) = cc {
                        ctx.unpin_native_roots(this_pin);
                        return Err(e);
                    }
                    // Perform the request (headers now staged) and inspect the
                    // status BEFORE handing out the body stream. Two cases must
                    // keep the legacy openStream behaviour the connection path
                    // lacks:
                    //   * 3xx — the connection path does not follow redirects,
                    //     while URL.openStream (and the real JDK's default
                    //     followRedirects) does;
                    //   * -1 — huc_real_perform folds connect failures into -1
                    //     per the getResponseCode contract, which must not become
                    //     a silent EMPTY stream here; openStream re-attempts and
                    //     raises the real IOException.
                    // A SocketTimeoutException from getResponseCode propagates
                    // as-is (no fallback), matching the real JDK.
                    let code_val = ctx.invoke_virtual(con, "getResponseCode", "()I", &[]);
                    let con = ctx.read_native_pin(con_pin, con);
                    let url_obj = ctx.read_native_pin(url_pin, url_obj);
                    ctx.unpin_native_roots(this_pin);
                    let code = match code_val {
                        Ok(Some(Value::Int(c))) => c,
                        Err(e) => return Err(e),
                        _ => -1,
                    };
                    if code == -1 || matches!(code, 301 | 302 | 303 | 307 | 308) {
                        return ctx.invoke_virtual(
                            url_obj,
                            "openStream",
                            "()Ljava/io/InputStream;",
                            &[],
                        );
                    }
                    return ctx.invoke_virtual(
                        con,
                        "getInputStream",
                        "()Ljava/io/InputStream;",
                        &[],
                    );
                }
                ctx.unpin_native_roots(this_pin);
                // Everything else: delegate directly to URL.openStream() — our
                // native handles jar:file: double-nested URLs correctly.
                if spring_dbg_enabled() {
                    eprintln!("[URLRES-DBG] UrlResource.getInputStream -> openStream");
                }
                ctx.invoke_virtual(url_obj, "openStream", "()Ljava/io/InputStream;", &[])
            }, NativeKind::SyntheticStub,
        );
    }

    let huc = "java/net/HttpURLConnection";
    r.register(huc, "connect", "()V", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        huc_perform(ctx, &mut this)?;
        Ok(None)
    }, NativeKind::SyntheticStub);
    r.register(huc, "getResponseCode", "()I", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        huc_perform(ctx, &mut this)?;
        Ok(Some(ctx.get_field(this, HUC_CODE)))
    }, NativeKind::SyntheticStub);
    r.register(
        huc,
        "getInputStream",
        "()Ljava/io/InputStream;",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            // S111r22 — For non-HTTP/HTTPS URLs (jar:file:, file:, classpath:, jrt:),
            // huc_perform would try to make a real HTTP request which fails. Instead,
            // delegate to URL.openStream() via the stored HUC_URL. This enables
            // SpringFactoriesLoader to read META-INF/spring.factories from nested JARs
            // when it goes through url.openConnection().getInputStream().
            let url_str = huc_url_string(ctx, &mut this);
            if !url_str.starts_with("http://") && !url_str.starts_with("https://") {
                // Non-HTTP: delegate to URL.openStream() on the stored URL object.
                let url_obj = match ctx.get_field(this, HUC_URL) {
                    Value::Object(Some(o)) => o,
                    _ => {
                        return Err(ioex(
                            "HttpURLConnection.getInputStream: no URL for non-http",
                        ))
                    }
                };
                return ctx.invoke_virtual(url_obj, "openStream", "()Ljava/io/InputStream;", &[]);
            }
            huc_perform(ctx, &mut this)?;
            let body = match ctx.get_field(this, HUC_BODY) {
                Value::Object(Some(a)) => a,
                _ => ctx.new_array(ArrayElementType::Byte, 0),
            };
            let len = ctx.array_length(body) as i32;
            // gc-common w20-d: the body is rooted across the stream's
            // allocation (it was stored through its pre-allocation address).
            let body_pin = ctx.pin_native_root(body);
            let stream = try_alloc_concurrent_synthetic(ctx, "java/io/ByteArrayInputStream", 4);
            let body = ctx.read_native_pin(body_pin, body);
            ctx.unpin_native_roots(body_pin);
            let stream = stream?;
            ctx.set_field(stream, 0, Value::Object(Some(body))); // buf
            ctx.set_field(stream, 1, Value::Int(0)); // pos
            ctx.set_field(stream, 2, Value::Int(0)); // mark
            ctx.set_field(stream, 3, Value::Int(len)); // count
            Ok(Some(Value::Object(Some(stream))))
        }, NativeKind::SyntheticStub,
    );
    // The generic carrier for `file:` and other non-HTTP URLs is a synthetic
    // HttpURLConnection so its getInputStream native wins over URLConnection's
    // real-JDK default body. Its inherited metadata accessors would otherwise
    // read uninitialised URLConnection fields and report zero. Preserve the
    // actual file timestamp for both direct getLastModified() callers and the
    // getHeaderFieldDate("last-modified", ...) shape used by Spring Boot's
    // JarUrlConnectionTests and NestedUrlConnectionTests.
    r.register(huc, "getLastModified", "()J", |ctx, args| {
        // A user subclass gets `URLConnection.getLastModified` itself, whose
        // `getHeaderFieldDate` accepts the two legacy date formats this
        // native's `parse_rfc1123_date_millis` does not. MEASURED
        // (`L6HttpLogicSweep` row 101: an RFC 850 `Last-Modified` read 0).
        uc_subclass_guard!(ctx, args, "getLastModified", "()J");
        let mut this = obj_arg(args, 0)?;
        let url = huc_origin_url_string(ctx, &mut this);
        let value = if url.starts_with("http://") || url.starts_with("https://") {
            // This class has TWO independent native registrations for
            // HttpURLConnection (see http_url_connection.rs) -- that one
            // owns setRequestMethod/getHeaderField's actual dispatch, but
            // this file's OWN huc_perform reads a separate HUC_METHOD
            // field that setRequestMethod never populates when the other
            // implementation wins. Calling huc_perform directly here
            // silently sent GET instead of the caller's configured method
            // and always returned 0 (ResourceTests.remoteResourceExists*).
            // Delegate to getHeaderField (an ordinary virtual dispatch, so
            // it always reaches whichever implementation actually wins)
            // instead of duplicating the request logic.
            //
            // gc-common w20-d: the connection is pinned across the name's
            // allocation and read back as the call's receiver.
            let this_pin = ctx.pin_native_root(this);
            let name = ctx.create_string("last-modified");
            let this = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            match ctx.invoke_virtual(
                this,
                "getHeaderField",
                "(Ljava/lang/String;)Ljava/lang/String;",
                &[Value::Object(Some(name))],
            ) {
                Ok(Some(Value::Object(Some(s)))) => ctx
                    .read_string(s)
                    .and_then(|v| parse_rfc1123_date_millis(&v))
                    .unwrap_or(0),
                _ => 0,
            }
        } else {
            synthetic_resource_url_last_modified(&url)
        };
        Ok(Some(Value::Long(value)))
    }, NativeKind::SyntheticStub);
    r.register(
        huc,
        "getHeaderFieldDate",
        "(Ljava/lang/String;J)J",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            let name =
                value_or_string(ctx, args.get(1).copied().unwrap_or(Value::Object(None)), "");
            let fallback = match args.get(2).copied() {
                Some(Value::Long(value)) => value,
                _ => 0,
            };
            let url = huc_origin_url_string(ctx, &mut this);
            if !url.starts_with("http://") && !url.starts_with("https://") {
                if name.eq_ignore_ascii_case("last-modified") {
                    let modified = synthetic_resource_url_last_modified(&url);
                    // RFC 1123 dates carry whole-second precision. FileURLConnection
                    // therefore rounds the filesystem's millisecond timestamp down
                    // before exposing it as the `last-modified` header date.
                    let header_date = modified / 1_000 * 1_000;
                    return Ok(Some(Value::Long(if header_date == 0 {
                        fallback
                    } else {
                        header_date
                    })));
                }
                return Ok(Some(Value::Long(fallback)));
            }
            // Real HTTP(S) response: parse whatever the actual header says
            // (any date-valued header, not just last-modified -- matches
            // real `HttpURLConnection.getHeaderFieldDate`, which is generic).
            // Delegate to getHeaderField -- see the sibling fix/comment in
            // `getLastModified` above for why this file's own huc_perform
            // must not be called directly here.
            //
            // gc-common w20-d: pinned across the name's allocation.
            let this_pin = ctx.pin_native_root(this);
            let name_obj = ctx.create_string(&name);
            this = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            let parsed = match ctx.invoke_virtual(
                this,
                "getHeaderField",
                "(Ljava/lang/String;)Ljava/lang/String;",
                &[Value::Object(Some(name_obj))],
            ) {
                Ok(Some(Value::Object(Some(s)))) => ctx
                    .read_string(s)
                    .and_then(|v| parse_rfc1123_date_millis(&v)),
                _ => None,
            };
            Ok(Some(Value::Long(parsed.unwrap_or(fallback))))
        }, NativeKind::SyntheticStub,
    );
    r.register(
        huc,
        "getHeaderField",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            // gc-common w20-d: the key is read BEFORE `huc_perform`, which
            // allocates (the `args` slice is not remapped), and `huc_perform`
            // hands `this` back current. The pin that stood here pinned the
            // address `huc_url_string` could already have vacated.
            let key_val = args.get(1).copied().unwrap_or(Value::Object(None));
            let key = value_or_string(ctx, key_val, "");
            huc_perform(ctx, &mut this)?;
            match huc_find_header_value(ctx, this, &key) {
                Some(val) => Ok(Some(Value::Object(Some(ctx.create_string(&val))))),
                None => Ok(Some(Value::Object(None))),
            }
        }, NativeKind::SyntheticStub,
    );
    r.register(
        huc,
        "getHeaderField",
        "(I)Ljava/lang/String;",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            huc_perform(ctx, &mut this)?;
            let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
            if idx < 0 {
                return Ok(Some(Value::Object(None)));
            }
            if let Value::Object(Some(arr)) = ctx.get_field(this, HUC_RESP_HEADERS) {
                let len = ctx.array_length(arr);
                if (idx as usize) < len {
                    if let Value::Object(Some(s)) = ctx.get_array_element(arr, idx as usize) {
                        let line = ctx.read_string(s).unwrap_or_default();
                        if let Some(colon) = line.find(':') {
                            let v = line[colon + 1..].trim().to_string();
                            let out = ctx.create_string(&v);
                            return Ok(Some(Value::Object(Some(out))));
                        }
                    }
                }
            }
            Ok(Some(Value::Object(None)))
        }, NativeKind::SyntheticStub,
    );
    r.register(huc, "getContentLength", "()I", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let url = huc_url_string(ctx, &mut this);
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Ok(Some(Value::Int(-1)));
        }
        huc_perform(ctx, &mut this)?;
        match ctx.get_field(this, HUC_BODY) {
            Value::Object(Some(a)) => Ok(Some(Value::Int(ctx.array_length(a) as i32))),
            _ => Ok(Some(Value::Int(-1))),
        }
    }, NativeKind::SyntheticStub);
    r.register(
        huc,
        "setRequestMethod",
        "(Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let mval = args.get(1).copied().unwrap_or(Value::Object(None));
            let method = value_or_string(ctx, mval, "GET");
            // gc-common w14-a: `create_string` can move `this`; pinned across it.
            let this_pin = ctx.pin_native_root(this);
            let s = ctx.create_string(&method);
            let this = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            ctx.set_field(this, HUC_METHOD, Value::Object(Some(s)));
            Ok(None)
        }, NativeKind::SyntheticStub,
    );
    r.register(huc, "disconnect", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, HUC_CONNECTED, Value::Int(0));
        ctx.set_field(this, HUC_FD, Value::Int(-1));
        Ok(None)
    }, NativeKind::SyntheticStub);
    r.register(huc, "setDoInput", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
        ctx.set_field(this, HUC_DO_INPUT, Value::Int(v));
        Ok(None)
    }, NativeKind::SyntheticStub);
    r.register(huc, "setDoOutput", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        // Real-JDK carrier (slot 0 holds the real `URLConnection.url` object,
        // not our synthetic conn-id int): set the REAL `doOutput` field by name
        // so the inherited `getDoOutput()` bytecode reports the caller's value.
        // Writing the synthetic `HUC_DO_OUTPUT` slot on a real object aliases
        // `connected` — it leaves `doOutput` false (Spring then skips the
        // request-body write → server blocks on the promised Content-Length →
        // "Read timed out") AND spuriously marks the connection connected.
        if matches!(ctx.get_field(this, 0), Value::Object(Some(_))) {
            ctx.set_field_by_name(this, "doOutput", Value::Int(v));        } else {
            ctx.set_field(this, HUC_DO_OUTPUT, Value::Int(v));        }
        Ok(None)
    }, NativeKind::SyntheticStub);

    // -----------------------------------------------------------------------
    // S111r24 — Spring Boot / Spring Framework compatibility natives.
    //
    // `UrlResource.getInputStream()` (Spring 5.x / 6.x) calls
    //   openConnection() → customizeConnection(con) → con.getInputStream()
    // where `customizeConnection` is defined on AbstractFileResolvingResource:
    //
    //   protected void customizeConnection(URLConnection con) {
    //       ResourceUtils.useCachesIfNecessary(con);  // ← getClass().getSimpleName()
    //       if (con instanceof HttpURLConnection) {
    //           customizeConnection((HttpURLConnection) con);
    //       }
    //   }
    //
    // `ResourceUtils.useCachesIfNecessary` calls
    //   con.getClass().getSimpleName().startsWith("JNLP")
    // then con.setUseCaches(false).  The `setUseCaches` is already a no-op
    // native on URLConnection; however any hiccup in the getSimpleName chain
    // (e.g. the Class mirror for our synthetic HttpURLConnection having a null
    // name slot) would throw an exception NOT covered by UrlResource's IOException
    // handler (offset 10 is outside the [13,17) try block), silently aborting
    // the getInputStream() call and leaving loadSpringFactories with no entries.
    //
    // Fix: intercept `ResourceUtils.useCachesIfNecessary` and both overloads of
    // `AbstractFileResolvingResource.customizeConnection` as native no-ops.
    // This lets execution fall straight through to con.getInputStream().
    // -----------------------------------------------------------------------

    // ResourceUtils.useCachesIfNecessary — IMPLEMENTED rather than no-op'd.
    // Spring's body is
    //   con.setUseCaches(con.getClass().getSimpleName().startsWith("JNLP"));
    // The blanket no-op predates `URLConnection.setUseCaches` being implemented
    // above (it now really writes `useCaches`), so it had become a silent drop
    // of Spring's "do NOT cache this connection" request — the flag Spring
    // Boot's nested `JarUrlConnection` reads to pick its close-on-close path,
    // and the one that keeps a jar from being held open after use. The original
    // reason for the stub was a `getClass().getSimpleName()` chain that could
    // throw out of `UrlResource.getInputStream()`'s try block; we sidestep it
    // entirely by deriving the simple name from the VM's own class table
    // instead of invoking the Java reflection chain.
    r.register(
        "org/springframework/util/ResourceUtils",
        "useCachesIfNecessary",
        "(Ljava/net/URLConnection;)V",
        |ctx, args| {
            let con = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let cid = ctx.class_id_of_object(con);
            let binary = ctx.class_name_of_id(cid).unwrap_or_default();
            let simple = binary.rsplit(&['/', '$'][..]).next().unwrap_or("");
            let use_caches = i32::from(simple.starts_with("JNLP"));
            // Errors are swallowed: the no-op this replaces did nothing at all,
            // so a failed setter leaves us exactly where we were before.
            let _ = ctx.invoke_virtual(con, "setUseCaches", "(Z)V", &[Value::Int(use_caches)]);
            Ok(None)
        }, NativeKind::SyntheticStub,
    );

    // S111r24 — Some loader paths can hand a null URL into
    // ResourceUtils.isFileURL/isJarURL during fallback probing. HotSpot-side
    // Spring code expects "not file/jar" in this case; without a guard we
    // NPE on `url.getProtocol()` and abort startup.
    r.register(
        "org/springframework/util/ResourceUtils",
        "isFileURL",
        "(Ljava/net/URL;)Z",
        |ctx, args| {
            if crate::nbflags().dbg_sbload {
                eprintln!("[DBG_SBLOAD] ResourceUtils.isFileURL native override");
            }
            let url = match args.get(0) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let proto = match ctx.invoke_virtual(url, "getProtocol", "()Ljava/lang/String;", &[])? {
                Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let is_file = proto == "file" || proto == "vfsfile" || proto == "vfs";
            Ok(Some(Value::Int(if is_file { 1 } else { 0 })))
        }, NativeKind::SyntheticStub,
    );
    r.register(
        "org/springframework/util/ResourceUtils",
        "isJarURL",
        "(Ljava/net/URL;)Z",
        |ctx, args| {
            if crate::nbflags().dbg_sbload {
                eprintln!("[DBG_SBLOAD] ResourceUtils.isJarURL native override");
            }
            let url = match args.get(0) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let proto = match ctx.invoke_virtual(url, "getProtocol", "()Ljava/lang/String;", &[])? {
                Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let is_jar = matches!(proto.as_str(), "jar" | "war" | "zip" | "vfszip" | "wsjar");
            Ok(Some(Value::Int(if is_jar { 1 } else { 0 })))
        }, NativeKind::SyntheticStub,
    );

    // S111r25 — Banner lookup is best-effort; unresolved classpath URLs can
    // trip `new UrlResource(null)` on some fallback paths. The original fix
    // here unconditionally returned null from both banner resolvers to keep
    // boot moving — but that also permanently disabled a WORKING custom
    // `banner.txt`/`banner.gif` lookup, since `getTextBanner`'s real bytecode
    // (`resourceLoader.getResource(location)`) never even ran anymore
    // (SpringApplicationTests.customBanner/customBannerWithProperties/
    // failureInANativeImageWritesFailureToSystemOut always printed the
    // DEFAULT SpringBootBanner instead of the test's `@WithResource
    // banner.txt`). Reimplement the real logic instead — `getBanner()`'s
    // `Environment.getProperty` / `ResourceLoader.getResource` /
    // `Resource.exists()` / `Resource.getURL()` calls, `ResourceBanner`
    // construction — but keep the S111r25 defensive intent by swallowing
    // ANY failure along the way (not just the real method's checked
    // `IOException`) and falling back to null, same as before.
    r.register(
        "org/springframework/boot/SpringApplicationBannerPrinter",
        "getTextBanner",
        "(Lorg/springframework/core/env/Environment;)Lorg/springframework/boot/Banner;",
        |ctx, args| {
            if crate::nbflags().dbg_sbload {
                eprintln!(
                    "[DBG_SBLOAD] SpringApplicationBannerPrinter.getTextBanner native override"
                );
            }
            let this = match obj_arg(args, 0) {
                Ok(o) => o,
                Err(_) => return Ok(Some(Value::Object(None))),
            };
            let environment = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let resource_loader = match ctx.get_field_by_name(this, "resourceLoader") {
                Value::Object(Some(o)) => o,
                _ => return Ok(Some(Value::Object(None))),
            };
            // gc-common w20-d: every step below is an allocation or a Java
            // call. The environment and the loader were held raw across the
            // two Strings (the first String across the second) and
            // `getProperty`, and the resource across `exists`, `getURL` and
            // `toExternalForm` before it was handed to `ResourceBanner`.
            // Rooted, each read back before its use.
            let mut scope = NativeHandleScope::new(ctx);
            let environment_h = scope.root(environment);
            let loader_h = scope.root(resource_loader);
            let prop_name = scope.create_string("spring.banner.location");
            let prop_h = scope.root(prop_name);
            let default_loc = scope.create_string("banner.txt");
            let environment = scope.get(&environment_h);
            let prop_name = scope.get(&prop_h);
            let location = match scope.invoke_virtual(
                environment,
                "getProperty",
                "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
                &[
                    Value::Object(Some(prop_name)),
                    Value::Object(Some(default_loc)),
                ],
            ) {
                Ok(Some(Value::Object(Some(s)))) => s,
                _ => return Ok(Some(Value::Object(None))),
            };
            let resource_loader = scope.get(&loader_h);
            let resource = match scope.invoke_virtual(
                resource_loader,
                "getResource",
                "(Ljava/lang/String;)Lorg/springframework/core/io/Resource;",
                &[Value::Object(Some(location))],
            ) {
                Ok(Some(Value::Object(Some(r)))) => r,
                _ => return Ok(Some(Value::Object(None))),
            };
            let resource_h = scope.root(resource);
            let exists = matches!(
                scope.invoke_virtual(resource, "exists", "()Z", &[]),
                Ok(Some(Value::Int(n))) if n != 0
            );
            if !exists {
                return Ok(Some(Value::Object(None)));
            }
            let resource = scope.get(&resource_h);
            let ctx = &mut *scope;
            let is_liquibase = match ctx.invoke_virtual(resource, "getURL", "()Ljava/net/URL;", &[])
            {
                Ok(Some(Value::Object(Some(url)))) => {
                    match ctx.invoke_virtual(url, "toExternalForm", "()Ljava/lang/String;", &[]) {
                        Ok(Some(Value::Object(Some(s)))) => ctx
                            .read_string(s)
                            .map(|s| s.contains("liquibase-core"))
                            .unwrap_or(false),
                        _ => false,
                    }
                }
                _ => false,
            };
            if is_liquibase {
                return Ok(Some(Value::Object(None)));
            }
            let resource = scope.get(&resource_h);
            match scope.new_object_initialized(
                "org/springframework/boot/ResourceBanner",
                "(Lorg/springframework/core/io/Resource;)V",
                &[Value::Object(Some(resource))],
            ) {
                Ok(v) => Ok(v),
                Err(_) => Ok(Some(Value::Object(None))),
            }
        }, NativeKind::SyntheticStub,
    );
    // KEEP the null. `null` is Spring's own "there is no image banner" answer
    // (the real method returns null whenever no banner.gif/jpg/png resource is
    // found), and it is the only honest one here: `ImageBanner` rasterises the
    // resource through `javax.imageio.ImageIO` + `java.awt.image`, which
    // CratonVM does not provide — returning a Banner we cannot print would fail
    // later and further from the cause.
    r.register(
        "org/springframework/boot/SpringApplicationBannerPrinter",
        "getImageBanner",
        "(Lorg/springframework/core/env/Environment;)Lorg/springframework/boot/Banner;",
        |_ctx, _args| Ok(Some(Value::Object(None))), NativeKind::SyntheticStub,
    );

    // S111r27 — Keep optional integrations truly optional. Spring computes
    // several static "xxxPresent" flags via ClassUtils.isPresent(...); when
    // these flip true under partial emulation, later probes may dive into
    // missing subsystems (JSF) and destabilize bootstrap.
    //
    // Groovy note (2026-07-07): this stub used to also force `groovy.*` to
    // absent, because letting Spring's `DelegatingSmartContextLoader` pick the
    // Groovy context loader used to wedge the VM in Groovy's shaded ANTLR4
    // compiler (millions of gc::guard OOB-field warnings, no completion). The
    // real culprit was OUR force-registered ANTLR ATN fast-path shim
    // (`native_antlr_atn_config_init` & friends in lib.rs, added e48e14cc)
    // applying standard-ANTLR4 `ATNConfig` slot indices (reachesIntoOuterContext=3,
    // semanticContext=4) to the `groovyjarjarantlr4` shaded copy — which is the
    // tunnelvisionlabs fork whose base `ATNConfig` legitimately has only THREE
    // fields (state / packed altAndOuterContextDepth / context) with subclass
    // fields at slots 3..5. Fixed by 50119adb (fork-aware packed layout +
    // `antlr_groovy_atn_special_slot`), so `groovy.*` presence is now decided
    // honestly by Spring's bytecode implementation below and `.groovy` bean scripts load
    // through the real `GenericGroovyXmlContextLoader`. See
    // test-context-constructor-param-annotation-offset.md.
    r.register(
        "org/springframework/util/ClassUtils",
        "isPresent",
        "(Ljava/lang/String;Ljava/lang/ClassLoader;)Z",
        |ctx, args| {
            let name = match args.first() {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            if name == "jakarta.faces.context.FacesContext" {
                return Ok(Some(Value::Int(0)));
            }
            // Preserve Spring's loader-specific semantics, including its
            // canonical-inner-name fallback (`Outer.Inner` -> `Outer$Inner`).
            // A global native lookup incorrectly reported such present classes
            // absent during auto-configuration exclusion validation.
            ctx.invoke_special_bytecode_only(
                "org/springframework/util/ClassUtils",
                "isPresent",
                "(Ljava/lang/String;Ljava/lang/ClassLoader;)Z",
                args,
            )
        }, NativeKind::SyntheticStub,
    );

    // S111r26 — JSF integration is optional; when the Faces API is absent,
    // Spring's FacesDependencyRegistrar probe should effectively no-op.
    // In our current runtime, that probe can escalate into hard failure via
    // NoClassDefFoundError on jakarta.faces.*. Short-circuit it.
    //
    // NOTE: the blanket `registerWebApplicationScopes` no-ops that used to sit
    // here are gone (context.annotation cluster fix). They pre-dated S111r27's
    // more surgical `ClassUtils.isPresent` fix below, which makes the real
    // `WebApplicationContextUtils.jsfPresent` static flag correctly compute
    // `false` for "jakarta.faces.context.FacesContext" — so the real
    // `registerWebApplicationScopes` body already skips
    // `FacesDependencyRegistrar` on its own and safely runs
    // `beanFactory.registerScope(SCOPE_REQUEST/SCOPE_SESSION, ...)`. Shadowing
    // it here was dropping those `registerScope` calls entirely, breaking
    // every plain (non-Boot) `GenericWebApplicationContext.refresh()` with
    // "IllegalStateException: No Scope registered for scope name 'request'/
    // 'session'" (ClassPathBeanDefinitionScannerJsr330ScopeIntegrationTests,
    // ClassPathBeanDefinitionScannerScopeIntegrationTests). Only the
    // `FacesDependencyRegistrar` probe itself stays intercepted below, as a
    // defense-in-depth backstop (it should now be unreachable).
    //
    // NARROWED (wave 4): tested against the SAME condition the real caller
    // tests (`jsfPresent`) instead of no-op'ing unconditionally. As written the
    // no-op was a landmine: if the `ClassUtils.isPresent` special-case above is
    // ever removed, this would silently drop the real
    // `registerResolvableDependency(FacesContext/ExternalContext, ...)` calls on
    // a genuine JSF classpath and every `@Inject FacesContext` would fail with
    // no diagnostic. Now the skip happens only when jakarta.faces really is
    // absent — the case this backstop was written for.
    r.register(
        "org/springframework/web/context/support/WebApplicationContextUtils$FacesDependencyRegistrar",
        "registerFacesDependencies",
        "(Lorg/springframework/beans/factory/config/ConfigurableListableBeanFactory;)V",
        |ctx, args| {
            if ctx
                .class_id_by_name("jakarta/faces/context/FacesContext")
                .is_some()
            {
                return ctx.invoke_special_bytecode_only(
                    "org/springframework/web/context/support/WebApplicationContextUtils$FacesDependencyRegistrar",
                    "registerFacesDependencies",
                    "(Lorg/springframework/beans/factory/config/ConfigurableListableBeanFactory;)V",
                    args,
                );
            }
            Ok(None)
        }, NativeKind::SyntheticStub,
    );

    // S111r57 — dispatch to Spring Boot's real generic
    // ServletWebServerApplicationContext.getWebServerFactory() implementation.
    // The generic context must select its own registered factory backend.
    //
    // In real Spring Boot, this protected method calls
    //   getBeanFactory().getBeanNamesForType(ServletWebServerFactory.class)
    // and throws MissingWebServerFactoryBeanException if zero matches. Under
    // CratonVM the auto-configuration that registers the Tomcat factory bean
    // never completes (Cglib/condition-evaluation issues upstream), so the
    // lookup fails. The old native short-circuit constructed a Tomcat factory
    // unconditionally, which is invalid for Jetty, Undertow, and generic modules:
    // they legitimately do not have Spring Boot's Tomcat implementation on their
    // class path. Execute the original bytecode body instead, which either finds
    // the registered backend or throws Spring's normal Java exception.
    //
    // SB 2.x: context = org/springframework/boot/web/servlet/context/ServletWebServerApplicationContext
    //         factory = org/springframework/boot/web/servlet/server/ServletWebServerFactory
    //         impl    = org/springframework/boot/web/embedded/tomcat/TomcatServletWebServerFactory
    //
    // SB 4.x: context = org/springframework/boot/web/server/servlet/context/ServletWebServerApplicationContext
    //         factory = org/springframework/boot/web/server/servlet/ServletWebServerFactory
    //         impl    = org/springframework/boot/tomcat/servlet/TomcatServletWebServerFactory

    // SB 4.x
    r.register(
        "org/springframework/boot/web/server/servlet/context/ServletWebServerApplicationContext",
        "getWebServerFactory",
        "()Lorg/springframework/boot/web/server/servlet/ServletWebServerFactory;",
        |ctx, args| {
            // gci i18/vp: a tail call (`continuation_pin::tail_call`); the body
            // creates beans (application constructors, the bean factory's
            // singleton lock), which may park.
            cratonvm_native_api::continuation_pin::tail_call(|| {
                ctx.invoke_special_bytecode_only(
                    "org/springframework/boot/web/server/servlet/context/ServletWebServerApplicationContext",
                    "getWebServerFactory",
                    "()Lorg/springframework/boot/web/server/servlet/ServletWebServerFactory;",
                    args,
                )
            })
        }, NativeKind::SyntheticStub,
    );

    // SB 2.x
    r.register(
        "org/springframework/boot/web/servlet/context/ServletWebServerApplicationContext",
        "getWebServerFactory",
        "()Lorg/springframework/boot/web/servlet/server/ServletWebServerFactory;",
        |ctx, args| {
            // gci i18/vp: a tail call, as the SB 4.x row above.
            cratonvm_native_api::continuation_pin::tail_call(|| {
                ctx.invoke_special_bytecode_only(
                    "org/springframework/boot/web/servlet/context/ServletWebServerApplicationContext",
                    "getWebServerFactory",
                    "()Lorg/springframework/boot/web/servlet/server/ServletWebServerFactory;",
                    args,
                )
            })
        }, NativeKind::SyntheticStub,
    );

    // CGLIB-δ — Spring WebFlux mirror of the servlet bypass above. Apps that
    // start a `ReactiveWebServerApplicationContext` (e.g. letsgo-gateway with
    // Spring Cloud Gateway) hit the same `MissingWebServerFactoryBeanException`
    // because CratonVM's CGLIB bypass skips the auto-config that registers
    // `nettyReactiveWebServerFactory` as a bean.
    //
    // The reactive context (SB 2.7 — `org/springframework/boot/web/reactive/context/
    // ReactiveWebServerApplicationContext`) calls:
    //   1. `getWebServerFactoryBeanName()` — looks up bean names, throws
    //      `MissingWebServerFactoryBeanException` if none. Native shim returns
    //      a synthetic bean name to bypass the throw site.
    //   2. `getWebServerFactory(String)` — `beanFactory.getBean(name, Class)`.
    //      Native shim constructs `NettyReactiveWebServerFactory` directly,
    //      ignoring the bean-name argument.
    //   3. `getBeanDefinition(name).isLazyInit()` — would still fail because no
    //      bean definition exists for our synthetic name. To dodge this entire
    //      chain we also no-op the private `createWebServer()` driver. The
    //      `serverManager` field stays null; `getWebServer()` will return null
    //      to the caller. This is a deliberate "boot past, don't actually
    //      serve" stance consistent with the Tomcat-side shims below.
    fn alloc_netty_reactive_factory(ctx: &mut dyn NativeContext) -> MethodCallResult {
        let impl_class =
            "org/springframework/boot/web/embedded/netty/NettyReactiveWebServerFactory";
        let obj_val = match ctx.new_object(impl_class) {
            Ok(Some(v)) => v,
            Ok(None) => return Ok(Some(Value::Object(None))),
            Err(e) => return Err(e),
        };
        let _ = ctx.invoke_special(impl_class, "<init>", "()V", &[obj_val]);
        Ok(Some(obj_val))
    }

    // SB 2.x reactive — primary path
    r.register(
        "org/springframework/boot/web/reactive/context/ReactiveWebServerApplicationContext",
        "getWebServerFactory",
        "(Ljava/lang/String;)Lorg/springframework/boot/web/reactive/server/ReactiveWebServerFactory;",
        |ctx, _args| alloc_netty_reactive_factory(ctx), NativeKind::SyntheticStub,
    );
    r.register(
        "org/springframework/boot/web/reactive/context/ReactiveWebServerApplicationContext",
        "getWebServerFactoryBeanName",
        "()Ljava/lang/String;",
        |ctx, _args| {
            let s = ctx.create_string("nettyReactiveWebServerFactory");
            Ok(Some(Value::Object(Some(s))))
        }, NativeKind::SyntheticStub,
    );
    // `createWebServer()` is the private driver that actually BUILDS the
    // reactive web server: it resolves the factory bean name, reads its bean
    // definition's `isLazyInit()`, constructs the `WebServerManager` (which,
    // when not lazy, calls `factory.getWebServer(handler)` and binds the port)
    // and registers the `webServerGracefulShutdown` / `webServerStartStop`
    // lifecycle singletons.
    //
    // IMPLEMENTED (wave 4). It used to be an unconditional no-op, "so the
    // downstream getBeanDefinition/isLazyInit/registerSingleton chain doesn't
    // run" — i.e. a `ReactiveWebServerApplicationContext` reported a successful
    // refresh with NO web server at all, no lifecycle beans, and
    // `getWebServer()` null. Nothing about that is a faithful constant; it is
    // the single largest fabrication left in this file.
    //
    // Run the real body. The chain it was avoiding is satisfiable now: the
    // sibling `getWebServerFactory` registration above hands back a real
    // `NettyReactiveWebServerFactory`, and `nettyReactiveWebServerFactory` is a
    // genuine bean definition in any app that pulls in
    // `ReactiveWebServerFactoryAutoConfiguration`, so `getBeanDefinition` no
    // longer has to fail. Where it still does (a hand-built context with no
    // such definition) fall back to the old skip — every failure point in the
    // real body precedes its first mutation, so the fallback cannot leave a
    // half-initialised context.
    fn reactive_create_web_server(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        // `createWebServer` is private, so it is declared ONLY on
        // ReactiveWebServerApplicationContext even when the receiver is the
        // AnnotationConfig subclass.
        match ctx.invoke_special_bytecode_only(
            "org/springframework/boot/web/reactive/context/ReactiveWebServerApplicationContext",
            "createWebServer",
            "()V",
            args,
        ) {
            Ok(v) => Ok(v),
            Err(_) => Ok(None),
        }
    }
    r.register(
        "org/springframework/boot/web/reactive/context/ReactiveWebServerApplicationContext",
        "createWebServer",
        "()V",
        reactive_create_web_server, NativeKind::SyntheticStub,
    );
    // The subclass used by Spring Boot's reactive auto-configuration —
    // dispatch resolves on the declaring class, but cover the subclass too
    // for safety in case bytecode binds the call to the subclass directly.
    r.register(
        "org/springframework/boot/web/reactive/context/AnnotationConfigReactiveWebServerApplicationContext",
        "createWebServer",
        "()V",
        reactive_create_web_server, NativeKind::SyntheticStub,
    );

    // residual-4 fix: the two `AbstractFileResolvingResource.customizeConnection`
    // no-ops above were REMOVED. They made `customizeConnection` a complete
    // no-op for every caller — including `AbstractFileResolvingResource.exists()`/
    // `contentLength()`/`lastModified()`, which call it directly (not just via
    // the separately-intercepted `UrlResource.getInputStream()` above) — so a
    // subclass overriding `customizeConnection(HttpURLConnection)` (e.g. to set
    // a custom request header) was silently never invoked, and
    // `useCachesIfNecessary`+the `instanceof HttpURLConnection` dispatch never
    // ran either. The original S111r24 motivation (`getClass().getSimpleName()`
    // throwing on a synthetic HttpURLConnection Class mirror, breaking
    // `UrlResource.getInputStream()`'s uncaught-exception path) is already
    // covered independently by the `UrlResource.getInputStream()` override
    // above, which bypasses `customizeConnection` entirely — and empirically,
    // `getClass().getSimpleName()` on the connection objects `customizeConnection`
    // actually receives now works fine (verified: real `sun.net.www.protocol.
    // http.HttpURLConnection`/`HttpsURLConnectionImpl` instances, not a synthetic
    // mirror). Letting the real bytecode run restores the real JDK contract:
    // `useCachesIfNecessary` + the `instanceof`-gated dispatch to
    // `customizeConnection(HttpURLConnection)`, which virtual-dispatches to
    // whatever subclass override exists (`UrlResource`'s own subclasses use
    // this to set custom request headers before `exists()`/`getInputStream()`
    // send the request — `ResourceTests.UrlResourceTests
    // .canCustomizeHttpUrlConnectionForExists[Fallback]`).;
}

// ===========================================================================
// RE.5 — java.net.http.HttpClient
// ===========================================================================

// Synthetic `java/net/http/HttpResponse` field layout used by the bare
// real-JDK `HttpClient` model. `HttpClient`/`HttpResponse` are abstract JDK
// types, so `newHttpClient()`/`send()` hand back synthetic instances whose
// runtime class IS the abstract class — every instance method therefore has to
// be registered directly on it (a real concrete `HttpClientImpl`/
// `HttpResponseImpl` is never materialised on this path).
//
// Client and builder configuration is deliberately represented by actual Java
// values, not presence flags.  Spring reads these values back through the JDK
// accessors and invokes ProxySelector during a request, so a one-field
// placeholder silently loses both identity and behaviour.
pub const RE5_CLIENT_VERSION: usize = 0;
pub const RE5_CLIENT_REDIRECT: usize = 1;
pub const RE5_CLIENT_CONNECT_TIMEOUT: usize = 2;
pub const RE5_CLIENT_SSL_CONTEXT: usize = 3;
pub const RE5_CLIENT_EXECUTOR: usize = 4;
pub const RE5_CLIENT_PROXY: usize = 5;
pub const RE5_CLIENT_AUTHENTICATOR: usize = 6;
pub const RE5_CLIENT_COOKIE_HANDLER: usize = 7;
pub const RE5_CLIENT_SSL_PARAMETERS: usize = 8;
pub const RE5_CLIENT_NUM_FIELDS: usize = 9;

/// `java.net.http.HttpClient` lifecycle state for the JDK 21+ `close()` /
/// `shutdown()` / `shutdownNow()` / `isTerminated()` / `awaitTermination()`
/// surface: `true` once a shutdown has been REQUESTED on that client.
///
/// Side-tabled rather than kept in a client field slot because HttpClient
/// carriers are allocated in three different shapes across this crate (9 slots
/// here, 10 in `http2.rs`, 1 in `phases_late/net_channels.rs`), so no slot index
/// is safe for all of them. Keyed by the client's `NativeObjKey` (its weak
/// lock key since gc-common w28-a, its identity hash before), which — like the
/// `SSLSessionContext` table above — survives a moving-GC relocation.
///
/// gc-common w16-a (`common-w16a-closed-httpclient-rows-outlive-the-client`):
/// a row is present once a shutdown was requested, and its value is the
/// client's address, its weak owner (`0` = none recorded, never judged).
/// [`gc_sweep_net_socket_rows`] drops a dead client's row. The row used to
/// stay until VM teardown: one per client closed (every try-with-resources
/// `HttpClient` on JDK 21+), and a later client handed the dead one's identity
/// hash answered `isTerminated() == true` before anyone closed it.
pub fn re5_client_shutdown_table() -> &'static Mutex<HashMap<NativeObjKey, usize>> {
    static T: OnceLock<Mutex<HashMap<NativeObjKey, usize>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

pub const RE5_BUILDER_VERSION: usize = 0;
pub const RE5_BUILDER_REDIRECT: usize = 1;
pub const RE5_BUILDER_CONNECT_TIMEOUT: usize = 2;
pub const RE5_BUILDER_SSL_CONTEXT: usize = 3;
pub const RE5_BUILDER_EXECUTOR: usize = 4;
pub const RE5_BUILDER_PROXY: usize = 5;
pub const RE5_BUILDER_AUTHENTICATOR: usize = 6;
pub const RE5_BUILDER_COOKIE_HANDLER: usize = 7;
pub const RE5_BUILDER_SSL_PARAMETERS: usize = 8;
pub const RE5_BUILDER_NUM_FIELDS: usize = 9;

pub fn re5_alloc_client(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    let client =
        try_alloc_concurrent_synthetic(ctx, "java/net/http/HttpClient", RE5_CLIENT_NUM_FIELDS)?;
    ctx.set_field(client, RE5_CLIENT_VERSION, Value::Object(None));
    ctx.set_field(client, RE5_CLIENT_REDIRECT, Value::Object(None));
    for field in [
        RE5_CLIENT_CONNECT_TIMEOUT,
        RE5_CLIENT_SSL_CONTEXT,
        RE5_CLIENT_EXECUTOR,
        RE5_CLIENT_PROXY,
        RE5_CLIENT_AUTHENTICATOR,
        RE5_CLIENT_COOKIE_HANDLER,
        RE5_CLIENT_SSL_PARAMETERS,
    ] {
        ctx.set_field(client, field, Value::Object(None));
    }
    Ok(client)
}

pub fn re5_alloc_client_builder(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    let builder = try_alloc_concurrent_synthetic(
        ctx,
        "java/net/http/HttpClient$Builder",
        RE5_BUILDER_NUM_FIELDS,
    )?;
    for field in 0..RE5_BUILDER_NUM_FIELDS {
        ctx.set_field(builder, field, Value::Object(None));
    }
    Ok(builder)
}

pub fn re5_optional(ctx: &mut dyn NativeContext, value: Value) -> MethodCallResult {
    ctx.invoke(
        "java/util/Optional",
        "ofNullable",
        "(Ljava/lang/Object;)Ljava/util/Optional;",
        &[value],
    )
}

pub fn re5_enum_name(ctx: &mut dyn NativeContext, value: Value) -> Option<String> {
    let obj = match value {
        Value::Object(Some(obj)) => obj,
        _ => return None,
    };
    let value = ctx
        .invoke_virtual(obj, "name", "()Ljava/lang/String;", &[])
        .ok()??;
    match value {
        Value::Object(Some(name)) => ctx.read_string(name),
        _ => None,
    }
}

pub const RE5_RESP_STATUS: usize = 0; // Int
pub const RE5_RESP_BODY_BYTES: usize = 1; // byte[]
pub const RE5_RESP_HEADERS: usize = 2; // String[] of "key: value"
pub const RE5_RESP_HANDLER_TAG: usize = 3; // String: how body() materialises
pub const RE5_RESP_BODY_OBJ: usize = 4; // Object: the RE5_TAG_HANDLED body value
                                    // The four added to close the HttpResponse registration gap (see
                                    // docs/feature-designs/native-dispatch-architecture.md):
                                    // this carrier used to answer only statusCode/body/headers, and
                                    // uri/request/version/previousResponse/sslSession -- the other five of
                                    // HttpResponse's eight instance accessors -- threw AbstractMethodError on a
                                    // live, default-mode send() call. `version()` needs no field: this whole
                                    // client only ever speaks HTTP/1.1 on the wire (`http_build_request`), so it
                                    // is a constant, not stored state.
pub const RE5_RESP_URI: usize = 5; // Object: java.net.URI, this hop's effective URL
pub const RE5_RESP_REQUEST: usize = 6; // Object: java.net.http.HttpRequest, this hop's request
pub const RE5_RESP_PREV: usize = 7; // Object: HttpResponse | null, previousResponse()
pub const RE5_RESP_SSL_SESSION: usize = 8; // Object: SSLSession | null, sslSession()
pub const RE5_RESP_NUM_FIELDS: usize = 9;

/// `RE5_RESP_HANDLER_TAG` value meaning "the body was produced by driving a
/// real user-supplied BodyHandler; `body()` returns `RE5_RESP_BODY_OBJ`".
pub const RE5_TAG_HANDLED: &str = "handled";

// Synthetic `java/net/http/HttpResponse$ResponseInfo` handed to a real
// BodyHandler's `apply` (see `re5_drive_body_handler`).
pub const RE5_RESPONSE_INFO: &str = "java/net/http/HttpResponse$ResponseInfo";
pub const RE5_RI_STATUS: usize = 0; // Int
pub const RE5_RI_HEADERS: usize = 1; // String[] of "key: value"
pub const RE5_RI_NUM_FIELDS: usize = 2;

// One-shot replay `Flow.Subscription` handed to a real BodySubscriber (see
// `re5_drive_body_handler`). Registered on its own wrapper class name -- NOT
// on `java/util/concurrent/Flow$Subscription` itself, whose `request`/
// `cancel` natives are already owned by the synthetic-Flow demand counters
// in `streams.rs`/`phases_late.rs` with an incompatible field layout.
// Abstract interface dispatch finds receiver-class natives (the
// `Enumeration$Impl` wrapper pattern, vm_exec C25).
pub const RE5_REPLAY_SUBSCRIPTION: &str = "cratonvm/net/HttpBodyReplaySubscription";
pub const RE5_SUB_SUBSCRIBER: usize = 0; // Flow.Subscriber the body is delivered to
pub const RE5_SUB_BODY: usize = 1; // byte[] wire body (null = empty body)
pub const RE5_SUB_STATE: usize = 2; // Int: 0 = pending, 1 = delivered, 2 = cancelled
pub const RE5_SUB_NUM_FIELDS: usize = 3;

/// Read a `byte[]` heap object into a `Vec<u8>` (high byte ignored).
pub fn re5_read_byte_array(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
    re5_read_byte_array_range(ctx, arr, 0, ctx.array_length(arr))
}

/// Read a slice of a `byte[]` heap object into a `Vec<u8>`.
pub fn re5_read_byte_array_range(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    off: usize,
    len: usize,
) -> Vec<u8> {
    let cap = ctx.array_length(arr);
    let start = off.min(cap);
    let end = start.saturating_add(len).min(cap);
    let mut out = Vec::with_capacity(end.saturating_sub(start));
    for i in start..end {
        if let Value::Int(b) = ctx.get_array_element(arr, i) {
            out.push((b & 0xff) as u8);
        }
    }
    out
}

/// Classify the BodyHandler passed to `send`/`sendAsync`.
///
/// Our synthetic `BodyHandlers` factories stash a tag string in slot 0
/// ("string" / "inputstream" / "bytearray" / "discarding") — those (and a
/// missing/null handler, which degrades to an InputStream body) return
/// `Some(tag)`, and `HttpResponse.body()` materialises the raw wire bytes
/// per the tag.
///
/// Any OTHER object is a real user-supplied `BodyHandler` implementation —
/// e.g. Spring's `JdkClientHttpRequest$DecompressingBodyHandler` (wraps
/// `ofInputStream` in a GZIP/Inflater stream) or the `BodyHandlers
/// .ofPublisher()` lambda (reactive `JdkClientHttpConnector`) — and returns
/// `None`: the caller must drive the real BodyHandler protocol via
/// [`re5_drive_body_handler`]. The pre-fix version silently defaulted these
/// to a raw InputStream, skipping the user's body transformation entirely
/// (gzip/deflate response bodies reached Spring still compressed —
/// `JdkClientHttpRequestFactoryTests.compressionGzip/compressionDeflate`).
pub fn re5_handler_tag(ctx: &dyn NativeContext, handler: Option<Value>) -> Option<String> {
    if let Some(Value::Object(Some(h))) = handler {
        let cid = ctx.class_id_of_object(h);
        if ctx.class_name_arc_of_id(cid).as_deref()
            == Some("java/net/http/HttpResponse$BodyHandler")
        {
            if let Value::Object(Some(s)) = ctx.get_field(h, 0) {
                if let Some(tag) = ctx.read_string(s) {
                    return Some(tag);
                }
            }
            // Synthetic-but-tagless: keep the legacy InputStream default
            // (a bare synthetic handler has no real apply() to drive).
            return Some("inputstream".to_string());
        }
        return None;
    }
    Some("inputstream".to_string())
}

/// A synthetic `HttpResponse.BodyHandler` whose slot 0 is `tag` (the shape
/// [`re5_handler_tag`] reads).
///
/// gc-common w20-d: the one body the four `BodyHandlers` factories carried
/// inline, with the tag String first and rooted across the handler's
/// allocation: each factory held the handler raw across the String's
/// allocation (a collecting one) and stored the tag through it.
pub fn re5_tagged_body_handler(ctx: &mut dyn NativeContext, tag: &str) -> MethodCallResult {
    let tag = ctx.create_string(tag);
    let mut scope = NativeHandleScope::new(ctx);
    let tag_h = scope.root(tag);
    let ctx = &mut *scope;
    let bh = try_alloc_concurrent_synthetic(ctx, "java/net/http/HttpResponse$BodyHandler", 1)?;
    let tag = scope.get(&tag_h);
    scope.set_field(bh, 0, Value::Object(Some(tag)));
    Ok(Some(Value::Object(Some(bh))))
}

/// Build the synthetic `java/net/http/HttpResponse` carrying the wire result.
pub fn re5_build_response(
    ctx: &mut dyn NativeContext,
    status: i32,
    headers: &[(String, String)],
    body: &[u8],
    handler_tag: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    // gc-common w20-d: the payload first, each piece rooted, and the response
    // last. The response was held raw across the body array, the header
    // array, one String per header and the tag, and the header array across
    // its Strings.
    let mut scope = NativeHandleScope::new(ctx);
    let body_arr = scope.new_array(ArrayElementType::Byte, body.len());
    for (i, b) in body.iter().enumerate() {
        scope.set_array_element(body_arr, i, Value::Int(*b as i32));
    }
    let body_h = scope.root(body_arr);
    let hdr_arr = scope.new_ref_array(ClassId::new(0), headers.len());
    let hdr_h = scope.root(hdr_arr);
    for (i, (k, v)) in headers.iter().enumerate() {
        let s = scope.create_string(&format!("{k}: {v}"));
        let hdr_arr = scope.get(&hdr_h);
        scope.set_array_element(hdr_arr, i, Value::Object(Some(s)));
    }
    let tag = scope.create_string(handler_tag);
    let tag_h = scope.root(tag);
    let ctx = &mut *scope;
    let out =
        try_alloc_concurrent_synthetic(ctx, "java/net/http/HttpResponse", RE5_RESP_NUM_FIELDS)?;
    let body_arr = scope.get(&body_h);
    let hdr_arr = scope.get(&hdr_h);
    let tag = scope.get(&tag_h);
    let ctx = &mut *scope;
    ctx.set_field(out, RE5_RESP_STATUS, Value::Int(status));
    ctx.set_field(out, RE5_RESP_BODY_BYTES, Value::Object(Some(body_arr)));
    ctx.set_field(out, RE5_RESP_HEADERS, Value::Object(Some(hdr_arr)));
    ctx.set_field(out, RE5_RESP_HANDLER_TAG, Value::Object(Some(tag)));
    // The four fields the registration-gap fix added (see RE5_RESP_URI's own
    // comment). Always written explicitly here to a safe default — never left
    // to the allocator's fill — exactly the discipline `re5_new_request_builder`
    // documents for the same reason: an unwritten reference slot is the shape
    // of the defect this carrier exists to not repeat. `re5_finish_response`,
    // called by every site that allocates a response via this function,
    // overwrites them with real values where this hop has any.
    ctx.set_field(out, RE5_RESP_URI, Value::Object(None));
    ctx.set_field(out, RE5_RESP_REQUEST, Value::Object(None));
    ctx.set_field(out, RE5_RESP_PREV, Value::Object(None));
    ctx.set_field(out, RE5_RESP_SSL_SESSION, Value::Object(None));
    Ok(out)
}

/// Populate the four fields `re5_build_response` leaves at their safe-null
/// default: the effective URI and request for THIS hop, the
/// `previousResponse()` chain (recursing into `resp.prev`), and a real
/// `SSLSession` when this hop negotiated TLS through the rustls-backed
/// exchange. Called once for the top-level response `re5_do_request` returns,
/// and recursively for every redirect hop underneath it. Returns the
/// (possibly relocated) reference to `out` — every caller must use the
/// RETURNED value, not the one it passed in; see the pinning comments inline.
pub fn re5_finish_response(
    ctx: &mut dyn NativeContext,
    out: ObjectRef,
    resp: &HttpResponse,
) -> Result<ObjectRef, MethodCallFailed> {
    let mut out = out;
    if !resp.url.is_empty() {
        let out_pin = ctx.pin_native_root(out);
        let uri_obj = re5_make_uri(ctx, &resp.url)?;
        out = ctx.read_native_pin(out_pin, out);
        ctx.unpin_native_roots(out_pin);
        ctx.set_field(out, RE5_RESP_URI, Value::Object(Some(uri_obj)));

        let out_pin = ctx.pin_native_root(out);
        let req_obj = re5_make_request_snapshot(ctx, &resp.method, &resp.url)?;
        out = ctx.read_native_pin(out_pin, out);
        ctx.unpin_native_roots(out_pin);
        ctx.set_field(out, RE5_RESP_REQUEST, Value::Object(Some(req_obj)));
    }
    if let Some(prev) = &resp.prev {
        // `out` must survive the ENTIRE nested build (which allocates
        // repeatedly, and may itself recurse further down the chain), so it is
        // pinned across the whole sub-call rather than around one allocation
        // at a time. `prev_obj` needs no pin of its own here: it is freshly
        // returned by `re5_finish_response` and used immediately, with only a
        // non-allocating pin refresh for `out` in between.
        let out_pin = ctx.pin_native_root(out);
        let prev_obj =
            re5_build_response(ctx, prev.status, &prev.headers, &prev.body, "inputstream")?;
        let prev_obj = re5_finish_response(ctx, prev_obj, prev)?;
        out = ctx.read_native_pin(out_pin, out);
        ctx.unpin_native_roots(out_pin);
        ctx.set_field(out, RE5_RESP_PREV, Value::Object(Some(prev_obj)));
    }
    if let Some(tls) = &resp.tls {
        let out_pin = ctx.pin_native_root(out);
        let session_obj = re5_build_ssl_session(ctx, tls, &resp.url)?;
        out = ctx.read_native_pin(out_pin, out);
        ctx.unpin_native_roots(out_pin);
        ctx.set_field(out, RE5_RESP_SSL_SESSION, Value::Object(Some(session_obj)));
    }
    Ok(out)
}

/// Build a `java.net.URI` from its external-form string — the mirror of
/// `re5_uri_string`, and the same `URI.create` pattern `HttpRequest.uri()`'s
/// own registration below already uses. Runs real bytecode (`URI.create` is
/// not itself natively overridden), so a caller holding another reference
/// across this call must pin it — see `re5_finish_response`.
pub fn re5_make_uri(ctx: &mut dyn NativeContext, url: &str) -> Result<ObjectRef, MethodCallFailed> {
    let s = ctx.create_string(url);
    match ctx.invoke(
        "java/net/URI",
        "create",
        "(Ljava/lang/String;)Ljava/net/URI;",
        &[Value::Object(Some(s))],
    )? {
        Some(Value::Object(Some(uri))) => Ok(uri),
        _ => Err(ioex(
            "URI.create returned null for HttpResponse.uri()/request().uri()",
        )),
    }
}

/// A minimal, real `java.net.http.HttpRequest` for one hop of a redirect
/// chain (or the top-level response): method and URI only, which is what
/// `HttpResponse.request()`'s callers actually inspect. Uses the SAME
/// `RE5_REQUEST_*` field layout every `HttpRequest`/`Builder` this file mints
/// already carries (`re5_new_request_builder`), so the request's own
/// already-registered accessors (`method()`, `uri()`, `version()`, ...) work
/// on it unmodified.
pub fn re5_make_request_snapshot(
    ctx: &mut dyn NativeContext,
    method: &str,
    url: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    // Same batching discipline as `re5_new_request_builder`: both strings are
    // created and pinned BEFORE the object that will hold them, so neither
    // allocating the other string nor allocating the request object itself
    // can leave a stale reference in a field this constructor is about to
    // write.
    let m = ctx.create_string(method);
    let m_pin = ctx.pin_native_root(m);
    let u = ctx.create_string(url);
    let u_pin = ctx.pin_native_root(u);
    let req =
        try_alloc_concurrent_synthetic(ctx, "java/net/http/HttpRequest", RE5_REQUEST_NUM_FIELDS)?;
    let m = ctx.read_native_pin(m_pin, m);
    let u = ctx.read_native_pin(u_pin, u);
    ctx.unpin_native_roots(u_pin);
    ctx.unpin_native_roots(m_pin);
    ctx.set_field(req, RE5_REQUEST_METHOD, Value::Object(Some(m)));
    ctx.set_field(req, RE5_REQUEST_URI, Value::Object(Some(u)));
    ctx.set_field(req, RE5_REQUEST_BODY, Value::Object(None));
    ctx.set_field(req, RE5_REQUEST_HEADERS, Value::Object(None));
    ctx.set_field(req, RE5_REQUEST_TIMEOUT_FIELD, Value::Object(None));
    ctx.set_field(req, RE5_REQUEST_EXPECT_CONTINUE, Value::Int(0));
    ctx.set_field(req, RE5_REQUEST_VERSION, Value::Object(None));
    ctx.set_field(req, RE5_REQUEST_BODY_PUBLISHER, Value::Object(None));
    Ok(req)
}

/// Build a real `javax.net.ssl.SSLSession` from a completed rustls handshake
/// this file captured (`Re5TlsSessionInfo`, filled in by `http_exchange_rustls`)
/// — the SAME 8-field layout and field semantics
/// `t27_tls::build_synthetic_ssl_session` uses for the SSLEngine-based doors
/// (cipher=0, protocol=1, valid=2, peerHost=3, peerPort=4, creationTime=5,
/// alpn=6, attrs=7 — see that function and the width table on
/// `t27_tls::session_cipher_slot`/`session_proto_slot`), so every
/// already-registered `SSLSession` accessor works on this object unmodified.
/// Real data throughout: a response only reaches this function when
/// `http_exchange_rustls` returned `Ok`, i.e. this is a genuinely negotiated
/// connection, not a stub standing in for one.
pub fn re5_build_ssl_session(
    ctx: &mut dyn NativeContext,
    tls: &Re5TlsSessionInfo,
    url: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let (_, host, port, _, _) =
        http_parse_url(url).unwrap_or_else(|_| (true, String::new(), 0, String::new(), None));
    // Same batching discipline as `build_synthetic_ssl_session`: every string
    // this object's fields will hold is created and pinned BEFORE the object
    // itself, so a moving collection inside a later allocation cannot leave a
    // stale reference in a field this constructor is about to write.
    let cipher_s = ctx.create_string(&tls.cipher_suite);
    let cipher_pin = ctx.pin_native_root(cipher_s);
    let proto_s = ctx.create_string(&tls.protocol);
    let proto_pin = ctx.pin_native_root(proto_s);
    let host_s = ctx.create_string(&host);
    let host_pin = ctx.pin_native_root(host_s);
    let ses = crate::t27_tls::alloc_session_carrier(ctx, 8)?;
    // gc-common w11-a: a new session object starts with no side-table rows a
    // collected session left under its identity key.
    crate::t27_tls::forget_session_rows(ctx, ses);
    let cipher_s = ctx.read_native_pin(cipher_pin, cipher_s);
    let proto_s = ctx.read_native_pin(proto_pin, proto_s);
    let host_s = ctx.read_native_pin(host_pin, host_s);
    ctx.unpin_native_roots(host_pin);
    ctx.unpin_native_roots(proto_pin);
    ctx.unpin_native_roots(cipher_pin);
    crate::t27_tls::sess_set(&*ctx, ses, 0, Value::Object(Some(cipher_s)));
    crate::t27_tls::sess_set(&*ctx, ses, 1, Value::Object(Some(proto_s)));
    // A response only reaches this function once `http_exchange_rustls` has
    // returned `Ok` — the handshake actually completed, so this is always a
    // genuinely negotiated session. Mirrors `build_synthetic_ssl_session`'s
    // OWN `negotiated` predicate, specialised: there is no "engine that never
    // connected" state on this path the way there is for a bare `SSLEngine`.
    crate::t27_tls::sess_set(&*ctx, ses, 2, Value::Int(1));
    crate::t27_tls::sess_set(&*ctx, ses, 3, Value::Object(Some(host_s)));
    crate::t27_tls::sess_set(
        &*ctx,
        ses,
        4,
        Value::Int(if port == 0 { -1 } else { i32::from(port) }),
    );
    crate::t27_tls::sess_set(&*ctx, ses, 5, Value::Long(crate::epoch_millis_now()));
    // No ALPN negotiation on this client — it only ever sends HTTP/1.1 on the
    // wire (see `RE5_RESP_URI`'s own comment on `version()`) — so slot 6 stays
    // at the allocator's default, the same way `build_synthetic_ssl_session`
    // leaves it when nothing negotiated one.
    crate::t27_tls::sess_set(&*ctx, ses, 6, Value::Object(None));
    if !tls.peer_certs_der.is_empty() {
        crate::t27_tls::record_client_peer_chain(ctx, ses, tls.peer_certs_der.clone());
    }
    Ok(ses)
}

/// Build the synthetic `java.net.http.HttpHeaders` view over a `String[]` of
/// `"key: value"` lines (the shape both the synthetic `HttpResponse` and the
/// synthetic `ResponseInfo` carry). The array is pinned across the
/// allocation so a moving collector can't leave the stored reference stale.
pub fn re5_make_http_headers(
    ctx: &mut dyn NativeContext,
    hdr_arr: Value,
) -> Result<ObjectRef, MethodCallFailed> {
    let pinned = match hdr_arr {
        Value::Object(Some(a)) => Some((ctx.pin_native_root(a), a)),
        _ => None,
    };
    let headers = try_alloc_concurrent_synthetic(ctx, "java/net/http/HttpHeaders", 1)?;
    let arr_now = match pinned {
        Some((pin, a)) => Value::Object(Some(ctx.read_native_pin(pin, a))),
        None => Value::Object(None),
    };
    ctx.set_field(headers, 0, arr_now);
    if let Some((pin, _)) = pinned {
        ctx.unpin_native_roots(pin);
    }
    Ok(headers)
}

/// `Flow.Subscription.request(long)` for the one-shot replay subscription
/// handed to real BodySubscribers by [`re5_drive_body_handler`]: on the
/// first positive demand, deliver the parked wire body as a single
/// `List.of(ByteBuffer)` `onNext` followed by `onComplete`, then drop the
/// parked references. Zero/negative or repeat demand is a no-op (the
/// reactive-streams spec says negative demand should `onError`; every JDK
/// `BodySubscriber` requests positive demand, so stay lenient).
///
/// Delivery is demand-driven rather than pushed eagerly from the driver
/// because not every `BodySubscriber` buffers: `BodySubscribers
/// .ofPublisher()`'s pass-through forwards items straight to a downstream
/// subscriber that only attaches (and signals demand) later, on a different
/// thread (Reactor, for Spring's reactive `JdkClientHttpConnector`) —
/// pushing before that demand would drop the body on the floor. All state
/// lives in the subscription's own Java fields (GC-traced), never in
/// cross-call native `ObjectRef`s.
pub fn re5_replay_subscription_request(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let n = match args.get(1).copied() {
        Some(Value::Long(v)) => v,
        Some(Value::Int(v)) => v as i64,
        _ => 0,
    };
    if n <= 0 || !matches!(ctx.get_field(this, RE5_SUB_STATE), Value::Int(0)) {
        return Ok(None);
    }
    // Claim delivery BEFORE invoking the subscriber: `onNext` commonly
    // re-enters `request` (e.g. `HttpResponseInputStream` requests the next
    // list while consuming the current one).
    ctx.set_field(this, RE5_SUB_STATE, Value::Int(1));
    let subscriber = match ctx.get_field(this, RE5_SUB_SUBSCRIBER) {
        Value::Object(Some(s)) => s,
        _ => return Ok(None),
    };
    let body = ctx.get_field(this, RE5_SUB_BODY);
    let this_pin = ctx.pin_native_root(this);
    let subscriber_pin = ctx.pin_native_root(subscriber);
    let deliver = |ctx: &mut dyn NativeContext| -> MethodCallResult {
        if let Value::Object(Some(arr)) = body {
            if ctx.array_length(arr) > 0 {
                let arr_pin = ctx.pin_native_root(arr);
                let arr_now = ctx.read_native_pin(arr_pin, arr);
                let bb = match ctx.invoke(
                    "java/nio/ByteBuffer",
                    "wrap",
                    "([B)Ljava/nio/ByteBuffer;",
                    &[Value::Object(Some(arr_now))],
                )? {
                    Some(Value::Object(Some(b))) => b,
                    _ => return Err(ioex("ByteBuffer.wrap returned null")),
                };
                let bb_pin = ctx.pin_native_root(bb);
                let bb_now = ctx.read_native_pin(bb_pin, bb);
                let list = match ctx.invoke(
                    "java/util/Collections",
                    "singletonList",
                    "(Ljava/lang/Object;)Ljava/util/List;",
                    &[Value::Object(Some(bb_now))],
                )? {
                    Some(v @ Value::Object(Some(_))) => v,
                    _ => return Err(ioex("Collections.singletonList returned null")),
                };
                let subscriber_now = ctx.read_native_pin(subscriber_pin, subscriber);
                // BodySubscriber<T> extends Flow.Subscriber<List<ByteBuffer>>;
                // the erased/bridge signature takes Object.
                ctx.invoke_virtual(subscriber_now, "onNext", "(Ljava/lang/Object;)V", &[list])?;
            }
        }
        let subscriber_now = ctx.read_native_pin(subscriber_pin, subscriber);
        ctx.invoke_virtual(subscriber_now, "onComplete", "()V", &[])?;
        Ok(None)
    };
    let result = deliver(ctx)?;
    // Drop the parked references so the delivered body can be collected.
    let this_now = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this_now, RE5_SUB_SUBSCRIBER, Value::Object(None));
    ctx.set_field(this_now, RE5_SUB_BODY, Value::Object(None));
    ctx.unpin_native_roots(this_pin);
    Ok(result)
}

/// The bytes of [`RE5_REPLAY_SUBSCRIPTION`] as a REAL class, compiled from
/// `class_bytes/HttpBodyReplaySubscription.java` (`javac --release 17`).
///
/// Under `--real-jdk` the fabricated carrier above cannot exist -- the class is
/// refused (`NoClassDefFoundError: cratonvm/net/HttpBodyReplaySubscription`, the
/// second fatal defect of `W7-24` section 4) -- and no public JDK class can take
/// its place: `Flow.Subscription`'s natives count demand and never deliver,
/// `SubmissionPublisher` moves delivery off the caller's thread, and the class
/// HotSpot returns is `jdk.internal`. What W7-24 said would have to exist is "a
/// `Flow.Subscription` implementation with real bytecode and synchronous
/// replay-on-first-request semantics", defined from bytes the VM ships; this is
/// it. Its `request`/`cancel` are the Java form of
/// [`re5_replay_subscription_request`] / [`re5_replay_subscription_cancel`].
pub const RE5_REAL_REPLAY_SUBSCRIPTION_BYTES: &[u8] =
    include_bytes!("../class_bytes/HttpBodyReplaySubscription.class");

/// Define (once per VM) and instantiate the real replay subscription.
pub fn re5_new_real_replay_subscription(
    ctx: &mut dyn NativeContext,
    subscriber: ObjectRef,
    body: Value,
) -> Result<ObjectRef, MethodCallFailed> {
    if ctx.class_id_by_name(RE5_REPLAY_SUBSCRIPTION).is_none()
        && ctx
            .define_class_from_bytes(RE5_REPLAY_SUBSCRIPTION, RE5_REAL_REPLAY_SUBSCRIPTION_BYTES)
            .is_none()
    {
        return Err(ioex(
            "cannot define the replay Flow.Subscription for HttpClient body delivery",
        ));
    }
    match ctx.new_object_initialized(
        RE5_REPLAY_SUBSCRIPTION,
        "(Ljava/util/concurrent/Flow$Subscriber;[B)V",
        &[Value::Object(Some(subscriber)), body],
    )? {
        Some(Value::Object(Some(sub))) => Ok(sub),
        _ => Err(ioex("replay Flow.Subscription construction returned null")),
    }
}

/// `Flow.Subscription.cancel()` for the one-shot replay subscription: mark
/// cancelled and drop the parked references. Idempotent; a cancel after
/// delivery is a no-op (the fields are already cleared).
pub fn re5_replay_subscription_cancel(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if matches!(ctx.get_field(this, RE5_SUB_STATE), Value::Int(0)) {
        ctx.set_field(this, RE5_SUB_STATE, Value::Int(2));
        ctx.set_field(this, RE5_SUB_SUBSCRIBER, Value::Object(None));
        ctx.set_field(this, RE5_SUB_BODY, Value::Object(None));
    }
    Ok(None)
}

/// Drive the real `java.net.http` BodyHandler protocol against a
/// user-supplied handler, exactly as the real client would:
///
/// ```text
/// subscriber = handler.apply(responseInfo)
/// subscriber.onSubscribe(replaySubscription)   // request(n) pulls the body
/// body = subscriber.getBody().toCompletableFuture().join()
/// ```
///
/// Returns the handler-produced body value `T` (e.g. a `GZIPInputStream`
/// for Spring's `DecompressingBodyHandler`, a `Flow.Publisher` for
/// `BodyHandlers.ofPublisher()`), which `HttpResponse.body()` then hands
/// back verbatim (tag [`RE5_TAG_HANDLED`]).
///
/// `join()` cannot deadlock for the JDK's own subscriber shapes:
/// `ofInputStream`/`ofPublisher` complete their `getBody()` stage
/// immediately/at `onSubscribe`, and eagerly-buffering shapes (`ofString`,
/// `ofByteArray`) signal demand during `onSubscribe`, which makes the
/// replay subscription deliver the whole body + `onComplete` before
/// `getBody()` is even called.
pub fn re5_drive_body_handler(
    ctx: &mut dyn NativeContext,
    handler: ObjectRef,
    status: i32,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<Value, MethodCallFailed> {
    let handler_pin = ctx.pin_native_root(handler);

    // ResponseInfo synthetic: statusCode + the same "key: value" String[]
    // shape the synthetic HttpResponse carries.
    let ri = try_alloc_concurrent_synthetic(ctx, RE5_RESPONSE_INFO, RE5_RI_NUM_FIELDS)?;
    ctx.set_field(ri, RE5_RI_STATUS, Value::Int(status));
    let ri_pin = ctx.pin_native_root(ri);
    let hdr_arr = ctx.new_ref_array(ClassId::new(0), headers.len());
    let hdr_pin = ctx.pin_native_root(hdr_arr);
    for (i, (k, v)) in headers.iter().enumerate() {
        let s = ctx.create_string(&format!("{k}: {v}"));
        let hdr_now = ctx.read_native_pin(hdr_pin, hdr_arr);
        ctx.set_array_element(hdr_now, i, Value::Object(Some(s)));
    }
    {
        let ri_now = ctx.read_native_pin(ri_pin, ri);
        let hdr_now = ctx.read_native_pin(hdr_pin, hdr_arr);
        ctx.set_field(ri_now, RE5_RI_HEADERS, Value::Object(Some(hdr_now)));
    }

    // subscriber = handler.apply(responseInfo)
    let subscriber = {
        let handler_now = ctx.read_native_pin(handler_pin, handler);
        let ri_now = ctx.read_native_pin(ri_pin, ri);
        match ctx.invoke_virtual(
            handler_now,
            "apply",
            "(Ljava/net/http/HttpResponse$ResponseInfo;)Ljava/net/http/HttpResponse$BodySubscriber;",
            &[Value::Object(Some(ri_now))],
        )? {
            Some(Value::Object(Some(s))) => s,
            _ => return Err(ioex("HttpResponse.BodyHandler.apply returned null")),
        }
    };
    let subscriber_pin = ctx.pin_native_root(subscriber);

    // Park subscriber + body bytes in the replay subscription's own Java
    // fields (GC-traced; delivery may happen later, on another thread).
    let body_arr = if body.is_empty() {
        None
    } else {
        let arr = ctx.new_array(ArrayElementType::Byte, body.len());
        for (i, b) in body.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(*b as i32));
        }
        Some((ctx.pin_native_root(arr), arr))
    };
    let subscription = if ctx.is_jdk_only() {
        // Strict: a REAL class, defined from shipped bytes -- the fabricated
        // carrier below is refused, and no public JDK class delivers a body
        // synchronously on the first `request(n)`.
        let subscriber_now = ctx.read_native_pin(subscriber_pin, subscriber);
        let body_val = match body_arr {
            Some((pin, a)) => Value::Object(Some(ctx.read_native_pin(pin, a))),
            None => Value::Object(None),
        };
        re5_new_real_replay_subscription(ctx, subscriber_now, body_val)?
    } else {
        let subscription =
            try_alloc_concurrent_synthetic(ctx, RE5_REPLAY_SUBSCRIPTION, RE5_SUB_NUM_FIELDS)?;
        let subscriber_now = ctx.read_native_pin(subscriber_pin, subscriber);
        ctx.set_field(
            subscription,
            RE5_SUB_SUBSCRIBER,
            Value::Object(Some(subscriber_now)),
        );
        let body_val = match body_arr {
            Some((pin, a)) => Value::Object(Some(ctx.read_native_pin(pin, a))),
            None => Value::Object(None),
        };
        ctx.set_field(subscription, RE5_SUB_BODY, body_val);
        ctx.set_field(subscription, RE5_SUB_STATE, Value::Int(0));
        subscription
    };
    let subscription_pin = ctx.pin_native_root(subscription);
    {
        let subscriber_now = ctx.read_native_pin(subscriber_pin, subscriber);
        let subscription_now = ctx.read_native_pin(subscription_pin, subscription);
        ctx.invoke_virtual(
            subscriber_now,
            "onSubscribe",
            "(Ljava/util/concurrent/Flow$Subscription;)V",
            &[Value::Object(Some(subscription_now))],
        )?;
    }

    // body = subscriber.getBody().toCompletableFuture().join()
    let stage = {
        let subscriber_now = ctx.read_native_pin(subscriber_pin, subscriber);
        match ctx.invoke_virtual(
            subscriber_now,
            "getBody",
            "()Ljava/util/concurrent/CompletionStage;",
            &[],
        )? {
            Some(Value::Object(Some(s))) => s,
            _ => return Err(ioex("BodySubscriber.getBody returned null")),
        }
    };
    // `getBody()` may return a CompletableFuture.MinimalStage, whose
    // `join()` throws UnsupportedOperationException -- always convert via
    // `toCompletableFuture()` first.
    let stage_pin = ctx.pin_native_root(stage);
    let cf = {
        let stage_now = ctx.read_native_pin(stage_pin, stage);
        match ctx.invoke_virtual(
            stage_now,
            "toCompletableFuture",
            "()Ljava/util/concurrent/CompletableFuture;",
            &[],
        )? {
            Some(Value::Object(Some(c))) => c,
            _ => return Err(ioex("CompletionStage.toCompletableFuture returned null")),
        }
    };
    let result = ctx.invoke_virtual(cf, "join", "()Ljava/lang/Object;", &[])?;
    ctx.unpin_native_roots(handler_pin);
    Ok(result.unwrap_or(Value::Object(None)))
}

pub const RE5_BODY_COLLECTOR_SUBSCRIBER: &str = "java/util/concurrent/Flow$Subscriber";
pub const RE5_PUBLISHER_WAIT: Duration = Duration::from_secs(10);

#[derive(Default)]
pub struct Re5PublisherBodyState {
    pub bytes: Vec<u8>,
    pub completed: bool,
    pub error: Option<String>,
    // Identity-preserving companion to `error`: a global GC root for the
    // ORIGINAL Throwable the Flow.Subscriber's onError delivered (set
    // alongside `error` by `re5_body_collector_on_error`, which runs on a
    // different Java thread than the one waiting in
    // `re5_collect_publisher_body`, hence a global root rather than a pin).
    // Real HotSpot propagates a request-body-publisher failure through
    // `HttpClient.sendAsync()`'s CompletableFuture as the SAME exception
    // object the publisher threw (confirmed empirically: `ClientHttpConnectorTests
    // .errorInRequestBody`'s `assertThat(throwable).isSameAs(error)` passes
    // 3/3 clean under real JDK 25). Without this, `re5_collect_publisher_body`
    // could only reconstruct a brand-new synthetic `IOException` from a text
    // message, which can never satisfy an identity (`isSameAs`) assertion --
    // not a timing flake, a structural identity loss for every request that
    // fails this way on the `Jdk` connector.
    pub error_obj_root: Option<usize>,
    // gc-common w15-a: set, under this state's lock, by
    // `re5_collect_publisher_body` once it has taken `error_obj_root` and
    // stopped reading. An `onError` that looked the collector up before the
    // collector left the table, and reaches this state only afterwards,
    // releases its own root instead of leaving it here for nobody.
    pub abandoned: bool,
}

#[derive(Default)]
pub struct Re5PublisherBodyCollector {
    pub state: StdMutex<Re5PublisherBodyState>,
    pub done: Condvar,
}

pub fn re5_body_collectors() -> &'static Mutex<HashMap<u64, Arc<Re5PublisherBodyCollector>>> {
    static COLLECTORS: OnceLock<Mutex<HashMap<u64, Arc<Re5PublisherBodyCollector>>>> =
        OnceLock::new();
    COLLECTORS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn re5_next_body_collector_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed).max(1)
}

pub fn re5_body_collector_id(ctx: &dyn NativeContext, subscriber: ObjectRef) -> Option<u64> {
    match ctx.get_field(subscriber, 0) {
        Value::Long(v) if v > 0 => Some(v as u64),
        Value::Int(v) if v > 0 => Some(v as u64),
        _ => None,
    }
}

pub fn re5_lookup_body_collector(
    ctx: &dyn NativeContext,
    subscriber: ObjectRef,
) -> Option<Arc<Re5PublisherBodyCollector>> {
    let id = re5_body_collector_id(ctx, subscriber)?;
    re5_body_collectors().lock().get(&id).cloned()
}

pub fn re5_is_byte_array(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    ctx.heap_kind_of(obj) == ObjectKind::Array
        && ctx.heap_element_type_of(obj) == ArrayElementType::Byte
}

pub fn re5_set_bytebuffer_position(ctx: &dyn NativeContext, bb: ObjectRef, pos: i32) {
    ctx.set_field_by_name(bb, "position", Value::Int(pos));
    ctx.set_field(bb, 1, Value::Int(pos));
}

pub fn re5_read_bytebuffer_fields(ctx: &dyn NativeContext, bb: ObjectRef) -> Option<Vec<u8>> {
    if let Value::Object(Some(arr)) = ctx.get_field_by_name(bb, "hb") {
        let pos = ctx.get_field_by_name(bb, "position").as_int().unwrap_or(0);
        let lim = ctx.get_field_by_name(bb, "limit").as_int().unwrap_or(pos);
        let off = ctx.get_field_by_name(bb, "offset").as_int().unwrap_or(0);
        let start = off.saturating_add(pos).max(0) as usize;
        let len = lim.saturating_sub(pos).max(0) as usize;
        let out = re5_read_byte_array_range(ctx, arr, start, len);
        re5_set_bytebuffer_position(ctx, bb, lim);
        return Some(out);
    }

    let arr = match ctx.get_field(bb, 0) {
        Value::Object(Some(a)) if re5_is_byte_array(ctx, a) => a,
        _ => return None,
    };
    let pos = ctx.get_field(bb, 1).as_int().unwrap_or(0);
    let lim = ctx.get_field(bb, 2).as_int().unwrap_or(pos);
    let len = lim.saturating_sub(pos).max(0) as usize;
    let out = re5_read_byte_array_range(ctx, arr, pos.max(0) as usize, len);
    re5_set_bytebuffer_position(ctx, bb, lim);
    Some(out)
}

pub fn re5_read_and_consume_bytebuffer(
    ctx: &mut dyn NativeContext,
    bb: ObjectRef,
) -> Result<Vec<u8>, MethodCallFailed> {
    if let Some(bytes) = re5_read_bytebuffer_fields(ctx, bb) {
        return Ok(bytes);
    }

    // gc-common w15-a: `bb` is pinned across `remaining()` (Java) and the
    // scratch array's allocation, and re-read for `get`. It used to be handed
    // to `get` from its pre-allocation address.
    let bb_pin = ctx.pin_native_root(bb);
    let rem = match ctx.invoke_virtual(bb, "remaining", "()I", &[]) {
        Ok(Some(Value::Int(n))) if n > 0 => n as usize,
        Ok(_) => {
            ctx.unpin_native_roots(bb_pin);
            return Ok(Vec::new());
        }
        Err(e) => {
            ctx.unpin_native_roots(bb_pin);
            return Err(e);
        }
    };
    let tmp = ctx.new_array(ArrayElementType::Byte, rem);
    let tmp_pin = ctx.pin_native_root(tmp);
    let bb = ctx.read_native_pin(bb_pin, bb);
    let get_result = ctx.invoke_virtual(
        bb,
        "get",
        "([B)Ljava/nio/ByteBuffer;",
        &[Value::Object(Some(tmp))],
    );
    let tmp = ctx.read_native_pin(tmp_pin, tmp);
    // Releases `tmp_pin` too.
    ctx.unpin_native_roots(bb_pin);
    get_result?;
    Ok(re5_read_byte_array(ctx, tmp))
}

pub fn re5_body_item_bytes(
    ctx: &mut dyn NativeContext,
    item: Value,
) -> Result<Vec<u8>, MethodCallFailed> {
    let obj = match item {
        Value::Object(Some(o)) => o,
        _ => return Ok(Vec::new()),
    };
    if re5_is_byte_array(ctx, obj) {
        return Ok(re5_read_byte_array(ctx, obj));
    }

    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(obj))
        .unwrap_or_default();
    if class_name == "java/lang/String" {
        return Ok(ctx.read_string(obj).unwrap_or_default().into_bytes());
    }
    if class_name.contains("ByteBuffer") || class_name == "java/nio/ByteBuffer" {
        return re5_read_and_consume_bytebuffer(ctx, obj);
    }
    Ok(Vec::new())
}

pub fn re5_throwable_text(ctx: &mut dyn NativeContext, value: Value) -> String {
    let obj = match value {
        Value::Object(Some(o)) => o,
        _ => return "publisher signalled an error".to_string(),
    };
    // gc-common w20-d: the class is read BEFORE `toString()` (Java); the
    // fallback read it through the throwable's pre-call address.
    let class_id = ctx.class_id_of_object(obj);
    if let Ok(Some(Value::Object(Some(s)))) =
        ctx.invoke_virtual(obj, "toString", "()Ljava/lang/String;", &[])
    {
        if let Some(text) = ctx.read_string(s) {
            return text;
        }
    }
    ctx.class_name_of_id(class_id)
        .unwrap_or_else(|| "publisher signalled an error".to_string())
}

pub fn re5_body_collector_on_subscribe(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(subscription))) = args.get(1).copied() {
        ctx.invoke_virtual(subscription, "request", "(J)V", &[Value::Long(i64::MAX)])?;
    }
    Ok(None)
}

pub fn re5_body_collector_on_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // gc-common w15-a: the collector is found (a field read of `this`) BEFORE
    // the item is decoded. Decoding a `ByteBuffer` runs `remaining()` / `get`
    // and allocates, and `this` is the `args` copy, which a moving collection
    // does not rewrite: the lookup used to read the id out of a possibly
    // vacated slot, and a miss silently dropped the chunk from the body.
    let collector = re5_lookup_body_collector(ctx, this);
    let bytes = re5_body_item_bytes(ctx, args.get(1).copied().unwrap_or(Value::Object(None)))?;
    if let Some(collector) = collector {
        let mut state = collector.state.lock().unwrap();
        state.bytes.extend_from_slice(&bytes);
        collector.done.notify_all();
    }
    Ok(None)
}

pub fn re5_body_collector_on_error(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // gc-common w15-a: the collector is looked up and the Throwable rooted
    // BEFORE `re5_throwable_text` runs `toString()` (Java, allocates). Both
    // used to come from the `args` copies after it, so a moving collection
    // there made the lookup read a vacated slot and rooted the Throwable's
    // OLD address -- the object `re5_collect_publisher_body` then rethrew.
    let collector = re5_lookup_body_collector(ctx, this);
    let throwable_val = args.get(1).copied().unwrap_or(Value::Object(None));
    // Root the actual Throwable object (if any) so `re5_collect_publisher_body`
    // -- woken on a different thread, possibly after a GC moves it -- can
    // rethrow the SAME object instead of only a text description. A global
    // root (not a pin) is required: `on_error` and the collect/wait side run
    // on different Java threads, so there is no shared native-call frame to
    // pin against.
    let error_obj_root = match throwable_val {
        Value::Object(Some(obj)) => Some(ctx.add_global_root(obj)),
        _ => None,
    };
    let throwable_now = match error_obj_root.and_then(|h| ctx.resolve_global_root(h)) {
        Some(obj) => Value::Object(Some(obj)),
        None => throwable_val,
    };
    let msg = re5_throwable_text(ctx, throwable_now);
    if let Some(collector) = collector {
        let mut state = collector.state.lock().unwrap();
        if state.abandoned {
            // The collecting side has already taken its answer and left.
            drop(state);
            if let Some(handle) = error_obj_root {
                ctx.remove_global_root(handle);
            }
            return Ok(None);
        }
        state.error = Some(msg);
        state.error_obj_root = error_obj_root;
        state.completed = true;
        collector.done.notify_all();
    } else if let Some(handle) = error_obj_root {
        // No collector (already timed out / removed) to hand the root to --
        // avoid leaking it.
        ctx.remove_global_root(handle);
    }
    Ok(None)
}

pub fn re5_body_collector_on_complete(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(collector) = re5_lookup_body_collector(ctx, this) {
        let mut state = collector.state.lock().unwrap();
        state.completed = true;
        collector.done.notify_all();
    }
    Ok(None)
}

/// Which JDK door a re5 `HttpClient` exchange (or its body-publisher wait)
/// runs for, so its blocking regions read the state the HotSpot thread has
/// there (interpreter round i3 wave 83, lane L6; the re5 twin of wave 82's
/// `http_client.rs` `do_send(.., sync)` split).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Re5Caller {
    /// `HttpClient.send`: JDK 25's `HttpClientImpl.send` is
    /// `sendAsync(..).get()`, so the HotSpot caller waits in
    /// `CompletableFuture.get` for the whole exchange: WAITING (parking).
    Send,
    /// `sendAsync`'s inline fallback (no task thread could be started): the
    /// JDK body returns at once, so the HotSpot caller is RUNNABLE in its own
    /// code -- a VM wait, not in native.
    SendAsyncInline,
    /// The task thread `sendAsync` starts (`re5_send_task_run`): it performs
    /// the socket exchange itself (RUNNABLE in native, as the JDK's
    /// `SelectorManager` reading the socket), and waits for a body publisher
    /// driven by another thread inside the VM (RUNNABLE).
    AsyncTask,
    /// `BodyPublisher.contentLength()`: the JDK answers the declared length
    /// at once without subscribing, so a publisher wait here is one HotSpot
    /// never makes -- a VM wait (RUNNABLE).
    ContentLength,
}

/// Open the blocking region of a re5 body-publisher wait for `caller`.
/// Paired with exactly one `end_blocking_region`.
pub fn re5_begin_publisher_wait_region(ctx: &mut dyn NativeContext, caller: Re5Caller) {
    match caller {
        Re5Caller::Send => {
            ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::Parked);
        }
        Re5Caller::SendAsyncInline | Re5Caller::AsyncTask | Re5Caller::ContentLength => {
            ctx.begin_vm_wait_region();
        }
    }
}

/// Open the blocking region of a re5 plain-HTTP exchange for `caller`.
/// Paired with exactly one `end_blocking_region`.
pub fn re5_begin_exchange_region(ctx: &mut dyn NativeContext, caller: Re5Caller) {
    match caller {
        Re5Caller::Send => {
            ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::Parked);
        }
        Re5Caller::AsyncTask => {
            ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
        }
        Re5Caller::SendAsyncInline | Re5Caller::ContentLength => ctx.begin_vm_wait_region(),
    }
}

pub fn re5_collect_publisher_body(
    ctx: &mut dyn NativeContext,
    publisher: ObjectRef,
    caller: Re5Caller,
) -> Result<Vec<u8>, MethodCallFailed> {
    // gc-common w15-a: `publisher` is pinned BEFORE the subscriber is
    // allocated (it used to be pinned after, i.e. from a possibly vacated
    // address), and the collector row is filed only once that allocation has
    // succeeded (a failed allocation used to leave the row in the table for
    // the life of the process).
    let pin = ctx.pin_native_root(publisher);
    let subscriber = match try_alloc_concurrent_synthetic(ctx, RE5_BODY_COLLECTOR_SUBSCRIBER, 1) {
        Ok(subscriber) => subscriber,
        Err(e) => {
            ctx.unpin_native_roots(pin);
            return Err(e);
        }
    };
    let id = re5_next_body_collector_id();
    let collector = Arc::new(Re5PublisherBodyCollector::default());
    re5_body_collectors().lock().insert(id, collector.clone());
    ctx.set_field(subscriber, 0, Value::Long(id as i64));
    let sub_global = ctx.add_global_root(subscriber);
    let sub_pin = ctx.pin_native_root(subscriber);
    let subscribe_result = ctx.invoke_virtual(
        ctx.read_native_pin(pin, publisher),
        "subscribe",
        "(Ljava/util/concurrent/Flow$Subscriber;)V",
        &[Value::Object(Some(
            ctx.read_native_pin(sub_pin, subscriber),
        ))],
    );
    ctx.unpin_native_roots(pin);

    if let Err(e) = subscribe_result {
        re5_body_collectors().lock().remove(&id);
        if sub_global != 0 {
            ctx.remove_global_root(sub_global);
        }
        // An `onError` delivered before `subscribe` threw left its root here.
        let late_root = {
            let mut state = collector.state.lock().unwrap_or_else(|e| e.into_inner());
            state.abandoned = true;
            state.error_obj_root.take()
        };
        if let Some(handle) = late_root {
            ctx.remove_global_root(handle);
        }
        return Err(e);
    }

    // STW cross-thread JIT-takeover deadlock fix, same family as the
    // http_perform_request fix in re5_do_request (see that comment for the
    // full mechanism): re5_collect_publisher_body's condvar wait blocks this
    // thread for up to RE5_PUBLISHER_WAIT waiting for a notify delivered by
    // a DIFFERENT Java thread (the Reactor scheduler thread driving the
    // Publisher, calling back into re5_body_collector_on_next/on_complete).
    // That signalling thread cooperates normally with an STW pause (it's
    // ordinary bytecode, hits interpreter safepoints); this thread, stuck in
    // a raw Rust condvar wait, does not -- so a concurrent STW request
    // starves waiting on THIS thread while the notify THIS thread needs
    // waits on the OTHER thread's own cooperation with that same pause.
    let deadline = Instant::now() + RE5_PUBLISHER_WAIT;
    // Wave 83 (lane L6): the region says what the caller's HotSpot thread
    // is doing meanwhile (`Re5Caller`).
    re5_begin_publisher_wait_region(ctx, caller);
    let mut state = collector.state.lock().unwrap_or_else(|e| e.into_inner());
    while !state.completed && state.error.is_none() {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let wait_for = deadline.saturating_duration_since(now);
        let (next_state, wait) = collector
            .done
            .wait_timeout(state, wait_for)
            .unwrap_or_else(|e| e.into_inner());
        state = next_state;
        if wait.timed_out() {
            break;
        }
    }
    ctx.end_blocking_region();

    let timed_out = !state.completed && state.error.is_none();
    let error = state.error.clone();
    let error_obj_root = state.error_obj_root.take();
    // See `Re5PublisherBodyState::abandoned`: a late `onError` releases its
    // own root from here on.
    state.abandoned = true;
    let out = std::mem::take(&mut state.bytes);
    drop(state);
    re5_body_collectors().lock().remove(&id);
    if sub_global != 0 {
        ctx.remove_global_root(sub_global);
    }

    if re5_dbg() {
        eprintln!(
            "[RE5-DBG] re5_collect_publisher_body id={id} timed_out={timed_out} bytes={} error={:?}",
            out.len(), error
        );
    }
    if timed_out {
        if let Some(handle) = error_obj_root {
            ctx.remove_global_root(handle);
        }
        return Err(ioex("HttpRequest body publisher did not complete"));
    }
    if error.is_some() {
        // Prefer rethrowing the ORIGINAL Throwable (identity-preserving,
        // matching real JDK's observed behaviour) over synthesizing a new
        // IOException from just its text. Resolution can fail if the root
        // somehow never got set; fall back to the old text-only wrapping
        // rather than silently swallowing the failure.
        if let Some(handle) = error_obj_root {
            let resolved = ctx.resolve_global_root(handle);
            ctx.remove_global_root(handle);
            if let Some(orig) = resolved {
                return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                    orig,
                ));
            }
        }
        let msg = error.unwrap();
        return Err(ioex(format!("HttpRequest body publisher failed: {msg}")));
    }
    Ok(out)
}

pub fn re5_request_body_bytes(
    ctx: &mut dyn NativeContext,
    value: Value,
    caller: Re5Caller,
) -> Result<Vec<u8>, MethodCallFailed> {
    let obj = match value {
        Value::Object(Some(o)) => o,
        _ => return Ok(Vec::new()),
    };
    if re5_is_byte_array(ctx, obj) {
        return Ok(re5_read_byte_array(ctx, obj));
    }

    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(obj))
        .unwrap_or_default();
    if class_name == "java/lang/String" {
        return Ok(ctx.read_string(obj).unwrap_or_default().into_bytes());
    }
    if class_name.contains("ByteBuffer") || class_name == "java/nio/ByteBuffer" {
        return re5_read_and_consume_bytebuffer(ctx, obj);
    }
    re5_collect_publisher_body(ctx, obj, caller)
}

// ---------------------------------------------------------------------------
// The synthetic `java/net/http/HttpRequest` / `HttpRequest$Builder` slot map.
//
// G29-1 (Mechanism A, instance 4). `HttpRequest$Builder.build()` mints an
// object stamped with the ABSTRACT class `java/net/http/HttpRequest`, so the
// receiver's runtime class IS the class every accessor resolves against.
// `java.net.http.HttpRequest` declares SEVEN abstract instance methods
// (`method`, `uri`, `timeout`, `version`, `bodyPublisher`, `expectContinue`,
// `headers`); until 2026-08-17 only three had a native, and the other four
// threw `AbstractMethodError: ... has no Code attribute` — the correct JVMS
// answer to a question that should never have been asked, because the mistake
// is at the mint, one call earlier. `HttpClient` in this same file is the
// control: it registers 7 of 7 and all seven are measured correct.
//
// Both the builder and the request it builds use THIS layout, and `build()`
// copies slot-for-slot, so the two must not drift. Slots 0..=4 predate G29-1
// and are named here for the first time; 5..=7 are new.
pub const RE5_REQUEST_METHOD: usize = 0; // String, never null (defaults to "GET")
pub const RE5_REQUEST_URI: usize = 1; // String (the URI's text), null until set
pub const RE5_REQUEST_BODY: usize = 2; // String | byte[] | Flow.Publisher: wire body
pub const RE5_REQUEST_HEADERS: usize = 3; // String[] of "key: value", null until set
pub const RE5_REQUEST_TIMEOUT_FIELD: usize = 4; // java.time.Duration, null = unset
pub const RE5_REQUEST_EXPECT_CONTINUE: usize = 5; // Int 0/1 — a (Z) parameter really
                                              // does arrive as an int here
pub const RE5_REQUEST_VERSION: usize = 6; // HttpClient$Version, null = unset
pub const RE5_REQUEST_BODY_PUBLISHER: usize = 7; // the BodyPublisher OBJECT itself
pub const RE5_REQUEST_NUM_FIELDS: usize = 8;

// The synthetic `java/net/http/HttpRequest$BodyPublisher`. Slot 0 is the
// payload every existing reader already expects (`re5_request_body_bytes`
// dispatches on its runtime type); slot 1 is the content length DECLARED by
// `fromPublisher(pub, len)`, which cannot be derived from slot 0 because a
// Flow.Publisher has no length until it is subscribed.
pub const RE5_BP_PAYLOAD: usize = 0;
pub const RE5_BP_DECLARED_LENGTH: usize = 1; // Long, or Object(None) = derive
pub const RE5_BP_NUM_FIELDS: usize = 2;

/// Allocate a fresh `HttpRequest$Builder` with every slot of
/// [`RE5_REQUEST_NUM_FIELDS`] explicitly initialised.
///
/// Every slot is written, including the ones whose "unset" value is the
/// allocator's zero: `build()` copies the whole range slot-for-slot, and a
/// builder that only initialises a prefix leaves the tail reading whatever the
/// allocator happened to leave there. That is how `expectContinue()` would
/// come to answer a value nobody set.
pub fn re5_new_request_builder(
    ctx: &mut dyn NativeContext,
    uri: Option<ObjectRef>,
) -> MethodCallResult {
    // `re5_uri_string`, `try_alloc_concurrent_synthetic` and `create_string`
    // ALL allocate, and a moving collector can run inside any of them — so the
    // URI text is produced first and pinned, the builder is pinned as soon as
    // it exists, and both are read back through their pins before use. The
    // batch started FIRST is the one unpinned at the end: releasing it releases
    // everything pinned after it.
    let uri_text = uri.map(|u| re5_uri_string(ctx, u));
    let text_pin = uri_text.map(|t| (ctx.pin_native_root(t), t));
    let b = try_alloc_concurrent_synthetic(
        ctx,
        "java/net/http/HttpRequest$Builder",
        RE5_REQUEST_NUM_FIELDS,
    )?;
    let b_pin = ctx.pin_native_root(b);
    let m = ctx.create_string("GET");
    let b_now = ctx.read_native_pin(b_pin, b);
    ctx.set_field(b_now, RE5_REQUEST_METHOD, Value::Object(Some(m)));
    let text_now = text_pin.map(|(pin, t)| ctx.read_native_pin(pin, t));
    ctx.set_field(b_now, RE5_REQUEST_URI, Value::Object(text_now));
    ctx.set_field(b_now, RE5_REQUEST_BODY, Value::Object(None));
    ctx.set_field(b_now, RE5_REQUEST_HEADERS, Value::Object(None));
    ctx.set_field(b_now, RE5_REQUEST_TIMEOUT_FIELD, Value::Object(None));
    ctx.set_field(b_now, RE5_REQUEST_EXPECT_CONTINUE, Value::Int(0));
    ctx.set_field(b_now, RE5_REQUEST_VERSION, Value::Object(None));
    ctx.set_field(b_now, RE5_REQUEST_BODY_PUBLISHER, Value::Object(None));
    match text_pin {
        Some((pin, _)) => ctx.unpin_native_roots(pin),
        None => ctx.unpin_native_roots(b_pin),
    }
    Ok(Some(Value::Object(Some(b_now))))
}

pub fn ise<S: Into<String>>(message: S) -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: message.into(),
    }
    .into()
}

/// A `NullPointerException` with NO message, which is what
/// `Objects.requireNonNull(x)` produces. MEASURED: HotSpot's `POST(null)`,
/// `method(null, …)`, `method("POST", null)`, `timeout(null)` and
/// `version(null)` all report `getMessage() == null`, where `header(null, …)`
/// reports the string `"name"`. The difference is observable from Java, so it
/// is modelled rather than approximated.
pub fn npe_no_message() -> cratonvm_types::error::MethodCallFailed {
    RuntimeError::NullPointerException { message: None }.into()
}

/// The shared body of `HttpRequest$Builder.POST` and `.PUT`.
pub fn re5_builder_verb_with_body(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    verb: &str,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // MEASURED: `POST(null)` is `NullPointerException` with a null message on
    // HotSpot. Do not silently build a body-less POST.
    let publisher = match args.get(1).copied() {
        Some(v @ Value::Object(Some(_))) => v,
        _ => return Err(npe_no_message()),
    };
    // The publisher is stored first: that write does not allocate, so both
    // `args` values are still current for it. Then the method string.
    re5_builder_set_publisher(ctx, this, publisher);
    let this = re5_builder_set_method(ctx, this, verb);
    Ok(Some(Value::Object(Some(this))))
}

/// Store `verb` as builder `this`'s method and answer `this` as it is NOW.
///
/// gc-common w14-a: `create_string` can collect, so `this` is pinned across
/// it. `GET()`/`DELETE()`/`HEAD()`/`POST()`/`PUT()` used to store the new
/// String into the `args` copy of `this` right after allocating it, and a
/// moving collection there wrote the method into a vacated builder and
/// handed that address back as the call's result.
pub fn re5_builder_set_method(ctx: &mut dyn NativeContext, this: ObjectRef, verb: &str) -> ObjectRef {
    let pin = ctx.pin_native_root(this);
    let m = ctx.create_string(verb);
    let this = ctx.read_native_pin(pin, this);
    ctx.unpin_native_roots(pin);
    ctx.set_field(this, RE5_REQUEST_METHOD, Value::Object(Some(m)));
    this
}

/// Record a `BodyPublisher` on a builder: both the wire payload (slot 2, which
/// `re5_do_request` has always read) and the publisher OBJECT (slot 7, which
/// `HttpRequest.bodyPublisher()` hands back). Two slots because they are two
/// different questions — "what goes on the socket" and "what did the caller
/// give me" — and the second one had no answer at all before G29-1.
pub fn re5_builder_set_publisher(ctx: &mut dyn NativeContext, this: ObjectRef, publisher: Value) {
    match publisher {
        Value::Object(Some(bp)) => {
            // Slot 0 is the payload only on the VM's own publisher (minted by
            // `re5_new_body_publisher` with the interface's name). Any other
            // publisher is a JDK one -- `BodyPublishers.ofInputStream`,
            // `ofFile`, `concat`, ... are not registered here, so their real
            // bytecode built them -- and IS the payload: a `Flow.Publisher`
            // that `re5_collect_publisher_body` subscribes to. Reading its
            // slot 0 handed `ofInputStream`'s `Supplier` to `subscribe`
            // (`NoSuchMethodError: Supplier.subscribe`, `NioConf19b` row
            // `httpstream` under `--compatible`, JIT round 19).
            let own = ctx
                .class_name_of_id(ctx.class_id_of_object(bp))
                .is_some_and(|n| n == "java/net/http/HttpRequest$BodyPublisher");
            let body = if own {
                ctx.get_field(bp, RE5_BP_PAYLOAD)
            } else {
                Value::Object(Some(bp))
            };
            ctx.set_field(this, RE5_REQUEST_BODY, body);
            ctx.set_field(this, RE5_REQUEST_BODY_PUBLISHER, Value::Object(Some(bp)));
        }
        _ => {
            ctx.set_field(this, RE5_REQUEST_BODY, Value::Object(None));
            ctx.set_field(this, RE5_REQUEST_BODY_PUBLISHER, Value::Object(None));
        }
    }
}

/// Mint a `BodyPublisher` over `payload`, with an optionally DECLARED content
/// length. `None` means "derive the length from the payload at call time",
/// which is what `ofString`/`ofByteArray`/`noBody` want.
pub fn re5_new_body_publisher(
    ctx: &mut dyn NativeContext,
    payload: Value,
    declared_length: Option<i64>,
) -> MethodCallResult {
    // The payload is pinned across the allocation so a moving collector cannot
    // leave the stored reference stale — the same treatment
    // `re5_make_http_headers` gives its header array.
    let payload_pin = match payload {
        Value::Object(Some(o)) => Some((ctx.pin_native_root(o), o)),
        _ => None,
    };
    let body = try_alloc_concurrent_synthetic(
        ctx,
        "java/net/http/HttpRequest$BodyPublisher",
        RE5_BP_NUM_FIELDS,
    )?;
    let payload_now = match payload_pin {
        Some((pin, o)) => Value::Object(Some(ctx.read_native_pin(pin, o))),
        None => payload,
    };
    ctx.set_field(body, RE5_BP_PAYLOAD, payload_now);
    ctx.set_field(
        body,
        RE5_BP_DECLARED_LENGTH,
        match declared_length {
            Some(n) => Value::Long(n),
            None => Value::Object(None),
        },
    );
    if let Some((pin, _)) = payload_pin {
        ctx.unpin_native_roots(pin);
    }
    Ok(Some(Value::Object(Some(body))))
}


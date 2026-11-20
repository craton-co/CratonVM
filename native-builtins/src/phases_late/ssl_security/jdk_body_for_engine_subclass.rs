#[allow(unused_imports)]
use super::*;

/// `SSLEngine.wrap/unwrap(ByteBuffer, ByteBuffer)` are concrete JDK one-line delegations to the
/// abstract four-argument form. The carrier natives below serve only the VM's own engines, which
/// are allocated on `SSLEngine` itself and have no override to delegate to; a real subclass
/// (Tomcat's `OpenSSLEngine` overrides only the four-argument form) gets the JDK body.
pub fn jdk_body_for_engine_subclass(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
) -> Option<MethodCallResult> {
    let Some(Value::Object(Some(this))) = args.first() else {
        return None;
    };
    let base = ctx.class_id_by_name("javax/net/ssl/SSLEngine")?;
    if ctx.class_id_of_object(*this) == base || !ctx.class_has_real_bytes(base) {
        return None;
    }
    Some(ctx.invoke_special_bytecode_only(
        "javax/net/ssl/SSLEngine",
        method,
        "(Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;",
        args,
    ))
}

/// Allocate a fresh SSLEngine with default field values.
pub fn ssleng_alloc(ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLEngine", 7)?;
    ctx.set_field(obj, 0, Value::Int(1)); // client_mode = true by default
    ctx.set_field(obj, 1, Value::Int(0)); // need_client_auth
    ctx.set_field(obj, 2, Value::Int(0)); // want_client_auth
    ctx.set_field(obj, 3, Value::Object(None)); // enabled_protocols
    ctx.set_field(obj, 4, Value::Object(None)); // enabled_cipher_suites
    ctx.set_field(obj, 5, Value::Int(0)); // handshake_status = NOT_HANDSHAKING
    ctx.set_field(obj, 6, Value::Object(None)); // session
    Ok(obj)
}

// =============================================================================
// Basic DER/ASN.1 TLV parser for X.509 subject/issuer extraction (non-feature-gated)
// =============================================================================

/// Read DER tag-length-value. Returns (tag, content_slice, rest_slice).
pub fn der_read_tlv(data: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    if data.is_empty() {
        return None;
    }
    let tag = data[0];
    if data.len() < 2 {
        return None;
    }
    let (length, header_len) = if data[1] & 0x80 == 0 {
        (data[1] as usize, 2)
    } else {
        let num_bytes = (data[1] & 0x7f) as usize;
        if num_bytes == 0 || num_bytes > 4 || data.len() < 2 + num_bytes {
            return None;
        }
        let mut len = 0usize;
        for i in 0..num_bytes {
            len = (len << 8) | data[2 + i] as usize;
        }
        (len, 2 + num_bytes)
    };
    if data.len() < header_len + length {
        return None;
    }
    Some((
        tag,
        &data[header_len..header_len + length],
        &data[header_len + length..],
    ))
}

/// Extract CommonName (OID 2.5.4.3) from a DER-encoded Name (SEQUENCE of RDNs).
pub fn der_extract_cn(name_data: &[u8]) -> String {
    // CN OID: 55 04 03
    let cn_oid: &[u8] = &[0x55, 0x04, 0x03];
    let mut parts = Vec::new();
    let mut pos = name_data;
    // Walk through SET OF RDNSequence
    while let Some((_tag, set_content, rest)) = der_read_tlv(pos) {
        // Each SET contains SEQUENCE of AttributeTypeAndValue
        let mut inner = set_content;
        while let Some((_tag2, seq_content, inner_rest)) = der_read_tlv(inner) {
            // SEQUENCE = OID + value
            if let Some((0x06, oid_bytes, val_rest)) = der_read_tlv(seq_content) {
                if let Some((val_tag, val_bytes, _)) = der_read_tlv(val_rest) {
                    let oid_name = match oid_bytes {
                        x if x == cn_oid => "CN",
                        x if x == &[0x55, 0x04, 0x06] => "C",
                        x if x == &[0x55, 0x04, 0x08] => "ST",
                        x if x == &[0x55, 0x04, 0x07] => "L",
                        x if x == &[0x55, 0x04, 0x0a] => "O",
                        x if x == &[0x55, 0x04, 0x0b] => "OU",
                        _ => "",
                    };
                    if !oid_name.is_empty()
                        && (val_tag == 0x0c || val_tag == 0x13 || val_tag == 0x16)
                    {
                        if let Ok(s) = std::str::from_utf8(val_bytes) {
                            parts.push(format!("{}={}", oid_name, s));
                        }
                    }
                }
            }
            inner = inner_rest;
        }
        pos = rest;
    }
    if parts.is_empty() {
        "CN=Unknown".to_string()
    } else {
        parts.join(", ")
    }
}

/// Extract subject and issuer DN strings from a DER-encoded X.509 certificate.
pub fn basic_der_extract_names(data: &[u8]) -> Option<(String, String)> {
    // Certificate = SEQUENCE { tbsCertificate, signatureAlgorithm, signatureValue }
    let (tag, cert_content, _) = der_read_tlv(data)?;
    if tag != 0x30 {
        return None;
    } // must be SEQUENCE

    // tbsCertificate = SEQUENCE { version, serialNumber, signature, issuer, validity, subject, ... }
    let (tag, tbs_content, _) = der_read_tlv(cert_content)?;
    if tag != 0x30 {
        return None;
    }

    let mut pos = tbs_content;

    // version: [0] EXPLICIT INTEGER (optional — if tag is 0xa0, skip it)
    if let Some((0xa0, _, rest)) = der_read_tlv(pos) {
        pos = rest;
    }

    // serialNumber: INTEGER
    let (0x02, _, rest) = der_read_tlv(pos)? else {
        return None;
    };
    pos = rest;

    // signature: AlgorithmIdentifier (SEQUENCE)
    let (0x30, _, rest) = der_read_tlv(pos)? else {
        return None;
    };
    pos = rest;

    // issuer: Name (SEQUENCE)
    let (0x30, issuer_content, rest) = der_read_tlv(pos)? else {
        return None;
    };
    let issuer = der_extract_cn(issuer_content);
    pos = rest;

    // validity: SEQUENCE { notBefore, notAfter }
    let (0x30, _, rest) = der_read_tlv(pos)? else {
        return None;
    };
    pos = rest;

    // subject: Name (SEQUENCE)
    let (0x30, subject_content, _) = der_read_tlv(pos)? else {
        return None;
    };
    let subject = der_extract_cn(subject_content);

    Some((subject, issuer))
}

/// The raw DER (tag, length and content) of a certificate's subject Name.
pub fn der_subject_name_raw(data: &[u8]) -> Option<&[u8]> {
    let (0x30, cert_content, _) = der_read_tlv(data)? else {
        return None;
    };
    let (0x30, tbs_content, _) = der_read_tlv(cert_content)? else {
        return None;
    };
    let mut pos = tbs_content;
    if let Some((0xa0, _, rest)) = der_read_tlv(pos) {
        pos = rest;
    }
    // serialNumber, signature, issuer, validity
    for want in [0x02u8, 0x30, 0x30, 0x30] {
        let (tag, _, rest) = der_read_tlv(pos)?;
        if tag != want {
            return None;
        }
        pos = rest;
    }
    let (0x30, _, rest) = der_read_tlv(pos)? else {
        return None;
    };
    Some(&pos[..pos.len() - rest.len()])}

/// SBR wave 4 (lane SSL): `true` when this VM runs `--real-jdk` AND
/// `X500Principal`'s shadows are retired (`RETIRED_SHADOW_SBR_SSL_TRIPLES`).
/// Then `getName`/`equals`/`hashCode` are the JDK's bytecode, which reads the
/// real `thisX500Name` field -- a 1-slot synthetic holding a String answers
/// them with a `NullPointerException`. Keyed on the rows, so
/// `CRATONVM_UNRETIRE_NATIVE_SHADOW` turns it off with them.
pub fn real_x500_principal(ctx: &dyn NativeContext) -> bool {
    static RETIRED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    ctx.aligned(cratonvm_types::HotSpotAlignment::JdkJsse)
        && *RETIRED.get_or_init(|| {
            cratonvm_native_api::retired_shadow::triple_is_retired_shadow(
                "javax/security/auth/x500/X500Principal",
                "getName",
                "()Ljava/lang/String;",
            )
        })
}

/// A REAL `X500Principal` built by its own `X500Principal(byte[])`
/// constructor from a DER-encoded Name, or `None` if the constructor refuses.
pub fn new_real_x500_principal_from_der(
    ctx: &mut dyn NativeContext,
    name_der: &[u8],
) -> Option<ObjectRef> {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, name_der.len());
    if !ctx.write_byte_array_from(arr, 0, name_der) {
        for (i, b) in name_der.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
        }
    }
    match ctx.new_object_initialized(
        "javax/security/auth/x500/X500Principal",
        "([B)V",
        &[Value::Object(Some(arr))],
    ) {
        Ok(Some(Value::Object(Some(p)))) => Some(p),
        _ => None,
    }
}

// =============================================================================
// java.security.cert — X509Certificate, CertificateFactory, CertPath
// =============================================================================

/// Recover the DER encoding backing a synthetic `java/security/cert/
/// X509Certificate` mirror.
///
/// Two shapes exist and both are in active use:
///   * 4-field mirror (`keystore::make_x509_mirror`'s fallback,
///     `t27_tls::register_accepted_issuers`, `phases_early`'s getCertificate
///     path) — slot 3 holds the raw DER as a `byte[]`;
///   * 3-field mirror plus a `crypto_impl` cert-store id in slot 2, which only
///     resolves under the `legacy-synthetic-crypto` feature.
///
/// Returns `None` when neither is present, so callers can raise the exception
/// their contract requires rather than inventing an empty/valid-looking answer.
pub fn x509_mirror_der(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<Vec<u8>> {
    if ctx.object_num_fields(this) > 3 {
        if let Value::Object(Some(der_arr)) = ctx.get_field(this, 3) {
            let len = ctx.array_length(der_arr);
            if len > 0 {
                let mut out = Vec::with_capacity(len);
                for i in 0..len {
                    if let Value::Int(b) = ctx.get_array_element(der_arr, i) {
                        out.push(b as u8);
                    }
                }
                if !out.is_empty() {
                    return Some(out);
                }
            }
        }
    }
    #[cfg(feature = "legacy-synthetic-crypto")]
    {
        let cert_id = match ctx.get_field(this, 2) {
            Value::Long(id) => id as u64,
            Value::Int(id) if id > 0 => id as u64,
            _ => 0,
        };
        if cert_id != 0 {
            if let Some(parsed) = crypto_impl::cert_get(cert_id) {
                if !parsed.encoded.is_empty() {
                    return Some(parsed.encoded.clone());
                }
            }
        }
    }
    None
}

/// Shared body for both `X509Certificate.checkValidity` overloads.
///
/// STUB-REMOVAL (wave 2): in the default build both overloads were
/// unconditional no-ops — a void "check" method that never throws is a
/// hard-coded PASS, so an expired or not-yet-valid certificate was certified as
/// currently valid. Parse the mirror's own DER and enforce RFC 5280 §4.1.2.5
/// for real, throwing the exact subclasses the JDK specifies
/// (`CertificateExpiredException` / `CertificateNotYetValidException`, both
/// `CertificateException`s) so a caller's existing catch blocks match.
///
/// `at_secs` is the instant to evaluate against, in seconds since the epoch.
/// A certificate whose DER cannot be recovered or parsed CANNOT be attested to,
/// so it throws `CertificateException` rather than passing silently.
pub fn x509_check_validity_at(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    at_secs: i64,
) -> MethodCallResult {
    let Some(der) = x509_mirror_der(ctx, this) else {
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/security/cert/CertificateException",
            "certificate validity cannot be checked: no encoded form available",
        ));
    };
    let parsed = match crate::x509_manager::parse_certificate(&der) {
        Ok(p) => p,
        Err(_) => {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/security/cert/CertificateException",
                "certificate validity cannot be checked: malformed certificate",
            ));
        }
    };
    if at_secs < parsed.not_before_secs {
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/security/cert/CertificateNotYetValidException",
            "NotBefore is in the future",
        ));
    }
    if at_secs > parsed.not_after_secs {
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/security/cert/CertificateExpiredException",
            "NotAfter has already passed",
        ));
    }
    Ok(None)
}

/// Read the `type` argument of `CertificateFactory.getInstance`, or `None` when
/// the caller passed `null`.
///
/// Fixed index 0 rather than `mac_algorithm_arg`'s relative scan: both overloads
/// here already address their arguments positionally (the two-argument one reads
/// the provider at `args[1]`, and `provider_chain::try_build_real_certificate_factory`
/// reads the type at `args[0]`), and that indexing is what resolves `X.509`
/// today — measured, not assumed. Changing it would be a second variable.
pub fn cf_type_arg(ctx: &mut dyn NativeContext, args: &[Value]) -> Option<String> {
    match args.first() {
        Some(Value::Object(Some(s))) => ctx.read_string(*s),
        _ => None,
    }
}

/// Can `CertificateFactory.getInstance(type)` be serviced at all?
///
/// ## W7-29 — the advertised-vs-implemented census, in its dangerous direction
///
/// This registration used to accept **any** type string: the real-SPI path was
/// tried, and on failure a bare one-field synthetic `CertificateFactory` was
/// returned unconditionally. That is the same species as the `Cipher` defect the
/// census named worst — an engine that validates nothing answers every name with
/// whatever its default arm does — and here the default arm is an X.509 parser.
///
/// Measured on the pre-built `target/release/cratonvm.exe` in BOTH default and
/// `--real-jdk` mode, against `jdk-25.0.3.9-hotspot` running the same class:
///
/// ```text
/// advertised = [X.509]                                        (both VMs agree)
///
///                              CratonVM                     HotSpot 25
/// getInstance("PKCS7")         a factory, getType()==null    CertificateException: PKCS7 not found
/// getInstance("AES")           a factory, getType()==null    CertificateException: AES not found
/// getInstance("")              a factory                     CertificateException:  not found
/// getInstance(null)            a factory                     NullPointerException: null type name
///
/// getInstance("PKCS7").generateCertificate(<X.509 DER>)
///                              sun.security.x509.X509CertImpl, CN=jcagap, 714 bytes
///                                                            (never reached — the throw is above)
/// ```
///
/// So a caller asking for PKCS#7 got an X.509 certificate back and no error
/// anywhere on the path. `Security.getAlgorithms("CertificateFactory")` answered
/// `[X.509]` on both VMs the whole time — advertised and implemented had drifted
/// apart in the direction nothing tests, because no census enumerates the names
/// an engine will accept but never advertised.
///
/// The predicate is the live provider registry, not a literal list, for the same
/// reason `KeyManagerFactory`/`TrustManagerFactory.getInstance` above use it:
/// `Security.getAlgorithms` is answered from that same registry
/// (`provider_chain::algorithms_for_service`), so advertised and serviceable
/// cannot drift by construction, and a caller-registered custom `Provider` that
/// really does implement PKCS#7 is still honoured. It is alias- and case-aware —
/// `X509`, `x.509` and `x509` all resolve, matching HotSpot, which echoes the
/// caller's own spelling back from `getType()`.
pub fn certificate_factory_type_supported(vm: usize, type_name: &str) -> bool {
    crate::jca::provider_chain::find_service_provider_in(vm, "CertificateFactory", type_name)
        .is_some()
}

/// Is this a type the one-field synthetic fallback can honestly serve?
///
/// The fallback has exactly one behaviour: `generateCertificate` parses X.509
/// DER (`x509_manager::parse_certificate`). It is reached when the real SPI
/// could not be constructed — pure-synthetic mode, or a registered provider
/// whose implementation class will not load — and returning it for a type that
/// is not X.509 would hand back an X.509 parser under another name, which is the
/// defect `certificate_factory_type_supported` exists to close, one layer down.
///
/// This is deliberately looser than the JDK's alias table (it also folds `X-509`)
/// and that is safe only because it runs strictly AFTER the registry gate: a
/// spelling the registry does not carry has already been refused, so the
/// looseness can widen nothing. It can only decide, among types the registry
/// accepted, which ones the fallback may stand in for.
pub fn certificate_factory_stub_serves(type_name: &str) -> bool {
    type_name.to_ascii_uppercase().replace(['.', '-'], "") == "X509"
}

/// `java.security.cert.CertificateException: <type> not found` — HotSpot's own
/// wording for a `CertificateFactory` type no provider services.
///
/// Measured on jdk-25.0.3.9-hotspot, not recalled: `getInstance("PKCS7")` is
/// `java.security.cert.CertificateException: PKCS7 not found`, and the
/// two-argument overload with a valid provider and a bogus type answers the
/// same message (the provider is resolved first, so a bogus provider is
/// `NoSuchProviderException` instead — that ordering is already implemented by
/// the two-argument registration below).
///
/// `CertificateException` and NOT `NoSuchAlgorithmException`: it is what
/// `CertificateFactory.getInstance(String)` declares — *"@throws
/// CertificateException if no `Provider` supports a `CertificateFactorySpi`
/// implementation for the specified type"* — so it is the checked exception a
/// caller's `catch` clause is written against. Throwing the unchecked
/// `SecurityException` instead would sail straight past that handler, the
/// mistake `jca::message_digest::md_get_instance` and `mac_no_such_algorithm`
/// both record having made and corrected.
pub fn cert_type_not_found(ctx: &mut dyn NativeContext, type_name: &str) -> MethodCallFailed {
    crate::phases_early::throw_jca_exc(
        ctx,
        "java/security/cert/CertificateException",
        &format!("{type_name} not found"),
    )
}

/// `X509Certificate.getSubjectDN()` / `getIssuerDN()` over the synthetic
/// 3-field layout: the String in `slot`, wrapped in a 1-field
/// `X500Principal`-shaped synthetic, or the raw slot when it holds no
/// reference (gc-common w15-d: the String is rooted across the allocation).
pub fn x509_dn_from_slot(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    slot: usize,
) -> MethodCallResult {
    if real_x500_principal(&*ctx) {
        // SBR wave 4: a real principal, as `getSubjectX500Principal` builds.
        return x509_principal_from_slot(ctx, this, slot);
    }
    let name = ctx.get_field(this, slot);
    if matches!(name, Value::Object(Some(_))) {
        // Wrap string in a Principal-like synthetic
        let princ = p68_alloc_with_slot(ctx, "javax/security/auth/x500/X500Principal", 1, 0, name)?;
        return Ok(Some(Value::Object(Some(princ))));
    }
    Ok(Some(name))
}

/// `X509Certificate.getSubjectX500Principal()` / `getIssuerX500Principal()`
/// over the synthetic layout: a REAL `X500Principal(String)` built from the
/// String in `slot`, else the 1-field synthetic holding it.
///
/// gc-common w15-d: the String is rooted before the constructor runs. A
/// constructor that throws has still run Java, and the fallback used to store
/// the String's pre-constructor address.
pub fn x509_principal_from_slot(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    slot: usize,
) -> MethodCallResult {
    let name = ctx.get_field(this, slot);
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let name_h = match name {
        Value::Object(Some(s)) => Some(scope.root(s)),
        _ => None,
    };
    if name_h.is_some() {
        if let Ok(Some(Value::Object(Some(p)))) = scope.new_object_initialized(
            "javax/security/auth/x500/X500Principal",
            "(Ljava/lang/String;)V",
            &[name],
        ) {
            return Ok(Some(Value::Object(Some(p))));
        }
    }
    let name_now = match name_h.as_ref() {
        Some(h) => Value::Object(Some(scope.get(h))),
        None => name,
    };
    let princ = p68_alloc_with_slot(
        &mut *scope,
        "javax/security/auth/x500/X500Principal",
        1,
        0,
        name_now,
    )?;
    Ok(Some(Value::Object(Some(princ))))
}

pub fn register_p68_security_cert(r: &mut NativeMethodRegistry) {
            // CertificateFactory
    let cf = "java/security/cert/CertificateFactory";
    r.register(
        cf,
        "getInstance",
        "(Ljava/lang/String;)Ljava/security/cert/CertificateFactory;",
        |ctx, args| {
            // W7-29: resolve and validate the type BEFORE either construction
            // path runs. Neither path validated anything, so every name this VM
            // does not service was answered with an X.509 parser wearing the
            // caller's label — see `certificate_factory_type_supported` for the
            // measurement.
            let Some(type_name) = cf_type_arg(ctx, args) else {
                // HotSpot: `CertificateFactory.getInstance(null)` is
                // `NullPointerException: null type name` — measured. It used to
                // return a working-looking factory here.
                return Err(RuntimeError::NullPointerException {
                    message: Some("null type name".to_string()),
                }
                .into());
            };
            if !certificate_factory_type_supported(ctx.vm_identity(), &type_name) {
                return Err(cert_type_not_found(ctx, &type_name));
            }
            // Real-JCA bring-up: prefer a genuine `CertificateFactory` wrapping
            // a real provider SPI over the synthetic 1-field stub below — see
            // `provider_chain::try_build_real_certificate_factory`'s doc
            // comment for the root-cause story (real-bytecode-only methods
            // like `generateCertPath` NPE on the synthetic stub's absent
            // `certFacSpi`). Falls back to the stub when the algorithm can't
            // be resolved (e.g. pure-synthetic mode, or a registered provider
            // whose implementation class will not load).
            if crate::real_jca_mode() || crate::route_ec_to_real() || crate::route_dsa_to_real() {
                if let Ok(Some(real_cf)) =
                    crate::jca::provider_chain::try_build_real_certificate_factory(ctx, args)
                {
                    return Ok(Some(Value::Object(Some(real_cf))));
                }
            }
            // The registry says some provider services this type but its real
            // SPI could not be built. The fallback only parses X.509, so for
            // anything else refusing is the honest answer — a wrong parse under
            // the right name is worse than a missing one.
            if !certificate_factory_stub_serves(&type_name) {
                return Err(cert_type_not_found(ctx, &type_name));
            }
            // gc-common w15-d: the `type` String is made FIRST and rooted, so
            // the factory's allocation is the last GC point; the name-based
            // store used to go through the factory's pre-allocation address.
            let spelling = ctx.create_string(&type_name);
            let spelling_pin = ctx.pin_native_root(spelling);
            let obj_new =
                try_alloc_concurrent_synthetic(ctx, "java/security/cert/CertificateFactory", 1);
            let spelling = ctx.read_native_pin(spelling_pin, spelling);
            ctx.unpin_native_roots(spelling_pin);
            let obj: ObjectRef = obj_new?;
            // W7-29 residual, closed 2026-08-12. This used to be
            // `ctx.set_field(obj, 0, Value::Object(None))`, and W7-29's "what
            // was deliberately not done" section named the consequence: in
            // real-JDK mode the funnel widens this object to the real
            // three-field layout (`provider`, `certFacSpi`, `type` — `javap
            // -p`), so raw slot 0 is `provider`, NOT `type`, and `getType()`
            // — which is real bytecode reading the real `type` field —
            // answered `null` where HotSpot echoes the caller's spelling
            // (`getInstance("x509").getType()` is `x509`).
            //
            // Writing BY NAME is the fix the record asked for and could not
            // verify: it lands on `type` whatever the layout is, and
            // `set_field_by_name` is a no-op when the field is absent
            // (synthetic-stub mode), so it is safe on both. The raw slot-0
            // write is gone rather than moved — nothing reads slot 0 of this
            // receiver (grepped), and writing a native's idea of a field over
            // whichever real field happens to sit at index 0 is the same
            // defect species as P1-C's stream carriers.
            ctx.set_field_by_name(obj, "type", Value::Object(Some(spelling)));
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cf,
        "getInstance",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/security/cert/CertificateFactory;",
        |ctx, args| {
            let provider_name = match args.get(1) {
                Some(Value::Object(Some(name))) => ctx.read_string(*name).unwrap_or_default(),
                _ => String::new(),
            };
            // gc-common w15-d: the type is read BEFORE `Security.getProvider`
            // (Java) runs, and the type String -- handed on to the one-argument
            // form below -- is pinned across it and the factory build. Both
            // used to be re-read from `args` after them.
            let type_name = cf_type_arg(ctx, args).unwrap_or_default();
            let type_arg = args.first().copied().unwrap_or(Value::Object(None));
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let type_h = match type_arg {
                Value::Object(Some(t)) => Some(scope.root(t)),
                _ => None,
            };
            // Do not silently substitute the default provider. The two-arg JCA
            // overload is a provider-selection API and must fail before any
            // certificate parsing when the requested provider is absent.
            let provider = scope.invoke(
                "java/security/Security",
                "getProvider",
                "(Ljava/lang/String;)Ljava/security/Provider;",
                &args[1..2],
            );
            if matches!(provider, Ok(Some(Value::Object(Some(_))))) {
                // The NAMED provider's own factory first — the one-argument form
                // below walks the chain and answers `SUN` for `X.509` no matter
                // who was asked. See
                // `provider_chain::try_build_real_certificate_factory_for`.
                if !type_name.is_empty() {
                    if let Ok(Some(real_cf)) =
                        crate::jca::provider_chain::try_build_real_certificate_factory_for(
                            &mut *scope,
                            Some(&provider_name),
                            &type_name,
                        )
                    {
                        return Ok(Some(Value::Object(Some(real_cf))));
                    }
                }
                let type_now = match type_h.as_ref() {
                    Some(h) => Value::Object(Some(scope.get(h))),
                    None => type_arg,
                };
                return scope.invoke(
                    "java/security/cert/CertificateFactory",
                    "getInstance",
                    "(Ljava/lang/String;)Ljava/security/cert/CertificateFactory;",
                    &[type_now],
                );
            }
            drop(scope);
            let detail = ctx.create_string(&format!("no such provider: {provider_name}"));
            Err(cratonvm_native_api::throwable_builder::throw_new(
                ctx,
                "java/security/NoSuchProviderException",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(detail))],
                || {
                    RuntimeError::SecurityException {
                        message: format!("no such provider: {provider_name}"),
                    }
                    .into()
                },
            ))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cf,
        "generateCertificate",
        "(Ljava/io/InputStream;)Ljava/security/cert/Certificate;",
        |ctx, args| {
            // Real-SPI fast path: a `CertificateFactory` built via
            // `getInstance(algo, Provider)` / `getInstance(algo, providerName)`
            // (both flow through `sun/security/jca/GetInstance.getInstance` ->
            // `getinstance_instance_provider[_obj]` in `jca::provider_chain`,
            // which run the class's REAL constructor) carries a genuine
            // `certFacSpi` field pointing at the provider's real
            // `CertificateFactorySpi` (e.g. BouncyCastle's
            // `org.bouncycastle.jcajce.provider.asymmetric.x509.CertificateFactory`).
            // Real `CertificateFactory.generateCertificate` is one line:
            // `return certFacSpi.engineGenerateCertificate(is)`. Delegating to
            // it here runs the genuine provider bytecode and produces the
            // provider's own concrete `Certificate` subclass — critical for
            // callers like BC's `JcaX509CertificateConverter.getCertificate`
            // that need a real `X509CertificateObject` (`getEncoded()`,
            // `verify()`, etc. are abstract on the bare `Certificate`/
            // `X509Certificate` classes and throw `AbstractMethodError`
            // otherwise). Only the OLD synthetic path — where `getInstance`
            // (1-arg, no provider) handed out the 1-field
            // `alloc_concurrent_synthetic` stub with no `certFacSpi` — falls
            // through to the legacy ad-hoc DER parser below.
            if let Some(Value::Object(Some(this))) = args.first() {
                if let Value::Object(Some(spi)) = ctx.get_field_by_name(*this, "certFacSpi") {
                    return ctx.invoke_virtual(
                        spi,
                        "engineGenerateCertificate",
                        "(Ljava/io/InputStream;)Ljava/security/cert/Certificate;",
                        &args[1..],
                    );
                }
            }

            // Read the full stream. Previously this only read a
            // `ByteArrayInputStream`'s field-0 `buf` array directly, so ANY
            // other InputStream subtype (e.g. a Netty/`SslContext` file or
            // buffer stream feeding `SelfSignedCertificate`'s cert) read zero
            // bytes and fell through to the empty-cert stub → 0-byte
            // `getEncoded()` → rustls `invalid peer certificate: BadEncoding`.
            // `p59_read_input_stream_fully` keeps the fast BAIS path and adds a
            // generic `read()`-loop fallback for every other stream type. See
            // http-server-sslengine-identity-singleton-clobber-FIXED.md.
            let der_data = if let Some(Value::Object(Some(is_ref))) = args.get(1) {
                match p59_read_input_stream_fully(ctx, *is_ref) {
                    Ok(bytes) if !bytes.is_empty() => Some(bytes),
                    _ => None,
                }
            } else {
                None
            };

            // BUGFIX (keycloak-07-04 x509-authoritykeyidentifier-npe): this is the
            // no-explicit-provider `getInstance("X.509")` path — the form virtually
            // all callers use, including BC's own `JcaX509CertificateConverter`
            // (`CertificateFactory.getInstance("X.509")`, no `.setProvider(...)`).
            // The old code here never stored the raw DER anywhere reachable by
            // `getEncoded()` unless the (default-off) `legacy-synthetic-crypto`
            // parser happened to be enabled AND succeed; otherwise `getEncoded()`
            // returned a 0-length byte[]. Re-parsing that empty array with BC's own
            // ASN.1 decoder (e.g. `new JcaX509CertificateHolder(cert)`, which
            // `createAuthorityKeyIdentifier` calls internally) hits
            // `ASN1InputStream.readObject()` returning `null` at immediate EOF, then
            // `ASN1UniversalType.checkedCast(null)` NPEs on `null.getClass()`.
            //
            // Build a REAL `sun.security.x509.X509CertImpl` from the DER instead —
            // same "prefer real over synthetic" pattern `keystore::make_x509_mirror`
            // already uses for KeyStore/TLS-peer certs. Real bytecode gives a
            // correct `getEncoded()` (and `checkValidity`/`verify`/
            // `getSubjectX500Principal`/etc. for free); if the real ctor itself
            // throws (malformed input), `make_x509_mirror` falls back to a
            // synthetic mirror that still stashes the DER in field 3, so
            // `getEncoded()` is never empty for a non-empty input stream.
            if let Some(ref raw) = der_data {
                // Accept PEM-armored streams too (real JDK's X509Factory sniffs
                // `-----BEGIN`): decode to DER first so the mirror stores real
                // cert bytes and `getEncoded()` is non-empty. A DER stream (no
                // armor) passes through `pem_block_to_der` unchanged. Fixes
                // Netty `SelfSignedCertificate` → empty `getEncoded()` → rustls
                // `invalid peer certificate: BadEncoding`
                // (http-server-sslengine-identity-singleton-clobber).
                let data = crate::pem_block_to_der(raw);
                let alias = basic_der_extract_names(&data)
                    .map(|(subject, _)| subject)
                    .unwrap_or_else(|| "CN=Unknown".into());
                return Ok(Some(Value::Object(Some(
                    crate::keystore::make_x509_mirror(ctx, &alias, &data)?,
                ))));
            }

            // Fallback: the input stream had no readable bytes at all.
            //
            // gc-common w15-d: the two Strings first (the first rooted across
            // the second), then the certificate, so the certificate is the
            // last GC point; all three stores used to go through its
            // pre-allocation address.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let sub_str = scope.create_string("CN=Unknown");
            let sub_h = scope.root(sub_str);
            let iss_str = scope.create_string("CN=Unknown");
            let iss_h = scope.root(iss_str);
            let cert: ObjectRef = try_alloc_concurrent_synthetic(
                &mut *scope,
                "java/security/cert/X509Certificate",
                3,
            )?;
            let sub = scope.get(&sub_h);
            let iss = scope.get(&iss_h);
            scope.set_field(cert, 0, Value::Object(Some(sub)));
            scope.set_field(cert, 1, Value::Object(Some(iss)));
            scope.set_field(cert, 2, Value::Long(0));
            Ok(Some(Value::Object(Some(cert))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cf,
        "generateCertificates",
        "(Ljava/io/InputStream;)Ljava/util/Collection;",
        |ctx, args| {
            // Real-SPI fast path — see the matching comment on
            // `generateCertificate` above; same rationale, same field.
            if let Some(Value::Object(Some(this))) = args.first() {
                if let Value::Object(Some(spi)) = ctx.get_field_by_name(*this, "certFacSpi") {
                    return ctx.invoke_virtual(
                        spi,
                        "engineGenerateCertificates",
                        "(Ljava/io/InputStream;)Ljava/util/Collection;",
                        &args[1..],
                    );
                }
            }

            // gc-common w15-d: the stream is read and the certificate built
            // BEFORE the list is allocated. The list and its array used to be
            // allocated first and then written after the stream's `read()`
            // loop and `make_x509_mirror` -- both Java -- through their
            // pre-call addresses (and `args[1]` was read after two
            // allocations).
            //
            // Try reading a cert from the input stream. Same DER-preservation fix
            // as `generateCertificate` above: build a real `X509CertImpl` (falls
            // back internally to a DER-stashing synthetic mirror) instead of a
            // bare 3-field stub whose `getEncoded()` would come back empty.
            let cert = match args.get(1) {
                Some(Value::Object(Some(is_ref))) => {
                    let raw = match p59_read_input_stream_fully(ctx, *is_ref) {
                        Ok(bytes) => bytes,
                        Err(_) => Vec::new(),
                    };
                    if raw.is_empty() {
                        None
                    } else {
                        // PEM-or-DER, same as `generateCertificate`.
                        let data = crate::pem_block_to_der(&raw);
                        let alias = basic_der_extract_names(&data)
                            .map(|(subject, _)| subject)
                            .unwrap_or_else(|| "CN=Unknown".into());
                        Some(crate::keystore::make_x509_mirror(ctx, &alias, &data)?)                    }                }
                _ => None,
            };
            // Return the certificate (if any) in a single-element list.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let cert_h = cert.map(|c| scope.root(c));
            let arr_new = scope.new_array(cratonvm_types::ArrayElementType::Reference, 10);
            let arr_h = scope.root(arr_new);
            let al: ObjectRef =
                try_alloc_concurrent_synthetic(&mut *scope, "java/util/ArrayList", 2)?;
            let arr = scope.get(&arr_h);
            scope.set_field(al, 0, Value::Object(Some(arr)));
            match cert_h.as_ref() {
                Some(h) => {
                    let cert_now = scope.get(h);
                    scope.set_array_element(arr, 0, Value::Object(Some(cert_now)));
                    scope.set_field(al, 1, Value::Int(1));
                }
                None => scope.set_field(al, 1, Value::Int(0)),
            }
            Ok(Some(Value::Object(Some(al))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // `sun.security.util.DerValue.getEncoded()` — an ALIAS for the method the
    // real class actually declares, `toByteArray()`.
    //
    // **This is a symptom fix and is deliberately recorded as one.** JDK 25's
    // `DerValue` has no `getEncoded()` (verified with `javap --module
    // java.base`), so the `NoSuchMethodError` netty's
    // `PemX509Certificate.append` raises is not a missing method — it is the
    // first place a WRONG OBJECT becomes visible: something on the
    // `CertificateFactory` path hands back a `DerValue` where an
    // `X509Certificate` was expected, and `append` is simply the first caller
    // to ask it for something only a certificate has.
    //
    // The alias is nevertheless correct for the value it returns: a `DerValue`
    // produced by parsing a certificate wraps that certificate's whole DER
    // SEQUENCE, and `toByteArray()` is exactly the encoding
    // `X509Certificate.getEncoded()` is contracted to produce. So the PEM
    // netty builds from it is the right PEM.
    //
    // What it does NOT do is fix the identity. Any other `X509Certificate`
    // method asked of that object — `getSubjectX500Principal`,
    // `checkValidity`, `getPublicKey` — still fails, and will fail with the
    // same shape of error. The producer is the real fix; see the
    // `openssl-key-material-and-engine-residuals` page.
    r.register(
        "sun/security/util/DerValue",
        "getEncoded",
        "()[B",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.invoke_virtual(this, "toByteArray", "()[B", &[])
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // Certificate base class
    let cert = "java/security/cert/Certificate";
    r.register(cert, "getType", "()Ljava/lang/String;", |ctx, _args| {
        let s = ctx.create_string("X.509");
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(cert, "getEncoded", "()[B", |ctx, args| {
        // NEW-13: when the X509Certificate was constructed from a TLS peer
        // cert chain (`SSLSession.getPeerCertificates`), the raw DER bytes
        // are stored directly in field 3 as a byte[]. Prefer that over the
        // legacy-synthetic-crypto `cert_store` lookup so that `getEncoded()`
        // returns a non-empty array even when crypto_impl is disabled.
        if let Some(Value::Object(Some(this))) = args.get(0) {
            if ctx.object_num_fields(*this) > 3 {
                if let Value::Object(Some(der_arr)) = ctx.get_field(*this, 3) {
                    let len = ctx.array_length(der_arr);
                    if len > 0 {
                        // gc-common w19-d: the stored DER array is rooted
                        // across the copy's allocation and copied from its
                        // current address (it was read through its address
                        // from before the allocation).
                        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
                        let der_h = scope.root(der_arr);
                        let out = scope.new_array(cratonvm_types::ArrayElementType::Byte, len);
                        let der_now = scope.get(&der_h);
                        for i in 0..len {
                            out_copy_byte(&mut *scope, der_now, out, i);
                        }
                        return Ok(Some(Value::Object(Some(out))));
                    }
                }
            }
        }
        #[cfg(feature = "legacy-synthetic-crypto")]
        if let Some(Value::Object(Some(this))) = args.get(0) {
            let cert_id = match ctx.get_field(*this, 2) {
                Value::Long(id) => id as u64,
                _ => 0,
            };
            if let Some(parsed) = crypto_impl::cert_get(cert_id) {
                // The array's own allocation is the only GC point here: the
                // fill loop below allocates nothing.
                let arr: ObjectRef =
                    ctx.new_array(cratonvm_types::ArrayElementType::Byte, parsed.encoded.len());
                for (i, &b) in parsed.encoded.iter().enumerate() {
                    ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
                }
                return Ok(Some(Value::Object(Some(arr))));
            }
        }
        // STUB-REMOVAL (wave 2): the tail used to hand back a ZERO-LENGTH
        // byte[]. An empty encoding is indistinguishable from "no certificate"
        // to every consumer (rustls answers it with `BadEncoding`, a pin/hash
        // check silently compares nothing), so a certificate object with no
        // recoverable DER must raise the exception its contract specifies
        // rather than fabricate one. Same treatment as the `X509Certificate`
        // override below.
        let this = obj_arg(args, 0)?;
        let nfields = ctx.object_num_fields(this);
        let message = format!("certificate has no encoded form ({nfields} fields)");
        Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/security/cert/CertificateEncodingException",
            &message,
        ))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        cert,
        "getPublicKey",
        "()Ljava/security/PublicKey;",
        |ctx, args| {
            let _ = (&ctx, &args); // used only under legacy-synthetic-crypto feature
            #[cfg(feature = "legacy-synthetic-crypto")]
            if let Some(Value::Object(Some(this))) = args.get(0) {
                let cert_id = match ctx.get_field(*this, 2) {
                    Value::Long(id) => id as u64,
                    _ => 0,
                };
                if let Some(parsed) = crypto_impl::cert_get(cert_id) {
                    let pk = try_alloc_concurrent_synthetic(ctx, "java/security/PublicKey", 4)?;
                    let alg_idx = match parsed.public_key_algorithm.as_str() {
                        "RSA" => 6,
                        "EC" => 7,
                        _ => 0,
                    };
                    ctx.set_field(pk, 0, Value::Int(alg_idx));
                    ctx.set_field(pk, 1, Value::Int(256));
                    ctx.set_field(pk, 2, Value::Int(parsed.public_key_bytes.len() as i32));
                    ctx.set_field(pk, 3, Value::Long(0));
                    return Ok(Some(Value::Object(Some(pk))));
                }
            }
            Ok(Some(Value::Object(None)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // SECURITY: `Certificate.verify(PublicKey)` MUST throw on a bad signature.
    // In the default build we deliberately DO NOT register a native override:
    // the real `java.security.cert` / `X509CertImpl.verify(PublicKey)` bytecode
    // then runs and performs the genuine signature check, throwing
    // `SignatureException` (or `InvalidKeyException` / `CertificateException`)
    // on failure. A no-op native that always returns `Ok(None)` would silently
    // certify any certificate against any key — a verification-bypass
    // vulnerability — so it is gated entirely behind the legacy feature.
    #[cfg(feature = "legacy-synthetic-crypto")]
    r.register(
        cert,
        "verify",
        "(Ljava/security/PublicKey;)V",
        |ctx, args| {
            if let Some(Value::Object(Some(this))) = args.get(0) {
                let cert_id = match ctx.get_field(*this, 2) {
                    Value::Long(id) => id as u64,
                    _ => 0,
                };
                if let Some(parsed) = crypto_impl::cert_get(cert_id) {
                    // Fail-closed: the legacy synthetic path can only attest to
                    // the signature it is able to verify with the material it
                    // holds. If verification does not succeed we throw rather
                    // than returning normally, so a caller never mistakes an
                    // unverifiable certificate for a verified one.
                    if !parsed.verify_signature(&parsed.public_key_bytes) {
                        return Err(p68_signature_failure(
                            ctx,
                            "certificate signature verification failed",
                        ));                    }                }            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // X509Certificate = 3-field (subject_str=0, issuer_str=1, cert_id=2)
    let x509 = "java/security/cert/X509Certificate";
    // gc-common w15-d: the four principal accessors share
    // `x509_dn_from_slot` / `x509_principal_from_slot`, which root the String
    // read off the certificate across the principal's allocation and across
    // the `X500Principal(String)` constructor (Java). Each copy stored the
    // String through the address read before those GC points.
    r.register(
        x509,
        "getSubjectDN",
        "()Ljava/security/Principal;",
        |ctx, args| x509_dn_from_slot(ctx, obj_arg(args, 0)?, 0), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        x509,
        "getIssuerDN",
        "()Ljava/security/Principal;",
        |ctx, args| x509_dn_from_slot(ctx, obj_arg(args, 0)?, 1), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        x509,
        "getSubjectX500Principal",
        "()Ljavax/security/auth/x500/X500Principal;",
        |ctx, args| x509_principal_from_slot(ctx, obj_arg(args, 0)?, 0), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        x509,
        "getIssuerX500Principal",
        "()Ljavax/security/auth/x500/X500Principal;",
        |ctx, args| x509_principal_from_slot(ctx, obj_arg(args, 0)?, 1), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(x509, "getNotBefore", "()Ljava/util/Date;", |ctx, args| {
        #[cfg(feature = "legacy-synthetic-crypto")]
        if let Some(Value::Object(Some(this))) = args.get(0) {
            let cert_id = match ctx.get_field(*this, 2) {
                Value::Long(id) => id as u64,
                _ => 0,
            };
            if let Some(parsed) = crypto_impl::cert_get(cert_id) {
                let date = try_alloc_concurrent_synthetic(ctx, "java/util/Date", 1)?;
                ctx.set_field(date, 0, Value::Long(parsed.not_before * 1000)); // millis
                return Ok(Some(Value::Object(Some(date))));
            }
        }
        let _ = (ctx, args);
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(x509, "getNotAfter", "()Ljava/util/Date;", |ctx, args| {
        #[cfg(feature = "legacy-synthetic-crypto")]
        if let Some(Value::Object(Some(this))) = args.get(0) {
            let cert_id = match ctx.get_field(*this, 2) {
                Value::Long(id) => id as u64,
                _ => 0,
            };
            if let Some(parsed) = crypto_impl::cert_get(cert_id) {
                let date = try_alloc_concurrent_synthetic(ctx, "java/util/Date", 1)?;
                ctx.set_field(date, 0, Value::Long(parsed.not_after * 1000));
                return Ok(Some(Value::Object(Some(date))));
            }
        }
        let _ = (ctx, args);
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        x509,
        "getSerialNumber",
        "()Ljava/math/BigInteger;",
        |ctx, args| {
            #[cfg(feature = "legacy-synthetic-crypto")]
            if let Some(Value::Object(Some(this))) = args.get(0) {
                let cert_id = match ctx.get_field(*this, 2) {
                    Value::Long(id) => id as u64,
                    _ => 0,
                };
                if let Some(parsed) = crypto_impl::cert_get(cert_id) {
                    // Create BigInteger with value from serial number bytes
                    // Convert serial number bytes to decimal string
                    let serial_hex: String = parsed
                        .serial_number
                        .iter()
                        .map(|b| format!("{:02x}", b))
                        .collect();
                    let serial_dec = u128::from_str_radix(&serial_hex, 16)
                        .map(|v| v.to_string())
                        .unwrap_or_else(|_| "0".to_string());
                    // `?`, because `bi_alloc` is fallible: it allocates a
                    // `BigInteger` and its `mag:[I`, either of which can fail.
                    // Without it the `Result` itself went into `Value::Object`,
                    // which is `E0308` — this arm is behind
                    // `legacy-synthetic-crypto` and nothing that builds by
                    // default compiles it.
                    let bi = bi_alloc(ctx, &serial_dec)?;
                    return Ok(Some(Value::Object(Some(bi))));
                }
            }
            let _ = (ctx, args);
            Ok(Some(Value::Object(None)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        x509,
        "getSigAlgName",
        "()Ljava/lang/String;",
        |ctx, args| {
            #[cfg(feature = "legacy-synthetic-crypto")]
            if let Some(Value::Object(Some(this))) = args.get(0) {
                let cert_id = match ctx.get_field(*this, 2) {
                    Value::Long(id) => id as u64,
                    _ => 0,
                };
                if let Some(parsed) = crypto_impl::cert_get(cert_id) {
                    let s = ctx.create_string(&parsed.sig_algorithm);
                    return Ok(Some(Value::Object(Some(s))));
                }
            }
            let _ = args;
            let s = ctx.create_string("SHA256withRSA");
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(x509, "getVersion", "()I", |ctx, args| {
        #[cfg(feature = "legacy-synthetic-crypto")]
        if let Some(Value::Object(Some(this))) = args.get(0) {
            let cert_id = match ctx.get_field(*this, 2) {
                Value::Long(id) => id as u64,
                _ => 0,
            };
            if let Some(parsed) = crypto_impl::cert_get(cert_id) {
                return Ok(Some(Value::Int(parsed.version as i32)));
            }
        }
        let _ = (ctx, args);
        Ok(Some(Value::Int(3)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(x509, "getType", "()Ljava/lang/String;", |ctx, _args| {
        let s = ctx.create_string("X.509");
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Bridge);
    // STUB-REMOVAL (wave 2): in the DEFAULT build (no `legacy-synthetic-crypto`)
    // this returned an EMPTY byte[] for every certificate. That is not a
    // harmless placeholder: `getEncoded()` is how a caller obtains the DER it
    // then hashes, pins, re-parses or feeds to a trust store, so an empty array
    // turns a real certificate into "no certificate" without any error —
    // rustls answers a zero-length chain with `BadEncoding`, and wave 1 had to
    // work around this from `keystore.rs`. The synthetic X.509 mirror
    // (`keystore::make_x509_mirror`'s fallback, `t27_tls::
    // register_accepted_issuers`, `phases_early`'s getCertificate path) stashes
    // the real DER in slot 3; read it, exactly as the `Certificate.getEncoded`
    // sibling above already does. `x509_mirror_der` is the shared reader.
    r.register(x509, "getEncoded", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(der) = x509_mirror_der(ctx, this) {
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, der.len());
            for (i, &b) in der.iter().enumerate() {
                ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
            }
            return Ok(Some(Value::Object(Some(arr))));
        }
        // No DER anywhere on this object. Real `X509Certificate.getEncoded()`
        // throws `CertificateEncodingException` rather than handing back an
        // empty array that the caller would mistake for a valid encoding.
        Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/security/cert/CertificateEncodingException",
            "certificate has no encoded form",
        ))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        x509,
        "getPublicKey",
        "()Ljava/security/PublicKey;",
        |ctx, args| {
            #[cfg(feature = "legacy-synthetic-crypto")]
            if let Some(Value::Object(Some(this))) = args.get(0) {
                let cert_id = match ctx.get_field(*this, 2) {
                    Value::Long(id) => id as u64,
                    _ => 0,
                };
                if let Some(parsed) = crypto_impl::cert_get(cert_id) {
                    let pk = try_alloc_concurrent_synthetic(ctx, "java/security/PublicKey", 4)?;
                    let alg_idx = match parsed.public_key_algorithm.as_str() {
                        "RSA" => 6,
                        "EC" => 7,
                        _ => 0,
                    };
                    ctx.set_field(pk, 0, Value::Int(alg_idx));
                    ctx.set_field(pk, 1, Value::Int(256));
                    ctx.set_field(pk, 2, Value::Int(parsed.public_key_bytes.len() as i32));
                    ctx.set_field(pk, 3, Value::Long(0));
                    return Ok(Some(Value::Object(Some(pk))));
                }
            }
            let _ = (ctx, args);
            Ok(Some(Value::Object(None)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // SECURITY: see the `Certificate.verify` note above. The default build does
    // NOT register this native, so the real `X509CertImpl.verify(PublicKey)`
    // bytecode performs the genuine signature check and throws on failure.
    // Previously this discarded the `verify_signature` result and always
    // returned success, certifying any certificate against any key.
    #[cfg(feature = "legacy-synthetic-crypto")]
    r.register(
        x509,
        "verify",
        "(Ljava/security/PublicKey;)V",
        |ctx, args| {
            if let Some(Value::Object(Some(this))) = args.get(0) {
                let cert_id = match ctx.get_field(*this, 2) {
                    Value::Long(id) => id as u64,
                    _ => 0,
                };
                if let Some(parsed) = crypto_impl::cert_get(cert_id) {
                    // Fail-closed: propagate a verification failure as a thrown
                    // SignatureException instead of discarding the result.
                    if !parsed.verify_signature(&parsed.public_key_bytes) {
                        return Err(p68_signature_failure(
                            ctx,
                            "certificate signature verification failed",
                        ));                    }                }            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // STUB-REMOVAL (wave 2): both overloads were hard-coded PASSES in the
    // default build (the whole body was `#[cfg(feature =
    // "legacy-synthetic-crypto")]`-gated, leaving `Ok(None)`). See
    // `x509_check_validity_at` for the enforcement and the exception contract.
    r.register(x509, "checkValidity", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        x509_check_validity_at(ctx, this, now)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(x509, "checkValidity", "(Ljava/util/Date;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let date = obj_arg(args, 1)?;
        // `Date.getTime()` is a real virtual call that can allocate/safepoint,
        // so `this` must be pinned across it (the synthetic 1-field Date shape
        // is not the only one that reaches here — a real `java.util.Date`
        // keeps its millis in `fastTime`, which no fixed slot index finds).
        let pin = ctx.pin_native_root(this);
        let date_pin = ctx.pin_native_root(date);
        let called = ctx.invoke_virtual(date, "getTime", "()J", &[]);
        let this = ctx.read_native_pin(pin, this);
        let date = ctx.read_native_pin(date_pin, date);
        ctx.unpin_native_roots(pin);
        let millis = match called? {
            Some(Value::Long(ms)) => ms,
            // Unreadable instant: fall back to the synthetic Date's slot 0.
            _ => match ctx.get_field(date, 0) {
                Value::Long(ms) => ms,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("checkValidity: date has no time value".into()),
                    }
                    .into());
                }
            },
        };
        x509_check_validity_at(ctx, this, millis.div_euclid(1000))
    }, cratonvm_native_api::NativeKind::Bridge);

    // getSignature() -> byte[] (on X509Certificate)
    r.register(x509, "getSignature", "()[B", |ctx, args| {
        #[cfg(feature = "legacy-synthetic-crypto")]
        if let Some(Value::Object(Some(this))) = args.get(0) {
            let cert_id = match ctx.get_field(*this, 2) {
                Value::Long(id) => id as u64,
                _ => 0,
            };
            if let Some(parsed) = crypto_impl::cert_get(cert_id) {
                let arr = ctx.new_array(
                    cratonvm_types::ArrayElementType::Byte,
                    parsed.signature_bytes.len(),
                );
                for (i, &b) in parsed.signature_bytes.iter().enumerate() {
                    ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
                }
                return Ok(Some(Value::Object(Some(arr))));
            }
        }
        let _ = args;
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 0);
        Ok(Some(Value::Object(Some(arr))))
    }, cratonvm_native_api::NativeKind::Bridge);

    // getTBSCertificate() -> byte[]
    r.register(x509, "getTBSCertificate", "()[B", |ctx, args| {
        #[cfg(feature = "legacy-synthetic-crypto")]
        if let Some(Value::Object(Some(this))) = args.get(0) {
            let cert_id = match ctx.get_field(*this, 2) {
                Value::Long(id) => id as u64,
                _ => 0,
            };
            if let Some(parsed) = crypto_impl::cert_get(cert_id) {
                let arr = ctx.new_array(
                    cratonvm_types::ArrayElementType::Byte,
                    parsed.tbs_bytes.len(),
                );
                for (i, &b) in parsed.tbs_bytes.iter().enumerate() {
                    ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
                }
                return Ok(Some(Value::Object(Some(arr))));
            }
        }
        let _ = args;
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 0);
        Ok(Some(Value::Object(Some(arr))))
    }, cratonvm_native_api::NativeKind::Bridge);
}


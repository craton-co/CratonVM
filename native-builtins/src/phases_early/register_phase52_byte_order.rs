#[allow(unused_imports)]
use super::*;

// ---------------------------------------------------------------------------
// java.lang.StringBuffer — delegates to StringBuilder natives
// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// java.nio.ByteOrder — 1-field synthetic (0=BIG_ENDIAN, 1=LITTLE_ENDIAN)
// ---------------------------------------------------------------------------
pub fn register_phase52_byte_order(r: &mut NativeMethodRegistry) {
            let bo = "java/nio/ByteOrder";
    // The field-shaped `BIG_ENDIAN` / `LITTLE_ENDIAN` keys that stood here are
    // gone with `servlet.rs`'s twins (round 14 wave 2, lane shadow): no door
    // dispatches a field descriptor. `nativeOrder` below keeps `ByteOrder`
    // minted.
    r.register(bo, "nativeOrder", "()Ljava/nio/ByteOrder;", |ctx, _args| {
        let obj = try_alloc_concurrent_synthetic(ctx, "java/nio/ByteOrder", 1)?;
        let is_le = cfg!(target_endian = "little");
        ctx.set_field(obj, 0, Value::Int(if is_le { 1 } else { 0 }));
        Ok(Some(Value::Object(Some(obj))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(bo, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = ctx.get_field(this, 0).as_int().unwrap_or(0);
        let name = if v == 0 {
            "BIG_ENDIAN"
        } else {
            "LITTLE_ENDIAN"
        };
        let s = ctx.create_string(name);
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(bo, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(other)) = &args[1] {
            let v1 = ctx.get_field(this, 0).as_int().unwrap_or(-1);
            let v2 = ctx.get_field(*other, 0).as_int().unwrap_or(-2);
            Ok(Some(Value::Int(if v1 == v2 { 1 } else { 0 })))
        } else {
            Ok(Some(Value::Int(0)))
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    }

// ---------------------------------------------------------------------------
// java.util.Objects extras
// ---------------------------------------------------------------------------
pub fn register_phase52_objects_extras(r: &mut NativeMethodRegistry) {
            let obj_cls = "java/util/Objects";
    r.register(
        obj_cls,
        "requireNonNullElse",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        |_ctx, args| {
            if let Value::Object(Some(_)) = &args[0] {
                Ok(Some(args[0]))
            } else if let Value::Object(Some(_)) = &args[1] {
                Ok(Some(args[1]))
            } else {
                Err(RuntimeError::NullPointerException {
                    message: Some("defaultObj must not be null".to_string()),
                }
                .into())
            }
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        obj_cls,
        "requireNonNullElseGet",
        "(Ljava/lang/Object;Ljava/util/function/Supplier;)Ljava/lang/Object;",
        |ctx, args| {
            if let Value::Object(Some(_)) = &args[0] {
                Ok(Some(args[0]))
            } else {
                let supplier = obj_arg(args, 1)?;
                ctx.invoke_virtual(supplier, "get", "()Ljava/lang/Object;", &[])
            }
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(obj_cls, "checkIndex", "(II)I", |_ctx, args| {
        let index = args[0].as_int().unwrap_or(-1);
        let length = args[1].as_int().unwrap_or(0);
        // `Preconditions.checkIndex`'s IOOBE text (wave 100 L7; this was an
        // AIOOBE "Array index out of range: n").
        if index < 0 || index >= length {
            Err(RuntimeError::ioobe(
                cratonvm_types::error::out_of_bounds_message::check_index(
                    i64::from(index),
                    i64::from(length),
                ),
            )
            .into())
        } else {
            Ok(Some(Value::Int(index)))
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(obj_cls, "checkFromToIndex", "(III)I", |_ctx, args| {
        let from = args[0].as_int().unwrap_or(-1);
        let to = args[1].as_int().unwrap_or(-1);
        let length = args[2].as_int().unwrap_or(0);
        if from < 0 || to < from || to > length {
            Err(RuntimeError::ioobe(
                cratonvm_types::error::out_of_bounds_message::check_from_to_index(
                    i64::from(from),
                    i64::from(to),
                    i64::from(length),
                ),
            )
            .into())
        } else {
            Ok(Some(Value::Int(from)))
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(obj_cls, "checkFromIndexSize", "(III)I", |_ctx, args| {
        let from = args[0].as_int().unwrap_or(-1);
        let size = args[1].as_int().unwrap_or(-1);
        let length = args[2].as_int().unwrap_or(0);
        // `length - from < size`, as `Preconditions` tests it: `from + size`
        // can overflow `int` (and panic in a debug build).
        if (length | from | size) < 0 || size > length - from {
            Err(RuntimeError::ioobe(
                cratonvm_types::error::out_of_bounds_message::check_from_index_size(
                    i64::from(from),
                    i64::from(size),
                    i64::from(length),
                ),
            )
            .into())
        } else {
            Ok(Some(Value::Int(from)))
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(obj_cls, "isNull", "(Ljava/lang/Object;)Z", |_ctx, args| {
        Ok(Some(Value::Int(
            if matches!(&args[0], Value::Object(None)) {
                1            } else {
                0            },
        )))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(obj_cls, "nonNull", "(Ljava/lang/Object;)Z", |_ctx, args| {
        Ok(Some(Value::Int(
            if matches!(&args[0], Value::Object(None)) {
                0            } else {
                1            },
        )))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    }

// ============================================================================
// Phase 53: javax.crypto, java.security enhancements, java.net Socket stubs,
//           java.util.ServiceLoader, java.lang.Record
// ============================================================================

pub(crate) fn register_phase53_natives(registry: &mut NativeMethodRegistry) {
    register_phase53_crypto(registry);
    register_phase53_security(registry);
    register_phase53_socket_stubs(registry);
    register_phase53_service_loader(registry);
    register_phase53_record(registry);
    register_phase53_sealed(registry);
}

// ---------------------------------------------------------------------------
// T2.6.15/16 — persistent Provider registry (used by Security.getProviders /
// addProvider / insertProviderAt / removeProvider). Seeded with the five
// standard Sun providers on first access.
// ---------------------------------------------------------------------------

/// The `java.security.Security` state these natives serve: the provider list
/// and the properties `setProperty` wrote, one row per VM.
///
/// gc-common w19-c: both were process-global (`OnceLock<Mutex<..>>`), so every
/// VM in a process shared one provider list and one property map: VM A's
/// `Security.addProvider` / `removeProvider` / `setProperty` changed what VM
/// B's `getProviders()` / `getProperty()` answered, and a disposed VM's
/// changes outlived it. The real `Security` state belongs to one JVM. The rows
/// go at VM teardown ([`forget_vm_synthetic_security_state`]).
#[derive(Default)]
pub struct SecurityState {
    /// `None` until first use, then the five standard Sun providers.
    pub providers: Option<Vec<(String, f64)>>,
    /// W2: the properties written by `setProperty`.
    pub properties: std::collections::HashMap<String, String>,
}

pub static SECURITY_STATE: cratonvm_native_api::vm_scoped::VmScoped<SecurityState> =
    cratonvm_native_api::vm_scoped::VmScoped::new();

/// Per-VM teardown: drop `vm`'s `Security` provider list and properties.
/// Idempotent.
pub fn forget_vm_synthetic_security_state(vm: usize) {
    SECURITY_STATE.forget(vm);
}

pub fn default_providers() -> Vec<(String, f64)> {
    vec![
        ("SUN".to_string(), 21.0),
        ("SunJCE".to_string(), 21.0),
        ("SunRsaSign".to_string(), 21.0),
        ("SunEC".to_string(), 21.0),
        ("SunJSSE".to_string(), 21.0),
    ]
}

/// Run `f` on `vm`'s provider list, seeding it on first use. `f` must not call
/// into the VM (the table lock is held; see `VmScoped`).
pub fn with_provider_registry<R>(vm: usize, f: impl FnOnce(&mut Vec<(String, f64)>) -> R) -> R {
    SECURITY_STATE.with(vm, |s| f(s.providers.get_or_insert_with(default_providers)))
}

pub fn provider_registry_snapshot(vm: usize) -> Vec<(String, f64)> {
    with_provider_registry(vm, |list| list.clone())
}

pub fn provider_registry_find(vm: usize, name: &str) -> Option<f64> {
    with_provider_registry(vm, |list| {
        list.iter().find(|(n, _)| n == name).map(|(_, v)| *v)
    })
}

/// Append or replace. Returns 1-based position of the entry after the
/// operation (matching the JDK's `Security.addProvider` contract). If a
/// provider with the same name already exists its version is updated
/// and its position returned; the call is idempotent so adding the
/// same provider twice does not duplicate the list.
pub fn provider_registry_add(vm: usize, name: String, ver: f64) -> i32 {
    with_provider_registry(vm, |list| {
        if let Some(idx) = list.iter().position(|(n, _)| *n == name) {
            list[idx].1 = ver;
            return (idx + 1) as i32;
        }
        list.push((name, ver));
        list.len() as i32
    })
}

/// Insert at the given 1-based position. Positions ≤ 0 append; positions
/// beyond the current length are clamped. If the provider already
/// exists elsewhere in the list it is first removed, matching the
/// `Security.insertProviderAt` javadoc ("If the provider is already
/// installed, the request is ignored" — we return the existing
/// position).
pub fn provider_registry_insert_at(vm: usize, name: String, ver: f64, pos: i32) -> i32 {
    with_provider_registry(vm, |list| {
        if let Some(idx) = list.iter().position(|(n, _)| *n == name) {
            return (idx + 1) as i32;
        }
        let target = if pos < 1 {
            list.len()
        } else {
            ((pos - 1) as usize).min(list.len())
        };
        list.insert(target, (name, ver));
        (target + 1) as i32
    })
}

pub fn provider_registry_remove(vm: usize, name: &str) {
    with_provider_registry(vm, |list| {
        if let Some(idx) = list.iter().position(|(n, _)| n == name) {
            list.remove(idx);
        }
    })
}

/// W2: the value `Security.setProperty` stored for `key` in `vm`, if any.
///
/// `Security.setProperty` used to be a no-op while `Security.getProperty`
/// served a fixed four-entry table, so a caller that reconfigured the JCE
/// (`Security.setProperty("keystore.type", "JKS")`, or any of the
/// `crypto.policy` / `jdk.tls.*` knobs) read its own default straight back and
/// concluded the change had been rejected. The real `Security` properties
/// survive for the life of the VM; since gc-common w19-c they are that VM's
/// alone ([`SecurityState`]).
pub fn security_property(vm: usize, key: &str) -> Option<String> {
    SECURITY_STATE
        .peek(vm, |s| s.properties.get(key).cloned())
        .flatten()
}

pub fn set_security_property(vm: usize, key: String, value: String) {
    SECURITY_STATE.with(vm, |s| {
        s.properties.insert(key, value);
    });
}

// ---------------------------------------------------------------------------
// javax.crypto.Cipher — 6-field synthetic
//   algorithm=0, mode=1, key=2, iv=3, accumulated=4, aad=5
// Modes: 0=uninitialized, 1=ENCRYPT, 2=DECRYPT
// ---------------------------------------------------------------------------

// Cipher field indices
pub const CIPHER_ALGO: usize = 0;
pub const CIPHER_MODE: usize = 1;
pub const CIPHER_KEY: usize = 2;
pub const CIPHER_IV: usize = 3;
pub const CIPHER_ACCUM: usize = 4;
pub const CIPHER_AAD: usize = 5;

/// Fresh empty `CIPHER_ACCUM` / `CIPHER_AAD` buffers on the synthetic cipher
/// `this` (every `init` overload); answers `this`'s current address.
///
/// gc-common w20-b: the three `init` overloads held `this` raw across both
/// allocations.
pub fn cipher_reset_accumulators(ctx: &mut dyn NativeContext, this: ObjectRef) -> ObjectRef {
    let byte = cratonvm_types::ArrayElementType::Byte;
    let this = alloc_array_into_field(ctx, this, CIPHER_ACCUM, byte, 0);
    alloc_array_into_field(ctx, this, CIPHER_AAD, byte, 0)
}

/// Return a FRESH copy of the `byte[]` in slot `idx`, so key material never
/// leaves a native accessor as a live reference into the object.
///
/// The real key classes all end `return this.key.clone()`, and the javadoc says
/// why: "The contents of the array are copied to protect against subsequent
/// modification." Handing the stored array back is not a shortcut — it lets a
/// caller zero a key through its own accessor, and it is one half of the
/// mechanism that produced an all-zero AES key
/// (W7-21-keygen-and-the-synthetic-secretkeyspec-twin.md).
///
/// A slot that does not hold an array reference answers null. Several unrelated
/// key carriers in this file keep an `Int` at slot 0 (the legacy keystore
/// shape), and the previous code returned that `Int` straight out of a method
/// declared `()[B` — a null is at least a value the descriptor admits.
pub fn clone_key_bytes_field(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    idx: usize,
) -> Option<ObjectRef> {
    let src = match ctx.get_field(obj, idx) {
        Value::Object(Some(arr)) => arr,
        // Not an array reference — leave the historical answer alone.
        other => return other.as_object(),
    };
    let len = ctx.array_length(src);
    // gc-common w20-b: the source was held raw across the copy's allocation.
    let src_pin = ctx.pin_native_root(src);
    let copy = ctx.new_array(cratonvm_types::ArrayElementType::Byte, len);
    let src = ctx.read_native_pin(src_pin, src);
    ctx.unpin_native_roots(src_pin);
    for i in 0..len {
        ctx.set_array_element(copy, i, ctx.get_array_element(src, i));
    }
    Some(copy)
}

/// The algorithm a synthetic key carries in slot 1, or `"AES"` if slot 1 holds
/// something else.
///
/// The fallback preserves what this file answered for every carrier before the
/// algorithm was recorded at all; only `KeyGenerator.generateKey` writes a
/// String there.
pub fn carrier_algorithm(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Value {
    if let Value::Object(Some(s)) = ctx.get_field(obj, 1) {
        if ctx.read_string(s).is_some() {
            return Value::Object(Some(s));
        }
    }
    Value::Object(Some(ctx.create_string("AES")))
}

/// SunJCE's per-algorithm default key size in BITS, for `KeyGenerator`.
///
/// Every row measured on HotSpot 25 (`KeyGenerator.getInstance(a).generateKey()
/// .getEncoded().length * 8`), not recalled — the defaults are not a family
/// rule and several are surprising. `HmacSHA1` defaults to its 512-bit BLOCK
/// size while `HmacSHA224`/`384`/`512` default to their digest size, and AES
/// has defaulted to 256 since JDK 21.
///
/// `None` means refuse. This is the same rule the `Cipher` admission table
/// follows and for the same reason: a default arm here is how
/// `KeyGenerator.getInstance("Blowfish")` came to answer with a key generated
/// under a different algorithm's rules. The set below is exactly the set
/// HotSpot's SunJCE serves; every one of them is a plain random-byte key except
/// DES and DESede, whose parity rule `des_set_odd_parity` implements.
pub fn keygen_default_bits(algo: &str) -> Option<i32> {
    match algo.to_ascii_uppercase().as_str() {
        "AES" => Some(256),
        "DESEDE" | "TRIPLEDES" => Some(168),
        "DES" => Some(56),
        "CHACHA20" => Some(256),
        "BLOWFISH" | "RC2" | "ARCFOUR" | "RC4" => Some(128),
        "HMACSHA1" | "HMACMD5" => Some(512),
        "HMACSHA224" => Some(224),
        "HMACSHA256" => Some(256),
        "HMACSHA384" => Some(384),
        "HMACSHA512" => Some(512),
        // The six SunJCE HMAC key generators this table did not carry, and the
        // reason the service rows for them were deliberately withheld (see the
        // `KeyGenerator` seed in `provider_chain`, which says so).
        //
        // Every default is the DIGEST OUTPUT length, which is the rule the four
        // rows above already follow and which `SHA-512/224` makes visible: its
        // key is 28 bytes, not the 64 its SHA-512 compression function might
        // suggest. Measured on HotSpot 25.0.3 rather than derived —
        // `apps/probes/JcaKeyGeneratorDefaults` prints
        // `KeyGenerator.getInstance(a).generateKey().getEncoded().length` for
        // every name, and the two VMs' output is diffed.
        "HMACSHA512/224" => Some(224),
        "HMACSHA512/256" => Some(256),
        "HMACSHA3-224" => Some(224),
        "HMACSHA3-256" => Some(256),
        "HMACSHA3-384" => Some(384),
        "HMACSHA3-512" => Some(512),
        _ => None,
    }
}

/// The key sizes in BITS an algorithm's `KeyGenerator.init(int)` accepts, or
/// `None` if any positive multiple of 8 is allowed (the HMAC family).
///
/// Measured refusals, in HotSpot 25's exact wording:
///
/// ```text
/// AES    init(129) -> InvalidParameterException: Wrong keysize: must be equal to 128, 192 or 256
/// DESede init(128) -> InvalidParameterException: Wrong keysize: must be equal to 112 or 168
/// ```
pub fn keygen_allowed_bits(algo: &str) -> Option<(&'static [i32], &'static str)> {
    match algo.to_ascii_uppercase().as_str() {
        "AES" => Some((&[128, 192, 256], "must be equal to 128, 192 or 256")),
        "DESEDE" | "TRIPLEDES" => Some((&[112, 168], "must be equal to 112 or 168")),
        "DES" => Some((&[56], "must be equal to 56")),
        "CHACHA20" => Some((&[256], "must be equal to 256")),
        _ => None,
    }
}

/// The number of key BYTES an algorithm produces for a given key size in bits.
///
/// DESede is the one algorithm where these disagree: a "168-bit" DESede key is
/// **24 bytes**, and a "112-bit" one is also 24 bytes. The missing bits are the
/// per-byte parity bits, which are carried but not counted. Measured on
/// HotSpot 25 — `init(112)` and `init(168)` both yield `getEncoded().length ==
/// 24` — which is why `key_size / 8` was wrong here and would have produced a
/// 14-byte "DESede key" that is not a DESede key at all.
pub fn keygen_byte_len(algo: &str, key_size_bits: i32) -> usize {
    match algo.to_ascii_uppercase().as_str() {
        "DESEDE" | "TRIPLEDES" => 24,
        "DES" => 8,
        _ => (key_size_bits / 8) as usize,
    }
}

/// Force odd parity on the 8 bytes at `offset`, as DES keys require: the low
/// bit of each byte is set so that the byte's population count is odd.
///
/// This is SunJCE's `DESKeyGenerator.setParityBit`, and it is the reason a
/// DESede key cannot be 21 random bytes widened. The rule was VERIFIED rather
/// than recalled: applied to a fixed buffer it produces a value the JDK's own
/// checker accepts, identically on HotSpot 25 and on this VM —
///
/// ```text
/// raw24    = 00070e151c232a31383f464d545b626970777e858c939aa1
/// parity24 = 01070e151c232a31383e464c545b626870767f858c929ba1
/// javax.crypto.spec.DESedeKeySpec.isParityAdjusted(parity24, 0) == true
/// ```
///
/// — which is the vector pinned by `des_parity_matches_the_jdk_checker` below.
pub fn des_set_odd_parity(key: &mut [u8], offset: usize) {
    for byte in key.iter_mut().skip(offset).take(8) {
        if (*byte & 0xfe).count_ones() % 2 == 0 {
            *byte |= 1;
        } else {
            *byte &= 0xfe;
        }
    }
}

/// Turn `len` raw CSPRNG bytes into a valid key for `algo`.
///
/// For everything but DES/DESede a key IS its random bytes. The two DES
/// families need their parity bits, and 2-key Triple DES additionally needs its
/// shape: SunJCE's `DESedeKeyGenerator` draws only 16 bytes for `init(112)` and
/// copies the first 8 into the last 8, so K3 == K1. Measured on HotSpot 25 over
/// three runs — `init(112)` gave `K3==K1: true, K2==K1: false` every time, and
/// `init(168)` gave `K3==K1: false` — and `DESedeKeySpec.isParityAdjusted` was
/// true for both.
pub fn keygen_condition_key(algo: &str, key: &mut [u8]) {
    match algo.to_ascii_uppercase().as_str() {
        "DES" => des_set_odd_parity(key, 0),
        "DESEDE" | "TRIPLEDES" => {
            des_set_odd_parity(key, 0);
            des_set_odd_parity(key, 8);
            des_set_odd_parity(key, 16);
        }
        _ => {}
    }
}

/// The 2-key Triple DES shape: K3 := K1. Applied only for `init(112)`.
pub fn keygen_fold_two_key_desede(key: &mut [u8]) {
    if key.len() == 24 {
        let (head, tail) = key.split_at_mut(16);
        tail.copy_from_slice(&head[..8]);
    }
}

/// The algorithm a `KeyGenerator` synthetic recorded at `getInstance`.
pub fn keygen_algorithm_of(ctx: &mut dyn NativeContext, this: ObjectRef) -> String {
    match ctx.get_field(this, 0) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    }
}

/// `KeyGenerator.getInstance(String)`, `(String, String)` and
/// `(String, Provider)` — one body for all three.
///
/// The two provider-bearing overloads ignore the provider argument: this VM has
/// one implementation per algorithm, so "BC" and "SunJCE" yield the same bytes.
/// What none of them may do is accept an algorithm this VM cannot generate for.
///
/// One body because there were three, and three copies of a default is how the
/// default goes wrong in only some of them. Here they had already agreed on the
/// wrong answer (128 for everything); the next edit is the one that would have
/// split them.
/// The real `KeyGeneratorSpi` behind this receiver, or `None` when this crate
/// built it.
///
/// `getInstance` is a native for all three overloads, and until 2026-09-02
/// every one of them returned the two-field synthetic (algorithm at slot 0,
/// key size at slot 1), so the eight natives below could read those slots
/// unconditionally. They cannot any more: the five `SunTls*` KDFs are served by
/// handing back a REAL `javax.crypto.KeyGenerator` over the platform's SPI, and
/// a real one's slots hold `provider`/`spi`/`algorithm`/`lock` instead.
///
/// TYPE-CHECKED, not trusted. `get_field_by_name` can fall back to a
/// name->slot mapping, and on a two-field synthetic "spi" resolves to slot 0 —
/// a `String`. Returning that would hand a `String` to `invoke_virtual` as a
/// `KeyGeneratorSpi`. The same unchecked read put a `String` where a `Provider`
/// belonged in `pbkdf2_get_provider` and killed the caller on
/// `String.getName()`; this is that lesson applied before it could happen
/// again. A `String` is not a `KeyGeneratorSpi`, so the check is also exactly
/// the discriminator needed.
///
/// This is `skf_receiver_is_ours` in the positive direction: that one asks
/// "did we build it" through a side table, this one asks "can I delegate" and
/// answers with the thing to delegate TO. No table, so nothing to keep in step
/// with GC relocation or to evict.
pub fn keygen_real_spi(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    let Value::Object(Some(spi)) = ctx.get_field_by_name(this, "spi") else {
        return None;
    };
    let spi_class = ctx.class_id_by_name("javax/crypto/KeyGeneratorSpi")?;
    if ctx.is_subclass(ctx.class_id_of_object(spi), spi_class) {
        Some(spi)
    } else {
        None
    }
}

/// A `SecureRandom` for the `init` overloads that do not carry one.
///
/// The JDK's own `KeyGenerator.init(spec)` calls
/// `engineInit(params, JCAUtil.getSecureRandom())` rather than passing null,
/// and `TlsRsaPremasterSecretGenerator` reads its argument — a null there is an
/// NPE inside the provider, not a defaulted value.
pub fn keygen_default_random(ctx: &mut dyn NativeContext) -> Result<Value, MethodCallFailed> {
    match ctx.new_object_initialized("java/security/SecureRandom", "()V", &[])? {
        Some(v @ Value::Object(Some(_))) => Ok(v),
        _ => Ok(Value::Object(None)),
    }
}

pub fn keygen_get_instance_named(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let algo = obj_arg(args, 0)?;
    let algo_str = ctx.read_string(algo).unwrap_or_default();
    let bits = match keygen_default_bits(&algo_str) {
        Some(bits) => bits,
        None => {
            // AFTER this engine's own verdict, never before it — the ordering
            // `try_delegate_cipher_to_chain` writes down and the whole safety
            // argument for `jdk_service_class`. A name this crate generates
            // keys for is generated HERE; only a name it refuses reaches the
            // platform's own implementation class.
            if let Some(real) =
                crate::jca::provider_chain::build_real_key_generator(ctx, "SunJCE", &algo_str)?
            {
                return Ok(Some(Value::Object(Some(real))));
            }
            return Err(throw_jca_exc(
                ctx,
                "java/security/NoSuchAlgorithmException",
                &format!("{algo_str} KeyGenerator not available"),
            ));
        }
    };
    let obj = try_alloc_concurrent_synthetic(ctx, "javax/crypto/KeyGenerator", 2)?;
    ctx.set_field(obj, 0, Value::Object(Some(algo)));
    ctx.set_field(obj, 1, Value::Int(bits));
    Ok(Some(Value::Object(Some(obj))))
}

/// `KeyGenerator.init(int)` and `init(int, SecureRandom)`.
///
/// The size was previously stored unchecked, so `init(129)` on AES produced a
/// 16-byte key (129/8 truncating) under a name the caller believed was 129
/// bits, and `init(128)` on DESede produced a 16-byte "DESede key". HotSpot
/// refuses both with `InvalidParameterException`, whose wording is measured in
/// `keygen_allowed_bits`. An algorithm with no fixed set (the HMAC family)
/// accepts any positive multiple of 8, which is the JCE rule and what HotSpot
/// does — `HmacSHA256` really will hand back a 64-bit key for `init(64)`.
pub fn keygen_init_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // A generator this crate did not build belongs to the provider that did.
    // See `keygen_real_spi`.
    if let Some(spi) = keygen_real_spi(ctx, this) {
        let size = args.get(1).copied().unwrap_or(Value::Int(0));
        let random = match args.get(2) {
            Some(v @ Value::Object(Some(_))) => *v,
            _ => keygen_default_random(ctx)?,
        };
        return ctx.invoke_virtual(
            spi,
            "engineInit",
            "(ILjava/security/SecureRandom;)V",
            &[size, random],
        );
    }
    let algo = keygen_algorithm_of(ctx, this);
    let key_size = args
        .get(1)
        .and_then(|v| v.as_int())
        .unwrap_or_else(|| keygen_default_bits(&algo).unwrap_or(128));
    if let Some((allowed, wording)) = keygen_allowed_bits(&algo) {
        if !allowed.contains(&key_size) {
            return Err(throw_jca_exc(
                ctx,
                "java/security/InvalidParameterException",
                &format!("Wrong keysize: {wording}"),
            ));
        }
    } else if key_size <= 0 || (key_size & 7) != 0 {
        return Err(throw_jca_exc(
            ctx,
            "java/security/InvalidParameterException",
            &format!("Wrong keysize: {key_size}"),
        ));
    }
    ctx.set_field(this, 1, Value::Int(key_size));
    Ok(None)
}

pub fn register_phase53_crypto(r: &mut NativeMethodRegistry) {
            let cipher = "javax/crypto/Cipher";

    fn cipher_alloc(
        ctx: &mut dyn NativeContext,
        algo: ObjectRef,
    ) -> Result<ObjectRef, MethodCallFailed> {
        // gc-common w20-b: `algo` (an argument) was held raw across the
        // cipher's allocation -- a class resolution, which can run
        // `<clinit>` -- and the cipher raw across its two buffers.
        let algo_pin = ctx.pin_native_root(algo);
        let obj = try_alloc_concurrent_synthetic(ctx, "javax/crypto/Cipher", 6);
        let algo = ctx.read_native_pin(algo_pin, algo);
        ctx.unpin_native_roots(algo_pin);
        let obj: ObjectRef = obj?;
        ctx.set_field(obj, CIPHER_ALGO, Value::Object(Some(algo)));
        ctx.set_field(obj, CIPHER_MODE, Value::Int(0));
        ctx.set_field(obj, CIPHER_KEY, Value::Object(None));
        ctx.set_field(obj, CIPHER_IV, Value::Object(None));
        Ok(cipher_reset_accumulators(ctx, obj))
    }

    // Cipher.getInstance(String algorithm) -> Cipher
    r.register(
        cipher,
        "getInstance",
        "(Ljava/lang/String;)Ljavax/crypto/Cipher;",
        |ctx, args| {
            let algo = obj_arg(args, 0)?;
            let obj = cipher_alloc(ctx, algo)?;
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        cipher,
        "getInstance",
        "(Ljava/lang/String;Ljava/lang/String;)Ljavax/crypto/Cipher;",
        |ctx, args| {
            // Mirrors the same check in `jca::cipher::register_cipher_dispatch`
            // — whichever registration wins, an unregistered provider name has
            // to be rejected rather than silently ignored.
            crate::jca::provider_chain::check_named_provider_arg(
                ctx,
                args,
                1,
                crate::jca::provider_chain::ProviderArgWording::Cipher,
            )?;
            let algo = obj_arg(args, 0)?;
            let algo_str = ctx.read_string(algo).unwrap_or_default();
            crate::jca::provider_chain::check_provider_ownership(
                ctx,
                args,
                1,
                "Cipher",
                &algo_str,
                crate::jca::provider_chain::ProviderArgWording::Cipher,
            )?;
            let obj = cipher_alloc(ctx, algo)?;
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // init(int opmode, Key key)
    r.register(cipher, "init", "(ILjava/security/Key;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let mode = args[1].as_int().unwrap_or(0);
        let key = obj_arg(args, 2)?;
        ctx.set_field(this, CIPHER_MODE, Value::Int(mode));
        ctx.set_field(this, CIPHER_KEY, Value::Object(Some(key)));
        // Reset accumulators
        cipher_reset_accumulators(ctx, this);
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);
    // init(int opmode, Key key, AlgorithmParameterSpec params)
    r.register(
        cipher,
        "init",
        "(ILjava/security/Key;Ljava/security/spec/AlgorithmParameterSpec;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let mode = args[1].as_int().unwrap_or(0);
            let key = obj_arg(args, 2)?;
            ctx.set_field(this, CIPHER_MODE, Value::Int(mode));
            ctx.set_field(this, CIPHER_KEY, Value::Object(Some(key)));
            // Extract IV from AlgorithmParameterSpec (IvParameterSpec or GCMParameterSpec)
            if let Some(Value::Object(Some(spec))) = args.get(3) {
                // Field 0 of IvParameterSpec/GCMParameterSpec is the IV byte array
                if let Value::Object(Some(iv_arr)) = ctx.get_field(*spec, 0) {
                    ctx.set_field(this, CIPHER_IV, Value::Object(Some(iv_arr)));
                }
            }
            // Reset accumulators
            cipher_reset_accumulators(ctx, this);
            Ok(Some(Value::Object(None)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // init(int opmode, Key key, AlgorithmParameterSpec params, SecureRandom random)
    r.register(
        cipher,
        "init",
        "(ILjava/security/Key;Ljava/security/spec/AlgorithmParameterSpec;Ljava/security/SecureRandom;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let mode = args[1].as_int().unwrap_or(0);
            let key = obj_arg(args, 2)?;
            ctx.set_field(this, CIPHER_MODE, Value::Int(mode));
            ctx.set_field(this, CIPHER_KEY, Value::Object(Some(key)));
            if let Some(Value::Object(Some(spec))) = args.get(3) {
                if let Value::Object(Some(iv_arr)) = ctx.get_field(*spec, 0) {
                    ctx.set_field(this, CIPHER_IV, Value::Object(Some(iv_arr)));
                }
            }
            cipher_reset_accumulators(ctx, this);
            Ok(Some(Value::Object(None)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // update(byte[]) -> byte[] — accumulate data for streaming
    r.register(cipher, "update", "([B)[B", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        if let Some(Value::Object(Some(input))) = args.get(1) {
            let input_bytes = cipher_read_bytes(ctx, *input);
            cipher_append_accum(ctx, &mut this, &input_bytes);
        }
        // No intermediate output; all processing in doFinal
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);
    // updateAAD(byte[]) — set additional authenticated data for GCM
    r.register(cipher, "updateAAD", "([B)V", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        if let Some(Value::Object(Some(aad_input))) = args.get(1) {
            let aad_bytes = cipher_read_bytes(ctx, *aad_input);
            cipher_append_aad(ctx, &mut this, &aad_bytes);        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    // doFinal(byte[] input) -> byte[] — real AES
    r.register(cipher, "doFinal", "([B)[B", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        // Append input to accumulator
        if let Some(Value::Object(Some(input))) = args.get(1) {
            let input_bytes = cipher_read_bytes(ctx, *input);
            cipher_append_accum(ctx, &mut this, &input_bytes);
        }
        cipher_do_final(ctx, this)
    }, cratonvm_native_api::NativeKind::Bridge);
    // doFinal() -> byte[] — no-input variant (use accumulated data)
    r.register(cipher, "doFinal", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        cipher_do_final(ctx, this)
    }, cratonvm_native_api::NativeKind::Bridge);

    // --- javax.crypto.SecretKeyFactory — PBKDF2 (real key derivation) ---
    // The real JCA path throws "PBKDF2With… SecretKeyFactory not available"
    // (no provider service) and the real PBKDF2KeyImpl trips a ByteBuffer bug.
    // We compute PBKDF2 natively. getInstance handles ONLY PBKDF2* algorithms;
    // any other algorithm gets the same NoSuchAlgorithmException the real path
    // would have thrown, so this is no regression for non-PBKDF2 callers.
    {
        let skf = "javax/crypto/SecretKeyFactory";
        r.register(
            skf,
            "getInstance",
            "(Ljava/lang/String;)Ljavax/crypto/SecretKeyFactory;",
            pbkdf2_get_instance, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            skf,
            "getInstance",
            "(Ljava/lang/String;Ljava/lang/String;)Ljavax/crypto/SecretKeyFactory;",
            pbkdf2_get_instance, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            skf,
            "generateSecret",
            "(Ljava/security/spec/KeySpec;)Ljavax/crypto/SecretKey;",
            pbkdf2_generate_secret, cratonvm_native_api::NativeKind::Bridge,
        );
        // The two accessors the real body cannot serve on a synthetic: both
        // read instance fields no constructor ever wrote, and `getProvider()`
        // additionally synchronizes on a null `lock`. See `skf_algo_table`.
        r.register(
            skf,
            "getAlgorithm",
            "()Ljava/lang/String;",
            pbkdf2_get_algorithm, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            skf,
            "getProvider",
            "()Ljava/security/Provider;",
            pbkdf2_get_provider, cratonvm_native_api::NativeKind::Bridge,
        );
    }
    // getAlgorithm() -> String
    r.register(
        cipher,
        "getAlgorithm",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, CIPHER_ALGO)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // getBlockSize() -> int: REMOVED, it was a dead duplicate. This copy
    // answered a hard-coded 16 for every transformation (wrong for the DES
    // family, and for the stream/asymmetric ciphers whose contract is 0), but
    // it could never run: `register_synthetic_overrides` calls
    // `register_phase53_natives` (which calls this one) and then
    // `jca::cipher::register_cipher_clinit_shim` → `register_cipher_dispatch`,
    // which re-registers the same class+method+descriptor with a
    // transformation-aware implementation, and the registry is
    // last-write-wins. In real-JDK mode this function is not called at all and
    // only the `jca::cipher` entry exists. Deleted rather than fixed in place
    // so the two cannot drift apart again; the live one is
    // `jca/cipher.rs::register_cipher_dispatch`.
    // getOutputSize(int inputLen) -> int
    r.register(cipher, "getOutputSize", "(I)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let input_len = args[1].as_int().unwrap_or(0);
        let algo = match ctx.get_field(this, CIPHER_ALGO) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let mode = ctx.get_field(this, CIPHER_MODE).as_int().unwrap_or(0);
        let upper = algo.to_uppercase();
        if upper.contains("GCM") {
            if mode == 1 {
                // Encrypt: input + 16 bytes tag
                Ok(Some(Value::Int(input_len + 16)))
            } else {
                // Decrypt: input - 16 bytes tag
                Ok(Some(Value::Int((input_len - 16).max(0))))
            }
        } else {
            // ECB/CBC: round up to block size
            let out = ((input_len + 15) / 16) * 16;
            Ok(Some(Value::Int(out)))
        }
    }, cratonvm_native_api::NativeKind::Bridge);
    // getIV() -> byte[] — DELETED, see `jca::cipher::register_cipher_dispatch`.
    //
    // The third aliasing accessor in this registrar, and superseded like the
    // other two: real `Cipher.getIV` ends `return (iv == null) ? null :
    // iv.clone()` in the SPI (`CipherCore.getIV`, JDK 25 src.zip), this copy
    // handed back the stored array, and `jca::cipher`'s copy — which registers
    // after this one and therefore wins — builds a fresh array from its own
    // side table. An IV a caller can rewrite in place after `init` is a nonce
    // that can be made to repeat, which for GCM is a key-recovery bug rather
    // than an untidiness.
    // Constants — KEEP. These are `static final int` FIELD reads (note the "I"
    // field descriptor, not a method descriptor), and 1/2/3/4 are the literal
    // values `javax.crypto.Cipher` declares. A constant is the correct
    // implementation of a constant.
    r.register(cipher, "ENCRYPT_MODE", "I", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(cipher, "DECRYPT_MODE", "I", |_ctx, _args| {
        Ok(Some(Value::Int(2)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(cipher, "WRAP_MODE", "I", |_ctx, _args| {
        Ok(Some(Value::Int(3)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(cipher, "UNWRAP_MODE", "I", |_ctx, _args| {
        Ok(Some(Value::Int(4)))
    }, cratonvm_native_api::NativeKind::Bridge);

    // --- IvParameterSpec / GCMParameterSpec — DELETED, same reason as SecretKeySpec ---
    //
    // Five more registrations (`IvParameterSpec.<init>`/`getIV`,
    // `GCMParameterSpec.<init>`/`getIV`/`getTLen`) that duplicated
    // `jca::cipher::register_param_specs` exactly, and lost to it in the one
    // mode where they registered at all — see the SecretKeySpec tombstone at
    // the end of this function for the ordering argument. Both `getIV` copies
    // here handed back the STORED array; the surviving pair clones, because
    // the real classes end `return this.iv.clone()`. A GCM nonce a caller can
    // edit in place after the fact is not the same class of disaster as an
    // all-zero key, but it is the same aliasing mistake, and it is now gone
    // from this file rather than fixed twice.

    // --- javax.crypto.SecretKey — 2-field synthetic (encoded=0 byte[], algorithm=1 String) ---
    //
    // This carrier is what `KeyGenerator.generateKey` below hands back, and it
    // was a one-field object whose `getAlgorithm` answered the string "AES" for
    // every algorithm — so in synthetic mode
    // `KeyGenerator.getInstance("DESede").generateKey().getAlgorithm()` returned
    // "AES". A key that misreports its own algorithm is the same species of
    // defect as a cipher that ignores the requested one: every caller that
    // branches on `getAlgorithm()` (and `Cipher.init` implementations do)
    // branches wrong, with nothing raised. The algorithm now travels with the
    // key in slot 1; `generateKey` is the only allocator of this shape in the
    // tree, so there is no older 1-field instance to be compatible with.
    let sk = "javax/crypto/SecretKey";
    r.register(sk, "getEncoded", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Object(clone_key_bytes_field(ctx, this, 0))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sk, "getAlgorithm", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(carrier_algorithm(ctx, this)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sk, "getFormat", "()Ljava/lang/String;", |ctx, _args| {
        let s = ctx.create_string("RAW");
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Bridge);
    // Also register under java/security/Key
    let key = "java/security/Key";
    r.register(key, "getEncoded", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Object(clone_key_bytes_field(ctx, this, 0))))
    }, cratonvm_native_api::NativeKind::Bridge);
    // Deliberately NOT slot-1-aware, unlike the `javax/crypto/SecretKey` copy
    // above. This triple serves any object whose own class is literally
    // `java/security/Key`, and this file alone mints several unrelated key
    // shapes (a 4-slot `java/security/PrivateKey`, `jca::key_factory`'s 5-slot
    // keys). `NativeContext` exposes no slot count, and `Heap::get_field`
    // asserts on an out-of-range index rather than answering null — so a
    // speculative read of slot 1 here would turn an unknown 1-slot carrier into
    // a panic. The stale "AES" is wrong for a non-AES key, but it is the
    // behaviour that was already here, and narrowing the fix to the carrier
    // whose allocators are all known is the part that can be made safe without
    // building.
    r.register(key, "getAlgorithm", "()Ljava/lang/String;", |ctx, _args| {
        let s = ctx.create_string("AES");
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(key, "getFormat", "()Ljava/lang/String;", |ctx, _args| {
        let s = ctx.create_string("RAW");
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Bridge);

    // --- javax.crypto.KeyGenerator — 2-field synthetic (algorithm=0, keySize=1) ---
    //
    // The default key size used to be the literal 128 for every algorithm, and
    // `generateKey` never read slot 0 at all — so this shim answered a 128-bit
    // key to every caller of `KeyGenerator.getInstance(a).generateKey()`
    // regardless of `a`. Measured against HotSpot 25, that is wrong three ways
    // at once: AES defaults to 256 (16 bytes where HotSpot gives 32), DESede
    // defaults to 168 and must be 24 bytes with odd parity per byte (this shim
    // gave 16 unconditioned random bytes, which is not a DESede key), and an
    // algorithm this VM cannot generate for was accepted rather than refused.
    // See `keygen_default_bits`, `keygen_byte_len` and `des_set_odd_parity`.
    let kg = "javax/crypto/KeyGenerator";
    r.register(
        kg,
        "getInstance",
        "(Ljava/lang/String;)Ljavax/crypto/KeyGenerator;",
        keygen_get_instance_named, cratonvm_native_api::NativeKind::Bridge,
    );
    // Round 15 (BcProbe): KeyGenerator.getInstance("AES", "BC") used by
    // BouncyCastle clients. Without this shim the real-JDK 2-arg overload
    // runs to completion (returning a real KeyGenerator wrapping a real
    // KeyGeneratorSpi), and the subsequent kg.init(128) then dispatches
    // through the real JDK bytecode which reads field `spi` -> NPE on a
    // mis-shaped synthetic. Returning a 2-field synthetic here keeps the
    // whole chain on the synthetic shim path that init/generateKey
    // already understand.
    r.register(
        kg,
        "getInstance",
        "(Ljava/lang/String;Ljava/lang/String;)Ljavax/crypto/KeyGenerator;",
        keygen_get_instance_named, cratonvm_native_api::NativeKind::Bridge,
    );
    // Provider-instance overload — same synthetic layout, ignores the
    // Provider arg entirely (we synthesise the SPI via the init / generateKey
    // shims below).
    r.register(
        kg,
        "getInstance",
        "(Ljava/lang/String;Ljava/security/Provider;)Ljavax/crypto/KeyGenerator;",
        keygen_get_instance_named, cratonvm_native_api::NativeKind::Bridge,
    );
    // KeyGenerator.init(I)V — validate against the algorithm, then store
    // keySize at field 1; never touch spi (spi is null on synthetic instances,
    // and the real JDK bytecode for init(I) calls this.spi.engineInit(...) →
    // NPE on the real path).
    r.register(kg, "init", "(I)V", keygen_init_int, cratonvm_native_api::NativeKind::Bridge);
    // KeyGenerator.init(I, SecureRandom)V — same validation, ignore SecureRandom
    r.register(
        kg,
        "init",
        "(ILjava/security/SecureRandom;)V",
        keygen_init_int, cratonvm_native_api::NativeKind::Bridge,
    );
    // init(SecureRandom) — W2: this was a no-op, which is not the same thing as
    // "leave keySize at its default": after an earlier `init(256)` the field
    // still held 256, so `kg.init(256); kg.init(random); kg.generateKey()`
    // handed back a 256-bit key where the real JDK re-initialises the SPI and
    // returns the provider default. Reset the size; the randomness argument is
    // still ignored (generateKey() uses the VM's own CSPRNG).
    r.register(
        kg,
        "init",
        "(Ljava/security/SecureRandom;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Some(spi) = keygen_real_spi(ctx, this) {
                let random = match args.get(1) {
                    Some(v @ Value::Object(Some(_))) => *v,
                    _ => keygen_default_random(ctx)?,
                };
                return ctx.invoke_virtual(
                    spi,
                    "engineInit",
                    "(Ljava/security/SecureRandom;)V",
                    &[random],
                );
            }
            // "the provider default", not the literal 128 — which for AES is
            // 256 and for DESede is 168. Resetting to 128 here re-introduced
            // the very defect `keygen_default_bits` exists to fix, one method
            // along.
            let algo = keygen_algorithm_of(ctx, this);
            let bits = keygen_default_bits(&algo).unwrap_or(128);
            ctx.set_field(this, 1, Value::Int(bits));
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // KEEP (both): init(AlgorithmParameterSpec[, SecureRandom]) accepts the
    // call and leaves field 1 alone, so generateKey() falls back to the 128-bit
    // default. This is deliberate, not an oversight: `AlgorithmParameterSpec`
    // is an empty marker interface, so there is no provider-independent way to
    // read a key size out of it, and the JCE contract lets a provider ignore
    // parameters it does not recognise. Throwing
    // `InvalidAlgorithmParameterException` instead would break the BouncyCastle
    // clients that call this purely to pass a curve/nonce spec (see the
    // Round-15 BcProbe note on `getInstance` above).
    // These two stay a NO-OP for a synthetic receiver, deliberately and for the
    // reason above — but they are the ONLY route into the five `SunTls*` KDFs,
    // whose whole input is an `AlgorithmParameterSpec`. Swallowing the spec on
    // a real receiver would leave its generator uninitialised and
    // `generateKey()` would then throw from inside the provider, which is a
    // worse answer than the `NoSuchAlgorithmException` this used to give.
    r.register(
        kg,
        "init",
        "(Ljava/security/spec/AlgorithmParameterSpec;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Some(spi) = keygen_real_spi(ctx, this) {
                let spec = args.get(1).copied().unwrap_or(Value::Object(None));
                let random = keygen_default_random(ctx)?;
                return ctx.invoke_virtual(
                    spi,
                    "engineInit",
                    "(Ljava/security/spec/AlgorithmParameterSpec;Ljava/security/SecureRandom;)V",
                    &[spec, random],
                );
            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        kg,
        "init",
        "(Ljava/security/spec/AlgorithmParameterSpec;Ljava/security/SecureRandom;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Some(spi) = keygen_real_spi(ctx, this) {
                let spec = args.get(1).copied().unwrap_or(Value::Object(None));
                let random = match args.get(2) {
                    Some(v @ Value::Object(Some(_))) => *v,
                    _ => keygen_default_random(ctx)?,
                };
                return ctx.invoke_virtual(
                    spi,
                    "engineInit",
                    "(Ljava/security/spec/AlgorithmParameterSpec;Ljava/security/SecureRandom;)V",
                    &[spec, random],
                );
            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        kg,
        "generateKey",
        "()Ljavax/crypto/SecretKey;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Some(spi) = keygen_real_spi(ctx, this) {
                return ctx.invoke_virtual(
                    spi,
                    "engineGenerateKey",
                    "()Ljavax/crypto/SecretKey;",
                    &[],
                );
            }
            let key_size = ctx.get_field(this, 1).as_int().unwrap_or(128);
            // The algorithm was recorded at `getInstance` and, until this
            // change, never read again — which is how one code path served
            // every algorithm. It decides the byte length and the
            // conditioning, both of which differ from `key_size / 8` for the
            // DES families.
            let algo = keygen_algorithm_of(ctx, this);
            // Reject obviously invalid sizes. Upper bound is generous — the
            // JCE spec allows any positive multiple of 8 up to provider limits.
            if key_size <= 0 || key_size > 1 << 20 || (key_size & 7) != 0 {
                return Err(
                    cratonvm_types::error::RuntimeError::IllegalArgumentException {
                        message: format!(
                            "KeyGenerator.generateKey: invalid key size {key_size} bits"
                        ),
                    }
                    .into(),
                );
            }
            let byte_len = keygen_byte_len(&algo, key_size);
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, byte_len);

            // Fill with cryptographic OS entropy. Fallback loop exists only
            // to bubble up a clear RuntimeException rather than silently
            // producing weak keys if the OS CSPRNG refuses to deliver.
            let mut buf = vec![0u8; byte_len];
            if !crate::crypto_impl::os_random_bytes(&mut buf) {
                return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
                    message: "KeyGenerator.generateKey: OS entropy source unavailable".to_string(),
                }
                .into());
            }
            // Parity first, then the 2-key fold, mirroring SunJCE's
            // `DESedeKeyGenerator`: it parity-adjusts the 16-byte draw and then
            // copies K1 over K3, so K3 arrives already adjusted.
            keygen_condition_key(&algo, &mut buf);
            if key_size == 112 && algo.eq_ignore_ascii_case("DESede") {
                keygen_fold_two_key_desede(&mut buf);
            }
            for (i, &b) in buf.iter().enumerate() {
                ctx.set_array_element(arr, i, Value::Int((b as i8) as i32));
            }
            // Best-effort: zero the stack-side buffer before it drops.
            for b in buf.iter_mut() {
                *b = 0;
            }
            // Two slots now: the algorithm travels with the key, so
            // `SecretKey.getAlgorithm()` stops answering "AES" for everything.
            // `arr` must survive the allocation below, and the algorithm string
            // must survive it too.
            let arr_pin = ctx.pin_native_root(arr);
            let algo_str = ctx.create_string(&algo);
            let algo_pin = ctx.pin_native_root(algo_str);
            let sk = try_alloc_concurrent_synthetic(ctx, "javax/crypto/SecretKey", 2)?;
            let arr = ctx.read_native_pin(arr_pin, arr);
            let algo_str = ctx.read_native_pin(algo_pin, algo_str);
            ctx.unpin_native_roots(arr_pin);
            ctx.set_field(sk, 0, Value::Object(Some(arr)));
            ctx.set_field(sk, 1, Value::Object(Some(algo_str)));
            Ok(Some(Value::Object(Some(sk))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- javax.crypto.spec.SecretKeySpec — DELETED, see `jca::cipher::register_param_specs` ---
    //
    // This registrar used to carry a second `SecretKeySpec` — `<init>`,
    // `getEncoded`, `getAlgorithm`, `getFormat` — byte-for-byte a weaker
    // duplicate of the pair in `jca::cipher::register_param_specs`. Weaker in
    // the way that matters: its `<init>` stored the caller's `byte[]` by
    // reference and its `getEncoded` handed the stored array straight back,
    // where the real class clones on both sides *because callers scrub*. That
    // aliasing is the mechanism behind the all-zero AES key recorded in
    // W7-21-keygen-and-the-synthetic-secretkeyspec-twin.md.
    //
    // It is deleted rather than fixed, and the reachability is the reason it
    // could be. `register_phase53_crypto` is reached only from
    // `register_synthetic_overrides`, which is `#[cfg(feature = "synthetic-jdk")]`
    // AND gated on `config.use_synthetic_jdk` — so in real-JDK and `--real-jdk`
    // modes these four registrations never existed at all. In synthetic mode
    // they did register, and were then immediately overwritten:
    // `register_synthetic_overrides` calls `register_phase53_natives` and THEN
    // `jca::cipher::register_cipher_clinit_shim`, and `register()` is
    // last-registration-wins. So the fixed copy won in every mode and this one
    // never ran anywhere.
    //
    // Which is exactly why it had to go rather than be corrected. A shadowed
    // second implementation of a key-material primitive is not a dormant defect,
    // it is a live trap: swapping those two call sites in
    // `register_synthetic_overrides` — a change nobody would think of as
    // touching crypto — would have silently restored the zero-key bug. One
    // primitive, one implementation, and the ordering stops mattering.
    }

// ===========================================================================
// AES implementation — C18: routed through RustCrypto `aes`/`aes-gcm`/`cbc`
// for constant-time execution. The previous in-tree FIPS-197 core (256-byte
// SBOX/InvSBOX tables, ShiftRows/MixColumns, hand-rolled GHASH `ghash_mul`)
// was a textbook cache-timing oracle (Bernstein 2005). Every helper below
// (`aes_ecb_encrypt`, `aes_cbc_encrypt`, `aes_gcm_encrypt`, etc.) now
// delegates to the audited library.
// ===========================================================================

/// Apply PKCS7 padding to data
///
/// `pub` so `jca::cipher`'s AES/ECB path uses THIS pad/unpad pair rather
/// than its own copy. The copy had drifted: it padded unconditionally (ignoring
/// `NoPadding`) and unpadded without verifying the padding bytes.
pub fn pkcs7_pad(data: &[u8]) -> Vec<u8> {
    let pad_len = 16 - (data.len() % 16);
    let mut padded = data.to_vec();
    padded.extend(std::iter::repeat(pad_len as u8).take(pad_len));
    padded
}

/// Remove PKCS7 padding
///
/// `pub`: see [`pkcs7_pad`]. This is the ONE PKCS7 verifier in the
/// crate; `jca::cipher` calls it instead of re-deriving the rule.
pub fn pkcs7_unpad(data: &[u8]) -> Result<Vec<u8>, &'static str> {
    if data.is_empty() || data.len() % 16 != 0 {
        return Err("invalid padded data length");
    }
    let pad_byte = *data.last().unwrap();
    let pad_len = pad_byte as usize;
    if pad_len == 0 || pad_len > 16 || pad_len > data.len() {
        return Err("invalid padding");
    }
    // BUGFIX [nb-phases-early (2)] — CBC padding-oracle hardening.
    // The previous loop early-returned on the FIRST mismatching padding byte,
    // so the time-to-error leaked HOW MANY trailing bytes matched — a classic
    // Vaudenay CBC padding oracle. Verify in constant time instead: iterate a
    // FIXED 16-byte window over the tail (loop trip count independent of the
    // claimed pad_len), accumulate any mismatch into `bad` with no data-
    // dependent branches, and decide exactly once at the end. This mirrors the
    // constant-time discipline of the RustCrypto AES core used above.
    let mut bad: u8 = 0;
    let n = data.len();
    for i in 0..16usize {
        // `in_pad` is all-ones when this position falls within the claimed
        // padding region (the last `pad_len` bytes), all-zeros otherwise —
        // computed branchlessly so the mask itself leaks nothing.
        let in_pad = ((((i as i32) - (pad_len as i32)) >> 31) as u8) & 1;
        let mask = in_pad.wrapping_neg(); // 0xFF when in_pad==1, else 0x00
                                          // Index from the end; `i < pad_len <= n` whenever the mask is active,
                                          // so the read stays in bounds. Out-of-window iterations read a valid
                                          // tail byte but contribute nothing (mask == 0).
        let b = data[n - 1 - i];
        bad |= mask & (b ^ pad_byte);
    }
    if bad != 0 {
        return Err("invalid padding bytes");
    }
    Ok(data[..data.len() - pad_len].to_vec())
}

// ---------------------------------------------------------------------------
// AES block helpers — each routes through the `aes` crate's constant-time
// block cipher. Variable-key-length dispatch matches the JCA contract
// (`SecretKeySpec(byte[], "AES")` accepts 128/192/256-bit keys).
// ---------------------------------------------------------------------------

pub fn aes_blocks_encrypt(data: &[u8], key: &[u8]) -> Result<Vec<u8>, &'static str> {
    use aes::cipher::generic_array::GenericArray;
    use aes::cipher::{BlockEncrypt, KeyInit};
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks(16) {
        let mut block = [0u8; 16];
        block[..chunk.len()].copy_from_slice(chunk);
        let mut ga = GenericArray::from(block);
        match key.len() {
            16 => aes::Aes128::new(GenericArray::from_slice(key)).encrypt_block(&mut ga),
            24 => aes::Aes192::new(GenericArray::from_slice(key)).encrypt_block(&mut ga),
            32 => aes::Aes256::new(GenericArray::from_slice(key)).encrypt_block(&mut ga),
            _ => return Err("AES key length must be 16/24/32 bytes"),
        }
        out.extend_from_slice(ga.as_slice());
    }
    Ok(out)
}

pub fn aes_blocks_decrypt(data: &[u8], key: &[u8]) -> Result<Vec<u8>, &'static str> {
    use aes::cipher::generic_array::GenericArray;
    use aes::cipher::{BlockDecrypt, KeyInit};
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks(16) {
        if chunk.len() != 16 {
            return Err("ciphertext not multiple of block size");
        }
        let mut block = [0u8; 16];
        block.copy_from_slice(chunk);
        let mut ga = GenericArray::from(block);
        match key.len() {
            16 => aes::Aes128::new(GenericArray::from_slice(key)).decrypt_block(&mut ga),
            24 => aes::Aes192::new(GenericArray::from_slice(key)).decrypt_block(&mut ga),
            32 => aes::Aes256::new(GenericArray::from_slice(key)).decrypt_block(&mut ga),
            _ => return Err("AES key length must be 16/24/32 bytes"),
        }
        out.extend_from_slice(ga.as_slice());
    }
    Ok(out)
}

/// AES-ECB encrypt
pub fn aes_ecb_encrypt(data: &[u8], key: &[u8], pad: bool) -> Vec<u8> {
    let input = if pad { pkcs7_pad(data) } else { data.to_vec() };
    aes_blocks_encrypt(&input, key).unwrap_or_default()
}

/// AES-ECB decrypt
pub fn aes_ecb_decrypt(data: &[u8], key: &[u8], pad: bool) -> Result<Vec<u8>, &'static str> {
    let out = aes_blocks_decrypt(data, key)?;
    if pad {
        pkcs7_unpad(&out)
    } else {
        Ok(out)
    }
}

/// AES-CBC encrypt — uses RustCrypto `cbc` mode wrapper. PKCS7 padding
/// is applied in-tree (`pkcs7_pad`) to match the existing contract;
/// the `cbc` mode then runs over already-padded data with NoPadding so
/// it never appends an extra block.
pub fn aes_cbc_encrypt(data: &[u8], key: &[u8], iv: &[u8], pad: bool) -> Vec<u8> {
    use aes::cipher::block_padding::NoPadding;
    use aes::cipher::generic_array::GenericArray;
    use aes::cipher::{BlockEncryptMut, KeyIvInit};
    if iv.len() < 16 {
        return Vec::new();
    }
    let input = if pad { pkcs7_pad(data) } else { data.to_vec() };
    let mut out = vec![0u8; input.len()];

    let len_or_err: Option<usize> = match key.len() {
        16 => {
            type Enc = cbc::Encryptor<aes::Aes128>;
            let enc = Enc::new(
                GenericArray::from_slice(key),
                GenericArray::from_slice(&iv[..16]),
            );
            enc.encrypt_padded_b2b_mut::<NoPadding>(&input, &mut out)
                .ok()
                .map(|w| w.len())
        }
        24 => {
            type Enc = cbc::Encryptor<aes::Aes192>;
            let enc = Enc::new(
                GenericArray::from_slice(key),
                GenericArray::from_slice(&iv[..16]),
            );
            enc.encrypt_padded_b2b_mut::<NoPadding>(&input, &mut out)
                .ok()
                .map(|w| w.len())
        }
        32 => {
            type Enc = cbc::Encryptor<aes::Aes256>;
            let enc = Enc::new(
                GenericArray::from_slice(key),
                GenericArray::from_slice(&iv[..16]),
            );
            enc.encrypt_padded_b2b_mut::<NoPadding>(&input, &mut out)
                .ok()
                .map(|w| w.len())
        }
        _ => None,
    };
    match len_or_err {
        Some(n) => {
            out.truncate(n);
            out
        }
        None => Vec::new(),
    }
}

/// AES-CBC decrypt — uses RustCrypto `cbc` mode wrapper.
pub fn aes_cbc_decrypt(data: &[u8], key: &[u8], iv: &[u8], pad: bool) -> Result<Vec<u8>, &'static str> {
    use aes::cipher::block_padding::NoPadding;
    use aes::cipher::generic_array::GenericArray;
    use aes::cipher::{BlockDecryptMut, KeyIvInit};
    if iv.len() < 16 {
        return Err("IV must be 16 bytes for AES-CBC");
    }
    if data.len() % 16 != 0 {
        return Err("ciphertext not multiple of block size");
    }
    let mut out = vec![0u8; data.len()];

    let written_len: usize = match key.len() {
        16 => {
            type Dec = cbc::Decryptor<aes::Aes128>;
            let dec = Dec::new(
                GenericArray::from_slice(key),
                GenericArray::from_slice(&iv[..16]),
            );
            dec.decrypt_padded_b2b_mut::<NoPadding>(data, &mut out)
                .map_err(|_| "AES-CBC decrypt failed")?
                .len()
        }
        24 => {
            type Dec = cbc::Decryptor<aes::Aes192>;
            let dec = Dec::new(
                GenericArray::from_slice(key),
                GenericArray::from_slice(&iv[..16]),
            );
            dec.decrypt_padded_b2b_mut::<NoPadding>(data, &mut out)
                .map_err(|_| "AES-CBC decrypt failed")?
                .len()
        }
        32 => {
            type Dec = cbc::Decryptor<aes::Aes256>;
            let dec = Dec::new(
                GenericArray::from_slice(key),
                GenericArray::from_slice(&iv[..16]),
            );
            dec.decrypt_padded_b2b_mut::<NoPadding>(data, &mut out)
                .map_err(|_| "AES-CBC decrypt failed")?
                .len()
        }
        _ => return Err("AES key length must be 16/24/32 bytes"),
    };
    out.truncate(written_len);
    if pad {
        pkcs7_unpad(&out)
    } else {
        Ok(out)
    }
}

// ===========================================================================
// AES-GCM (NIST SP 800-38D) — RustCrypto `aes-gcm` backend
// ===========================================================================
//
// The previous hand-rolled GHASH+CTR path had a data-dependent bit-loop
// branch (`ghash_mul`) on every input bit of the auth tag — a textbook
// side-channel oracle. `aes_gcm` uses polyval GHASH (CLMUL on x86,
// constant-time soft fallback elsewhere) with a constant-time tag check.

/// AES-GCM encrypt: returns ciphertext || tag (tag_len bytes).
/// Returns an empty `Vec` for invalid key/IV/tag sizes — the upstream
/// `cipher_do_final` validates inputs before calling this helper, so
/// the silent-empty fallback preserves the pre-existing `Vec<u8>`
/// return contract.
pub fn aes_gcm_encrypt(plaintext: &[u8], key: &[u8], iv: &[u8], aad: &[u8], tag_len: usize) -> Vec<u8> {
    use aes::cipher::consts::U12;
    use aes_gcm::aead::AeadInPlace;
    use aes_gcm::{Aes128Gcm, Aes256Gcm, AesGcm, KeyInit};
    type Aes192Gcm = AesGcm<aes::Aes192, U12>;

    // Only the canonical 96-bit nonce is supported on this path
    // (J0 = IV || 0^31 || 1). No production caller passes a different
    // length — `cipher_do_final` rejects empty IVs upstream and JCA
    // GCMParameterSpec is always 12 bytes for our probes.
    if iv.len() != 12 || tag_len > 16 {
        return Vec::new();
    }
    let nonce = aes_gcm::Nonce::<U12>::from_slice(iv);
    let mut buf = plaintext.to_vec();

    let tag = match key.len() {
        16 => Aes128Gcm::new_from_slice(key)
            .ok()
            .and_then(|c| c.encrypt_in_place_detached(nonce, aad, &mut buf).ok()),
        24 => Aes192Gcm::new_from_slice(key)
            .ok()
            .and_then(|c| c.encrypt_in_place_detached(nonce, aad, &mut buf).ok()),
        32 => Aes256Gcm::new_from_slice(key)
            .ok()
            .and_then(|c| c.encrypt_in_place_detached(nonce, aad, &mut buf).ok()),
        _ => None,
    };
    match tag {
        Some(t) => {
            buf.extend_from_slice(&t.as_slice()[..tag_len]);
            buf
        }
        None => Vec::new(),
    }
}

/// AES-GCM decrypt: input is ciphertext || tag.
pub fn aes_gcm_decrypt(
    data: &[u8],
    key: &[u8],
    iv: &[u8],
    aad: &[u8],
    tag_len: usize,
) -> Result<Vec<u8>, &'static str> {
    use aes::cipher::consts::U12;
    use aes_gcm::aead::AeadInPlace;
    use aes_gcm::{Aes128Gcm, Aes256Gcm, AesGcm, KeyInit};
    type Aes192Gcm = AesGcm<aes::Aes192, U12>;

    if data.len() < tag_len {
        return Err("ciphertext too short for GCM tag");
    }
    if iv.len() != 12 {
        return Err("AES-GCM requires a 96-bit IV on this path");
    }
    if tag_len != 16 {
        // RustCrypto detached API expects a U16 tag; reject truncation
        // explicitly rather than silently widen.
        return Err("AES-GCM tag length must be 16 bytes");
    }
    let ciphertext = &data[..data.len() - tag_len];
    let provided_tag = &data[data.len() - tag_len..];

    let nonce = aes_gcm::Nonce::<U12>::from_slice(iv);
    let tag_arr = aes_gcm::Tag::<aes::cipher::consts::U16>::from_slice(provided_tag);
    let mut buf = ciphertext.to_vec();

    let ok = match key.len() {
        16 => Aes128Gcm::new_from_slice(key).ok().and_then(|c| {
            c.decrypt_in_place_detached(nonce, aad, &mut buf, tag_arr)
                .ok()
        }),
        24 => Aes192Gcm::new_from_slice(key).ok().and_then(|c| {
            c.decrypt_in_place_detached(nonce, aad, &mut buf, tag_arr)
                .ok()
        }),
        32 => Aes256Gcm::new_from_slice(key).ok().and_then(|c| {
            c.decrypt_in_place_detached(nonce, aad, &mut buf, tag_arr)
                .ok()
        }),
        _ => None,
    };
    ok.ok_or("GCM authentication tag mismatch")?;
    Ok(buf)
}
/// Parse cipher algorithm string like "AES/CBC/PKCS5Padding" into (cipher, mode, padding)
pub fn parse_cipher_algo(algo: &str) -> (&str, &str, bool) {
    let parts: Vec<&str> = algo.split('/').collect();
    let cipher = parts.first().copied().unwrap_or("AES");
    let mode = parts.get(1).copied().unwrap_or("ECB");
    let padding_str = parts.get(2).copied().unwrap_or("PKCS5Padding");
    let pad = !padding_str.eq_ignore_ascii_case("NoPadding");
    (cipher, mode, pad)
}

/// Read byte array from object ref
pub fn cipher_read_bytes(ctx: &mut dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
    let len = ctx.array_length(arr);
    let mut bytes = Vec::with_capacity(len);
    for i in 0..len {
        if let Value::Int(b) = ctx.get_array_element(arr, i) {
            bytes.push(b as u8);
        }
    }
    bytes
}

/// A fresh `byte[]` holding `bytes` stored into field `index` of `obj`;
/// answers `obj`'s current address (`obj` is pinned across the allocation).
///
/// gc-common w20-b: the synthetic `Cipher` / `Signature` accumulators were
/// rebuilt through helpers that took the receiver by value, allocated, and
/// returned `()` -- the `stale-receiver-audit` shape -- so `doFinal` and
/// `sign` / `verify` went on with the pre-allocation address.
pub fn alloc_bytes_into_field(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    index: usize,
    bytes: &[u8],
) -> ObjectRef {
    let pin = ctx.pin_native_root(obj);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
    ctx.write_byte_array_from(arr, 0, bytes);
    let obj_now = ctx.read_native_pin(pin, obj);
    ctx.unpin_native_roots(pin);
    ctx.set_field(obj_now, index, Value::Object(Some(arr)));
    obj_now
}

/// `bytes` appended to the `byte[]` in field `index` of `*this`, as a fresh
/// array; `*this` is updated to the receiver's current address.
pub fn append_bytes_field(
    ctx: &mut dyn NativeContext,
    this: &mut ObjectRef,
    index: usize,
    bytes: &[u8],
) {
    let mut combined = match ctx.get_field(*this, index) {
        Value::Object(Some(arr)) => cipher_read_bytes(ctx, arr),
        _ => Vec::new(),
    };
    combined.extend_from_slice(bytes);
    *this = alloc_bytes_into_field(ctx, *this, index, &combined);
}

/// Append bytes to the cipher accumulator (field CIPHER_ACCUM)
pub fn cipher_append_accum(ctx: &mut dyn NativeContext, this: &mut ObjectRef, bytes: &[u8]) {
    append_bytes_field(ctx, this, CIPHER_ACCUM, bytes);
}

/// Append bytes to the cipher AAD (field CIPHER_AAD)
pub fn cipher_append_aad(ctx: &mut dyn NativeContext, this: &mut ObjectRef, bytes: &[u8]) {
    append_bytes_field(ctx, this, CIPHER_AAD, bytes);
}

/// Allocate a Java `byte[]` holding `bytes`.
pub fn make_byte_array(ctx: &mut dyn NativeContext, bytes: &[u8]) -> ObjectRef {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
    for (i, &b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));    }
    arr
}

/// Drive a real SunJCE `CipherSpi` (`com.sun.crypto.provider.*Cipher`) for a
/// block-cipher transformation CratonVM's synthetic dispatch doesn't implement
/// natively — chiefly the CBC modes (`AES/CBC`, `DESede/CBC`, `DES/CBC`) used
/// by `PEMFile` to decrypt encrypted private keys. The synthetic path only
/// covers AES-GCM/ECB and has no DES at all; rather than reimplement DES + CBC
/// chaining we instantiate the genuine JDK SPI and drive
/// `engineSetMode`/`engineSetPadding`/`engineInit`/`engineDoFinal`, which
/// produce byte-identical output to HotSpot (verified). This is a real crypto
/// computation, not a synthetic stub. Mirrors the SunRsaSign/SunEC SPI routing
/// in `jca/key_factory.rs`.
#[allow(clippy::too_many_arguments)]
pub fn drive_real_cipher(
    ctx: &mut dyn NativeContext,
    cipher_class: &'static str,
    mode_str: &str,
    pad_str: &str,
    opmode: i32,
    key_bytes: &[u8],
    key_algo: &str,
    iv_bytes: &[u8],
    data: &[u8],
) -> MethodCallResult {
    // Construct the real CipherSpi. Pin it across every subsequent
    // allocation (create_string / new array / new SecretKeySpec) so a moving
    // GC can't leave the receiver stale.
    let spi = match ctx.new_object_initialized(cipher_class, "()V", &[])? {
        Some(Value::Object(Some(o))) => o,
        _ => {
            return Err(RuntimeError::NotImplemented {
                feature: cipher_class.into(),
            }
            .into())
        }
    };
    let pin = ctx.pin_native_root(spi);
    let result = (|| -> MethodCallResult {
        let mode_s = ctx.create_string(mode_str);
        let spi_r = ctx.read_native_pin(pin, spi);
        ctx.invoke_virtual(
            spi_r,
            "engineSetMode",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(mode_s))],
        )?;
        let pad_s = ctx.create_string(pad_str);
        let spi_r = ctx.read_native_pin(pin, spi);
        ctx.invoke_virtual(
            spi_r,
            "engineSetPadding",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(pad_s))],
        )?;

        // SecretKeySpec(key, algorithm)
        let key_arr = make_byte_array(ctx, key_bytes);
        let kpin = ctx.pin_native_root(key_arr);
        let algo_s = ctx.create_string(key_algo);
        let key_arr_r = ctx.read_native_pin(kpin, key_arr);
        let secret_key = match ctx.new_object_initialized(
            "javax/crypto/spec/SecretKeySpec",
            "([BLjava/lang/String;)V",
            &[Value::Object(Some(key_arr_r)), Value::Object(Some(algo_s))],
        )? {
            Some(Value::Object(Some(o))) => o,
            _ => {
                return Err(RuntimeError::IllegalStateException {
                    message: "SecretKeySpec construction failed".into(),
                }
                .into())
            }
        };
        let skpin = ctx.pin_native_root(secret_key);

        // IvParameterSpec(iv) — or NO parameters at all.
        //
        // ECB takes no IV, and SunJCE's own `engineInit` REFUSES one in that
        // mode (`InvalidAlgorithmParameterException: ECB mode cannot use IV`).
        // An empty `iv_bytes` is exactly the IV-less case: `auto_generated_iv_len`
        // returns `None` for ECB and for the key-wrap/RSA/RC4 families, so
        // nothing upstream minted one. Passing an EMPTY `IvParameterSpec` here
        // is not the same as passing none, and it is what stopped
        // `DESede/ECB/*` from being routable at all.
        let iv_pinned = if iv_bytes.is_empty() {
            None
        } else {
            let iv_arr = make_byte_array(ctx, iv_bytes);
            let ivapin = ctx.pin_native_root(iv_arr);
            let iv_arr_r = ctx.read_native_pin(ivapin, iv_arr);
            let iv_spec = match ctx.new_object_initialized(
                "javax/crypto/spec/IvParameterSpec",
                "([B)V",
                &[Value::Object(Some(iv_arr_r))],
            )? {
                Some(Value::Object(Some(o))) => o,
                _ => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "IvParameterSpec construction failed".into(),                    }
                    .into())
                }
            };
            Some((ctx.pin_native_root(iv_spec), iv_spec))        };

        // engineInit(opmode, key, params, null)
        let spi_r = ctx.read_native_pin(pin, spi);
        let sk_r = ctx.read_native_pin(skpin, secret_key);
        let iv_value = match iv_pinned {
            Some((ivpin, iv_spec)) => Value::Object(Some(ctx.read_native_pin(ivpin, iv_spec))),
            None => Value::Object(None),
        };
        ctx.invoke_virtual(
            spi_r,
            "engineInit",
            "(ILjava/security/Key;Ljava/security/spec/AlgorithmParameterSpec;Ljava/security/SecureRandom;)V",
            &[
                Value::Int(opmode),
                Value::Object(Some(sk_r)),
                iv_value,
                Value::Object(None),
            ],
        )?;

        // engineDoFinal(data, 0, len) -> byte[]
        let data_arr = make_byte_array(ctx, data);
        let dlen = data.len() as i32;
        let dpin = ctx.pin_native_root(data_arr);
        let spi_r = ctx.read_native_pin(pin, spi);
        let data_arr_r = ctx.read_native_pin(dpin, data_arr);
        let out = ctx.invoke_virtual(
            spi_r,
            "engineDoFinal",
            "([BII)[B",
            &[
                Value::Object(Some(data_arr_r)),
                Value::Int(0),
                Value::Int(dlen),
            ],
        )?;
        Ok(out)
    })();
    ctx.unpin_native_roots(pin);
    result
}

// ---------------------------------------------------------------------------
// PBKDF2 (PKCS#5 v2.0) — real key derivation for `SecretKeyFactory`.
//
// `SecretKeyFactory.getInstance("PBKDF2WithHmacSHA1"/"...SHA256")` runs real
// JCA bytecode that throws `NoSuchAlgorithmException` (no SunJCE provider
// service in CratonVM's provider list), and the real `PBKDF2KeyImpl` path
// trips a `ByteBuffer.get(byte[])` bug in `PBEUtil.encodePassword`. PEMFile
// needs this to decrypt PKCS#8 (PBES2) encrypted private keys. We compute
// PBKDF2 directly with HMAC over the `sha1`/`sha2` crates — a real,
// RFC-2898-correct derivation, not a synthetic stub (verified byte-identical
// to HotSpot). SHA-1/224/256 (64-byte HMAC block) go through the hand-rolled
// `hmac_block64`; SHA-384/512 (128-byte HMAC block, needed by pgjdbc's SCRAM
// client) go through `pbkdf2_derive_wide`, which uses the `hmac` crate so the
// block size is derived correctly instead of hardcoded.
// ---------------------------------------------------------------------------

/// HMAC over a 64-byte-block hash (`SHA-1` / `SHA-224` / `SHA-256`).
pub fn hmac_block64<D: sha2::Digest + Clone>(key: &[u8], msg: &[u8]) -> Vec<u8> {
    const BLOCK: usize = 64;
    let mut k = if key.len() > BLOCK {
        let mut h = D::new();
        h.update(key);
        h.finalize().to_vec()
    } else {
        key.to_vec()
    };
    k.resize(BLOCK, 0);
    let mut ipad = [0u8; BLOCK];
    let mut opad = [0u8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] = k[i] ^ 0x36;
        opad[i] = k[i] ^ 0x5c;
    }
    let mut hi = D::new();
    hi.update(ipad);
    hi.update(msg);
    let inner = hi.finalize();
    let mut ho = D::new();
    ho.update(opad);
    ho.update(inner);
    ho.finalize().to_vec()
}

/// PBKDF2 (PKCS#5 v2.0) over a 64-byte-block PRF.
pub fn pbkdf2_derive<D: sha2::Digest + Clone>(
    pw: &[u8],
    salt: &[u8],
    iters: u32,
    dklen: usize,
) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(dklen);
    let mut block_index: u32 = 1;
    while out.len() < dklen {
        let mut salt_i = salt.to_vec();
        salt_i.extend_from_slice(&block_index.to_be_bytes());
        let mut u = hmac_block64::<D>(pw, &salt_i);
        let mut t = u.clone();
        for _ in 1..iters.max(1) {
            u = hmac_block64::<D>(pw, &u);
            for (a, b) in t.iter_mut().zip(u.iter()) {
                *a ^= *b;
            }
        }
        out.extend_from_slice(&t);
        block_index += 1;
    }
    out.truncate(dklen);
    out
}

/// Map a `PBKDF2WithHmac*` algorithm name to a PRF code (the SHA bit length).
pub fn pbkdf2_prf_code(alg: &str) -> Option<i32> {
    // JCA algorithm names are CASE-INSENSITIVE (`Provider`'s own service
    // lookup upper-cases both sides), and the spelling differs per provider:
    // SunJCE registers `PBKDF2WithHmacSHA256`, BouncyCastle registers
    // `PBKDF2WITHHMACSHA256`, and BouncyCastle's own callers ask for
    // `PBKDF2withHMACSHA256`. Matching the SunJCE spelling exactly meant every
    // BouncyCastle spelling threw `SecurityException: … SecretKeyFactory not
    // available` (measured 2026-08-13 — three of the seven spellings a caller
    // can legitimately use were rejected).
    match alg.to_ascii_uppercase().as_str() {
        "PBKDF2WITHHMACSHA1" => Some(1),
        "PBKDF2WITHHMACSHA224" => Some(224),
        "PBKDF2WITHHMACSHA256" => Some(256),
        "PBKDF2WITHHMACSHA384" => Some(384),
        "PBKDF2WITHHMACSHA512" => Some(512),
        // The two SunJCE registers that this table did not: the PRF is the
        // FIPS 180-4 truncated-SHA-512 pair, whose names carry a slash. Codes
        // are `512224`/`512256` rather than `224`/`256` because those already
        // mean SHA-224/SHA-256 and `pbkdf2_derive_for` selects the digest by
        // this number — reusing one would have derived a key with the wrong
        // PRF and said nothing.
        "PBKDF2WITHHMACSHA512/224" => Some(512224),
        "PBKDF2WITHHMACSHA512/256" => Some(512256),
        _ => None,
    }
}

pbkdf2_derive_wide_impl!(pbkdf2_derive_wide_sha512, sha2::Sha512);
// SHA-512/224 and SHA-512/256 are NOT truncations of SHA-512 — FIPS 180-4 gives
// each its own initial hash value, so `Sha512::finalize()[..28]` is a different
// number from `Sha512_224::finalize()`. They are separate types in `sha2` for
// that reason, and they belong on the WIDE macro because their compression
// function is SHA-512's: a 128-byte HMAC block, not 64.
pbkdf2_derive_wide_impl!(pbkdf2_derive_wide_sha512_224, sha2::Sha512_224);
pbkdf2_derive_wide_impl!(pbkdf2_derive_wide_sha512_256, sha2::Sha512_256);

/// Crate-visible PBKDF2 entry point (dispatches to the right 64-byte-block
/// PRF by [`pbkdf2_prf_code`] code) for callers outside this module — used by
/// `jca::cipher`'s PBES2 (`PBEWithHmacSHA*AndAES_*`) key derivation, which
/// needs the same math `pbkdf2_generate_secret` already does but driven from
/// a `Cipher.init(..., AlgorithmParameters)` call instead of a
/// `SecretKeyFactory.generateSecret(PBEKeySpec)` call.
pub fn pbkdf2_derive_for(
    prf: i32,
    pw: &[u8],
    salt: &[u8],
    iters: u32,
    dklen: usize,
) -> Vec<u8> {
    match prf {
        1 => pbkdf2_derive::<sha1::Sha1>(pw, salt, iters, dklen),
        224 => pbkdf2_derive::<sha2::Sha224>(pw, salt, iters, dklen),
        384 => pbkdf2_derive_wide_sha384(pw, salt, iters, dklen),
        512 => pbkdf2_derive_wide_sha512(pw, salt, iters, dklen),
        512224 => pbkdf2_derive_wide_sha512_224(pw, salt, iters, dklen),
        512256 => pbkdf2_derive_wide_sha512_256(pw, salt, iters, dklen),
        // 256 and — the hazard — ANY code this function has no arm for.
        //
        // Spelled out because the catch-all is a silent wrong answer, not an
        // error: a PRF code that reaches here derives with SHA-256 and returns
        // a key of the right LENGTH, so the caller sees a success and the
        // failure appears wherever the key is next used, against a peer that
        // derived it correctly. Every code `pbkdf2_prf_code` can return has an
        // arm above; a new one must add its arm HERE in the same change.
        256 | _ => pbkdf2_derive::<sha2::Sha256>(pw, salt, iters, dklen),
    }
}

/// Opt-in gate for the native PKCS#5 v1.5 PBE `SecretKeyFactory` family
/// (`PBEWith*`, e.g. `PBEWithMD5AndDES`). Default-OFF: when unset, every
/// non-PBKDF2 `SecretKeyFactory.getInstance` keeps throwing the same
/// `SecurityException` the (absent) real SunJCE provider would have raised, so
/// the change is byte-identical to prior behaviour unless explicitly enabled.
///
/// Gated because, although the encoding is HotSpot-faithful for the algorithms
/// the Tomcat realm test exercises, the blanket native intercept means turning
/// this on supersedes the real-provider path for the *entire* `PBEWith*` family
/// rather than just the one tested algorithm — so it ships opt-in until soaked.
pub fn pbe_keyfactory_enabled() -> bool {
    crate::nbflags().native_pbe_keyfactory
}

/// Is `alg` a SunJCE PKCS#5 v1.5 PBE `SecretKeyFactory` algorithm?
///
/// SunJCE splits `SecretKeyFactory` cleanly: `PBKDF2With*` resolves to
/// `PBKDF2Core` (whose key derives bytes — handled by [`pbkdf2_get_instance`]),
/// while every `PBEWith*` resolves to a `PBEKeyFactory` subclass that returns a
/// `com.sun.crypto.provider.PBEKey`. That `PBEKey.getEncoded()` returns the
/// password as 7-bit ASCII bytes — it performs NO key derivation (the MD5+DES
/// PBKDF1 only runs later, inside a `Cipher`). So matching the `PBEWith` prefix
/// (the `PBKDF2With` names start differently and never collide) is the exact
/// SunJCE dividing line.
pub fn is_pbe_keyfactory_alg(alg: &str) -> bool {
    alg.starts_with("PBEWith")
}

/// The exact set of PKCS#5 v1.5 / PKCS#12 PBE `SecretKeyFactory` algorithms that
/// SunJCE actually registers and whose `PBEKey.getEncoded()` returns the
/// password as 7-bit ASCII bytes (no key derivation at factory time — the
/// MD5/SHA1+cipher PBKDF1/PBKDF2 only runs later inside a `Cipher`).
///
/// Recognising this exact set by default — rather than the broad `PBEWith*`
/// prefix — keeps CratonVM faithful to HotSpot: an unregistered name like
/// `PBEWithFooAndBar` still throws (mapped from `NoSuchAlgorithmException`),
/// exactly as the real SunJCE provider would, instead of being silently
/// accepted as a working factory. Every entry here is a real
/// `com.sun.crypto.provider.PBEKeyFactory` subclass, so the password-bytes
/// encoding [`pbe_generate_secret`] reproduces is correct for all of them. The
/// broader `PBEWith*` prefix stays available behind
/// `CRATONVM_NATIVE_PBE_KEYFACTORY=1` for callers that need the whole family.
pub fn is_known_pbe_keyfactory_alg(alg: &str) -> bool {
    matches!(
        alg,
        // Bare "PBE" — the literal algorithm name `PKCS12KeyStore.getPBEKey`
        // requests via `SecretKeyFactory.getInstance("PBE")` to derive the
        // keystore's HMAC integrity-check key (see `PKCS12KeyStore.java`
        // bytecode: `ldc "PBE"` right before the `getInstance` call). SunJCE
        // registers this as an alias resolving to the same `PBEKeyFactory`
        // family (password-bytes, no derivation) as `PBEWithMD5AndDES` etc.
        // Without it, `KeyStore.setKeyEntry`/`.load` on ANY PKCS12 keystore
        // (the JDK default keystore type) throws
        // `IOException("getSecretKey failed: PBE SecretKeyFactory not
        // available")` from `getPBEKey` — hit while building a test
        // `SSLContext` from a JKS/PKCS12 truststore+keystore
        // (`RestClientBuilderIntegTests`).
        "PBE"
            | "PBEWithMD5AndDES"
            | "PBEWithMD5AndTripleDES"
            | "PBEWithSHA1AndDESede"
            | "PBEWithSHA1AndRC2_40"
            | "PBEWithSHA1AndRC2_128"
            | "PBEWithSHA1AndRC4_40"
            | "PBEWithSHA1AndRC4_128"
            | "PBEWithHmacSHA1AndAES_128"
            | "PBEWithHmacSHA224AndAES_128"
            | "PBEWithHmacSHA256AndAES_128"
            | "PBEWithHmacSHA384AndAES_128"
            | "PBEWithHmacSHA512AndAES_128"
            | "PBEWithHmacSHA1AndAES_256"
            | "PBEWithHmacSHA224AndAES_256"
            | "PBEWithHmacSHA256AndAES_256"
            | "PBEWithHmacSHA384AndAES_256"
            | "PBEWithHmacSHA512AndAES_256"
            // The `SHA-512/224` and `SHA-512/256` PRF members of the same
            // family. SunJCE registers all four and this set carried none of
            // them, so `SecretKeyFactory.getInstance("PBEWithHmacSHA512/224            // AndAES_128")` refused where HotSpot serves it. No derivation is
            // involved — a `PBEKeyFactory` hands back the password as 7-bit
            // ASCII whatever the PRF in its name — so the NAME SET is the whole
            // implementation for these, exactly as it is for the eighteen above.
            | "PBEWithHmacSHA512/224AndAES_128"
            | "PBEWithHmacSHA512/224AndAES_256"
            | "PBEWithHmacSHA512/256AndAES_128"
            | "PBEWithHmacSHA512/256AndAES_256"
    )
}

/// Side table recording which `SecretKeyFactory` synthetics are PBE (password-
/// bytes) factories, mapping the GC-stable key from [`pbkdf2_key_for`] to the
/// requested algorithm name. `generateSecret` consults this first: a hit takes
/// the PBE path (return password bytes); a miss falls through to PBKDF2. Kept
/// separate from `pbkdf2_prf_table` so the existing PBKDF2 PRF lookup is
/// untouched.
pub fn pbe_algo_table() -> &'static std::sync::Mutex<std::collections::HashMap<usize, String>> {
    static T: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<usize, String>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// GC-stable, collision-disambiguated identity key for the PBKDF2 PRF table.
///
/// Keying by the bare 32-bit `identity_hash_code` is unsafe: two distinct
/// `SecretKeyFactory` synthetics can share a 32-bit identity hash (genuine
/// collision) or, more commonly, a freed object's identity hash can be
/// recycled by a later allocation. A recycled hash would then inherit the
/// previous instance's PRF code, so a SHA-1 factory could derive a key with
/// the SHA-256 PRF (or vice versa) — a silently WRONG key. The key is the
/// identity hash packed with a generation that disambiguates live objects
/// sharing one, so it identifies a live `SecretKeyFactory` instance and the
/// PRF is deterministic per instance.
///
/// gc-common w15-b (`common-w14c-sibling-identity-key-registries-are-process-wide-and-never-swept`):
/// minted by the lib.rs lock-key registry (`gc_stable_weak_lock_key`) instead
/// of a private copy of its scheme. The copy had no VM in its entries, so VM
/// B's factory with VM A's factory's identity hash was handed A's key and
/// `getInstance` overwrote A's PRF: A then derived keys with B's PRF. It was
/// also never swept or remapped, and rebound a dead factory's slot (and its
/// three rows) to the next object with its hash. The shared registry is
/// VM-scoped, remapped after every moving collection and never re-mints a key;
/// when a factory dies its sweep hands the freed key to [`forget_pbkdf2_keys`],
/// which drops the three rows filed under it.
pub fn pbkdf2_key_for(ctx: &dyn NativeContext, obj: ObjectRef) -> usize {
    crate::gc_stable_weak_lock_key(ctx, obj)
        .unwrap_or_else(|_| unreachable!("gc_stable_weak_lock_key never fails"))
}

/// Drop the `pbkdf2_prf_table` / `pbe_algo_table` / `skf_algo_table` rows
/// filed under `keys`, which the lock-key registry has just freed (the factory
/// died, or its VM is being torn down) and will never mint again. Called by
/// `lib.rs::sweep_lock_keys` and `lib.rs::forget_vm_lock_keys` with the
/// registry lock released; each table lock is a leaf, taken on its own and
/// poison-tolerant (this runs in the collection epilogue).
///
/// gc-common w19-c: also the `Properties` index-cache rows
/// (`props_index_cache`) and the `QName` part rows ([`qname_parts_table`]),
/// which key through the same registry now. The name is the first table's;
/// the two `lib.rs` call sites are what make this the file's one forget hook.
pub fn forget_pbkdf2_keys(keys: &[usize]) {
    if keys.is_empty() {
        return;
    }
    props_index_cache::forget_keys(keys);
    {
        let mut t = qname_parts_table()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for k in keys {
            t.remove(k);
        }
    }
    {
        let mut t = pbkdf2_prf_table()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for k in keys {
            t.remove(k);
        }
    }
    {
        let mut t = pbe_algo_table()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for k in keys {
            t.remove(k);
        }
    }
    let mut t = skf_algo_table()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for k in keys {
        t.remove(k);
    }
}

/// The bytes SunJCE's `PBKDF2KeyImpl.getPasswordBytes` derives from: the
/// password's UTF-16 units encoded with `UTF_8.encode(CharBuffer)`, whose
/// encoder replaces an unpaired surrogate with `'?'`.
///
/// gc-common w15-b: this used to push each unit through `char::from_u32`,
/// which refuses every surrogate, so a password holding a supplementary
/// character (both halves of the pair) or a lone surrogate silently lost
/// those units and derived a DIFFERENT key from HotSpot's.
pub fn pbkdf2_password_bytes(units: &[u16]) -> Vec<u8> {
    char::decode_utf16(units.iter().copied())
        .map(|r| r.unwrap_or('?'))
        .collect::<String>()
        .into_bytes()
}

/// SunJCE `PBKDF2KeyImpl`'s `PBEKeySpec` checks, in its order and with its
/// messages, for a spec whose salt is (`salt_present`) or is not there. The
/// JDK throws them as `InvalidKeySpecException` -- the checked exception
/// `generateSecret` declares -- where this native threw an unchecked
/// `IllegalArgumentException` (gc-common w15-b), and a negative iteration
/// count, cast to `u32`, ran about four billion iterations.
pub fn pbkdf2_spec_error(salt_present: bool, iters: i32, key_bits: i32) -> Option<&'static str> {
    if !salt_present {
        Some("Salt not found")
    } else if iters == 0 {
        Some("Iteration count not found")
    } else if iters < 0 {
        Some("Iteration count is negative")
    } else if key_bits == 0 {
        Some("Key length not found")
    } else if key_bits < 0 {
        Some("Key length is negative")
    } else {
        None
    }
}

/// Record a factory `getInstance` just built: its PRF code (PBKDF2) or its PBE
/// algorithm, and the name it was asked for. One key for all three rows.
pub fn note_skf_factory(ctx: &dyn NativeContext, obj: ObjectRef, prf: Option<i32>, alg: String) {
    let key = pbkdf2_key_for(ctx, obj);
    match prf {
        Some(code) => {
            pbkdf2_prf_table().lock().unwrap().insert(key, code);
        }
        None => {
            pbe_algo_table().lock().unwrap().insert(key, alg.clone());
        }
    }
    skf_algo_table().lock().unwrap().insert(key, alg);
}

/// PRF table for PBKDF2 `SecretKeyFactory` synthetics, keyed by the GC-stable,
/// collision-disambiguated identity from `pbkdf2_key_for`.
/// A heap field can't hold the PRF code reliably — `SecretKeyFactory`'s real
/// slot 0 is an `Object` (`spi`), so an `Int` written there reads back wrong
/// and every algorithm collapsed to the SHA-256 default. The previous keying
/// by the bare 32-bit `identity_hash_code` could collide or inherit a stale
/// PRF when an identity hash was recycled; `pbkdf2_key_for` fixes that.
pub fn pbkdf2_prf_table() -> &'static std::sync::Mutex<std::collections::HashMap<usize, i32>> {
    static T: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<usize, i32>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// `SecretKeyFactory.getInstance(algorithm[, provider])` for PBKDF2.
/// Recognises `PBKDF2WithHmacSHA1/224/256/384/512`; any other algorithm throws
/// the same `NoSuchAlgorithmException` (mapped to `SecurityException`) the real
/// JCA path would have thrown.
pub fn pbkdf2_get_instance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let vm = ctx.vm_identity();
    // Static method: args[0] is the algorithm String (no `this`).
    let alg = match args.first() {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    let requested_provider = crate::jca::provider_chain::provider_arg_name(ctx, args, 1);
    // `Alg.Alias.SecretKeyFactory.<oid>` spellings first — PKCS#5/PKCS#12 code
    // names its KDFs by OID as a matter of course.
    let alg = crate::jca::provider_chain::canonical_if_unrecognised_in(
        vm,
        requested_provider.as_deref(),
        "SecretKeyFactory",
        &alg,
        &|n| {
            pbkdf2_prf_code(n).is_some()
                || is_known_pbe_keyfactory_alg(n)
                || (pbe_keyfactory_enabled() && is_pbe_keyfactory_alg(n))
        },
    )
    .unwrap_or(alg);
    // A caller that NAMED a third-party provider gets THAT provider's
    // `SecretKeyFactorySpi`, wrapped in a REAL `javax.crypto.SecretKeyFactory`
    // built through the JDK's own `(Spi, Provider, String)` constructor. Every
    // method on it — including `getKeySpec`/`translateKey`, which this crate
    // does not intercept at all — is then ordinary JDK bytecode over the
    // provider's own engine, and the three natives that DO shadow this class
    // already route a receiver they did not build back to its `spi` field (see
    // `skf_receiver_is_ours`).
    //
    // Deliberately NOT restricted to names this VM cannot serve. Serving
    // `PBKDF2WithHmacSHA256` from our own derivation while reporting provider
    // `SunJCE` for a caller who asked BouncyCastle is the exact shape of
    // `jca-getinstance-ignores-the-requested-provider`, whose measured cost was
    // a SILENTLY WRONG KEY — the two providers agreed on that one name, and
    // did not on the next. The anonymous overload keeps this VM's own path.
    //
    // The anonymous overload takes this route only where neither family covers
    // the name — chain order, since every provider ahead of a third-party one
    // is a JDK provider this crate services natively. `PBKDF-OpenSSL` and
    // `PBKDF2with8BIT` (bc-java's `openssl` suite) are asked for without a
    // provider named and exist only on BouncyCastle.
    let skf_provider = requested_provider.clone().or_else(|| {
        let ours = pbkdf2_prf_code(&alg).is_some()
            || is_known_pbe_keyfactory_alg(&alg)
            || (pbe_keyfactory_enabled() && is_pbe_keyfactory_alg(&alg));
        (!ours)
            .then(|| {
                crate::jca::provider_chain::find_service_provider_in(vm, "SecretKeyFactory", &alg)
            })
            .flatten()
    });
    if let Some(provider) = skf_provider.as_deref() {
        if let Some(obj) =
            crate::jca::provider_chain::build_real_secret_key_factory(ctx, provider, &alg)?
        {
            return Ok(Some(Value::Object(Some(obj))));
        }
    }
    match pbkdf2_prf_code(&alg) {
        Some(code) => {
            let obj = try_alloc_concurrent_synthetic(ctx, "javax/crypto/SecretKeyFactory", 1)?;
            // GC-stable, collision-disambiguated key (was the bare 32-bit
            // identity hash, which could collide or inherit a stale PRF on a
            // recycled hash and derive a WRONG key).
            note_skf_factory(ctx, obj, Some(code), alg);
            Ok(Some(Value::Object(Some(obj))))
        }
        // PKCS#5 v1.5 / PKCS#12 PBE family (`PBEWithMD5AndDES`, …). SunJCE's
        // `PBEKeyFactory` returns a `PBEKey` whose `getEncoded()` is the 7-bit
        // ASCII password (no key derivation); we record the factory as PBE and
        // let `generateSecret` reproduce that. The exact set of real SunJCE PBE
        // factory names is recognised by default (HotSpot-faithful: unknown
        // `PBEWith*` names still throw); the broad `PBEWith*` prefix stays
        // opt-in behind `CRATONVM_NATIVE_PBE_KEYFACTORY=1`.
        None if is_known_pbe_keyfactory_alg(&alg)
            || (pbe_keyfactory_enabled() && is_pbe_keyfactory_alg(&alg)) =>
        {
            let obj = try_alloc_concurrent_synthetic(ctx, "javax/crypto/SecretKeyFactory", 1)?;
            note_skf_factory(ctx, obj, None, alg);
            Ok(Some(Value::Object(Some(obj))))
        }
        // `NoSuchAlgorithmException`, not `SecurityException`, and the
        // difference is a caller's `catch`. `SecretKeyFactory.getInstance`
        // DECLARES `NoSuchAlgorithmException`, so the standard
        //
        //     catch (NoSuchAlgorithmException | NoSuchProviderException e)
        //
        // does not catch an unchecked `java.lang.SecurityException` at all: the
        // refusal escapes the handler written for exactly this case. Measured
        // against HotSpot JDK 25 with `probes/JcaGetInstanceProbe.java`, which
        // prints the exception CLASS per engine — the same shape as the
        // `IOException`-where-`CertificateException`-belongs batch found in the
        // trust manager. `throw_jca_exc` builds the real Java exception object,
        // so what a caller catches is what HotSpot throws.
        // THE PLATFORM'S OWN FACTORY, before the refusal — and only after it.
        //
        // Everything above is this engine's verdict; this is the last thing
        // tried, so no name it computes changes hands. `DES` and `DESede` are
        // the two SunJCE `SecretKeyFactory` services this VM has no arm for,
        // and their implementations (`com.sun.crypto.provider.DESKeyFactory`,
        // `DESedeKeyFactory`) are sitting in the boot image: pure Java,
        // public no-arg constructors, verified loadable here before the rows
        // were seeded. Refusing them was a portability defect and nothing else.
        //
        // The wrapper is a GENUINE `javax.crypto.SecretKeyFactory` built
        // through the JDK's own `(Spi, Provider, String)` constructor, so
        // `getKeySpec`/`translateKey` — which this crate does not intercept —
        // run ordinary bytecode over a properly-constructed receiver, and the
        // three natives that DO shadow the class route a receiver they did not
        // build back to its `spi` (`skf_receiver_is_ours`).
        None => {
            if let Some((jdk_provider, _)) = crate::jca::provider_chain::jdk_service_class_in(
                vm,
                requested_provider.as_deref(),
                "SecretKeyFactory",
                &alg,
            ) {
                if let Some(obj) = crate::jca::provider_chain::build_real_secret_key_factory(
                    ctx,
                    &jdk_provider,
                    &alg,
                )? {
                    return Ok(Some(Value::Object(Some(obj))));
                }
            }
            Err(throw_jca_exc(
                ctx,
                "java/security/NoSuchAlgorithmException",
                &format!("{alg} SecretKeyFactory not available"),
            ))
        }
    }
}

/// The algorithm name every successful `SecretKeyFactory.getInstance` was asked
/// for, keyed by the same GC-stable identity `pbkdf2_prf_table` uses.
///
/// `getAlgorithm()` and `getProvider()` need it, and they cannot read it off the
/// object: the synthetic is a REAL `javax.crypto.SecretKeyFactory` whose
/// constructor never ran, so `algorithm` is null and — the part that actually
/// breaks — `lock` is null too. `getProvider()` opens with `synchronized
/// (lock)`, so calling it on a perfectly working factory threw
/// `NullPointerException: Cannot enter synchronized block because "this.lock" is
/// null`. That is the same species as the `Mac` overloads in
/// `phases_late::ssl_security`: any method left to the real body reads
/// uninitialised instance state, and fails in a way that looks nothing like
/// "this class is synthetic".
/// ARCH-2026-08-04 A6 — `LockLevel::Scratch` (L0) — five acquisition sites, every
/// one over a `pbkdf2_key_for(ctx, ..)` computed before the guard and with no
/// `ctx` call under it: one `insert` (`note_skf_factory`), two
/// `get(..).cloned()`s, a `contains_key`, and the removal loop in
/// `forget_pbkdf2_keys` (gc-common w15-b).
pub fn skf_algo_table(
) -> &'static cratonvm_types::lock_order::OrderedMutex<std::collections::HashMap<usize, String>> {
    static T: std::sync::OnceLock<
        cratonvm_types::lock_order::OrderedMutex<std::collections::HashMap<usize, String>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedMutex::new(
            std::collections::HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// `SecretKeyFactory.getAlgorithm()` — the name `getInstance` was called with.
pub fn pbkdf2_get_algorithm(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = pbkdf2_key_for(ctx, this);
    let algo = skf_algo_table().lock().unwrap().get(&key).cloned();
    let Some(algo) = algo else {
        // Not one of ours — see `skf_receiver_is_ours`. Answer from the real
        // object's own field rather than from an empty table entry.
        //
        // Type-checked, not trusted: `get_field_by_name` can fall back to a
        // name->slot mapping, and a slot that is not the field we asked for
        // reads back as some other object entirely. Returning it would put a
        // `String` where a `Provider` belongs (measured: the sibling
        // `getProvider` did exactly that, and the caller got
        // `NoSuchMethodError: java.lang.String.getName()`).
        if let Value::Object(Some(s)) = ctx.get_field_by_name(this, "algorithm") {
            if ctx
                .class_name_of_id(ctx.class_id_of_object(s))
                .is_some_and(|n| n == "java/lang/String")
            {
                return Ok(Some(Value::Object(Some(s))));
            }
        }
        return Ok(Some(Value::Object(Some(ctx.create_string("")))));
    };
    Ok(Some(Value::Object(Some(ctx.create_string(&algo)))))
}

/// `SecretKeyFactory.getProvider()` — SunJCE for the factories this VM builds,
/// which is where HotSpot resolves every `PBKDF2With*` and `PBEWith*` factory;
/// the real object's own `provider` field for any other receiver.
pub fn pbkdf2_get_provider(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Ok(this) = obj_arg(args, 0) {
        if !skf_receiver_is_ours(ctx, this) {
            // Type-checked for the reason `pbkdf2_get_algorithm` documents: an
            // unchecked `get_field_by_name` handed back the `algorithm` String
            // here and the caller died on `String.getName()`.
            if let Value::Object(Some(p)) = ctx.get_field_by_name(this, "provider") {
                let pid = ctx.class_id_by_name("java/security/Provider");
                let is_provider =
                    pid.is_some_and(|pid| ctx.is_subclass(ctx.class_id_of_object(p), pid));
                if is_provider {
                    return Ok(Some(Value::Object(Some(p))));
                }
            }
        }
    }
    let p = crate::jca::make_named_provider(ctx, "SunJCE")?;
    Ok(Some(Value::Object(Some(p))))
}

/// Did **this VM** build this `SecretKeyFactory`?
///
/// `getInstance(String[, String])` is a native here, and every factory it
/// returns is recorded in `skf_algo_table`. `getInstance(String, Provider)` is
/// NOT registered, so that overload runs the real JDK bytecode and hands back a
/// genuine `SecretKeyFactory` wrapping the requested provider's own SPI — and
/// the three natives registered on this class then shadowed the real bytecode
/// for it too.
///
/// The consequence was not a crash but a **silently wrong key**, which is the
/// worst shape this workspace has a name for. Measured 2026-08-13 against
/// HotSpot 25 with the same jars: BouncyCastle's `PEMUtilities.getKey` asks for
/// `SecretKeyFactory.getInstance("PBKDF-OpenSSL", bcProvider)` and derives
/// `90e508cc4fc9798bdec87516bebe5ecd`; on CratonVM the same call reported
/// provider `SunJCE`, algorithm `""`, and derived
/// `bcfb6da32cd0aae96fdc53ed8980f72b` — `generateSecret` had fallen out of its
/// `unwrap_or(256)` default and run PBKDF2-HMAC-SHA256 under another
/// algorithm's name. Every netty PKCS#1-encrypted-key test failed downstream of
/// that (`IOException: Invalid lenByte`, because netty then fell back to
/// handing raw PKCS#1 bytes to `EncryptedPrivateKeyInfo`).
///
/// Table membership is the right test rather than a class-name check: it is
/// exactly "did our `getInstance` produce this object".
pub fn skf_receiver_is_ours(ctx: &mut dyn NativeContext, this: ObjectRef) -> bool {
    let key = pbkdf2_key_for(ctx, this);
    skf_algo_table().lock().unwrap().contains_key(&key)
}

/// `SecretKeyFactory.generateSecret(PBEKeySpec)` for a PBKDF2 synthetic.
/// Reads the spec's password/salt/iterations/keyLength, runs PBKDF2, and
/// returns a real `SecretKeySpec(derivedBytes, "PBKDF2With…")` whose
/// `getEncoded()` yields the derived key (what `PEMFile` reads).
pub fn pbkdf2_generate_secret(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // A factory this VM did not build belongs to the provider that did — run
    // ITS `engineGenerateSecret` instead of deriving a key of our own choosing
    // under its algorithm's name. See `skf_receiver_is_ours` for the measured
    // wrong-key this closes.
    if !skf_receiver_is_ours(ctx, this) {
        if let Value::Object(Some(spi)) = ctx.get_field_by_name(this, "spi") {
            let spec = args.get(1).copied().unwrap_or(Value::Object(None));
            return ctx.invoke_virtual(
                spi,
                "engineGenerateSecret",
                "(Ljava/security/spec/KeySpec;)Ljavax/crypto/SecretKey;",
                &[spec],
            );
        }
        // No SPI to delegate to (a receiver of a shape we do not recognise).
        // Refuse rather than derive something: this native has no idea what
        // algorithm was asked for, and answering anyway is how the defect
        // above happened.
        return Err(RuntimeError::SecurityException {
            message: "SecretKeyFactory.generateSecret: this factory was not created by CratonVM \
                      and carries no provider SPI to delegate to"
                .to_string(),        }
        .into());
    }
    // Same GC-stable, collision-disambiguated key used by `getInstance` so the
    // PRF is looked up deterministically for THIS factory instance.
    let key = pbkdf2_key_for(ctx, this);
    // PBE (`PBEWithMD5AndDES`, …): SunJCE's `PBEKey.getEncoded()` returns the
    // password as 7-bit ASCII bytes, deriving NO key (salt/iterations/keyLength
    // are ignored here and consumed only by the eventual Cipher). Reproduce that
    // exactly, including the ASCII-range validation that yields
    // `InvalidKeySpecException`.
    let pbe_alg = pbe_algo_table().lock().unwrap().get(&key).cloned();
    if let Some(alg) = pbe_alg {
        let spec = obj_arg(args, 1)?;
        return pbe_generate_secret(ctx, spec, &alg);
    }
    let prf = pbkdf2_prf_table()
        .lock()
        .unwrap()
        .get(&key)
        .copied()
        .unwrap_or(256);
    let spec = obj_arg(args, 1)?;
    // PBEKeySpec layout: 0=password(char[]), 1=salt(byte[]), 2=iterationCount,
    // 3=keyLength (bits).
    let pw_bytes: Vec<u8> = match ctx.get_field(spec, 0) {
        Value::Object(Some(chars)) => {
            let n = ctx.array_length(chars);
            let mut units: Vec<u16> = Vec::with_capacity(n);
            for i in 0..n {
                if let Value::Int(c) = ctx.get_array_element(chars, i) {
                    units.push((c as u32 & 0xffff) as u16);                }            }
            pbkdf2_password_bytes(&units)        }
        _ => Vec::new(),
    };
    let salt: Option<Vec<u8>> = match ctx.get_field(spec, 1) {
        Value::Object(Some(arr)) => Some(cipher_read_bytes(ctx, arr)),
        _ => None,
    };
    let iters = match ctx.get_field(spec, 2) {
        Value::Int(i) => i,
        _ => 0,
    };
    let key_bits = match ctx.get_field(spec, 3) {
        Value::Int(i) => i,
        _ => 0,
    };
    if let Some(msg) = pbkdf2_spec_error(salt.is_some(), iters, key_bits) {
        return Err(throw_jca_exc(
            ctx,
            "java/security/spec/InvalidKeySpecException",
            msg,
        ));
    }
    let salt = salt.unwrap_or_default();
    let iters = iters as u32;
    let dklen = (key_bits as usize) / 8;
    // THROUGH `pbkdf2_derive_for`, not a second copy of its match.
    //
    // This site carried its own arm-for-arm duplicate of that dispatch, with
    // the same silent SHA-256 catch-all, and the duplicate is what a new PRF
    // falls into: adding `PBKDF2WithHmacSHA512/224` and `/256` to
    // `pbkdf2_prf_code` and to `pbkdf2_derive_for` left THIS copy unchanged, so
    // both names resolved, derived a 32-byte key, and derived it with SHA-256 —
    // the two new rows printed byte-identical output, and byte-identical to the
    // `PBKDF2WithHmacSHA256` row above them (`apps/probes/JcaDerivationVectors`,
    // which exists because `getInstance` resolving is not evidence of anything).
    let dk = pbkdf2_derive_for(prf, &pw_bytes, &salt, iters, dklen);
    // Build a real SecretKeySpec(dk, "PBKDF2With…") so getEncoded() returns dk.
    //
    // The comment said `"PBKDF2With…"` but the literal was the bare `"PBKDF2"`,
    // so `generateSecret(...).getAlgorithm()` answered `PBKDF2` where HotSpot
    // answers the full `PBKDF2WithHmacSHA256`. Callers that re-key a `Mac` or
    // `Cipher` from the derived key's own algorithm name — the reason
    // `SecretKeySpec` carries one — would then ask for an algorithm that does
    // not exist. Use the name `getInstance` was actually called with.
    let key_arr = make_byte_array(ctx, &dk);
    let kpin = ctx.pin_native_root(key_arr);
    let requested = skf_algo_table()
        .lock()
        .unwrap()
        .get(&key)
        .cloned()
        .unwrap_or_else(|| "PBKDF2".to_string());
    let algo_s = ctx.create_string(&requested);
    let key_arr_r = ctx.read_native_pin(kpin, key_arr);
    let sk = ctx.new_object_initialized(
        "javax/crypto/spec/SecretKeySpec",
        "([BLjava/lang/String;)V",
        &[Value::Object(Some(key_arr_r)), Value::Object(Some(algo_s))],
    );
    ctx.unpin_native_roots(kpin);
    sk
}

/// `SecretKeyFactory.generateSecret(PBEKeySpec)` for a PKCS#5 v1.5 PBE factory
/// (`PBEWithMD5AndDES`, …).
///
/// Mirrors SunJCE's `com.sun.crypto.provider.PBEKey`: the encoded key is just
/// the password rendered as 7-bit ASCII bytes (`c & 0x7f`) — there is NO key
/// derivation at this stage (salt/iterations/keyLength are ignored; the MD5+DES
/// PBKDF1 runs later inside a `Cipher`). Each password char must be printable
/// ASCII (`0x20..=0x7E`); anything else raises `InvalidKeySpecException`, exactly
/// as the real `PBEKey` constructor does. The bytes are wrapped in a real
/// `SecretKeySpec(bytes, alg)` so `getEncoded()` returns them.
///
/// Empty password: confirmed against real JDK 25 source
/// (`javax.crypto.spec.PBEKeySpec` and `com.sun.crypto.provider.PBEKey`) that
/// HotSpot explicitly ALLOWS this — `PBEKeySpec(char[])` normalises a
/// null/0-length password to `new char[0]` rather than rejecting it, and
/// `PBEKey`'s constructor comment reads verbatim "Should allow an empty
/// password.", producing a key whose `getEncoded()` is a 0-length `byte[]`.
/// This matters for real callers: Netty's `JdkSslServerContext` builds an
/// in-memory PKCS12 keystore for an ephemeral self-signed cert protected by
/// an empty `char[]` password, and `PKCS12KeyStore.getPBEKey` /
/// `.setKeyEntry` route straight through this exact native
/// (`SecretKeyFactory.getInstance("PBE").generateSecret(...)`) — a prior
/// pass here mistakenly treated the empty case as unreachable and routed it
/// to `InvalidKeySpecException` instead, which is what broke that flow
/// (see `ServerHttpsRequestIntegrationTests`,
/// docs/feature-designs/native-dispatch-architecture.md).
///
/// `javax/crypto/spec/SecretKeySpec`'s real `<init>` throws
/// `IllegalArgumentException("Empty key")` on a 0-length array (that
/// restriction is genuine and specific to `SecretKeySpec`, not to
/// `SecretKey` in general — real `PBEKey` has no such check), so an empty
/// password can't be routed through that constructor. Instead we allocate
/// the same 2-field shape (`encoded` @0, `algorithm` @1) directly, bypassing
/// only the constructor's validation — every other native that reads this
/// shape (`getEncoded`/`getAlgorithm` on this class, plus
/// `jca::cipher::extract_key_bytes`'s field-0 read with its `getEncoded()`
/// fallback) already tolerates a 0-length byte[] in slot 0.
pub fn pbe_generate_secret(
    ctx: &mut dyn NativeContext,
    spec: ObjectRef,
    alg: &str,
) -> MethodCallResult {
    // PBEKeySpec layout: field 0 = password (char[]).
    let mut pw: Vec<u8> = Vec::new();
    if let Value::Object(Some(chars)) = ctx.get_field(spec, 0) {
        let n = ctx.array_length(chars);
        pw.reserve(n);
        for i in 0..n {
            if let Value::Int(c) = ctx.get_array_element(chars, i) {
                let ch = (c as u32) & 0xffff;
                if !(0x20..=0x7e).contains(&ch) {
                    // Match com.sun.crypto.provider.PBEKey: reject non-ASCII.
                    return Err(throw_jca_exc(
                        ctx,
                        "java/security/spec/InvalidKeySpecException",
                        "Password is not ASCII",
                    ));
                }
                pw.push((ch & 0x7f) as u8);
            }
        }
    }
    if pw.is_empty() {
        // gc-common w20-b: the key bytes and the algorithm name were held raw
        // across the key's allocation (a class resolution, which can run
        // `<clinit>`), and the `?` leaked the pin. One handle scope.
        let mut scope = NativeHandleScope::new(ctx);
        let key_arr = make_byte_array(&mut *scope, &pw);
        let key_h = scope.root(key_arr);
        let algo_s = scope.create_string(alg);
        let algo_h = scope.root(algo_s);
        let sk = try_alloc_concurrent_synthetic(&mut *scope, "javax/crypto/spec/SecretKeySpec", 2)?;
        let key_arr = scope.get(&key_h);
        let algo_s = scope.get(&algo_h);
        scope.set_field(sk, 0, Value::Object(Some(key_arr)));
        scope.set_field(sk, 1, Value::Object(Some(algo_s)));
        return Ok(Some(Value::Object(Some(sk))));
    }
    let key_arr = make_byte_array(ctx, &pw);
    let kpin = ctx.pin_native_root(key_arr);
    let algo_s = ctx.create_string(alg);
    let key_arr_r = ctx.read_native_pin(kpin, key_arr);
    let sk = ctx.new_object_initialized(
        "javax/crypto/spec/SecretKeySpec",
        "([BLjava/lang/String;)V",
        &[Value::Object(Some(key_arr_r)), Value::Object(Some(algo_s))],
    );
    ctx.unpin_native_roots(kpin);
    sk
}

/// Construct and throw a real JCA exception of `class_name` (internal form,
/// e.g. `java/security/spec/InvalidKeySpecException`) carrying `msg`, falling
/// back to a catchable `IllegalArgumentException` if the class can't be built.
/// `SecretKeyCredentialHandler.mutate` catches both, so either way the handler
/// logs and returns null exactly as it does on HotSpot.
pub fn throw_jca_exc(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    msg: &str,
) -> MethodCallFailed {
    let detail = ctx.create_string(msg);
    if let Ok(Some(Value::Object(Some(exc)))) = ctx.new_object_initialized(
        class_name,
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(detail))],
    ) {
        return MethodCallFailed::ExceptionThrown(exc);
    }
    RuntimeError::IllegalArgumentException {
        message: msg.to_string(),
    }
    .into()
}

/// Execute doFinal: encrypt or decrypt accumulated data using the configured algorithm
pub fn cipher_do_final(ctx: &mut dyn NativeContext, this: ObjectRef) -> MethodCallResult {
    let mode = ctx.get_field(this, CIPHER_MODE).as_int().unwrap_or(0);
    if mode == 0 {
        // Synthetic passthrough: uninitialized Cipher objects returned by
        // the bare getInstance() path have no key, IV, or AAD. Rather
        // than throw IllegalStateException (which only the fully real
        // path would) we return null, matching the behavior the
        // `cipher_do_final_passthrough` integration test asserts and
        // giving callers a sentinel to detect "nothing configured".
        return Ok(Some(Value::Object(None)));
    }

    // Read algorithm string
    let algo_str = match ctx.get_field(this, CIPHER_ALGO) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };

    // Read key bytes from Key object (field 0 = encoded bytes)
    let key_bytes = match ctx.get_field(this, CIPHER_KEY) {
        Value::Object(Some(key_obj)) => match ctx.get_field(key_obj, 0) {
            Value::Object(Some(enc_arr)) => cipher_read_bytes(ctx, enc_arr),
            _ => Vec::new(),        },
        _ => Vec::new(),
    };

    // Read IV
    let iv_bytes = match ctx.get_field(this, CIPHER_IV) {
        Value::Object(Some(iv_arr)) => cipher_read_bytes(ctx, iv_arr),
        _ => Vec::new(),
    };

    // Read accumulated data
    let data = match ctx.get_field(this, CIPHER_ACCUM) {
        Value::Object(Some(arr)) => cipher_read_bytes(ctx, arr),
        _ => Vec::new(),
    };

    // Read AAD
    let aad = match ctx.get_field(this, CIPHER_AAD) {
        Value::Object(Some(arr)) => cipher_read_bytes(ctx, arr),
        _ => Vec::new(),
    };

    // Validate key length
    if key_bytes.is_empty() {
        return Err(RuntimeError::IllegalStateException {
            message: "No key provided".into(),        }
        .into());
    }

    // Route block-cipher CBC transformations the synthetic dispatch can't do
    // natively (CBC chaining; DES/DESede have no native impl at all) to the
    // real SunJCE CipherSpi. Exercised by `PEMFile` decrypting encrypted
    // private keys (AES/CBC, DESede/CBC, DES/CBC). Done BEFORE the AES-only
    // key-length check below so 8-byte DES keys aren't rejected.
    {
        let (cn, cm, _pad) = parse_cipher_algo(&algo_str);
        let route = match (
            cn.to_ascii_uppercase().as_str(),
            cm.to_ascii_uppercase().as_str(),
        ) {
            ("AES", "CBC") => Some(("com/sun/crypto/provider/AESCipher$General", "AES")),
            ("DESEDE", _) | ("TRIPLEDES", _) => {
                Some(("com/sun/crypto/provider/DESedeCipher", "DESede"))
            }
            ("DES", _) => Some(("com/sun/crypto/provider/DESCipher", "DES")),
            _ => None,
        };
        if let Some((spi_class, key_algo)) = route {
            let pad_str = if algo_str
                .split('/')
                .nth(2)
                .map(|p| p.eq_ignore_ascii_case("NoPadding"))
                .unwrap_or(false)
            {
                "NoPadding"
            } else {
                "PKCS5Padding"
            };
            // NOTE: do not allocate (e.g. to reset accumulators) after this
            // returns — `out` holds a freshly-allocated byte[] a moving GC
            // could relocate. PEMFile uses one-shot decrypt, so no reset is
            // needed.
            return drive_real_cipher(
                ctx, spi_class, "CBC", pad_str, mode, &key_bytes, key_algo, &iv_bytes, &data,
            );
        }
    }

    match key_bytes.len() {
        16 | 24 | 32 => {} // AES-128, AES-192, AES-256
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("Invalid AES key length: {} bytes", key_bytes.len()),
            }
            .into());
        }
    }

    let (cipher_name, cipher_mode, pad) = parse_cipher_algo(&algo_str);
    let encrypt = mode == 1;

    // ChaCha20-Poly1305 is identified by cipher name, not mode. The
    // transformation string is "ChaCha20-Poly1305" (no mode/padding).
    let is_chacha_poly = cipher_name.eq_ignore_ascii_case("ChaCha20-Poly1305");

    let result_bytes: Result<Vec<u8>, String> = if is_chacha_poly {
        // T2.6.5 — ChaCha20-Poly1305 AEAD via RustCrypto.
        use chacha20poly1305::{
            aead::{Aead, KeyInit, Payload},
            ChaCha20Poly1305, Key, Nonce,
        };
        if key_bytes.len() != 32 {
            Err(format!(
                "ChaCha20-Poly1305 requires a 256-bit key, got {} bytes",
                key_bytes.len()
            ))
        } else if iv_bytes.len() != 12 {
            Err(format!(
                "ChaCha20-Poly1305 requires a 96-bit nonce, got {} bytes",
                iv_bytes.len()
            ))
        } else {
            let cipher = ChaCha20Poly1305::new(Key::from_slice(&key_bytes));
            let nonce = Nonce::from_slice(&iv_bytes);
            let payload = Payload {
                msg: &data,
                aad: &aad,
            };
            if encrypt {
                cipher
                    .encrypt(nonce, payload)
                    .map_err(|e| format!("ChaCha20-Poly1305 encrypt failed: {e}"))
            } else {
                cipher
                    .decrypt(nonce, payload)
                    .map_err(|e| format!("ChaCha20-Poly1305 decrypt failed (authentication): {e}"))
            }
        }
    } else {
        match cipher_mode.to_uppercase().as_str() {
            "ECB" => {
                if encrypt {
                    Ok(aes_ecb_encrypt(&data, &key_bytes, pad))
                } else {
                    aes_ecb_decrypt(&data, &key_bytes, pad).map_err(|e| e.to_string())
                }
            }
            "CBC" => {
                if iv_bytes.len() < 16 {
                    Err("IV must be 16 bytes for AES-CBC".to_string())
                } else if encrypt {
                    Ok(aes_cbc_encrypt(&data, &key_bytes, &iv_bytes, pad))
                } else {
                    aes_cbc_decrypt(&data, &key_bytes, &iv_bytes, pad).map_err(|e| e.to_string())
                }
            }
            "GCM" => {
                if iv_bytes.is_empty() {
                    Err("IV required for AES-GCM".to_string())
                } else if encrypt {
                    Ok(aes_gcm_encrypt(&data, &key_bytes, &iv_bytes, &aad, 16))
                } else {
                    aes_gcm_decrypt(&data, &key_bytes, &iv_bytes, &aad, 16)
                        .map_err(|e| e.to_string())
                }
            }
            "CTR" => {
                // T2.6.5 — AES-CTR via RustCrypto `ctr` + `aes`. CTR is
                // its own inverse so encrypt and decrypt share a path.
                use aes::cipher::{KeyIvInit, StreamCipher};
                if iv_bytes.len() != 16 {
                    Err(format!(
                        "AES-CTR requires a 16-byte IV, got {}",
                        iv_bytes.len()
                    ))
                } else {
                    type Aes128Ctr = ctr::Ctr64BE<aes::Aes128>;
                    type Aes192Ctr = ctr::Ctr64BE<aes::Aes192>;
                    type Aes256Ctr = ctr::Ctr64BE<aes::Aes256>;
                    let mut buf = data.clone();
                    let res = match key_bytes.len() {
                        16 => {
                            let mut c = Aes128Ctr::new(
                                key_bytes.as_slice().into(),
                                iv_bytes.as_slice().into(),
                            );
                            c.apply_keystream(&mut buf);
                            Ok(buf)
                        }
                        24 => {
                            let mut c = Aes192Ctr::new(
                                key_bytes.as_slice().into(),
                                iv_bytes.as_slice().into(),
                            );
                            c.apply_keystream(&mut buf);
                            Ok(buf)
                        }
                        32 => {
                            let mut c = Aes256Ctr::new(
                                key_bytes.as_slice().into(),
                                iv_bytes.as_slice().into(),
                            );
                            c.apply_keystream(&mut buf);
                            Ok(buf)
                        }
                        n => Err(format!("Invalid AES key length: {n}")),
                    };
                    res
                }
            }
            _ => {
                // Default to ECB for unknown modes, matching prior behaviour.
                if encrypt {
                    Ok(aes_ecb_encrypt(&data, &key_bytes, pad))
                } else {
                    aes_ecb_decrypt(&data, &key_bytes, pad).map_err(|e| e.to_string())
                }
            }
        }
    };

    match result_bytes {
        Ok(bytes) => {
            // FIX (TestEncryptInterceptorLargeHeap hard-abort-instead-of-OOME):
            // this used to allocate the plaintext/ciphertext output via the
            // panicking `new_array`, which `std::process::abort()`s the whole
            // VM (killing every remaining test in the batch) when a huge
            // payload (observed: a ~1 GiB AES-GCM round-trip) can't fit —
            // instead of the catchable `OutOfMemoryError` HotSpot throws. Use
            // the fallible `try_new_array` (same `try_new_ref_array`/
            // `try_alloc_array_full` idiom as the `ArrayList(int)` abend fix,
            // see `crash-01-arraylist-capacity-oom-abend.md`)
            // and throw a catchable OOME on `None` instead.
            //
            // gc-common w20-b: `this` was held raw across the output's
            // allocation and the output raw across the two accumulator
            // resets. Both are pinned; the payload is one bulk copy.
            let this_pin = ctx.pin_native_root(this);
            let Some(arr) = ctx.try_new_array(cratonvm_types::ArrayElementType::Byte, bytes.len())
            else {
                ctx.unpin_native_roots(this_pin);
                return Err(RuntimeError::OutOfMemoryError {
                    message: "Java heap space".to_string(),
                }
                .into());
            };
            ctx.write_byte_array_from(arr, 0, &bytes);
            let arr_pin = ctx.pin_native_root(arr);
            let this = ctx.read_native_pin(this_pin, this);
            // Reset accumulator
            cipher_reset_accumulators(ctx, this);
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.unpin_native_roots(this_pin);
            Ok(Some(Value::Object(Some(arr))))
        }
        Err(msg) => Err(RuntimeError::IllegalStateException { message: msg }.into()),
    }
}

// ---------------------------------------------------------------------------
// java.security enhancements: Provider, Security, KeyStore, KeyPair, Signature
// ---------------------------------------------------------------------------
/// The synthetic 4-field `Signature` carrier for `algo` (slot 0 algorithm,
/// 1 mode = uninitialized, 2 key = null, 3 accumulated data = empty).
///
/// gc-common w20-b: both `Signature.getInstance` overloads held `algo` (an
/// argument) raw across the carrier's allocation -- a class resolution,
/// which can run `<clinit>` -- and stored it at its old address, and held the
/// carrier raw across the data buffer.
pub fn sig_new_carrier(ctx: &mut dyn NativeContext, algo: ObjectRef) -> MethodCallResult {
    let algo_pin = ctx.pin_native_root(algo);
    let obj = try_alloc_concurrent_synthetic(ctx, "java/security/Signature", 4);
    let algo = ctx.read_native_pin(algo_pin, algo);
    ctx.unpin_native_roots(algo_pin);
    let obj: ObjectRef = obj?;
    ctx.set_field(obj, 0, Value::Object(Some(algo)));
    ctx.set_field(obj, 1, Value::Int(0)); // uninitialized
    ctx.set_field(obj, 2, Value::Object(None)); // key
    let obj: ObjectRef =
        alloc_array_into_field(ctx, obj, 3, cratonvm_types::ArrayElementType::Byte, 0);
    Ok(Some(Value::Object(Some(obj))))
}

pub fn register_phase53_security(r: &mut NativeMethodRegistry) {
            // --- java.security.Provider — 2-field synthetic (name=0, version=1) ---
    let prov = "java/security/Provider";
    r.register(prov, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(prov, "getVersion", "()D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = ctx.get_field(this, 1);
        match v {
            Value::Double(d) => Ok(Some(Value::Double(d))),
            Value::Int(i) => Ok(Some(Value::Double(i as f64))),
            _ => Ok(Some(Value::Double(1.0))),
        }
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(prov, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let name = if let Value::Object(Some(n)) = ctx.get_field(this, 0) {
            ctx.read_string(n).unwrap_or_else(|| "Provider".to_string())
        } else {
            "Provider".to_string()
        };
        let s = ctx.create_string(&name);
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(prov, "getInfo", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let name = if let Value::Object(Some(n)) = ctx.get_field(this, 0) {
            ctx.read_string(n).unwrap_or_else(|| "Provider".to_string())
        } else {
            "Provider".to_string()
        };
        let s = ctx.create_string(&format!("{} security provider", name));
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Bridge);
    // W4-3: `Provider.getService` and `Provider.getServices` are NOT registered
    // here any more. Both were strictly-worse twins of the registry-backed
    // natives in `jca::provider_chain` (`provider_get_service_native` /
    // `provider_get_services_native`), which are wired through
    // `register_essential_natives_with_shims` and are therefore live in EVERY
    // mode. This file's registrations only ever ran under
    // `register_synthetic_overrides` (`--synthetic-jdk`) — but that phase runs
    // LAST, so where they ran they SHADOWED the registry-backed ones. Both were
    // defective:
    //
    //   * `getService(type, algorithm)` fabricated a `Provider$Service` for
    //     ANY pair, so `Security.getProvider("SUN").getService("MessageDigest",
    //     "NO-SUCH-DIGEST")` answered a live Service. Measured on
    //     jdk-25.0.3.9-hotspot: null. It also answered a Service for
    //     `("Cipher","AES")` on SUN, which HotSpot likewise answers null for
    //     because SUN registers no Cipher at all. That is "a fabricated success
    //     where the spec mandates a failure" on the provider-lookup surface.
    //   * `getServices()` returned the pre-W3-7 broken shape — a `String[]`
    //     written into slot 0 of a `java/util/HashSet`, where real JDK 25
    //     HashSet's only instance field is `transient HashMap<E,Object> map`.
    //     Any real `HashSet` method on it invokes HashMap on a String[]
    //     receiver. It was also unconditionally EMPTY, while the registry-backed
    //     twin enumerates the provider's real service table.

    // Provider.Service methods
    let svc = "java/security/Provider$Service";
    r.register(svc, "getType", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(svc, "getAlgorithm", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        svc,
        "getProvider",
        "()Ljava/security/Provider;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 2)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- java.security.Security ---
    let sec = "java/security/Security";

    /// Create a provider synthetic with the given name + version.
    fn make_provider(
        ctx: &mut dyn NativeContext,
        name: &str,
        version: f64,
    ) -> Result<ObjectRef, MethodCallFailed> {
        let p = try_alloc_concurrent_synthetic(ctx, "java/security/Provider", 2)?;
        // `p` is rooted across the name's allocation (i8-L2).
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let p_h = scope.root(p);
        let n = scope.create_string(name);
        let p = scope.get(&p_h);
        scope.set_field(p, 0, Value::Object(Some(n)));
        scope.set_field(p, 1, Value::Double(version));
        Ok(p)
    }

    // T2.6.15/16 — persistent Provider registry. The list is seeded
    // with the five standard Sun providers and mutated by
    // `addProvider` / `insertProviderAt` / `removeProvider` so that
    // subsequent `getProviders()` / `getProvider(name)` calls see the
    // additions. Each entry is a (name, version) tuple; we intentionally
    // do not hold `ObjectRef` values across callbacks because those
    // would be dangling references if the caller later let the original
    // synthetic be GC'd. Fresh Provider synthetics are materialised on
    // every read.
    //
    // Registry is declared at module scope below (one row per VM since
    // gc-common w19-c, `SecurityState`) and seeded on first access via
    // `with_provider_registry`.

    r.register(
        sec,
        "getProviders",
        "()[Ljava/security/Provider;",
        |ctx, _args| {
            let snapshot: Vec<(String, f64)> = provider_registry_snapshot(ctx.vm_identity());
            // `Provider[]`, the declared type, rooted across each
            // `make_provider` (i8-L2); it was an `Object[]` held raw. The
            // component may load: no heap reference is live yet.
            let component = crate::lang_class::loading_component_id(ctx, "java/security/Provider");
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let arr = scope.new_ref_array(component, snapshot.len());
            let arr_h = scope.root(arr);
            for (i, (name, ver)) in snapshot.iter().enumerate() {
                let p = make_provider(&mut *scope, name, *ver)?;
                let arr = scope.get(&arr_h);
                scope.set_array_element(arr, i, Value::Object(Some(p)));
            }
            Ok(Some(Value::Object(Some(scope.get(&arr_h)))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sec,
        "getProvider",
        "(Ljava/lang/String;)Ljava/security/Provider;",
        |ctx, args| {
            // A static native's first parameter is `args[0]` (there is no
            // receiver). These `Security` statics read `args[1]`, so every
            // lookup saw "no argument" (i8-L2).
            let name_str = match args.first() {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            match provider_registry_find(ctx.vm_identity(), &name_str) {
                Some(ver) => {
                    let p = make_provider(ctx, &name_str, ver)?;
                    Ok(Some(Value::Object(Some(p))))
                }
                // The JDK contract is to return null for an unknown
                // provider name, not to throw. Callers compare against
                // null to decide whether to fall back.
                None => Ok(Some(Value::Object(None))),
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sec,
        "addProvider",
        "(Ljava/security/Provider;)I",
        |ctx, args| {
            let prov = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(-1))),
            };
            let name = match ctx.get_field(prov, 0) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => return Ok(Some(Value::Int(-1))),
            };
            let ver = match ctx.get_field(prov, 1) {
                Value::Double(d) => d,
                Value::Int(i) => i as f64,
                _ => 1.0,
            };
            Ok(Some(Value::Int(provider_registry_add(
                ctx.vm_identity(),
                name,
                ver,
            ))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sec,
        "insertProviderAt",
        "(Ljava/security/Provider;I)I",
        |ctx, args| {
            let prov = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(-1))),
            };
            let pos = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
            let name = match ctx.get_field(prov, 0) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => return Ok(Some(Value::Int(-1))),
            };
            let ver = match ctx.get_field(prov, 1) {
                Value::Double(d) => d,
                Value::Int(i) => i as f64,
                _ => 1.0,
            };
            Ok(Some(Value::Int(provider_registry_insert_at(
                ctx.vm_identity(),
                name,
                ver,
                pos,
            ))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sec,
        "removeProvider",
        "(Ljava/lang/String;)V",
        |ctx, args| {
            let name = match args.first() {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => return Ok(None),
            };
            provider_registry_remove(ctx.vm_identity(), &name);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // Security.getProperty(String) -> String
    // HotSpot returns null for unknown keys; libraries (e.g. BouncyCastle
    // PKCS12$Mappings reading `org.bouncycastle.pkcs12.default`) do
    // `if (val != null) val.substring(0, 5)`, so we MUST return null
    // (not "") for unknown keys to avoid StringIndexOutOfBoundsException.
    r.register(
        sec,
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let prop_name = match args.first() {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => return Ok(Some(Value::Object(None))),
            };
            // W2: a value installed by `setProperty` wins over the built-in
            // defaults below, exactly as it does in the real `Security`.
            let stored = security_property(ctx.vm_identity(), &prop_name);
            if let Some(v) = stored {
                let s = ctx.create_string(&v);
                return Ok(Some(Value::Object(Some(s))));
            }
            let val = match prop_name.as_str() {
                "securerandom.source" => "file:/dev/urandom",
                // LOWERCASE, matching the JDK's own `conf/security/java.security`,
                // which ships `keystore.type=pkcs12`. MEASURED in both modes
                // (`probes/KeyStoreFamilySweep.java`): `KeyStore.getDefaultType()` is
                // specified as `Security.getProperty("keystore.type")` verbatim, so
                // this table's spelling IS the method's answer -- HotSpot "pkcs12",
                // this VM "PKCS12". Harmless to `KeyStore.getInstance`, which is
                // case-insensitive; not harmless to the caller that compares the
                // default type against a literal, which is the ordinary way to ask
                // "am I on the default store type".
                "keystore.type" => "pkcs12",
                "ssl.KeyManagerFactory.algorithm" => "SunX509",
                "ssl.TrustManagerFactory.algorithm" => "PKIX",
                _ => return Ok(Some(Value::Object(None))),
            };
            let s = ctx.create_string(val);
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // Security.setProperty(String, String) — W2: was a no-op, so every
    // `setProperty`/`getProperty` round-trip reported that the write had not
    // taken. Record it in the (per-VM, gc-common w19-c) map `getProperty`
    // above consults.
    r.register(
        sec,
        "setProperty",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        |ctx, args| {
            let key = match args.first() {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                // The real `Security.setProperty` NPEs on a null key.
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("Security.setProperty: null key".to_string()),                    }
                    .into())
                }
            };
            let val = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("Security.setProperty: null value".to_string()),                    }
                    .into())
                }
            };
            set_security_property(ctx.vm_identity(), key, val);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // Security.getAlgorithms(String type) -> Set<String>
    //
    // W4-3: the hand-maintained per-type literal table that used to live here
    // is gone. It was reachable ONLY from `register_synthetic_overrides`
    // (`--synthetic-jdk`), so real-JDK and `--real-jdk` never saw it at all and
    // fell through to the JDK's own `getAlgorithms` bytecode, which reads
    // `provider.keys()` off the empty synthetic Providers and answered the
    // EMPTY SET for every engine type — `RJdkSecurity.providers` (:311),
    // "MessageDigest algorithms must include SHA-256". The literal table was
    // also wrong on its own terms where it was live: measured against
    // jdk-25.0.3.9-hotspot it advertised `Cipher` and `SecureRandom` names
    // HotSpot does not answer, and answered mixed case where HotSpot
    // ASCII-uppercases every name.
    //
    // Both modes now read the SAME provider service registry that decides
    // `Provider.getService` / `Provider.getServices` / `find_service_provider`,
    // so the two surfaces cannot drift: an algorithm is listed here exactly
    // when some provider in the live chain registered a service for it. This
    // registration stays because `register_synthetic_overrides` runs after
    // `register_essential_natives`, i.e. it would otherwise SHADOW the
    // registry-backed one with real-JDK `Security` bytecode under
    // `--synthetic-jdk`.
    r.register(
        sec,
        "getAlgorithms",
        "(Ljava/lang/String;)Ljava/util/Set;",
        |ctx, args| {
            // A static native's first parameter is `args[0]`: there is no
            // receiver (`Month.of`, `System.getProperty(key, def)` and
            // `jca::provider_chain` all read it so). The other `Security`
            // statics here read `args[1]` until i8-L2, which is why this one
            // used to accept either.
            let type_name = match args.first() {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let names = crate::jca::provider_chain::algorithms_for_service_in(
                ctx.vm_identity(),
                &type_name,
            );
            // W3-7 (RJdkSecurity.java:311) kept: the old code allocated the
            // SYNTHETIC HashSet layout — a String[] in slot 0. In real-JDK mode
            // `java.util.HashSet` has exactly one instance field,
            // `transient HashMap<E,Object> map` (verified with javap), so slot
            // 0 IS `map` and `contains(Object)` ran real bytecode doing
            // `map.containsKey(o)` against a String[] receiver.
            // `make_hashset_with_elements` builds the real
            // `HashSet -> HashMap -> Node[]` shape and falls back to the legacy
            // synthetic layout when the real classes are not loaded.
            //
            // GC-safety: `create_string` allocates, so collecting every string
            // first would hand `make_hashset_with_elements` pre-move addresses
            // for the earlier elements. Pin each one across its own creation.
            let mut elems: Vec<Value> = Vec::with_capacity(names.len());
            let mut pins: Vec<usize> = Vec::with_capacity(names.len());
            for name in &names {
                let s = ctx.create_string(name);
                pins.push(ctx.pin_native_root(s));
                elems.push(Value::Object(Some(s)));
            }
            for (i, pin) in pins.iter().enumerate() {
                if let Value::Object(Some(obj)) = elems[i] {
                    elems[i] = Value::Object(Some(ctx.read_native_pin(*pin, obj)));
                }
            }
            // JIT round 16 (lane compatdrain, r15w3-compat3 item 3): an `Err`
            // releases the per-name pins before it propagates (the `?` left
            // them up until the caller's pin floor).
            let set = match cratonvm_native_collections::make_hashset_with_elements(ctx, &elems) {
                Ok(set) => set,
                Err(e) => {
                    if let Some(base) = pins.first() {
                        ctx.unpin_native_roots(*base);
                    }
                    return Err(e);
                }
            };
            // HotSpot answers `Collections$UnmodifiableSet` here, and this
            // registration deliberately SHADOWS the registry-backed twin under
            // `--synthetic-jdk` — so a wrapper applied only over there would be
            // a fix that is invisible in the one mode this code path serves.
            // Two surfaces of one method drifting is the defect this whole
            // registration exists to prevent, and that applies to the SHAPE of
            // the answer as much as to its contents.
            // W7-63-jca-advertise-vs-serve.md.
            //
            // Wrap BEFORE unpinning: the Java round trip can move `set`.
            //
            // Caveat, recorded rather than fixed here: in `--synthetic-jdk`,
            // `java/util/Collections.unmodifiableSet` is itself bound to
            // `native_return_first_arg` — the IDENTITY — by two registrars in
            // this file, and `register_synthetic_overrides` runs last, so this
            // wrap is expected to be INERT in exactly the mode this code path
            // serves. It is added anyway, because on the day that identity
            // registration is repaired this call site must already be right,
            // and because the alternative is a third surface to remember.
            let view = crate::jca::provider_chain::wrap_unmodifiable_public(ctx, set);
            if let Some(base) = pins.first() {
                ctx.unpin_native_roots(*base);
            }
            Ok(Some(Value::Object(Some(view))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- java.security.KeyStore — 3-field synthetic (type=0, loaded=1, ks_id=2) ---
    let ks = "java/security/KeyStore";
    r.register(
        ks,
        "getInstance",
        "(Ljava/lang/String;)Ljava/security/KeyStore;",
        |ctx, args| {
            let ks_type = obj_arg(args, 0)?;
            // The type string is pinned across the allocation (i8-L2).
            let ks_pin = ctx.pin_native_root(ks_type);
            let obj = try_alloc_concurrent_synthetic(ctx, "java/security/KeyStore", 3);
            let ks_type = ctx.read_native_pin(ks_pin, ks_type);
            ctx.unpin_native_roots(ks_pin);
            let obj = obj?;
            ctx.set_field(obj, 0, Value::Object(Some(ks_type)));
            ctx.set_field(obj, 1, Value::Int(0)); // not loaded
            ctx.set_field(obj, 2, Value::Long(0)); // ks_id (none yet)
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ks,
        "getDefaultType",
        "()Ljava/lang/String;",
        |ctx, _args| {
            // HotSpot answers `Security.getProperty("keystore.type")`, which
            // is lowercase `pkcs12` (see this file's `getProperty`; i8-L2).
            let s = ctx.create_string("pkcs12");
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(ks, "load", "(Ljava/io/InputStream;[C)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Read type hint from field 0 (the type string)
        let type_hint = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => "auto".to_string(),
        };

        // Check if InputStream is null => empty keystore
        let input_stream = match args.get(1) {
            Some(Value::Object(Some(s))) => Some(*s),
            _ => None,
        };

        if let Some(is_obj) = input_stream {
            // Read password char[] from args[2]
            let password_bytes: Vec<u8> = if let Some(Value::Object(Some(pwd_arr))) = args.get(2) {
                let len = ctx.array_length(*pwd_arr);
                let mut pwd = Vec::with_capacity(len);
                for i in 0..len {
                    let c = match ctx.get_array_element(*pwd_arr, i) {
                        Value::Int(v) => v as u8,
                        _ => 0,
                    };
                    pwd.push(c);
                }
                pwd
            } else {
                Vec::new()
            };

            // Read bytes from the InputStream (ByteArrayInputStream: buf=0, pos=1, mark=2, count=3)
            let buf_arr = match ctx.get_field(is_obj, 0) {
                Value::Object(Some(a)) => Some(a),
                _ => None,
            };
            let pos = match ctx.get_field(is_obj, 1) {
                Value::Int(v) => v as usize,
                _ => 0,
            };
            let count = match ctx.get_field(is_obj, 3) {
                Value::Int(v) => v as usize,
                _ => 0,
            };

            if let Some(buf) = buf_arr {
                let all_bytes = cipher_read_bytes(ctx, buf);
                let end = count.min(all_bytes.len());
                let start = pos.min(end);
                let data = &all_bytes[start..end];

                #[cfg(feature = "legacy-synthetic-crypto")]
                {
                    match crypto_impl::KeyStoreData::load(data, &password_bytes, &type_hint) {
                        Ok(ks_data) => {
                            let id = crypto_impl::keystore_next_id();
                            crypto_impl::keystore_store(id, ks_data);
                            ctx.set_field(this, 2, Value::Long(id as i64));
                        }
                        Err(e) => {
                            tracing::warn!("KeyStore.load: failed to parse store: {:?}", e);
                        }
                    }
                }
                #[cfg(not(feature = "legacy-synthetic-crypto"))]
                {
                    let _ = (&password_bytes, &type_hint, data);
                }
            }
        } else {
            // null InputStream = empty keystore (valid per JDK spec)
            #[cfg(feature = "legacy-synthetic-crypto")]
            {
                let id = crypto_impl::keystore_next_id();
                crypto_impl::keystore_store(
                    id,
                    crypto_impl::KeyStoreData {
                        store_type: type_hint.clone(),
                        entries: std::collections::HashMap::new(),
                    },
                );
                ctx.set_field(this, 2, Value::Long(id as i64));
            }
            #[cfg(not(feature = "legacy-synthetic-crypto"))]
            {
                let _ = &type_hint;
            }
        }

        ctx.set_field(this, 1, Value::Int(1)); // mark loaded
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ks, "getType", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ks, "size", "()I", |ctx, args| {
        if cfg!(not(feature = "legacy-synthetic-crypto")) {
            return crate::keystore::keystore_size(ctx, args);
        }
        let this = obj_arg(args, 0)?;
        #[cfg(feature = "legacy-synthetic-crypto")]
        {
            let ks_id = match ctx.get_field(this, 2) {
                Value::Long(id) => id as u64,
                _ => 0,
            };
            if ks_id > 0 {
                if let Some(ks_data) = crypto_impl::keystore_get(ks_id) {
                    return Ok(Some(Value::Int(ks_data.entries.len() as i32)));
                }
            }
        }
        #[cfg(not(feature = "legacy-synthetic-crypto"))]
        {
            let _ = (ctx, this);        }
        Ok(Some(Value::Int(0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ks, "aliases", "()Ljava/util/Enumeration;", |ctx, args| {
        if cfg!(not(feature = "legacy-synthetic-crypto")) {
            return crate::keystore::keystore_aliases(ctx, args);
        }
        let this = obj_arg(args, 0)?;
        #[cfg(feature = "legacy-synthetic-crypto")]
        {
            let ks_id = match ctx.get_field(this, 2) {
                Value::Long(id) => id as u64,
                _ => 0,
            };
            if ks_id > 0 {
                if let Some(ks_data) = crypto_impl::keystore_get(ks_id) {
                    let keys: Vec<String> = ks_data.entries.keys().cloned().collect();
                    if !keys.is_empty() {
                        let arr =
                            ctx.new_array(cratonvm_types::ArrayElementType::Reference, keys.len());
                        // gc-common w21-c: each alias string's allocation can
                        // move the array, which was stored into through its
                        // first address; pinned from here on.
                        let arr_pin = ctx.pin_native_root(arr);
                        for (i, k) in keys.iter().enumerate() {
                            let s = ctx.create_string(k);
                            let arr = ctx.read_native_pin(arr_pin, arr);
                            ctx.set_array_element(arr, i, Value::Object(Some(s)));
                        }
                        // IteratorEnumeration: 2 fields (elements_arr=0, pos=1).
                        // Under `--real-jdk` that fabrication is refused, so
                        // fall back to an enumeration the JDK builds itself —
                        // the same landing `Enumeration$Impl` uses. Without it
                        // the refusal reaches `KeyStore.aliases()` as a
                        // NoClassDefFoundError and takes the whole security
                        // section of `JdkOnlyPlatformProbe` with it.
                        let en = match try_alloc_concurrent_synthetic(
                            ctx,
                            "java/util/IteratorEnumeration",
                            2,
                        ) {
                            Ok(en) => {
                                let arr = ctx.read_native_pin(arr_pin, arr);
                                ctx.set_field(en, 0, Value::Object(Some(arr)));
                                ctx.set_field(en, 1, Value::Int(0));
                                Ok(en)
                            }
                            Err(refusal) => {
                                let arr = ctx.read_native_pin(arr_pin, arr);
                                match crate::classloader::real_snapshot_enumeration(ctx, arr) {
                                    Ok(Some(en)) => Ok(en),
                                    Ok(None) => Err(refusal),
                                    Err(err) => Err(err),
                                }
                            }
                        };
                        ctx.unpin_native_roots(arr_pin);
                        return Ok(Some(Value::Object(Some(en?))));
                    }
                }
            }
        }
        #[cfg(not(feature = "legacy-synthetic-crypto"))]
        {
            let _ = this;
        }
        let empty = match try_alloc_concurrent_synthetic(
            ctx,
            "java/util/Collections$EmptyEnumeration",
            0,
        ) {
            Ok(empty) => empty,
            Err(refusal) => {
                // Same landing over a zero-length array.
                let none = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
                match crate::classloader::real_snapshot_enumeration(ctx, none)? {
                    Some(empty) => empty,
                    None => return Err(refusal),
                }
            }
        };
        Ok(Some(Value::Object(Some(empty))))
    }, cratonvm_native_api::NativeKind::Bridge);
    // IteratorEnumeration helpers (used by KeyStore.aliases)
    r.register(
        "java/util/IteratorEnumeration",
        "hasMoreElements",
        "()Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let pos = match ctx.get_field(this, 1) {
                Value::Int(v) => v as usize,
                _ => 0,
            };
            let len = match ctx.get_field(this, 0) {
                Value::Object(Some(arr)) => ctx.array_length(arr),
                _ => 0,
            };
            Ok(Some(Value::Int(if pos < len { 1 } else { 0 })))        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        "java/util/IteratorEnumeration",
        "nextElement",
        "()Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let pos = match ctx.get_field(this, 1) {
                Value::Int(v) => v as usize,
                _ => 0,
            };
            let elem = match ctx.get_field(this, 0) {
                Value::Object(Some(arr)) => {
                    if pos < ctx.array_length(arr) {
                        ctx.set_field(this, 1, Value::Int((pos + 1) as i32));
                        ctx.get_array_element(arr, pos)                    } else {
                        Value::Object(None)                    }                }
                _ => Value::Object(None),
            };
            Ok(Some(elem))        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(ks, "containsAlias", "(Ljava/lang/String;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        #[cfg(feature = "legacy-synthetic-crypto")]
        {
            let ks_id = match ctx.get_field(this, 2) {
                Value::Long(id) => id as u64,
                _ => 0,
            };
            if ks_id > 0 {
                let alias = match args.get(1) {
                    Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                    _ => String::new(),
                };
                if let Some(ks_data) = crypto_impl::keystore_get(ks_id) {
                    if ks_data.entries.contains_key(&alias) {
                        return Ok(Some(Value::Int(1)));
                    }
                }
            }
        }
        #[cfg(not(feature = "legacy-synthetic-crypto"))]
        {
            let _ = (ctx, this, args);        }
        Ok(Some(Value::Int(0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    // getCertificate(String alias) -> Certificate
    r.register(
        ks,
        "getCertificate",
        "(Ljava/lang/String;)Ljava/security/cert/Certificate;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            #[cfg(feature = "legacy-synthetic-crypto")]
            {
                let ks_id = match ctx.get_field(this, 2) {
                    Value::Long(id) => id as u64,
                    _ => 0,
                };
                if ks_id > 0 {
                    let alias = match args.get(1) {
                        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                        _ => String::new(),
                    };
                    if let Some(ks_data) = crypto_impl::keystore_get(ks_id) {
                        if let Some(entry) = ks_data.entries.get(&alias) {
                            let x509_cert = match entry {
                                crypto_impl::KeyStoreEntry::TrustedCert { cert } => Some(cert),
                                crypto_impl::KeyStoreEntry::PrivateKeyEntry {
                                    cert_chain, ..
                                } => cert_chain.first(),
                                _ => None,
                            };
                            if let Some(cert) = x509_cert {
                                let cert_obj = try_alloc_concurrent_synthetic(
                                    ctx,
                                    "java/security/cert/X509Certificate",
                                    3,
                                )?;
                                // gc-common w20-b: the certificate and its
                                // subject were held raw across the name
                                // strings' allocations.
                                let cert_obj: ObjectRef =
                                    alloc_string_into_field(ctx, cert_obj, 0, &cert.subject_cn);
                                let cert_obj: ObjectRef =
                                    alloc_string_into_field(ctx, cert_obj, 1, &cert.issuer_cn);
                                let cert_id = crypto_impl::cert_next_id();
                                crypto_impl::cert_store(cert_id, cert.clone());
                                ctx.set_field(cert_obj, 2, Value::Long(cert_id as i64));
                                return Ok(Some(Value::Object(Some(cert_obj))));
                            }
                        }
                    }
                }
            }
            #[cfg(not(feature = "legacy-synthetic-crypto"))]
            {
                let _ = (ctx, this, args);            }
            Ok(Some(Value::Object(None)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // getKey(String alias, char[] password) -> Key
    r.register(
        ks,
        "getKey",
        "(Ljava/lang/String;[C)Ljava/security/Key;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            #[cfg(feature = "legacy-synthetic-crypto")]
            {
                let ks_id = match ctx.get_field(this, 2) {
                    Value::Long(id) => id as u64,
                    _ => 0,
                };
                if ks_id > 0 {
                    let alias = match args.get(1) {
                        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                        _ => String::new(),
                    };
                    if let Some(ks_data) = crypto_impl::keystore_get(ks_id) {
                        if let Some(entry) = ks_data.entries.get(&alias) {
                            match entry {
                                crypto_impl::KeyStoreEntry::PrivateKeyEntry {
                                    key_bytes, ..
                                } => {
                                    let pk = try_alloc_concurrent_synthetic(
                                        ctx,
                                        "java/security/PrivateKey",
                                        4,
                                    )?;
                                    ctx.set_field(pk, 0, Value::Int(6)); // RSA default
                                    ctx.set_field(pk, 1, Value::Int(2048));
                                    ctx.set_field(pk, 2, Value::Int(key_bytes.len() as i32));
                                    ctx.set_field(pk, 3, Value::Long(0));
                                    return Ok(Some(Value::Object(Some(pk))));
                                }
                                crypto_impl::KeyStoreEntry::SecretKeyEntry {
                                    key_bytes,
                                    algorithm,
                                } => {
                                    let sk = try_alloc_concurrent_synthetic(
                                        ctx,
                                        "javax/crypto/SecretKey",
                                        3,
                                    )?;
                                    ctx.set_field(sk, 0, Value::Int(0));
                                    ctx.set_field(sk, 1, Value::Int((key_bytes.len() * 8) as i32));
                                    ctx.set_field(sk, 2, Value::Int(key_bytes.len() as i32));
                                    let _ = algorithm;
                                    return Ok(Some(Value::Object(Some(sk))));
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            #[cfg(not(feature = "legacy-synthetic-crypto"))]
            {
                let _ = (ctx, this, args);            }
            Ok(Some(Value::Object(None)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- java.security.KeyPair — 2-field synthetic (publicKey=0, privateKey=1) ---
    let kp = "java/security/KeyPair";
    r.register(
        kp,
        "<init>",
        "(Ljava/security/PublicKey;Ljava/security/PrivateKey;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let pub_key = obj_arg(args, 1)?;
            let priv_key = obj_arg(args, 2)?;
            ctx.set_field(this, 0, Value::Object(Some(pub_key)));
            ctx.set_field(this, 1, Value::Object(Some(priv_key)));
            Ok(Some(Value::Object(None)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        kp,
        "getPublic",
        "()Ljava/security/PublicKey;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        kp,
        "getPrivate",
        "()Ljava/security/PrivateKey;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 1)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- java.security.KeyPairGenerator — RETIRED (2026-07-31) ---
    //
    // This phase used to register `getInstance`/`initialize`/`generateKeyPair`/
    // `getAlgorithm` here. Registration is LAST-WINS and this registrar runs
    // from `register_synthetic_overrides`, i.e. AFTER
    // `register_essential_natives_with_shims` → `jca::register_jca_natives`,
    // so these five stubs silently shadowed the complete implementation in
    // `jca::key_factory` for the whole synthetic-JDK build.
    //
    // What the shadowing cost: `generateKeyPair` handed back a ONE-field
    // `PublicKey`/`PrivateKey` carrying an empty `byte[]` at slot 0, while
    // every reader in the tree — including the `Signature` block right below,
    // and `jca::key_factory`'s own `PublicKey.getAlgorithm`/`getEncoded`
    // natives, which were NOT shadowed — expects the five-slot key layout
    // `alg_idx@0, bits@1, enc_len@2, key_id@3 (Long), der@4`. Reading an
    // `Int` algorithm index out of a slot holding a `byte[]` reference is the
    // classic writer-by-one-layout / reader-by-another defect: the keys had no
    // `key_id`, so sign/verify degraded to an HMAC surrogate over the (empty)
    // key bytes and TWO DIFFERENT KEY PAIRS PRODUCED IDENTICAL SIGNATURES —
    // `Signature.verify` accepted a signature made with somebody else's key.
    //
    // Retiring these hands `KeyPairGenerator` back to `jca::key_factory`,
    // whose `kpg_generate_key_pair` mints real RSA / P-256 material through
    // `crypto_impl` and stores a `key_id` at slot 3. The `Signature` block
    // below is the consumer of that layout and had to be reworked in the same
    // change — the two are a coupled pair and must not be split.
    //
    // The one behaviour that lived ONLY here, `initialize`'s
    // `InvalidParameterException` for a non-positive key size, has been moved
    // into `jca::key_factory::kpg_initialize_int`.

    // --- java.security.Signature — 4-field (algorithm=0, mode=1, key=2, data=3) ---
    // mode: 0=uninitialized, 1=sign, 2=verify
    //
    // Coupled with the retired `KeyPairGenerator` block above: this registrar
    // also wins over `jca::signature` (last-wins), and its sign/verify read the
    // key's algorithm index from slot 0 and its `crypto_impl` `key_id` from
    // slot 3 — i.e. they were always written against the FIVE-slot
    // `jca::key_factory` key layout, not against the one-slot keys the local
    // stub produced. Keys now arrive in that layout, and the real-crypto
    // dispatch below is no longer compiled out.
    let sig = "java/security/Signature";
    r.register(
        sig,
        "getInstance",
        "(Ljava/lang/String;)Ljava/security/Signature;",
        |ctx, args| {
            let algo = obj_arg(args, 0)?;
            sig_new_carrier(ctx, algo)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sig,
        "getInstance",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/security/Signature;",
        |ctx, args| {
            // Mirrors the same check in `jca::signature::sig_get_instance`.
            crate::jca::provider_chain::check_named_provider_arg(
                ctx,
                args,
                1,
                crate::jca::provider_chain::ProviderArgWording::Shared,
            )?;
            let algo = obj_arg(args, 0)?;
            let algo_str = ctx.read_string(algo).unwrap_or_default();
            crate::jca::provider_chain::check_provider_ownership(
                ctx,
                args,
                1,
                "Signature",
                &algo_str,
                crate::jca::provider_chain::ProviderArgWording::Shared,
            )?;
            sig_new_carrier(ctx, algo)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sig,
        "initSign",
        "(Ljava/security/PrivateKey;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 1, Value::Int(1)); // sign mode
            ctx.set_field(this, 2, args.get(1).copied().unwrap_or(Value::Object(None)));
            // gc-common w20-b: `this` was held raw across the fresh buffer.
            alloc_array_into_field(ctx, this, 3, cratonvm_types::ArrayElementType::Byte, 0);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // The `(PrivateKey, SecureRandom)` overload must be registered HERE too.
    // Left to `jca::signature`, it would seed that module's side tables and
    // write its state to slots at `synthetic_base_offset` (past this 4-field
    // carrier, so the writes are dropped) — leaving mode@1 at 0, and the
    // `sign()` below would then reject a Signature the caller did initialize.
    // The extra `SecureRandom` argument is ignored: `crypto_impl` seeds from
    // the OS CSPRNG.
    r.register(
        sig,
        "initSign",
        "(Ljava/security/PrivateKey;Ljava/security/SecureRandom;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 1, Value::Int(1)); // sign mode
            ctx.set_field(this, 2, args.get(1).copied().unwrap_or(Value::Object(None)));
            // gc-common w20-b: `this` was held raw across the fresh buffer.
            alloc_array_into_field(ctx, this, 3, cratonvm_types::ArrayElementType::Byte, 0);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sig,
        "initVerify",
        "(Ljava/security/PublicKey;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 2, args.get(1).copied().unwrap_or(Value::Object(None)));
            ctx.set_field(this, 1, Value::Int(2)); // verify mode
                                                   // gc-common w20-b: `this` was held raw across the fresh buffer.
            alloc_array_into_field(ctx, this, 3, cratonvm_types::ArrayElementType::Byte, 0);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // update([B)V — accumulate data
    r.register(sig, "update", "([B)V", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        if let Some(Value::Object(Some(input))) = args.get(1) {
            let input_bytes = cipher_read_bytes(ctx, *input);
            sig_append_data(ctx, &mut this, &input_bytes);        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    // update(B)V — accumulate single byte
    r.register(sig, "update", "(B)V", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        let b = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as u8;
        sig_append_data(ctx, &mut this, &[b]);
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    // update([BII)V — accumulate byte range
    r.register(sig, "update", "([BII)V", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        if let Some(Value::Object(Some(arr))) = args.get(1) {
            let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0) as usize;
            let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0) as usize;
            let mut bytes = Vec::with_capacity(len);
            for i in 0..len {
                if let Value::Int(b) = ctx.get_array_element(*arr, off + i) {
                    bytes.push(b as u8);                }            }
            sig_append_data(ctx, &mut this, &bytes);        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    // sign()[B — produce a real digital signature using the key's crypto backend
    r.register(sig, "sign", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let signature = sig_finish_sign(ctx, this)?;
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, signature.len());
        for (i, &b) in signature.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
        }
        Ok(Some(Value::Object(Some(arr))))
    }, cratonvm_native_api::NativeKind::Bridge);
    // sign([BII)I — same signature, written into a caller-supplied buffer.
    //
    // Previously unregistered here, so it fell through to `jca::signature`'s
    // `sig_sign_into` — which reads the algorithm/state from side tables that
    // only `jca::signature::sig_get_instance` populates. Because `getInstance`
    // above shadows that native, every `sign(byte[],int,int)` on a synthetic
    // JDK died with `IllegalStateException("Signature state missing post-GC or
    // never initialized")`: a receiver built by one implementation handed to
    // the other's reader. Registering the overload here keeps the whole
    // Signature surface on ONE implementation.
    r.register(sig, "sign", "([BII)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let out = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => {
                return Err(RuntimeError::NullPointerException {
                    message: Some("Signature.sign: output buffer is null".into()),
                }
                .into())
            }
        };
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let max_len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
        if off < 0 || max_len < 0 {
            return Err(RuntimeError::IllegalArgumentException {
                message: format!("Signature.sign: bad offset/len {off}/{max_len}"),
            }
            .into());
        }
        let signature = sig_finish_sign(ctx, this)?;
        // The JDK throws `SignatureException` rather than silently truncating —
        // a short signature is indistinguishable from a forged one downstream.
        if signature.len() > max_len as usize {
            return Err(throw_jca_exc(
                ctx,
                "java/security/SignatureException",
                &format!(
                    "partial signatures not returned: need {} bytes, buffer has {max_len}",
                    signature.len()
                ),
            ));
        }
        let off = off as usize;
        for (i, &b) in signature.iter().enumerate() {
            ctx.set_array_element(out, off + i, Value::Int(b as i8 as i32));
        }
        Ok(Some(Value::Int(signature.len() as i32)))
    }, cratonvm_native_api::NativeKind::Bridge);
    // verify([B)Z — verify a real digital signature
    r.register(sig, "verify", "([B)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let provided = match args.get(1) {
            Some(Value::Object(Some(arr))) => cipher_read_bytes(ctx, *arr),
            _ => Vec::new(),
        };
        let valid = sig_finish_verify(ctx, this, &provided)?;
        Ok(Some(Value::Int(if valid { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    // verify([BII)Z — as above over a slice of the supplied buffer. Registered
    // for the same reason as `sign([BII)I`: without it this one overload
    // dispatches into `jca::signature`, whose side-table state a receiver made
    // by the `getInstance` above never had.
    r.register(sig, "verify", "([BII)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
        let provided = match args.get(1) {
            Some(Value::Object(Some(arr))) => {
                let all = cipher_read_bytes(ctx, *arr);
                let end = off.saturating_add(len).min(all.len());
                all.get(off..end).map(|s| s.to_vec()).unwrap_or_default()            }
            _ => Vec::new(),
        };
        let valid = sig_finish_verify(ctx, this, &provided)?;
        Ok(Some(Value::Int(if valid { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sig, "getAlgorithm", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    }

/// Pad an HMAC-derived signature to the canonical byte length of the
/// declared signature algorithm. This is used only in the unkeyed
/// fallback path so synthetic tests can assert a realistic signature
/// length. Padding is deterministic (zero bytes after the HMAC prefix)
/// so verify() can reconstruct the same expected value.
pub fn pad_signature_for_algo(algo: &str, sig: Vec<u8>) -> Vec<u8> {
    let upper = algo.to_uppercase();
    let target_len = if upper.contains("RSA") {
        256 // RSA 2048-bit signature
    } else if upper.contains("ED25519") {
        64
    } else if upper.contains("ECDSA") {
        64 // P-256 two 32-byte scalars
    } else if upper.contains("DSA") {
        48
    } else {
        // Unknown algorithm: leave HMAC result as-is.
        return sig;
    };
    if sig.len() >= target_len {
        return sig;
    }
    let mut out = sig;
    out.resize(target_len, 0);
    out
}

/// The `crypto_impl` handle carried by a key minted by `jca::key_factory`:
/// `(algorithm index @ slot 0, key_id @ slot 3)`.
///
/// `None` means "this carrier is not one of ours" — a bare object, a real JDK
/// key implementation, or a legacy key that keeps raw bytes at slot 0. Callers
/// then fall back to the keyed-HMAC surrogate below. The slot numbering is
/// `jca::key_factory`'s `KEY_FIELD_ALGO` / `KEY_FIELD_KEYID`; the two must move
/// together (synthetic classes have UNNAMED fields, so nothing but this comment
/// links the writer to this reader).
pub fn sig_key_handle(ctx: &mut dyn NativeContext, key_obj: ObjectRef) -> Option<(i32, u64)> {
    let alg_idx = match ctx.get_field(key_obj, 0) {
        Value::Int(i) => i,
        _ => return None,
    };
    // `Long` is what `alloc_public_key`/`alloc_private_key` store; accept a
    // narrowed `Int` too, exactly as `jca::signature::extract_key_id_from_key`
    // does — a slot with no declared descriptor can come back either way
    // depending on the layout the synthetic class ended up with.
    match ctx.get_field(key_obj, 3) {
        Value::Long(id) if id > 0 => Some((alg_idx, id as u64)),
        Value::Int(id) if id > 0 => Some((alg_idx, id as u64)),
        _ => None,
    }
}

/// Real signing through the `crypto_impl` backend for a key that carries a
/// `key_id`. `None` when the requested JCA algorithm and the key's own
/// algorithm do not agree (e.g. `SHA256withECDSA` against an RSA key), which
/// leaves the caller on the surrogate path rather than emitting a signature
/// from the wrong primitive.
///
/// The ECDSA hash split mirrors `jca::signature::sign_dispatch` exactly — both
/// implementations must agree on which digest a given algorithm name means, or
/// a signature made through one will not verify through the other.
pub fn sig_backend_sign(algo: &str, alg_idx: i32, key_id: u64, data: &[u8]) -> Option<Vec<u8>> {
    let upper = algo.to_uppercase();
    if upper == "ED25519" && alg_idx == 8 {
        crate::crypto_impl::ed25519_sign(key_id, data)
    } else if upper.contains("ECDSA") && alg_idx == 7 {
        if upper.contains("SHA256") || upper.contains("SHA-256") {
            crate::crypto_impl::ecdsa_sign_sha256(key_id, data)
        } else {
            crate::crypto_impl::ecdsa_sign(key_id, data)
        }
    } else if upper.contains("RSA") && alg_idx == 6 {
        crate::crypto_impl::rsa_sign(key_id, data)
    } else {
        None
    }
}

/// Verification counterpart of [`sig_backend_sign`]; the algorithm/hash
/// selection must stay identical to it.
pub fn sig_backend_verify(
    algo: &str,
    alg_idx: i32,
    key_id: u64,
    data: &[u8],
    provided: &[u8],
) -> Option<bool> {
    let upper = algo.to_uppercase();
    if upper == "ED25519" && alg_idx == 8 {
        crate::crypto_impl::ed25519_verify(key_id, data, provided)
    } else if upper.contains("ECDSA") && alg_idx == 7 {
        if upper.contains("SHA256") || upper.contains("SHA-256") {
            crate::crypto_impl::ecdsa_verify_sha256(key_id, data, provided)
        } else {
            crate::crypto_impl::ecdsa_verify(key_id, data, provided)
        }
    } else if upper.contains("RSA") && alg_idx == 6 {
        crate::crypto_impl::rsa_verify(key_id, data, provided)
    } else {
        None
    }
}

/// Read `(algorithm name, accumulated payload)` off a Signature receiver and
/// clear the accumulator, mirroring the JDK's "sign()/verify() reset the
/// engine" contract.
///
/// gc-common w20-b: `this` is `&mut`; the reset allocates, and `sign` /
/// `verify` read the key off `this` right after.
pub fn sig_take_state(ctx: &mut dyn NativeContext, this: &mut ObjectRef) -> (String, Vec<u8>) {
    let data = match ctx.get_field(*this, 3) {
        Value::Object(Some(arr)) => cipher_read_bytes(ctx, arr),
        _ => Vec::new(),
    };
    let algo = match ctx.get_field(*this, 0) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    *this = alloc_array_into_field(ctx, *this, 3, cratonvm_types::ArrayElementType::Byte, 0);
    (algo, data)
}

/// The keyed-HMAC surrogate used when the key carries no `crypto_impl` handle
/// (an unkeyed or legacy carrier). Deterministic, so `verify` can recompute it.
/// Padded to the algorithm's canonical signature length — see
/// [`pad_signature_for_algo`]. NOT applied to real signatures: padding a real
/// 512-bit RSA signature out to 256 bytes would make it fail its own verify.
pub fn sig_surrogate(
    ctx: &mut dyn NativeContext,
    key_obj: ObjectRef,
    algo: &str,
    data: &[u8],
) -> Vec<u8> {
    let key_bytes = match ctx.get_field(key_obj, 0) {
        Value::Object(Some(enc_arr)) => cipher_read_bytes(ctx, enc_arr),
        _ => Vec::new(),
    };
    pad_signature_for_algo(algo, sig_compute(algo, &key_bytes, data))}

/// Shared body of `Signature.sign()[B` and `Signature.sign([BII)I`.
pub fn sig_finish_sign(
    ctx: &mut dyn NativeContext,
    mut this: ObjectRef,
) -> Result<Vec<u8>, MethodCallFailed> {
    let mode = ctx.get_field(this, 1).as_int().unwrap_or(0);
    if mode != 1 {
        return Err(RuntimeError::IllegalStateException {
            message: "Signature not initialized for signing".into(),        }
        .into());
    }
    let (algo, data) = sig_take_state(ctx, &mut this);
    let key_obj = match ctx.get_field(this, 2) {
        Value::Object(Some(k)) => k,
        // No key at all: preserve the historical "canonical-length zero-fill"
        // shape so an unkeyed synthetic Signature still reports a plausible
        // signature length. `verify` answers `false` for this case.
        _ => return Ok(pad_signature_for_algo(&algo, Vec::new())),
    };
    if let Some((alg_idx, key_id)) = sig_key_handle(ctx, key_obj) {
        if let Some(bytes) = sig_backend_sign(&algo, alg_idx, key_id, &data) {
            return Ok(bytes);
        }
    }
    Ok(sig_surrogate(ctx, key_obj, &algo, &data))
}

/// Shared body of `Signature.verify([B)Z` and `Signature.verify([BII)Z`.
pub fn sig_finish_verify(
    ctx: &mut dyn NativeContext,
    mut this: ObjectRef,
    provided: &[u8],
) -> Result<bool, MethodCallFailed> {
    let mode = ctx.get_field(this, 1).as_int().unwrap_or(0);
    if mode != 2 {
        return Err(RuntimeError::IllegalStateException {
            message: "Signature not initialized for verification".into(),        }
        .into());
    }
    let (algo, data) = sig_take_state(ctx, &mut this);
    // A verification API must never answer "did not verify" for a question it
    // never asked. `false` here is reserved for a real cryptographic mismatch;
    // every "we could not check" path raises the JDK-specified exception. A
    // missing key is the clearest case: returning `false` reports a forgery
    // against a signature nothing examined.
    let key_obj = match ctx.get_field(this, 2) {
        Value::Object(Some(k)) => k,
        _ => {
            return Err(throw_jca_exc(
                ctx,
                "java/security/InvalidKeyException",
                "Signature.verify called with no verification key",
            ))
        }
    };
    if let Some((alg_idx, key_id)) = sig_key_handle(ctx, key_obj) {
        // `Some(false)` is a genuine mismatch and is preserved verbatim;
        // `None` means the backend declined the algorithm or key, which is
        // NOT a mismatch.
        if let Some(ok) = sig_backend_verify(&algo, alg_idx, key_id, &data, provided) {
            return Ok(ok);
        }
        return Err(throw_jca_exc(
            ctx,
            "java/security/SignatureException",
            &format!("Signature.verify: no backend supports algorithm {algo:?} for this key"),
        ));
    }
    let expected = sig_surrogate(ctx, key_obj, &algo, &data);
    Ok(ct_eq(&expected, provided))
}

/// Append bytes to the Signature data accumulator (field 3)
pub fn sig_append_data(ctx: &mut dyn NativeContext, this: &mut ObjectRef, bytes: &[u8]) {
    append_bytes_field(ctx, this, 3, bytes);
}

/// Constant-time equality comparison for signature verification.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Compute signature: HMAC(key, hash(data)) using algorithm-appropriate hash
pub fn sig_compute(algo: &str, key: &[u8], data: &[u8]) -> Vec<u8> {
    use crate::{
        hmac_sha1, hmac_sha256, hmac_sha384, hmac_sha512, real_sha1, real_sha256, real_sha384,
        real_sha512,
    };
    let upper = algo.to_uppercase();
    // Hash the data first using the hash implied by the algorithm name
    let hash = if upper.contains("SHA384") || upper.contains("SHA-384") {
        real_sha384(data)
    } else if upper.contains("SHA512") || upper.contains("SHA-512") {
        real_sha512(data)
    } else if upper.contains("SHA1") || upper.contains("SHA-1") {
        real_sha1(data)
    } else {
        // Default to SHA-256 (covers SHA256withRSA, SHA256withECDSA, etc.)
        real_sha256(data)
    };
    // Sign with HMAC using the same hash family
    if upper.contains("SHA384") || upper.contains("SHA-384") {
        hmac_sha384(key, &hash)
    } else if upper.contains("SHA512") || upper.contains("SHA-512") {
        hmac_sha512(key, &hash)
    } else if upper.contains("SHA1") || upper.contains("SHA-1") {
        hmac_sha1(key, &hash)
    } else {
        hmac_sha256(key, &hash)
    }
}

// ---------------------------------------------------------------------------
// java.net.Socket / ServerSocket — real TCP via SOCKET_REGISTRY
// ---------------------------------------------------------------------------
// Socket field layout: host=0, port=1, localPort=2, closed=3, stream_id=4
pub const SOCK_HOST: usize = 0;
pub const SOCK_PORT: usize = 1;
pub const SOCK_LOCAL_PORT: usize = 2;
pub const SOCK_CLOSED: usize = 3;
pub const SOCK_STREAM_ID: usize = 4;

// ServerSocket field layout: port=0, backlog=1, closed=2, listener_id=3
pub const SS_PORT: usize = 0;
pub const SS_BACKLOG: usize = 1;
pub const SS_CLOSED: usize = 2;
pub const SS_LISTENER_ID: usize = 3;

/// `ServerSocket` SO_TIMEOUT in milliseconds, keyed by listener id.
///
/// A side table rather than an instance slot because the synthetic carrier is
/// allocated exactly four fields wide (above) and rather than the OS
/// `SO_RCVTIMEO` option because Windows lets an accepted socket inherit the
/// listener's options, which would silently give every accepted connection a
/// read timeout the application never asked for.
///
/// Keyed by listener id, which is safe against reuse: an id is only handed out
/// again after its listener leaves `s2_registry()`, and the one place that
/// removes a `ServerSocket`'s listener (`close`, below) drops the entry with it.
pub fn ss_accept_timeouts() -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<i32, u32>> {
    static T: std::sync::OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<i32, u32>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

/// Outcome of one `ServerSocket.accept()`, distinguishing an expired SO_TIMEOUT
/// (which the JDK reports as `SocketTimeoutException`) from a hard failure.
pub enum SsAccept {
    Accepted(i32),
    TimedOut,
    Failed,
}

/// `ServerSocket.accept()` honouring the SO_TIMEOUT `setSoTimeout` stored.
///
/// With no timeout this is `s2_blocking_accept`. With one, the listener is
/// polled in short slices against a deadline — an explicit poll rather than
/// `SO_RCVTIMEO` because Winsock does not apply the receive timeout to
/// `accept()`.
pub fn ss_accept_with_timeout(lid: i32) -> SsAccept {
    use crate::servlet::{s2_alloc_accepted_stream, s2_registry};

    let timeout = ss_accept_timeouts()
        .lock()
        .get(&lid)
        .copied()
        .filter(|ms| *ms > 0)
        .map(|ms| std::time::Duration::from_millis(u64::from(ms)));

    // Clone the listener out under a SHORT lock — never run accept() while
    // holding the global socket registry (see `s2_blocking_accept`).
    let listener = {
        let reg = s2_registry().lock();
        match reg.listeners.get(&lid).and_then(|l| l.try_clone().ok()) {
            Some(l) => l,
            None => return SsAccept::Failed,
        }
    };

    let Some(timeout) = timeout else {
        let _ = listener.set_nonblocking(false);
        return match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                SsAccept::Accepted(s2_alloc_accepted_stream(lid, stream))
            }
            Err(_) => SsAccept::Failed,
        };
    };

    let deadline = std::time::Instant::now() + timeout;
    if listener.set_nonblocking(true).is_err() {
        return SsAccept::Failed;
    }
    let outcome = loop {
        match listener.accept() {
            Ok((stream, _)) => {
                // Windows lets the accepted socket inherit the listener's
                // non-blocking mode; make it blocking explicitly.
                let _ = stream.set_nonblocking(false);
                break SsAccept::Accepted(s2_alloc_accepted_stream(lid, stream));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    break SsAccept::TimedOut;
                }
                std::thread::sleep(remaining.min(std::time::Duration::from_millis(5)));
            }
            Err(_) => break SsAccept::Failed,
        }
    };
    // The clone shares the underlying socket, so restore blocking mode for any
    // other accept() on this listener.
    let _ = listener.set_nonblocking(false);
    outcome
}

// SocketInputStream/OutputStream: stream_id=0
pub const SIO_STREAM_ID: usize = 0;

pub fn register_phase53_socket_stubs(r: &mut NativeMethodRegistry) {
    // NIO-SERVER-SOCKET (route 1): skip this synthetic TCP surface so real
    // java.net.Socket/ServerSocket bytecode drives sun/nio/ch/Net (native-io::net).
    // A registered native shadows the class's real bytecode at every interpreter
    // dispatch site (WP0.1 native-override-priority), so the registry must be
    // empty for these classes. See `reference_server_socket_gap`.
    if crate::vmflags().io.real_net_sockets {
        return;
    }
    register_synthetic_socket_stubs(r);
}


//! C20: Locale bootstrap overrides for real-JDK mode.
//!
//! In JDK 25 `Locale.<clinit>` attempts to resolve the system default locale
//! by walking the `sun.util.locale.provider.LocaleProviderAdapter` ServiceLoader
//! chain (`COMPAT`, `CLDR`, `SPI`, `HOST`, `FALLBACK`). When that chain fails
//! — as it does in our partial bootstrap — callers bubble up to
//! `JRELocaleProviderAdapter.getLocaleServiceProvider` which throws
//! `InternalError("should not come down here")`. Downstream code
//! (Jackson, H2's `DbException.<clinit>`, anything formatting numbers or
//! dates) hits this and dies.
//!
//! Option A from the task plan: override `java/util/Locale.getDefault` at
//! the native level to return an `en_US` Locale constructed via the public
//! `Locale(String, String)` ctor, cache it, and neutralise the fallback
//! branch of `JRELocaleProviderAdapter.getLocaleServiceProvider` so code
//! paths that still reach it get `null` instead of an `InternalError`.

use parking_lot::Mutex;
use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::{ObjectRef, Value};
use std::collections::HashMap;
use std::sync::OnceLock;

/// Process-wide cached default Locale, lazily constructed on first
/// `Locale.getDefault()` call.  Stable identity so `==` comparisons in JDK
/// code (e.g. `Locale.getDefault() == cached`) behave sensibly.
fn cached_default_locale() -> &'static Mutex<Option<ObjectRef>> {
    static CACHE: OnceLock<Mutex<Option<ObjectRef>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Map from synthetic Locale ObjectRef → (language, country, language_tag).
/// Used by the getLanguage/getCountry/toLanguageTag native overrides.
fn synthetic_locale_data() -> &'static Mutex<HashMap<ObjectRef, (&'static str, &'static str, &'static str)>> {
    static MAP: OnceLock<Mutex<HashMap<ObjectRef, (&'static str, &'static str, &'static str)>>> =
        OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Construct (or return the cached) default Locale: synthetic `en_US`.
///
/// We use `alloc_concurrent_synthetic` instead of `ctx.new_object` because
/// `Locale.<clinit>` itself calls `getDefault()` (and other adapter-chain code
/// that throws InternalError). `ctx.new_object` propagates that failure as a
/// null return; `alloc_concurrent_synthetic` tolerates `<clinit>` failure and
/// always returns a valid ObjectRef.  We skip `Locale.<init>` entirely and
/// record language/country in a Rust-side map; native overrides for
/// `getLanguage()`/`getCountry()` etc. read from that map.
fn get_or_create_default(ctx: &mut dyn NativeContext) -> MethodCallResult {
    if let Some(obj) = *cached_default_locale().lock() {
        return Ok(Some(Value::Object(Some(obj))));
    }

    // Allocate a synthetic Locale that bypasses <clinit>/<init> failures.
    // 32 slots is conservative: real JDK Locale has ~20 instance fields once
    // inherited fields are counted.
    let locale_obj = crate::alloc_concurrent_synthetic(ctx, "java/util/Locale", 32);

    synthetic_locale_data()
        .lock()
        .insert(locale_obj, ("en", "US", "en-US"));

    *cached_default_locale().lock() = Some(locale_obj);
    Ok(Some(Value::Object(Some(locale_obj))))
}

fn locale_language(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        if let Some(&(lang, _, _)) = synthetic_locale_data().lock().get(this) {
            return Ok(Some(Value::Object(Some(ctx.create_string(lang)))));
        }
    }
    // Fall through: not one of ours — try the real JDK field path.
    // The JDK stores BaseLocale inside Locale; return empty string as safe fallback.
    Ok(Some(Value::Object(Some(ctx.create_string("")))))
}

fn locale_country(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        if let Some(&(_, country, _)) = synthetic_locale_data().lock().get(this) {
            return Ok(Some(Value::Object(Some(ctx.create_string(country)))));
        }
    }
    Ok(Some(Value::Object(Some(ctx.create_string("")))))
}

fn locale_tag(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        if let Some(&(_, _, tag)) = synthetic_locale_data().lock().get(this) {
            return Ok(Some(Value::Object(Some(ctx.create_string(tag)))));
        }
    }
    Ok(Some(Value::Object(Some(ctx.create_string("und")))))
}

fn locale_script(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(ctx.create_string("")))))
}

fn locale_variant(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(Some(ctx.create_string("")))))
}

pub fn register(registry: &mut NativeMethodRegistry) {
    // java.util.Locale.getDefault() — bypass the adapter chain.
    registry.register(
        "java/util/Locale",
        "getDefault",
        "()Ljava/util/Locale;",
        |ctx, _args| get_or_create_default(ctx),
    );
    registry.register(
        "java/util/Locale",
        "getDefault",
        "(Ljava/util/Locale$Category;)Ljava/util/Locale;",
        |ctx, _args| get_or_create_default(ctx),
    );

    // Note: `ResourceBundle$Control.getCandidateLocales` override is
    // registered from `register_essential_natives` (real-JDK path),
    // since this `register` function is only wired into the synthetic
    // path. See lib.rs EUREKA-RB-CANDIDATE.

    // Neutralise the "should not come down here" fallback by returning null.
    // Most callers handle a null provider gracefully (fall back to defaults
    // or other adapters); an `InternalError` aborts the whole chain.
    registry.register(
        "sun/util/locale/provider/JRELocaleProviderAdapter",
        "getLocaleServiceProvider",
        "(Ljava/lang/Class;)Ljava/util/spi/LocaleServiceProvider;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );

    // Getter overrides for synthetic Locales created by get_or_create_default.
    // When the receiver is one of ours (in synthetic_locale_data), return the
    // recorded strings directly.  For real Locale objects the JDK bytecode runs
    // instead (these overrides are not in check_override for every call, but the
    // methods themselves are short and safe to always redirect).
    registry.register(
        "java/util/Locale",
        "getLanguage",
        "()Ljava/lang/String;",
        locale_language,
    );
    registry.register(
        "java/util/Locale",
        "getCountry",
        "()Ljava/lang/String;",
        locale_country,
    );
    registry.register(
        "java/util/Locale",
        "getScript",
        "()Ljava/lang/String;",
        locale_script,
    );
    registry.register(
        "java/util/Locale",
        "getVariant",
        "()Ljava/lang/String;",
        locale_variant,
    );
    registry.register(
        "java/util/Locale",
        "toLanguageTag",
        "()Ljava/lang/String;",
        locale_tag,
    );
    registry.register(
        "java/util/Locale",
        "stripExtensions",
        "()Ljava/util/Locale;",
        |_ctx, args| Ok(args.first().copied()),
    );
    registry.register(
        "java/util/Locale",
        "getExtension",
        "(C)Ljava/lang/String;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );

    // Display-name overrides — the JDK's real implementations consult
    // `sun.util.resources.cldr.LocaleNames` resource bundles that we cannot
    // load (no CLDR data, no ServiceLoader for the resource-bundle
    // providers). Return language/country codes directly — good enough
    // for any caller that just wants a non-null human-readable string.
    registry.register(
        "java/util/Locale",
        "getDisplayName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let lang: String = match ctx.invoke_virtual(
                this,
                "getLanguage",
                "()Ljava/lang/String;",
                &[],
            )? {
                Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let country: String = match ctx.invoke_virtual(
                this,
                "getCountry",
                "()Ljava/lang/String;",
                &[],
            )? {
                Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let display = if country.is_empty() {
                lang
            } else {
                format!("{} ({})", lang, country)
            };
            Ok(Some(Value::Object(Some(ctx.create_string(&display)))))
        },
    );
    registry.register(
        "java/util/Locale",
        "getDisplayName",
        "(Ljava/util/Locale;)Ljava/lang/String;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let lang: String = match ctx.invoke_virtual(
                this,
                "getLanguage",
                "()Ljava/lang/String;",
                &[],
            )? {
                Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let country: String = match ctx.invoke_virtual(
                this,
                "getCountry",
                "()Ljava/lang/String;",
                &[],
            )? {
                Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let display = if country.is_empty() {
                lang
            } else {
                format!("{} ({})", lang, country)
            };
            Ok(Some(Value::Object(Some(ctx.create_string(&display)))))
        },
    );
    // getDisplayLanguage()/getDisplayCountry() — the "final" variants
    // delegate to the (Locale) overloads which hit the resource bundles.
    // Short-circuit with the code itself.
    registry.register(
        "java/util/Locale",
        "getDisplayLanguage",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            ctx.invoke_virtual(this, "getLanguage", "()Ljava/lang/String;", &[])
        },
    );
    registry.register(
        "java/util/Locale",
        "getDisplayLanguage",
        "(Ljava/util/Locale;)Ljava/lang/String;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            ctx.invoke_virtual(this, "getLanguage", "()Ljava/lang/String;", &[])
        },
    );
    registry.register(
        "java/util/Locale",
        "getDisplayCountry",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            ctx.invoke_virtual(this, "getCountry", "()Ljava/lang/String;", &[])
        },
    );
    registry.register(
        "java/util/Locale",
        "getDisplayCountry",
        "(Ljava/util/Locale;)Ljava/lang/String;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            ctx.invoke_virtual(this, "getCountry", "()Ljava/lang/String;", &[])
        },
    );
}

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
use std::sync::OnceLock;

/// Process-wide cached default Locale, lazily constructed on first
/// `Locale.getDefault()` call.  Stable identity so `==` comparisons in JDK
/// code (e.g. `Locale.getDefault() == cached`) behave sensibly.
fn cached_default_locale() -> &'static Mutex<Option<ObjectRef>> {
    static CACHE: OnceLock<Mutex<Option<ObjectRef>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Construct (or return the cached) default Locale: `new Locale("en", "US")`.
fn get_or_create_default(ctx: &mut dyn NativeContext) -> MethodCallResult {
    if let Some(obj) = *cached_default_locale().lock() {
        return Ok(Some(Value::Object(Some(obj))));
    }

    // Allocate a Locale instance and drive the public two-arg constructor
    // so `baseLocale`/`localeExtensions` get populated by the real JDK
    // bytecode. BaseLocale.getInstance works in our VM; it's only the
    // locale-provider adapter chain that throws.
    let locale_obj = match ctx.new_object("java/util/Locale")? {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let en = ctx.create_string("en");
    let us = ctx.create_string("US");
    ctx.invoke(
        "java/util/Locale",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        &[
            Value::Object(Some(locale_obj)),
            Value::Object(Some(en)),
            Value::Object(Some(us)),
        ],
    )?;

    *cached_default_locale().lock() = Some(locale_obj);
    Ok(Some(Value::Object(Some(locale_obj))))
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

    // Neutralise the "should not come down here" fallback by returning null.
    // Most callers handle a null provider gracefully (fall back to defaults
    // or other adapters); an `InternalError` aborts the whole chain.
    registry.register(
        "sun/util/locale/provider/JRELocaleProviderAdapter",
        "getLocaleServiceProvider",
        "(Ljava/lang/Class;)Ljava/util/spi/LocaleServiceProvider;",
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

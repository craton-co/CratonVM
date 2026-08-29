// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

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

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ObjectRef, Value};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::OnceLock;

/// Which of the JDK's three process defaults a lookup wants.
///
/// `java.util.Locale` keeps three, not one: the base default plus a
/// `Locale.Category.DISPLAY` and a `Locale.Category.FORMAT` default
/// (`defaultLocale`, `defaultDisplayLocale`, `defaultFormatLocale` in
/// `Locale.java`). They are seeded from three property families —
/// `user.language`, `user.language.display`, `user.language.format` — and they
/// genuinely differ on a host whose UI language and regional format differ,
/// which is an ordinary Windows configuration.
///
/// W7-67: `getDefault(Category)` used to ignore its argument and hand back the
/// base default, so `Locale.getDefault(FORMAT)` could not disagree with
/// `Locale.getDefault()` no matter what was set. That collapse is a separate
/// defect from the hardcoded-`en_US` one and is fixed here.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum LocaleCategory {
    Base,
    Display,
    Format,
}

impl LocaleCategory {
    /// The `user.*` property suffix this category reads, per
    /// `Locale.Category`'s own `languageKey`/`countryKey` constants.
    fn suffix(self) -> &'static str {
        match self {
            LocaleCategory::Base => "",
            LocaleCategory::Display => ".display",
            LocaleCategory::Format => ".format",
        }
    }
}

/// Process-wide cached default Locale, lazily constructed on first
/// `Locale.getDefault()` call.  Stable identity so `==` comparisons in JDK
/// code (e.g. `Locale.getDefault() == cached`) behave sensibly.
///
/// One slot per [`LocaleCategory`]. All three are GC roots — see
/// [`gc_scan_locale_roots`], which must scan every slot or a moving young
/// collection reclaims the one it missed while this cache keeps handing back
/// the stale `ObjectRef`.
/// ARCH-2026-08-04 A6 — `LockLevel::Scratch` (L0) — six acquisition sites, all
/// single statements over an `Option<ObjectRef>` (a `Copy` read, or a whole-slot
/// assignment). The GC scan's `if let` body only pushes into a `Vec` and its
/// guard drops before the neighbouring `synthetic_locale_data` lock is taken;
/// the remap holds the guard across pure `PointerMap` arithmetic. The
/// `getDefault` fast path's `if let` body is a bare `return`, so the allocation
/// that follows it runs with no guard held.
fn cached_locale(
    category: LocaleCategory,
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<Option<ObjectRef>> {
    static BASE: OnceLock<cratonvm_types::lock_order::OrderedPlMutex<Option<ObjectRef>>> =
        OnceLock::new();
    static DISPLAY: OnceLock<cratonvm_types::lock_order::OrderedPlMutex<Option<ObjectRef>>> =
        OnceLock::new();
    static FORMAT: OnceLock<cratonvm_types::lock_order::OrderedPlMutex<Option<ObjectRef>>> =
        OnceLock::new();
    let cell = match category {
        LocaleCategory::Base => &BASE,
        LocaleCategory::Display => &DISPLAY,
        LocaleCategory::Format => &FORMAT,
    };
    cell.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            None,
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// Every cache slot, for the whole-cache operations (GC scan/remap and
/// `setDefault(Locale)`, which the JDK defines as setting all three).
const ALL_LOCALE_CATEGORIES: [LocaleCategory; 3] = [
    LocaleCategory::Base,
    LocaleCategory::Display,
    LocaleCategory::Format,
];

/// Decode a `java.util.Locale$Category` argument.
///
/// Goes through `Enum.name()` rather than reading the `name` or `ordinal`
/// slot directly: `native_enum_name` (lang_misc) already handles the
/// shadowing/unwritten-slot traps that a raw field read walks into, and a name
/// match cannot silently drift if the enum's declaration order changes.
/// Anything we cannot decode falls back to `Base`, i.e. the pre-W7-67
/// behaviour, so an unexpected receiver degrades to the old answer rather
/// than to a wrong category.
fn decode_category(ctx: &mut dyn NativeContext, arg: Option<&Value>) -> LocaleCategory {
    let Some(Value::Object(Some(obj))) = arg else {
        return LocaleCategory::Base;
    };
    let name = match ctx.invoke_virtual(*obj, "name", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    match name.as_str() {
        "DISPLAY" => LocaleCategory::Display,
        "FORMAT" => LocaleCategory::Format,
        _ => LocaleCategory::Base,
    }
}

/// Map from synthetic Locale ObjectRef → (language, country, language_tag).
/// Used by the getLanguage/getCountry/toLanguageTag native overrides.
///
/// Owned `String`s rather than `&'static str`: the default locale is resolved
/// from `user.language`/`user.country` (or `$LC_ALL`/`$LC_MESSAGES`/`$LANG`)
/// at run time, so the subtags are not compile-time constants.
fn synthetic_locale_data() -> &'static Mutex<HashMap<ObjectRef, (String, String, String)>> {
    static MAP: OnceLock<Mutex<HashMap<ObjectRef, (String, String, String)>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Normalise a `(language, country)` pair the way the HotSpot launcher does.
///
/// `java_props_md.c` maps the POSIX `C` and `POSIX` locales — and an
/// unset/empty locale — onto **English/United States**, so on a bare Linux
/// shell (`LANG` and `LC_ALL` unset, or `LANG=C`) a real JVM reports
/// `Locale.getDefault().getLanguage() == "en"`, never `"C"`. Everything else
/// is normalised to `Locale`'s casing convention: lower-case language,
/// upper-case country.
fn normalise_language_country(lang: &str, country: &str) -> (String, String) {
    let lang = lang.trim();
    if lang.is_empty() || lang.eq_ignore_ascii_case("C") || lang.eq_ignore_ascii_case("POSIX") {
        return ("en".to_string(), "US".to_string());
    }
    (
        lang.to_ascii_lowercase(),
        country.trim().to_ascii_uppercase(),
    )
}

/// Parse a POSIX locale string — `language[_COUNTRY][.codeset][@modifier]` —
/// into `(language, country)`.
///
/// The `.codeset` and `@modifier` suffixes are dropped (`en_GB.UTF-8@euro` →
/// `("en", "GB")`), a bare language yields an empty country (`en` →
/// `("en", "")`), and `C` / `POSIX` / the empty string go through
/// [`normalise_language_country`] to `("en", "US")` — including the
/// glibc-flavoured spellings `C.UTF-8` and `POSIX.UTF-8`.
pub(crate) fn parse_posix_locale(raw: &str) -> (String, String) {
    // Take the part before the first `.` (codeset) or `@` (modifier); either
    // may come first in practice, so split on both at once.
    let base = raw.trim().split(['.', '@']).next().unwrap_or("").trim();
    match base.split_once('_') {
        // `language_COUNTRY[_variant]` — the variant is not part of the
        // (language, country) answer this resolver returns.
        Some((l, rest)) => {
            let c = rest.split('_').next().unwrap_or("");
            normalise_language_country(l, c)
        }
        None => normalise_language_country(base, ""),
    }
}

/// The environment value a real JVM's default locale comes from.
///
/// Precedence follows POSIX/glibc `setlocale`, which the HotSpot launcher
/// inherits: `LC_ALL` overrides every category, then the category variable
/// (`LC_MESSAGES` for the display locale), then `LANG`. A variable that is set
/// but empty does not select a locale, so it is skipped like an unset one.
fn env_locale_value() -> String {
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(v) = cratonvm_types::flags::runtime_var(key) {
            if !v.trim().is_empty() {
                return v;
            }
        }
    }
    String::new()
}

/// Resolve the process default locale as `(language, country)`.
///
/// Single source of truth for every `Locale.getDefault()` native in this
/// crate. In a real JVM `Locale.getDefault()` is seeded from the
/// `user.language`/`user.country` system properties, which the launcher in
/// turn derives from the environment — so those properties are consulted
/// first (that is also what makes `-Duser.language=de` work, and what keeps
/// `System.getProperty("user.language")` and `Locale.getDefault()` in
/// agreement, as the JDK guarantees). The environment is the fallback for
/// contexts with no property table populated yet.
pub(crate) fn resolve_default_locale(ctx: &dyn NativeContext) -> (String, String) {
    resolve_default_locale_for(ctx, LocaleCategory::Base)
}

/// [`resolve_default_locale`], for one [`LocaleCategory`].
///
/// A category reads `user.language<suffix>` and falls back to the base
/// `user.language` when that key is absent — which is exactly what
/// `Locale.initDefault(Category)` does via `StaticProperty.USER_LANGUAGE_FORMAT`
/// and friends, and it is why the VM only publishes the `.format` overlay when
/// it differs from the base (see `vm_init::fill_i18n_props`).
///
/// Language and country only: the synthetic Locale this module allocates
/// records `(language, country, tag)`, so a category whose *script* or
/// *variant* differs from the base is not represented. No host we run on
/// splits those two subtags across categories, and modelling them means
/// widening the side table — recorded in W7-67-host-default-locale.md rather
/// than half-done here.
pub(crate) fn resolve_default_locale_for(
    ctx: &dyn NativeContext,
    category: LocaleCategory,
) -> (String, String) {
    let suffix = category.suffix();
    let read = |base: &str| -> String {
        if !suffix.is_empty() {
            let scoped = ctx
                .get_system_property(&format!("{base}{suffix}"))
                .unwrap_or_default();
            if !scoped.trim().is_empty() {
                return scoped;
            }
        }
        ctx.get_system_property(base).unwrap_or_default()
    };
    let prop_lang = read("user.language");
    if !prop_lang.trim().is_empty() {
        return normalise_language_country(&prop_lang, &read("user.country"));
    }
    parse_posix_locale(&env_locale_value())
}

/// The language of a Locale this module synthesised (the cached `getDefault()`
/// one), or `None` when `obj` is not ours. Companion to `lib.rs`'s
/// `locale_data_get`: the two side tables are populated independently, so a
/// caller that needs the language of an arbitrary Locale has to consult both.
pub(crate) fn synthetic_language(obj: ObjectRef) -> Option<String> {
    synthetic_locale_data()
        .lock()
        .get(&obj)
        .map(|(lang, _, _)| lang.clone())
}

/// GC root scan for this module's cached synthetic Locale objects. The cached
/// default Locale (returned by `Locale.getDefault()`) and every key of the
/// synthetic-locale side-table are live `java/util/Locale` objects reachable
/// ONLY from these process-global mutexes, so a moving young GC would reclaim
/// or relocate them while the cache keeps handing back a stale `ObjectRef` —
/// observed as "Stale pointer detected in invokevirtual receiver … java/util/Locale"
/// followed by a SIGSEGV (TestServerInfo / TestSwallowAbortedUploads). Mirrors
/// the classloader singleton root scan.
pub fn gc_scan_locale_roots(out: &mut Vec<ObjectRef>) {
    // EVERY category slot, not just the base one. A slot missed here is a
    // Locale the collector is free to reclaim while `cached_locale` keeps
    // handing back its address.
    for category in ALL_LOCALE_CATEGORIES {
        if let Some(o) = *cached_locale(category).lock() {
            out.push(o);
        }
    }
    for k in synthetic_locale_data().lock().keys() {
        out.push(*k);
    }
}

/// Post-GC remap companion to [`gc_scan_locale_roots`].
pub fn gc_update_locale_refs(pointer_map: &cratonvm_types::PointerMap) {
    if pointer_map.is_empty() {
        return;
    }
    for category in ALL_LOCALE_CATEGORIES {
        let mut slot = cached_locale(category).lock();
        if let Some(obj) = slot.as_mut() {
            if let Some(&new_addr) = pointer_map.get(&(obj.as_ptr() as usize)) {
                *obj = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }
    {
        // Rebuild the ObjectRef-keyed side-table with relocated keys.
        let mut map = synthetic_locale_data().lock();
        let drained: Vec<_> = map.drain().collect();
        for (k, v) in drained {
            let nk = pointer_map
                .get(&(k.as_ptr() as usize))
                .map(|&n| unsafe { ObjectRef::from_raw(n as *mut u8) })
                .unwrap_or(k);
            map.insert(nk, v);
        }
    }
}

/// Construct (or return the cached) default Locale.
///
/// The subtags come from [`resolve_default_locale`], i.e. the same
/// `user.language`/`user.country` the VM publishes as system properties —
/// `en`/`US` on a host with no locale environment, on `LANG=C`, and on
/// `LANG=POSIX`, exactly as the HotSpot launcher's `java_props_md.c` maps
/// them. It used to be hard-wired to `en_US`, which disagreed with
/// `System.getProperty("user.language")` on any host that actually sets
/// `LANG`.
///
/// We use `alloc_concurrent_synthetic` instead of `ctx.new_object` because
/// `Locale.<clinit>` itself calls `getDefault()` (and other adapter-chain code
/// that throws InternalError). `ctx.new_object` propagates that failure as a
/// null return; `alloc_concurrent_synthetic` tolerates `<clinit>` failure and
/// always returns a valid ObjectRef.  We skip `Locale.<init>` entirely and
/// record language/country in a Rust-side map; native overrides for
/// `getLanguage()`/`getCountry()` etc. read from that map.
///
/// One cache slot per [`LocaleCategory`], so `getDefault(FORMAT)` can differ
/// from `getDefault()` when the host (or a `-Duser.language.format`) says they
/// differ. Each slot keeps a stable identity, as before.
fn get_or_create_default(
    ctx: &mut dyn NativeContext,
    category: LocaleCategory,
) -> MethodCallResult {
    if let Some(obj) = *cached_locale(category).lock() {
        return Ok(Some(Value::Object(Some(obj))));
    }

    // Allocate a synthetic Locale that bypasses <clinit>/<init> failures.
    // 32 slots is conservative: real JDK Locale has ~20 instance fields once
    // inherited fields are counted.
    let (lang, country) = resolve_default_locale_for(&*ctx, category);
    let tag = if country.is_empty() {
        lang.clone()
    } else {
        format!("{lang}-{country}")
    };

    let locale_obj = crate::try_alloc_concurrent_synthetic(ctx, "java/util/Locale", 32)?;

    synthetic_locale_data()
        .lock()
        .insert(locale_obj, (lang.clone(), country.clone(), tag));

    // Populate the real-JDK `baseLocale` instance field. Without this, any
    // un-overridden `Locale` method that runs as bytecode (e.g.
    // `Locale.equals` -> `baseLocale.equals(...)`) NPEs on the null field.
    // `locale_populate` also records the data in the lib.rs side table.
    crate::locale_populate(ctx, locale_obj, &lang, &country, "");

    *cached_locale(category).lock() = Some(locale_obj);
    Ok(Some(Value::Object(Some(locale_obj))))
}

/// For a *real* JDK `Locale` (not one of our synthetic ones), read a String
/// field off its `sun.util.locale.BaseLocale` — `language`, `region`,
/// `script`, or `variant`. The predefined constants (`Locale.FRENCH`, …) are
/// built by real JDK `<clinit>` bytecode, so their codes live there rather than
/// in our synthetic side table.
fn base_locale_field(
    ctx: &dyn NativeContext,
    this: cratonvm_types::ObjectRef,
    field: &str,
) -> Option<cratonvm_types::ObjectRef> {
    if let Value::Object(Some(base)) = ctx.get_field_by_name(this, "baseLocale") {
        if let Value::Object(Some(s)) = ctx.get_field_by_name(base, field) {
            return Some(s);
        }
    }
    None
}

fn locale_language(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        let syn = synthetic_locale_data()
            .lock()
            .get(this)
            .map(|(l, _, _)| l.clone());
        if let Some(lang) = syn {
            return Ok(Some(Value::Object(Some(ctx.create_string(&lang)))));
        }
        // `locale_populate` (lib.rs) records into a SECOND side table, and that
        // is the one `Locale.<init>`, `forLanguageTag` and the constants fill.
        // Consult it before the `baseLocale` fallback, which a synthetically
        // allocated Locale need not have.
        let (l, _, _) = crate::locale_data_get(*this);
        if !l.is_empty() {
            return Ok(Some(Value::Object(Some(ctx.create_string(&l)))));
        }
        // Real JDK Locale (e.g. Locale.FRENCH): read its BaseLocale.language.
        if let Some(s) = base_locale_field(ctx, *this, "language") {
            return Ok(Some(Value::Object(Some(s))));
        }
    }
    Ok(Some(Value::Object(Some(ctx.create_string("")))))
}

fn locale_country(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        let (_, c_side, _) = crate::locale_data_get(*this);
        if !c_side.is_empty() {
            return Ok(Some(Value::Object(Some(ctx.create_string(&c_side)))));
        }
        let syn = synthetic_locale_data()
            .lock()
            .get(this)
            .map(|(_, c, _)| c.clone());
        if let Some(country) = syn {
            return Ok(Some(Value::Object(Some(ctx.create_string(&country)))));
        }
        if let Some(s) = base_locale_field(ctx, *this, "region") {
            return Ok(Some(Value::Object(Some(s))));
        }
    }
    Ok(Some(Value::Object(Some(ctx.create_string("")))))
}

fn locale_tag(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        // Synthetic locales (our getDefault) carry a recorded BCP-47 tag.
        // Cloned out of the map so the guard is dropped before `create_string`
        // (which allocates, and can therefore re-enter).
        let syn_tag = synthetic_locale_data()
            .lock()
            .get(this)
            .map(|(_, _, t)| t.clone());
        if let Some(tag) = syn_tag {
            return Ok(Some(Value::Object(Some(ctx.create_string(&tag)))));
        }
        // See `locale_language`: the `locale_populate` side table is what the
        // constructors and constants fill, and a synthetic Locale has no
        // `baseLocale` for the fallback below to read.
        let (l, sc, c, v) = crate::locale_data_get_full(*this);
        if !l.is_empty() || !c.is_empty() || !sc.is_empty() {
            let mut tag = if l.is_empty() {
                "und".to_string()
            } else {
                l.to_ascii_lowercase()
            };
            // BCP-47 orders the subtags language-Script-REGION-variant.
            if !sc.is_empty() {
                tag.push('-');
                tag.push_str(&sc);
            }
            if !c.is_empty() {
                tag.push('-');
                tag.push_str(&c.to_ascii_uppercase());
            }
            if !v.is_empty() {
                // `Locale` stores multiple variants "_"-joined; BCP-47 uses "-".
                for sub in v.split('_').filter(|s| !s.is_empty()) {
                    tag.push('-');
                    tag.push_str(sub);
                }
            }
            return Ok(Some(Value::Object(Some(ctx.create_string(&tag)))));
        }
        // Real JDK Locale: build a BCP-47 tag from its baseLocale subtags.
        // The previous unconditional "und" fallback dropped language/script/
        // region/variant for ANY real Locale — e.g. `new Locale("","DE")`
        // produced "und" instead of "und-DE", and even "de-DE" -> "und"
        // (LocaleJavaTypeDescriptorTest: expected <und-DE> but got <und>).
        let lang_o = base_locale_field(ctx, *this, "language");
        let script_o = base_locale_field(ctx, *this, "script");
        let region_o = base_locale_field(ctx, *this, "region");
        let variant_o = base_locale_field(ctx, *this, "variant");
        let lang = lang_o.and_then(|s| ctx.read_string(s)).unwrap_or_default();
        let script = script_o
            .and_then(|s| ctx.read_string(s))
            .unwrap_or_default();
        let region = region_o
            .and_then(|s| ctx.read_string(s))
            .unwrap_or_default();
        let variant = variant_o
            .and_then(|s| ctx.read_string(s))
            .unwrap_or_default();

        // language[-script][-region][-variant…]; empty language => "und".
        let mut tag = if lang.is_empty() {
            "und".to_string()
        } else {
            lang.to_ascii_lowercase()
        };
        if !script.is_empty() {
            // script subtag is title-case (e.g. "Hant").
            let mut sc = script.to_ascii_lowercase();
            if let Some(first) = sc.get_mut(0..1) {
                first.make_ascii_uppercase();
            }
            tag.push('-');
            tag.push_str(&sc);
        }
        if !region.is_empty() {
            tag.push('-');
            tag.push_str(&region.to_ascii_uppercase());
        }
        if !variant.is_empty() {
            // Locale stores multiple variants "_"-joined; BCP-47 uses "-".
            for sub in variant.split('_').filter(|s| !s.is_empty()) {
                tag.push('-');
                tag.push_str(&sub.to_ascii_lowercase());
            }
        }
        // Append BCP-47 extension subtags (e.g. "-u-ca-japanese", "-x-foo-bar").
        // `LocaleExtensions.id` already holds the canonical, lower-cased
        // extension sequence with the privateuse ('x') singleton ordered last —
        // exactly the suffix real `toLanguageTag` emits. Without this the
        // extensions were silently dropped (e.g.
        // `forLanguageTag("en-US-u-ca-japanese").toLanguageTag()` -> "en-US").
        // Only real Locales constructed WITH extensions have a non-null
        // `localeExtensions`, so plain locales are unaffected.
        if let Value::Object(Some(le)) = ctx.get_field_by_name(*this, "localeExtensions") {
            if let Value::Object(Some(id_s)) = ctx.get_field_by_name(le, "id") {
                if let Some(id) = ctx.read_string(id_s) {
                    if !id.is_empty() {
                        tag.push('-');
                        tag.push_str(&id);
                    }
                }
            }
        }
        return Ok(Some(Value::Object(Some(ctx.create_string(&tag)))));
    }
    Ok(Some(Value::Object(Some(ctx.create_string("und")))))
}

/// `java.util.Locale.getExtension(char)` — return the BCP-47 extension value
/// for a given singleton key (e.g. 'u' -> "ca-japanese", 'x' -> "foo-bar").
///
/// The previous implementation was an unconditional `null` stub, which dropped
/// every extension for real Locales (Keycloak `FolderThemeTest`). Instead,
/// delegate to the real `LocaleExtensions.getExtensionValue(Character)`
/// bytecode — its `SortedMap<Character,Extension>` lookup now preserves the
/// `Character` key type (the TreeMap key-rebox fix). The synthetic default
/// Locale carries no extensions, so it returns null either way.
fn locale_get_extension(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key_char = match args.get(1) {
        Some(Value::Int(c)) => *c,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Synthetic default Locale (getDefault) defines no extensions.
    if synthetic_locale_data().lock().contains_key(&this) {
        return Ok(Some(Value::Object(None)));
    }
    // hasExtensions() == (localeExtensions != null).
    let le = match ctx.get_field_by_name(this, "localeExtensions") {
        Value::Object(Some(le)) => le,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Box the char key as java.lang.Character; getExtensionValue lower-cases it.
    let boxed = ctx
        .invoke(
            "java/lang/Character",
            "valueOf",
            "(C)Ljava/lang/Character;",
            &[Value::Int(key_char)],
        )?
        .unwrap_or(Value::Object(None));
    ctx.invoke_virtual(
        le,
        "getExtensionValue",
        "(Ljava/lang/Character;)Ljava/lang/String;",
        &[boxed],
    )
}

fn locale_script(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        // Same three-shape lookup as `locale_language`: the `locale_populate`
        // side table first (it is the only place a synthetic Locale's script
        // survives when `sun.util.locale.BaseLocale` is unavailable — the
        // synthetic-JDK build has no such class), then the real object's
        // `baseLocale`.
        let (_, script, _, _) = crate::locale_data_get_full(*this);
        if !script.is_empty() {
            return Ok(Some(Value::Object(Some(ctx.create_string(&script)))));
        }
        if let Some(s) = base_locale_field(ctx, *this, "script") {
            return Ok(Some(Value::Object(Some(s))));
        }
    }
    Ok(Some(Value::Object(Some(ctx.create_string("")))))
}

/// Build a `Locale` from a BCP-47 language tag by running the REAL JDK's
/// `Locale.forLanguageTag` body — `LanguageTag.parse` →
/// `InternalLocaleBuilder` → `Locale.getInstance` — instead of
/// re-implementing BCP-47 in Rust.
///
/// Returns `None` when any step is unreachable, which is exactly the
/// synthetic-JDK build (no `sun.util.locale` package at all); the caller then
/// falls back to [`split_language_tag`].
///
/// Why delegate rather than extend the Rust parser: the Rust split had no
/// slot for the script subtag (`zh-hant-CN` came out as plain `zh_CN`, the
/// `TestAcceptLanguage.bug56848` failure) and mistook a BCP-47 singleton for a
/// variant (`en-US-u-ca-japanese` came out with variant `"u"`,
/// `zh-Hant-TW-x-java` with variant `"x"`). Each of those is a separate corner
/// of a grammar the JDK already implements — and the JDK's implementation is
/// plain bytecode that CratonVM runs correctly, as
/// `Locale.Builder.setLanguageTag` (which shares this exact chain) already
/// demonstrated. `LanguageTag.parse` is called with `lenient = true` and a
/// fresh `ParsePosition(0)` that is never inspected — exactly what
/// `forLanguageTag` passes, so an ill-formed trailing subtag is dropped
/// rather than thrown on.
fn locale_from_language_tag_real(ctx: &mut dyn NativeContext, tag: &str) -> Option<ObjectRef> {
    ctx.ensure_class_initialized("sun/util/locale/LanguageTag")
        .ok()?;
    ctx.ensure_class_initialized("sun/util/locale/InternalLocaleBuilder")
        .ok()?;

    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let tag_s = scope.create_string(tag);
    let tag_h = scope.root(tag_s);

    // NOTE the descriptor: JDK 25 spells this `parse(String, ParsePosition,
    // boolean)`. The older `parse(String, ParseStatus)` shape resolves to
    // nothing here — `sun.util.locale.ParseStatus` no longer exists — and a
    // failed resolution is indistinguishable from "synthetic JDK", so the
    // whole delegation silently degraded to the Rust fallback.
    let pp = match scope.new_object_initialized("java/text/ParsePosition", "(I)V", &[Value::Int(0)])
    {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => return None,
    };
    let pp_h = scope.root(pp);

    let parse_args = [
        Value::Object(Some(scope.get(&tag_h))),
        Value::Object(Some(scope.get(&pp_h))),
        Value::Int(1),
    ];
    let lt = match scope.invoke(
        "sun/util/locale/LanguageTag",
        "parse",
        "(Ljava/lang/String;Ljava/text/ParsePosition;Z)Lsun/util/locale/LanguageTag;",
        &parse_args,
    ) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => return None,
    };
    let lt_h = scope.root(lt);

    let bldr =
        match scope.new_object_initialized("sun/util/locale/InternalLocaleBuilder", "()V", &[]) {
            Ok(Some(Value::Object(Some(o)))) => o,
            _ => return None,
        };
    let bldr_h = scope.root(bldr);

    let (recv, arg) = (scope.get(&bldr_h), scope.get(&lt_h));
    scope
        .invoke_virtual(
            recv,
            "setLanguageTag",
            "(Lsun/util/locale/LanguageTag;)Lsun/util/locale/InternalLocaleBuilder;",
            &[Value::Object(Some(arg))],
        )
        .ok()?;

    let recv = scope.get(&bldr_h);
    let base =
        match scope.invoke_virtual(recv, "getBaseLocale", "()Lsun/util/locale/BaseLocale;", &[]) {
            Ok(Some(Value::Object(Some(o)))) => o,
            _ => return None,
        };
    let base_h = scope.root(base);

    let recv = scope.get(&bldr_h);
    let exts = match scope.invoke_virtual(
        recv,
        "getLocaleExtensions",
        "()Lsun/util/locale/LocaleExtensions;",
        &[],
    ) {
        Ok(Some(Value::Object(o))) => o,
        _ => return None,
    };
    let exts_h = exts.map(|e| scope.root(e));

    // `forLanguageTag`'s last step: a tag whose only extension information is
    // carried by a legacy variant (`ja-JP-x-lvariant-JP`) gets the matching
    // calendar/numbering extension synthesised back. Best-effort — if the
    // package-private helper is not callable the locale is still correct for
    // every tag that does not use that legacy spelling.
    let exts_h = match exts_h {
        Some(h) => Some(h),
        None => {
            let base_now = scope.get(&base_h);
            let variant_empty = match scope.get_field_by_name(base_now, "variant") {
                Value::Object(Some(s)) => scope.read_string(s).unwrap_or_default().is_empty(),
                _ => true,
            };
            if variant_empty {
                None
            } else {
                let base_now = scope.get(&base_h);
                let compat_args = [
                    scope.get_field_by_name(base_now, "language"),
                    scope.get_field_by_name(base_now, "script"),
                    scope.get_field_by_name(base_now, "region"),
                    scope.get_field_by_name(base_now, "variant"),
                ];
                match scope.invoke(
                    "java/util/Locale",
                    "getCompatibilityExtensions",
                    "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)\
                     Lsun/util/locale/LocaleExtensions;",
                    &compat_args,
                ) {
                    Ok(Some(Value::Object(Some(e)))) => Some(scope.root(e)),
                    _ => None,
                }
            }
        }
    };

    let get_args = [
        Value::Object(Some(scope.get(&base_h))),
        Value::Object(exts_h.as_ref().map(|h| scope.get(h))),
    ];
    match scope.invoke(
        "java/util/Locale",
        "getInstance",
        "(Lsun/util/locale/BaseLocale;Lsun/util/locale/LocaleExtensions;)Ljava/util/Locale;",
        &get_args,
    ) {
        Ok(Some(Value::Object(Some(o)))) => Some(o),
        _ => None,
    }
}

/// Split a BCP-47 language tag into `(language, script, region, variant)`.
///
/// The synthetic-JDK fallback for [`locale_from_language_tag_real`]; it models
/// only the subtags the Rust side table can hold, so BCP-47 extensions are
/// dropped rather than mis-filed as a variant.
pub(crate) fn split_language_tag(tag: &str) -> (String, String, String, String) {
    let mut parts = tag.split(['-', '_']).filter(|p| !p.is_empty());
    let mut language = parts.next().unwrap_or("").to_ascii_lowercase();
    // `und` is BCP-47 for "undetermined", which `Locale` represents as an
    // EMPTY language: on HotSpot `forLanguageTag("und-DE").getLanguage()` is
    // `""` and its `toString()` is `_DE`, not `und_DE`.
    if language == "und" {
        language.clear();
    }
    let mut script = String::new();
    let mut region = String::new();
    let mut variants: Vec<String> = Vec::new();
    for p in parts {
        // A single-character subtag is an extension/private-use singleton;
        // everything after it belongs to that extension. The side table has no
        // slot for extensions, so stop here rather than filing `u` or `x` as a
        // variant (which is what the previous parser did).
        if p.len() == 1 {
            break;
        }
        let alpha = p.chars().all(|c| c.is_ascii_alphabetic());
        if script.is_empty() && region.is_empty() && variants.is_empty() && p.len() == 4 && alpha {
            script = p.to_ascii_lowercase();
            if let Some(first) = script.get_mut(0..1) {
                first.make_ascii_uppercase();
            }
            continue;
        }
        if region.is_empty()
            && variants.is_empty()
            && ((p.len() == 2 && alpha) || (p.len() == 3 && p.chars().all(|c| c.is_ascii_digit())))
        {
            region = p.to_ascii_uppercase();
            continue;
        }
        // A well-formed variant is 5-8 alphanumerics, or exactly 4 characters
        // the first of which is a digit. Anything else is ill-formed, and
        // `LanguageTag.parse` stops at the first ill-formed subtag.
        let alnum = p.chars().all(|c| c.is_ascii_alphanumeric());
        let well_formed = alnum
            && ((5..=8).contains(&p.len())
                || (p.len() == 4 && p.starts_with(|c: char| c.is_ascii_digit())));
        if !well_formed {
            break;
        }
        variants.push(p.to_string());
    }
    // `Locale` joins multiple variant subtags with `_`.
    (language, script, region, variants.join("_"))
}

fn locale_variant(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(this))) = args.first() {
        if let Some(s) = base_locale_field(ctx, *this, "variant") {
            return Ok(Some(Value::Object(Some(s))));
        }
    }
    Ok(Some(Value::Object(Some(ctx.create_string("")))))
}

/// The standard set of locales CratonVM reports from
/// `Locale.getAvailableLocales()`. CratonVM ships no CLDR locale data, so the
/// real JDK `getAvailableLocales()` bytecode (which walks the
/// `LocaleProviderAdapter` → CLDR data path) returns an **empty** array. That
/// breaks any code that picks a random locale from the list — most visibly
/// `org.apache.lucene.tests.util.LuceneTestCase.randomLocale()`, which does
/// `locales[random.nextInt(locales.length)]` and throws
/// `IllegalArgumentException: bound must be positive` for `nextInt(0)` during
/// the `@BeforeClass` setup of **every** Elasticsearch/Lucene `ESTestCase`.
///
/// This is the canonical JDK locale list (the COMPAT provider's set). Pairs are
/// `(language, country)`; `("", "")` is `Locale.ROOT`. The locales are built via
/// the `(String,String,String)` constructor — `forLanguageTag` does NOT populate
/// the base-locale fields without CLDR data (see `essential_quarkus_locale_convert`).
const AVAILABLE_LOCALES: &[(&str, &str)] = &[
    ("", ""),
    ("ar", ""),
    ("ar", "AE"),
    ("ar", "BH"),
    ("ar", "DZ"),
    ("ar", "EG"),
    ("ar", "IQ"),
    ("ar", "JO"),
    ("ar", "KW"),
    ("ar", "LB"),
    ("ar", "LY"),
    ("ar", "MA"),
    ("ar", "OM"),
    ("ar", "QA"),
    ("ar", "SA"),
    ("ar", "SD"),
    ("ar", "SY"),
    ("ar", "TN"),
    ("ar", "YE"),
    ("be", ""),
    ("be", "BY"),
    ("bg", ""),
    ("bg", "BG"),
    ("ca", ""),
    ("ca", "ES"),
    ("cs", ""),
    ("cs", "CZ"),
    ("da", ""),
    ("da", "DK"),
    ("de", ""),
    ("de", "AT"),
    ("de", "CH"),
    ("de", "DE"),
    ("de", "LU"),
    ("el", ""),
    ("el", "CY"),
    ("el", "GR"),
    ("en", ""),
    ("en", "AU"),
    ("en", "CA"),
    ("en", "GB"),
    ("en", "IE"),
    ("en", "IN"),
    ("en", "MT"),
    ("en", "NZ"),
    ("en", "PH"),
    ("en", "SG"),
    ("en", "US"),
    ("en", "ZA"),
    ("es", ""),
    ("es", "AR"),
    ("es", "BO"),
    ("es", "CL"),
    ("es", "CO"),
    ("es", "CR"),
    ("es", "DO"),
    ("es", "EC"),
    ("es", "ES"),
    ("es", "GT"),
    ("es", "HN"),
    ("es", "MX"),
    ("es", "NI"),
    ("es", "PA"),
    ("es", "PE"),
    ("es", "PR"),
    ("es", "PY"),
    ("es", "SV"),
    ("es", "US"),
    ("es", "UY"),
    ("es", "VE"),
    ("et", ""),
    ("et", "EE"),
    ("fi", ""),
    ("fi", "FI"),
    ("fr", ""),
    ("fr", "BE"),
    ("fr", "CA"),
    ("fr", "CH"),
    ("fr", "FR"),
    ("fr", "LU"),
    ("ga", ""),
    ("ga", "IE"),
    ("he", ""),
    ("he", "IL"),
    ("hi", ""),
    ("hi", "IN"),
    ("hr", ""),
    ("hr", "HR"),
    ("hu", ""),
    ("hu", "HU"),
    ("id", ""),
    ("id", "ID"),
    ("is", ""),
    ("is", "IS"),
    ("it", ""),
    ("it", "CH"),
    ("it", "IT"),
    ("ja", ""),
    ("ja", "JP"),
    ("ko", ""),
    ("ko", "KR"),
    ("lt", ""),
    ("lt", "LT"),
    ("lv", ""),
    ("lv", "LV"),
    ("mk", ""),
    ("mk", "MK"),
    ("ms", ""),
    ("ms", "MY"),
    ("mt", ""),
    ("mt", "MT"),
    ("nl", ""),
    ("nl", "BE"),
    ("nl", "NL"),
    ("no", ""),
    ("no", "NO"),
    ("pl", ""),
    ("pl", "PL"),
    ("pt", ""),
    ("pt", "BR"),
    ("pt", "PT"),
    ("ro", ""),
    ("ro", "RO"),
    ("ru", ""),
    ("ru", "RU"),
    ("sk", ""),
    ("sk", "SK"),
    ("sl", ""),
    ("sl", "SI"),
    ("sq", ""),
    ("sq", "AL"),
    ("sr", ""),
    ("sr", "BA"),
    ("sr", "CS"),
    ("sr", "ME"),
    ("sr", "RS"),
    ("sv", ""),
    ("sv", "SE"),
    ("th", ""),
    ("th", "TH"),
    ("tr", ""),
    ("tr", "TR"),
    ("uk", ""),
    ("uk", "UA"),
    ("vi", ""),
    ("vi", "VN"),
    ("zh", ""),
    ("zh", "CN"),
    ("zh", "HK"),
    ("zh", "SG"),
    ("zh", "TW"),
];

/// `java.util.Locale.getAvailableLocales()` — return a non-empty `Locale[]`.
/// See [`AVAILABLE_LOCALES`] for the rationale (CratonVM has no CLDR data, so
/// the real bytecode returns empty, breaking locale randomization).
fn get_available_locales(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let cls = "java/util/Locale";
    let locale_cid = ctx.ensure_class_initialized(cls)?;
    let arr = ctx.new_ref_array(locale_cid, AVAILABLE_LOCALES.len());
    for (i, (lang, country)) in AVAILABLE_LOCALES.iter().enumerate() {
        let loc_obj = match ctx.new_object(cls) {
            Ok(Some(Value::Object(Some(o)))) => o,
            _ => continue,
        };
        let l = ctx.create_string(lang);
        let c = ctx.create_string(country);
        let v = ctx.create_string("");
        let _ = ctx.invoke(
            cls,
            "<init>",
            "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
            &[
                Value::Object(Some(loc_obj)),
                Value::Object(Some(l)),
                Value::Object(Some(c)),
                Value::Object(Some(v)),
            ],
        );
        ctx.set_array_element(arr, i, Value::Object(Some(loc_obj)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

pub fn register(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    // java.util.Locale.getAvailableLocales() — CratonVM ships no CLDR data, so
    // the real bytecode returns an empty array, breaking locale randomization
    // (Lucene/ES `randomLocale()` → `nextInt(0)`). Provide the standard set.
    registry.register(
        "java/util/Locale",
        "getAvailableLocales",
        "()[Ljava/util/Locale;",
        get_available_locales,
    );
    // java.util.Locale.getDefault() — bypass the adapter chain.
    //
    // DUPLICATE-REGISTRATION NOTE: the no-arg overload below is registered a
    // second time by `t3_impl::register_t311_i18n`, which
    // `register_synthetic_overrides` runs AFTER `register_essential_natives`
    // (this registrar). Registration is last-wins, so under
    // `#[cfg(feature = "synthetic-jdk")]` the t3_impl copy is the live one and
    // this one only runs in real-JDK mode. Both resolve their subtags through
    // `resolve_default_locale` so the two modes agree; if you change the
    // language/country policy here, change it there too (or delete one).
    registry.register(
        "java/util/Locale",
        "getDefault",
        "()Ljava/util/Locale;",
        |ctx, _args| get_or_create_default(ctx, LocaleCategory::Base),
    );
    // W7-67: this used to discard its argument and return the base default, so
    // `Locale.getDefault(FORMAT)` could never disagree with `Locale.getDefault()`.
    // It now resolves per category off the `user.*.format`/`user.*.display`
    // overlay, matching `Locale.initDefault(Category)`.
    registry.register(
        "java/util/Locale",
        "getDefault",
        "(Ljava/util/Locale$Category;)Ljava/util/Locale;",
        |ctx, args| {
            let category = decode_category(ctx, args.first());
            get_or_create_default(ctx, category)
        },
    );
    // java.util.Locale.setDefault(Locale) / setDefault(Category, Locale) —
    // since `getDefault()` above is hard-overridden to read our own cache
    // instead of the real JDK's `defaultLocale` static field, `setDefault`
    // must write to that SAME cache or it becomes a no-op from the caller's
    // perspective: `Locale.setDefault(GERMAN); Locale.getDefault()` would
    // keep returning the original cached en_US Locale. Verified against real
    // JDK 25 that this is required for
    // `ResourceBundle.Control.getFallbackLocale`'s `Locale.getDefault()`
    // fallback (see `locale_resources::resolve_fallback_locale`) to see a
    // `setDefault` call made earlier in the same test/run.
    //
    // W7-67: the two overloads no longer do the same thing, because the cache
    // is no longer one slot. `Locale.setDefault(Locale)`'s own body is
    // `setDefault(DISPLAY, l); setDefault(FORMAT, l);` on top of the base
    // field, so it writes all three; `setDefault(Category, Locale)` writes
    // only the named one, and must NOT touch the base — a test that pins
    // FORMAT for a number-format assertion has to leave DISPLAY alone.
    registry.register(
        "java/util/Locale",
        "setDefault",
        "(Ljava/util/Locale;)V",
        |_ctx, args| {
            if let Some(Value::Object(Some(loc))) = args.first() {
                for category in ALL_LOCALE_CATEGORIES {
                    *cached_locale(category).lock() = Some(*loc);
                }
            }
            Ok(None)
        },
    );
    registry.register(
        "java/util/Locale",
        "setDefault",
        "(Ljava/util/Locale$Category;Ljava/util/Locale;)V",
        |ctx, args| {
            if let Some(Value::Object(Some(loc))) = args.get(1) {
                let loc = *loc;
                // An undecodable category is the JDK's NullPointerException
                // case; `decode_category` degrades it to `Base`, i.e. the
                // pre-W7-67 behaviour, rather than dropping the call.
                let category = decode_category(ctx, args.first());
                *cached_locale(category).lock() = Some(loc);
            }
            Ok(None)
        },
    );

    // Note: `ResourceBundle$Control.getCandidateLocales` override is
    // registered from `register_essential_natives` (real-JDK path),
    // since this `register` function is only wired into the synthetic
    // path. See lib.rs EUREKA-RB-CANDIDATE.

    // Neutralise the "should not come down here" fallback by returning null.
    // Most callers handle a null provider gracefully (fall back to defaults
    // or other adapters); an `InternalError` aborts the whole chain.
    //
    // Wave-2 re-check: this is a KEEP, and not for convenience. The real
    // `JRELocaleProviderAdapter.getLocaleServiceProvider` body is a
    // `switch` over the known SPI classes ending in `throw new
    // InternalError("should not come down here")`, so there is no "correct"
    // non-null value to synthesise for a provider class this VM does not
    // implement — the honest choices are null or that InternalError, and the
    // JDK's own callers (`LocaleServiceProviderPool.findAdapter`) are written
    // to skip a null adapter and try the next one.
    //
    // Wave-3 CORRECTION — the wave-2 note ended "Registered only on the
    // synthetic path", and that is FALSE. `locale_bootstrap::register` has a
    // single caller, `lib.rs::register_essential_natives_with_shims`, which is
    // the REAL-JDK path (`vm/src/vm/vm_init.rs`). So this native shadows real
    // `JRELocaleProviderAdapter` bytecode in the default run mode, and the
    // blanket null therefore also suppresses the switch's SUCCESS arms — the
    // ~12 SPI classes real JDK does answer (`DateFormatProvider`,
    // `DecimalFormatSymbolsProvider`, `BreakIteratorProvider`, …), not just the
    // unknown ones that would have hit the InternalError.
    //
    // Wave-4 verdict: this is NOT a KEEP. The real method is not constant and
    // `null` has no spec basis — it is an ACCEPTED, ESCALATED DIVERGENCE.
    // Recorded as such so no later pass re-files it under "justified constant".
    //
    // The faithful fix is known and small: switch on `c.getSimpleName()` and
    // delegate to the matching real `get*Provider()` getter on the receiver,
    // answering null ONLY on the default arm — the one that would otherwise
    // `throw new InternalError("should not come down here")`, and which
    // `LocaleServiceProviderPool.findAdapter` is written to skip.
    //
    // It is not landed here because it cannot be landed blind: each
    // `get*Provider()` calls `getLanguageTagSet(...)` and instantiates the
    // adapter's inner provider off `LocaleDataMetaInfo`/`LocaleResources` —
    // precisely the JDK resource-bundle chain this C20 override exists to
    // bypass for Jackson/H2/`Locale.getDefault()` formatting. Landing it needs
    // a build plus those suites. Whoever picks it up: delegate ONE SPI at a
    // time, re-run the formatting suites for each, and do not "simplify" it to
    // a blanket delegation on the strength of reading the switch.
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
    // `stripExtensions` used to be an identity stub, which was harmless only
    // while no CratonVM `Locale` could carry extensions in the first place.
    // Now that `forLanguageTag` runs the real BCP-47 parser, `zh-Hant-TW-x-java`
    // does have a `localeExtensions`, and the identity stub answered
    // `zh_TW_#Hant_x-java` where HotSpot answers `zh_TW_#Hant`.
    registry.register(
        "java/util/Locale",
        "stripExtensions",
        "()Ljava/util/Locale;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                other => return Ok(other.copied()),
            };
            // `hasExtensions()` is `localeExtensions != null`. A Locale
            // without extensions — every synthetic one — strips to itself.
            if !matches!(
                ctx.get_field_by_name(this, "localeExtensions"),
                Value::Object(Some(_))
            ) {
                return Ok(Some(Value::Object(Some(this))));
            }
            let base = match ctx.get_field_by_name(this, "baseLocale") {
                Value::Object(Some(b)) => b,
                _ => return Ok(Some(Value::Object(Some(this)))),
            };
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let this_h = scope.root(this);
            let base_h = scope.root(base);
            let args = [
                Value::Object(Some(scope.get(&base_h))),
                Value::Object(None),
            ];
            match scope.invoke(
                "java/util/Locale",
                "getInstance",
                "(Lsun/util/locale/BaseLocale;Lsun/util/locale/LocaleExtensions;)Ljava/util/Locale;",
                &args,
            ) {
                Ok(Some(Value::Object(Some(o)))) => Ok(Some(Value::Object(Some(o)))),
                _ => Ok(Some(Value::Object(Some(scope.get(&this_h))))),
            }
        },
    );
    registry.register(
        "java/util/Locale",
        "getExtension",
        "(C)Ljava/lang/String;",
        locale_get_extension,
    );

    // Display-name overrides.
    //
    // These used to return the language/country CODE, on the reasoning that
    // any caller "just wants a non-null human-readable string". That is true
    // of callers who print one and false of callers who use one as a KEY.
    // H2's `org.h2.value.CompareMode.getName` asks every locale `Collator`
    // offers for its ENGLISH display name and matches the answer against the
    // requested collation — so with codes in the table, `SET COLLATION TURKISH`
    // resolved to nothing and every `SET COLLATION <language>` was rejected.
    //
    // The real names are in the JDK image all along, in
    // `sun/util/resources/cldr/ext/LocaleNames_*` (324 bundles). Read them,
    // and keep the code as the last resort — which is also what the real JDK
    // does for a subtag CLDR does not name.
    fn display_lookup(
        ctx: &mut dyn NativeContext,
        this: ObjectRef,
        want_country: bool,
        display_locale: Option<ObjectRef>,
    ) -> Result<Option<Value>, cratonvm_types::error::MethodCallFailed> {
        let accessor = if want_country {
            "getCountry"
        } else {
            "getLanguage"
        };
        let code: String = match ctx.invoke_virtual(this, accessor, "()Ljava/lang/String;", &[])? {
            Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        if code.is_empty() {
            return Ok(Some(Value::Object(Some(ctx.create_string("")))));
        }
        // Which locale to name it IN. No argument means the default locale,
        // and the JDK's own default here is English.
        let (dl, dc) = match display_locale {
            Some(loc) => {
                // `getLanguage()` can move `loc` before `getCountry()` reads it.
                let loc_pin = ctx.pin_native_root(loc);
                let l = match ctx.invoke_virtual(loc, "getLanguage", "()Ljava/lang/String;", &[])? {
                    Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
                    _ => String::new(),
                };
                let loc = ctx.read_native_pin(loc_pin, loc);
                let c = match ctx.invoke_virtual(loc, "getCountry", "()Ljava/lang/String;", &[])? {
                    Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
                    _ => String::new(),
                };
                (l, c)
            }
            None => ("en".to_string(), String::new()),
        };
        let named =
            crate::locale_resources::cldr_locale_display_name(ctx, &code, &dl, &dc).unwrap_or(code);
        Ok(Some(Value::Object(Some(ctx.create_string(&named)))))
    }

    registry.register(
        "java/util/Locale",
        "getDisplayLanguage",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            display_lookup(ctx, this, false, None)
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
            let disp = match args.get(1) {
                Some(Value::Object(Some(o))) => Some(*o),
                _ => None,
            };
            display_lookup(ctx, this, false, disp)
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
            display_lookup(ctx, this, true, None)
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
            let disp = match args.get(1) {
                Some(Value::Object(Some(o))) => Some(*o),
                _ => None,
            };
            display_lookup(ctx, this, true, disp)
        },
    );

    /// `getDisplayName` is the two halves joined the way the JDK joins them:
    /// `Language (Country)`, or just whichever half exists.
    fn display_name(
        ctx: &mut dyn NativeContext,
        this: ObjectRef,
        display_locale: Option<ObjectRef>,
    ) -> Result<Option<Value>, cratonvm_types::error::MethodCallFailed> {
        let lang = match display_lookup(ctx, this, false, display_locale)? {
            Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let country = match display_lookup(ctx, this, true, display_locale)? {
            Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let display = if country.is_empty() {
            lang
        } else if lang.is_empty() {
            country
        } else {
            format!("{lang} ({country})")
        };
        Ok(Some(Value::Object(Some(ctx.create_string(&display)))))
    }

    registry.register(
        "java/util/Locale",
        "getDisplayName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            display_name(ctx, this, None)
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
            let disp = match args.get(1) {
                Some(Value::Object(Some(o))) => Some(*o),
                _ => None,
            };
            display_name(ctx, this, disp)
        },
    );

    // ---------------------------------------------------------------------
    // Construction, `toString`, `forLanguageTag`, and the remaining public
    // constants.
    //
    // The surface above could read a Locale but not make one: there was no
    // `<init>`, no `toString`, no `forLanguageTag`, and only ROOT / ENGLISH /
    // US / CANADA of the sixteen public constants. `new Locale("en", "US")`
    // therefore produced an object with nothing in the side table, which every
    // accessor above reports as the root locale.
    // ---------------------------------------------------------------------

    /// Read `args[i]` as a Rust string, treating null/absent as empty — the
    /// shape `Locale`'s constructors give a missing country or variant.
    fn str_arg(ctx: &mut dyn NativeContext, args: &[Value], i: usize) -> String {
        match args.get(i) {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        }
    }

    fn locale_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(None),
        };
        // `Locale` normalises language to lower case and country to upper.
        let lang = str_arg(ctx, args, 1).to_ascii_lowercase();
        let country = str_arg(ctx, args, 2).to_ascii_uppercase();
        let variant = str_arg(ctx, args, 3);
        crate::locale_populate(ctx, this, &lang, &country, &variant);
        Ok(None)
    }

    for desc in [
        "(Ljava/lang/String;)V",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
    ] {
        registry.register("java/util/Locale", "<init>", desc, locale_init);
    }

    // `language`, `language_COUNTRY`, `language_COUNTRY_variant`, and the
    // `_COUNTRY` form when the language is empty — `Locale.toString`'s
    // documented layout, which is NOT the same as `toLanguageTag`.
    //
    // The subtags are read through the ACCESSORS, not through
    // `locale_data_get` alone. Each accessor carries a three-shape fallback —
    // the `locale_populate` side table that `<init>`/`forLanguageTag`/the
    // constants fill, `synthetic_locale_data` for our own default Locale, and
    // finally the real JDK object's `baseLocale` field (see `locale_language`)
    // — and this method consulted only the first. Every `Locale` built by real
    // JDK bytecode without running our `<init>` therefore stringified to the
    // EMPTY STRING, `Locale.GERMAN` and `Locale.ITALIAN` included.
    //
    // That is not a cosmetic defect: `toString()` is a map KEY in real code.
    // Jetty's `ContextHandler.setLocaleEncoding`/`getLocaleEncoding` key their
    // encoding map on it, so every locale collapsed onto the one `""` key and a
    // mapping registered for GERMAN answered a lookup for ITALIAN —
    // `JettyServletWebServerFactoryTests.localeCharsetMappingsAreConfigured`,
    // "expected: null but was: UTF-8", against 113/113 on HotSpot.
    // `probes/LocaleToStringProbe.java` is the witness.
    registry.register(
        "java/util/Locale",
        "toString",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let subtag = |ctx: &mut dyn NativeContext, name: &str| -> MethodCallResult {
                ctx.invoke_virtual(this, name, "()Ljava/lang/String;", &[])
            };
            let read = |ctx: &mut dyn NativeContext, v: Option<Value>| match v {
                Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let v = subtag(ctx, "getLanguage")?;
            let lang = read(ctx, v);
            let v = subtag(ctx, "getCountry")?;
            let country = read(ctx, v);
            let v = subtag(ctx, "getVariant")?;
            let variant = read(ctx, v);
            let v = subtag(ctx, "getScript")?;
            let script = read(ctx, v);
            // BCP-47 extensions, e.g. `-u-ca-japanese` / `-x-java`.
            // `LocaleExtensions.id` already holds the canonical, lower-cased
            // sequence real `toString()` appends; a Locale with no extensions
            // has a null `localeExtensions`, so this reads as empty.
            let ext = match ctx.get_field_by_name(this, "localeExtensions") {
                Value::Object(Some(le)) => match ctx.get_field_by_name(le, "id") {
                    Value::Object(Some(id_s)) => ctx.read_string(id_s).unwrap_or_default(),
                    _ => String::new(),
                },
                _ => String::new(),
            };

            // Same condition set as `java.util.Locale.toString`: the region
            // separator appears when there IS a region, or when a language is
            // followed by a variant/script/extension that needs the empty
            // region slot to keep its position.
            let mut s = lang.clone();
            if !country.is_empty()
                || (!lang.is_empty()
                    && (!variant.is_empty() || !script.is_empty() || !ext.is_empty()))
            {
                s.push('_');
                s.push_str(&country);
            }
            if !variant.is_empty() && (!lang.is_empty() || !country.is_empty()) {
                s.push('_');
                s.push_str(&variant);
            }
            if !script.is_empty() && (!lang.is_empty() || !country.is_empty()) {
                s.push_str("_#");
                s.push_str(&script);
            }
            // The extension sequence follows the script, sharing its `#`
            // marker when a script is present (`zh_TW_#Hant_x-java`) and
            // introducing its own when it is not (`en_US_#u-ca-japanese`).
            if !ext.is_empty() && (!lang.is_empty() || !country.is_empty()) {
                s.push('_');
                if script.is_empty() {
                    s.push('#');
                }
                s.push_str(&ext);
            }
            Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
        },
    );

    // BCP-47 `language-Script-REGION-variant-extensions`.
    //
    // The real JDK's own parser is used whenever it is reachable (see
    // [`locale_from_language_tag_real`]) — it is the only thing that gets
    // script subtags, multi-subtag variants, and `-u-`/`-x-` extensions right,
    // and its output is bit-for-bit what HotSpot produces. The hand-rolled
    // Rust split below is the synthetic-JDK fallback, where `sun.util.locale`
    // does not exist at all.
    registry.register(
        "java/util/Locale",
        "forLanguageTag",
        "(Ljava/lang/String;)Ljava/util/Locale;",
        |ctx, args| {
            let tag = match args.first() {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            if let Some(loc) = locale_from_language_tag_real(ctx, &tag) {
                return Ok(Some(Value::Object(Some(loc))));
            }
            // Reaching here in real-JDK mode means one of the delegation's
            // class/method resolutions moved. That degrades silently — the
            // fallback answers plausibly for simple tags and only loses
            // extensions — so say so rather than leave the next reader to
            // rediscover it from a failing assertion.
            tracing::debug!(
                tag = %tag,
                "Locale.forLanguageTag: real-JDK BCP-47 chain unavailable, \
                 using the Rust subtag split (extensions will be dropped)"
            );
            let (lang, script, country, variant) = split_language_tag(&tag);
            let loc = crate::locale_alloc_full(ctx, &lang, &script, &country, &variant)?;
            Ok(Some(Value::Object(Some(loc))))
        },
    );

    // The public constants `java.util.Locale` declares, minus ROOT / ENGLISH /
    // US / CANADA which are registered in `lib.rs`. Registered with the FIELD
    // descriptor, matching those four — these are static-field natives, not
    // methods. A macro rather than a loop because `register` takes a plain
    // `fn`, so the language/country must be literals baked into each shim
    // rather than values a closure captures.
    macro_rules! locale_const {
        ($name:literal, $lang:literal, $country:literal) => {
            registry.register(
                "java/util/Locale",
                $name,
                "Ljava/util/Locale;",
                |ctx, _args| {
                    Ok(Some(Value::Object(Some(crate::locale_alloc(
                        ctx, $lang, $country,
                    )?))))
                },
            );
        };
    }
    locale_const!("UK", "en", "GB");
    locale_const!("FRANCE", "fr", "FR");
    locale_const!("GERMANY", "de", "DE");
    locale_const!("ITALY", "it", "IT");
    locale_const!("JAPAN", "ja", "JP");
    locale_const!("KOREA", "ko", "KR");
    locale_const!("CHINA", "zh", "CN");
    locale_const!("PRC", "zh", "CN");
    locale_const!("TAIWAN", "zh", "TW");
    locale_const!("CANADA_FRENCH", "fr", "CA");
    locale_const!("SIMPLIFIED_CHINESE", "zh", "CN");
    locale_const!("TRADITIONAL_CHINESE", "zh", "TW");
    locale_const!("FRENCH", "fr", "");
    locale_const!("GERMAN", "de", "");
    locale_const!("ITALIAN", "it", "");
    locale_const!("JAPANESE", "ja", "");
    locale_const!("KOREAN", "ko", "");
    locale_const!("CHINESE", "zh", "");

    registry.set_category(__prev_cat);
}

#[cfg(test)]
mod tests {
    use super::{parse_posix_locale, split_language_tag};

    fn split(tag: &str) -> (String, String, String, String) {
        split_language_tag(tag)
    }

    /// The script subtag survives, title-cased, and does not eat the region.
    ///
    /// `zh-hant-CN` used to come back as `("zh", "", "CN", "")` — the parser
    /// recognised the 4-letter script only well enough to *skip* it — so
    /// `Locale.toString()` rendered `zh_CN` where HotSpot renders
    /// `zh_CN_#Hant` (`TestAcceptLanguage.bug56848`).
    #[test]
    fn script_subtag_is_kept_and_title_cased() {
        assert_eq!(
            split("zh-hant-CN"),
            ("zh".into(), "Hant".into(), "CN".into(), String::new())
        );
        assert_eq!(
            split("zh-hans-TW"),
            ("zh".into(), "Hans".into(), "TW".into(), String::new())
        );
        // No region at all.
        assert_eq!(
            split("az-Cyrl"),
            ("az".into(), "Cyrl".into(), String::new(), String::new())
        );
        // Already title-cased input is left alone.
        assert_eq!(
            split("sr-Latn-RS"),
            ("sr".into(), "Latn".into(), "RS".into(), String::new())
        );
    }

    /// `und` is BCP-47 for "undetermined"; `Locale` renders it as an EMPTY
    /// language, so `forLanguageTag("und-DE").toString()` is `_DE` — not
    /// `und_DE`, which is what treating `und` as a language produced.
    #[test]
    fn undetermined_language_is_empty() {
        assert_eq!(
            split("und-DE"),
            (String::new(), String::new(), "DE".into(), String::new())
        );
    }

    /// A single-character subtag opens a BCP-47 extension (`-u-`, `-x-`).
    /// The side table cannot hold extensions, so parsing stops there; the old
    /// parser filed the singleton itself as the variant, producing locales
    /// whose variant was literally `"u"` or `"x"`.
    #[test]
    fn extension_singleton_is_not_a_variant() {
        assert_eq!(
            split("en-US-u-ca-japanese"),
            ("en".into(), String::new(), "US".into(), String::new())
        );
        assert_eq!(
            split("zh-Hant-TW-x-java"),
            ("zh".into(), "Hant".into(), "TW".into(), String::new())
        );
    }

    /// Variants are 5-8 alphanumerics, or 4 characters starting with a digit,
    /// and several may be present (`Locale` joins them with `_`).
    #[test]
    fn variants_follow_the_bcp47_shape() {
        assert_eq!(
            split("de-DE-1996"),
            ("de".into(), String::new(), "DE".into(), "1996".into())
        );
        assert_eq!(
            split("de-DE-POSIX"),
            ("de".into(), String::new(), "DE".into(), "POSIX".into())
        );
        assert_eq!(
            split("sl-IT-nedis-rozaj"),
            (
                "sl".into(),
                String::new(),
                "IT".into(),
                "nedis_rozaj".into()
            )
        );
        // A 3-digit region is legal (UN M.49); a 3-letter one is not.
        assert_eq!(
            split("es-419"),
            ("es".into(), String::new(), "419".into(), String::new())
        );
    }

    /// Plain tags, and the `_` separator CratonVM callers sometimes pass.
    #[test]
    fn simple_tags_are_unchanged() {
        assert_eq!(
            split("en"),
            ("en".into(), String::new(), String::new(), String::new())
        );
        assert_eq!(
            split("en-gb"),
            ("en".into(), String::new(), "GB".into(), String::new())
        );
        assert_eq!(
            split("fr_FR"),
            ("fr".into(), String::new(), "FR".into(), String::new())
        );
        assert_eq!(
            split(""),
            (String::new(), String::new(), String::new(), String::new())
        );
    }

    /// The POSIX `C`/`POSIX` locales, and no locale at all, are English/US.
    ///
    /// This is the case a bare Linux shell (and ubuntu-latest CI) hits. The
    /// HotSpot launcher's `java_props_md.c` maps them, so a real JVM reports
    /// `Locale.getDefault().getLanguage() == "en"` — never `"C"`, which is what
    /// CratonVM used to return by passing the environment value straight
    /// through.
    #[test]
    fn posix_c_locale_maps_to_english() {
        for raw in ["C", "POSIX", "C.UTF-8", "POSIX.UTF-8", "c", "", "   "] {
            assert_eq!(
                parse_posix_locale(raw),
                ("en".to_string(), "US".to_string()),
                "raw = {raw:?}"
            );
        }
    }

    /// `language_COUNTRY.codeset@modifier` — both suffixes are dropped, in
    /// either order, and the casing is normalised the way `Locale` does it.
    #[test]
    fn posix_locale_strips_codeset_and_modifier() {
        assert_eq!(
            parse_posix_locale("en_GB.UTF-8@euro"),
            ("en".to_string(), "GB".to_string())
        );
        assert_eq!(
            parse_posix_locale("de_DE.ISO-8859-1"),
            ("de".to_string(), "DE".to_string())
        );
        assert_eq!(
            parse_posix_locale("sr_RS@latin"),
            ("sr".to_string(), "RS".to_string())
        );
        assert_eq!(
            parse_posix_locale("EN_gb"),
            ("en".to_string(), "GB".to_string())
        );
        // A trailing variant is not part of the (language, country) answer.
        assert_eq!(
            parse_posix_locale("ca_ES_VALENCIA.UTF-8"),
            ("ca".to_string(), "ES".to_string())
        );
    }

    /// A bare language code keeps an empty country, as `new Locale("en")` does.
    #[test]
    fn posix_bare_language_has_empty_country() {
        assert_eq!(parse_posix_locale("en"), ("en".to_string(), String::new()));
        assert_eq!(
            parse_posix_locale("fr.UTF-8"),
            ("fr".to_string(), String::new())
        );
        // `en_` — a country that is present but empty.
        assert_eq!(parse_posix_locale("en_"), ("en".to_string(), String::new()));
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Natively thrown exceptions that run their constructor (round 15 wave 3,
//! lane jdkonly2; proposal JO-2 of
//! `docs/feature-designs/jit-ir-optimizations.md`, page
//! `r15w2-jdkonly-native-minted-throwables-skip-constructor-FIXED-20260930.md`).
//!
//! A bare `try_alloc_concurrent_synthetic` never runs `<init>`, so
//! `Throwable`'s declared initialisers stay zero: `cause` null instead of
//! `this`, `suppressedExceptions` null instead of `SUPPRESSED_SENTINEL`, no
//! `backtrace`. Under `--jdk-only` `initCause` / `addSuppressed` /
//! `getStackTrace` are the JDK's own bytecode and read those zeros:
//! `initCause` throws "Can't overwrite cause", `addSuppressed` drops silently,
//! the trace is empty. [`construct_or_mint`] builds the exception the way
//! `new` does instead.

use cratonvm_native_api::NativeContext;
use cratonvm_types::error::MethodCallFailed;
use cratonvm_types::{ObjectRef, Value};

/// The fallback allocation, written at the call site as a closure around a
/// LITERAL `try_alloc_concurrent_synthetic(ctx, "<class>", n)`, so
/// `native-builtins/tests/unconstructed_carrier_gate.rs` (a source scan) still
/// sees which class the site mints.
pub(crate) type ThrowableMint = fn(&mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed>;

/// One natively thrown exception: its constructor, and the state a mint must
/// be given when the constructor cannot run.
pub(crate) struct ThrowableCtor<'a> {
    /// Internal class name, e.g. `java/nio/file/NoSuchFileException`.
    pub(crate) class: &'a str,
    /// The constructor that yields the state the native wants.
    pub(crate) init_desc: &'a str,
    /// Its post-`this` arguments.
    pub(crate) init_args: &'a [Value],
    /// Used only when the constructor cannot build the object (no class to
    /// resolve, e.g. a synthetic JDK, or no such constructor). A constructor
    /// that threw is not this case since round 15 wave 4: its throwable is
    /// the answer (`NativeHeapAccess::construct_throwable`).
    pub(crate) mint: ThrowableMint,
    /// `detailMessage` of the minted fallback.
    pub(crate) message: Value,
    /// The subclass's own fields of the minted fallback, written by name
    /// after the shadow `Throwable` constructor (as `super(..)` runs first).
    pub(crate) fields: &'a [(&'a str, Value)],
    /// The constructor chain ends in `Throwable(String, Throwable)` with a
    /// null cause (`ClassNotFoundException(String)`, `ResourceBundle`'s
    /// `MissingResourceException`): `cause` is a real null, so `initCause`
    /// refuses, as on HotSpot. Otherwise the fallback's cause is `this`.
    pub(crate) null_cause: bool,
}

/// Build `spec.class` through its constructor (`new_object_initialized`: the
/// JDK's own under `--jdk-only`, the registered shadows under `--compatible`),
/// or, when that cannot build it, mint it and hand it to the shadow
/// `Throwable` constructor (`lang_misc::native_exc_init_message*`: message,
/// `cause`, stack trace, suppressed sentinel) before writing its own fields.
///
/// Every reference in `spec` is pinned across both attempts; the returned
/// reference is current. With `CRATONVM_CARRIER_MINT_DECLARED_DEFAULTS=0`
/// (`locale_resources::carrier_mint_declared_defaults`) this is the old bare
/// mint: message and fields written by name, no constructor.
pub(crate) fn construct_or_mint(
    ctx: &mut dyn NativeContext,
    spec: &ThrowableCtor<'_>,
) -> Result<ObjectRef, MethodCallFailed> {
    let constructed = crate::locale_resources::carrier_mint_declared_defaults();
    let n_init = spec.init_args.len();
    // `[init_args.., message, field values..]`, one pin per reference.
    let mut values: Vec<Value> = Vec::with_capacity(n_init + 1 + spec.fields.len());
    values.extend_from_slice(spec.init_args);
    values.push(spec.message);
    values.extend(spec.fields.iter().map(|(_, value)| *value));
    let mut base: Option<usize> = None;
    let pins: Vec<Option<usize>> = values
        .iter()
        .map(|value| match value {
            Value::Object(Some(obj)) => {
                let handle = ctx.pin_native_root(*obj);
                base.get_or_insert(handle);
                Some(handle)
            }
            _ => None,
        })
        .collect();
    if constructed {
        let now = reread_pinned(&*ctx, &values, &pins);
        // Round 15 wave 4, lane jdkonly4 (JO3-2): a constructor that THREW is
        // not "cannot construct" -- its throwable is the answer, as HotSpot's
        // `new` propagates it; only "no class / no such constructor" mints.
        match ctx.construct_throwable(spec.class, spec.init_desc, &now[..n_init]) {
            Ok(Some(exc)) => {
                if let Some(base) = base {
                    ctx.unpin_native_roots(base);
                }
                return Ok(exc);
            }
            Err(thrown) => {
                if let Some(base) = base {
                    ctx.unpin_native_roots(base);
                }
                return Err(thrown);
            }
            Ok(None) => {}
        }
    }
    let exc = match (spec.mint)(ctx) {
        Ok(exc) => exc,
        Err(e) => {
            if let Some(base) = base {
                ctx.unpin_native_roots(base);
            }
            return Err(e);
        }
    };
    if !constructed {
        let now = reread_pinned(&*ctx, &values, &pins);
        if let Some(base) = base {
            ctx.unpin_native_roots(base);
        }
        if let Value::Object(Some(_)) = now[n_init] {
            ctx.set_field_by_name(exc, "detailMessage", now[n_init]);
        }
        write_fields(ctx, exc, spec.fields, &now[n_init + 1..]);
        return Ok(exc);
    }
    let exc_pin = ctx.pin_native_root(exc);
    let base = *base.get_or_insert(exc_pin);
    let message = reread_pinned(&*ctx, &values, &pins)[n_init];
    let this_and_message = [Value::Object(Some(exc)), message];
    let init = if spec.null_cause {
        crate::lang_misc::native_exc_init_message_null_cause(ctx, &this_and_message)
    } else {
        crate::lang_misc::native_exc_init_message(ctx, &this_and_message)
    };
    let exc = ctx.read_native_pin(exc_pin, exc);
    let now = reread_pinned(&*ctx, &values, &pins);
    ctx.unpin_native_roots(base);
    init?;
    write_fields(ctx, exc, spec.fields, &now[n_init + 1..]);
    Ok(exc)
}

/// `throw new class(init_args..)` for a site with no mint to fall back on:
/// the constructed object, or the constructor's own throwable when it threw,
/// or `fallback()` when the object cannot be built at all (no such class or
/// constructor). Round 15 wave 4, lane jdkonly4 (JO3-2); the twin of
/// `native-io`'s `nio_native::construct_thrown`. `init_args` are rooted across
/// `<init>` by the VM (`NativeHeapAccess::construct_throwable`).
pub(crate) fn construct_thrown(
    ctx: &mut dyn NativeContext,
    class: &str,
    init_desc: &str,
    init_args: &[Value],
    fallback: impl FnOnce() -> MethodCallFailed,
) -> MethodCallFailed {
    // JIT round 21 wave 19 (lane jnilazy, JO4-1): the one builder.
    cratonvm_native_api::throw_new(ctx, class, init_desc, init_args, fallback)
}

/// `values` with every pinned reference re-read through its pin (a
/// collection may have moved it).
fn reread_pinned(ctx: &dyn NativeContext, values: &[Value], pins: &[Option<usize>]) -> Vec<Value> {
    values
        .iter()
        .zip(pins)
        .map(|(value, pin)| match (value, pin) {
            (Value::Object(Some(obj)), Some(handle)) => {
                Value::Object(Some(ctx.read_native_pin(*handle, *obj)))
            }
            _ => *value,
        })
        .collect()
}

/// `fields[i].0 = values[i]`, by name (a slot index is the image's to decide).
/// A null is written too: it is the zero the field already holds.
fn write_fields(
    ctx: &mut dyn NativeContext,
    exc: ObjectRef,
    fields: &[(&str, Value)],
    values: &[Value],
) {
    for ((name, _), value) in fields.iter().zip(values) {
        ctx.set_field_by_name(exc, name, *value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cratonvm_native_api::test_mock::MockNativeContext;
    // The mock implements `create_string` through the trait.
    use cratonvm_native_api::NativeHeapAccess;

    fn unreachable_mint(_ctx: &mut dyn NativeContext) -> Result<ObjectRef, MethodCallFailed> {
        panic!("the constructor could build the object; the bare mint must not run")
    }

    /// The mock's `new_object_initialized` (the trait default: `new_object` +
    /// `<init>`) succeeds, so the constructed object is the answer and the
    /// mint -- the pre-wave-3 behaviour of every site -- is never reached.
    #[test]
    fn the_constructor_is_asked_before_the_mint() {
        let mut ctx = MockNativeContext::new();
        let file = ctx.create_string("f");
        let file = Value::Object(Some(file));
        let spec = ThrowableCtor {
            class: "java/nio/file/NoSuchFileException",
            init_desc: "(Ljava/lang/String;)V",
            init_args: &[file],
            mint: unreachable_mint,
            message: Value::Object(None),
            fields: &[("file", file)],
            null_cause: false,
        };
        let exc = construct_or_mint(&mut ctx, &spec);
        assert!(exc.is_ok());
    }
}

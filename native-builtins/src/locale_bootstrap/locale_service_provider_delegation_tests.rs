use super::*;
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};
use cratonvm_types::error::MethodCallResult;

/// `LocaleProviderAdapter.findAdapter` accepts an adapter only when
/// `adapter.getLocaleServiceProvider(spi)` is non-null. This native used to
/// answer `null` for EVERY spi, so no adapter was ever accepted and
/// `getAdapter` fell through to `fallbackLocaleProviderAdapter` -- which on
/// JDK 21 (and not on 25) returns its root `LocaleResources` for any
/// locale. That is the whole JDK 21 strict-mode locale defect: every locale
/// formatted as English, with nothing missing and nothing thrown.
///
/// These are mock tests because the defect is invisible on a JDK 25 image,
/// and a JDK 25 image is what CI has.
fn adapter_and_spi_mirror(
    ctx: &mut MockNativeContext,
    spi_class: &str,
) -> (ObjectRef, ObjectRef) {
    let adapter_cid = ctx
        .ensure_class_initialized("sun/util/locale/provider/CLDRLocaleProviderAdapter")
        .expect("declare adapter");
    let adapter = ctx.alloc_object(adapter_cid, 0);
    let spi_cid = ctx
        .ensure_class_initialized(spi_class)
        .expect("declare spi");
    let mirror = ctx.get_class_mirror(spi_cid);
    (adapter, mirror)
}

/// The delegation must be a call to the receiver's own
/// `getDecimalFormatSymbolsProvider`, with the JDK signature. Asserting
/// only "non-null was returned" would also pass for a stub that invented
/// some other object.
fn record_call(
    _ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method_name == "getDecimalFormatSymbolsProvider"
        && descriptor == "()Ljava/text/spi/DecimalFormatSymbolsProvider;"
    {
        // A sentinel Int is not a legal return here; it is used BECAUSE it
        // cannot be confused with anything the blanket-null path produces.
        Some(Ok(Some(Value::Int(0x0DF5))))
    } else if method_name == "getCalendarDataProvider"
        && descriptor == "()Ljava/util/spi/CalendarDataProvider;"
    {
        Some(Ok(Some(Value::Int(0x0CDA))))
    } else if method_name == "getTimeZoneNameProvider"
        && descriptor == "()Ljava/util/spi/TimeZoneNameProvider;"
    {
        Some(Ok(Some(Value::Int(0x07A7))))
    } else {
        None
    }
}

#[test]
fn the_decimal_format_symbols_provider_is_delegated_to_the_real_getter() {
    let mut ctx = MockNativeContext::new();
    let (adapter, mirror) =
        adapter_and_spi_mirror(&mut ctx, "java/text/spi/DecimalFormatSymbolsProvider");
    ctx.set_invoke_virtual_hook(record_call);

    let got = jre_adapter_get_locale_service_provider(
        &mut ctx,
        &[Value::Object(Some(adapter)), Value::Object(Some(mirror))],
    );

    assert_eq!(
        got.ok().flatten(),
        Some(Value::Int(0x0DF5)),
        "the DecimalFormatSymbolsProvider arm did not delegate; a null here \
         makes findAdapter skip every adapter and fall through to the \
         fallback one, whose LocaleResources is ROOT on JDK 21"
    );
}

/// Wave-6's arm. `Calendar.getInstance(de-DE)` reported Sunday / 1 minimal
/// day -- the same as every other locale -- because
/// `CalendarDataUtility.retrieveFirstDayOfWeek` reaches its provider
/// through `LocaleServiceProviderPool`, the pool reaches every candidate
/// through this method, and this method answered null for all of them.
#[test]
fn the_calendar_data_provider_is_delegated_to_the_real_getter() {
    let mut ctx = MockNativeContext::new();
    let (adapter, mirror) =
        adapter_and_spi_mirror(&mut ctx, "java/util/spi/CalendarDataProvider");
    ctx.set_invoke_virtual_hook(record_call);

    let got = jre_adapter_get_locale_service_provider(
        &mut ctx,
        &[Value::Object(Some(adapter)), Value::Object(Some(mirror))],
    );

    assert_eq!(
        got.ok().flatten(),
        Some(Value::Int(0x0CDA)),
        "the CalendarDataProvider arm did not delegate; a null here makes \
         the pool find no provider at all, and every locale then gets \
         CalendarDataUtility's own 1/1 defaults"
    );
}

/// 2026-09-12's arm. With a null here `getAdapter(TimeZoneNameProvider,
/// loc)` fell through to FALLBACK, whose provider does not derive CLDR's
/// inherited (empty) zone names, and `SimpleDateFormat.parse("... CET")`
/// threw `NullPointerException: "zoneName" is null` (Tomcat
/// `TestConcurrentDateFormat.testFormatReturnsGMTAfterParseCET`).
#[test]
fn the_time_zone_name_provider_is_delegated_to_the_real_getter() {
    let mut ctx = MockNativeContext::new();
    let (adapter, mirror) =
        adapter_and_spi_mirror(&mut ctx, "java/util/spi/TimeZoneNameProvider");
    ctx.set_invoke_virtual_hook(record_call);

    let got = jre_adapter_get_locale_service_provider(
        &mut ctx,
        &[Value::Object(Some(adapter)), Value::Object(Some(mirror))],
    );

    assert_eq!(
        got.ok().flatten(),
        Some(Value::Int(0x07A7)),
        "the TimeZoneNameProvider arm did not delegate; a null here leaves \
         zone names to the FALLBACK adapter, which keeps CLDR's empty \
         inherited names and lets SimpleDateFormat NPE on them"
    );
}

/// The control that makes the test above mean something. Without it, an arm
/// that delegated for EVERY spi class would pass, and every one of the
/// remaining SPIs would silently start walking the resource-bundle chain
/// this override exists to bypass.
#[test]
fn an_spi_that_has_no_arm_yet_still_answers_null() {
    let mut ctx = MockNativeContext::new();
    // `CalendarDataProvider` used to be this control's example; it has an
    // arm of its own since wave-6, so the control moved to an SPI that
    // still has none. `BreakIteratorProvider` is one of the ~11 left.
    let (adapter, mirror) =
        adapter_and_spi_mirror(&mut ctx, "java/text/spi/BreakIteratorProvider");
    ctx.set_invoke_virtual_hook(record_call);

    let got = jre_adapter_get_locale_service_provider(
        &mut ctx,
        &[Value::Object(Some(adapter)), Value::Object(Some(mirror))],
    );

    assert_eq!(
        got.ok().flatten(),
        Some(Value::Object(None)),
        "an SPI with no arm must still answer null -- delegating for all of \
         them at once is exactly what the wave-4 note forbids"
    );
}

/// A near miss: same simple name, different package. The match is on the
/// fully qualified name, so a class that merely ENDS the same way must not
/// be claimed.
#[test]
fn a_same_named_class_in_another_package_is_not_claimed() {
    let mut ctx = MockNativeContext::new();
    let (adapter, mirror) =
        adapter_and_spi_mirror(&mut ctx, "com/example/spi/DecimalFormatSymbolsProvider");
    ctx.set_invoke_virtual_hook(record_call);

    let got = jre_adapter_get_locale_service_provider(
        &mut ctx,
        &[Value::Object(Some(adapter)), Value::Object(Some(mirror))],
    );

    assert_eq!(
        got.ok().flatten(),
        Some(Value::Object(None)),
        "the match must be on the fully qualified name, not a suffix"
    );
}

/// The degrade path. If the real getter throws, the arm must fall back to
/// the legacy `null` rather than propagate -- otherwise this change could
/// turn a wrong-but-working format call into a throw out of
/// `DecimalFormatSymbols.getInstance` on an image that cannot walk the
/// resource-bundle chain. Without this test the fallback is one `return`
/// away from being deleted as dead code by someone tidying the match.
#[test]
fn a_throwing_getter_degrades_to_the_legacy_null_instead_of_propagating() {
    fn throwing_call(
        _ctx: &mut MockNativeContext,
        _receiver: ObjectRef,
        method_name: &str,
        _descriptor: &str,
        _args: &[Value],
    ) -> Option<MethodCallResult> {
        if method_name == "getDecimalFormatSymbolsProvider" {
            Some(Err(
                cratonvm_types::error::RuntimeError::IllegalAccessException {
                    message: "the resource-bundle chain is unavailable".to_string(),
                }
                .into(),
            ))
        } else {
            None
        }
    }

    let mut ctx = MockNativeContext::new();
    let (adapter, mirror) =
        adapter_and_spi_mirror(&mut ctx, "java/text/spi/DecimalFormatSymbolsProvider");
    ctx.set_invoke_virtual_hook(throwing_call);

    let got = jre_adapter_get_locale_service_provider(
        &mut ctx,
        &[Value::Object(Some(adapter)), Value::Object(Some(mirror))],
    );

    assert_eq!(
        got.ok().flatten(),
        Some(Value::Object(None)),
        "a throwing getter must degrade to null; propagating would make this \
         arm capable of breaking a call that merely formatted wrongly before"
    );
}

/// A missing or null Class argument must not panic and must not delegate:
/// the JDK contract for an unrecognised spi is the null that
/// `findAdapter` is written to skip.
#[test]
fn a_null_spi_argument_answers_null_rather_than_delegating() {
    let mut ctx = MockNativeContext::new();
    let (adapter, _mirror) =
        adapter_and_spi_mirror(&mut ctx, "java/text/spi/DecimalFormatSymbolsProvider");
    ctx.set_invoke_virtual_hook(record_call);

    let got = jre_adapter_get_locale_service_provider(
        &mut ctx,
        &[Value::Object(Some(adapter)), Value::Object(None)],
    );

    assert_eq!(got.ok().flatten(), Some(Value::Object(None)));
}

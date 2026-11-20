//! Interpreter round i1, wave 9, lane L2: a synthetic enum native that
//! answers a constant answers THE constant the class published (JLS 8.9),
//! not a fresh copy
//! (`interpreter-L2-synthetic-enum-values-mint-fresh-constants-FIXED-20260925`).
// Registrars are called by path, not imported: the registrar drift gate
// reads a module-level `use` as a shipping root.
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeHeapAccess, NativeMethodRegistry,
    NativeSystemAccess,
};
use cratonvm_types::{ClassId, Value};

const TU: &str = "java/util/concurrent/TimeUnit";

/// Declare `names` as static fields of `class`'s own type (slots
/// `0..names.len()`), as the synthetic class model does.
fn declare_enum_statics(ctx: &mut MockNativeContext, class: &str, names: &[&str]) -> ClassId {
    let cid = ctx
        .ensure_class_initialized(class)
        .expect("mock registers the class");
    let descriptor = format!("L{class};");
    let fields = names
        .iter()
        .enumerate()
        .map(|(slot, name)| FieldMetadata {
            name: (*name).to_string(),
            descriptor: descriptor.clone(),
            access_flags: 0x0019, // public static final
            slot_index: slot,
            declaring_class_id: cid,
            is_static: true,
        })
        .collect();
    ctx.set_declared_fields(cid, fields);
    cid
}

fn object(v: Result<Option<Value>, cratonvm_types::error::MethodCallFailed>) -> Value {
    match v {
        Ok(Some(v @ Value::Object(Some(_)))) => v,
        other => panic!("expected an object, got {other:?}"),
    }
}

/// `TimeUnit.values()[i]` and `TimeUnit.valueOf(name)` are the statics
/// `<clinit>` published; `valueOf` refuses an unknown name and a null one
/// the way `Enum.valueOf` does.
#[test]
fn time_unit_values_and_value_of_answer_the_published_constants() {
    let mut r = NativeMethodRegistry::new();
    super::register_timeunit_natives(&mut r);
    let clinit = r.find(TU, "<clinit>", "()V").expect("registered");
    let values = r
        .find(TU, "values", "()[Ljava/util/concurrent/TimeUnit;")
        .expect("registered");
    let value_of = r
        .find(
            TU,
            "valueOf",
            "(Ljava/lang/String;)Ljava/util/concurrent/TimeUnit;",
        )
        .expect("registered");
    let mut ctx = mock_ctx();
    let cid = declare_enum_statics(&mut ctx, TU, &super::TIME_UNIT_NAMES);
    clinit(&mut ctx, &[]).expect("<clinit>");
    let published: Vec<Value> = (0..super::TIME_UNIT_NAMES.len())
        .map(|slot| ctx.get_static_field(cid, slot))
        .collect();
    assert!(published
        .iter()
        .all(|v| matches!(v, Value::Object(Some(_)))));

    let Value::Object(Some(arr)) = object(values(&mut ctx, &[])) else {
        unreachable!("`object` answers only an object")
    };
    assert_eq!(ctx.array_length(arr), published.len());
    for (i, constant) in published.iter().enumerate() {
        assert_eq!(ctx.get_array_element(arr, i), *constant, "values()[{i}]");
    }
    let Value::Object(Some(again)) = object(values(&mut ctx, &[])) else {
        unreachable!("`object` answers only an object")
    };
    assert_ne!(again, arr, "values() is a fresh array each call");

    let seconds = ctx.create_string("SECONDS");
    assert_eq!(
        object(value_of(&mut ctx, &[Value::Object(Some(seconds))])),
        published[3]
    );
    let bogus = ctx.create_string("FORTNIGHTS");
    assert!(value_of(&mut ctx, &[Value::Object(Some(bogus))]).is_err());
    assert!(value_of(&mut ctx, &[Value::Object(None)]).is_err());
}

/// `DayOfWeek`'s phase-52 bodies answer the constants `<clinit>` published
/// (which carry `Enum`'s `name` / `ordinal`, not the bodies' slot-0 int),
/// read those constants' value correctly, and reduce `plus`'s amount
/// before adding it.
#[test]
fn day_of_week_bodies_answer_and_read_the_published_constants() {
    const DOW: &str = "java/time/DayOfWeek";
    const NAMES: [&str; 7] = [
        "MONDAY",
        "TUESDAY",
        "WEDNESDAY",
        "THURSDAY",
        "FRIDAY",
        "SATURDAY",
        "SUNDAY",
    ];
    let mut r = NativeMethodRegistry::new();
    super::register_phase52_time_enums(&mut r);
    let of = r
        .find(DOW, "of", "(I)Ljava/time/DayOfWeek;")
        .expect("registered");
    let get_value = r.find(DOW, "getValue", "()I").expect("registered");
    let values = r
        .find(DOW, "values", "()[Ljava/time/DayOfWeek;")
        .expect("registered");
    let plus = r
        .find(DOW, "plus", "(J)Ljava/time/DayOfWeek;")
        .expect("registered");
    let mut ctx = mock_ctx();
    let cid = declare_enum_statics(&mut ctx, DOW, &NAMES);
    // Published the way `publish_synthetic_enum_constants` does: slot 0
    // `Enum.name`, slot 1 `Enum.ordinal`.
    let mut published = Vec::new();
    for (ord, name) in NAMES.iter().enumerate() {
        let obj = ctx.alloc_object(cid, 2);
        let name_obj = ctx.create_string(name);
        ctx.set_field(obj, 0, Value::Object(Some(name_obj)));
        ctx.set_field(obj, 1, Value::Int(ord as i32));
        ctx.set_static_field(cid, ord, Value::Object(Some(obj)));
        published.push(Value::Object(Some(obj)));
    }
    assert_eq!(object(of(&mut ctx, &[Value::Int(5)])), published[4]);
    assert_eq!(
        get_value(&mut ctx, &[published[4]]).expect("getValue"),
        Some(Value::Int(5)),
        "FRIDAY.getValue()"
    );
    let Value::Object(Some(arr)) = object(values(&mut ctx, &[])) else {
        unreachable!("`object` answers only an object")
    };
    for (i, constant) in published.iter().enumerate() {
        assert_eq!(ctx.get_array_element(arr, i), *constant, "values()[{i}]");
    }
    // Long.MAX_VALUE is a multiple of 7: SUNDAY.plus(it) is SUNDAY.
    assert_eq!(
        object(plus(&mut ctx, &[published[6], Value::Long(i64::MAX)])),
        published[6]
    );
}

/// `p57_alloc_enum` hands back the published constant when the class has
/// one, and still builds a constant for a class that does not.
#[test]
fn p57_alloc_enum_prefers_the_published_constant() {
    const FVR: &str = "java/nio/file/FileVisitResult";
    let mut ctx = mock_ctx();
    let cid = declare_enum_statics(&mut ctx, FVR, &["CONTINUE", "TERMINATE"]);
    let published = ctx.alloc_object(cid, 2);
    ctx.set_static_field(cid, 1, Value::Object(Some(published)));
    use crate::phases_late::nio_file::p57_alloc_enum;
    let answer = p57_alloc_enum(&mut ctx, FVR, "TERMINATE", 1);
    assert_eq!(object(answer), Value::Object(Some(published)));
    // Declared but not published yet, and not declared at all: a
    // constant is still built.
    let unpublished = p57_alloc_enum(&mut ctx, FVR, "CONTINUE", 0);
    assert_ne!(object(unpublished), Value::Object(Some(published)));
    object(p57_alloc_enum(
        &mut ctx,
        "java/text/Normalizer$Form",
        "NFC",
        0,
    ));
}

/// Run `class`'s registered `<clinit>` over statics declared for `names`
/// and return what it published, slot by slot (all non-null).
fn run_clinit(
    r: &NativeMethodRegistry,
    ctx: &mut MockNativeContext,
    class: &str,
    names: &[&str],
) -> (ClassId, Vec<Value>) {
    let clinit = r
        .find(class, "<clinit>", "()V")
        .expect("<clinit> registered");
    let cid = declare_enum_statics(ctx, class, names);
    clinit(ctx, &[]).expect("<clinit>");
    let published: Vec<Value> = (0..names.len())
        .map(|slot| ctx.get_static_field(cid, slot))
        .collect();
    assert!(
        published
            .iter()
            .all(|v| matches!(v, Value::Object(Some(_)))),
        "{class}: every constant published"
    );
    // Idempotent: a second run leaves the constants alone.
    clinit(ctx, &[]).expect("<clinit> again");
    for (slot, constant) in published.iter().enumerate() {
        assert_eq!(
            ctx.get_static_field(cid, slot),
            *constant,
            "{class} slot {slot}"
        );
    }
    (cid, published)
}

fn assert_values_are(ctx: &mut MockNativeContext, arr: Value, published: &[Value]) {
    let Value::Object(Some(arr)) = arr else {
        unreachable!("`object` answers only an object")
    };
    assert_eq!(ctx.array_length(arr), published.len());
    for (i, constant) in published.iter().enumerate() {
        assert_eq!(ctx.get_array_element(arr, i), *constant, "values()[{i}]");
    }
}

/// i10-L2: `Month` publishes its twelve constants, and `of` / `valueOf`
/// / `values` / `plus` / `minus` answer them; `valueOf` is exact and
/// refuses null, as `Enum.valueOf` does.
#[test]
fn i10_month_bodies_answer_the_published_constants() {
    const MONTH: &str = "java/time/Month";
    let mut r = NativeMethodRegistry::new();
    super::register_phase52_time_enums(&mut r);
    let mut ctx = mock_ctx();
    let (_, published) = run_clinit(&r, &mut ctx, MONTH, &super::MONTH_NAMES);
    let of = r.find(MONTH, "of", "(I)Ljava/time/Month;").expect("of");
    let value_of = r
        .find(MONTH, "valueOf", "(Ljava/lang/String;)Ljava/time/Month;")
        .expect("valueOf");
    let values = r
        .find(MONTH, "values", "()[Ljava/time/Month;")
        .expect("values");
    let plus = r.find(MONTH, "plus", "(J)Ljava/time/Month;").expect("plus");
    let minus = r
        .find(MONTH, "minus", "(J)Ljava/time/Month;")
        .expect("minus");
    assert_eq!(object(of(&mut ctx, &[Value::Int(1)])), published[0]);
    assert_eq!(object(of(&mut ctx, &[Value::Int(12)])), published[11]);
    let march = ctx.create_string("MARCH");
    assert_eq!(
        object(value_of(&mut ctx, &[Value::Object(Some(march))])),
        published[2]
    );
    let lower = ctx.create_string("march");
    assert!(
        value_of(&mut ctx, &[Value::Object(Some(lower))]).is_err(),
        "Enum.valueOf is exact"
    );
    assert!(value_of(&mut ctx, &[Value::Object(None)]).is_err());
    let arr = object(values(&mut ctx, &[]));
    assert_values_are(&mut ctx, arr, &published);
    // JANUARY-based, so the answers do not depend on reading the value
    // back out of the mock's constants.
    assert_eq!(
        object(plus(&mut ctx, &[published[0], Value::Long(13)])),
        published[1]
    );
    // `Long.MIN_VALUE % 12 == -8`: JANUARY.minus(it) is SEPTEMBER, and
    // the old `v - 1 - sub` form overflowed here.
    assert_eq!(
        object(minus(&mut ctx, &[published[0], Value::Long(i64::MIN)])),
        published[8]
    );
}

/// i10-L2: `ChronoUnit` publishes its sixteen constants and `valueOf` /
/// `values` answer them.
#[test]
fn i10_chrono_unit_bodies_answer_the_published_constants() {
    let mut r = NativeMethodRegistry::new();
    super::register_phase52_chrono_unit(&mut r);
    let mut ctx = mock_ctx();
    let (_, published) =
        run_clinit(&r, &mut ctx, super::CHRONO_UNIT, &super::CHRONO_UNIT_NAMES);
    let value_of = r
        .find(
            super::CHRONO_UNIT,
            "valueOf",
            "(Ljava/lang/String;)Ljava/time/temporal/ChronoUnit;",
        )
        .expect("valueOf");
    let values = r
        .find(
            super::CHRONO_UNIT,
            "values",
            "()[Ljava/time/temporal/ChronoUnit;",
        )
        .expect("values");
    let ordinal = r
        .find(super::CHRONO_UNIT, "ordinal", "()I")
        .expect("ordinal");
    let days = ctx.create_string("DAYS");
    assert_eq!(
        object(value_of(&mut ctx, &[Value::Object(Some(days))])),
        published[7]
    );
    assert!(value_of(&mut ctx, &[Value::Object(None)]).is_err());
    let arr = object(values(&mut ctx, &[]));
    assert_values_are(&mut ctx, arr, &published);
    assert_eq!(
        ordinal(&mut ctx, &[published[3]]).expect("ordinal"),
        Some(Value::Int(3)),
        "SECONDS.ordinal()"
    );
}

/// i10-L2: `RoundingMode` publishes its eight constants; `valueOf`,
/// `values` and `new MathContext(int)`'s default mode answer them.
#[test]
fn i10_rounding_mode_bodies_answer_the_published_constants() {
    let mut r = NativeMethodRegistry::new();
    super::register_phase52_rounding_mode(&mut r);
    super::register_phase52_math_context(&mut r);
    let mut ctx = mock_ctx();
    let (_, published) = run_clinit(
        &r,
        &mut ctx,
        super::ROUNDING_MODE,
        &super::ROUNDING_MODE_NAMES,
    );
    let value_of = r
        .find(
            super::ROUNDING_MODE,
            "valueOf",
            "(Ljava/lang/String;)Ljava/math/RoundingMode;",
        )
        .expect("valueOf");
    let values = r
        .find(
            super::ROUNDING_MODE,
            "values",
            "()[Ljava/math/RoundingMode;",
        )
        .expect("values");
    let half_even = ctx.create_string("HALF_EVEN");
    assert_eq!(
        object(value_of(&mut ctx, &[Value::Object(Some(half_even))])),
        published[6]
    );
    let arr = object(values(&mut ctx, &[]));
    assert_values_are(&mut ctx, arr, &published);
    let init = r
        .find("java/math/MathContext", "<init>", "(I)V")
        .expect("MathContext.<init>(I)");
    let mc_class = ctx
        .ensure_class_initialized("java/math/MathContext")
        .expect("mock registers the class");
    let mc = ctx.alloc_object(mc_class, 2);
    init(&mut ctx, &[Value::Object(Some(mc)), Value::Int(7)]).expect("<init>");
    assert_eq!(ctx.get_field(mc, 1), published[4], "HALF_UP, the constant");
}

/// i10-L2: `FormatStyle` publishes its four constants, which `values`
/// and `valueOf` (through `p57_alloc_enum`) then answer.
#[test]
fn i10_format_style_bodies_answer_the_published_constants() {
    const FS: &str = "java/time/format/FormatStyle";
    let mut r = NativeMethodRegistry::new();
    crate::phases_late::text_intl::register_p65_datetime_builder(&mut r);
    let mut ctx = mock_ctx();
    let names = ["FULL", "LONG", "MEDIUM", "SHORT"];
    let (_, published) = run_clinit(&r, &mut ctx, FS, &names);
    let value_of = r
        .find(
            FS,
            "valueOf",
            "(Ljava/lang/String;)Ljava/time/format/FormatStyle;",
        )
        .expect("valueOf");
    let values = r
        .find(FS, "values", "()[Ljava/time/format/FormatStyle;")
        .expect("values");
    let short = ctx.create_string("SHORT");
    assert_eq!(
        object(value_of(&mut ctx, &[Value::Object(Some(short))])),
        published[3]
    );
    let arr = object(values(&mut ctx, &[]));
    assert_values_are(&mut ctx, arr, &published);
}

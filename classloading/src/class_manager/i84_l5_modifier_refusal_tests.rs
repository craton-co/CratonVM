use super::{class_modifier_refusal, member_modifier_refusal};
use cratonvm_reader::class_access_flags::{
    ClassAccessFlags, FieldAccessFlags, MethodAccessFlags,
};
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::ConstantPool;
use cratonvm_reader::field::ClassFileField;
use cratonvm_reader::method::ClassFileMethod;
use cratonvm_reader::ClassFile;
use cratonvm_types::error::LinkageError;
use std::sync::Arc;

fn class(flags: u16, major: u16) -> ClassFile {
    ClassFile {
        version: ClassFileVersion { major, minor: 0 },
        constant_pool: ConstantPool::new(vec![]),
        access_flags: ClassAccessFlags::from_bits_retain(flags),
        this_class: Arc::from("p/X"),
        super_class: Some(Arc::from("java/lang/Object")),
        interfaces: vec![],
        fields: vec![],
        methods: vec![],
        attributes: vec![],
    }
}

fn method(name: &str, flags: u16) -> ClassFileMethod {
    ClassFileMethod {
        access_flags: MethodAccessFlags::from_bits_retain(flags),
        name: Arc::from(name),
        descriptor: Arc::from("()V"),
        attributes: vec![],
    }
}

fn field(flags: u16) -> ClassFileField {
    ClassFileField {
        access_flags: FieldAccessFlags::from_bits_retain(flags),
        name: Arc::from("f"),
        descriptor: Arc::from("I"),
        attributes: vec![],
    }
}

fn text(err: Option<LinkageError>) -> Option<String> {
    err.map(|e| match e {
        LinkageError::ClassFormatError {
            class_name,
            message,
        } => format!(
            "CFE {}",
            LinkageError::class_format_error_text(&class_name, &message)
        ),
        LinkageError::NoClassDefFoundError { class_name } => format!("NCDFE {class_name}"),
        other => format!("{other:?}"),
    })
}

#[test]
fn class_modifiers_use_hotspot_words_and_scope() {
    assert_eq!(
        text(class_modifier_refusal(&class(0x0431, 69), "p/X", true)).as_deref(),
        Some("CFE Illegal class modifiers in class p/X: 0x431")
    );
    assert_eq!(
        text(class_modifier_refusal(&class(0x0201, 69), "", true)).as_deref(),
        Some("CFE Illegal class modifiers in class <Unknown>: 0x201"),
        "an interface without abstract; no name is <Unknown>"
    );
    assert_eq!(
        text(class_modifier_refusal(&class(0x0621, 69), "p/X", true)).as_deref(),
        Some("CFE Illegal class modifiers in class p/X: 0x621"),
        "an interface with ACC_SUPER"
    );
    assert!(class_modifier_refusal(&class(0x0431, 69), "p/X", false).is_none());
    assert!(class_modifier_refusal(&class(0x0601, 69), "p/X", true).is_none());
    assert!(class_modifier_refusal(&class(0x2601, 69), "p/X", true).is_none());
    assert!(class_modifier_refusal(&class(0x4031, 69), "p/X", true).is_none());
    assert_eq!(
        text(class_modifier_refusal(&class(0x8000, 69), "module-info", false)).as_deref(),
        Some("NCDFE module-info is not a class because access_flag ACC_MODULE is set"),
        "the module flag is refused in every loader"
    );
    assert!(
        class_modifier_refusal(&class(0x8021, 52), "p/X", true).is_none(),
        "before version 53 the module bit is not recognized"
    );
}

#[test]
fn member_modifiers_use_hotspot_words_and_order() {
    let mut c = class(0x0021, 69);
    c.fields.push(field(0x0003));
    c.methods.push(method("m", 0x0402));
    assert_eq!(
        text(member_modifier_refusal(&c, true)).as_deref(),
        Some("CFE Illegal field modifiers in class p/X: 0x3"),
        "fields are parsed before methods"
    );
    c.fields.clear();
    assert_eq!(
        text(member_modifier_refusal(&c, true)).as_deref(),
        Some("CFE Method m in class p/X has illegal modifiers: 0x402")
    );
    assert!(member_modifier_refusal(&c, false).is_none());
    c.methods = vec![method("<clinit>", 0x0000)];
    assert_eq!(
        text(member_modifier_refusal(&c, false)).as_deref(),
        Some("CFE Method <clinit> is not static in class file p/X"),
        "in every loader"
    );
    c.methods = vec![
        method("<clinit>", 0x0008),
        method("<init>", 0x0001),
        method("a", 0x0401),
        method("s", 0x0809),
    ];
    assert!(member_modifier_refusal(&c, true).is_none());

    let mut i = class(0x0601, 69);
    i.fields.push(field(0x0019));
    i.methods = vec![method("d", 0x0001), method("p", 0x0002), method("a", 0x0401)];
    assert!(member_modifier_refusal(&i, true).is_none());
    i.fields.push(field(0x001A));
    assert_eq!(
        text(member_modifier_refusal(&i, true)).as_deref(),
        Some("CFE Illegal field modifiers in class p/X: 0x1A")
    );
    i.fields.pop();
    i.methods.push(method("q", 0x0000));
    assert_eq!(
        text(member_modifier_refusal(&i, true)).as_deref(),
        Some("CFE Method q in class p/X has illegal modifiers: 0x0"),
        "an interface method is exactly one of public / private"
    );
    i.methods.pop();
    i.methods.push(method("<init>", 0x0001));
    assert_eq!(
        text(member_modifier_refusal(&i, false)).as_deref(),
        Some("CFE Interface cannot have a method named <init>, class file p/X")
    );
}

/// t1 wave 10, lane I24W: `-Xverify:none` (`modifiers_need_verify` and
/// `names_need_verify` false, `need_verify` kept) drops the field / method
/// modifier and member-name legality and keeps the structural checks.
#[test]
fn noverify_relaxes_modifiers_and_names_only() {
    use super::member_refusal;
    let mut c = class(0x0421, 69);
    c.fields.push(field(0x0003));
    c.methods.push(method("m", 0x0411));
    c.methods.push(method("a.b", 0x0401));
    assert_eq!(
        text(member_refusal(&c, true, true, true, true, false)).as_deref(),
        Some("CFE Illegal field modifiers in class p/X: 0x3")
    );
    assert!(member_refusal(&c, true, false, false, true, false).is_none());
    c.fields.clear();
    assert_eq!(
        text(member_refusal(&c, true, true, true, true, false)).as_deref(),
        Some("CFE Method m in class p/X has illegal modifiers: 0x411")
    );
    assert_eq!(
        text(member_refusal(&c, true, true, false, true, false)).as_deref(),
        Some("CFE Method m in class p/X has illegal modifiers: 0x411"),
        "names relaxed alone keep the modifier check"
    );
    c.methods.remove(0);
    assert_eq!(
        text(member_refusal(&c, true, false, true, true, false)).as_deref(),
        Some("CFE Illegal method name \"a.b\" in class p/X")
    );
    assert!(member_refusal(&c, true, false, false, true, false).is_none());
    // Kept with the structural checks on (`CRATONVM_NOVERIFY_RELAXES_STRUCTURAL_CHECKS=0`;
    // see `t1w13_noverify_relaxes_the_structural_member_checks`): a
    // duplicate method, and the every-loader `<clinit>` rule.
    c.methods.push(method("a.b", 0x0401));
    assert_eq!(
        text(member_refusal(&c, true, false, false, true, false)).as_deref(),
        Some("CFE Duplicate method name \"a.b\" with signature \"()V\" in class file p/X")
    );
    c.methods = vec![method("<clinit>", 0x0000)];
    assert_eq!(
        text(member_refusal(&c, true, false, false, true, false)).as_deref(),
        Some("CFE Method <clinit> is not static in class file p/X")
    );
    assert!(class_modifier_refusal(&class(0x0431, 69), "p/X", false).is_none());
}

/// t1 wave 13, lane I24W: with the structural checks relaxed too
/// (`-Xverify:none`, `structural` false), the duplicate members, method
/// descriptors and field descriptors with a layout type go, as on HotSpot
/// 25.0.4.1 (probe `T1W13I24WStructuralFormat` rows S01, S02, S10, S30);
/// a field descriptor with no layout type (S09, HotSpot's own crash) and
/// the every-loader `<clinit>` rule stay.
#[test]
fn t1w13_noverify_relaxes_the_structural_member_checks() {
    use super::member_refusal;
    let mut c = class(0x0421, 69);
    c.methods.push(method("m", 0x0401));
    c.methods.push(method("m", 0x0401));
    assert_eq!(
        text(member_refusal(&c, true, false, false, true, false)).as_deref(),
        Some("CFE Duplicate method name \"m\" with signature \"()V\" in class file p/X")
    );
    assert!(member_refusal(&c, true, false, false, false, false).is_none());
    c.methods = vec![method("m", 0x0401)];
    c.methods[0].descriptor = Arc::from("(V)V");
    assert_eq!(
        text(member_refusal(&c, true, false, false, true, false)).as_deref(),
        Some("CFE Method \"m\" in class p/X has illegal signature \"(V)V\"")
    );
    assert!(member_refusal(&c, true, false, false, false, false).is_none());
    c.methods.clear();
    c.fields = vec![field(0x0001), field(0x0001)];
    assert_eq!(
        text(member_refusal(&c, true, false, false, true, false)).as_deref(),
        Some("CFE Duplicate field name \"f\" with signature \"I\" in class file p/X")
    );
    assert!(member_refusal(&c, true, false, false, false, false).is_none());
    c.fields = vec![field(0x0001)];
    c.fields[0].descriptor = Arc::from("[V");
    assert!(member_refusal(&c, true, false, false, true, false).is_some());
    assert!(member_refusal(&c, true, false, false, false, false).is_none());
    c.fields[0].descriptor = Arc::from("Q");
    assert_eq!(
        text(member_refusal(&c, true, false, false, false, false)).as_deref(),
        Some("CFE Field \"f\" in class p/X has illegal signature \"Q\""),
        "no layout type: HotSpot crashes, this VM keeps the refusal"
    );
    c.fields.clear();
    c.methods = vec![method("<clinit>", 0x0000)];
    assert_eq!(
        text(member_refusal(&c, true, false, false, false, false)).as_deref(),
        Some("CFE Method <clinit> is not static in class file p/X")
    );
}

/// t1 wave 13, lane I24W: the `InnerClasses` index types are judged in
/// every scope (rows S27, S46, S47), the both-outer-and-inner rule only
/// with `structural` (S16).
#[test]
fn t1w13_inner_class_index_types_hold_in_every_scope() {
    use super::inner_class_modifier_refusal;
    use cratonvm_reader::attribute::{Attribute, InnerClassInfo, LazyAttribute};
    use cratonvm_reader::constant_pool::ConstantPoolEntry as E;
    let cp = ConstantPool::new(vec![E::Tombstone, E::ClassReference { name_index: 0 }, E::Integer(7)]);
    let table = |inner: u16, outer: u16, name: u16| {
        vec![LazyAttribute::Decoded(Attribute::InnerClasses(vec![InnerClassInfo {
            inner_class_info_index: inner,
            outer_class_info_index: outer,
            inner_name_index: name,
            inner_class_access_flags: 0x0001,
        }]))]
    };
    let judge = |attrs: &[LazyAttribute], structural: bool| {
        text(inner_class_modifier_refusal(attrs, &cp, 69, "p/X", structural, structural))
    };
    for structural in [true, false] {
        assert_eq!(
            judge(&table(2, 0, 0), structural).as_deref(),
            Some("CFE inner_class_info_index 2 has bad constant type in class file p/X")
        );
        assert_eq!(
            judge(&table(1, 2, 0), structural).as_deref(),
            Some("CFE outer_class_info_index 2 has bad constant type in class file p/X")
        );
        assert_eq!(
            judge(&table(1, 0, 2), structural).as_deref(),
            Some("CFE inner_name_index 2 has bad constant type in class file p/X")
        );
        assert_eq!(judge(&table(1, 0, 0), structural), None);
    }
    assert_eq!(
        judge(&table(1, 1, 0), true).as_deref(),
        Some("CFE Class is both outer and inner class in class file p/X")
    );
    assert_eq!(judge(&table(1, 1, 0), false), None);
}

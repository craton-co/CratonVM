//! j27 wave 4, lane INDYTRAP
//! (`docs/feature-designs/native-dispatch-architecture.md`):
//! a javac `makeConcatWithConstants` site that has NEVER run -- the cold
//! `throw new IllegalArgumentException("neg: " + v)` arm -- gets its
//! compiled concat bridge site from the constant pool alone, with no
//! linkage, no VM and no Java code run, so the single-pass `0xba` arm
//! emits the bridge call and plants no reason-8 trap (`has_indy_trap`
//! stays false). The bridged site renders what `StringConcatFactory`
//! renders. Only a shape javac never emits keeps the trap.

use super::*;
use crate::threading::jvm_thread::ThreadId;
use cratonvm_reader::constant_pool::ConstantPoolEntry as CPE;

/// A class's pool and `BootstrapMethods`, one javac concat site per
/// [`Self::site`] call. cp #1-#7 are the `makeConcatWithConstants`
/// handle every site's bootstrap names (javac emits one per class).
struct JavacPool {
    entries: Vec<CPE>,
    bootstraps: Vec<BootstrapMethod>,
}

impl JavacPool {
    fn new() -> Self {
        let utf8 = |s: &str| CPE::Utf8(s.into());
        let entries = vec![
            CPE::Tombstone,
            utf8(STRING_CONCAT_FACTORY),           // 1
            CPE::ClassReference { name_index: 1 }, // 2
            utf8(MAKE_CONCAT_WITH_CONSTANTS),      // 3
            utf8(
                "(Ljava/lang/invoke/MethodHandles$Lookup;Ljava/lang/String;\
                 Ljava/lang/invoke/MethodType;Ljava/lang/String;[Ljava/lang/Object;)\
                 Ljava/lang/invoke/CallSite;",
            ), // 4
            CPE::NameAndType {
                name_index: 3,
                descriptor_index: 4,
            }, // 5
            CPE::MethodReference {
                class_index: 2,
                name_and_type_index: 5,
            }, // 6
            CPE::MethodHandle {
                reference_kind: 6,
                reference_index: 6,
            }, // 7: REF_invokeStatic
        ];
        Self {
            entries,
            bootstraps: Vec::new(),
        }
    }

    fn push(&mut self, entry: CPE) -> u16 {
        self.entries.push(entry);
        u16::try_from(self.entries.len() - 1).expect("a small test pool")
    }

    fn string(&mut self, text: &str) -> u16 {
        let string_index = self.push(CPE::Utf8(text.into()));
        self.push(CPE::StringReference { string_index })
    }

    /// The `InvokeDynamic` entry of a site with this recipe, these
    /// constants (`constant` gives each one's pool entry) and this call
    /// type.
    fn site(&mut self, recipe: &str, constants: &[u16], descriptor: &str) -> u16 {
        let mut bootstrap_arguments = vec![self.string(recipe)];
        bootstrap_arguments.extend_from_slice(constants);
        self.bootstraps.push(BootstrapMethod {
            bootstrap_method_ref: 7,
            bootstrap_arguments,
        });
        let bootstrap_method_attr_index =
            u16::try_from(self.bootstraps.len() - 1).expect("a small test pool");
        let name_index = self.push(CPE::Utf8(MAKE_CONCAT_WITH_CONSTANTS.into()));
        let descriptor_index = self.push(CPE::Utf8(descriptor.into()));
        let name_and_type_index = self.push(CPE::NameAndType {
            name_index,
            descriptor_index,
        });
        self.push(CPE::InvokeDynamic {
            bootstrap_method_attr_index,
            name_and_type_index,
        })
    }
}

fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// Every operand kind the cold-arm message rows of
/// `tools/probes/jit/j27/J27IndyTrapRows.java` use, through the CP-only
/// door (`singleton_site` `None`, the weakest door): each unlinked site
/// is bridged and answers HotSpot's text.
#[test]
fn an_unlinked_javac_concat_is_bridged_and_renders_the_factory_text() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "j27-indytrap-concat");
    let mut pool = JavacPool::new();
    // javac moves a literal holding a tag unit into a `\2` constant.
    let tagged = pool.string("<\u{1}>");
    let bang = pool.string("!");
    let one_third = f32::to_bits(1.0f32 / 3.0) as i64;
    let big = f32::to_bits(1.0e10f32) as i64;
    let point3 = (0.1f64 + 0.2).to_bits() as i64;
    let neg_zero = (-0.0f64).to_bits() as i64;
    let tiny = 1.0e-5f64.to_bits() as i64;
    let cases: Vec<(u16, Vec<i64>, Vec<u16>)> = vec![
        (pool.site("neg: \u{1}", &[], "(I)Ljava/lang/String;"), vec![-5], units("neg: -5")),
        (
            pool.site("s=\u{1};o=\u{1}", &[], "(Ljava/lang/String;Ljava/lang/Object;)Ljava/lang/String;"),
            vec![0, 0],
            units("s=null;o=null"),
        ),
        (pool.site("c=\u{1}", &[], "(C)Ljava/lang/String;"), vec![0x41], units("c=A")),
        (
            pool.site("f=\u{1},\u{1}", &[], "(FF)Ljava/lang/String;"),
            vec![one_third, big],
            units("f=0.33333334,1.0E10"),
        ),
        (
            pool.site("d=\u{1},\u{1},\u{1}", &[], "(DDD)Ljava/lang/String;"),
            vec![point3, neg_zero, tiny],
            units("d=0.30000000000000004,-0.0,1.0E-5"),
        ),
        (
            pool.site("\u{3a9}=\u{1} \u{1}", &[], "(IC)Ljava/lang/String;"),
            vec![7, 0x3a9],
            units("\u{3a9}=7 \u{3a9}"),
        ),
        (
            pool.site("\u{2}\u{1}\u{2}", &[tagged, bang], "(J)Ljava/lang/String;"),
            vec![i64::MIN],
            units("<\u{1}>-9223372036854775808!"),
        ),
    ];
    let cp = ConstantPool::new(pool.entries.clone());
    let class_id = ClassId::new(0x7_2704);
    for (cp_index, args, want) in cases {
        let site =
            make_jit_indy_bridge_site_from_parts(&cp, &pool.bootstraps, cp_index, class_id, None)
                .unwrap_or(0);
        assert_ne!(site, 0, "cp#{cp_index}: an unlinked javac concat is bridged");
        // SAFETY: `site` was built just above; the allocation is process-lived.
        assert_eq!(unsafe { jit_indy_site_kind(site) }, JIT_INDY_SITE_CONCAT);
        // SAFETY: `site` is the live concat site just built; `args` holds
        // one descriptor-typed slot per operand, every object slot null.
        let r = unsafe {
            execute_jit_string_concat_raw(&shared, &mut thread, site, args.as_ptr(), args.len())
        };
        let Ok(Some(r)) = r else {
            panic!("cp#{cp_index}: the bridged concat did not answer");
        };
        assert_eq!(
            read_java_string_units(&shared.mem.heap, r),
            Some(want),
            "cp#{cp_index}"
        );
        // SAFETY: built by `Box::into_raw` and never shared (not interned).
        drop(unsafe { Box::from_raw(site as *mut JitStringConcatSite) });
    }
}

/// The residual trap set: a `Class` constant (rendered `interface X` for
/// a loaded interface, which the pool cannot tell) or a tag count that
/// disagrees with the call type keeps the reason-8 trap, so the
/// interpreter answers it -- these are the shapes javac never emits.
#[test]
fn a_shape_javac_never_emits_keeps_its_trap() {
    let mut pool = JavacPool::new();
    let class_name = pool.push(CPE::Utf8("java/lang/Runnable".into()));
    let class_constant = pool.push(CPE::ClassReference {
        name_index: class_name,
    });
    let with_class = pool.site("\u{2}\u{1}", &[class_constant], "(I)Ljava/lang/String;");
    let arity = pool.site("\u{1}\u{1}", &[], "(I)Ljava/lang/String;");
    let javac = pool.site("v=\u{1}", &[], "(I)Ljava/lang/String;");
    let cp = ConstantPool::new(pool.entries.clone());
    let class_id = ClassId::new(0x7_2705);
    let site = |cp_index| {
        make_jit_indy_bridge_site_from_parts(&cp, &pool.bootstraps, cp_index, class_id, None)
    };
    assert_eq!(site(with_class), None, "a Class constant");
    assert_eq!(site(arity), None, "two operand tags, one operand");
    assert!(
        resolve_concat_constant_units(&cp, 0).is_none()
            && resolve_concat_constant_units(&cp, u16::MAX).is_none(),
        "R5: unresolvable static constant must be rejected so linkage routes to BootstrapMethodError"
    );
    let bridged = site(javac).expect("the control: javac's shape is bridged");
    let str_u = pool.push(CPE::Utf8("ok".into()));
    let str_c = pool.push(CPE::StringReference { string_index: str_u });
    let cp2 = ConstantPool::new(pool.entries.clone());
    assert!(resolve_concat_constant_units(&cp2, str_c).is_some());
    // SAFETY: built by `Box::into_raw` and never shared (not interned).
    drop(unsafe { Box::from_raw(bridged as *mut JitStringConcatSite) });
}

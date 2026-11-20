use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use crate::test_utils::MockNativeContext;

// Private VM identities and provider names: no other test uses them, so
// these tests need no lock and never touch `TEST_VM`'s shared row.
const W22B_PROVIDER: &str = "W22bTestProvider";

/// Drops every listed VM's registry (and its lock keys, which the
/// `Provider.put` natives mint) even when an assertion fails.
struct Forget(&'static [usize]);
impl Drop for Forget {
    fn drop(&mut self) {
        for vm in self.0 {
            forget_vm_provider_registry(*vm);
            forget_vm_real_provider_roots(*vm);
            crate::forget_vm_lock_keys(*vm);
        }
    }
}

fn names(vm: usize) -> Vec<String> {
    chain_provider_names(vm)
}

fn seed_names() -> Vec<String> {
    seed_chain().into_iter().map(|(name, _, _)| name).collect()
}

#[test]
fn add_insert_and_remove_in_one_vm_are_invisible_to_another() {
    const VM_A: usize = 0x22B0_0A08;
    const VM_B: usize = 0x22B0_0B08;
    let _forget = Forget(&[VM_A, VM_B]);

    assert!(add(VM_A, W22B_PROVIDER.to_string(), 1.5) >= 1);
    assert_eq!(insert_at(VM_A, "W22bFirst".to_string(), 2.0, 1), 1);
    remove(VM_A, "SunJCE");

    let a = names(VM_A);
    assert_eq!(a.first().map(String::as_str), Some("W22bFirst"));
    assert_eq!(a.last().map(String::as_str), Some(W22B_PROVIDER));
    assert!(!a.iter().any(|n| n == "SunJCE"), "A removed SunJCE: {a:?}");
    assert_eq!(
        find(VM_A, W22B_PROVIDER),
        Some((1.5, USER_PROVIDER_COVERAGE))
    );

    // B never wrote: it reads the JDK seed, untouched by A.
    assert_eq!(names(VM_B), seed_names(), "A's chain edits leaked into B");
    assert_eq!(find(VM_B, W22B_PROVIDER), None);
    assert_eq!(find(VM_B, "W22bFirst"), None);
    assert!(
        find(VM_B, "SunJCE").is_some(),
        "A's removeProvider reached B"
    );

    // B's own edit is B's alone, in both directions.
    remove(VM_B, "SUN");
    assert!(find(VM_B, "SUN").is_none());
    assert!(find(VM_A, "SUN").is_some());
    assert_eq!(
        find(VM_A, W22B_PROVIDER),
        Some((1.5, USER_PROVIDER_COVERAGE))
    );
}

#[test]
fn a_put_in_one_vm_is_invisible_to_another() {
    const VM_A: usize = 0x22B0_1A08;
    const VM_B: usize = 0x22B0_1B08;
    let _forget = Forget(&[VM_A, VM_B]);

    add(VM_A, W22B_PROVIDER.to_string(), 1.0);
    with_registry(VM_A, |reg| {
        assert!(reg.apply_legacy_put(W22B_PROVIDER, "Cipher.W22B", "com.example.W22bCipher"));
        assert!(reg.apply_legacy_put(W22B_PROVIDER, "Alg.Alias.Cipher.W22BALIAS", "W22B"));
    });

    assert!(provider_offers(VM_A, W22B_PROVIDER, "Cipher", "w22b"));
    assert_eq!(
        find_service_provider_in(VM_A, "Cipher", "W22B").as_deref(),
        Some(W22B_PROVIDER)
    );
    assert_eq!(
        canonical_service_algorithm(VM_A, None, "Cipher", "w22balias").as_deref(),
        Some("W22B")
    );
    assert_eq!(
        third_party_service_class(VM_A, None, "Cipher", "W22BALIAS").as_deref(),
        Some("com/example/W22bCipher")
    );

    assert!(!provider_offers(VM_B, W22B_PROVIDER, "Cipher", "W22B"));
    assert!(get_service_entry(VM_B, W22B_PROVIDER, "Cipher", "W22B").is_none());
    assert_eq!(find_service_provider_in(VM_B, "Cipher", "W22B"), None);
    assert_eq!(
        canonical_service_algorithm(VM_B, None, "Cipher", "W22BALIAS"),
        None
    );
    assert!(!algorithms_for_service_in(VM_B, "Cipher")
        .iter()
        .any(|a| a == "W22B"));
    assert!(algorithms_for_service_in(VM_A, "Cipher")
        .iter()
        .any(|a| a == "W22B"));
}

/// The same isolation through the `Provider.put` / `getProperty` and
/// `Security.removeProvider` natives, each VM a mock with its own identity.
#[test]
fn the_provider_and_security_natives_write_only_their_own_vm() {
    const VM_A: usize = 0x22B0_2A08;
    const VM_B: usize = 0x22B0_2B08;
    let _forget = Forget(&[VM_A, VM_B]);
    let mut a = MockNativeContext::new();
    a.set_vm_identity(VM_A);
    let mut b = MockNativeContext::new();
    b.set_vm_identity(VM_B);

    let provider = |ctx: &mut MockNativeContext| {
        let prov = ctx.alloc_object(cratonvm_types::ClassId::new(0), 8);
        let name = ctx.create_string(W22B_PROVIDER);
        ctx.set_field(prov, 0, Value::Object(Some(name)));
        ctx.set_field(prov, 1, Value::Double(1.0));
        prov
    };
    let prov_a = provider(&mut a);
    let key = a.create_string("MessageDigest.W22B-DIGEST");
    let value = a.create_string("com.example.W22bDigest");
    let res = provider_put_native(
        &mut a,
        &[
            Value::Object(Some(prov_a)),
            Value::Object(Some(key)),
            Value::Object(Some(value)),
        ],
    );
    assert!(
        matches!(res, Ok(Some(Value::Object(None)))),
        "put answered {res:?}"
    );
    assert_eq!(
        get_service_entry(VM_A, W22B_PROVIDER, "MessageDigest", "W22B-DIGEST")
            .map(|e| e.class_name),
        Some("com.example.W22bDigest".to_string())
    );
    assert!(get_service_entry(VM_B, W22B_PROVIDER, "MessageDigest", "W22B-DIGEST").is_none());

    // `getProperty` on a same-named provider in B does not see A's row.
    let prov_b = provider(&mut b);
    let key_b = b.create_string("MessageDigest.W22B-DIGEST");
    let got = provider_get_property(
        &mut b,
        &[Value::Object(Some(prov_b)), Value::Object(Some(key_b))],
    );
    assert!(
        matches!(got, Ok(Some(Value::Object(None)))),
        "B read A's property: {got:?}"
    );
    let prov_a = provider(&mut a);
    let key_a = a.create_string("MessageDigest.W22B-DIGEST");
    let got = provider_get_property(
        &mut a,
        &[Value::Object(Some(prov_a)), Value::Object(Some(key_a))],
    );
    match got {
        Ok(Some(Value::Object(Some(s)))) => {
            assert_eq!(a.read_string(s).as_deref(), Some("com.example.W22bDigest"))
        }
        other => panic!("A lost its own property: {other:?}"),
    }

    // `Security.removeProvider("SunEC")` in B leaves A's chain whole.
    let sun_ec = b.create_string("SunEC");
    let r = security_remove_provider(&mut b, &[Value::Object(Some(sun_ec))]);
    assert!(matches!(r, Ok(None)), "removeProvider answered {r:?}");
    assert!(find(VM_B, "SunEC").is_none());
    assert!(
        find(VM_A, "SunEC").is_some(),
        "B's removeProvider reached A"
    );
}

#[test]
fn a_torn_down_vms_additions_are_gone() {
    const VM_C: usize = 0x22B0_3C08;
    let _forget = Forget(&[VM_C]);
    add(VM_C, "W22bGone".to_string(), 3.0);
    remove(VM_C, "SunRsaSign");
    with_registry(VM_C, |reg| {
        reg.apply_legacy_put("W22bGone", "Signature.W22BGONE", "com.example.Gone")
    });
    assert!(find(VM_C, "W22bGone").is_some());

    forget_vm_provider_registry(VM_C);
    assert!(!registries().lock().contains_key(&VM_C));
    assert_eq!(find(VM_C, "W22bGone"), None);
    assert!(find(VM_C, "SunRsaSign").is_some());
    assert!(get_service_entry(VM_C, "W22bGone", "Signature", "W22BGONE").is_none());
    assert_eq!(names(VM_C), seed_names());
    // A read after teardown does not resurrect the row.
    assert!(!registries().lock().contains_key(&VM_C));
    forget_vm_provider_registry(VM_C);
}

/// A VM's first write copies the seed, and another VM's overwrite of a SEED
/// row (the process tables let `register`'s re-seed and every VM clobber
/// each other's) changes nothing for it.
#[test]
fn a_fresh_vm_starts_from_the_jdk_seed() {
    const VM_D: usize = 0x22B0_4D08;
    const VM_E: usize = 0x22B0_4E08;
    let _forget = Forget(&[VM_D, VM_E]);
    let seed = arm_service_seed(real_jca_bridges_wired());
    let seeded = |vm: usize| get_service_entry(vm, "SunRsaSign", "KeyFactory", "RSA");
    let seed_row = seed
        .service_entry("SunRsaSign", "KeyFactory", "RSA")
        .cloned()
        .expect("the direct-native seed registers SunRsaSign KeyFactory.RSA");

    with_registry(VM_E, |reg| {
        reg.put_service("SunRsaSign", "KeyFactory", "RSA", "com.example.NotRsa")
    });
    assert_eq!(
        seeded(VM_E).map(|e| e.class_name).as_deref(),
        Some("com.example.NotRsa")
    );

    // D's first write creates its row from the seed.
    add(VM_D, W22B_PROVIDER.to_string(), 1.0);
    assert_eq!(
        seeded(VM_D).map(|e| e.class_name),
        Some(seed_row.class_name)
    );
    let (services, aliases) =
        read_registry(VM_D, |reg| (reg.services.len(), reg.aliases.len()));
    assert_eq!(services, seed.services.len());
    assert_eq!(aliases, seed.aliases.len());
    let mut expected = seed_names();
    expected.push(W22B_PROVIDER.to_string());
    assert_eq!(names(VM_D), expected);
}

/// `Security.getProperty`'s `java.security` fallback reads the file of the
/// asking VM's own `java.home`, not whichever VM asked first.
#[test]
fn java_security_file_is_read_per_java_home() {
    struct RemoveDirs(Vec<std::path::PathBuf>);
    impl Drop for RemoveDirs {
        fn drop(&mut self) {
            for d in &self.0 {
                let _ = std::fs::remove_dir_all(d);
            }
        }
    }
    let make_home = |tag: &str, value: &str| {
        let home = std::env::temp_dir().join(format!(
            "cratonvm-w22b-java-home-{tag}-{}",
            std::process::id()
        ));
        let conf = home.join("conf").join("security");
        std::fs::create_dir_all(&conf).expect("temp java.home");
        std::fs::write(
            conf.join("java.security"),
            format!("# w22b\nw22b.test.key={value}\n"),
        )
        .expect("temp java.security");
        home
    };
    let home_a = make_home("a", "from-a");
    let home_b = make_home("b", "from-b");
    let _cleanup = RemoveDirs(vec![home_a.clone(), home_b.clone()]);

    let mut a = MockNativeContext::new();
    a.set_system_property("java.home", &home_a.to_string_lossy());
    let mut b = MockNativeContext::new();
    b.set_system_property("java.home", &home_b.to_string_lossy());
    let mut none = MockNativeContext::new();

    assert_eq!(
        java_security_file_property(&mut a, "w22b.test.key").as_deref(),
        Some("from-a")
    );
    assert_eq!(
        java_security_file_property(&mut b, "w22b.test.key").as_deref(),
        Some("from-b"),
        "B was answered from A's java.security"
    );
    assert_eq!(
        java_security_file_property(&mut none, "w22b.test.key"),
        None
    );
    assert_eq!(
        java_security_file_property(&mut a, "w22b.test.key").as_deref(),
        Some("from-a")
    );
}

use super::jdk_level_below;
use crate::types::Value;

fn body_of<'a>(src: &'a str, signature: &str) -> &'a str {
    let start = src.find(signature).unwrap_or_else(|| panic!("{signature}"));
    let end = start + src[start..].find("\n    }\n").expect("the body ends");
    &src[start..end]
}

// `VM.initLevel(int)` throws `InternalError("Bad level")` unless the level
// rises; `initPhase1`'s tail writes `VM.initLevel(1)` only from the
// static's 0. It used to ask the `VM.initLevel()` METHOD, which
// `--compatible` answers from a registered native (`max(live, 2)`), so the
// write never ran there.
#[test]
fn the_tail_writes_level_one_only_from_a_static_zero() {
    assert!(jdk_level_below(Some(Value::Int(0)), 1));
    for level in 1..=5 {
        assert!(!jdk_level_below(Some(Value::Int(level)), 1), "level {level}");
    }
    assert!(!jdk_level_below(Some(Value::Int(-1)), 1));
    assert!(!jdk_level_below(None, 1), "VM not loaded: nothing written");
    assert!(!jdk_level_below(Some(Value::Object(None)), 1));
}

#[test]
fn the_tail_reads_the_static_not_the_method() {
    let src = concat!(include_str!("../vm_init/mod.rs"), "\n", include_str!("../vm_init/missing_native_entry.rs"), "\n", include_str!("../vm_init/real_jdk_to_array_typed.rs"), "\n", include_str!("../vm_init/zgc_relocation_permitted.rs"), "\n", include_str!("../vm_init/wait_site.rs"), "\n", include_str!("../vm_init/class_store_cut_with.rs")).replace("\r\n", "\n");
    let body = body_of(&src, "    pub fn run_deferred_init_phase1_tail(&mut self) -> bool {");
    assert!(
        !body.contains(concat!("\"initLevel\", ", "\"()I\"")),
        "the tail asks VM.initLevel() again: a registered native answers it"
    );
    assert!(body.contains(concat!("self.raise_jdk", "_init_level(1)")));
}

#[test]
fn one_helper_owns_the_init_and_the_readiness_check() {
    let src = concat!(
        include_str!("../vm_init/mod.rs"),
        "\n",
        include_str!("../vm_init/missing_native_entry.rs"),
        "\n",
        include_str!("../vm_init/real_jdk_to_array_typed.rs"),
        "\n",
        include_str!("../vm_init/zgc_relocation_permitted.rs"),
        "\n",
        include_str!("../vm_init/wait_site.rs"),
        "\n",
        include_str!("../vm_init/class_store_cut_with.rs")
    )
    .replace("\r\n", "\n");
    let helper = body_of(
        &src,
        "fn boot_init_java_lang_invoke(&mut self) -> Result<(), MethodCallFailed> {",
    );
    let mhn = helper
        .find("\"java/lang/invoke/MethodHandleNatives\"")
        .expect("MethodHandleNatives.<clinit> runs");
    let check = helper
        .find("\"isJavaLangInvokeInited\"")
        .expect("the static is confirmed");
    assert!(mhn < check);
    // The one-shot setter is `MethodHandleNatives.<clinit>`'s: a direct
    // call poisons that initializer ("java.lang.invoke already inited").
    assert!(!helper.contains("\"setJavaLangInvokeInited\""));
    let booted = body_of(
        &src,
        "    pub fn boot_publish_jdk_booted(&mut self) -> Result<(), MethodCallFailed> {",
    );
    assert!(
        !booted.contains("\"refKindIsField\""),
        "a second copy of the init is back in boot_publish_jdk_booted"
    );
    assert_eq!(
        booted.matches(concat!("self.boot_init_java", "_lang_invoke()?;")).count(),
        1,
        "the real-JDK arm"
    );
}

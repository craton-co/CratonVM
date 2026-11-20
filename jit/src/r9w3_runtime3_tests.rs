    use super::*;
    use std::sync::Arc;

    fn push_compile_stack(keys: &[(&str, &str, &str)]) {
        JIT_COMPILE_STACK.with(|stack| {
            let mut stack = stack.borrow_mut();
            for (c, m, d) in keys {
                stack.push(JitCompileMethodKey::new(c, m, d));
            }
        });
    }

    // ── object-hashcode-on-a-string-receiver-pays-the-mic-native-helper ──

    fn receivers_at(pc: usize, counts: &[(u32, u32)]) -> profile::MethodProfile {
        let mut prof = profile::MethodProfile::default();
        let site = prof.receivers.entry(pc).or_default();
        for &(class_id, n) in counts {
            site.insert(class_id, n);
        }
        prof
    }

    const HASH_STRING_ID: u32 = 0x4_0001;

    fn hash_layout() -> StringFieldLayout {
        StringFieldLayout {
            string_class_id: HASH_STRING_ID,
            ..STRING_INTRINSIC_NAME_PROBE
        }
    }

    fn hash_guard(
        class: &str,
        name: &str,
        desc: &str,
        prof: Option<&profile::MethodProfile>,
        pc: usize,
    ) -> Option<u32> {
        object_hashcode_string_guard(class, name, desc, Some(hash_layout()), prof, pc)
    }

    /// Positive evidence only: a String-dominant profile admits the guarded
    /// intrinsic for an `Object`- or `CharSequence`-typed `hashCode()`; a
    /// mixed, thin or missing profile, any other member, and a `String`-typed
    /// site (the ordinary resolver's business) do not.
    #[test]
    fn object_hashcode_takes_the_string_intrinsic_only_on_a_string_dominant_profile() {
        let dominant = receivers_at(7, &[(HASH_STRING_ID, 950), (0x4_0002, 50)]);
        let mixed = receivers_at(7, &[(HASH_STRING_ID, 800), (0x4_0002, 200)]);
        let thin = receivers_at(7, &[(HASH_STRING_ID, MIN_GUARDED_RECEIVER_EVIDENCE - 1)]);
        let obj = "java/lang/Object";
        assert_eq!(
            hash_guard(obj, "hashCode", "()I", Some(&dominant), 7),
            Some(HASH_STRING_ID)
        );
        assert_eq!(
            hash_guard(
                "java/lang/CharSequence",
                "hashCode",
                "()I",
                Some(&dominant),
                7
            ),
            Some(HASH_STRING_ID)
        );
        assert_eq!(hash_guard(obj, "hashCode", "()I", Some(&mixed), 7), None);
        assert_eq!(hash_guard(obj, "hashCode", "()I", Some(&thin), 7), None);
        assert_eq!(hash_guard(obj, "hashCode", "()I", None, 7), None);
        assert_eq!(
            hash_guard(obj, "hashCode", "()I", Some(&dominant), 8),
            None,
            "the evidence is per bci"
        );
        assert_eq!(
            hash_guard(obj, "toString", "()Ljava/lang/String;", Some(&dominant), 7),
            None
        );
        assert_eq!(
            hash_guard("java/lang/String", "hashCode", "()I", Some(&dominant), 7),
            None
        );
        assert_eq!(
            object_hashcode_string_guard(obj, "hashCode", "()I", None, Some(&dominant), 7),
            None,
            "no resolved String layout, nothing to decode with"
        );
        let no_coder = StringFieldLayout {
            has_coder: false,
            ..hash_layout()
        };
        assert_eq!(
            object_hashcode_string_guard(
                obj,
                "hashCode",
                "()I",
                Some(no_coder),
                Some(&dominant),
                7
            ),
            None,
            "the inline decode needs the compact `coder` layout"
        );
    }

    // ── perf-mutual-recursion-forces-every-call-through-invoke-dispatch ──

    // A cycle closed by a STATICALLY bound edge is recorded, but no longer
    // forces every edge into its participants onto `jit_invoke_dispatch`: the
    // closing edge itself is refused by `note_recursive_compile_cycle`'s
    // `true`, and that is the one dispatch the cycle needs. The legacy rule
    // is one flag away.
// A cycle closed by an INLINE-CACHE edge stays sticky: a MIC/PIC slot is
    // filled at run time with the newest entry, so the "baked edges only
    // point at older artifacts" argument does not cover it.
// A target that is not open BELOW the current compile closes nothing and
    // records nothing; the propagation mark stays sticky.
// End to end through `try_compile`: A calls B, B calls A (both
    // `invokestatic`). B's call back to A closes the open compile and keeps
    // dispatch; A's call to B -- a cycle participant, but published by the
    // time A asks -- now binds B's entry DIRECTLY. Before wave 3 it fell back
    // to dispatch too (`recursive_compile_cycle_routes_parent_direct_call_through_dispatch`
    // in `tests.rs` pins that legacy rule under `CRATONVM_JIT_CYCLE_DIRECT_BIND=0`).

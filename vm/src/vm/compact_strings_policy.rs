//! `-XX:-CompactStrings` (j27 wave 9, lane W9VER; page
//! `docs/internal/jit/j27-compact-strings-off-not-honoured-FIXED-20261029.md`).
//!
//! Two different bits, kept apart:
//! * `ClassRealm::compact_strings`: the String LAYOUT (`byte[] value` +
//!   `coder`), true over every real JDK image.
//! * `ClassRealm::compact_strings_policy`: HotSpot's `CompactStrings`, the
//!   JDK's `String.COMPACT_STRINGS`. With it `false` every String is UTF16.
//!
//! Every producer that may write a LATIN1 String asks
//! [`ClassRealm::latin1_coder_allowed`]; compiled code reads the byte
//! [`ClassRealm::jit_compact_strings_flag`] as `COMPACT_STRINGS`.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::vm::realms::ClassRealm;

impl ClassRealm {
    /// May a String this VM builds be LATIN1? The JDK's `COMPACT_STRINGS`:
    /// the compact layout is in place AND `-XX:-CompactStrings` was not given.
    pub fn latin1_coder_allowed(&self) -> bool {
        self.compact_strings_policy && self.compact_strings.load(Ordering::Relaxed)
    }

    /// The byte compiled code reads as `COMPACT_STRINGS` (the inline
    /// `coder()` / `isLatin1()`, the concat and `char[]` constructors): the
    /// layout bit itself, or an always-false byte under `-XX:-CompactStrings`.
    /// The policy is fixed at construction, so the address is stable.
    pub fn jit_compact_strings_flag(&self) -> &AtomicBool {
        if self.compact_strings_policy {
            &self.compact_strings
        } else {
            &self.compact_strings_off_flag
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cratonvm_types::{ObjectRef, Value};

    use crate::config::VmConfig;
    use crate::vm::SharedVm;

    /// A VM with the compact layout armed (as `pre_init_string_statics`
    /// leaves it over a real JDK), with or without `-XX:-CompactStrings`.
    fn compact_vm(compact_strings_flag: bool) -> Arc<SharedVm> {
        let mut config = VmConfig::default();
        if !compact_strings_flag {
            config.jvm_args = vec!["-XX:-CompactStrings".to_string()];
        }
        let shared = Arc::new(SharedVm::new(config));
        shared.classes.compact_strings.store(true, Ordering::Relaxed);
        shared.classes.cached_string_num_fields.store(4, Ordering::Relaxed);
        shared
    }

    fn coder_and_value_len(shared: &SharedVm, s: ObjectRef) -> (Value, usize) {
        let value = match shared.mem.heap.get_field(s, 0) {
            Value::Object(Some(arr)) => arr,
            other => panic!("value is not an array: {other:?}"),
        };
        (shared.mem.heap.get_field(s, 1), shared.mem.heap.array_length(value))
    }

    use std::sync::atomic::Ordering;

    #[test]
    fn the_default_vm_builds_latin1_and_the_off_arm_builds_utf16() {
        let on = compact_vm(true);
        assert!(on.classes.latin1_coder_allowed());
        let s = crate::vm::create_java_string_uninterned(&on, "abc");
        assert_eq!(coder_and_value_len(&on, s), (Value::Int(0), 3));
        let e = crate::vm::create_java_string_uninterned(&on, "a\u{e9}");
        assert_eq!(coder_and_value_len(&on, e), (Value::Int(0), 2));

        let off = compact_vm(false);
        assert!(!off.classes.latin1_coder_allowed());
        for text in ["abc", "a\u{e9}", ""] {
            let s = crate::vm::create_java_string_uninterned(&off, text);
            let units = text.encode_utf16().count();
            assert_eq!(coder_and_value_len(&off, s), (Value::Int(1), 2 * units), "{text:?}");
            assert_eq!(crate::vm::read_java_string(&off.mem.heap, s).as_deref(), Some(text));
        }
        let pooled = crate::vm::create_java_string(&off, "lit");
        assert_eq!(coder_and_value_len(&off, pooled), (Value::Int(1), 6));
    }

    #[test]
    fn the_latin1_constructor_declines_under_the_off_arm() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};
        let off = compact_vm(false);
        let mut thread = JvmThread::new(ThreadId(0), "w9ver-compact-off");
        assert!(crate::vm::try_create_java_string_latin1_threaded(&off, &mut thread, b"abc").is_none());
        let s = crate::vm::try_create_java_string_uninterned_gc_safe_threaded(&off, &mut thread, "abc")
            .expect("room for three chars");
        assert_eq!(coder_and_value_len(&off, s), (Value::Int(1), 6));

        let on = compact_vm(true);
        let s = crate::vm::try_create_java_string_latin1_threaded(&on, &mut thread, b"abc")
            .expect("room for three chars");
        assert_eq!(coder_and_value_len(&on, s), (Value::Int(0), 3));
    }

    #[test]
    fn compiled_code_reads_compact_strings_false_under_the_off_arm() {
        let off = compact_vm(false);
        assert!(off.classes.compact_strings.load(Ordering::Relaxed), "the layout stays compact");
        assert!(!off.classes.jit_compact_strings_flag().load(Ordering::Relaxed));
        let on = compact_vm(true);
        assert!(on.classes.jit_compact_strings_flag().load(Ordering::Relaxed));
        assert!(std::ptr::eq(on.classes.jit_compact_strings_flag(), &on.classes.compact_strings));
    }
}

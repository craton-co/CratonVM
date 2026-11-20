    //! nio21 wave 10, lane L3 (W9L2-1, `CRATONVM_JIT_INDY_CONSTANT_LAMBDA`):
    //! a compiled non-capturing lambda site publishes the address of its
    //! singleton slot at `JIT_INDY_SITE_CONSTANT_WORD_OFFSET`, and that word
    //! reads exactly the instance the interpreter's table hands out.

    use super::*;
    use crate::threading::jvm_thread::ThreadId;

fn site_at(ptr: usize) -> &'static JitIndyGenericSite {
        // SAFETY: a leaked site from `new_jit_indy_generic_site` (process-lived).
        unsafe { &*(ptr as *const JitIndyGenericSite) }
    }

    /// The word as compiled code reads it: `[site + 8]`.
    fn raw_word(ptr: usize) -> usize {
        // SAFETY: `ptr` is a live site; the offset is pinned by the const
        // assertions beside `JIT_INDY_SITE_CONSTANT_WORD_OFFSET`.
        unsafe {
            std::ptr::read_volatile((ptr + JIT_INDY_SITE_CONSTANT_WORD_OFFSET) as *const usize)
        }
    }

    /// `[[site + 8]]`, the second load of the compiled probe.
    fn slot_value(word: usize) -> usize {
        // SAFETY: a published word names a slot the site's `OnceLock` keeps.
        unsafe { std::ptr::read_volatile(word as *const usize) }
    }

    fn zero_capture_site(class: u32, key: (u64, usize)) -> usize {
        new_jit_indy_generic_site(
            ClassId::new(class),
            4,
            "()Ljava/lang/Runnable;",
            Vec::new(),
            b'L',
            key,
            false,
            false,
            false,
        )    }
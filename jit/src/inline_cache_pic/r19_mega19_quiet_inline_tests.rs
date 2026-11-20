    //! JIT round 19, lane mega19, MEGA5-18-4
    //! (`CRATONVM_JIT_MEGA_GATE_FIRST_NO_INLINE_PUBLISH`): a slot whose site
    //! tests its gate before every inline read stops taking inline-way
    //! publications once it is megamorphic; the hashed ways still take them.
    use super::*;
    use std::sync::atomic::Ordering;

/// Raw, even entry words (bit 0 is the context tag): published, never called.
    fn word(class_id: u32) -> u64 {
        0x1000 + (u64::from(class_id) << 4)
    }

    fn inline_holds(pic: &JitPICSlot, class_id: u32) -> bool {
        (0..JIT_PIC_ENTRIES).any(|i| pic.way(i).is_some_and(|(c, e, _)| c == class_id && e != 0))
    }

    /// A fresh slot (every inline way free), megamorphic, optionally marked.
    fn megamorphic_slot(mark: bool) -> JitPICSlot {
        let pic = JitPICSlot::new();
        if mark {
            pic.note_gate_tested_first();
        }
        pic.megamorphic.store(1, Ordering::Release);
        pic    }
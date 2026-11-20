    use super::*;

#[test]
    fn only_slots_that_survive_a_call_are_admitted() {
        assert!(survives_a_call(StackSlot::Frame(8)));
        assert!(survives_a_call(StackSlot::CalleeSaved(12)));
        assert!(!survives_a_call(StackSlot::Scratch(8)));
    }

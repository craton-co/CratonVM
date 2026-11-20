use super::*;

// Interpreter round i3 wave 75, lane L3: `CRATONVM_REFLECT_INVOKE_SETTLED`
// and `CRATONVM_REFLECT_CTOR_PLAN_PER_VM` are read once per registry with
// the `truthy_word` rule, each independent of the other, and the setters
// win; `CRATONVM_REFLECT_NO_EXPORT_GATE` is read by presence.

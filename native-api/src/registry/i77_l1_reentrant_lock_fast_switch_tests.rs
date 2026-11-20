use super::*;

// Interpreter round i3 wave 77, lane L1:
// `CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST` is read once per registry
// with the `truthy_word` rule (default off), holds in either mode but
// only over a real JDK, and the setter wins over the env read; the
// decline-all diagnostic reads the same way.

use super::*;

// Interpreter round i3 wave 74, lane L5:
// `CRATONVM_INFLATER_RAW_ADLER_ZERO`, `CRATONVM_ARRAYS_MISMATCH_BULK` and
// `CRATONVM_ZIP_UTILS_INTRINSICS` are read once per registry with the
// `on_unless_zero` rule (on by default since the wave-74 triage), each
// independent of the others.

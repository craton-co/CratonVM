use super::*;


// Interpreter round i3 wave 77, lane L5:
// `CRATONVM_ZIP_ENTRY_HIT_INTRINSIC` and `CRATONVM_REFERENCE_LAYOUT_MEMO`
// are read once per registry with the `truthy_word` rule, each
// independent of the other and of the miss switch (every case sets all
// three words explicitly, so nothing here depends on a default); the
// setters override the env read; a fresh registry's layout slot is empty.

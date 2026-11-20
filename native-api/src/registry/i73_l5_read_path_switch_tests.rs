use super::*;

// Interpreter round i3 wave 73, lane L5:
// `CRATONVM_PLATFORM_RESOURCE_JRT_WALK` and
// `CRATONVM_INFLATER_REUSE_BUFFERS` are read once per registry with the
// `truthy_word` rule (off unless a truthy word), each independent of the
// other.

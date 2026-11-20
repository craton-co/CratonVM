use super::*;

// JIT round 21, lane ci: the three Latin-1 case statics (as statics
// only) are what the VM's resolver drops from a nested body to a bound
// call; any other site, and `=0`, keeps the nested body.

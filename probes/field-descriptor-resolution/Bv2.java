// Version 2: B declares its own `int x`, shadowing A's `Object x`. A legal
// separately compiled change. Compile into `v2c`; this is what Main RUNS
// against, and the constant-pool descriptor recorded against version 1 is what
// must decide which `x` the fieldref names.
public class B extends A { public int x = 42; }

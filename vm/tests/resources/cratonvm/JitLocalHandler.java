package cratonvm;

/**
 * Regression fixture for the RBC.6 JIT gate relaxation (jit/src/lib.rs).
 *
 * Before this fix, `try_compile_inner` permanently refused to JIT-compile any
 * method containing both `athrow` and a non-empty local exception table
 * (`scan.has_athrow && !cached.exception_table.is_empty()`), on the theory
 * that the JIT could not dispatch a thrown exception to an in-method
 * handler. That theory stopped being true the day after the gate landed:
 * `execute_jit_call` (vm/src/runtime/interpreter.rs) already routes any
 * pending exception a JIT-compiled method returns through THAT method's own
 * `cached.exception_table` via `route_jit_exception_through_method`, and
 * `emitted_athrow` already forces `has_dispatch = true` so the draining path
 * is always taken. This fixture warms each `*Step` method (which has its
 * OWN local try/catch) well past the JIT hotness threshold, directly
 * calling the method repeatedly so IT (not just its caller) gets
 * JIT-compiled and its own handler dispatch is exercised while compiled.
 *
 * Two custom exception types are used (instead of java.io.IOException /
 * java.lang.IllegalArgumentException, mirroring
 * org.apache.catalina.connector.Response.toAbsolute()'s real shape) so this
 * fixture has no dependency on a real JDK rt.jar / synthetic-hierarchy
 * coverage of those specific classes.
 */
public class JitLocalHandler {

    static class Inner extends RuntimeException {
        Inner(String m) { super(m); }
    }

    static class Outer extends RuntimeException {
        Outer(String m, Throwable cause) { super(m, cause); }
    }

    // Shape 1: single catch + unconditional rethrow of a DIFFERENT exception
    // type — org.apache.catalina.connector.Response.toAbsolute()'s exact
    // shape (try { ... } catch (IOException) { throw new
    // IllegalArgumentException(...) }).
    static int rethrowStep(int i, boolean fail) {
        try {
            if (fail) {
                throw new Inner("boom" + i);
            }
            return i;
        } catch (Inner inner) {
            throw new Outer("wrapped" + i, inner);
        }
    }

    public static int rethrowChecksum() {
        int checksum = 0;
        for (int i = 0; i < 20000; i++) {
            boolean fail = (i % 2 == 0);
            try {
                checksum += rethrowStep(i % 7, fail);
            } catch (Outer outer) {
                checksum += 1000;
                checksum += (outer.getCause() instanceof Inner) ? 1 : 0;
            }
        }
        return checksum;
    }

    // Shape 2: catch + return (swallow, recover a value) — no rethrow at
    // all, so `scan.has_athrow` is false for THIS method's own bytecode
    // (the `athrow` for RuntimeException's implicit throw lives in the
    // interpreter/native constructor path, not as a bytecode `athrow` in
    // catchReturnStep — the local handler here exercises the
    // ALREADY-compiling "declares a handler, no local athrow" shape that
    // BUG-H already covered; kept as a control case in this same fixture).
    static int catchReturnStep(int x) {
        try {
            if (x < 0) {
                throw new Inner("neg" + x);
            }
            return x * 2;
        } catch (Inner inner) {
            return -1;
        }
    }

    public static int catchReturnChecksum() {
        int checksum = 0;
        for (int i = 0; i < 20000; i++) {
            checksum += catchReturnStep(i - 10000);
        }
        return checksum;
    }

    // Shape 3: catch + fall-through — control continues past the try/catch
    // to more code in the same method, using a local assigned INSIDE the
    // catch.
    static int catchFallThroughStep(int x) {
        int result;
        try {
            if (x % 3 == 0) {
                throw new Inner("div3-" + x);
            }
            result = x + 100;
        } catch (Inner inner) {
            result = -100;
        }
        return result + 1;
    }

    public static int catchFallThroughChecksum() {
        int checksum = 0;
        for (int i = 0; i < 20000; i++) {
            checksum += catchFallThroughStep(i);
        }
        return checksum;
    }

    // Shape 4: multi-catch (two distinct catch clauses, first-match-wins).
    static class TypeA extends RuntimeException {
        TypeA(String m) { super(m); }
    }

    static class TypeB extends RuntimeException {
        TypeB(String m) { super(m); }
    }

    static int multiCatchStep(int x) {
        try {
            if (x == 1) {
                throw new TypeA("a" + x);
            } else if (x == 2) {
                throw new TypeB("b" + x);
            }
            return 0;
        } catch (TypeA a) {
            return 10;
        } catch (TypeB b) {
            return 20;
        }
    }

    public static int multiCatchChecksum() {
        int checksum = 0;
        for (int i = 0; i < 20000; i++) {
            checksum += multiCatchStep(i % 3);
        }
        return checksum;
    }

    // Shape 5: nested try/catch — inner handler catches its own exception;
    // outer handler catches a DIFFERENT type that only a path the inner
    // handler does NOT match can produce.
    static int nestedTryStep(int x) {
        try {
            try {
                if (x == 1) {
                    throw new TypeA("inner-a" + x);
                } else if (x == 2) {
                    throw new TypeB("inner-b" + x);
                }
                return 1;
            } catch (TypeA a) {
                // Caught locally by the INNER handler — never reaches outer.
                return 2;
            }
        } catch (TypeB b) {
            // Only reached for x == 2, since TypeA never escapes the inner try.
            return 3;
        }
    }

    public static int nestedTryChecksum() {
        int checksum = 0;
        for (int i = 0; i < 20000; i++) {
            checksum += nestedTryStep(i % 3);
        }
        return checksum;
    }

    // Shape 6: an exception thrown INSIDE the handler body itself must NOT
    // be caught by that same handler (classic dispatch off-by-one) — it
    // must propagate past this method entirely.
    static int throwsInHandlerStep(int x) {
        try {
            throw new TypeA("primary" + x);
        } catch (TypeA a) {
            if (x == 7) {
                // Same exception class as the catch clause — if the JIT's
                // routing incorrectly re-enters this handler for its own
                // exception, this returns -999 (or hangs) instead of
                // propagating.
                throw new TypeA("secondary" + x);
            }
            return 5;
        }
    }

    public static int throwsInHandlerChecksum() {
        int checksum = 0;
        int secondaryEscapes = 0;
        for (int i = 0; i < 20000; i++) {
            int x = i % 10;
            try {
                checksum += throwsInHandlerStep(x);
            } catch (TypeA a) {
                if (a.getMessage() != null && a.getMessage().startsWith("secondary")) {
                    secondaryEscapes++;
                }
            }
        }
        // Encode both values into one int so the Rust test can assert a
        // single exact number: checksum*10000 + secondaryEscapes.
        return checksum * 10000 + secondaryEscapes;
    }
}

// JAVA21+
package cratonvm;

import java.lang.ScopedValue;

/**
 * Session 51: JDK 25 — Scoped Values (JEP 487) conformance tests.
 *
 * Each test method returns an int:
 *   1 = success
 *   0 = failure
 *   other positive = expected value for multi-threaded tests
 */
public class ScopedValueComplete {

    // ========================================================================
    // BASIC ScopedValue OPERATIONS
    // ========================================================================

    // 1: ScopedValue.newInstance creates unbound value
    public static int testNewInstance() {
        ScopedValue<String> sv = ScopedValue.newInstance();
        if (sv != null && !sv.isBound()) return 1;
        return 0;
    }

    // 2: ScopedValue.where + run binds value
    public static int testWhereRun() {
        ScopedValue<String> sv = ScopedValue.newInstance();
        final int[] result = {0};
        ScopedValue.where(sv, "hello").run(() -> {
            if (sv.isBound() && "hello".equals(sv.get())) {
                result[0] = 1;
            }
        });
        return result[0];
    }

    // 3: ScopedValue is unbound after run completes
    public static int testUnboundAfterRun() {
        ScopedValue<String> sv = ScopedValue.newInstance();
        ScopedValue.where(sv, "temp").run(() -> {
            // bound inside
        });
        if (!sv.isBound()) return 1;
        return 0;
    }

    // 4: ScopedValue.where + call returns result
    public static int testWhereCall() {
        ScopedValue<Integer> sv = ScopedValue.newInstance();
        try {
            Integer result = ScopedValue.where(sv, 42).call(() -> {
                return sv.get();
            });
            if (result != null && result == 42) return 1;
            return 0;
        } catch (Exception e) {
            return 0;
        }
    }

    // 5: get() on unbound ScopedValue throws
    public static int testGetUnboundThrows() {
        ScopedValue<String> sv = ScopedValue.newInstance();
        try {
            sv.get();
            return 0; // should not reach here
        } catch (Exception e) {
            return 1;
        }
    }

    // 6: isBound returns false initially
    public static int testIsBoundInitiallyFalse() {
        ScopedValue<Object> sv = ScopedValue.newInstance();
        if (!sv.isBound()) return 1;
        return 0;
    }

    // 7: isBound returns true inside where/run
    public static int testIsBoundInside() {
        ScopedValue<Integer> sv = ScopedValue.newInstance();
        final int[] result = {0};
        ScopedValue.where(sv, 10).run(() -> {
            if (sv.isBound()) {
                result[0] = 1;
            }
        });
        return result[0];
    }

    // ========================================================================
    // orElse / orElseThrow
    // ========================================================================

    // 8: orElse returns bound value when bound
    public static int testOrElseBound() {
        ScopedValue<String> sv = ScopedValue.newInstance();
        final int[] result = {0};
        ScopedValue.where(sv, "yes").run(() -> {
            String val = sv.orElse("no");
            if ("yes".equals(val)) {
                result[0] = 1;
            }
        });
        return result[0];
    }

    // 9: orElse returns default when unbound
    public static int testOrElseUnbound() {
        ScopedValue<String> sv = ScopedValue.newInstance();
        String val = sv.orElse("default");
        if ("default".equals(val)) return 1;
        return 0;
    }

    // 10: orElse with null value
    public static int testOrElseNull() {
        ScopedValue<String> sv = ScopedValue.newInstance();
        final int[] result = {0};
        ScopedValue.where(sv, null).run(() -> {
            String val = sv.orElse("fallback");
            // When bound to null, orElse should return null (the bound value)
            if (val == null) {
                result[0] = 1;
            }
        });
        return result[0];
    }

    // 11: orElseThrow returns value when bound
    public static int testOrElseThrowBound() {
        ScopedValue<String> sv = ScopedValue.newInstance();
        final int[] result = {0};
        ScopedValue.where(sv, "value").run(() -> {
            try {
                String val = sv.orElseThrow(() -> new RuntimeException("fail"));
                if ("value".equals(val)) {
                    result[0] = 1;
                }
            } catch (Exception e) {
                result[0] = 0;
            }
        });
        return result[0];
    }

    // 12: orElseThrow throws when unbound
    public static int testOrElseThrowUnbound() {
        ScopedValue<String> sv = ScopedValue.newInstance();
        try {
            sv.orElseThrow(() -> new RuntimeException("expected"));
            return 0;
        } catch (RuntimeException e) {
            return 1;
        } catch (Exception e) {
            return 0;
        }
    }

    // ========================================================================
    // NESTED / REBINDING
    // ========================================================================

    // 13: Nested rebinding shadows outer value
    public static int testNestedRebinding() {
        ScopedValue<Integer> sv = ScopedValue.newInstance();
        final int[] result = {0};
        ScopedValue.where(sv, 1).run(() -> {
            int outer = (int) sv.get();
            ScopedValue.where(sv, 2).run(() -> {
                int inner = (int) sv.get();
                if (outer == 1 && inner == 2) {
                    result[0] = 1;
                }
            });
        });
        return result[0];
    }

    // 14: Outer value restored after nested run
    public static int testOuterRestoredAfterNested() {
        ScopedValue<String> sv = ScopedValue.newInstance();
        final int[] result = {0};
        ScopedValue.where(sv, "outer").run(() -> {
            ScopedValue.where(sv, "inner").run(() -> {
                // inner context
            });
            // outer should be restored
            if ("outer".equals(sv.get())) {
                result[0] = 1;
            }
        });
        return result[0];
    }

    // 15: Three levels of nesting
    public static int testTripleNesting() {
        ScopedValue<Integer> sv = ScopedValue.newInstance();
        final int[] result = {0};
        ScopedValue.where(sv, 1).run(() -> {
            ScopedValue.where(sv, 2).run(() -> {
                ScopedValue.where(sv, 3).run(() -> {
                    if ((int) sv.get() == 3) {
                        result[0] = 1;
                    }
                });
                // back to 2
                if ((int) sv.get() != 2) result[0] = 0;
            });
            // back to 1
            if ((int) sv.get() != 1) result[0] = 0;
        });
        return result[0];
    }

    // ========================================================================
    // MULTIPLE ScopedValues
    // ========================================================================

    // 16: Two independent ScopedValues
    public static int testTwoScopedValues() {
        ScopedValue<String> sv1 = ScopedValue.newInstance();
        ScopedValue<Integer> sv2 = ScopedValue.newInstance();
        final int[] result = {0};
        ScopedValue.where(sv1, "hello")
            .where(sv2, 42)
            .run(() -> {
                if ("hello".equals(sv1.get()) && (int) sv2.get() == 42) {
                    result[0] = 1;
                }
            });
        return result[0];
    }

    // 17: Chained where binds multiple values
    public static int testChainedWhere() {
        ScopedValue<String> a = ScopedValue.newInstance();
        ScopedValue<String> b = ScopedValue.newInstance();
        ScopedValue<String> c = ScopedValue.newInstance();
        final int[] result = {0};
        ScopedValue.where(a, "A")
            .where(b, "B")
            .where(c, "C")
            .run(() -> {
                if ("A".equals(a.get()) && "B".equals(b.get()) && "C".equals(c.get())) {
                    result[0] = 1;
                }
            });
        return result[0];
    }

    // 18: Multiple SVs, only one rebound in nested scope
    public static int testPartialRebind() {
        ScopedValue<String> sv1 = ScopedValue.newInstance();
        ScopedValue<String> sv2 = ScopedValue.newInstance();
        final int[] result = {0};
        ScopedValue.where(sv1, "v1").where(sv2, "v2").run(() -> {
            ScopedValue.where(sv1, "v1-new").run(() -> {
                // sv1 rebound, sv2 unchanged
                if ("v1-new".equals(sv1.get()) && "v2".equals(sv2.get())) {
                    result[0] = 1;
                }
            });
        });
        return result[0];
    }

    // ========================================================================
    // CALL WITH RETURN VALUES
    // ========================================================================

    // 19: call returns computed value
    public static int testCallReturn() {
        ScopedValue<Integer> sv = ScopedValue.newInstance();
        try {
            int result = ScopedValue.where(sv, 10).call(() -> (int) sv.get() * 3);
            if (result == 30) return 1;
            return 0;
        } catch (Exception e) {
            return 0;
        }
    }

    // 20: call with string concatenation
    public static int testCallStringConcat() {
        ScopedValue<String> sv = ScopedValue.newInstance();
        try {
            String result = ScopedValue.where(sv, "world").call(() -> "hello " + sv.get());
            if ("hello world".equals(result)) return 1;
            return 0;
        } catch (Exception e) {
            return 0;
        }
    }

    // ========================================================================
    // EXCEPTION HANDLING
    // ========================================================================

    // 21: Exception in run unbinds properly
    public static int testExceptionInRunUnbinds() {
        ScopedValue<String> sv = ScopedValue.newInstance();
        try {
            ScopedValue.where(sv, "temp").run(() -> {
                throw new RuntimeException("test");
            });
        } catch (RuntimeException e) {
            // ignored
        }
        // Should be unbound after exception
        if (!sv.isBound()) return 1;
        return 0;
    }

    // 22: Exception in call unbinds properly
    public static int testExceptionInCallUnbinds() {
        ScopedValue<String> sv = ScopedValue.newInstance();
        try {
            ScopedValue.where(sv, "temp").call(() -> {
                throw new RuntimeException("test");
            });
        } catch (Exception e) {
            // ignored
        }
        if (!sv.isBound()) return 1;
        return 0;
    }

    // 23: hashCode is stable
    public static int testHashCodeStable() {
        ScopedValue<String> sv = ScopedValue.newInstance();
        int h1 = sv.hashCode();
        int h2 = sv.hashCode();
        if (h1 == h2 && h1 != 0) return 1;
        return 0;
    }

    // 24: Two ScopedValues have different hashCodes
    public static int testHashCodeDifferent() {
        ScopedValue<String> sv1 = ScopedValue.newInstance();
        ScopedValue<String> sv2 = ScopedValue.newInstance();
        // Not guaranteed but very likely for different objects
        if (sv1.hashCode() != sv2.hashCode()) return 1;
        return 0;
    }

    // ========================================================================
    // BINDING WITH NULL
    // ========================================================================

    // 25: Can bind null value
    public static int testBindNull() {
        ScopedValue<String> sv = ScopedValue.newInstance();
        final int[] result = {0};
        ScopedValue.where(sv, null).run(() -> {
            if (sv.isBound() && sv.get() == null) {
                result[0] = 1;
            }
        });
        return result[0];
    }

    // ========================================================================
    // THREAD INHERITANCE
    // ========================================================================

    // 26: ScopedValue visible in child thread (via shared object state)
    // A ScopedValue binding is NOT inherited by an ordinary child thread.
    //
    // JEP 506: `where(k, v).run(op)` binds for the dynamic extent of `op` **on
    // the running thread**. Only a StructuredTaskScope fork inherits the
    // binding; a plain `new Thread(...)` started inside the extent sees the
    // ScopedValue as unbound. This test used to assert the opposite, with the
    // comment "in our VM model, ScopedValue binding is per-object-field, so
    // child threads see the same state" — i.e. it pinned a CratonVM
    // implementation detail as if it were the specification. Under a real JDK
    // 25 it returns 0, not 1.
    public static int testThreadVisibility() {
        ScopedValue<Integer> sv = ScopedValue.newInstance();
        final int[] childSaw = {-1};
        ScopedValue.where(sv, 100).run(() -> {
            Thread t = new Thread(() -> {
                childSaw[0] = sv.isBound() ? (int) sv.get() : 0;
            });
            t.start();
            try { t.join(); } catch (InterruptedException e) {}
            // The binding is still in effect on THIS thread.
            if (!sv.isBound() || sv.get() != 100) {
                childSaw[0] = -2;
            }
        });
        return childSaw[0] == 0 ? 1 : 0;
    }

    // 27: Rebinding in child thread doesn't affect parent
    public static int testChildRebindNoAffectParent() {
        ScopedValue<Integer> sv = ScopedValue.newInstance();
        final int[] parentVal = {0};
        ScopedValue.where(sv, 1).run(() -> {
            Thread t = new Thread(() -> {
                ScopedValue.where(sv, 999).run(() -> {
                    // child sees 999
                });
            });
            t.start();
            try { t.join(); } catch (InterruptedException e) {}
            // Parent should still see 1
            parentVal[0] = (int) sv.get();
        });
        if (parentVal[0] == 1) return 1;
        return 0;
    }

    // ========================================================================
    // CARRIER OPERATIONS
    // ========================================================================

    // 28: Carrier can retrieve bound value via call
    public static int testCarrierGet() {
        ScopedValue<String> sv = ScopedValue.newInstance();
        var carrier = ScopedValue.where(sv, "carried");
        try {
            String val = carrier.call(() -> sv.get());
            if ("carried".equals(val)) return 1;
            return 0;
        } catch (Exception e) {
            return 0;
        }
    }

    // 29: Multiple runs with same carrier
    public static int testMultipleRuns() {
        ScopedValue<Integer> sv = ScopedValue.newInstance();
        var carrier = ScopedValue.where(sv, 5);
        final int[] sum = {0};
        carrier.run(() -> sum[0] += (int) sv.get());
        carrier.run(() -> sum[0] += (int) sv.get());
        carrier.run(() -> sum[0] += (int) sv.get());
        if (sum[0] == 15) return 1;
        return 0;
    }

    // 30: Carrier reuse after exception
    public static int testCarrierReuseAfterException() {
        ScopedValue<String> sv = ScopedValue.newInstance();
        var carrier = ScopedValue.where(sv, "reuse");
        try {
            carrier.run(() -> { throw new RuntimeException("boom"); });
        } catch (RuntimeException e) { /* expected */ }
        // Should still work
        final int[] result = {0};
        carrier.run(() -> {
            if ("reuse".equals(sv.get())) {
                result[0] = 1;
            }
        });
        return result[0];
    }
}

package cratonvm;

// PGO-02 end-to-end fixture (docs/feature-designs/profile-guided-inlining.md):
// each entry point is called REPEATEDLY BY THE TEST HARNESS (not looped
// internally in Java) so the METHOD's own invocation count drives normal
// tiered compilation - the code path this lane actually touches. An
// internal-loop design would instead need OSR (on-stack-replacement) to
// compile mid-call, a different, untouched code path.
public class PgoGuardedVirtualInline {
    static class A {
        int tag(int x) {
            return x + 1;
        }
    }

    static class B extends A {
        @Override
        int tag(int x) {
            return x + 1000;
        }
    }

    static class C extends A {
        @Override
        int tag(int x) {
            return x + 2000;
        }
    }

    static class D extends A {
        @Override
        int tag(int x) {
            return x + 3000;
        }
    }

    static class Thrower extends A {
        @Override
        int tag(int x) {
            if (x == 7) {
                throw new IllegalStateException("boom-" + x);
            }
            return x + 1;
        }
    }

    private static final A ONLY_A = new A();
    private static final A ONLY_B = new B();
    private static final A ONLY_C = new C();
    private static final A ONLY_D = new D();
    private static final A THROWS_AT_7 = new Thrower();
    private static A current = ONLY_A;

    public static void setCurrent(int which) {
        current = switch (which) {
            case 1 -> ONLY_B;
            case 2 -> ONLY_C;
            case 3 -> ONLY_D;
            default -> ONLY_A;
        };
    }

    // Fixed monomorphic-A call site: builds an unambiguous A profile and
    // (with the flag on) compiles with an A-guarded inline. Positive control
    // for the guard-hit path.
    public static int callA(int x) {
        return ONLY_A.tag(x);
    }

    // Guard-hit / guard-miss call site: `current`'s concrete class decides
    // the receiver on every call. A caller builds a monomorphic-A profile
    // here (same as callA, but kept separate so callA's own profile/compile
    // is never disturbed), then switches `current` to a DIFFERENT concrete
    // class after the guard is baked in - the miss edge must still dispatch
    // correctly, not silently run A's inlined body against a non-A receiver.
    public static int callCurrent(int x) {
        return current.tag(x);
    }

    // Exception behavior through a compiled, guard-eligible call site: the
    // callee throws and this method catches internally (an *uncaught*
    // exception through an inlined frame needs deopt-metadata machinery this
    // increment does not touch - see the retired doc's known-gaps section),
    // returning a sentinel that only the exact interpreter-vs-compiled
    // control-flow path could produce correctly.
    public static int callThrowerCaught(int x) {
        try {
            return THROWS_AT_7.tag(x);
        } catch (IllegalStateException e) {
            return -1000 - x;
        }
    }

    // Polymorphic (4 distinct types, none dominant) call site: must never
    // crash or dispatch to the wrong body regardless of whether plan_inline
    // classifies it as Megamorphic or ReceiverNotDominant.
    public static int callPoly(int x, int which) {
        A recv = switch (which % 4) {
            case 1 -> ONLY_B;
            case 2 -> ONLY_C;
            case 3 -> ONLY_D;
            default -> ONLY_A;
        };
        return recv.tag(x);
    }
}

package cratonvm;

// PGO-02 end-to-end fixture (docs/feature-designs/profile-guided-inlining.md):
// each entry point is called REPEATEDLY BY THE TEST HARNESS (not looped
// internally in Java) so the METHOD's own invocation count drives normal
// tiered compilation - the code path this lane actually touches. An
// internal-loop design would instead need OSR (on-stack-replacement) to
// compile mid-call, a different, untouched code path.
public class PgoGuardedVirtualInline {
    static class A {
        // PUBLIC on purpose. The guarded inliner refuses a package-private
        // method selected from a class other than the constant-pool class,
        // because whether it OVERRIDES the resolved method depends on runtime
        // packages it does not resolve — see
        // `receiver_resolution_is_dispatch_faithful`. Every override below is
        // therefore public, which is also the ordinary shape of a virtual call
        // in real Java.
        public int tag(int x) {
            return x + 1;
        }
    }

    static class B extends A {
        @Override
        public int tag(int x) {
            return x + 1000;
        }
    }

    static class C extends A {
        @Override
        public int tag(int x) {
            return x + 2000;
        }
    }

    static class D extends A {
        @Override
        public int tag(int x) {
            return x + 3000;
        }
    }

    static class Thrower extends A {
        @Override
        public int tag(int x) {
            if (x == 7) {
                throw new IllegalStateException("boom-" + x);
            }
            return x + 1;
        }
    }

    // An overriding body that divides by an instance field. Every bytecode in
    // it is inlineable (iload/aload/getfield/idiv/ireturn — no `new`, no
    // `athrow`, no exception table), so the guarded inliner can splice it; set
    // `divisor` to 0 and the spliced body raises ArithmeticException from
    // INSIDE an inlined frame, with no handler anywhere in the caller.
    static class Divider extends A {
        int divisor = 1;

        @Override
        public int tag(int x) {
            return x / divisor;
        }
    }

    interface Tagger {
        int itag(int x);
    }

    static class OnlyImpl implements Tagger {
        @Override
        public int itag(int x) {
            return x + 77;
        }
    }

    private static final A ONLY_A = new A();
    private static final A ONLY_B = new B();
    private static final A ONLY_C = new C();
    private static final A ONLY_D = new D();
    private static final A THROWS_AT_7 = new Thrower();
    private static final Tagger ONLY_IMPL = new OnlyImpl();
    private static final Divider DIVIDER = new Divider();
    private static A current = ONLY_A;

    public static void setDivisor(int d) {
        DIVIDER.divisor = d;
    }

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

    // The constant-pool class and the speculated receiver class DISAGREE.
    // `ONLY_B`'s static type is `A`, so javac emits `invokevirtual A.tag`, but
    // every receiver this site ever sees is exactly `B`, which OVERRIDES
    // `tag`. A guard compares the receiver against B's class id; the body
    // spliced behind it must therefore be B's, not the one `A.tag` resolves
    // to. Getting this wrong returns x+1 instead of x+1000 with no crash and
    // no diagnostic.
    public static int callOverride(int x) {
        return ONLY_B.tag(x);
    }

    // Two receiver classes in an even mix, BOTH overriding `tag`. The top two
    // hold 100% of the observations, which clears the Bimorphic threshold, and
    // the two bodies are different methods — so each guard must carry its own.
    // A one-guard lowering still produces correct answers here (the second
    // class simply misses and dispatches), which is why the test checks the
    // spliced byte count and not only the results.
    public static int callBimorphic(int x, int which) {
        A recv = (which & 1) == 0 ? ONLY_B : ONLY_C;
        return recv.tag(x);
    }

    // An UNCAUGHT exception raised inside a guard-hit inlined body. No `try`
    // anywhere in this method, so the site is not in a protected range and the
    // guarded inliner may speculate on it; once `divisor` is 0 the spliced
    // `idiv` raises ArithmeticException with no handler in the inlined frame
    // and none in this one either.
    public static int callDivider(int x) {
        return DIVIDER.tag(x);
    }

    // The stack-trace check's own entry point onto the same receiver, and it
    // needs one because it cannot share `callDivider`.
    //
    // `check_uncaught_from_inlined_frame` runs first, on the same `Vm`, and
    // leaves `callDivider` compiled and spliced. The stack-trace check's first
    // measurement is documented as "the SAME call before the method compiled"
    // and was not: it was a second COMPILED reading, so the check compared the
    // compiled path against itself. Its own `interpreted == 0` floor is what
    // caught that -- intermittently, because whether a given call enters the
    // artifact is timing-dependent, so the same binary alternated between a
    // 2-frame reading and an empty one.
    //
    // A separate caller is a separate call SITE with its own profile and its
    // own artifact, so this one is genuinely cold when that check starts. The
    // receiver is deliberately the SAME `DIVIDER`: the callee is what the
    // check is about, and giving it a second one would change what is
    // measured.
    public static int callDividerForTrace(int x) {
        return DIVIDER.tag(x);
    }

    // How many frames naming `tag` appear in the stack trace of an exception
    // raised inside `callDivider`'s call to `Divider.tag`?
    //
    // The brief's verification list asks that a guard which always fires
    // produce "the same observable results as the un-inlined path — same
    // exceptions, same STACK TRACES, same `finally` execution". A spliced body
    // has no frame of its own, so this is the measurement that says whether
    // the trace still names the callee. The `try` is HERE, not in
    // `callDivider`, so `callDivider`'s site stays outside any protected range
    // and remains eligible for the guard.
    public static int dividerTagFrames(int x) {
        try {
            return callDivider(x);
        } catch (Throwable e) {
            int n = 0;
            for (StackTraceElement el : e.getStackTrace()) {
                if ("tag".equals(el.getMethodName())) {
                    n++;
                }
            }
            return -n;
        }
    }

    // A callee holding a `finally`, called from a guard-eligible site. The
    // inline resolver refuses any callee with a non-empty exception table, so
    // this must never be spliced — and the `finally` must run either way.
    static class FinallyTagger extends A {
        static int sideEffects = 0;

        @Override
        public int tag(int x) {
            try {
                if (x == 13) {
                    return -13;
                }
                return x + 1;
            } finally {
                sideEffects++;
            }
        }
    }

    private static final A FINALLY_TAGGER = new FinallyTagger();

    public static int callFinally(int x) {
        return FINALLY_TAGGER.tag(x);
    }

    public static int finallySideEffects() {
        return FinallyTagger.sideEffects;
    }

    // A `synchronized` callee at a guard-eligible site. Every `FrameState` the
    // lowerer builds hard-codes an empty monitor list, so an inlined body can
    // carry no monitor state — the brief's second blocker. This must never be
    // spliced.
    static class SyncTagger extends A {
        @Override
        public synchronized int tag(int x) {
            return x + 42;
        }
    }

    private static final A SYNC_TAGGER = new SyncTagger();

    public static int callSynchronized(int x) {
        return SYNC_TAGGER.tag(x);
    }

    // A callee whose body takes a monitor (`monitorenter`/`monitorexit`) at a
    // guard-eligible site. Same refusal, different bytecode shape.
    static class MonitorTagger extends A {
        private final Object lock = new Object();

        @Override
        public int tag(int x) {
            synchronized (lock) {
                return x + 99;
            }
        }
    }

    private static final A MONITOR_TAGGER = new MonitorTagger();

    public static int callMonitor(int x) {
        return MONITOR_TAGGER.tag(x);
    }

    // Interface reach: `invokeinterface Tagger.itag`, one implementation.
    // Resolving from the constant-pool class finds only the ABSTRACT method
    // (no Code attribute), so this site can never inline unless resolution
    // starts from the speculated receiver class.
    public static int callIface(int x) {
        return ONLY_IMPL.itag(x);
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

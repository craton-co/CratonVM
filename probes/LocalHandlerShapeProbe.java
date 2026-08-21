/**
 * Does a compiled frame that runs its OWN `catch` block still get the same
 * ANSWER as one that deopts out to run it interpreted?
 *
 * `CRATONVM_JIT_LOCAL_HANDLERS` lets a throwing site inside one of the
 * compiled method's own protected ranges jump straight into the compiled
 * handler block instead of leaving the artifact (reason-9 deopt, exceptional
 * frame reconstruction, an interpreted handler, and — inside an OSR'd loop — a
 * re-entry at the next back edge). That is a THROUGHPUT change and must not be
 * a behaviour change, so this probe is about the printed digest and not about
 * the clock: run it with the flag off and on and diff.
 *
 *   javac -nowarn -d . probes/LocalHandlerShapeProbe.java
 *   java                                          -cp . LocalHandlerShapeProbe 200000
 *   cratonvm --java-home <jdk>                    -cp . LocalHandlerShapeProbe 200000
 *   CRATONVM_JIT_LOCAL_HANDLERS=1 cratonvm ...    -cp . LocalHandlerShapeProbe 200000
 *   cratonvm --nojit --java-home <jdk>            -cp . LocalHandlerShapeProbe 200000
 *
 * The shapes are chosen to be the ones the feature has to get right rather
 * than the ones it is FOR:
 *
 *   caughtHere        the plain case — a callee throws, this frame catches.
 *   caughtByType      TWO typed handlers over one range, so table ORDER and
 *                     catch-type matching both matter. A dispatch that
 *                     answered "first entry that covers the bci" would pass
 *                     `caughtHere` and fail this.
 *   caughtBySuper     the thrown class is a strict SUBCLASS of the catch type,
 *                     so an id comparison is not enough — assignability is.
 *   notCaughtHere     a throw this frame does NOT catch, which must propagate
 *                     to the caller exactly as before.
 *   finallyRuns       a catch-all (`catch_type == 0`) plus a `finally` that
 *                     must run on both the normal and the exceptional path.
 *   nestedTry         an inner `try` inside the outer one: two ranges cover
 *                     the same bci and the inner must win.
 *   handlerRethrows   the handler itself throws, from inside compiled code.
 *   localsSurvive     locals written before the throw must read back in the
 *                     handler — the whole point of not leaving the frame.
 *
 * Each arm is a method reached MANY times so the method-entry compiler takes
 * it, and each also carries a hot loop so the OSR door takes it too; the
 * feature is armed at both.
 */
public final class LocalHandlerShapeProbe {

    static final class Alpha extends RuntimeException {
        Alpha() { super(null, null, false, false); }
    }
    static final class Beta extends RuntimeException {
        Beta() { super(null, null, false, false); }
    }
    /** A strict subclass, for the assignability arm. */
    static final class AlphaChild extends RuntimeException {
        AlphaChild() { super(null, null, false, false); }
    }

    static final Alpha ALPHA = new Alpha();
    static final Beta BETA = new Beta();
    static final AlphaChild ALPHA_CHILD = new AlphaChild();

    static long digest;

    static void mix(long v) { digest = digest * 1000003L + v; }

    // ---- throwers, kept out of line so the throw is a post-invoke edge ----

    static int throwAlpha(int i) { throw ALPHA; }
    static int throwBeta(int i) { throw BETA; }
    static int throwAlphaChild(int i) { throw ALPHA_CHILD; }
    static int throwIllegal(int i) { throw new IllegalStateException("no"); }
    static int identity(int i) { return i; }

    static int maybeThrowAlpha(int i) {
        if ((i & 7) == 0) { throw ALPHA; }
        return i;
    }

    // ---- arms ----

    static long caughtHere(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            try {
                acc += maybeThrowAlpha(i);
            } catch (Alpha e) {
                acc += 1;
            }
        }
        return acc;
    }

    static long caughtByType(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            try {
                if ((i & 3) == 0) {
                    acc += throwAlpha(i);
                } else if ((i & 3) == 1) {
                    acc += throwBeta(i);
                } else {
                    acc += identity(i);
                }
            } catch (Alpha e) {
                acc += 2;
            } catch (Beta e) {
                acc += 3;
            }
        }
        return acc;
    }

    static long caughtBySuper(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            try {
                if ((i & 3) == 0) {
                    acc += throwAlphaChild(i);
                } else {
                    acc += identity(i);
                }
            } catch (RuntimeException e) {
                acc += e instanceof AlphaChild ? 5 : 7;
            }
        }
        return acc;
    }

    static long notCaughtHere(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            try {
                acc += inner(i);
            } catch (IllegalStateException e) {
                // Only the OUTER frame catches this one; `inner` must let it
                // through even though `inner` has a handler of its own.
                acc += 11;
            }
        }
        return acc;
    }

    /** Catches `Alpha` only, so the `IllegalStateException` propagates. */
    static int inner(int i) {
        try {
            if ((i & 15) == 0) {
                return throwIllegal(i);
            }
            return identity(i);
        } catch (Alpha e) {
            return 13;
        }
    }

    static long finallyRuns(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            try {
                acc += maybeThrowAlpha(i);
            } catch (Alpha e) {
                acc += 17;
            } finally {
                acc += 1;
            }
        }
        return acc;
    }

    static long nestedTry(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            try {
                try {
                    if ((i & 3) == 0) {
                        acc += throwAlpha(i);
                    } else if ((i & 3) == 1) {
                        acc += throwBeta(i);
                    } else {
                        acc += identity(i);
                    }
                } catch (Alpha e) {
                    acc += 19;
                }
            } catch (Beta e) {
                acc += 23;
            }
        }
        return acc;
    }

    static long handlerRethrows(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            try {
                rethrower(i);
                acc += 1;
            } catch (Beta e) {
                acc += 29;
            }
        }
        return acc;
    }

    static void rethrower(int i) {
        try {
            if ((i & 3) == 0) {
                throwAlpha(i);
            }
        } catch (Alpha e) {
            throw BETA;
        }
    }

    static long localsSurvive(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            int a = i * 2;
            int b = i * 3;
            long c = ((long) i) * 5L;
            String s = (i & 1) == 0 ? "even" : "odd";
            try {
                a += maybeThrowAlpha(i);
            } catch (Alpha e) {
                // Every local written before the throw must read back here.
                // This is exactly what NOT leaving the frame buys, and exactly
                // what a wrong frame model would corrupt.
                acc += a + b + c + s.length();
            }
            acc += a + b;
        }
        return acc;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        // Several rounds so every arm is method-entry compiled AND its loop is
        // OSR-compiled, and so a handler that only works on the first pass
        // shows up as a digest difference between rounds.
        for (int round = 0; round < 3; round++) {
            mix(caughtHere(n));
            mix(caughtByType(n));
            mix(caughtBySuper(n));
            mix(notCaughtHere(n));
            mix(finallyRuns(n));
            mix(nestedTry(n));
            mix(handlerRethrows(n));
            mix(localsSurvive(n));
        }
        System.out.println("digest=" + digest);
    }
}

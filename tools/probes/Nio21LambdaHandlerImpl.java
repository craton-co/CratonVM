// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.concurrent.locks.ReentrantLock;

/**
 * Round nio21 wave 16, lane L2: lambda impls that DECLARE AN EXCEPTION TABLE, entered compiled
 * through the interpreted lambda door ({@code CRATONVM_LAMBDA_DOOR_COMPILED}), the one-shot arm
 * ({@code CRATONVM_LAMBDA_HANDLER_IMPL_ONESHOT}) and the general path's site arm
 * ({@code CRATONVM_LAMBDA_SITE_ONESHOT}).
 *
 * <p>Conformance (default): every impl is driven past the tier-up threshold on its quiet path
 * and then, inside the same loop, takes its own handler (an explicit throw, an implicit
 * {@code ArithmeticException}, an implicit NPE), lets an exception escape to the caller, and
 * runs its {@code finally}. Each shape runs from an interpreted-or-OSR driver ({@code drive}) and
 * from a compiled caller ({@code driveHot}, called often enough to compile). The output is sums
 * and counters only, identical on HotSpot 25 and on every setting of the three switches.
 *
 * <p>Bench ({@code -Dnio21.rows=bench}): {@code lockref} (a static {@code try/finally} method
 * through a method reference, the {@code Nio20JitRows rlock} shape) and {@code lockcap} (a
 * capturing {@code try/finally} lambda, the {@code Nio21UnsafeVolatile lock} shape), printed as
 * {@code <row> <N> ns/op}.
 *
 * <p>nio21 wave 21 L1 added {@code calleethrow} and {@code escapeimplicit}, and runs the whole
 * battery also under {@code CRATONVM_JIT_LAMBDA_HANDLER_IMPL=1} (handler-bearing impls admitted to
 * the compiled lambda site: the Rust direct arm and the machine-code thunk).
 *
 * <pre>
 *   javac -d /tmp/p21 tools/probes/Nio21LambdaHandlerImpl.java
 *   CRATONVM_JIT_LAMBDA_HANDLER_IMPL=1 cratonvm --jdk-only -cp /tmp/p21 Nio21LambdaHandlerImpl
 *   cratonvm --jdk-only   -cp /tmp/p21 Nio21LambdaHandlerImpl
 *   cratonvm --compatible -cp /tmp/p21 Nio21LambdaHandlerImpl
 *   CRATONVM_LAMBDA_DOOR_COMPILED=0 CRATONVM_LAMBDA_HANDLER_IMPL_ONESHOT=0 \
 *     CRATONVM_LAMBDA_SITE_ONESHOT=0 cratonvm --jdk-only -cp /tmp/p21 Nio21LambdaHandlerImpl
 *   cratonvm --jdk-only -Dnio21.rows=bench -cp /tmp/p21 Nio21LambdaHandlerImpl
 * </pre>
 */
public class Nio21LambdaHandlerImpl {
    interface Op {
        int run(int i);
    }

    static final String ROWS = System.getProperty("nio21.rows", "conf");
    static final int N = Integer.getInteger("nio21.n", 200_000);

    static final ReentrantLock LOCK = new ReentrantLock();
    static int counter;
    static int finallies;
    static int caught;

    /** The {@code rlockOp} shape: a static method with a try/finally, as a method reference. */
    static int lockedStep(int i) {
        LOCK.lock();
        try {
            counter += i & 1;
            return counter & 7;
        } finally {
            LOCK.unlock();
        }
    }

    /** Catches its own explicit throw every 1000th call. */
    static int catchesOwnThrow(int i) {
        try {
            if (i % 1000 == 999) {
                throw new IllegalStateException("x" + i);
            }
            return i & 3;
        } catch (IllegalStateException e) {
            caught++;
            return e.getMessage().length();
        } finally {
            finallies++;
        }
    }

    /** Catches an implicit ArithmeticException from its own division. */
    static int catchesOwnDivide(int i) {
        try {
            return 100 / (i % 501);
        } catch (ArithmeticException e) {
            caught++;
            return -7;
        }
    }

    static final int[] SOMETIMES_NULL_HOLDER = new int[1];

    /** Catches an implicit NPE from its own array access. */
    static int catchesOwnNpe(int i) {
        int[] a = (i % 777 == 776) ? null : SOMETIMES_NULL_HOLDER;
        try {
            return a.length + i;
        } catch (NullPointerException e) {
            caught++;
            return -9;
        }
    }

    /** Its handler does not match: the exception escapes to the caller after the finally. */
    static int escapes(int i) {
        try {
            if (i % 1234 == 1233) {
                throw new UnsupportedOperationException("u");
            }
            return 1;
        } catch (IllegalArgumentException e) {
            return -100;
        } finally {
            finallies++;
        }
    }

    static int calleeThrows(int i) {
        if (i % 500 == 499) {
            throw new IllegalStateException("c");
        }
        return i & 1;
    }

    /**
     * nio21 wave 21 L1: an explicit throw from a CALLEE, caught by the impl's own handler, with
     * a finally.
     */
    static int catchesCalleeThrow(int i) {
        try {
            return calleeThrows(i) + 1;
        } catch (IllegalStateException e) {
            caught++;
            return -3;
        } finally {
            finallies++;
        }
    }

    /**
     * nio21 wave 21 L1: an implicit ArithmeticException its handler does not take escapes to
     * the caller after the finally ran.
     */
    static int escapesImplicit(int i) {
        try {
            return 10 / (i % 911);
        } catch (IllegalArgumentException e) {
            return -100;
        } finally {
            finallies++;
        }
    }

    static long drive(Op op, int n) {
        long h = 0;
        int escaped = 0;
        for (int i = 0; i < n; i++) {
            try {
                h += op.run(i);
            } catch (UnsupportedOperationException | ArithmeticException e) {
                escaped++;
            }
        }
        return h * 31 + escaped;
    }

    static long driveHot(Op op, int from, int to) {
        long h = 0;
        int escaped = 0;
        for (int i = from; i < to; i++) {
            try {
                h += op.run(i);
            } catch (UnsupportedOperationException | ArithmeticException e) {
                escaped++;
            }
        }
        return h * 31 + escaped;
    }

    /** Many short calls, so the compiled caller ({@code driveHot}) is what runs most of them. */
    static long chunked(Op op, int n) {
        long h = 0;
        for (int c = 0; c < n; c += 100) {
            h = h * 7 + driveHot(op, c, Math.min(n, c + 100));
        }
        return h;
    }

    static void conf() {
        int n = 20_000;
        ReentrantLock local = new ReentrantLock();
        int[] box = new int[1];
        Op[] ops = {
            Nio21LambdaHandlerImpl::lockedStep,
            Nio21LambdaHandlerImpl::catchesOwnThrow,
            Nio21LambdaHandlerImpl::catchesOwnDivide,
            Nio21LambdaHandlerImpl::catchesOwnNpe,
            Nio21LambdaHandlerImpl::escapes,
            Nio21LambdaHandlerImpl::catchesCalleeThrow,
            Nio21LambdaHandlerImpl::escapesImplicit,
            // Capturing, with its own try/finally (the `lock` row shape).
            k -> {
                local.lock();
                try {
                    box[0] += k & 3;
                    return box[0] & 15;
                } finally {
                    local.unlock();
                }
            },
            // Capturing, catching its own implicit exception.
            k -> {
                try {
                    return box.length / (k % 333);
                } catch (ArithmeticException e) {
                    return 5;
                } finally {
                    box[0]++;
                }
            },
        };
        String[] names = {"lockref", "ownthrow", "owndiv", "ownnpe", "escape", "calleethrow", "escapeimplicit",
            "lockcap", "capdiv"};
        for (int k = 0; k < ops.length; k++) {
            counter = 0;
            finallies = 0;
            caught = 0;
            box[0] = 0;
            long a = drive(ops[k], n);
            long b = chunked(ops[k], n);
            System.out.println("conf " + names[k] + " drive=" + a + " chunked=" + b + " counter=" + counter
                    + " finallies=" + finallies + " caught=" + caught + " box=" + box[0]
                    + " held=" + LOCK.isHeldByCurrentThread() + "/" + local.isHeldByCurrentThread());
        }
    }

    static void row(String name, Op op) {
        long h = drive(op, N);
        h ^= drive(op, N);
        long t0 = System.nanoTime();
        h ^= drive(op, N);
        long ns = (System.nanoTime() - t0) / N;
        if (h == 42) {
            System.err.print("");
        }
        System.out.println(name + " " + ns + " ns/op");
    }

    public static void main(String[] args) {
        if (ROWS.equals("bench")) {
            ReentrantLock local = new ReentrantLock();
            row("lockref", Nio21LambdaHandlerImpl::lockedStep);
            row("lockcap", k -> {
                local.lock();
                try {
                    return k & 7;
                } finally {
                    local.unlock();
                }
            });
            return;
        }
        conf();
    }
}

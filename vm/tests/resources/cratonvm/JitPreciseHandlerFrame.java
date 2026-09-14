package cratonvm;

/**
 * Regression fixture for the JIT's exception-routing back into a compiled
 * method's OWN handler table — the RBC.6 family.
 *
 * Every entry point returns a MISMATCH COUNT (0 == pass) rather than a
 * checksum, so a duplicated loop iteration (an OSR bail re-running part of the
 * loop) re-checks instead of corrupting the expected value; only a genuine
 * routing defect can make these non-zero.
 *
 * Four shapes, each of which was silently wrong before the 2026-07-28 fixes
 * (the first three) or the 2026-08-01 one (`loopStep`):
 *
 * <ul>
 *   <li>{@code plainStep} — the handler reads only parameters, so this shape
 *       needs none of the precise-handler machinery and has always been
 *       compilable. Its callee {@code maybeThrow} athrows at ITS bci 13, and
 *       that bci was handed to {@code plainStep}'s drain as if it were its own;
 *       {@code plainStep}'s protected range is [0,4), so no handler matched and
 *       the exception escaped its own {@code catch}.</li>
 *   <li>{@code buildStep} — the protected range ends immediately after its last
 *       invoke, which is what javac emits for {@code try { f(x); } catch}. The
 *       precise exceptional frame used to be keyed on the invoke's SUCCESSOR,
 *       which is `end_pc` — outside the very handler that had to run.
 *       (`net.minidev.json.JSONValue.toJSONString` is the real-world instance:
 *       range [8,14), invoke at pc 11.) `sb` is also a non-parameter local read
 *       inside the handler, so this is the population the precise-handler-frame
 *       relaxation admits.</li>
 *   <li>{@code scopeStep} — `keep` is read ONLY on the path through the
 *       handler, so the handler-blind liveness behind register allocation
 *       considered it dead across the whole try and let it share a register
 *       with `other`, which IS live there.</li>
 *   <li>{@code loopStep} — the non-parameter local at risk is the LOOP's
 *       iterator, which the handler itself never touches: it is read by the
 *       loop head the handler falls through to. `run_jit_callee_handler` (the
 *       sink that resumes a compiled CALLEE at its own handler) rebuilt the
 *       frame from the incoming arguments alone and ignored the precise
 *       reason-9 frame, so the iterator came back null and the next
 *       `hasNext()` NPE'd. Spring Boot's
 *       `BindConverter.convert(Object, TypeDescriptor, TypeDescriptor)` is the
 *       real-world instance (27 of 43 `LiquibaseAutoConfigurationTests`
 *       methods). `CRATONVM_NO_JIT_CALLEE_HANDLER_PRECISE_FRAME=1` restores the
 *       defect on the same binary.</li>
 * </ul>
 */
public class JitPreciseHandlerFrame {

    static class Boom extends RuntimeException {
        Boom(String m) {
            super(m);
        }
    }

    static final class Cell {
        final int v;

        Cell(int v) {
            this.v = v;
        }
    }

    static void maybeThrow(boolean fail) {
        if (fail) {
            throw new Boom("boom");
        }
    }

    // Handler reads only parameters — compiles regardless of the
    // precise-handler-frame gate.
    static int plainStep(int i, boolean fail) {
        try {
            maybeThrow(fail);
        } catch (Boom b) {
            return i * 2;
        }
        return i;
    }

    public static int plainMismatches() {
        int bad = 0;
        for (int i = 0; i < 20000; i++) {
            int v = i % 97;
            boolean fail = (i % 3) == 0;
            int got;
            try {
                got = plainStep(v, fail);
            } catch (Boom escaped) {
                // plainStep's own catch did not run.
                bad++;
                continue;
            }
            if (got != (fail ? v * 2 : v)) {
                bad++;
            }
        }
        return bad;
    }

    static void append(StringBuilder sb, int i, boolean fail) {
        sb.append((char) ('0' + (i % 10)));
        if (fail) {
            throw new Boom("boom");
        }
        sb.append('x');
    }

    // `sb` is local 2 — assigned before the try, read INSIDE the handler and
    // again after it. The invoke is the last instruction of the protected range.
    static String buildStep(int i, boolean fail) {
        StringBuilder sb = new StringBuilder();
        try {
            append(sb, i, fail);
        } catch (Boom b) {
            sb.append('!');
        }
        return sb.toString();
    }

    public static int buildMismatches() {
        int bad = 0;
        for (int i = 0; i < 20000; i++) {
            boolean fail = (i % 3) == 0;
            String got;
            try {
                got = buildStep(i, fail);
            } catch (Boom escaped) {
                bad++;
                continue;
            }
            String want = "" + (char) ('0' + (i % 10)) + (fail ? '!' : 'x');
            if (!want.equals(got)) {
                bad++;
            }
        }
        return bad;
    }

    // `keep` is read ONLY on the path through the handler; `other` is read on
    // both paths. Handler-blind liveness makes them non-interfering.
    static int scopeStep(int i, boolean fail) {
        Cell keep = new Cell(i);
        Cell other = new Cell(i + 1);
        try {
            maybeThrow(fail);
        } catch (Boom b) {
            return keep.v * 3 + other.v;
        }
        return other.v;
    }

    public static int scopeMismatches() {
        int bad = 0;
        for (int i = 0; i < 20000; i++) {
            int v = i % 97;
            boolean fail = (i % 5) == 0;
            int got;
            try {
                got = scopeStep(v, fail);
            } catch (Boom escaped) {
                bad++;
                continue;
            }
            if (got != (fail ? v * 3 + (v + 1) : v + 1)) {
                bad++;
            }
        }
        return bad;
    }

    // ---- loopStep: the iterator is the local at risk ----------------------

    interface Step {
        int apply(int i);
    }

    static final class Adder implements Step {
        private final int add;

        Adder(int add) {
            this.add = add;
        }

        @Override
        public int apply(int i) {
            return i + this.add;
        }
    }

    static final class Thrower implements Step {
        @Override
        public int apply(int i) {
            throw new Boom("step");
        }
    }

    // A plain `ArrayList`, populated by `add`. `Arrays.asList(...)` iterated
    // zero elements under the synthetic JDK the in-process test VM boots with,
    // which made every iteration mismatch for a reason that had nothing to do
    // with the defect under test.
    static final java.util.List<Step> STEPS = new java.util.ArrayList<Step>();

    static {
        STEPS.add(new Adder(1));
        STEPS.add(new Thrower());
        STEPS.add(new Adder(2));
        STEPS.add(new Thrower());
        STEPS.add(new Adder(4));
    }

    /**
     * The `BindConverter.convert` shape. `sum` and the loop's `Iterator` are
     * both non-parameter locals assigned BEFORE the protected range; the
     * handler reads `sum` (so this method needs precise frames at all), and the
     * ITERATOR is read only by the loop head the handler falls through to.
     * Resuming on `this`-plus-parameters zeroes both.
     */
    static int loopStep(int i, boolean doubleIt) {
        int sum = 0;
        for (Step step : STEPS) {
            try {
                // ONLY an `invokeinterface` inside the protected range. An
                // `instanceof` here (the first draft had one) is opcode 0xc1,
                // which `precise_frame_publishing_opcode` does not admit, so
                // the whole method was refused compilation and the test was a
                // false pass in both A/B arms.
                sum += step.apply(i);
            } catch (Boom b) {
                sum -= 1;
            }
        }
        return doubleIt ? sum * 2 : sum;
    }

    /**
     * A HOT caller, and the reason this shape reaches the sink under test at
     * all. `run_jit_callee_handler` is only entered from
     * `route_implicit_exc_through_callee`, i.e. when a COMPILED caller
     * dispatched the throwing callee. `loopMismatches` itself runs its loop
     * once and is OSR-denied, so calling `loopStep` directly from it leaves the
     * exception on the ordinary interpreter drain and the test is a false pass
     * (measured: 0 mismatches in both A/B arms). This wrapper is invoked 20,000
     * times, compiles on the invocation counter, and puts a compiled frame
     * between the two.
     */
    static int loopCall(int i, boolean doubleIt) {
        int r = loopStep(i, doubleIt);
        if (r == Integer.MIN_VALUE) {
            // Never taken; keeps this from being a trivial forwarder the
            // compiler can fold the callee into.
            throw new Boom("unreachable");
        }
        return r;
    }

    // ---- instanceofStep: the protected range holds an `instanceof` ---------

    /**
     * The `instanceof` shape — and the reason `loopStep` above had to give one
     * up.
     *
     * `instanceof` is opcode 0xc1. It used to sit inside
     * `may_throw_without_precise_frame`'s `0xbb..=0xc1` range, so a protected
     * range containing one refused the WHOLE method — even though the x64
     * lowering of `instanceof` cannot throw: it emits one call to
     * `jit_instanceof`, which returns 0 or 1 on every path and never stashes a
     * pending exception. That is why the note on `loopStep` records its first
     * draft as a false pass in BOTH arms of its own A/B: the method under test
     * never compiled, so neither arm exercised anything.
     *
     * `tag` is a non-parameter local assigned before the try and read inside
     * the handler, so this method needs the precise-handler-frame machinery —
     * it exercises the gate rather than side-stepping it. The range holds both
     * an `instanceof` and a throwing `invokeinterface`, so admitting 0xc1 is
     * held to the real shape: it must not disturb frame publication for the
     * invoke that does throw.
     */
    static int instanceofStep(int i, boolean fail) {
        Cell tag = new Cell(i + 7);
        Step step = fail ? new Thrower() : new Adder(3);
        int r = 0;
        try {
            if (step instanceof Adder) {
                r = 1;
            }
            r += step.apply(i);
        } catch (Boom b) {
            // Reads `tag`, which the params-only reconstruction cannot restore.
            return tag.v * 10;
        }
        return r + tag.v;
    }

    public static int instanceofMismatches() {
        int bad = 0;
        for (int i = 0; i < 20000; i++) {
            int v = i % 97;
            boolean fail = (i % 3) == 0;
            int got;
            try {
                got = instanceofStep(v, fail);
            } catch (Boom escaped) {
                bad++;
                continue;
            }
            int want = fail ? (v + 7) * 10 : 1 + (v + 3) + (v + 7);
            if (got != want) {
                bad++;
            }
        }
        return bad;
    }

    public static int loopMismatches() {
        int bad = 0;
        for (int i = 0; i < 20000; i++) {
            int v = i % 97;
            boolean doubleIt = (i % 4) == 0;
            int got;
            try {
                got = loopCall(v, doubleIt);
            } catch (RuntimeException escaped) {
                // Either loopStep's own catch did not run, or the resumed frame
                // handed the loop a null iterator.
                bad++;
                continue;
            }
            // three adders (+1, +2, +4) and two throwers (-1 each)
            int sum = (v + 1) + (v + 2) + (v + 4) - 2;
            if (got != (doubleIt ? sum * 2 : sum)) {
                bad++;
            }
        }
        return bad;
    }
}

/**
 * Conformance probe for the population newly admitted to the optimizing (C2)
 * tier by the exception-table fix: methods that declare a `try`/`catch` whose
 * handler reads only parameters (so RBC.6 never fires and
 * `precise_exception_frames` stays off).
 *
 * The fix makes `IrBuilder::build` SKIP handler bodies instead of walking them
 * with stale abstract state. The risks that creates are all about the skip:
 *
 *   - resyncing after a skipped region (code textually AFTER a handler),
 *   - stepping over multi-byte operands (switch tables inside/after a try),
 *   - locals written in the try body and read after it,
 *   - a back-edge FROM a handler into a loop (the handler's `continue`),
 *     which must leave the loop-header region and its phis in lockstep,
 *   - the handler actually running: the exception must still be caught, with
 *     the right value, when the method is hot/compiled.
 *
 * Every case is run hot enough to compile and asserts the exact result the
 * interpreter would produce. Run identically on HotSpot and CratonVM; any
 * difference is a bug. Failure mode is NOT a crash — it is a wrong value.
 */
public final class C2Handlers {

    static int failures = 0;

    static void check(String what, Object actual, Object expected) {
        boolean ok = (actual == null) ? expected == null : actual.equals(expected);
        if (!ok) {
            failures++;
            if (failures < 40) {
                System.out.println("FAIL " + what + ": got " + actual + " want " + expected);
            }
        }
    }

    static class Boom extends RuntimeException {
        Boom() { super(null, null, false, false); }
    }

    static int sideEffects = 0;

    static int mayThrow(int n) {
        if ((n & 1) == 1) { throw new Boom(); }
        return n * 2;
    }

    // ---- 1. handler reads only the parameter, not taken on the hot path ----

    static int paramOnlyFast(int n) {
        try {
            return mayThrow(n) + 1;
        } catch (Boom e) {
            return -n;
        }
    }

    // ---- 2. code AFTER the handler must still be compiled correctly --------
    //
    // The handler body sits textually between the try and the tail, so the
    // builder skips a region and must resync on the tail's merge target.

    static int codeAfterHandler(int n) {
        int acc = n;
        try {
            acc += mayThrow(n);
        } catch (Boom e) {
            acc = -1;
        }
        acc = acc * 3 + 7;          // reachable ONLY by falling out of both arms
        if (acc > 1000) { acc -= 1000; }
        return acc;
    }

    // ---- 3. locals written inside the try, read after it -------------------

    static int localsAcrossTry(int n) {
        int a = n + 1;
        int b = 0;
        try {
            b = mayThrow(n);
            a = a + b;
        } catch (Boom e) {
            b = 99;
        }
        return a * 10 + b;
    }

    // ---- 4. a switch AFTER the handler: multi-byte operand resync ----------

    static int switchAfterHandler(int n) {
        int k;
        try {
            k = mayThrow(n);
        } catch (Boom e) {
            k = 3;
        }
        switch (k) {
            case 0: return 100;
            case 2: return 200;
            case 3: return 300;
            case 4: return 400;
            case 6: return 600;
            default: return 900 + k;
        }
    }

    // ---- 5. a switch INSIDE the try ---------------------------------------

    static int switchInsideTry(int n) {
        try {
            switch (n % 4) {
                case 0: return mayThrow(n);
                case 1: return mayThrow(n) + 1;
                case 2: return mayThrow(n) + 2;
                default: return mayThrow(n) + 3;
            }
        } catch (Boom e) {
            return -5;
        }
    }

    // ---- 6. back-edge FROM the handler: catch does `continue` --------------
    //
    // The loop header gets a back-edge from skipped handler code. Region
    // inputs and loop-carried phi inputs must stay in lockstep.

    static int handlerContinues(int n) {
        int sum = 0;
        for (int i = 0; i < n; i++) {
            try {
                sum += mayThrow(i);
            } catch (Boom e) {
                continue;           // backward branch out of the handler
            }
            sum += 1;               // only on the non-throwing path
        }
        return sum;
    }

    // ---- 7. nested try/catch ----------------------------------------------

    static int nestedTry(int n) {
        int r = 0;
        try {
            try {
                r = mayThrow(n);
            } catch (Boom e) {
                r = 7;
            }
            r += mayThrow(n + 1);
        } catch (Boom e) {
            r += 1000;
        }
        return r;
    }

    // ---- 8. two handlers on one try ---------------------------------------

    static int twoHandlers(int n) {
        try {
            if (n % 3 == 0) { throw new IllegalStateException(); }
            return mayThrow(n);
        } catch (Boom e) {
            return -1;
        } catch (IllegalStateException e) {
            return -2;
        }
    }

    // ---- 9. implicit exceptions inside the protected range -----------------

    static final int[] ARR = { 10, 20, 30 };

    static int implicitAioobe(int i) {
        try {
            return ARR[i];
        } catch (ArrayIndexOutOfBoundsException e) {
            return -77;
        }
    }

    static int implicitDivZero(int d) {
        try {
            return 100 / d;
        } catch (ArithmeticException e) {
            return -88;
        }
    }

    static int implicitNpe(int[] a) {
        try {
            return a.length;
        } catch (NullPointerException e) {
            return -99;
        }
    }

    // ---- 10. a loop INSIDE the try ----------------------------------------

    static int loopInsideTry(int n) {
        int sum = 0;
        try {
            for (int i = 0; i < n; i++) {
                sum += mayThrow(i * 2);   // never throws: i*2 is even
            }
            if (n > 3) { sum += mayThrow(n | 1); }   // throws for n > 3
        } catch (Boom e) {
            return -sum;
        }
        return sum;
    }

    // ---- expected values, computed without any try/catch ------------------

    static int expParamOnlyFast(int n)   { return ((n & 1) == 1) ? -n : n * 2 + 1; }
    static int expCodeAfterHandler(int n) {
        int acc = ((n & 1) == 1) ? -1 : n + n * 2;
        acc = acc * 3 + 7;
        if (acc > 1000) { acc -= 1000; }
        return acc;
    }
    static int expLocalsAcrossTry(int n) {
        int a = n + 1, b;
        if ((n & 1) == 1) { b = 99; } else { b = n * 2; a = a + b; }
        return a * 10 + b;
    }
    static int expSwitchAfterHandler(int n) {
        int k = ((n & 1) == 1) ? 3 : n * 2;
        switch (k) {
            case 0: return 100;
            case 2: return 200;
            case 3: return 300;
            case 4: return 400;
            case 6: return 600;
            default: return 900 + k;
        }
    }
    static int expSwitchInsideTry(int n) {
        if ((n & 1) == 1) { return -5; }
        int base = n * 2;
        switch (n % 4) {
            case 0: return base;
            case 2: return base + 2;
            default: return base + 3;   // n even ⇒ n%4 is 0 or 2
        }
    }
    static int expHandlerContinues(int n) {
        int sum = 0;
        for (int i = 0; i < n; i++) {
            if ((i & 1) == 1) { continue; }
            sum += i * 2;
            sum += 1;
        }
        return sum;
    }
    static int expNestedTry(int n) {
        int r = ((n & 1) == 1) ? 7 : n * 2;
        if (((n + 1) & 1) == 1) { return r + 1000; }
        return r + (n + 1) * 2;
    }
    static int expTwoHandlers(int n) {
        if (n % 3 == 0) { return -2; }
        return ((n & 1) == 1) ? -1 : n * 2;
    }
    static int expLoopInsideTry(int n) {
        int sum = 0;
        for (int i = 0; i < n; i++) { sum += i * 2 * 2; }
        if (n > 3) { return -sum; }   // (n|1) is odd ⇒ throws
        return sum;
    }

    static String only = "all";

    static boolean on(String name) { return only.equals("all") || only.equals(name); }

    static void runAll(int n) {
        if (on("paramOnlyFast")) check("paramOnlyFast/" + n, paramOnlyFast(n), expParamOnlyFast(n));
        if (on("codeAfterHandler")) check("codeAfterHandler/" + n, codeAfterHandler(n), expCodeAfterHandler(n));
        if (on("localsAcrossTry")) check("localsAcrossTry/" + n, localsAcrossTry(n), expLocalsAcrossTry(n));
        if (on("switchAfterHandler")) check("switchAfterHandler/" + n, switchAfterHandler(n), expSwitchAfterHandler(n));
        if (on("switchInsideTry")) check("switchInsideTry/" + n, switchInsideTry(n), expSwitchInsideTry(n));
        if (on("handlerContinues")) check("handlerContinues/" + n, handlerContinues(n % 17), expHandlerContinues(n % 17));
        if (on("nestedTry")) check("nestedTry/" + n, nestedTry(n), expNestedTry(n));
        if (on("twoHandlers")) check("twoHandlers/" + n, twoHandlers(n), expTwoHandlers(n));
        if (on("implicitAioobe")) check("implicitAioobe/" + n, implicitAioobe(n % 5), (n % 5) < 3 ? ARR[n % 5] : -77);
        if (on("implicitDivZero")) check("implicitDivZero/" + n, implicitDivZero(n % 4), (n % 4) == 0 ? -88 : 100 / (n % 4));
        if (on("implicitNpe")) check("implicitNpe/" + n, implicitNpe((n % 6 == 0) ? null : ARR), (n % 6 == 0) ? -99 : 3);
        if (on("loopInsideTry")) check("loopInsideTry/" + n, loopInsideTry(n % 9), expLoopInsideTry(n % 9));
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        only = args.length > 1 ? args[1] : "all";
        for (int i = 0; i < iters; i++) {
            runAll(i % 23);
        }
        System.out.println("iterations=" + iters + " failures=" + failures);
        System.out.println(failures == 0 ? "C2 HANDLERS OK" : "C2 HANDLERS BROKEN");
        if (failures != 0) { System.exit(1); }
    }
}

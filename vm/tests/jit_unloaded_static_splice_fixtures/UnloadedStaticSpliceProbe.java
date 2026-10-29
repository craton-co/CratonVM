// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
public final class UnloadedStaticSpliceProbe {
    static int before, after, initialized, failures;
    static volatile int sink;
    static int check(String message, int expected, int actual) {
        if (expected != actual) return UnloadedFailure.fail(message, -123);
        return actual;
    }
    static void step(String message, int actual) {
        before++;
        sink = check(message, 7, actual);
        after++;
    }
    static int checkLoop(String message, int expected, int actual) {
        if (expected != actual) return UnloadedLoopFailure.fail(message, -321);
        return actual;
    }
    static void loopAndFail(String message) {
        for (int i = 0; i < 10000; i++) {
            before++;
            sink = checkLoop(message, 7, i == 9999 ? 8 : 7);
            after++;
        }
    }
    public static void main(String[] args) {
        String message = new String("cold-call-token");
        // Let the background compiler finish the caller's direct callee
        // bindings before the second cold exit. A shorter warmup can instead
        // enter loopAndFail through optimizing OSR and miss the service path.
        for (int i = 0; i < 200000; i++) step(message, 7);
        if (initialized != 0) throw new AssertionError("eager class initialization");
        System.gc();
        int beforeWas = before, afterWas = after;
        try { step(message, 8); throw new AssertionError("missing failure"); }
        catch (IllegalArgumentException e) {
            if (!"cold-call-token:-123".equals(e.getMessage())) throw new AssertionError(e);
        }
        if (before != beforeWas + 1 || after != afterWas || initialized != 1 || failures != 1)
            throw new AssertionError("replayed effect or lost scope: " + before + "/" + after + "/" + initialized + "/" + failures);
        step(message, 7);
        if (after != afterWas + 1) throw new AssertionError("hot path after cold exit");
        beforeWas = before; afterWas = after;
        try { loopAndFail(message); throw new AssertionError("missing loop failure"); }
        catch (IllegalArgumentException e) {
            if (!"cold-call-token:-321".equals(e.getMessage())) throw new AssertionError(e);
        }
        if (before != beforeWas + 10000 || after != afterWas + 9999 || initialized != 2 || failures != 2)
            throw new AssertionError("OSR replayed effect or lost scope: " + before + "/" + after + "/" + initialized + "/" + failures);
        System.out.println("UNLOADED-STATIC-SPLICE-OK");
    }
}
final class UnloadedLoopFailure {
    static { UnloadedStaticSpliceProbe.initialized++; }
    static int fail(String message, int value) {
        UnloadedStaticSpliceProbe.failures++;
        System.gc();
        throw new IllegalArgumentException(message + ":" + value);
    }
}
final class UnloadedFailure {
    static { UnloadedStaticSpliceProbe.initialized++; }
    static int fail(String message, int value) {
        UnloadedStaticSpliceProbe.failures++;
        System.gc();
        throw new IllegalArgumentException(message + ":" + value);
    }
}

// A root activation begins interpreted rather than being eagerly direct-bound
// to a C1 body by its already compiled caller. The cold call mutates memory and
// returns into a live loop, which must import the interpreter's updated state.
final class UnloadedStaticOsrProbe {
    static int before, after, initialized, failures;
    static volatile int sink;
    static int check(String token, int expected, int actual) {
        if (expected != actual) return UnloadedOsrCold.resolve(token, actual);
        return actual;
    }
    public static void main(String[] args) {
        int total = 0;
        String token = makeToken();
        for (int i = 0; i < 200000; i++) {
            before++;
            int value = check(token, 7, i == 120000 ? 8 : 7);
            total += value;
            sink = total;
            after++;
        }
        finish(token, total);
    }
    static String makeToken() { return new String("osr-cold-token"); }
    static void finish(String token, int total) {
        if (!"osr-cold-token".equals(token)) throw new AssertionError("lost caller token");
        if (before != 200000 || after != 200000 || initialized != 1 || failures != 1 || total != 1400001)
            throw new AssertionError("OSR replay or lost state: " + before + "/" + after + "/" + initialized + "/" + failures + "/" + total);
        System.out.println("UNLOADED-STATIC-OSR-OK");
    }
}
final class UnloadedOsrCold {
    static { UnloadedStaticOsrProbe.initialized++; }
    static int resolve(String token, int value) {
        if (UnloadedStaticOsrProbe.before != 120001 || UnloadedStaticOsrProbe.after != 120000)
            throw new AssertionError("cold scope replayed");
        UnloadedStaticOsrProbe.failures++;
        System.gc();
        if (!"osr-cold-token".equals(token)) throw new AssertionError("lost cold-call operand");
        return value;
    }
}

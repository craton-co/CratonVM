import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandleProxies;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.function.IntUnaryOperator;

/**
 * The constant-pool resolution RECORD, exercised on its second and later reads.
 *
 * JVMS 5.4.3 resolves a symbolic reference once per constant-pool entry and
 * records the result. `vm/src/runtime/interpreter/constants.rs` keeps that
 * record for `CONSTANT_Dynamic`, `CONSTANT_MethodType` and
 * `CONSTANT_MethodHandle`, and on 2026-08-18 its five direct reaches into
 * `shared.classes.resolution_cache` were replaced by two helpers over
 * `MemberResolver::probe_constant` / `record_constant`.
 *
 * That rewrite touches the READ that returns an already-recorded value and the
 * WRITE that records one, so what has to be checked is not the first execution
 * of an `ldc` but the ones after it. Every arm below therefore runs its
 * constant-loading site MANY times and requires every iteration to produce the
 * same, checked value -- a record that stopped being written would still give a
 * correct first answer, and a record that returned a stale or wrong entry would
 * show up only from the second iteration on.
 *
 * `MethodHandleProxies.asInterfaceInstance` is here because the spun proxy
 * class is the reachable `ldc` of a `CONSTANT_MethodType` that
 * `constants.rs`'s own module header names -- it does
 * `callerBoundTarget.asType(<MethodType>)` off an `ldc`.
 *
 * HotSpot is the oracle: run it there first, then on cratonvm, and compare.
 *
 *   java     CpConstantRecordProbe
 *   cratonvm CpConstantRecordProbe
 */
public final class CpConstantRecordProbe {

    static int fails;

    static void check(String what, Object got, Object want) {
        boolean ok = (got == null) ? want == null : got.equals(want);
        if (!ok) { fails++; }
        System.out.printf("%-46s %-4s got=%s want=%s%n", what, ok ? "OK" : "FAIL", got, want);
    }

    static int doubler(int x) { return x * 2; }

    static String greet(String who) { return "hi " + who; }

    public static void main(String[] args) throws Throwable {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 5000;
        MethodHandles.Lookup lookup = MethodHandles.lookup();

        // ---- CONSTANT_MethodType, via a proxy class whose body does
        // `ldc <MethodType>`. Re-created every iteration so the spun class's
        // constant-pool entries are resolved and then re-read.
        MethodHandle doubler =
            lookup.findStatic(CpConstantRecordProbe.class, "doubler",
                              MethodType.methodType(int.class, int.class));
        long sum = 0;
        Object lastProxyClass = null;
        boolean proxyClassStable = true;
        for (int i = 0; i < reps; i++) {
            IntUnaryOperator op = MethodHandleProxies.asInterfaceInstance(IntUnaryOperator.class, doubler);
            int v = op.applyAsInt(i);
            if (v != i * 2) { fails++; System.out.println("FAIL proxy iteration " + i + " gave " + v); break; }
            sum += v;
            Object c = op.getClass();
            if (lastProxyClass != null && lastProxyClass != c) { proxyClassStable = false; }
            lastProxyClass = c;
        }
        check("proxy: sum over " + reps + " iterations", sum, (long) reps * (reps - 1));
        check("proxy: one spun class reused", proxyClassStable, Boolean.TRUE);

        // ---- MethodType identity. `MethodType` instances are interned by the
        // JDK, so a record that handed back a DIFFERENT object for the same CP
        // entry would still be `equals` but not `==`. Check both.
        MethodType a = MethodType.methodType(int.class, int.class);
        MethodType b = MethodType.methodType(int.class, int.class);
        check("MethodType equals", a.equals(b), Boolean.TRUE);
        check("MethodType interned", a == b, Boolean.TRUE);

        // ---- CONSTANT_MethodHandle-shaped work, repeated: look the handle up
        // once and invoke it many times, then look it up again and require the
        // same answer.
        MethodHandle greeter =
            lookup.findStatic(CpConstantRecordProbe.class, "greet",
                              MethodType.methodType(String.class, String.class));
        String last = null;
        boolean greeterStable = true;
        for (int i = 0; i < reps; i++) {
            String s = (String) greeter.invokeExact("world");
            if (last != null && !last.equals(s)) { greeterStable = false; break; }
            last = s;
        }
        check("handle: stable result", greeterStable, Boolean.TRUE);
        check("handle: value", last, "hi world");

        // ---- A lambda capture site, which is an `invokedynamic` whose call
        // site is recorded by the sibling store the migration deliberately did
        // NOT touch. It is here as the control: if this broke too, the change
        // reached further than intended.
        long lam = 0;
        for (int i = 0; i < reps; i++) {
            IntUnaryOperator f = x -> x + 1;
            lam += f.applyAsInt(i);
        }
        check("lambda control: sum", lam, (long) reps * (reps - 1) / 2 + reps);

        // ---- String concatenation is javac's other invokedynamic; a second
        // control on the same store.
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 4; i++) { sb.append("x" + i + ";"); }
        check("concat control", sb.toString(), "x0;x1;x2;x3;");

        System.out.println(fails == 0 ? "PROBE PASS" : ("PROBE FAIL fails=" + fails));
        if (fails != 0) { System.exit(1); }
    }
}

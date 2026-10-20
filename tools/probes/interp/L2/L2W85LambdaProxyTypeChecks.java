// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 85, lane L2 -- every type check asked of a LAMBDA
// (a hidden class on HotSpot, a synthetic `0x8000_0000+` class id here, with no
// class-store entry). Wave 84's L2W84ArrayStoreHiddenName found
// `Object[] a = new String[1]; a[0] = (Runnable) () -> {};` storing the lambda
// (`typecheck::aastore_element_assignable` failed open on "element class not in
// the store"): heap type safety broken. Every door must judge a lambda by its
// real interfaces: its functional interface, `Serializable` only for a
// serializable lambda, and `Object`.
//
// Each row runs its check 3000 times (so the JIT compiles it, when on) and
// prints the verdict of the FIRST run plus `stable` (every run agreed). The
// message is the first run's, so OmitStackTraceInFastThrow (which drops the
// message of a hot C2 throw on HotSpot) cannot move it. `$$Lambda/0x<hex>` is
// normalised to `$$Lambda/0xN`.
//
// Run (stdout must be the block below, byte for byte):
//   $J/javac -d out tools/probes/interp/L2/L2W85LambdaProxyTypeChecks.java
//   $B --java-home $J/.. -cp out L2W85LambdaProxyTypeChecks
//   $B --nojit --java-home $J/.. -cp out L2W85LambdaProxyTypeChecks
//   $B --compatible --java-home $J/.. -cp out L2W85LambdaProxyTypeChecks   (and --nojit)
//   $J/java -cp out L2W85LambdaProxyTypeChecks
//
// Derived from the JVMS (aastore, checkcast, instanceof), Class.isInstance /
// Class.cast (`cannotCastMsg`), JVM_ArrayCopy (`ObjArrayKlass::do_copy`) and
// Method.invoke's `argument type mismatch`; refresh from $J/java before reading
// a DIFF.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// aastore-String[] ArrayStoreException: L2W85LambdaProxyTypeChecks$$Lambda/0xN stable=true
// aastore-Runnable[] stored stable=true
// aastore-Object[] stored stable=true
// aastore-Comparable[] ArrayStoreException: L2W85LambdaProxyTypeChecks$$Lambda/0xN stable=true
// aastore-Serializable[]-plain ArrayStoreException: L2W85LambdaProxyTypeChecks$$Lambda/0xN stable=true
// aastore-Serializable[]-serializable stored stable=true
// checkcast-String ClassCastException: class L2W85LambdaProxyTypeChecks$$Lambda/0xN cannot be cast to class java.lang.String (L2W85LambdaProxyTypeChecks$$Lambda/0xN is in unnamed module of loader 'app'; java.lang.String is in module java.base of loader 'bootstrap') stable=true
// checkcast-Runnable ok stable=true
// checkcast-Comparable ClassCastException: class L2W85LambdaProxyTypeChecks$$Lambda/0xN cannot be cast to class java.lang.Comparable (L2W85LambdaProxyTypeChecks$$Lambda/0xN is in unnamed module of loader 'app'; java.lang.Comparable is in module java.base of loader 'bootstrap') stable=true
// instanceof-String false stable=true
// instanceof-Runnable true stable=true
// instanceof-Serializable-plain false stable=true
// instanceof-Serializable-serializable true stable=true
// isInstance-String false stable=true
// isInstance-Runnable true stable=true
// isInstance-Comparable false stable=true
// cast-String ClassCastException: Cannot cast L2W85LambdaProxyTypeChecks$$Lambda/0xN to java.lang.String stable=true
// cast-Runnable ok stable=true
// arraycopy-String[] ArrayStoreException: arraycopy: element type mismatch: can not cast one of the elements of java.lang.Object[] to the type of the destination array, java.lang.String stable=true
// arraycopy-Runnable[] copied stable=true
// copyOf-String[] ArrayStoreException: arraycopy: element type mismatch: can not cast one of the elements of java.lang.Object[] to the type of the destination array, java.lang.String stable=true
// invoke-Comparable-formal IllegalArgumentException: argument type mismatch stable=true
// invoke-Runnable-formal invoked stable=true
// HOTSPOT_EXPECTED_END
import java.io.Serializable;
import java.lang.reflect.Method;
import java.util.Arrays;
import java.util.function.Function;

public class L2W85LambdaProxyTypeChecks {
    static final int RUNS = 3000;
    static final Runnable PLAIN = () -> { };
    static final Runnable SERIAL = (Runnable & Serializable) () -> { };

    static String norm(String s) {
        return s == null ? "null" : s.replaceAll("\\$\\$Lambda/0x[0-9a-fA-F]+", "\\$\\$Lambda/0xN");
    }

    static String outcome(Throwable t) {
        return t.getClass().getSimpleName() + ": " + norm(t.getMessage());
    }

    static void row(String label, Function<Object, String> check, Object value) {
        String first = null;
        String firstKind = null;
        boolean stable = true;
        for (int i = 0; i < RUNS; i++) {
            String r;
            String kind;
            try {
                r = check.apply(value);
                kind = r;
            } catch (Throwable t) {
                r = outcome(t);
                kind = t.getClass().getName();
            }
            if (first == null) {
                first = r;
                firstKind = kind;
            } else if (!kind.equals(firstKind)) {
                stable = false;
            }
        }
        System.out.println(label + " " + first + " stable=" + stable);
    }

    static String storeString(Object v) {
        Object[] a = new String[1];
        a[0] = v;
        return "stored";
    }

    static String storeRunnable(Object v) {
        Object[] a = new Runnable[1];
        a[0] = v;
        return "stored";
    }

    static String storeObject(Object v) {
        Object[] a = new Object[1];
        a[0] = v;
        return "stored";
    }

    static String storeComparable(Object v) {
        Object[] a = new Comparable[1];
        a[0] = v;
        return "stored";
    }

    static String storeSerializable(Object v) {
        Object[] a = new Serializable[1];
        a[0] = v;
        return "stored";
    }

    static String castString(Object v) {
        String s = (String) v;
        return s == null ? "null" : "ok";
    }

    static String castRunnable(Object v) {
        Runnable r = (Runnable) v;
        return r == null ? "null" : "ok";
    }

    @SuppressWarnings({"rawtypes", "unused"})
    static String castComparable(Object v) {
        Comparable c = (Comparable) v;
        return c == null ? "null" : "ok";
    }

    static String copyInto(Object v, Object[] dest) {
        Object[] src = {v};
        System.arraycopy(src, 0, dest, 0, 1);
        return "copied";
    }

    public static void takeComparable(Comparable<?> c) {
    }

    public static void takeRunnable(Runnable r) {
    }

    static String invoke(Method m, Object v) {
        try {
            m.invoke(null, v);
            return "invoked";
        } catch (IllegalArgumentException e) {
            return outcome(e);
        } catch (ReflectiveOperationException e) {
            return outcome(e);
        }
    }

    public static void main(String[] args) throws Exception {
        row("aastore-String[]", L2W85LambdaProxyTypeChecks::storeString, PLAIN);
        row("aastore-Runnable[]", L2W85LambdaProxyTypeChecks::storeRunnable, PLAIN);
        row("aastore-Object[]", L2W85LambdaProxyTypeChecks::storeObject, PLAIN);
        row("aastore-Comparable[]", L2W85LambdaProxyTypeChecks::storeComparable, PLAIN);
        row("aastore-Serializable[]-plain", L2W85LambdaProxyTypeChecks::storeSerializable, PLAIN);
        row("aastore-Serializable[]-serializable", L2W85LambdaProxyTypeChecks::storeSerializable, SERIAL);

        row("checkcast-String", L2W85LambdaProxyTypeChecks::castString, PLAIN);
        row("checkcast-Runnable", L2W85LambdaProxyTypeChecks::castRunnable, PLAIN);
        row("checkcast-Comparable", L2W85LambdaProxyTypeChecks::castComparable, PLAIN);

        row("instanceof-String", v -> String.valueOf(v instanceof String), PLAIN);
        row("instanceof-Runnable", v -> String.valueOf(v instanceof Runnable), PLAIN);
        row("instanceof-Serializable-plain", v -> String.valueOf(v instanceof Serializable), PLAIN);
        row("instanceof-Serializable-serializable", v -> String.valueOf(v instanceof Serializable), SERIAL);

        row("isInstance-String", v -> String.valueOf(String.class.isInstance(v)), PLAIN);
        row("isInstance-Runnable", v -> String.valueOf(Runnable.class.isInstance(v)), PLAIN);
        row("isInstance-Comparable", v -> String.valueOf(Comparable.class.isInstance(v)), PLAIN);

        row("cast-String", v -> String.class.cast(v) == null ? "null" : "ok", PLAIN);
        row("cast-Runnable", v -> Runnable.class.cast(v) == null ? "null" : "ok", PLAIN);

        row("arraycopy-String[]", v -> copyInto(v, new String[1]), PLAIN);
        row("arraycopy-Runnable[]", v -> copyInto(v, new Runnable[1]), PLAIN);
        row("copyOf-String[]", v -> {
            Object[] src = {v};
            String[] out = Arrays.copyOf(src, 1, String[].class);
            return out.length == 1 ? "copied" : "short";
        }, PLAIN);

        Method takeComparable = L2W85LambdaProxyTypeChecks.class.getMethod("takeComparable", Comparable.class);
        Method takeRunnable = L2W85LambdaProxyTypeChecks.class.getMethod("takeRunnable", Runnable.class);
        row("invoke-Comparable-formal", v -> invoke(takeComparable, v), PLAIN);
        row("invoke-Runnable-formal", v -> invoke(takeRunnable, v), PLAIN);
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.concurrent.ConcurrentHashMap;
import java.util.function.Function;
import java.util.function.Supplier;

/**
 * JIT round 21 wave 4, lane compat: does every door into a JDK method that has a registered
 * native in {@code --compatible} reach the SAME implementation the interpreter's cold call
 * reaches? For each row the JDK method is called five ways and the result (or the exception's
 * class and message) is printed per door:
 *
 * <ul>
 *   <li>{@code cold}: the first call from a small static method (interpreted);</li>
 *   <li>{@code warm}: the last of WARM calls through that same method (compiled, OSR or tier-up);</li>
 *   <li>{@code lambda}: the last of WARM calls through a lambda whose body makes the call;</li>
 *   <li>{@code mh}: the last of WARM calls through {@code MethodHandles.lookup().findStatic/findVirtual};</li>
 *   <li>{@code reflect}: the last of WARM calls through {@code Method.invoke} (cause unwrapped).</li>
 * </ul>
 *
 * <p>Then {@code agree <row> yes|no}: whether the five lines are identical. Every line is
 * deterministic; no timing lines.
 *
 * <p>Run: {@code cratonvm -Xmx512m R21CompatDispatchRows.java} (launcher default {@code --jdk-only}),
 * again with {@code --compatible}, and with {@code --nojit} as the interpreter control; diff each
 * against {@code java -Xmx512m R21CompatDispatchRows.java} (HotSpot 25). No {@code --add-exports}
 * needed. Kill-switch control for COMPAT21-1: {@code CRATONVM_JIT_THIN_HELPER_ASKS_RESOLVER=0}.
 *
 * <p>HotSpot expectation: every row prints {@code agree <row> yes}, and the per-door lines are the
 * JDK 25 bodies' answers (e.g. {@code objects-checkIndex-oob} is
 * {@code EXC java.lang.IndexOutOfBoundsException: Index 5 out of bounds for length 3},
 * {@code integer-valueOf-128} is {@code 128 cached=false}). Both CratonVM modes must equal it byte
 * for byte.
 */
public class R21CompatDispatchRows {
    static final int WARM = 12_000;
    static final Thread MAIN = Thread.currentThread();
    static final ThreadLocal<String> TL = new ThreadLocal<>();

    static String show(Object r) {
        if (r == null) return "null";
        if (r instanceof Object[] arr) return Arrays.deepToString(arr);
        if (r instanceof int[] arr) return Arrays.toString(arr);
        return String.valueOf(r);
    }

    static String exc(Throwable t) {
        if (t instanceof InvocationTargetException ite && ite.getCause() != null) t = ite.getCause();
        return "EXC " + t.getClass().getName() + ": " + t.getMessage();
    }

    interface Call { Object call(Object[] a) throws Throwable; }

    static String once(Call c, Supplier<Object[]> args, Function<Object, Object> post) {
        try {
            Object r = c.call(args.get());
            return show(post == null ? r : post.apply(r));
        } catch (Throwable t) {
            return exc(t);
        }
    }

    static String warmed(Call c, Supplier<Object[]> args, Function<Object, Object> post) {
        String last = null;
        for (int i = 0; i < WARM; i++) last = once(c, args, post);
        return last;
    }

    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d00(Object[] a) { return Math.floorMod((Integer) a[0], (Integer) a[1]); }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d01(Object[] a) { return Math.addExact((Integer) a[0], (Integer) a[1]); }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d02(Object[] a) { return Math.abs((Integer) a[0]); }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d03(Object[] a) { return Math.sqrt((Double) a[0]); }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d04(Object[] a) { return StrictMath.cbrt((Double) a[0]); }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d05(Object[] a) { return ((String) a[0]).codePointAt((Integer) a[1]); }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d06(Object[] a) { return ((String) a[0]).intern(); }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d07(Object[] a) { return ((String) a[0]).isBlank(); }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d08(Object[] a) { return ((String) a[0]).matches((String) a[1]); }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d09(Object[] a) { return String.format((String) a[0], (Object[]) a[1]); }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d10(Object[] a) { return Objects.checkIndex((Integer) a[0], (Integer) a[1]); }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d11(Object[] a) { return Objects.checkIndex((Integer) a[0], (Integer) a[1]); }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d12(Object[] a) { return Objects.requireNonNull(a[0], (String) a[1]); }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d13(Object[] a) { return Objects.hash((Object[]) a[0]); }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d14(Object[] a) { return Integer.valueOf((Integer) a[0]); }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d15(Object[] a) { return Integer.valueOf((Integer) a[0]); }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d16(Object[] a) { return ((Integer) a[0]).intValue(); }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d17(Object[] a) { return Long.valueOf((Long) a[0]); }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d18(Object[] a) { return Character.forDigit((Integer) a[0], (Integer) a[1]); }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d19(Object[] a) { System.arraycopy(a[0], (Integer) a[1], a[2], (Integer) a[3], (Integer) a[4]); return "void"; }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d20(Object[] a) { System.arraycopy(a[0], (Integer) a[1], a[2], (Integer) a[3], (Integer) a[4]); return "void"; }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d21(Object[] a) { return ((Class<?>) a[0]).cast(a[1]); }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d22(Object[] a) { return ((Class<?>) a[0]).getCanonicalName(); }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d23(Object[] a) { return ((Class<?>) a[0]).arrayType(); }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d24(Object[] a) { return Thread.currentThread(); }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d25(Object[] a) { return ((ThreadLocal<?>) a[0]).get(); }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d26(Object[] a) { return ((ArrayList<?>) a[0]).get((Integer) a[1]); }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d27(Object[] a) { return ((HashMap<Object, Object>) a[0]).put(a[1], a[2]); }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d28(Object[] a) { return ((ConcurrentHashMap<?, ?>) a[0]).get(a[1]); }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d29(Object[] a) { return ((ByteBuffer) a[0]).get((Integer) a[1]); }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d30(Object[] a) { return ((ByteBuffer) a[0]).put((Integer) a[1], (Byte) a[2]); }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d31(Object[] a) { java.lang.ref.Reference.reachabilityFence(a[0]); return "void"; }
    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object d32(Object[] a) { return a[0].getClass(); }

    static void row(String name, Call direct, Call lambda, Class<?> owner, String method, boolean isStatic,
            MethodType type, Supplier<Object[]> args, Function<Object, Object> post) throws Throwable {
        String cold = once(direct, args, post);
        String warm = warmed(direct, args, post);
        String lam = warmed(lambda, args, post);
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodHandle mh = (isStatic ? lookup.findStatic(owner, method, type)
                : lookup.findVirtual(owner, method, type)).asFixedArity();
        String viaMh = warmed(a -> mh.invokeWithArguments(a), args, post);
        Method rm = owner.getMethod(method, type.parameterArray());
        String viaReflect = warmed(a -> isStatic ? rm.invoke(null, a)
                : rm.invoke(a[0], Arrays.copyOfRange(a, 1, a.length)), args, post);
        System.out.println("row " + name + " cold " + cold);
        System.out.println("row " + name + " warm " + warm);
        System.out.println("row " + name + " lambda " + lam);
        System.out.println("row " + name + " mh " + viaMh);
        System.out.println("row " + name + " reflect " + viaReflect);
        boolean agree = cold.equals(warm) && cold.equals(lam) && cold.equals(viaMh) && cold.equals(viaReflect);
        System.out.println("agree " + name + " " + (agree ? "yes" : "no"));
    }

    @SuppressWarnings({"unchecked", "rawtypes"})
    public static void main(String[] argv) throws Throwable {
        TL.set("r21-tl-value");
        row("math-floorMod-div0", R21CompatDispatchRows::d00, a -> { return Math.floorMod((Integer) a[0], (Integer) a[1]); },
                Math.class, "floorMod", true, MethodType.methodType(int.class, int.class, int.class),
                () -> new Object[] {7, 0}, null);
        row("math-addExact-overflow", R21CompatDispatchRows::d01, a -> { return Math.addExact((Integer) a[0], (Integer) a[1]); },
                Math.class, "addExact", true, MethodType.methodType(int.class, int.class, int.class),
                () -> new Object[] {Integer.MAX_VALUE, 1}, null);
        row("math-abs-min", R21CompatDispatchRows::d02, a -> { return Math.abs((Integer) a[0]); },
                Math.class, "abs", true, MethodType.methodType(int.class, int.class),
                () -> new Object[] {Integer.MIN_VALUE}, null);
        row("math-sqrt-neg-bits", R21CompatDispatchRows::d03, a -> { return Math.sqrt((Double) a[0]); },
                Math.class, "sqrt", true, MethodType.methodType(double.class, double.class),
                () -> new Object[] {-1.0}, r -> Long.toHexString(Double.doubleToRawLongBits((Double) r)));
        row("strictmath-cbrt", R21CompatDispatchRows::d04, a -> { return StrictMath.cbrt((Double) a[0]); },
                StrictMath.class, "cbrt", true, MethodType.methodType(double.class, double.class),
                () -> new Object[] {-27.0}, null);
        row("string-codePointAt-oob", R21CompatDispatchRows::d05, a -> { return ((String) a[0]).codePointAt((Integer) a[1]); },
                String.class, "codePointAt", false, MethodType.methodType(int.class, int.class),
                () -> new Object[] {"abc", 5}, null);
        row("string-intern-identity", R21CompatDispatchRows::d06, a -> { return ((String) a[0]).intern(); },
                String.class, "intern", false, MethodType.methodType(String.class),
                () -> new Object[] {new String("r21compat-intern")}, r -> r + " literal=" + (r == "r21compat-intern"));
        row("string-isBlank", R21CompatDispatchRows::d07, a -> { return ((String) a[0]).isBlank(); },
                String.class, "isBlank", false, MethodType.methodType(boolean.class),
                () -> new Object[] {" \t "}, null);
        row("string-matches", R21CompatDispatchRows::d08, a -> { return ((String) a[0]).matches((String) a[1]); },
                String.class, "matches", false, MethodType.methodType(boolean.class, String.class),
                () -> new Object[] {"a1b2", "[a-z0-9]+"}, null);
        row("string-format", R21CompatDispatchRows::d09, a -> { return String.format((String) a[0], (Object[]) a[1]); },
                String.class, "format", true, MethodType.methodType(String.class, String.class, Object[].class),
                () -> new Object[] {"%05d|%s", new Object[] {42, "x"}}, null);
        row("objects-checkIndex-oob", R21CompatDispatchRows::d10, a -> { return Objects.checkIndex((Integer) a[0], (Integer) a[1]); },
                Objects.class, "checkIndex", true, MethodType.methodType(int.class, int.class, int.class),
                () -> new Object[] {5, 3}, null);
        row("objects-checkIndex-in", R21CompatDispatchRows::d11, a -> { return Objects.checkIndex((Integer) a[0], (Integer) a[1]); },
                Objects.class, "checkIndex", true, MethodType.methodType(int.class, int.class, int.class),
                () -> new Object[] {2, 3}, null);
        row("objects-requireNonNull-msg", R21CompatDispatchRows::d12, a -> { return Objects.requireNonNull(a[0], (String) a[1]); },
                Objects.class, "requireNonNull", true, MethodType.methodType(Object.class, Object.class, String.class),
                () -> new Object[] {null, "r21 compat message"}, null);
        row("objects-hash", R21CompatDispatchRows::d13, a -> { return Objects.hash((Object[]) a[0]); },
                Objects.class, "hash", true, MethodType.methodType(int.class, Object[].class),
                () -> new Object[] {new Object[] {1, "a", null}}, null);
        row("integer-valueOf-127", R21CompatDispatchRows::d14, a -> { return Integer.valueOf((Integer) a[0]); },
                Integer.class, "valueOf", true, MethodType.methodType(Integer.class, int.class),
                () -> new Object[] {127}, r -> r + " cached=" + (r == Integer.valueOf((Integer) r)));
        row("integer-valueOf-128", R21CompatDispatchRows::d15, a -> { return Integer.valueOf((Integer) a[0]); },
                Integer.class, "valueOf", true, MethodType.methodType(Integer.class, int.class),
                () -> new Object[] {128}, r -> r + " cached=" + (r == Integer.valueOf((Integer) r)));
        row("integer-intValue", R21CompatDispatchRows::d16, a -> { return ((Integer) a[0]).intValue(); },
                Integer.class, "intValue", false, MethodType.methodType(int.class),
                () -> new Object[] {Integer.valueOf(-5)}, null);
        row("long-valueOf-127", R21CompatDispatchRows::d17, a -> { return Long.valueOf((Long) a[0]); },
                Long.class, "valueOf", true, MethodType.methodType(Long.class, long.class),
                () -> new Object[] {127L}, r -> r + " cached=" + (r == Long.valueOf((Long) r)));
        row("character-forDigit", R21CompatDispatchRows::d18, a -> { return Character.forDigit((Integer) a[0], (Integer) a[1]); },
                Character.class, "forDigit", true, MethodType.methodType(char.class, int.class, int.class),
                () -> new Object[] {11, 16}, null);
        row("system-arraycopy-oob", R21CompatDispatchRows::d19, a -> { System.arraycopy(a[0], (Integer) a[1], a[2], (Integer) a[3], (Integer) a[4]); return "void"; },
                System.class, "arraycopy", true, MethodType.methodType(void.class, Object.class, int.class, Object.class, int.class, int.class),
                () -> new Object[] {new int[3], 0, new int[3], 1, 5}, null);
        row("system-arraycopy-store", R21CompatDispatchRows::d20, a -> { System.arraycopy(a[0], (Integer) a[1], a[2], (Integer) a[3], (Integer) a[4]); return "void"; },
                System.class, "arraycopy", true, MethodType.methodType(void.class, Object.class, int.class, Object.class, int.class, int.class),
                () -> new Object[] {new Object[] {"s", 1}, 0, new String[2], 0, 2}, null);
        row("class-cast-fail", R21CompatDispatchRows::d21, a -> { return ((Class<?>) a[0]).cast(a[1]); },
                Class.class, "cast", false, MethodType.methodType(Object.class, Object.class),
                () -> new Object[] {String.class, 1}, null);
        row("class-getCanonicalName", R21CompatDispatchRows::d22, a -> { return ((Class<?>) a[0]).getCanonicalName(); },
                Class.class, "getCanonicalName", false, MethodType.methodType(String.class),
                () -> new Object[] {java.util.Map.Entry[][].class}, null);
        row("class-arrayType", R21CompatDispatchRows::d23, a -> { return ((Class<?>) a[0]).arrayType(); },
                Class.class, "arrayType", false, MethodType.methodType(Class.class),
                () -> new Object[] {String.class}, r -> ((Class<?>) r).getName());
        row("thread-currentThread", R21CompatDispatchRows::d24, a -> { return Thread.currentThread(); },
                Thread.class, "currentThread", true, MethodType.methodType(Thread.class),
                () -> new Object[0], r -> ((Thread) r).getName() + " same=" + (r == MAIN));
        row("threadlocal-get", R21CompatDispatchRows::d25, a -> { return ((ThreadLocal<?>) a[0]).get(); },
                ThreadLocal.class, "get", false, MethodType.methodType(Object.class),
                () -> new Object[] {TL}, null);
        row("arraylist-get-oob", R21CompatDispatchRows::d26, a -> { return ((ArrayList<?>) a[0]).get((Integer) a[1]); },
                ArrayList.class, "get", false, MethodType.methodType(Object.class, int.class),
                () -> new Object[] {new ArrayList<>(List.of("x")), 3}, null);
        row("hashmap-put-prev", R21CompatDispatchRows::d27, a -> { return ((HashMap<Object, Object>) a[0]).put(a[1], a[2]); },
                HashMap.class, "put", false, MethodType.methodType(Object.class, Object.class, Object.class),
                () -> new Object[] {new HashMap<>(Map.of("k", 1)), "k", 2}, null);
        row("chm-get-null", R21CompatDispatchRows::d28, a -> { return ((ConcurrentHashMap<?, ?>) a[0]).get(a[1]); },
                ConcurrentHashMap.class, "get", false, MethodType.methodType(Object.class, Object.class),
                () -> new Object[] {new ConcurrentHashMap<>(Map.of("k", 1)), null}, null);
        row("bytebuffer-direct-get-oob", R21CompatDispatchRows::d29, a -> { return ((ByteBuffer) a[0]).get((Integer) a[1]); },
                ByteBuffer.class, "get", false, MethodType.methodType(byte.class, int.class),
                () -> new Object[] {ByteBuffer.allocateDirect(4), 8}, null);
        row("bytebuffer-direct-put-get", R21CompatDispatchRows::d30, a -> { return ((ByteBuffer) a[0]).put((Integer) a[1], (Byte) a[2]); },
                ByteBuffer.class, "put", false, MethodType.methodType(ByteBuffer.class, int.class, byte.class),
                () -> new Object[] {ByteBuffer.allocateDirect(4), 1, (byte) -7}, r -> ((ByteBuffer) r).get(1));
        row("reference-reachabilityFence", R21CompatDispatchRows::d31, a -> { java.lang.ref.Reference.reachabilityFence(a[0]); return "void"; },
                java.lang.ref.Reference.class, "reachabilityFence", true, MethodType.methodType(void.class, Object.class),
                () -> new Object[] {"fence"}, null);
        row("object-getClass", R21CompatDispatchRows::d32, a -> { return a[0].getClass(); },
                Object.class, "getClass", false, MethodType.methodType(Class.class),
                () -> new Object[] {Integer.valueOf(3)}, r -> ((Class<?>) r).getName());
        System.out.println("done");
    }
}

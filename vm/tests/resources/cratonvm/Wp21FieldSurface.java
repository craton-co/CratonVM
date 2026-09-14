package cratonvm;

import java.lang.reflect.Field;
import java.lang.reflect.Modifier;

/**
 * WP2.1-field — end-to-end probes for {@code java.lang.reflect.Field}
 * with volatile-aware semantics, final-write check, and primitive
 * boxing/unboxing.
 *
 * <h2>What this fixture proves</h2>
 * <ol>
 *   <li>{@code Field.getInt(o)} / {@code Field.setInt(o,v)} round-trip
 *       on a primitive int field.</li>
 *   <li>{@code Field.getLong(o)} / {@code Field.setLong(o,v)} round-trip
 *       on a {@code volatile long} field — exercises the volatile-aware
 *       acquire/release fence path under
 *       {@code native-builtins/src/lang_class.rs::volatile_*_fence}.</li>
 *   <li>{@code Field.setInt(o,v)} on a non-static {@code final int}
 *       throws {@code IllegalAccessException} unless
 *       {@code setAccessible(true)} was called first; and a
 *       {@code static final} write throws even with
 *       {@code setAccessible(true)}.</li>
 *   <li>{@code Field.get(obj)} on an int field returns an
 *       {@code Integer} (boxing); {@code Field.set(obj, Integer)} on an
 *       int field unboxes.</li>
 *   <li>{@code Field.getName/getType/getModifiers/getDeclaringClass}
 *       round-trip.</li>
 *   <li>Reference + array fields round-trip through
 *       {@code Field.get/set}.</li>
 * </ol>
 *
 * Following the {@code WpN_M*} convention, each method returns a small
 * int (1 = pass, 0 = fail, negative = explicit failure mode) that the
 * Rust harness asserts on.
 */
public class Wp21FieldSurface {

    // Fields under test. Mirror the WP2.1 hot-list breadth: primitive,
    // reference, array, volatile, instance-final, static-final.

    /** Plain primitive instance field. */
    public int x;

    /** Volatile long — exercises Unsafe.getLong/putLongVolatile path. */
    public volatile long v;

    /** Plain (non-volatile) long — diagnostic differentiator for setLong. */
    public long plainLong;

    /** Volatile reference — exercises Unsafe.getReferenceVolatile. */
    public volatile Object refV;

    /** Plain reference (non-volatile). */
    public String s;

    /** Plain array reference. */
    public int[] arr;

    /** Instance final — write must require setAccessible(true). */
    public final int Y = 9;

    /** Static final — write must always fail via Field.set. */
    public static final int K = 42;

    // ---------------------------------------------------------------
    // Probes — one per acceptance bullet.
    // ---------------------------------------------------------------

    /**
     * Returns 1 iff Field.getInt and Field.setInt round-trip on a
     * primitive int field. Sentinels: 0 on any failure mode.
     */
    public static int intGetterSetterRoundTrips() {
        try {
            Wp21FieldSurface o = new Wp21FieldSurface();
            o.x = 5;
            Field f = Wp21FieldSurface.class.getDeclaredField("x");
            int got = f.getInt(o);
            if (got != 5) return 0;
            f.setInt(o, 7);
            if (f.getInt(o) != 7) return 0;
            if (o.x != 7) return 0;
            return 1;
        } catch (Throwable t) {
            return 0;
        }
    }

    /**
     * Returns 1 iff Field.getLong/setLong round-trip on a volatile
     * long field. Exercises the volatile-aware acquire/release fence
     * path; on a single-thread fixture we cannot assert ordering, but
     * we CAN pin the fact that the volatile-decorated field still
     * round-trips identically — and the Rust integration test asserts
     * the unsafe getLongVolatile native dispatched through.
     */
    /**
     * Diagnostic: same shape as volatileLongGetterSetterRoundTrips
     * but on a plain long — pinpoints whether the bug is in setLong
     * (will fail) or in the volatile-aware path (will pass).
     */
    public static int plainLongGetterSetterRoundTripsDiag() {
        Wp21FieldSurface o = new Wp21FieldSurface();
        o.plainLong = 100L;
        Field fp;
        try {
            fp = Wp21FieldSurface.class.getDeclaredField("plainLong");
        } catch (Throwable t) { return -10; }
        long got;
        try {
            got = fp.getLong(o);
        } catch (Throwable t) { return -12; }
        if (got != 100L) return -20;
        try {
            fp.setLong(o, 200L);
        } catch (Throwable t) { return -13; }
        if (o.plainLong != 200L) return -22;
        return 1;
    }

    public static int volatileLongGetterSetterRoundTrips() {
        // Step-by-step sentinels so a failing run pinpoints the broken native:
        //  -10  : Wp21FieldSurface.class.getDeclaredField("v") threw
        //  -11  : Field.getModifiers() threw
        //  -2   : ACC_VOLATILE bit missing on getModifiers
        //  -12  : Field.getLong(o) initial read threw
        //  0a   : initial getLong returned wrong value (-20 sentinel)
        //  -13  : Field.setLong(o, 200) threw
        //  -14  : second Field.getLong(o) threw
        //  0b   : second getLong returned wrong value (-21 sentinel)
        //  0c   : underlying field o.v didn't update (-22 sentinel)
        Wp21FieldSurface o = new Wp21FieldSurface();
        o.v = 100L;
        Field fv;
        try {
            fv = Wp21FieldSurface.class.getDeclaredField("v");
        } catch (Throwable t) {
            return -10;
        }
        int mods;
        try {
            mods = fv.getModifiers();
        } catch (Throwable t) {
            return -11;
        }
        if ((mods & Modifier.VOLATILE) == 0) return -2;
        long got;
        try {
            got = fv.getLong(o);
        } catch (Throwable t) {
            return -12;
        }
        if (got != 100L) return -20;
        try {
            fv.setLong(o, 200L);
        } catch (Throwable t) {
            return -13;
        }
        long got2;
        try {
            got2 = fv.getLong(o);
        } catch (Throwable t) {
            return -14;
        }
        if (got2 != 200L) return -21;
        if (o.v != 200L) return -22;
        return 1;
    }

    /**
     * Returns 1 iff:
     *   - Field.setInt on a non-static final field throws
     *     IllegalAccessException without setAccessible(true).
     *   - Same field with setAccessible(true) succeeds.
     *   - Field.set on a static final throws regardless.
     */
    public static int finalCheckEnforced() {
        try {
            Wp21FieldSurface o = new Wp21FieldSurface();

            // (a) Non-static final without setAccessible — must throw.
            Field ff = Wp21FieldSurface.class.getDeclaredField("Y");
            try {
                ff.setInt(o, 11);
                return -1; // didn't throw — bug.
            } catch (IllegalAccessException expected) {
                // good
            }

            // (b) Non-static final WITH setAccessible — must succeed.
            ff.setAccessible(true);
            ff.setInt(o, 11);
            if (ff.getInt(o) != 11) return -2;

            // (c) Static final without setAccessible — must throw.
            Field sk = Wp21FieldSurface.class.getDeclaredField("K");
            try {
                sk.setInt(null, 999);
                return -3;
            } catch (IllegalAccessException expected) {
                // good
            }

            // (d) Static final WITH setAccessible — must STILL throw
            // per Field.set Javadoc (only Unsafe / VarHandle escape).
            sk.setAccessible(true);
            try {
                sk.setInt(null, 999);
                return -4;
            } catch (IllegalAccessException expected) {
                // good
            }
            return 1;
        } catch (Throwable t) {
            return -5;
        }
    }

    /**
     * Returns 1 iff Field.get on int returns Integer; Field.set on int
     * accepts Integer (boxing/unboxing).
     */
    public static int boxingRoundTrips() {
        try {
            Wp21FieldSurface o = new Wp21FieldSurface();
            o.x = 3;
            Field f = Wp21FieldSurface.class.getDeclaredField("x");
            Object boxed = f.get(o);
            if (!(boxed instanceof Integer)) return -1;
            if (((Integer) boxed).intValue() != 3) return -2;
            f.set(o, Integer.valueOf(13));
            if (o.x != 13) return -3;
            // Mismatched-type set: passing a String to an int field
            // must throw IllegalArgumentException.
            try {
                f.set(o, "not an int");
                return -4;
            } catch (IllegalArgumentException expected) {
                // good
            }
            return 1;
        } catch (Throwable t) {
            return -5;
        }
    }

    /**
     * Returns 1 iff Field.getName/getType/getModifiers/getDeclaringClass
     * round-trip with the expected values for "x" (primitive int).
     */
    public static int metadataRoundTrips() {
        try {
            Field f = Wp21FieldSurface.class.getDeclaredField("x");
            if (!"x".equals(f.getName())) return -1;
            if (f.getType() != int.class) return -2;
            int mods = f.getModifiers();
            if ((mods & Modifier.PUBLIC) == 0) return -3;
            if ((mods & Modifier.STATIC) != 0) return -4;
            if ((mods & Modifier.VOLATILE) != 0) return -5;
            if (f.getDeclaringClass() != Wp21FieldSurface.class) return -6;

            // Volatile long: ACC_VOLATILE bit must be set on getModifiers.
            Field fv = Wp21FieldSurface.class.getDeclaredField("v");
            if (fv.getType() != long.class) return -7;
            if ((fv.getModifiers() & Modifier.VOLATILE) == 0) return -8;

            // Static final: ACC_STATIC | ACC_FINAL must be set.
            Field sk = Wp21FieldSurface.class.getDeclaredField("K");
            int kmods = sk.getModifiers();
            if ((kmods & Modifier.STATIC) == 0) return -9;
            if ((kmods & Modifier.FINAL) == 0) return -10;
            return 1;
        } catch (Throwable t) {
            return -11;
        }
    }

    /**
     * Returns 1 iff reference and array Field.get/set round-trip.
     */
    public static int referenceAndArrayRoundTrips() {
        try {
            Wp21FieldSurface o = new Wp21FieldSurface();
            Field fs = Wp21FieldSurface.class.getDeclaredField("s");
            fs.set(o, "hello");
            Object got = fs.get(o);
            if (!(got instanceof String)) return -1;
            if (!"hello".equals(got)) return -2;

            Field fa = Wp21FieldSurface.class.getDeclaredField("arr");
            int[] payload = new int[] { 10, 20, 30 };
            fa.set(o, payload);
            Object back = fa.get(o);
            if (!(back instanceof int[])) return -3;
            int[] arr = (int[]) back;
            if (arr.length != 3) return -4;
            if (arr[0] != 10 || arr[1] != 20 || arr[2] != 30) return -5;

            // Volatile reference round-trip.
            Field frv = Wp21FieldSurface.class.getDeclaredField("refV");
            String marker = "marker";
            frv.set(o, marker);
            if (frv.get(o) != marker) return -6;
            return 1;
        } catch (Throwable t) {
            return -7;
        }
    }

    /**
     * Composite probe: 1 iff every probe above passed.
     */
    public static int allFieldProbesPass() {
        if (intGetterSetterRoundTrips() != 1) return 0;
        if (volatileLongGetterSetterRoundTrips() != 1) return 0;
        if (finalCheckEnforced() != 1) return 0;
        if (boxingRoundTrips() != 1) return 0;
        if (metadataRoundTrips() != 1) return 0;
        if (referenceAndArrayRoundTrips() != 1) return 0;
        return 1;
    }
}

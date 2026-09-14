package cratonvm;

/**
 * Differential exercise program for the interpreter intrinsic table.
 *
 * Exercises EVERY method in the intrinsic set defined by
 * intrinsic_table_contract.md, with an edge-case input matrix:
 *   - Object.getClass / hashCode
 *   - String.length / charAt / isEmpty
 *   - System.arraycopy (normal, overlapping, and the 3 exception cases)
 *   - StringBuilder.append (6 descriptors) / toString / length
 *   - Integer.valueOf / intValue / parseInt (+ NumberFormatException)
 *   - Long.valueOf / longValue / parseLong (+ NumberFormatException)
 *   - Math.abs/min/max (int, long), Math.abs/sqrt (double)
 *
 * The program is fully deterministic: every observable result is folded into
 * a 64-bit FNV-1a hash and printed as the single final line
 *     INTRINSIC_DIFF_OK <hex-hash>
 * Because the intrinsic fast path must be byte-for-byte identical to the
 * normal native dispatch path, this final line MUST be character-identical
 * whether intrinsics are enabled or disabled (CRATONVM_DISABLE_INTRINSICS=1).
 *
 * Each individual result is also printed on its own line (prefixed "r:") so
 * that a divergence can be localised, not just detected.
 *
 * Plain Java 8 syntax only -- compiled by the legacy pass of vm/build.rs.
 */
public class IntrinsicDiff {

    // ---- deterministic FNV-1a 64-bit accumulator -----------------------
    private static long hash = 0xcbf29ce484222325L;

    private static void mix(long v) {
        // fold an 8-byte little-endian value into the hash
        for (int i = 0; i < 8; i++) {
            hash ^= (v & 0xffL);
            hash *= 0x100000001b3L;
            v >>>= 8;
        }
    }

    /** Record one named observation: print it and fold it into the hash. */
    private static void rec(String label, String value) {
        System.out.println("r:" + label + "=" + value);
        // fold the label and value text so order and content both matter
        for (int i = 0; i < label.length(); i++) mix(label.charAt(i));
        mix(0x7c); // '|' separator so "ab"+"c" != "a"+"bc"
        for (int i = 0; i < value.length(); i++) mix(value.charAt(i));
        mix(0x0a);
    }

    private static void rec(String label, long value) { rec(label, Long.toString(value)); }
    private static void rec(String label, boolean value) { rec(label, value ? "true" : "false"); }

    public static void main(String[] args) {
        objectIntrinsics();
        stringIntrinsics();
        systemArraycopyIntrinsics();
        stringBuilderIntrinsics();
        integerIntrinsics();
        longIntrinsics();
        mathIntrinsics();

        // Final deterministic line. Hex, lower-case, zero-padded to 16 chars.
        String h = Long.toHexString(hash);
        StringBuilder pad = new StringBuilder();
        for (int i = h.length(); i < 16; i++) pad.append('0');
        pad.append(h);
        System.out.println("INTRINSIC_DIFF_OK " + pad.toString());
    }

    // -------------------------------------------------------------------
    // java/lang/Object  -- getClass, hashCode
    // -------------------------------------------------------------------
    private static void objectIntrinsics() {
        Object o = new Object();
        // getClass() identity must be stable & correct.
        Class<?> c = o.getClass();
        rec("Object.getClass.name", c.getName());
        rec("Object.getClass.stable", o.getClass() == o.getClass());
        // getClass on a String and an array.
        rec("String.getClass.name", "abc".getClass().getName());
        rec("intarr.getClass.name", new int[0].getClass().getName());
        // hashCode() on java.lang.Object: identity hash. Its absolute value
        // is non-deterministic, so we DON'T fold it -- we only fold the
        // *consistency* contract (same object -> same hashCode).
        int h1 = o.hashCode();
        int h2 = o.hashCode();
        rec("Object.hashCode.consistent", h1 == h2);
        // System.identityHashCode must agree with Object.hashCode for a
        // class that does not override hashCode.
        rec("Object.hashCode.eq.identity", h1 == System.identityHashCode(o));
    }

    // -------------------------------------------------------------------
    // java/lang/String  -- length, charAt, isEmpty
    // -------------------------------------------------------------------
    private static void stringIntrinsics() {
        String empty = "";
        String s = "Craton";          // length 6
        // 'e-acute' (U+00E9) + a CJK char (U+4E2D). Written as backslash-u escapes,
        // the source stays pure-ASCII and compiles deterministically under
        // any platform default javac encoding (vm/build.rs passes no
        // -encoding flag on the fallback pass).
        String unicode = "\u00e9\u4e2d";

        rec("String.length.empty", empty.length());
        rec("String.length.s", s.length());
        rec("String.length.unicode", unicode.length());

        rec("String.isEmpty.empty", empty.isEmpty());
        rec("String.isEmpty.s", s.isEmpty());

        // charAt at index 0 and at the last index.
        rec("String.charAt.0", (long) s.charAt(0));
        rec("String.charAt.last", (long) s.charAt(s.length() - 1));
        rec("String.charAt.unicode0", (long) unicode.charAt(0));

        // charAt out-of-bounds must throw StringIndexOutOfBoundsException.
        try {
            char ignored = s.charAt(s.length()); // one past the end
            rec("String.charAt.oob.high", "NO-THROW:" + ignored);
        } catch (StringIndexOutOfBoundsException e) {
            rec("String.charAt.oob.high", "SIOOBE");
        }
        try {
            char ignored = s.charAt(-1);
            rec("String.charAt.oob.neg", "NO-THROW:" + ignored);
        } catch (StringIndexOutOfBoundsException e) {
            rec("String.charAt.oob.neg", "SIOOBE");
        }
        // charAt on an empty string at index 0.
        try {
            char ignored = empty.charAt(0);
            rec("String.charAt.empty0", "NO-THROW:" + ignored);
        } catch (StringIndexOutOfBoundsException e) {
            rec("String.charAt.empty0", "SIOOBE");
        }
    }

    // -------------------------------------------------------------------
    // java/lang/System.arraycopy -- normal, overlapping, NPE,
    //                               ArrayStoreException, AIOOBE
    // -------------------------------------------------------------------
    private static void systemArraycopyIntrinsics() {
        // Normal full copy.
        int[] src = { 10, 20, 30, 40, 50 };
        int[] dst = new int[5];
        System.arraycopy(src, 0, dst, 0, 5);
        rec("arraycopy.normal", java.util.Arrays.toString(dst));

        // Partial copy with offsets.
        int[] dst2 = { -1, -1, -1, -1, -1 };
        System.arraycopy(src, 1, dst2, 2, 3);
        rec("arraycopy.partial", java.util.Arrays.toString(dst2));

        // Overlapping copy within the SAME array, forward direction.
        int[] ov = { 1, 2, 3, 4, 5, 6 };
        System.arraycopy(ov, 0, ov, 2, 4);
        rec("arraycopy.overlap.fwd", java.util.Arrays.toString(ov));

        // Overlapping copy within the same array, backward direction.
        int[] ov2 = { 1, 2, 3, 4, 5, 6 };
        System.arraycopy(ov2, 2, ov2, 0, 4);
        rec("arraycopy.overlap.bwd", java.util.Arrays.toString(ov2));

        // Zero-length copy is a no-op.
        int[] z = { 7, 8, 9 };
        System.arraycopy(z, 1, z, 0, 0);
        rec("arraycopy.zero", java.util.Arrays.toString(z));

        // Object[] copy (reference array).
        String[] os = { "a", "b", "c" };
        String[] od = new String[3];
        System.arraycopy(os, 0, od, 0, 3);
        rec("arraycopy.objarr", java.util.Arrays.toString(od));

        // --- exception case 1: NullPointerException (null src) ----------
        try {
            System.arraycopy(null, 0, dst, 0, 1);
            rec("arraycopy.npe.src", "NO-THROW");
        } catch (NullPointerException e) {
            rec("arraycopy.npe.src", "NPE");
        }
        // NullPointerException (null dst).
        try {
            System.arraycopy(src, 0, null, 0, 1);
            rec("arraycopy.npe.dst", "NO-THROW");
        } catch (NullPointerException e) {
            rec("arraycopy.npe.dst", "NPE");
        }

        // --- exception case 2: ArrayStoreException ----------------------
        // Copy an Object[] holding a non-String into a String[].
        Object[] mixed = { "ok", Integer.valueOf(7) };
        String[] strDst = new String[2];
        try {
            System.arraycopy(mixed, 0, strDst, 0, 2);
            rec("arraycopy.ase", "NO-THROW");
        } catch (ArrayStoreException e) {
            rec("arraycopy.ase", "ASE");
        }
        // Incompatible primitive vs reference array types also -> ASE.
        try {
            System.arraycopy(src, 0, od, 0, 1); // int[] -> String[]
            rec("arraycopy.ase.prim", "NO-THROW");
        } catch (ArrayStoreException e) {
            rec("arraycopy.ase.prim", "ASE");
        }

        // --- exception case 3: ArrayIndexOutOfBoundsException -----------
        try {
            System.arraycopy(src, 3, dst, 0, 5); // reads past src end
            rec("arraycopy.aioobe.src", "NO-THROW");
        } catch (ArrayIndexOutOfBoundsException e) {
            rec("arraycopy.aioobe.src", "AIOOBE");
        }
        try {
            System.arraycopy(src, 0, dst, 3, 5); // writes past dst end
            rec("arraycopy.aioobe.dst", "NO-THROW");
        } catch (ArrayIndexOutOfBoundsException e) {
            rec("arraycopy.aioobe.dst", "AIOOBE");
        }
        try {
            System.arraycopy(src, -1, dst, 0, 1); // negative srcPos
            rec("arraycopy.aioobe.neg", "NO-THROW");
        } catch (ArrayIndexOutOfBoundsException e) {
            rec("arraycopy.aioobe.neg", "AIOOBE");
        }
        try {
            System.arraycopy(src, 0, dst, 0, -1); // negative length
            rec("arraycopy.aioobe.neglen", "NO-THROW");
        } catch (ArrayIndexOutOfBoundsException e) {
            rec("arraycopy.aioobe.neglen", "AIOOBE");
        }
    }

    // -------------------------------------------------------------------
    // java/lang/StringBuilder -- append x6 descriptors, toString, length
    // -------------------------------------------------------------------
    private static void stringBuilderIntrinsics() {
        // append(String) and append(Object) and chains.
        StringBuilder sb = new StringBuilder();
        sb.append("hello");                       // append(String)
        sb.append((String) null);                 // append(String) null -> "null"
        sb.append(42);                            // append(int)
        sb.append(-7);                            // append(int) negative
        sb.append('Z');                           // append(char)
        sb.append(9876543210L);                   // append(long)
        sb.append(-1L);                           // append(long) negative
        sb.append(true);                          // append(boolean)
        sb.append(false);                         // append(boolean)
        Object obj = java.util.Arrays.asList(1, 2);
        sb.append(obj);                           // append(Object)
        sb.append((Object) null);                 // append(Object) null -> "null"

        rec("SB.length", sb.length());
        rec("SB.toString", sb.toString());

        // append(int) boundary values.
        rec("SB.append.intmax", new StringBuilder().append(Integer.MAX_VALUE).toString());
        rec("SB.append.intmin", new StringBuilder().append(Integer.MIN_VALUE).toString());
        // append(long) boundary values.
        rec("SB.append.longmax", new StringBuilder().append(Long.MAX_VALUE).toString());
        rec("SB.append.longmin", new StringBuilder().append(Long.MIN_VALUE).toString());

        // Fluent chain returning the same builder each time.
        String chain = new StringBuilder()
                .append("a").append(1).append('b').append(2L).append(true)
                .toString();
        rec("SB.chain", chain);

        // length of an empty builder, and after append.
        StringBuilder e = new StringBuilder();
        rec("SB.length.empty", e.length());
        e.append("xyz");
        rec("SB.length.after", e.length());

        // toString of an empty builder.
        rec("SB.toString.empty", new StringBuilder().toString());
    }

    // -------------------------------------------------------------------
    // java/lang/Integer -- valueOf, intValue, parseInt
    // -------------------------------------------------------------------
    private static void integerIntrinsics() {
        // valueOf across the small-cache range and outside it.
        Integer a = Integer.valueOf(0);
        Integer b = Integer.valueOf(127);     // cached
        Integer c = Integer.valueOf(128);     // not cached
        Integer d = Integer.valueOf(-128);    // cached
        Integer e = Integer.valueOf(Integer.MAX_VALUE);
        Integer f = Integer.valueOf(Integer.MIN_VALUE);
        rec("Integer.valueOf.0", a.intValue());
        rec("Integer.valueOf.127", b.intValue());
        rec("Integer.valueOf.128", c.intValue());
        rec("Integer.valueOf.-128", d.intValue());
        rec("Integer.valueOf.max", e.intValue());
        rec("Integer.valueOf.min", f.intValue());
        // The JLS-mandated identity of the cached range [-128,127].
        rec("Integer.cache.127", Integer.valueOf(127) == Integer.valueOf(127));

        // intValue round-trips.
        rec("Integer.intValue.boxed", Integer.valueOf(314159).intValue());

        // parseInt -- valid forms.
        rec("Integer.parseInt.pos", Integer.parseInt("12345"));
        rec("Integer.parseInt.neg", Integer.parseInt("-9999"));
        rec("Integer.parseInt.plus", Integer.parseInt("+7"));
        rec("Integer.parseInt.zero", Integer.parseInt("0"));
        rec("Integer.parseInt.max", Integer.parseInt("2147483647"));
        rec("Integer.parseInt.min", Integer.parseInt("-2147483648"));

        // parseInt -- NumberFormatException cases.
        try {
            int v = Integer.parseInt("not-a-number");
            rec("Integer.parseInt.bad", "NO-THROW:" + v);
        } catch (NumberFormatException ex) {
            rec("Integer.parseInt.bad", "NFE");
        }
        try {
            int v = Integer.parseInt("");
            rec("Integer.parseInt.empty", "NO-THROW:" + v);
        } catch (NumberFormatException ex) {
            rec("Integer.parseInt.empty", "NFE");
        }
        try {
            int v = Integer.parseInt("2147483648"); // overflow
            rec("Integer.parseInt.overflow", "NO-THROW:" + v);
        } catch (NumberFormatException ex) {
            rec("Integer.parseInt.overflow", "NFE");
        }
    }

    // -------------------------------------------------------------------
    // java/lang/Long -- valueOf, longValue, parseLong
    // -------------------------------------------------------------------
    private static void longIntrinsics() {
        Long a = Long.valueOf(0L);
        Long b = Long.valueOf(127L);          // cached
        Long c = Long.valueOf(128L);          // not cached
        Long d = Long.valueOf(Long.MAX_VALUE);
        Long e = Long.valueOf(Long.MIN_VALUE);
        rec("Long.valueOf.0", a.longValue());
        rec("Long.valueOf.127", b.longValue());
        rec("Long.valueOf.128", c.longValue());
        rec("Long.valueOf.max", d.longValue());
        rec("Long.valueOf.min", e.longValue());
        rec("Long.cache.127", Long.valueOf(127L) == Long.valueOf(127L));

        rec("Long.longValue.boxed", Long.valueOf(9223372036854775807L).longValue());

        rec("Long.parseLong.pos", Long.parseLong("9000000000000"));
        rec("Long.parseLong.neg", Long.parseLong("-9000000000000"));
        rec("Long.parseLong.zero", Long.parseLong("0"));
        rec("Long.parseLong.max", Long.parseLong("9223372036854775807"));
        rec("Long.parseLong.min", Long.parseLong("-9223372036854775808"));

        try {
            long v = Long.parseLong("garbage");
            rec("Long.parseLong.bad", "NO-THROW:" + v);
        } catch (NumberFormatException ex) {
            rec("Long.parseLong.bad", "NFE");
        }
        try {
            long v = Long.parseLong("");
            rec("Long.parseLong.empty", "NO-THROW:" + v);
        } catch (NumberFormatException ex) {
            rec("Long.parseLong.empty", "NFE");
        }
        try {
            long v = Long.parseLong("9223372036854775808"); // overflow
            rec("Long.parseLong.overflow", "NO-THROW:" + v);
        } catch (NumberFormatException ex) {
            rec("Long.parseLong.overflow", "NFE");
        }
    }

    // -------------------------------------------------------------------
    // java/lang/Math -- abs/min/max (int,long), abs/sqrt (double)
    // -------------------------------------------------------------------
    private static void mathIntrinsics() {
        // Math.abs(int) -- including the Integer.MIN_VALUE pathology, where
        // abs(MIN_VALUE) == MIN_VALUE (overflow, by spec).
        rec("Math.abs.int.pos", Math.abs(123));
        rec("Math.abs.int.neg", Math.abs(-123));
        rec("Math.abs.int.zero", Math.abs(0));
        rec("Math.abs.int.min", Math.abs(Integer.MIN_VALUE));
        rec("Math.abs.int.max", Math.abs(Integer.MAX_VALUE));

        // Math.abs(long) -- including Long.MIN_VALUE pathology.
        rec("Math.abs.long.pos", Math.abs(123456789012L));
        rec("Math.abs.long.neg", Math.abs(-123456789012L));
        rec("Math.abs.long.min", Math.abs(Long.MIN_VALUE));
        rec("Math.abs.long.max", Math.abs(Long.MAX_VALUE));

        // Math.min/max(int,int).
        rec("Math.min.int", Math.min(5, -3));
        rec("Math.max.int", Math.max(5, -3));
        rec("Math.min.int.eq", Math.min(7, 7));
        rec("Math.max.int.eq", Math.max(7, 7));
        rec("Math.min.int.extremes", Math.min(Integer.MIN_VALUE, Integer.MAX_VALUE));
        rec("Math.max.int.extremes", Math.max(Integer.MIN_VALUE, Integer.MAX_VALUE));

        // Math.min/max(long,long).
        rec("Math.min.long", Math.min(5_000_000_000L, -3_000_000_000L));
        rec("Math.max.long", Math.max(5_000_000_000L, -3_000_000_000L));
        rec("Math.min.long.extremes", Math.min(Long.MIN_VALUE, Long.MAX_VALUE));
        rec("Math.max.long.extremes", Math.max(Long.MIN_VALUE, Long.MAX_VALUE));

        // Math.abs(double) -- positive, negative, zero, -0.0, NaN, infinities.
        rec("Math.abs.double.pos", Double.toString(Math.abs(3.5)));
        rec("Math.abs.double.neg", Double.toString(Math.abs(-3.5)));
        rec("Math.abs.double.zero", Double.toString(Math.abs(0.0)));
        // abs(-0.0) must be +0.0 -- fold the raw bits to catch a sign-bit bug.
        rec("Math.abs.double.negzero.bits",
                Long.toString(Double.doubleToRawLongBits(Math.abs(-0.0))));
        rec("Math.abs.double.nan", Double.toString(Math.abs(Double.NaN)));
        rec("Math.abs.double.posinf", Double.toString(Math.abs(Double.POSITIVE_INFINITY)));
        rec("Math.abs.double.neginf", Double.toString(Math.abs(Double.NEGATIVE_INFINITY)));

        // Math.sqrt(double) -- exact squares, irrational, edge cases.
        rec("Math.sqrt.4", Double.toString(Math.sqrt(4.0)));
        rec("Math.sqrt.2", Double.toString(Math.sqrt(2.0)));
        rec("Math.sqrt.0", Double.toString(Math.sqrt(0.0)));
        rec("Math.sqrt.negzero.bits",
                Long.toString(Double.doubleToRawLongBits(Math.sqrt(-0.0))));
        rec("Math.sqrt.neg", Double.toString(Math.sqrt(-1.0)));   // -> NaN
        rec("Math.sqrt.nan", Double.toString(Math.sqrt(Double.NaN)));
        rec("Math.sqrt.posinf", Double.toString(Math.sqrt(Double.POSITIVE_INFINITY)));
        rec("Math.sqrt.big", Double.toString(Math.sqrt(1.0e300)));
    }
}

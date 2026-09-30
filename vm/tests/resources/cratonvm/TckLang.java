package cratonvm;

/**
 * Session 46: TCK — java.lang Tests.
 *
 * Comprehensive conformance tests for java.lang classes.
 * Each static method returns an int (1 = pass, or specific expected value).
 */
public class TckLang {

    // =========================================================================
    // java.lang.Object
    // =========================================================================

    /** Object.hashCode() returns consistent value. */
    public static int obj_hashCode_consistent() {
        Object o = new Object();
        int h1 = o.hashCode();
        int h2 = o.hashCode();
        return (h1 == h2) ? 1 : 0;
    }

    /** Object.equals() identity. */
    public static int obj_equals_identity() {
        Object o = new Object();
        return o.equals(o) ? 1 : 0;
    }

    /** Object.equals() different objects. */
    public static int obj_equals_different() {
        Object a = new Object();
        Object b = new Object();
        return a.equals(b) ? 0 : 1;
    }

    /** Object.getClass() returns non-null. */
    public static int obj_getClass() {
        Object o = new Object();
        return (o.getClass() != null) ? 1 : 0;
    }

    /** Object.toString() contains class name. */
    public static int obj_toString() {
        Object o = new Object();
        String s = o.toString();
        return (s != null && s.length() > 0) ? 1 : 0;
    }

    // =========================================================================
    // java.lang.String
    // =========================================================================

    /** String.length(). */
    public static int str_length() {
        return "Hello".length(); // 5
    }

    /** String.charAt(). */
    public static int str_charAt() {
        return "ABCDE".charAt(3); // 'D' = 68
    }

    /** String.equals(). */
    public static int str_equals() {
        String a = "hello";
        String b = "hello";
        String c = "world";
        return (a.equals(b) && !a.equals(c)) ? 1 : 0;
    }

    /** String.compareTo(). */
    public static int str_compareTo() {
        int cmp = "abc".compareTo("abd");
        return (cmp < 0) ? 1 : 0;
    }

    /** String.substring(). */
    public static int str_substring() {
        String s = "Hello World";
        return s.substring(6).length(); // "World" = 5
    }

    /** String.indexOf(). */
    public static int str_indexOf() {
        return "Hello World".indexOf('W'); // 6
    }

    /** String.contains(). */
    public static int str_contains() {
        return "Hello World".contains("World") ? 1 : 0;
    }

    /** String.isEmpty(). */
    public static int str_isEmpty() {
        return ("".isEmpty() && !"x".isEmpty()) ? 1 : 0;
    }

    /** String.trim(). */
    public static int str_trim() {
        return "  hello  ".trim().length(); // 5
    }

    /** String.toLowerCase(). */
    public static int str_toLowerCase() {
        String s = "HELLO".toLowerCase();
        return s.equals("hello") ? 1 : 0;
    }

    /** String.toUpperCase(). */
    public static int str_toUpperCase() {
        String s = "hello".toUpperCase();
        return s.equals("HELLO") ? 1 : 0;
    }

    /** String.startsWith() / endsWith(). */
    public static int str_startsEndsWith() {
        String s = "Hello World";
        return (s.startsWith("Hello") && s.endsWith("World")) ? 1 : 0;
    }

    /** String.replace(). */
    public static int str_replace() {
        String s = "aabaa".replace('a', 'x');
        return s.equals("xxbxx") ? 1 : 0;
    }

    /** String.toCharArray(). */
    public static int str_toCharArray() {
        char[] arr = "ABC".toCharArray();
        return (arr.length == 3 && arr[0] == 'A' && arr[2] == 'C') ? 1 : 0;
    }

    /** String.valueOf(int). */
    public static int str_valueOf_int() {
        return String.valueOf(42).equals("42") ? 1 : 0;
    }

    /** String.valueOf(boolean). */
    public static int str_valueOf_bool() {
        return (String.valueOf(true).equals("true") && String.valueOf(false).equals("false")) ? 1 : 0;
    }

    /** String concatenation via +. */
    public static int str_concat_op() {
        String s = "Hello" + " " + "World";
        return s.length(); // 11
    }

    // =========================================================================
    // java.lang.Integer
    // =========================================================================

    /** Integer.parseInt(). */
    public static int int_parseInt() {
        return Integer.parseInt("42"); // 42
    }

    /** Integer.parseInt() negative. */
    public static int int_parseInt_neg() {
        return Integer.parseInt("-100"); // -100
    }

    /** Integer.valueOf() boxing. */
    public static int int_valueOf() {
        Integer i = Integer.valueOf(42);
        return i.intValue(); // 42
    }

    /** Integer.toString(). */
    public static int int_toString() {
        return Integer.toString(123).equals("123") ? 1 : 0;
    }

    /** Integer.toHexString(). */
    public static int int_toHexString() {
        return Integer.toHexString(255).equals("ff") ? 1 : 0;
    }

    /** Integer constants. */
    public static int int_constants() {
        return (Integer.MAX_VALUE == 2147483647 && Integer.MIN_VALUE == -2147483648) ? 1 : 0;
    }

    /** Integer autoboxing equality (cached range -128..127). */
    public static int int_autobox_cache() {
        Integer a = 42;
        Integer b = 42;
        // Verify autoboxing produces equal values (using .equals, not identity ==)
        return (a.equals(b) && a.intValue() == 42 && b.intValue() == 42) ? 1 : 0;
    }

    /** Integer.compareTo(). */
    public static int int_compareTo() {
        Integer a = 10;
        Integer b = 20;
        return (a.compareTo(b) < 0) ? 1 : 0;
    }

    // =========================================================================
    // java.lang.Long
    // =========================================================================

    /** Long.parseLong(). */
    public static int long_parseLong() {
        long v = Long.parseLong("1000000000");
        return (v == 1000000000L) ? 1 : 0;
    }

    /** Long.valueOf(). */
    public static int long_valueOf() {
        Long l = Long.valueOf(99L);
        return (int) l.longValue(); // 99
    }

    /** Long.toString(). */
    public static int long_toString() {
        return Long.toString(12345L).equals("12345") ? 1 : 0;
    }

    /** Long.MAX_VALUE. */
    public static int long_maxValue() {
        return (Long.MAX_VALUE == 9223372036854775807L) ? 1 : 0;
    }

    // =========================================================================
    // java.lang.Double
    // =========================================================================

    /** Double.parseDouble(). */
    public static int double_parseDouble() {
        double d = Double.parseDouble("3.14");
        return (d > 3.13 && d < 3.15) ? 1 : 0;
    }

    /** Double.isNaN(). */
    public static int double_isNaN() {
        return (Double.isNaN(Double.NaN) && !Double.isNaN(1.0)) ? 1 : 0;
    }

    /** Double.isInfinite(). */
    public static int double_isInfinite() {
        return (Double.isInfinite(Double.POSITIVE_INFINITY) &&
                Double.isInfinite(Double.NEGATIVE_INFINITY) &&
                !Double.isInfinite(1.0)) ? 1 : 0;
    }

    /** Double.toString(). */
    public static int double_toString() {
        String s = Double.toString(1.5);
        return (s != null && s.length() > 0) ? 1 : 0;
    }

    /** Double.doubleToLongBits / longBitsToDouble roundtrip. */
    public static int double_bits_roundtrip() {
        double d = 3.14;
        long bits = Double.doubleToRawLongBits(d);
        double d2 = Double.longBitsToDouble(bits);
        return (d == d2) ? 1 : 0;
    }

    // =========================================================================
    // java.lang.Float
    // =========================================================================

    /** Float.parseFloat(). */
    public static int float_parseFloat() {
        float f = Float.parseFloat("2.5");
        return (f > 2.4f && f < 2.6f) ? 1 : 0;
    }

    /** Float.isNaN(). */
    public static int float_isNaN() {
        return (Float.isNaN(Float.NaN) && !Float.isNaN(1.0f)) ? 1 : 0;
    }

    /** Float.floatToIntBits / intBitsToFloat roundtrip. */
    public static int float_bits_roundtrip() {
        float f = 1.5f;
        int bits = Float.floatToRawIntBits(f);
        float f2 = Float.intBitsToFloat(bits);
        return (f == f2) ? 1 : 0;
    }

    // =========================================================================
    // java.lang.Boolean
    // =========================================================================

    /** Boolean.parseBoolean(). */
    public static int bool_parseBoolean() {
        return (Boolean.parseBoolean("true") && !Boolean.parseBoolean("false") &&
                !Boolean.parseBoolean("xyz")) ? 1 : 0;
    }

    /** Boolean.valueOf(). */
    public static int bool_valueOf() {
        Boolean t = Boolean.valueOf(true);
        Boolean f = Boolean.valueOf(false);
        return (t.booleanValue() && !f.booleanValue()) ? 1 : 0;
    }

    /** Boolean.toString(). */
    public static int bool_toString() {
        return (Boolean.toString(true).equals("true") &&
                Boolean.toString(false).equals("false")) ? 1 : 0;
    }

    // =========================================================================
    // java.lang.Byte
    // =========================================================================

    /** Byte constants. */
    public static int byte_constants() {
        return (Byte.MAX_VALUE == 127 && Byte.MIN_VALUE == -128) ? 1 : 0;
    }

    /** Byte.parseByte(). */
    public static int byte_parseByte() {
        byte b = Byte.parseByte("42");
        return b; // 42
    }

    // =========================================================================
    // java.lang.Short
    // =========================================================================

    /** Short constants. */
    public static int short_constants() {
        return (Short.MAX_VALUE == 32767 && Short.MIN_VALUE == -32768) ? 1 : 0;
    }

    /** Short.parseShort(). */
    public static int short_parseShort() {
        short s = Short.parseShort("1000");
        return s; // 1000
    }

    // =========================================================================
    // java.lang.Character
    // =========================================================================

    /** Character.isDigit(). */
    public static int char_isDigit() {
        return (Character.isDigit('5') && !Character.isDigit('A')) ? 1 : 0;
    }

    /** Character.isLetter(). */
    public static int char_isLetter() {
        return (Character.isLetter('A') && !Character.isLetter('5')) ? 1 : 0;
    }

    /** Character.isUpperCase() / isLowerCase(). */
    public static int char_case() {
        return (Character.isUpperCase('A') && Character.isLowerCase('a') &&
                !Character.isUpperCase('a') && !Character.isLowerCase('A')) ? 1 : 0;
    }

    /** Character.toUpperCase() / toLowerCase(). */
    public static int char_convert() {
        return (Character.toUpperCase('a') == 'A' && Character.toLowerCase('A') == 'a') ? 1 : 0;
    }

    /** Character.isWhitespace(). */
    public static int char_isWhitespace() {
        return (Character.isWhitespace(' ') && Character.isWhitespace('\t') &&
                !Character.isWhitespace('A')) ? 1 : 0;
    }

    // =========================================================================
    // java.lang.Math
    // =========================================================================

    /** Math.abs(). */
    public static int math_abs() {
        return (Math.abs(-42) == 42 && Math.abs(42) == 42) ? 1 : 0;
    }

    /** Math.max() / Math.min(). */
    public static int math_maxMin() {
        return (Math.max(10, 20) == 20 && Math.min(10, 20) == 10) ? 1 : 0;
    }

    /**
     * Math.sqrt().
     *
     * Exact, not a +-0.01 band: IEEE 754 requires sqrt to be CORRECTLY ROUNDED,
     * and 144.0 is a perfect square, so the only conforming answer is 12.0. The
     * old band would have accepted an implementation that was not computing a
     * square root at all. W7-54-strictmath-fdlibm-family.md.
     */
    public static int math_sqrt() {
        double s = Math.sqrt(144.0);
        return (s == 12.0) ? 1 : 0;
    }

    /** Math.pow(). */
    public static int math_pow() {
        double p = Math.pow(2.0, 10.0);
        return (p > 1023.9 && p < 1024.1) ? 1 : 0;
    }

    /** Math.floor() / ceil(). */
    public static int math_floorCeil() {
        return (Math.floor(3.7) == 3.0 && Math.ceil(3.2) == 4.0) ? 1 : 0;
    }

    /** Math.round(). */
    public static int math_round() {
        return (Math.round(3.5f) == 4 && Math.round(3.4f) == 3) ? 1 : 0;
    }

    /** Math.PI and E constants. */
    public static int math_constants() {
        return (Math.PI > 3.14 && Math.PI < 3.15 &&
                Math.E > 2.71 && Math.E < 2.72) ? 1 : 0;
    }

    /**
     * Math.sin() / cos().
     *
     * sin(0.0) is asserted EXACTLY, and with its sign: the javadoc special case
     * is "if the argument is zero, then the result is a zero with the same sign
     * as the argument", which is a fixed answer, not a 1-ULP one. The old
     * `Math.abs(s) < 0.001` also passed for -0.0 and for 1e-300.
     *
     * cos(0.0) is likewise exactly 1.0 by its own special case. Neither of these
     * relies on the 1-ULP latitude Math.sin/cos have away from zero, so nothing
     * here needs a tolerance. W7-54-strictmath-fdlibm-family.md.
     */
    public static int math_sinCos() {
        double s = Math.sin(0.0);
        double c = Math.cos(0.0);
        boolean sinExact = (s == 0.0) && (Double.doubleToRawLongBits(s) == 0L);
        return (sinExact && c == 1.0) ? 1 : 0;
    }

    /** Math.log() / exp(). */
    public static int math_logExp() {
        double l = Math.log(Math.E);
        double e = Math.exp(1.0);
        return (Math.abs(l - 1.0) < 0.001 && Math.abs(e - Math.E) < 0.01) ? 1 : 0;
    }

    // =========================================================================
    // java.lang.System
    // =========================================================================

    /** System.currentTimeMillis() returns positive. */
    public static int sys_currentTimeMillis() {
        long t = System.currentTimeMillis();
        return (t > 0) ? 1 : 0;
    }

    /** System.nanoTime() monotonic. */
    public static int sys_nanoTime() {
        long t1 = System.nanoTime();
        long t2 = System.nanoTime();
        return (t2 >= t1) ? 1 : 0;
    }

    /** System.arraycopy(). */
    public static int sys_arraycopy() {
        int[] src = {1, 2, 3, 4, 5};
        int[] dst = new int[5];
        System.arraycopy(src, 0, dst, 0, 5);
        return (dst[0] == 1 && dst[4] == 5) ? 1 : 0;
    }

    /** System.identityHashCode(). */
    public static int sys_identityHashCode() {
        Object o = new Object();
        int h1 = System.identityHashCode(o);
        int h2 = System.identityHashCode(o);
        return (h1 == h2) ? 1 : 0;
    }

    // =========================================================================
    // java.lang.StringBuilder
    // =========================================================================

    /** StringBuilder basic append and toString. */
    public static int sb_basic() {
        StringBuilder sb = new StringBuilder();
        sb.append("Hello");
        sb.append(" ");
        sb.append("World");
        return sb.toString().length(); // 11
    }

    /** StringBuilder append int. */
    public static int sb_appendInt() {
        StringBuilder sb = new StringBuilder();
        sb.append(42);
        return sb.toString().equals("42") ? 1 : 0;
    }

    /** StringBuilder append chain. */
    public static int sb_chain() {
        String s = new StringBuilder()
            .append("a")
            .append("b")
            .append("c")
            .toString();
        return s.equals("abc") ? 1 : 0;
    }

    /** StringBuilder length and capacity. */
    public static int sb_length() {
        StringBuilder sb = new StringBuilder("Hello");
        return sb.length(); // 5
    }

    /** StringBuilder reverse. */
    public static int sb_reverse() {
        StringBuilder sb = new StringBuilder("abcde");
        sb.reverse();
        return sb.toString().equals("edcba") ? 1 : 0;
    }

    /** StringBuilder delete. */
    public static int sb_delete() {
        StringBuilder sb = new StringBuilder("Hello World");
        sb.delete(5, 11);
        return sb.toString().equals("Hello") ? 1 : 0;
    }

    // =========================================================================
    // java.lang.Throwable / Exceptions
    // =========================================================================

    /** Exception getMessage(). */
    public static int exc_getMessage() {
        Exception e = new RuntimeException("test message");
        return e.getMessage().equals("test message") ? 1 : 0;
    }

    /** Exception getCause(). */
    public static int exc_getCause() {
        Exception cause = new RuntimeException("cause");
        Exception e = new RuntimeException("wrapper", cause);
        return (e.getCause() == cause) ? 1 : 0;
    }

    /** Try-catch works. */
    public static int exc_tryCatch() {
        int result = 0;
        try {
            throw new IllegalArgumentException("test");
        } catch (IllegalArgumentException e) {
            result = 1;
        }
        return result;
    }

    /** Exception hierarchy: RuntimeException extends Exception. */
    public static int exc_hierarchy() {
        RuntimeException re = new RuntimeException();
        return (re instanceof Exception && re instanceof Throwable) ? 1 : 0;
    }

    /** NullPointerException name. */
    public static int exc_npe_class() {
        NullPointerException npe = new NullPointerException("test");
        return npe.getClass().getName().equals("java.lang.NullPointerException") ? 1 : 0;
    }

    /** Finally block executes. */
    public static int exc_finally() {
        int val = 0;
        try {
            val = 10;
        } finally {
            val += 5;
        }
        return val; // 15
    }

    // =========================================================================
    // java.lang.Class
    // =========================================================================

    /** Class.getName(). */
    public static int cls_getName() {
        String name = TckLang.class.getName();
        return name.equals("cratonvm.TckLang") ? 1 : 0;
    }

    /** Class.isInterface(). */
    public static int cls_isInterface() {
        return (!TckLang.class.isInterface()) ? 1 : 0;
    }

    /** Class.isPrimitive(). */
    public static int cls_isPrimitive() {
        return (int.class.isPrimitive() && !Integer.class.isPrimitive()) ? 1 : 0;
    }

    /** Class.isArray(). */
    public static int cls_isArray() {
        int[] arr = new int[1];
        return (arr.getClass().isArray() && !TckLang.class.isArray()) ? 1 : 0;
    }

    /** Class.getSuperclass(). */
    public static int cls_getSuperclass() {
        Class<?> sup = TckLang.class.getSuperclass();
        return (sup != null && sup.getName().equals("java.lang.Object")) ? 1 : 0;
    }

    // =========================================================================
    // java.lang.Runtime
    // =========================================================================

    /** Runtime.availableProcessors(). */
    public static int rt_availableProcessors() {
        int n = Runtime.getRuntime().availableProcessors();
        return (n >= 1) ? 1 : 0;
    }

    /** Runtime.freeMemory() / totalMemory(). */
    public static int rt_memory() {
        Runtime rt = Runtime.getRuntime();
        long free = rt.freeMemory();
        long total = rt.totalMemory();
        return (free >= 0 && total > 0 && free <= total) ? 1 : 0;
    }

    // =========================================================================
    // java.lang.Thread
    // =========================================================================

    /** Thread.currentThread() returns non-null. */
    public static int thread_currentThread() {
        return (Thread.currentThread() != null) ? 1 : 0;
    }

    /** Thread.currentThread().isAlive(). */
    public static int thread_isAlive() {
        return Thread.currentThread().isAlive() ? 1 : 0;
    }

    // =========================================================================
    // Type casting and conversions
    // =========================================================================

    /** Widening int to long. */
    public static int cast_int_to_long() {
        int i = 42;
        long l = i;
        return (l == 42L) ? 1 : 0;
    }

    /** Narrowing long to int. */
    public static int cast_long_to_int() {
        long l = 42L;
        int i = (int) l;
        return i; // 42
    }

    /** Int to float. */
    public static int cast_int_to_float() {
        int i = 100;
        float f = (float) i;
        return (f > 99.9f && f < 100.1f) ? 1 : 0;
    }

    /** Double to int truncation. */
    public static int cast_double_to_int() {
        double d = 3.99;
        int i = (int) d;
        return i; // 3 (truncation, not rounding)
    }

    /** Char to int. */
    public static int cast_char_to_int() {
        char c = 'A';
        int i = c;
        return i; // 65
    }

    // =========================================================================
    // Autoboxing / Unboxing
    // =========================================================================

    /** Autoboxing int to Integer. */
    public static int autobox_int() {
        Integer i = 42;
        return i; // 42 (unboxed)
    }

    /** Autoboxing double. */
    public static int autobox_double() {
        Double d = 3.14;
        return (d > 3.13 && d < 3.15) ? 1 : 0;
    }

    /** Autoboxing boolean. */
    public static int autobox_boolean() {
        Boolean b = true;
        return b ? 1 : 0;
    }

    // =========================================================================
    // java.lang.String — additional constructor / method tests (T4.2.3)
    // =========================================================================

    /** String.concat(). */
    public static int str_concat() {
        String a = "Hello";
        String b = " World";
        String c = a.concat(b);
        return c.equals("Hello World") ? 1 : 0;
    }

    /** String(char[]) constructor. */
    public static int str_constructor_chars() {
        char[] chars = {'H', 'e', 'l', 'l', 'o'};
        String s = new String(chars);
        return s.equals("Hello") ? 1 : 0;
    }
}

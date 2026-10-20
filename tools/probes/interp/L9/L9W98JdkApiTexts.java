// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 98, lane L9 -- exception type and text of common
// JDK API misuses. On HotSpot every row is the JDK's own bytecode (JEP 358
// messages naming the JDK method's <parameterN>/<localN>, or the JDK's
// literal text); --compatible serves many of these methods with Rust natives,
// whose texts must match byte for byte. A detector: each row prints
// "<label>: <class>: <message>" or "<label>: ok <value>".
//
//   $J/javac -d out tools/probes/interp/L9/L9W98JdkApiTexts.java
//   $B [--compatible] [--nojit] --java-home $J/.. -cp out L9W98JdkApiTexts
//   $J/java -cp out L9W98JdkApiTexts
import java.math.BigDecimal;
import java.math.BigInteger;
import java.nio.charset.Charset;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collection;
import java.util.Collections;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.UUID;

public class L9W98JdkApiTexts {
    enum Color {
        RED
    }

    interface Shape {
        Object run() throws Throwable;
    }

    static void row(String label, Shape s) {
        String text;
        try {
            text = "ok " + s.run();
        } catch (Throwable t) {
            text = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(label + ": " + text);
    }

    public static void main(String[] args) {
        String n = null;
        // ---- String with null arguments ----
        row("concat", () -> "abc".concat(n));
        row("compareTo", () -> "abc".compareTo(n));
        row("compareToIgnoreCase", () -> "abc".compareToIgnoreCase(n));
        row("contains", () -> "abc".contains(n));
        row("indexOf", () -> "abc".indexOf(n));
        row("lastIndexOf", () -> "abc".lastIndexOf(n));
        row("startsWith", () -> "abc".startsWith(n));
        row("endsWith", () -> "abc".endsWith(n));
        row("replace-target", () -> "abc".replace(n, "x"));
        row("replace-replacement", () -> "abc".replace("a", n));
        row("split", () -> "abc".split(n));
        row("equalsIgnoreCase", () -> "abc".equalsIgnoreCase(n));
        row("contentEquals", () -> "abc".contentEquals((CharSequence) null));
        row("join-delim", () -> String.join(n, "a", "b"));
        row("join-elements", () -> String.join(",", (Iterable<String>) null));
        row("valueOf-chars", () -> String.valueOf((char[]) null));
        row("new-string-chars", () -> new String((char[]) null));
        row("new-string-bytes", () -> new String((byte[]) null, Charset.defaultCharset()));
        row("getBytes-charset", () -> "abc".getBytes((Charset) null));
        row("format-null", () -> String.format(n, 1));
        row("repeat-negative", () -> "ab".repeat(-1));
        // ---- String / StringBuilder indexes ----
        row("charAt", () -> "abc".charAt(5));
        row("charAt-neg", () -> "abc".charAt(-1));
        row("substring", () -> "abc".substring(2, 1));
        row("substring-begin", () -> "abc".substring(-1));
        row("substring-end", () -> "abc".substring(4));
        row("codePointAt", () -> "abc".codePointAt(3));
        row("subSequence", () -> "abc".subSequence(1, 9));
        row("sb-charAt", () -> new StringBuilder("abc").charAt(3));
        row("sb-deleteCharAt", () -> new StringBuilder("abc").deleteCharAt(5));
        row("sb-delete", () -> new StringBuilder("abc").delete(2, 1));
        row("sb-insert", () -> new StringBuilder("abc").insert(9, "x"));
        row("sb-setLength", () -> { new StringBuilder("abc").setLength(-1); return null; });
        row("sb-setCharAt", () -> { new StringBuilder("abc").setCharAt(3, 'x'); return null; });
        row("sb-substring", () -> new StringBuilder("abc").substring(4));
        row("sb-replace", () -> new StringBuilder("abc").replace(4, 5, "x"));
        row("sb-new-null", () -> new StringBuilder((String) null));
        row("sb-append-chars-null", () -> new StringBuilder().append((char[]) null));
        row("sb-append-range", () -> new StringBuilder().append("abc", 2, 9));
        row("sb-new-negative", () -> new StringBuilder(-1));
        row("getChars", () -> { "abc".getChars(0, 4, new char[4], 0); return null; });
        // ---- numbers ----
        row("parseInt-null", () -> Integer.parseInt(n));
        row("parseInt-empty", () -> Integer.parseInt(""));
        row("parseInt-junk", () -> Integer.parseInt("12x"));
        row("parseInt-overflow", () -> Integer.parseInt("99999999999"));
        row("parseInt-radix", () -> Integer.parseInt("1", 99));
        row("parseInt-radix16", () -> Integer.parseInt("zz", 16));
        row("parseLong-null", () -> Long.parseLong(n));
        row("parseLong-junk", () -> Long.parseLong("-"));
        row("valueOf-int-null", () -> Integer.valueOf(n));
        row("parseUnsigned-minus", () -> Integer.parseUnsignedInt("-1"));
        row("parseDouble-null", () -> Double.parseDouble(n));
        row("parseDouble-junk", () -> Double.parseDouble("1.2.3"));
        row("parseFloat-empty", () -> Float.parseFloat(""));
        row("decode-null", () -> Integer.decode(n));
        row("decode-empty", () -> Integer.decode(""));
        row("toChars", () -> Character.toChars(-1));
        row("addExact", () -> Math.addExact(Integer.MAX_VALUE, 1));
        row("multiplyExact-long", () -> Math.multiplyExact(Long.MAX_VALUE, 2L));
        row("toIntExact", () -> Math.toIntExact(1L << 40));
        row("negateExact", () -> Math.negateExact(Integer.MIN_VALUE));
        row("floorMod-zero", () -> Math.floorMod(1, 0));
        row("divideUnsigned", () -> Long.divideUnsigned(1L, 0L));
        row("bigint-divide-zero", () -> BigInteger.ONE.divide(BigInteger.ZERO));
        row("bigint-mod-neg", () -> BigInteger.ONE.mod(BigInteger.valueOf(-3)));
        row("bigint-modInverse", () -> BigInteger.TWO.modInverse(BigInteger.valueOf(4)));
        row("bigint-pow-neg", () -> BigInteger.TWO.pow(-1));
        row("bigint-parse", () -> new BigInteger("12a"));
        row("bigint-parse-empty", () -> new BigInteger(""));
        row("bigdec-divide", () -> BigDecimal.ONE.divide(BigDecimal.valueOf(3)));
        row("bigdec-divide-zero", () -> BigDecimal.ONE.divide(BigDecimal.ZERO));
        row("bigdec-zero-zero", () -> BigDecimal.ZERO.divide(BigDecimal.ZERO));
        row("bigdec-parse", () -> new BigDecimal("1..2"));
        row("bigdec-intValueExact", () -> new BigDecimal("1.5").intValueExact());
        row("bigdec-setScale", () -> new BigDecimal("1.25").setScale(1));
        // ---- collections ----
        row("list-of-null", () -> List.of((Object) null));
        row("map-of-null-value", () -> Map.of("a", (Object) null));
        row("arraylist-null", () -> new ArrayList<>((Collection<?>) null));
        row("arraylist-addAll-null", () -> new ArrayList<>().addAll(null));
        row("hashmap-null", () -> new HashMap<>((Map<?, ?>) null));
        row("hashmap-putAll-null", () -> { new HashMap<>().putAll(null); return null; });
        row("sort-null", () -> { Collections.sort((List<String>) null); return null; });
        row("asList-null", () -> Arrays.asList((Object[]) null));
        row("arraylist-get", () -> new ArrayList<>().get(0));
        row("arraylist-get-neg", () -> new ArrayList<>(List.of(1)).get(-1));
        row("arraylist-set", () -> new ArrayList<>(List.of(1)).set(1, 2));
        row("arraylist-add-index", () -> { new ArrayList<>().add(2, "x"); return null; });
        row("arraylist-remove", () -> new ArrayList<>().remove(0));
        row("arraylist-sublist", () -> new ArrayList<>(List.of(1, 2)).subList(1, 0));
        row("arraylist-new-neg", () -> new ArrayList<>(-1));
        row("hashmap-new-neg", () -> new HashMap<>(-1));
        row("list-of-get", () -> List.of(1, 2).get(2));
        row("list-of-add", () -> List.of(1).add(2));
        row("unmodifiable-add", () -> Collections.unmodifiableList(new ArrayList<>()).add(1));
        row("aslist-add", () -> Arrays.asList(1).add(2));
        row("empty-iterator", () -> new ArrayList<>().iterator().next());
        row("optional-get", () -> Optional.empty().get());
        row("optional-of-null", () -> Optional.of(n));
        row("optional-orElseThrow", () -> Optional.empty().orElseThrow());
        row("copyOf-neg", () -> Arrays.copyOf(new int[1], -1));
        row("copyOfRange", () -> Arrays.copyOfRange(new int[1], 2, 1));
        row("fill-range", () -> { Arrays.fill(new int[2], 3, 1, 0); return null; });
        row("fill-range-high", () -> { Arrays.fill(new int[2], 0, 5, 0); return null; });
        row("sort-range", () -> { Arrays.sort(new int[2], 0, 5); return null; });
        row("asList-get", () -> Arrays.asList(1, 2).get(5));
        // ---- misc ----
        row("enum-valueOf", () -> Color.valueOf("BLUE"));
        row("enum-valueOf-null", () -> Color.valueOf(n));
        row("uuid", () -> UUID.fromString("x"));
        row("class-cast-method", () -> Integer.class.cast("s"));
        row("forName-missing", () -> Class.forName("no.such.Klass"));
        row("forName-array-syntax", () -> Class.forName("java.lang.String[]"));
        row("forName-null", () -> Class.forName(n));
        row("format-conversion", () -> String.format("%d", "x"));
        row("format-missing", () -> String.format("%s %s", "x"));
        row("thread-priority", () -> { new Thread(() -> {}).setPriority(11); return null; });
        row("objects-checkIndex", () -> java.util.Objects.checkIndex(5, 3));
        row("objects-checkFromToIndex", () -> java.util.Objects.checkFromToIndex(2, 1, 3));
        row("objects-checkFromIndexSize", () -> java.util.Objects.checkFromIndexSize(2, 5, 3));
        row("char-digit-radix", () -> (int) Character.forDigit(1, 99));
        row("system-getProperty-empty", () -> System.getProperty(""));
        row("system-getProperty-null", () -> System.getProperty(n));
    }
}

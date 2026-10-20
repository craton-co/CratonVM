// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 106, lane L7 (hunt; i100-L7-native-shadow-npe-texts' JDK side):
// exception texts of JDK methods that CratonVM shadows with registered natives (String,
// StringBuilder, Integer / Long / Double parsing, Arrays, ArrayList, List.of / Collections
// stand-ins). HotSpot runs the JDK 25 bytecode, so a null argument is a JEP 358 text that names
// the JDK's own parameter (java.base carries a LocalVariableTable), and a range error is the
// JDK's own check. A native that answers with a text of its own ("null object argument",
// "Array index out of range: n" where the JDK uses Preconditions, a different exception class)
// shows here as a DIFF. Each row is a single call (no hot loop), then the same row once more
// after 20000 non-throwing calls of the same method.
//
//   javac -d out L7W106JdkShadowTexts.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L7W106JdkShadowTexts
// Warm rows print the exception class only (HotSpot's -XX:+OmitStackTraceInFastThrow may drop
// the message and frames once C2 compiles a site); the cold rows carry the texts.
// The cold lines were predicted from the JDK 25 sources and measured SAME on HotSpot 25 and on
// CratonVM w106 (Windows, both modes, JIT and --nojit).
// HOTSPOT_EXPECTED_BEGIN
// cold string-compareTo-null -> java.lang.NullPointerException: Cannot read field "value" because "anotherString" is null
// cold string-concat-null -> java.lang.NullPointerException: Cannot invoke "String.isEmpty()" because "str" is null
// cold string-contains-null -> java.lang.NullPointerException: Cannot invoke "java.lang.CharSequence.toString()" because "s" is null
// cold string-startsWith-null -> java.lang.NullPointerException: Cannot invoke "String.length()" because "prefix" is null
// cold string-charAt-past -> java.lang.StringIndexOutOfBoundsException: Index 5 out of bounds for length 3
// cold string-substring-past -> java.lang.StringIndexOutOfBoundsException: begin 5, end 3, length 3
// cold string-substring-reversed -> java.lang.StringIndexOutOfBoundsException: begin 2, end 1, length 3
// cold string-repeat-negative -> java.lang.IllegalArgumentException: count is negative: -1
// cold sb-charAt-past -> java.lang.StringIndexOutOfBoundsException: Index 5 out of bounds for length 3
// cold sb-deleteCharAt-past -> java.lang.StringIndexOutOfBoundsException: Index 5 out of bounds for length 3
// cold sb-setLength-negative -> java.lang.StringIndexOutOfBoundsException: String index out of range: -1
// cold sb-insert-past -> java.lang.StringIndexOutOfBoundsException: offset 9, length 3
// cold sb-delete-reversed -> java.lang.StringIndexOutOfBoundsException: start 2, end 1, length 3
// cold sb-substring-past -> java.lang.StringIndexOutOfBoundsException: start 4, end 3, length 3
// cold sb-append-null-chars -> java.lang.NullPointerException: Cannot read the array length because "str" is null
// cold parseInt-null -> java.lang.NumberFormatException: Cannot parse null string: null
// cold parseLong-null -> java.lang.NumberFormatException: Cannot parse null string: null
// cold parseInt-empty -> java.lang.NumberFormatException: For input string: ""
// cold parseInt-radix -> java.lang.NumberFormatException: radix 99 greater than Character.MAX_RADIX
// cold arrays-fill-reversed -> java.lang.IllegalArgumentException: fromIndex(2) > toIndex(1)
// cold arrays-fill-negative -> java.lang.ArrayIndexOutOfBoundsException: Array index out of range: -1
// cold arrays-fill-past -> java.lang.ArrayIndexOutOfBoundsException: Array index out of range: 5
// cold arrays-copyOf-negative -> java.lang.NegativeArraySizeException: -1
// cold arrays-copyOfRange-reversed -> java.lang.IllegalArgumentException: 2 > 1
// cold arraylist-get-empty -> java.lang.IndexOutOfBoundsException: Index 0 out of bounds for length 0
// cold arraylist-add-past -> java.lang.IndexOutOfBoundsException: Index: 2, Size: 0
// cold arraylist-capacity -> java.lang.IllegalArgumentException: Illegal Capacity: -1
// cold arraylist-subList-reversed -> java.lang.IllegalArgumentException: fromIndex(1) > toIndex(0)
// cold hashmap-capacity -> java.lang.IllegalArgumentException: Illegal initial capacity: -1
// cold listOf0-get -> java.lang.ArrayIndexOutOfBoundsException: Index 0 out of bounds for length 0
// cold listOf1-get -> java.lang.IndexOutOfBoundsException: Index: 1 Size: 1
// cold listOf-null-element -> java.lang.NullPointerException: null
// cold listOf-add -> java.lang.UnsupportedOperationException: null
// cold emptyList-get -> java.lang.IndexOutOfBoundsException: Index: 0
// cold singletonList-get -> java.lang.IndexOutOfBoundsException: Index: 1, Size: 1
// warm string-compareTo-null -> java.lang.NullPointerException
// warm string-concat-null -> java.lang.NullPointerException
// warm string-contains-null -> java.lang.NullPointerException
// warm string-startsWith-null -> java.lang.NullPointerException
// warm string-charAt-past -> java.lang.StringIndexOutOfBoundsException
// warm string-substring-past -> java.lang.StringIndexOutOfBoundsException
// warm string-substring-reversed -> java.lang.StringIndexOutOfBoundsException
// warm string-repeat-negative -> java.lang.IllegalArgumentException
// warm sb-charAt-past -> java.lang.StringIndexOutOfBoundsException
// warm sb-deleteCharAt-past -> java.lang.StringIndexOutOfBoundsException
// warm sb-setLength-negative -> java.lang.StringIndexOutOfBoundsException
// warm sb-insert-past -> java.lang.StringIndexOutOfBoundsException
// warm sb-delete-reversed -> java.lang.StringIndexOutOfBoundsException
// warm sb-substring-past -> java.lang.StringIndexOutOfBoundsException
// warm sb-append-null-chars -> java.lang.NullPointerException
// warm parseInt-null -> java.lang.NumberFormatException
// warm parseLong-null -> java.lang.NumberFormatException
// warm parseInt-empty -> java.lang.NumberFormatException
// warm parseInt-radix -> java.lang.NumberFormatException
// warm arrays-fill-reversed -> java.lang.IllegalArgumentException
// warm arrays-fill-negative -> java.lang.ArrayIndexOutOfBoundsException
// warm arrays-fill-past -> java.lang.ArrayIndexOutOfBoundsException
// warm arrays-copyOf-negative -> java.lang.NegativeArraySizeException
// warm arrays-copyOfRange-reversed -> java.lang.IllegalArgumentException
// warm arraylist-get-empty -> java.lang.IndexOutOfBoundsException
// warm arraylist-add-past -> java.lang.IndexOutOfBoundsException
// warm arraylist-capacity -> java.lang.IllegalArgumentException
// warm arraylist-subList-reversed -> java.lang.IllegalArgumentException
// warm hashmap-capacity -> java.lang.IllegalArgumentException
// warm listOf0-get -> java.lang.ArrayIndexOutOfBoundsException
// warm listOf1-get -> java.lang.IndexOutOfBoundsException
// warm listOf-null-element -> java.lang.NullPointerException
// warm listOf-add -> java.lang.UnsupportedOperationException
// warm emptyList-get -> java.lang.IndexOutOfBoundsException
// warm singletonList-get -> java.lang.IndexOutOfBoundsException
// HOTSPOT_EXPECTED_END
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.HashMap;
import java.util.List;

public class L7W106JdkShadowTexts {
    // Warm rows print the exception CLASS only: once C2 has compiled a site, HotSpot may throw a
    // preallocated stackless exception without a message (-XX:+OmitStackTraceInFastThrow), so the
    // text and frames there depend on JIT timing. The texts are checked by the cold rows.
    static final String WARM = "warm";

    interface Row {
        Object run() throws Throwable;
    }

    static int sink;
    static String NUL;
    static char[] NOCHARS;
    static String ABC = "abc";

    static void row(String phase, String name, Row r) {
        String answer;
        try {
            answer = "no exception: " + r.run();
        } catch (Throwable t) {
            answer = WARM.equals(phase) ? t.getClass().getName() : t.toString();
        }
        System.out.println(phase + " " + name + " -> " + answer);
    }

    static void rows(String phase) {
        row(phase, "string-compareTo-null", () -> ABC.compareTo(NUL));
        row(phase, "string-concat-null", () -> ABC.concat(NUL));
        row(phase, "string-contains-null", () -> ABC.contains(NUL));
        row(phase, "string-startsWith-null", () -> ABC.startsWith(NUL));
        row(phase, "string-charAt-past", () -> ABC.charAt(5));
        row(phase, "string-substring-past", () -> ABC.substring(5));
        row(phase, "string-substring-reversed", () -> ABC.substring(2, 1));
        row(phase, "string-repeat-negative", () -> ABC.repeat(-1));
        row(phase, "sb-charAt-past", () -> new StringBuilder(ABC).charAt(5));
        row(phase, "sb-deleteCharAt-past", () -> new StringBuilder(ABC).deleteCharAt(5));
        row(phase, "sb-setLength-negative", () -> {
            new StringBuilder(ABC).setLength(-1);
            return "set";
        });
        row(phase, "sb-insert-past", () -> new StringBuilder(ABC).insert(9, "x"));
        row(phase, "sb-delete-reversed", () -> new StringBuilder(ABC).delete(2, 1));
        row(phase, "sb-substring-past", () -> new StringBuilder(ABC).substring(4));
        row(phase, "sb-append-null-chars", () -> new StringBuilder(ABC).append(NOCHARS));
        row(phase, "parseInt-null", () -> Integer.parseInt(NUL));
        row(phase, "parseLong-null", () -> Long.parseLong(NUL));
        row(phase, "parseInt-empty", () -> Integer.parseInt(""));
        row(phase, "parseInt-radix", () -> Integer.parseInt("1", 99));
        row(phase, "arrays-fill-reversed", () -> {
            Arrays.fill(new int[3], 2, 1, 0);
            return "filled";
        });
        row(phase, "arrays-fill-negative", () -> {
            Arrays.fill(new int[3], -1, 1, 0);
            return "filled";
        });
        row(phase, "arrays-fill-past", () -> {
            Arrays.fill(new int[3], 0, 5, 0);
            return "filled";
        });
        row(phase, "arrays-copyOf-negative", () -> Arrays.copyOf(new int[2], -1).length);
        row(phase, "arrays-copyOfRange-reversed", () -> Arrays.copyOfRange(new int[3], 2, 1).length);
        row(phase, "arraylist-get-empty", () -> new ArrayList<String>().get(0));
        row(phase, "arraylist-add-past", () -> {
            new ArrayList<String>().add(2, "x");
            return "added";
        });
        row(phase, "arraylist-capacity", () -> new ArrayList<String>(-1));
        row(phase, "arraylist-subList-reversed", () -> new ArrayList<String>().subList(1, 0));
        row(phase, "hashmap-capacity", () -> new HashMap<String, String>(-1));
        row(phase, "listOf0-get", () -> List.of().get(0));
        row(phase, "listOf1-get", () -> List.of("a").get(1));
        row(phase, "listOf-null-element", () -> List.of("a", NUL));
        row(phase, "listOf-add", () -> List.of("a").add("b"));
        row(phase, "emptyList-get", () -> Collections.emptyList().get(0));
        row(phase, "singletonList-get", () -> Collections.singletonList("a").get(1));
    }

    static void warm() {
        for (int i = 0; i < 20_000; i++) {
            StringBuilder sb = new StringBuilder(ABC);
            sink += ABC.compareTo("abd") + ABC.concat("d").length() + (ABC.contains("b") ? 1 : 0)
                + (ABC.startsWith("a") ? 1 : 0) + ABC.charAt(1) + ABC.substring(1).length()
                + ABC.substring(1, 2).length() + ABC.repeat(1).length();
            sink += sb.charAt(1) + sb.deleteCharAt(1).length() + sb.insert(1, "x").length()
                + sb.delete(0, 1).length() + sb.substring(1).length() + sb.append(new char[] {'q'}).length();
            sb.setLength(1);
            sink += Integer.parseInt("12") + (int) Long.parseLong("13") + Integer.parseInt("7", 8);
            int[] a = new int[3];
            Arrays.fill(a, 0, 2, i);
            sink += Arrays.copyOf(a, 2).length + Arrays.copyOfRange(a, 1, 2).length;
            ArrayList<String> list = new ArrayList<>(2);
            list.add(0, "x");
            sink += list.get(0).length() + list.subList(0, 1).size() + new HashMap<String, String>(2).size();
            sink += List.of("a", "b").get(1).length() + List.of("a").get(0).length()
                + Collections.singletonList("a").get(0).length() + Collections.emptyList().size();
        }
    }

    public static void main(String[] args) {
        rows("cold");
        warm();
        rows("warm");
    }
}

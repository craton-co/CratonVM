// JAVA21+
package cratonvm;

import java.util.Formatter;
import java.text.MessageFormat;
import java.text.DecimalFormat;

/**
 * Session 21: String Concat and Formatting.
 * StringConcatFactory, String.format(), Formatter, MessageFormat, DecimalFormat.
 */
public class StringFormatComplete {

    // ---- Test 1: Basic string concatenation (invokedynamic StringConcatFactory) ----

    public static int testStringConcat() {
        String a = "Hello";
        String b = "World";
        String result = a + ", " + b + "!";
        return result.equals("Hello, World!") ? 1 : 0; // 1
    }

    // ---- Test 2: Concat with primitives ----

    public static int testConcatWithPrimitives() {
        int x = 42;
        long y = 100L;
        double z = 3.14;
        boolean b = true;
        String result = "x=" + x + " y=" + y + " z=" + z + " b=" + b;
        return result.equals("x=42 y=100 z=3.14 b=true") ? 1 : 0; // 1
    }

    // ---- Test 3: Concat with null ----

    public static int testConcatWithNull() {
        String s = null;
        String result = "value=" + s;
        return result.equals("value=null") ? 1 : 0; // 1
    }

    // ---- Test 4: String.format %s ----

    public static int testFormatString() {
        String result = String.format("Hello, %s!", "World");
        return result.equals("Hello, World!") ? 1 : 0; // 1
    }

    // ---- Test 5: String.format %d ----

    public static int testFormatInt() {
        String result = String.format("The answer is %d", 42);
        return result.equals("The answer is 42") ? 1 : 0; // 1
    }

    // ---- Test 6: String.format %f ----

    public static int testFormatFloat() {
        String result = String.format("Pi is %.2f", 3.14159);
        return result.equals("Pi is 3.14") ? 1 : 0; // 1
    }

    // ---- Test 7: String.format %x (hex) ----

    public static int testFormatHex() {
        String result = String.format("%x", 255);
        return result.equals("ff") ? 1 : 0; // 1
    }

    // ---- Test 8: String.format %o (octal) ----

    public static int testFormatOctal() {
        String result = String.format("%o", 8);
        return result.equals("10") ? 1 : 0; // 1
    }

    // ---- Test 9: String.format %b (boolean) ----

    public static int testFormatBoolean() {
        String r1 = String.format("%b", true);
        String r2 = String.format("%b", false);
        return (r1.equals("true") && r2.equals("false")) ? 1 : 0; // 1
    }

    // ---- Test 10: String.format %c (char) ----

    public static int testFormatChar() {
        String result = String.format("%c", 'A');
        return result.equals("A") ? 1 : 0; // 1
    }

    // ---- Test 11: String.format with width ----

    public static int testFormatWidth() {
        String result = String.format("[%10s]", "hi");
        return result.equals("[        hi]") ? 1 : 0; // 1
    }

    // ---- Test 12: String.format with left-justify ----

    public static int testFormatLeftJustify() {
        String result = String.format("[%-10s]", "hi");
        return result.equals("[hi        ]") ? 1 : 0; // 1
    }

    // ---- Test 13: String.format with zero-padding ----

    public static int testFormatZeroPad() {
        String result = String.format("%05d", 42);
        return result.equals("00042") ? 1 : 0; // 1
    }

    // ---- Test 14: String.format %% literal ----

    public static int testFormatPercent() {
        String result = String.format("100%%");
        return result.equals("100%") ? 1 : 0; // 1
    }

    // ---- Test 15: String.format %n (newline) ----

    public static int testFormatNewline() {
        String result = String.format("a%nb");
        // %n is platform-specific, but should contain a and b separated by newline
        return (result.startsWith("a") && result.endsWith("b") && result.length() >= 3) ? 1 : 0; // 1
    }

    // ---- Test 16: String.format multiple args ----

    public static int testFormatMultipleArgs() {
        String result = String.format("%s is %d years old", "Alice", 30);
        return result.equals("Alice is 30 years old") ? 1 : 0; // 1
    }

    // ---- Test 17: String.format %e (scientific) ----

    public static int testFormatScientific() {
        String result = String.format("%.2e", 12345.6789);
        // Should be 1.23e+04 or 1.23E+04
        return (result.contains("1.23") && (result.contains("e") || result.contains("E"))) ? 1 : 0; // 1
    }

    // ---- Test 18: String.format %X (uppercase hex) ----

    public static int testFormatUpperHex() {
        String result = String.format("%X", 255);
        return result.equals("FF") ? 1 : 0; // 1
    }

    // ---- Test 19: String.formatted() instance method (Java 15+) ----

    public static int testStringFormatted() {
        String template = "Hello, %s! You are %d.";
        String result = template.formatted("Bob", 25);
        return result.equals("Hello, Bob! You are 25.") ? 1 : 0; // 1
    }

    // ---- Test 20: Formatter object ----

    public static int testFormatterObject() {
        Formatter fmt = new Formatter();
        fmt.format("Name: %s, Age: %d", "Eve", 28);
        String result = fmt.toString();
        fmt.close();
        return result.equals("Name: Eve, Age: 28") ? 1 : 0; // 1
    }

    // ---- Test 21: Formatter multiple format calls (append) ----

    public static int testFormatterAppend() {
        Formatter fmt = new Formatter();
        fmt.format("Hello");
        fmt.format(" ");
        fmt.format("World");
        String result = fmt.toString();
        fmt.close();
        return result.equals("Hello World") ? 1 : 0; // 1
    }

    // ---- Test 22: MessageFormat.format() ----

    public static int testMessageFormat() {
        String result = MessageFormat.format("Hello, {0}! You are {1} years old.", "Charlie", 35);
        // MessageFormat converts integers, might produce "35" or "35"
        return result.contains("Charlie") && result.contains("35") ? 1 : 0; // 1
    }

    // ---- Test 23: MessageFormat multiple placeholders ----

    public static int testMessageFormatMultiple() {
        String result = MessageFormat.format("{0} + {1} = {2}", 1, 2, 3);
        return result.contains("1") && result.contains("2") && result.contains("3") ? 1 : 0; // 1
    }

    // ---- Test 24: DecimalFormat basic ----

    public static int testDecimalFormatBasic() {
        DecimalFormat df = new DecimalFormat("#,##0.00");
        String result = df.format(1234.5);
        // Should be "1,234.50"
        return result.equals("1,234.50") ? 1 : 0; // 1
    }

    // ---- Test 25: DecimalFormat integer ----

    public static int testDecimalFormatInteger() {
        DecimalFormat df = new DecimalFormat("#,##0");
        String result = df.format(1000000);
        return result.equals("1,000,000") ? 1 : 0; // 1
    }

    // ---- Test 26: DecimalFormat no grouping ----

    public static int testDecimalFormatNoGrouping() {
        DecimalFormat df = new DecimalFormat("0.000");
        String result = df.format(3.14159);
        return result.equals("3.142") ? 1 : 0; // 1
    }

    // ---- Test 27: Concat with char ----

    public static int testConcatWithChar() {
        char c = '!';
        String result = "Hello" + c;
        return result.equals("Hello!") ? 1 : 0; // 1
    }

    // ---- Test 28: String.format + sign flag ----

    public static int testFormatPlusSign() {
        String result = String.format("%+d", 42);
        return result.equals("+42") ? 1 : 0; // 1
    }

    // ---- Test 29: Concat in loop ----

    public static int testConcatInLoop() {
        String result = "";
        for (int i = 0; i < 5; i++) {
            result = result + i;
        }
        return result.equals("01234") ? 1 : 0; // 1
    }

    // ---- Test 30: StringBuilder interop with format ----

    public static int testStringBuilderWithFormat() {
        StringBuilder sb = new StringBuilder();
        sb.append(String.format("(%d, %d)", 10, 20));
        sb.append(" -> ");
        sb.append(String.format("(%d, %d)", 30, 40));
        String result = sb.toString();
        return result.equals("(10, 20) -> (30, 40)") ? 1 : 0; // 1
    }
}

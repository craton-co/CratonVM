package cratonvm;

import java.text.BreakIterator;
import java.util.Locale;

/**
 * Regression probe for Spring Boot JsonReader's readJson -> JSONTokener path.
 *
 * JsonReader reads JSON through StringBuilder.append(char[], int, int), then
 * JSONTokener extracts an ordinary (unescaped) JSON string with
 * String.substring(start, end). Both bounds are deliberately non-zero so this
 * also guards the range-copy path rather than only whole-string copies.
 */
public final class JsonReaderSubstringProbe {

    private JsonReaderSubstringProbe() {
    }

    public static int preservesTrailingWord() {
        String expected = "Server namespace has moved to spring.server";
        String source = "{\"reason\":\"" + expected + "\"}";
        char[] chars = source.toCharArray();
        StringBuilder copied = new StringBuilder();
        copied.append(chars, 0, chars.length);
        String input = copied.toString();
        int start = input.indexOf(expected);
        String actual = new String(input.substring(start, start + expected.length()));
        return expected.equals(actual) ? 1 : 0;
    }

    public static int preservesTrailingWordAtJsonReaderOffset() {
        String expected = "Server namespace has moved to spring.server";
        StringBuilder source = new StringBuilder();
        for (int i = 0; i < 122; i++) {
            source.append('x');
        }
        source.append(expected).append("\", tail");
        String actual = new String(source.toString().substring(122, 165));
        return expected.equals(actual) ? 1 : 0;
    }

    public static int preservesSentenceAfterDottedIdentifier() {
        String expected = "Server namespace has moved to spring.server";
        BreakIterator iterator = BreakIterator.getSentenceInstance(Locale.US);
        iterator.setText(expected);
        return iterator.first() == 0 && iterator.next() == expected.length() ? 1 : 0;
    }

    public static void main(String[] args) {
        if (preservesTrailingWord() != 1 || preservesTrailingWordAtJsonReaderOffset() != 1
                || preservesSentenceAfterDottedIdentifier() != 1) {
            throw new AssertionError("JsonReader-shaped read or sentence extraction lost trailing characters");
        }
        System.out.println("JSON_READER_SUBSTRING_PROBE=PASS");
    }
}

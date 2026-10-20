/** Character.getType has TWO overloads. The earlier probe only exercised one. */
public class GetTypeOverloadProbe {
    public static void main(String[] a) {
        for (char c : new char[] { 'A', 'B', 'a', 'z', 'Α', 'Σ', 'À', '\'' }) {
            int viaChar = Character.getType(c);
            int viaInt = Character.getType((int) c);
            System.out.println("U+" + String.format("%04X", (int) c)
                    + "  getType(char)=" + viaChar
                    + "  getType(int)=" + viaInt
                    + "  agree=" + (viaChar == viaInt));
        }
        // The exact predicate ConditionalSpecialCasing.isCased applies.
        for (char c : new char[] { 'A', 'a', 'Α', 'À' }) {
            int t = Character.getType(c);
            boolean cased = t == Character.LOWERCASE_LETTER
                    || t == Character.UPPERCASE_LETTER
                    || t == Character.TITLECASE_LETTER;
            System.out.println("isCased-shape(U+" + String.format("%04X", (int) c) + ") = " + cased
                    + "  (type=" + t + ")");
        }
        System.out.println("UPPERCASE_LETTER=" + (int) Character.UPPERCASE_LETTER
                + " LOWERCASE_LETTER=" + (int) Character.LOWERCASE_LETTER
                + " TITLECASE_LETTER=" + (int) Character.TITLECASE_LETTER);
        System.out.println("RESULT done");
    }
}

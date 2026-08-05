public class StringAsciiDecodeProbe {
    static String units(String s) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < s.length(); i++) {
            if (i > 0) sb.append(' ');
            sb.append(String.format("%04X", (int) s.charAt(i)));
        }
        return sb.toString();
    }
    public static void main(String[] a) throws Exception {
        byte[] latin1 = { (byte) 0xE9, 'a', (byte) 0xFF };
        for (String cs : new String[] { "US-ASCII", "ISO-8859-1", "UTF-8" }) {
            String s = new String(latin1, cs);
            System.out.println(cs + " len=" + s.length() + " units=[" + units(s) + "]");
        }
        byte[] pure = { 'h', 'i' };
        System.out.println("pure-ascii len=" + new String(pure, "US-ASCII").length()
                + " units=[" + units(new String(pure, "US-ASCII")) + "]");
        System.out.println("ASCII-PROBE-DONE");
    }
}

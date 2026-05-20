package cratonvm;
public class TckStringBuilder {
    public static int sb_empty() { return new StringBuilder().length() == 0 ? 1 : 0; }
    public static int sb_append_string() { return "hello".equals(new StringBuilder().append("hello").toString()) ? 1 : 0; }
    public static int sb_append_int() { return "42".equals(new StringBuilder().append(42).toString()) ? 1 : 0; }
    public static int sb_append_char() { return "a".equals(new StringBuilder().append('a').toString()) ? 1 : 0; }
    public static int sb_append_boolean() { return "true".equals(new StringBuilder().append(true).toString()) ? 1 : 0; }
    public static int sb_append_double() { StringBuilder sb = new StringBuilder(); sb.append(3.14); return sb.toString().startsWith("3.14") ? 1 : 0; }
    public static int sb_capacity() { return new StringBuilder(100).capacity() >= 100 ? 1 : 0; }
    public static int sb_charAt() { return new StringBuilder("abc").charAt(1) == 'b' ? 1 : 0; }
    public static int sb_length() { return new StringBuilder("hello").length() == 5 ? 1 : 0; }
    public static int sb_reverse() { return "cba".equals(new StringBuilder("abc").reverse().toString()) ? 1 : 0; }
    public static int sb_delete() { return "ac".equals(new StringBuilder("abc").delete(1, 2).toString()) ? 1 : 0; }
    public static int sb_insert() { return "aXbc".equals(new StringBuilder("abc").insert(1, "X").toString()) ? 1 : 0; }
    public static int sb_replace() { return "aXc".equals(new StringBuilder("abc").replace(1, 2, "X").toString()) ? 1 : 0; }
    public static int sb_substring() { return "bc".equals(new StringBuilder("abc").substring(1)) ? 1 : 0; }
    public static int sb_indexOf() { return new StringBuilder("abcabc").indexOf("bc") == 1 ? 1 : 0; }
}

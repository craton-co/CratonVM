package cratonvm;
import java.util.regex.*;
public class TckRegex {
    public static int pattern_compile() { return Pattern.compile("abc") != null ? 1 : 0; }
    public static int pattern_matches() { return Pattern.matches("\\d+", "123") ? 1 : 0; }
    public static int pattern_no_match() { return !Pattern.matches("\\d+", "abc") ? 1 : 0; }
    public static int matcher_find() { Matcher m = Pattern.compile("\\d+").matcher("abc123def"); return m.find() ? 1 : 0; }
    public static int matcher_group() { Matcher m = Pattern.compile("(\\d+)").matcher("abc123def"); m.find(); return "123".equals(m.group(1)) ? 1 : 0; }
    public static int matcher_replaceAll() { return "xxx".equals(Pattern.compile("\\d").matcher("123").replaceAll("x")) ? 1 : 0; }
    public static int matcher_replaceFirst() { return "x23".equals(Pattern.compile("\\d").matcher("123").replaceFirst("x")) ? 1 : 0; }
    public static int pattern_split() { String[] parts = Pattern.compile(",").split("a,b,c"); return parts.length == 3 && "b".equals(parts[1]) ? 1 : 0; }
    public static int pattern_quote() { String q = Pattern.quote("[test]"); return Pattern.matches(q, "[test]") ? 1 : 0; }
    public static int pattern_flags() { Pattern p = Pattern.compile("abc", Pattern.CASE_INSENSITIVE); return (p.flags() & Pattern.CASE_INSENSITIVE) != 0 ? 1 : 0; }
    public static int pattern_toString() { return "abc".equals(Pattern.compile("abc").toString()) ? 1 : 0; }
}

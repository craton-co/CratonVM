package cratonvm;
import java.text.*;
public class TckDecimalFormat {
    public static int df_format_int() { return "42".equals(new DecimalFormat("0").format(42)) ? 1 : 0; }
    public static int df_format_double() { String s = new DecimalFormat("0.00").format(3.14); return s != null && s.contains("3") && s.contains("14") ? 1 : 0; }
    public static int df_pattern() { DecimalFormat df = new DecimalFormat("0.00"); return df.toPattern() != null ? 1 : 0; }
    public static int df_parse() { try { Number n = new DecimalFormat("0").parse("42"); return n.intValue() == 42 ? 1 : 0; } catch (Exception e) { return 0; } }
    public static int df_negative() { String s = new DecimalFormat("0").format(-5); return s.contains("5") ? 1 : 0; }
    public static int df_grouping() { DecimalFormat df = new DecimalFormat("#,###"); String s = df.format(1000000); return s != null && s.length() > 0 ? 1 : 0; }
    public static int df_percent() { DecimalFormat df = new DecimalFormat("0%"); String s = df.format(0.5); return s != null ? 1 : 0; }
    public static int df_max_fraction() { DecimalFormat df = new DecimalFormat("0"); df.setMaximumFractionDigits(2); return df.getMaximumFractionDigits() == 2 ? 1 : 0; }
}

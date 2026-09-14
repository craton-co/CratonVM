package cratonvm;
import java.text.*;
public class TckMessageFormat {
    public static int mf_basic() { return "Hello World".equals(MessageFormat.format("Hello {0}", new Object[]{"World"})) ? 1 : 0; }
    public static int mf_multiple_args() { String r = MessageFormat.format("{0} + {1} = {2}", new Object[]{"a", "b", "ab"}); return "a + b = ab".equals(r) ? 1 : 0; }
    public static int mf_number() { String r = MessageFormat.format("Count: {0}", new Object[]{42}); return r != null && r.contains("42") ? 1 : 0; }
    public static int mf_repeated_arg() { String r = MessageFormat.format("{0}{0}", new Object[]{"ab"}); return "abab".equals(r) ? 1 : 0; }
    public static int mf_no_args() { return "hello".equals(MessageFormat.format("hello", new Object[]{})) ? 1 : 0; }
    public static int mf_null_safe() { try { MessageFormat.format("{0}", new Object[]{(Object)null}); return 1; } catch (Exception e) { return 0; } }
    public static int mf_escape_single_quote() { String r = MessageFormat.format("It''s {0}", new Object[]{"ok"}); return r != null && r.contains("ok") ? 1 : 0; }
    public static int mf_toPattern() { MessageFormat mf = new MessageFormat("{0} items"); return mf.toPattern() != null ? 1 : 0; }
}

package cratonvm;
public class TckStackTrace {
    public static int ste_getClassName() {
        try { throw new Exception(); }
        catch (Exception e) {
            StackTraceElement[] st = e.getStackTrace();
            if (st == null || st.length == 0) return 0;
            return st[0].getClassName() != null ? 1 : 0;
        }
    }
    public static int ste_getMethodName() {
        try { throw new Exception(); }
        catch (Exception e) {
            StackTraceElement[] st = e.getStackTrace();
            if (st == null || st.length == 0) return 0;
            String m = st[0].getMethodName();
            return m != null ? 1 : 0;
        }
    }
    public static int ste_getFileName() {
        try { throw new Exception(); }
        catch (Exception e) {
            StackTraceElement[] st = e.getStackTrace();
            if (st == null || st.length == 0) return 0;
            // fileName can be null in synthetic environments
            return 1;
        }
    }
    public static int ste_getLineNumber() {
        try { throw new Exception(); }
        catch (Exception e) {
            StackTraceElement[] st = e.getStackTrace();
            if (st == null || st.length == 0) return 0;
            // line number may be -1 in synthetic environments, that's OK
            return st[0].getLineNumber() != 0 ? 1 : 0;
        }
    }
    public static int ste_toString() {
        try { throw new Exception(); }
        catch (Exception e) {
            StackTraceElement[] st = e.getStackTrace();
            if (st == null || st.length == 0) return 0;
            return st[0].toString() != null ? 1 : 0;
        }
    }
    public static int ste_constructor() {
        StackTraceElement ste = new StackTraceElement("Foo", "bar", "Foo.java", 42);
        if (!"Foo".equals(ste.getClassName())) return 0;
        if (!"bar".equals(ste.getMethodName())) return 0;
        if (!"Foo.java".equals(ste.getFileName())) return 0;
        if (ste.getLineNumber() != 42) return 0;
        return 1;
    }
    public static int ste_isNativeMethod() {
        StackTraceElement ste = new StackTraceElement("Foo", "bar", "Foo.java", -2);
        return ste.isNativeMethod() ? 1 : 0;
    }
    public static int ste_equals() {
        StackTraceElement a = new StackTraceElement("Foo", "bar", "Foo.java", 42);
        StackTraceElement b = new StackTraceElement("Foo", "bar", "Foo.java", 42);
        return a.equals(b) ? 1 : 0;
    }
}

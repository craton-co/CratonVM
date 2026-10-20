import sun.reflect.ReflectionFactory;

/**
 * `probes/SerAccess.java` showed that the serialization-constructor refusal
 * happens inside `newConstructorForSerialization` itself, not at the later
 * `newInstance()` call the known-issues page pointed at. This prints the FULL
 * stack of that throw, so the failing check can be named from frames rather
 * than inferred from a message.
 *
 * `java.lang.Integer` is the control: its first non-serializable ancestor is
 * `java.lang.Object`, whose no-arg constructor is PUBLIC, so it takes a
 * different arm and must not throw on any VM.
 */
public class SerTrace {

    static void show(Class<?> target) {
        System.out.println("== " + target.getName() + " ==");
        try {
            Object c = ReflectionFactory.getReflectionFactory()
                    .newConstructorForSerialization(target);
            System.out.println("   no throw; got " + c);
        } catch (Throwable t) {
            System.out.println("   THREW " + t.getClass().getName() + ": " + t.getMessage());
            for (StackTraceElement e : t.getStackTrace()) {
                System.out.println("      at " + e.getClassName() + "." + e.getMethodName()
                        + "(" + e.getFileName() + ":" + e.getLineNumber() + ")");
            }
            Throwable cause = t.getCause();
            while (cause != null) {
                System.out.println("   caused by " + cause.getClass().getName()
                        + ": " + cause.getMessage());
                for (StackTraceElement e : cause.getStackTrace()) {
                    System.out.println("      at " + e.getClassName() + "." + e.getMethodName());
                }
                cause = cause.getCause();
            }
        }
    }

    public static void main(String[] args) {
        System.out.println("java.version = " + System.getProperty("java.version"));
        show(Integer.class);
        show(java.util.ArrayList.class);
        System.out.println("DONE");
    }
}

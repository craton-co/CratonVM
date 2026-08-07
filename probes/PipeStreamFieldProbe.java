import java.lang.reflect.Field;

/**
 * Why is `ProcessImpl$ProcessPipeInputStream.closeLock` null?
 *
 * `destroy()` closes all three streams, and both pipe-stream classes guard
 * `close()` with `synchronized (closeLock)`, where `closeLock` is a
 * `private final Object closeLock = new Object()` instance initializer. A null
 * there means the field initializer did not run, or did not land in the slot
 * the read uses — two very different defects.
 *
 * Print the concrete class of each stream, then every declared field of that
 * class with its value, so the answer is "the initializer never ran" (all
 * fields default) versus "it ran into the wrong slot" (values present but
 * shifted).
 */
public class PipeStreamFieldProbe {
    static void dump(String label, Object o) {
        if (o == null) {
            System.out.println(label + ".class=null");
            return;
        }
        Class<?> c = o.getClass();
        System.out.println(label + ".class=" + c.getName());
        System.out.println(label + ".super=" + c.getSuperclass().getName());
        for (Class<?> k = c; k != null && k != Object.class; k = k.getSuperclass()) {
            for (Field f : k.getDeclaredFields()) {
                if (java.lang.reflect.Modifier.isStatic(f.getModifiers())) {
                    continue;
                }
                String v;
                try {
                    f.setAccessible(true);
                    Object got = f.get(o);
                    v = (got == null) ? "null" : got.getClass().getName();
                } catch (Throwable t) {
                    v = "threw " + t.getClass().getName();
                }
                System.out.println(label + ".field " + k.getSimpleName() + "." + f.getName()
                        + " : " + f.getType().getSimpleName() + " = " + v);
            }
        }
    }

    public static void main(String[] args) throws Exception {
        Process p = new ProcessBuilder("/bin/sleep", "5").start();
        dump("stdout", p.getInputStream());
        dump("stdin", p.getOutputStream());
        p.destroyForcibly();
        p.waitFor();
        System.out.println("DONE");
    }
}

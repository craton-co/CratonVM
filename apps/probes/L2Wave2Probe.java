import java.lang.management.ManagementFactory;
import java.lang.management.MemoryUsage;
import java.lang.reflect.Method;

/** Lane 2 wave 2: the families a corpus screen called clean and nothing else
 *  had measured -- `java.lang.Object`, `Package`, `StringUTF16`,
 *  `Throwable.initCause`, five throwable constructor sets, and
 *  `java.lang.management`'s factories.
 *
 *  ASKS SHAPES, NOT VALUES. Every MXBean number here is a fact about the
 *  machine and the moment (`getUsed()`, uptime, thread counts), and the two VMs
 *  are *supposed* to disagree about all of them. So the rows are orderings,
 *  bounds and non-nullness -- `getInit() <= getUsed()`, `getMax() == -1 || >=
 *  getUsed()` -- which the specification fixes and which a broken shim gets
 *  wrong. Same for `Object.toString()`: the identity hash is asked as a shape
 *  (starts with the class name, contains '@', stable across calls, two objects
 *  render differently) and never printed.
 *
 *  AND IT AIMS AT EDGES. Of the campaign's first 48 defects, not one was a
 *  wrong answer to an ordinary call. So the null argument, the second
 *  `initCause`, the self-cause, `wait` without the monitor and `wait(-1)` are
 *  the rows that matter; the happy path is here to prove the instrument works.
 */
public class L2Wave2Probe {
    static int rows = 0;

    /** Escapes anything outside printable ASCII.
     *
     *  `StringUTF16.getChars` writes into a `char[]` this probe deliberately
     *  leaves partly unwritten, so a row rendered it with NUL bytes in it --
     *  which made the output file BINARY, so `diff` printed
     *  "Binary files differ" and `grep -c '^[<>]'` counted **zero** differing
     *  lines over a run that had twelve. A probe whose output cannot be diffed
     *  reads exactly like a probe that passed. */
    static void row(String label, Object v) {
        String s = String.valueOf(v);
        StringBuilder out = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c >= 0x20 && c < 0x7f) {
                out.append(c);
            } else {
                out.append(String.format("<U+%04x>", (int) c));
            }
        }
        System.out.println(label + " |" + out + "|");
        rows++;
    }

    static void ask(String label, Thing t) {
        try {
            row(label, t.get());
        } catch (Throwable e) {
            row(label, "THREW " + e.getClass().getName() + ": " + e.getMessage());
        }
    }

    interface Thing {
        Object get() throws Throwable;
    }

    // ---------- java.lang.Object ----------
    static void object() {
        Object a = new Object();
        Object b = new Object();
        ask("obj equals self", () -> a.equals(a));
        ask("obj equals other", () -> a.equals(b));
        ask("obj equals null", () -> a.equals(null));
        ask("obj toString starts with class", () -> a.toString().startsWith("java.lang.Object@"));
        ask("obj toString stable", () -> a.toString().equals(a.toString()));
        ask("obj toString differs", () -> !a.toString().equals(b.toString()));
        ask("obj toString hash matches", () ->
            a.toString().endsWith(Integer.toHexString(a.hashCode())));
        ask("obj hashCode stable", () -> a.hashCode() == a.hashCode());
        // wait/notify contract, never a call that could block forever
        ask("wait no monitor", () -> { a.wait(1); return "no throw"; });
        ask("wait negative", () -> {
            synchronized (a) { a.wait(-1); }
            return "no throw";
        });
        ask("wait 1ms with monitor", () -> {
            synchronized (a) { a.wait(1); }
            return "returned";
        });
        ask("wait(0,0) negative nanos", () -> {
            synchronized (a) { a.wait(0, -1); }
            return "no throw";
        });
        ask("notify no monitor", () -> { a.notify(); return "no throw"; });
        // finalize is protected; a subclass can call super.finalize()
        ask("finalize via super", () -> {
            new Object() { Object call() throws Throwable { super.finalize(); return "ok"; } }.call();
            return "ok";
        });
        ask("clone non-cloneable", () -> {
            new Object() { Object call() throws Throwable { return super.clone(); } }.call();
            return "cloned";
        });
    }

    // ---------- java.lang.Package ----------
    static void pkg() {
        ask("getPackages non-empty", () -> Package.getPackages().length > 0);
        ask("getPackages has java.lang", () -> {
            for (Package p : Package.getPackages()) {
                if (p.getName().equals("java.lang")) return true;
            }
            return false;
        });
        Package p = Object.class.getPackage();
        ask("java.lang package name", () -> p.getName());
        ask("package hashCode stable", () -> p.hashCode() == p.hashCode());
        ask("package hashCode == name hashCode", () -> p.hashCode() == p.getName().hashCode());
        ask("package equals self", () -> p.equals(p));
        ask("package equals null", () -> p.equals(null));
        ask("package equals other type", () -> p.equals("java.lang"));
    }

    // ---------- java.lang.StringUTF16.getChars ----------
    static void stringUtf16() {
        ask("StringUTF16.getChars", () -> {
            Class<?> c = Class.forName("java.lang.StringUTF16");
            Method m = c.getDeclaredMethod("getChars", byte[].class, int.class,
                                           int.class, char[].class, int.class);
            m.setAccessible(true);
            // "AB" in UTF16 little-endian byte form
            byte[] val = {65, 0, 66, 0, 67, 0};
            char[] out = new char[3];
            m.invoke(null, val, 0, 3, out, 0);
            return new String(out);
        });
        ask("StringUTF16.getChars partial", () -> {
            Class<?> c = Class.forName("java.lang.StringUTF16");
            Method m = c.getDeclaredMethod("getChars", byte[].class, int.class,
                                           int.class, char[].class, int.class);
            m.setAccessible(true);
            byte[] val = {65, 0, 66, 0, 67, 0};
            char[] out = new char[4];
            out[0] = 'x';
            m.invoke(null, val, 1, 3, out, 1);
            return new String(out);
        });
    }

    // ---------- Throwable.initCause and the constructor sets ----------
    static void throwables() {
        ask("initCause once", () -> {
            Throwable t = new Throwable("m");
            return t.initCause(new RuntimeException("c")).getCause().getMessage();
        });
        ask("initCause twice", () -> {
            Throwable t = new Throwable("m");
            t.initCause(new RuntimeException("c"));
            t.initCause(new RuntimeException("d"));
            return "no throw";
        });
        ask("initCause self", () -> {
            Throwable t = new Throwable("m");
            t.initCause(t);
            return "no throw";
        });
        ask("initCause after cause ctor", () -> {
            Throwable t = new Throwable("m", new RuntimeException("c"));
            t.initCause(new RuntimeException("d"));
            return "no throw";
        });
        ask("initCause null once", () -> {
            Throwable t = new Throwable("m");
            t.initCause(null);
            return String.valueOf(t.getCause());
        });

        // `ExceptionInInitializerError` does NOT declare `initCause`; it inherits
        // `Throwable`'s, which is concrete -- so the registration on it is a
        // genuine bucket-B shadow and not the phantom an inherited `<init>`
        // would be. A constructor is never inherited; an ordinary method is.
        ask("eiie initCause once", () -> {
            Throwable t = new ExceptionInInitializerError("m");
            return String.valueOf(t.initCause(new RuntimeException("c")).getCause());
        });
        ask("eiie initCause twice", () -> {
            Throwable t = new ExceptionInInitializerError("m");
            t.initCause(new RuntimeException("c"));
            t.initCause(new RuntimeException("d"));
            return "no throw";
        });
        ask("eiie initCause self", () -> {
            Throwable t = new ExceptionInInitializerError("m");
            t.initCause(t);
            return "no throw";
        });
        ask("eiie cause ctor then initCause", () -> {
            Throwable t = new ExceptionInInitializerError(new RuntimeException("c"));
            t.initCause(new RuntimeException("d"));
            return "no throw";
        });
        ask("eiie getException", () -> String.valueOf(
            ((ExceptionInInitializerError) new ExceptionInInitializerError(
                new RuntimeException("c"))).getException()));

        // constructor sets: message, cause, and that the stack trace is real
        ctor("VirtualMachineError", new VirtualMachineError() {});
        each("ExceptionInInitializerError",
             new ExceptionInInitializerError(),
             new ExceptionInInitializerError("m"),
             new ExceptionInInitializerError(new RuntimeException("c")));
        each("IllegalThreadStateException",
             new IllegalThreadStateException(),
             new IllegalThreadStateException("m"));
        each("UnsatisfiedLinkError",
             new UnsatisfiedLinkError(),
             new UnsatisfiedLinkError("m"));
        each("NullPointerException", new NullPointerException("m"));
    }

    static void ctor(String tag, Throwable t) {
        ask(tag + " message", () -> String.valueOf(t.getMessage()));
        ask(tag + " cause", () -> String.valueOf(t.getCause()));
        ask(tag + " trace non-empty", () -> t.getStackTrace().length > 0);
        ask(tag + " top frame is this class", () ->
            t.getStackTrace()[0].getClassName().startsWith("L2Wave2Probe"));
    }

    static void each(String tag, Throwable... ts) {
        int i = 0;
        for (Throwable t : ts) {
            ctor(tag + "[" + (i++) + "]", t);
            final Throwable ft = t;
            ask(tag + " toString shape", () ->
                ft.toString().startsWith(ft.getClass().getName()));
            ask(tag + " suppressed empty", () -> ft.getSuppressed().length);
        }
    }

    // ---------- java.lang.management ----------
    static void management() {
        ask("classLoading non-null", () -> ManagementFactory.getClassLoadingMXBean() != null);
        ask("classLoading loaded >= 0", () ->
            ManagementFactory.getClassLoadingMXBean().getLoadedClassCount() >= 0);
        ask("compilation nullable", () -> {
            Object o = ManagementFactory.getCompilationMXBean();
            return o == null ? "null" : "present";
        });
        ask("memory non-null", () -> ManagementFactory.getMemoryMXBean() != null);
        ask("runtime non-null", () -> ManagementFactory.getRuntimeMXBean() != null);
        ask("runtime uptime >= 0", () -> ManagementFactory.getRuntimeMXBean().getUptime() >= 0);
        ask("thread non-null", () -> ManagementFactory.getThreadMXBean() != null);
        ask("thread count > 0", () -> ManagementFactory.getThreadMXBean().getThreadCount() > 0);
        ask("os non-null", () -> ManagementFactory.getOperatingSystemMXBean() != null);
        ask("os processors > 0", () ->
            ManagementFactory.getOperatingSystemMXBean().getAvailableProcessors() > 0);
        ask("gc beans is a list", () -> ManagementFactory.getGarbageCollectorMXBeans() != null);
        ask("platform bean by class", () ->
            ManagementFactory.getPlatformMXBean(java.lang.management.RuntimeMXBean.class) != null);
        ask("platform beans by class", () ->
            ManagementFactory.getPlatformMXBeans(java.lang.management.MemoryMXBean.class).size() >= 1);
        ask("platform bean null class", () ->
            ManagementFactory.getPlatformMXBean(null) != null);

        MemoryUsage heap = ManagementFactory.getMemoryMXBean().getHeapMemoryUsage();
        ask("heap usage non-null", () -> heap != null);
        ask("heap init >= -1", () -> heap.getInit() >= -1);
        ask("heap used >= 0", () -> heap.getUsed() >= 0);
        ask("heap committed >= used", () -> heap.getCommitted() >= heap.getUsed());
        ask("heap max -1 or >= used", () -> heap.getMax() == -1 || heap.getMax() >= heap.getUsed());
        ask("heap toString shape", () -> heap.toString().startsWith("init = "));
        MemoryUsage made = new MemoryUsage(1, 2, 3, 4);
        ask("made init", () -> made.getInit());
        ask("made used", () -> made.getUsed());
        ask("made committed", () -> made.getCommitted());
        ask("made max", () -> made.getMax());
        ask("made bad order", () -> new MemoryUsage(1, 5, 3, 4).getUsed());
        ask("made negative used", () -> new MemoryUsage(1, -2, 3, 4).getUsed());
    }

    public static void main(String[] a) {
        object();
        pkg();
        stringUtf16();
        throwables();
        management();
        System.out.println("rows " + rows);
        System.out.println("DONE L2Wave2Probe");
    }
}

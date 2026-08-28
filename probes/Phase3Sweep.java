import java.io.*;
import java.lang.invoke.*;
import java.util.*;

/** The four still-listed PHASE 3 items from `docs/feature-designs/
 *  jdk-only-completion-roadmap.md`, asked of the VM directly.
 *
 *  The roadmap is dated 2026-08-12 and says "FINAL". Three of its four Phase 3
 *  entries now LOOK closed in the source -- `constants.rs` carries an `ldc`
 *  `MethodType`/`MethodHandle`/`Dynamic` decode, `lang_system.rs` carries a
 *  typed linkage-error recovery, and `lang_string.rs` is full of
 *  `Locale.Category.FORMAT`. A comment claiming a capability outlives the day
 *  it was true exactly as often as one denying it, so none of that is evidence.
 *  This probe asks the running VM.
 *
 *    P3-A  the JIT omits the aastore covariance check (cold vs HOT)
 *    P3-B  MethodHandleProxies.asInterfaceInstance -> ClassFormatError
 *    P3-C  typed linkage errors flattened to ClassFormatError
 *    P3-E  String.format with no Locale localises against ROOT
 *
 *  DETERMINISM: no timings, no identity hashes, no thread names. The P3-A arm
 *  reports only the pair (cold answer, hot answer) -- what matters is whether
 *  they AGREE, which is a property, not a measurement.
 */
public class Phase3Sweep {
    static String esc(String s) {
        StringBuilder b = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }
    static void p(String tag, Object v) {
        System.out.println(esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }
    static void t(String tag, ThrowingRun r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }
    interface ThrowingRun { void run() throws Throwable; }

    // ---- P3-A: aastore covariance, cold and hot ------------------------
    /** Stores `val` into `arr[0]` through an `Object[]` alias, so javac emits a
     *  bare `aastore` and the CHECK is the VM's, not the compiler's. */
    static String store(Object[] arr, Object val) {
        try { arr[0] = val; return "no-throw"; }
        catch (ArrayStoreException e) { return e.getClass().getSimpleName(); }
    }
    static void aastore() {
        String[] strings = new String[1];
        String cold = store(strings, Integer.valueOf(7));
        // Make `store` hot enough to be compiled. Legal stores only, so nothing
        // here can deoptimise on the exception path.
        Object[] objects = new Object[1];
        for (int i = 0; i < 400_000; i++) store(objects, "ok");
        String hot = store(strings, Integer.valueOf(7));
        p("P3-A aastore cold", cold);
        p("P3-A aastore hot", hot);
        p("P3-A cold == hot", cold.equals(hot));
        // The same question one level down: a covariant array passed as its
        // supertype, which is the shape the roadmap's own repro used.
        Number[] nums = new Integer[1];
        String coldN = store(nums, Double.valueOf(1.0));
        for (int i = 0; i < 400_000; i++) store(objects, "ok");
        String hotN = store(nums, Double.valueOf(1.0));
        p("P3-A covariant cold", coldN);
        p("P3-A covariant hot", hotN);
        p("P3-A covariant cold == hot", coldN.equals(hotN));
        // Legal stores must stay legal on both tiers.
        p("P3-A legal store cold", store(new Object[1], "s"));
        p("P3-A legal null store", store(new String[1], null));
    }

    // ---- P3-B: MethodHandleProxies -------------------------------------
    public interface Adder { int add(int a, int b); }
    static void proxies() {
        try {
            MethodHandle mh = MethodHandles.lookup().findStatic(
                Math.class, "max", MethodType.methodType(int.class, int.class, int.class));
            Adder a = MethodHandleProxies.asInterfaceInstance(Adder.class, mh);
            p("P3-B asInterfaceInstance built", a != null);
            p("P3-B proxy call", a.add(3, 9));
            p("P3-B isWrapperInstance", MethodHandleProxies.isWrapperInstance(a));
            p("P3-B wrapperInstanceType is Adder",
              MethodHandleProxies.wrapperInstanceTarget(a).type().returnType() == int.class);
        } catch (Throwable e) {
            p("P3-B asInterfaceInstance", "THREW " + e.getClass().getName()
              + (e.getMessage() == null ? "" : ": " + e.getMessage()));
        }
        // A plain dynamic proxy, for contrast: if THIS is broken the failure is
        // not MethodHandleProxies-specific and the P3-B diagnosis is wrong.
        try {
            Adder p = (Adder) java.lang.reflect.Proxy.newProxyInstance(
                Phase3Sweep.class.getClassLoader(), new Class<?>[]{Adder.class},
                (proxy, m, args) -> ((Integer) args[0]) + ((Integer) args[1]));
            p("P3-B plain Proxy call", p.add(3, 9));
        } catch (Throwable e) {
            p("P3-B plain Proxy", "THREW " + e.getClass().getName());
        }
    }

    // ---- P3-C: typed linkage errors ------------------------------------
    static class Defiler extends ClassLoader {
        Defiler() { super(Phase3Sweep.class.getClassLoader()); }
        Class<?> def(byte[] b) { return defineClass(null, b, 0, b.length); }
    }
    static byte[] ownBytes() throws IOException {
        try (InputStream in = Phase3Sweep.class.getResourceAsStream("/Phase3Sweep.class")) {
            if (in == null) return null;
            ByteArrayOutputStream o = new ByteArrayOutputStream();
            byte[] buf = new byte[4096];
            for (int n; (n = in.read(buf)) > 0; ) o.write(buf, 0, n);
            return o.toByteArray();
        }
    }
    static void linkage() throws Exception {
        byte[] good = ownBytes();
        p("P3-C own class bytes readable", good != null);
        if (good == null) return;
        p("P3-C class file major version", ((good[6] & 0xff) << 8) | (good[7] & 0xff));

        // (a) a class file from the FUTURE must be UnsupportedClassVersionError
        byte[] future = good.clone();
        future[6] = (byte) 0x00; future[7] = (byte) 0xFF;   // major 255
        t("P3-C future version", () -> new Defiler().def(future));
        p("P3-C future version type", thrownType(() -> new Defiler().def(future)));

        // (b) a bad magic number must be ClassFormatError
        byte[] magic = good.clone();
        magic[0] = 0x00;
        p("P3-C bad magic type", thrownType(() -> new Defiler().def(magic)));

        // (c) truncation must be ClassFormatError, not an internal error
        byte[] cut = Arrays.copyOf(good, 20);
        p("P3-C truncated type", thrownType(() -> new Defiler().def(cut)));

        // (d) an empty array
        p("P3-C empty bytes type", thrownType(() -> new Defiler().def(new byte[0])));

        // (e) the message must not be a Rust Debug rendering. Printing the
        //     message itself would be a diff on wording, so print only the two
        //     properties that matter: it names no Rust type and no braces.
        String msg = thrownMessage(() -> new Defiler().def(future));
        p("P3-C msg mentions Linkage(", msg.contains("Linkage("));
        p("P3-C msg has a brace", msg.contains("{"));
        p("P3-C msg mentions class_name:", msg.contains("class_name:"));
    }
    static String thrownType(ThrowingRun r) {
        try { r.run(); return "no-throw"; }
        catch (Throwable e) { return e.getClass().getName(); }
    }
    static String thrownMessage(ThrowingRun r) {
        try { r.run(); return ""; }
        catch (Throwable e) { return String.valueOf(e.getMessage()); }
    }

    // ---- P3-E: String.format and the FORMAT default --------------------
    static void formatLocale() {
        Locale savedFormat = Locale.getDefault(Locale.Category.FORMAT);
        Locale savedDisplay = Locale.getDefault(Locale.Category.DISPLAY);
        try {
            // GERMANY: grouping '.', decimal ',' -- ROOT is the opposite, so a
            // single formatted number separates the two hypotheses outright.
            Locale.setDefault(Locale.Category.FORMAT, Locale.GERMANY);
            p("P3-E FORMAT default", Locale.getDefault(Locale.Category.FORMAT));
            p("P3-E no-locale %,.2f", String.format("%,.2f", 1234.5));
            p("P3-E explicit GERMANY %,.2f", String.format(Locale.GERMANY, "%,.2f", 1234.5));
            p("P3-E explicit ROOT %,.2f", String.format(Locale.ROOT, "%,.2f", 1234.5));
            p("P3-E no-locale %,d", String.format("%,d", 1234567));
            p("P3-E no-locale %e", String.format("%e", 1234.5));
            p("P3-E no-locale %.3f", String.format("%.3f", 0.5));
            // Formatter and PrintStream must agree with String.format.
            StringBuilder sb = new StringBuilder();
            new Formatter(sb).format("%,.2f", 1234.5);
            p("P3-E Formatter no-locale", sb.toString());
            p("P3-E String.valueOf unaffected", String.valueOf(1234.5));
            // DISPLAY must NOT drive formatting.
            Locale.setDefault(Locale.Category.DISPLAY, Locale.US);
            p("P3-E DISPLAY=US does not change format", String.format("%,.2f", 1234.5));
            // and back to ROOT-ish to show the switch is live
            Locale.setDefault(Locale.Category.FORMAT, Locale.US);
            p("P3-E FORMAT=US %,.2f", String.format("%,.2f", 1234.5));
        } catch (Throwable e) {
            p("P3-E", "THREW " + e.getClass().getName());
        } finally {
            Locale.setDefault(Locale.Category.FORMAT, savedFormat);
            Locale.setDefault(Locale.Category.DISPLAY, savedDisplay);
        }
    }

    public static void main(String[] a) throws Exception {
        aastore();
        proxies();
        linkage();
        formatLocale();
        System.out.println("DONE Phase3Sweep");
    }
}

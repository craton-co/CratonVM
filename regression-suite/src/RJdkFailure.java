import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.InvocationTargetException;
import java.nio.charset.StandardCharsets;
import java.nio.charset.UnsupportedCharsetException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.NoSuchAlgorithmException;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;

/**
 * JDK-only corpus: failure semantics -- missing class, missing method, missing
 * native, missing module, unsupported platform service.
 *
 * The point of {@code --jdk-only} is that an absent thing FAILS in the way the
 * specification says, instead of being fabricated. Every assertion here is
 * mode-INDEPENDENT: a correct VM produces exactly this behaviour in
 * {@code --real-jdk} too, so the vector is a valid HotSpot diff in both modes.
 * The mode-DIVERGENT probes (the enterprise-prefix compatibility stubs) live in
 * RJdkStrict, which only runs under {@code --jdk-only}.
 *
 * Genuine LINKAGE errors (NoSuchMethodError / NoClassDefFoundError) cannot be
 * produced by javac, so this vector manufactures them the honest way: it takes
 * the bytes javac emitted for a caller class and rewrites one same-length ASCII
 * constant-pool entry, then defines the result as a hidden class. Nothing is
 * hand-assembled.
 *
 * Determinism: exception MESSAGES that embed paths or library search lists are
 * never printed; only type names and the class/method names this vector chose.
 */
public class RJdkFailure {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    public interface Task {
        String run();
    }

    /** The call target. Both methods really exist. */
    public static class Victim {
        public static String victimPresentOk() {
            return "present";
        }

        public static String victimAbsentZero() {
            return "absent0";
        }

        public static int victimFieldOk = 7;
    }

    /** Its bytes are patched to call a method that does not exist. */
    public static class MissingMethodCaller implements Task {
        public MissingMethodCaller() {
        }

        @Override
        public String run() {
            return Victim.victimAbsentZero();
        }
    }

    /** Its bytes are patched to reference a class that does not exist. */
    public static class MissingClassCaller implements Task {
        public MissingClassCaller() {
        }

        @Override
        public String run() {
            return Victim.victimPresentOk();
        }
    }

    static byte[] bytesOf(String simpleBinaryName) throws Exception {
        try (InputStream in = RJdkFailure.class.getResourceAsStream(simpleBinaryName + ".class")) {
            check(in != null, "class bytes not on the class path: " + simpleBinaryName);
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            byte[] buf = new byte[4096];
            int n;
            while ((n = in.read(buf)) > 0) {
                out.write(buf, 0, n);
            }
            return out.toByteArray();
        }
    }

    /** Replace one same-length ASCII run in the class file. Returns the count. */
    static int patch(byte[] bytes, String from, String to) {
        check(from.length() == to.length(), "patch strings must be the same length");
        byte[] f = from.getBytes(StandardCharsets.US_ASCII);
        byte[] t = to.getBytes(StandardCharsets.US_ASCII);
        int hits = 0;
        outer:
        for (int i = 0; i + f.length <= bytes.length; i++) {
            for (int j = 0; j < f.length; j++) {
                if (bytes[i + j] != f[j]) {
                    continue outer;
                }
            }
            System.arraycopy(t, 0, bytes, i, t.length);
            hits++;
        }
        return hits;
    }

    static Task defineTask(byte[] bytes) throws Throwable {
        Class<?> k = MethodHandles.lookup()
                .defineHiddenClass(bytes, true, MethodHandles.Lookup.ClassOption.NESTMATE)
                .lookupClass();
        return (Task) k.getDeclaredConstructor().newInstance();
    }

    static void missingClass() throws Exception {
        String absent = "com.cratonvm.absent.NoSuchClass20260731";
        ClassLoader loader = RJdkFailure.class.getClassLoader();

        boolean threw = false;
        String message = "";
        try {
            Class.forName(absent);
        } catch (ClassNotFoundException e) {
            threw = true;
            message = e.getMessage();
        }
        check(threw, "Class.forName of an absent class must raise ClassNotFoundException");
        check(absent.equals(message), "CNFE message must be the class name: " + message);

        threw = false;
        try {
            Class.forName(absent, false, loader);
        } catch (ClassNotFoundException expected) {
            threw = true;
        }
        check(threw, "Class.forName(name, false, loader) must also throw");

        threw = false;
        try {
            loader.loadClass(absent);
        } catch (ClassNotFoundException expected) {
            threw = true;
        }
        check(threw, "ClassLoader.loadClass must throw ClassNotFoundException");

        // Same for an absent array element type and an absent nested class.
        threw = false;
        String arrayMessage = "";
        try {
            Class.forName("[L" + absent + ";");
        } catch (ClassNotFoundException expected) {
            threw = true;
            arrayMessage = expected.getMessage();
        }
        check(threw, "an array of an absent class must also fail");
        // L16 - HotSpot never hands an array descriptor to a class loader:
        // JVMS 5.3.3 creates an array class from its ELEMENT type, so
        // Class.forName strips the '[' itself and the name that reaches a
        // loader - and therefore the CNFE message - is the element's.
        check(absent.equals(arrayMessage),
                "an array CNFE must name the element, not the descriptor: " + arrayMessage);

        threw = false;
        arrayMessage = "";
        try {
            Class.forName("[[L" + absent + ";");
        } catch (ClassNotFoundException expected) {
            threw = true;
            arrayMessage = expected.getMessage();
        }
        check(threw, "a two-dimensional array of an absent class must also fail");
        check(absent.equals(arrayMessage),
                "every dimension is stripped before naming the element: " + arrayMessage);

        // The OTHER shape, and the reason the fix went into Class.forName and
        // not into the loader: ClassLoader.loadClass never resolves an array
        // form at all, so its CNFE names the descriptor it was given.
        threw = false;
        String loaderArrayMessage = "";
        try {
            loader.loadClass("[L" + absent + ";");
        } catch (ClassNotFoundException expected) {
            threw = true;
            loaderArrayMessage = expected.getMessage();
        }
        check(threw, "ClassLoader.loadClass of an array descriptor must throw");
        check(("[L" + absent + ";").equals(loaderArrayMessage),
                "loadClass names the descriptor it was asked for: " + loaderArrayMessage);

        // A malformed name is rejected too.
        threw = false;
        try {
            Class.forName("java/lang/String");
        } catch (ClassNotFoundException expected) {
            threw = true;
        }
        check(threw, "a slash-form name must not resolve");

        // Resources behave the same way: absent means null, not empty.
        check(loader.getResource("cratonvm/absent/resource.txt") == null,
                "an absent resource must be null");
        check(loader.getResourceAsStream("cratonvm/absent/resource.txt") == null,
                "an absent resource stream must be null");
        System.out.println("CK RJdkFailure missingClass=" + message);
    }

    static void linkageErrors() throws Throwable {
        // NoSuchMethodError: the caller's constant pool names a method that is
        // not on the (real, unmodified) Victim class.
        byte[] mm = bytesOf("RJdkFailure$MissingMethodCaller");
        check(patch(mm, "victimAbsentZero", "victimAbsentZerX") == 1,
                "expected exactly one occurrence of the method name to patch");
        Task badMethod = defineTask(mm);
        boolean threw = false;
        String kind = "";
        try {
            badMethod.run();
        } catch (NoSuchMethodError e) {
            threw = true;
            kind = "NoSuchMethodError";
            check(e.getMessage() != null && e.getMessage().contains("victimAbsentZerX"),
                    "NoSuchMethodError must name the missing method: " + e.getMessage());
        }
        check(threw, "a call to an absent method must raise NoSuchMethodError, got " + kind);

        // NoClassDefFoundError: the caller's constant pool names an absent class.
        byte[] mc = bytesOf("RJdkFailure$MissingClassCaller");
        check(patch(mc, "RJdkFailure$Victim", "RJdkFailure$VictiX") >= 1,
                "expected the class name to be patchable");
        Task badClass = defineTask(mc);
        threw = false;
        String kind2 = "";
        try {
            badClass.run();
        } catch (NoClassDefFoundError e) {
            threw = true;
            kind2 = "NoClassDefFoundError";
            check(e.getMessage() != null && e.getMessage().contains("VictiX"),
                    "NoClassDefFoundError must name the missing class: " + e.getMessage());
            check(e.getCause() == null || e.getCause() instanceof ClassNotFoundException,
                    "NCDFE cause must be a ClassNotFoundException when present");
        }
        check(threw, "a reference to an absent class must raise NoClassDefFoundError");

        // The UNPATCHED caller must still work -- proving the patch, not the
        // hidden-class mechanism, is what broke it.
        Task good = defineTask(bytesOf("RJdkFailure$MissingMethodCaller"));
        check("absent0".equals(good.run()), "the unpatched caller must still work");

        // Reflective misses use the CHECKED exception family instead.
        threw = false;
        try {
            Victim.class.getDeclaredMethod("noSuchMethodAtAll");
        } catch (NoSuchMethodException expected) {
            threw = true;
        }
        check(threw, "reflection uses NoSuchMethodException for a missing method");
        threw = false;
        try {
            Victim.class.getDeclaredField("noSuchFieldAtAll");
        } catch (NoSuchFieldException expected) {
            threw = true;
        }
        check(threw, "reflection uses NoSuchFieldException for a missing field");
        System.out.println("CK RJdkFailure linkage=" + kind + "," + kind2);
    }

    /** An ACC_NATIVE method with no binding must be an UnsatisfiedLinkError. */
    static native long missingNative(long x);

    static void missingNativeBinding() {
        boolean threw = false;
        try {
            missingNative(1L);
        } catch (UnsatisfiedLinkError e) {
            // The message names the method; it does NOT embed a path, so it is
            // safe to assert on the method name.
            threw = e.getMessage() == null || e.getMessage().contains("missingNative");
        }
        check(threw, "an unbound native must raise UnsatisfiedLinkError");

        threw = false;
        try {
            System.loadLibrary("cratonvm_absent_library_20260731");
        } catch (UnsatisfiedLinkError expected) {
            threw = true;
        }
        check(threw, "loading an absent library must raise UnsatisfiedLinkError");

        threw = false;
        try {
            missingNative(2L);
        } catch (UnsatisfiedLinkError expected) {
            threw = true;
        }
        check(threw, "the failure must be repeatable, not cached into a success");
        System.out.println("CK RJdkFailure missingNative=UnsatisfiedLinkError");
    }

    static void missingModule() throws Exception {
        check(ModuleLayer.boot().findModule("cratonvm.no.such.module").isEmpty(),
                "an absent module must not be found in the boot layer");
        check(ModuleLayer.boot().findModule("java.base").isPresent(), "java.base must be present");

        // A class from a module that is not in the boot layer resolves to a
        // plain ClassNotFoundException -- not a fabricated class.
        boolean threw = false;
        try {
            Class.forName("javafx.application.Application");
        } catch (ClassNotFoundException expected) {
            threw = true;
        }
        check(threw, "a class from an unresolved module must raise ClassNotFoundException");

        // ModuleFinder over an empty path must simply find nothing.
        check(java.lang.module.ModuleFinder.of().find("anything").isEmpty(),
                "an empty ModuleFinder must find nothing");
        check(java.lang.module.ModuleFinder.ofSystem().find("java.base").isPresent(),
                "the system finder must know java.base");
        check(java.lang.module.ModuleFinder.ofSystem().find("cratonvm.absent").isEmpty(),
                "the system finder must not invent a module");
        System.out.println("CK RJdkFailure missingModule=ok");
    }

    static void unsupportedPlatformService() throws Exception {
        // An unknown JCA algorithm.
        boolean threw = false;
        try {
            java.security.MessageDigest.getInstance("CRATONVM-NO-SUCH-DIGEST");
        } catch (NoSuchAlgorithmException expected) {
            threw = true;
        }
        check(threw, "an unknown digest must raise NoSuchAlgorithmException");
        threw = false;
        try {
            javax.crypto.Cipher.getInstance("CRATONVM-NO-SUCH-CIPHER");
        } catch (NoSuchAlgorithmException expected) {
            threw = true;
        }
        check(threw, "an unknown cipher must raise NoSuchAlgorithmException");
        threw = false;
        try {
            java.security.KeyFactory.getInstance("CRATONVM-NO-SUCH-KEYFACTORY");
        } catch (NoSuchAlgorithmException expected) {
            threw = true;
        }
        check(threw, "an unknown key factory must raise NoSuchAlgorithmException");

        // An unknown charset.
        threw = false;
        try {
            java.nio.charset.Charset.forName("CRATONVM-NO-SUCH-CHARSET");
        } catch (UnsupportedCharsetException expected) {
            threw = true;
        }
        check(threw, "an unknown charset must raise UnsupportedCharsetException");
        check(!java.nio.charset.Charset.isSupported("CRATONVM-NO-SUCH-CHARSET"),
                "isSupported must be false for an unknown charset");
        check(java.nio.charset.Charset.isSupported("UTF-8"), "UTF-8 must be supported");

        // An unknown file-attribute view. "basic" is mandated everywhere; the
        // POSIX/DOS split is platform-specific and is deliberately NOT asserted.
        Path tmp = Files.createTempFile("rjdkfailure", ".tmp");
        try {
            check(Files.getFileAttributeView(tmp,
                    java.nio.file.attribute.BasicFileAttributeView.class) != null,
                    "the basic attribute view is mandatory");
            check(java.nio.file.FileSystems.getDefault().supportedFileAttributeViews()
                    .contains("basic"), "supportedFileAttributeViews must contain basic");
            threw = false;
            try {
                Files.readAttributes(tmp, "cratonvmnosuchview:size");
            } catch (UnsupportedOperationException expected) {
                threw = true;
            }
            check(threw, "an unknown attribute view must raise UnsupportedOperationException");
        } finally {
            Files.deleteIfExists(tmp);
        }

        // An unknown time zone falls back to GMT rather than throwing -- that is
        // the spec, and a VM that throws here would be WRONG.
        check(java.util.TimeZone.getTimeZone("Cratonvm/Nowhere").getID().equals("GMT"),
                "an unknown TimeZone id must fall back to GMT");
        threw = false;
        try {
            java.time.ZoneId.of("Cratonvm/Nowhere");
        } catch (java.time.zone.ZoneRulesException expected) {
            threw = true;
        }
        check(threw, "ZoneId.of must reject an unknown zone");

        List<String> covered = new ArrayList<>(Arrays.asList(
                "digest", "cipher", "keyFactory", "charset", "attributeView", "zoneId"));
        Collections.sort(covered);
        System.out.println("CK RJdkFailure unsupported=" + covered);
    }

    public static void main(String[] args) throws Throwable {
        missingClass();
        linkageErrors();
        missingNativeBinding();
        missingModule();
        unsupportedPlatformService();
        System.out.println("CK RJdkFailure checks=" + checks);
        System.out.println("PASS RJdkFailure (" + checks + " checks)");
    }
}

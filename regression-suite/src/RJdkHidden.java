import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;

/**
 * JDK-only corpus: hidden classes --
 * {@link MethodHandles.Lookup#defineHiddenClass}, nestmate access.
 *
 * Per the contract (feature-designs/jdk-only-mode.md §1.6) a hidden class is
 * legitimately VM-defined and carries {@code ClassOrigin::HiddenClass}; it is
 * NOT a compatibility stub, so this must keep working under {@code --jdk-only}.
 *
 * The class bytes are the ones javac produced for {@link Payload}, read back
 * off the class path as a resource -- no bytecode assembler needed, and the
 * bytes are therefore real javac output for whichever {@code --release} the
 * suite compiled with.
 *
 * Determinism: a hidden class's {@link Class#getName()} carries an
 * address-derived {@code /0x...} suffix and is NEVER printed; only structural
 * predicates are.
 */
public class RJdkHidden {
    static int checks;

    /** Read by the hidden nestmate through direct (private) field access. */
    private static final int SECRET = 4242;

    private static String privateStatic(String in) {
        return "priv:" + in;
    }

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    /** The contract the hidden class is reached through. */
    public interface Callable {
        String call(String arg);
    }

    /**
     * Compiled by javac into {@code RJdkHidden$Payload.class}; those bytes are
     * then defined a SECOND time as a hidden class. Its private accesses to the
     * enclosing class compile to plain {@code getstatic}/{@code invokestatic}
     * (Java 11 nestmates), so they only link if the hidden class really joined
     * the nest.
     */
    public static class Payload implements Callable {
        public Payload() {
        }

        @Override
        public String call(String arg) {
            return privateStatic(arg) + "/" + SECRET;
        }

        public static int secretTimes(int n) {
            return SECRET * n;
        }
    }

    static byte[] payloadBytes() throws Exception {
        String resource = "RJdkHidden$Payload.class";
        try (InputStream in = RJdkHidden.class.getResourceAsStream(resource)) {
            check(in != null, "payload class bytes not found on the class path: " + resource);
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            byte[] buf = new byte[4096];
            int n;
            while ((n = in.read(buf)) > 0) {
                out.write(buf, 0, n);
            }
            byte[] b = out.toByteArray();
            check(b.length > 100, "payload class bytes too short: " + b.length);
            check((b[0] & 0xff) == 0xCA && (b[1] & 0xff) == 0xFE, "payload is not a class file");
            return b;
        }
    }

    static void defineNestmate() throws Throwable {
        byte[] bytes = payloadBytes();
        MethodHandles.Lookup hidden = MethodHandles.lookup()
                .defineHiddenClass(bytes, true, MethodHandles.Lookup.ClassOption.NESTMATE);
        Class<?> hc = hidden.lookupClass();

        check(hc.isHidden(), "defineHiddenClass must produce a hidden class");
        check(hc != Payload.class, "the hidden class must be distinct from the loaded Payload");
        check(hc.getName().contains("/0x"), "hidden class name shape");
        // NB: getSimpleName() on a hidden NESTED class raises
        // IncompatibleClassChangeError ("disagree on InnerClasses attribute") on
        // HotSpot -- the binary name prefix is the portable shape assertion.
        check(hc.getName().startsWith("RJdkHidden$Payload/0x"), "hidden binary name prefix");
        check(Callable.class.isAssignableFrom(hc), "hidden class implements Callable");
        check(hc.getClassLoader() == RJdkHidden.class.getClassLoader(),
                "hidden class shares the lookup class's loader");
        check(hc.getNestHost() == RJdkHidden.class,
                "NESTMATE must place the hidden class in the lookup class's nest, got "
                        + hc.getNestHost());

        // A hidden class is not findable by name, ever.
        boolean threw = false;
        try {
            Class.forName(hc.getName(), false, RJdkHidden.class.getClassLoader());
        } catch (ClassNotFoundException expected) {
            threw = true;
        }
        check(threw, "a hidden class must not be resolvable by name");

        // Instantiate it and call through the interface: this executes the
        // nestmate's private getstatic/invokestatic against RJdkHidden.
        MethodHandle ctor = hidden.findConstructor(hc, MethodType.methodType(void.class));
        Callable c = (Callable) ctor.invoke();
        check(c.call("x").equals("priv:x/4242"), "nestmate private access: " + c.call("x"));

        // ...and through a static method handle.
        MethodHandle st = hidden.findStatic(hc, "secretTimes",
                MethodType.methodType(int.class, int.class));
        check((int) st.invokeExact(2) == 8484, "hidden static invoke");

        // Reflection over the hidden class still works.
        Method m = hc.getMethod("call", String.class);
        check("priv:y/4242".equals(m.invoke(c, "y")), "reflective invoke on a hidden class");

        // Two definitions of the same bytes are two distinct runtime classes.
        Class<?> hc2 = MethodHandles.lookup()
                .defineHiddenClass(bytes, true, MethodHandles.Lookup.ClassOption.NESTMATE)
                .lookupClass();
        check(hc2 != hc, "each defineHiddenClass call defines a NEW class");
        check(!hc2.getName().equals(hc.getName()), "distinct hidden classes get distinct names");
        Callable c2 = (Callable) hidden.findConstructor(hc2, MethodType.methodType(void.class))
                .invoke();
        check(c2.call("z").equals("priv:z/4242"), "second hidden class works too");
        check(c2.getClass() != c.getClass(), "instances have distinct classes");

        System.out.println("CK RJdkHidden hidden=" + hc.isHidden()
                + " nestHostIsLookup=" + (hc.getNestHost() == RJdkHidden.class)
                + " namePrefix=" + hc.getName().startsWith("RJdkHidden$Payload/0x")
                + " call=" + c.call("x") + " static=" + (int) st.invokeExact(2));
    }

    /** Without NESTMATE the class is defined into its own nest and has no private access. */
    static void defineNonNestmate() throws Throwable {
        byte[] bytes = payloadBytes();
        Class<?> hc = MethodHandles.lookup().defineHiddenClass(bytes, false).lookupClass();
        check(hc.isHidden(), "non-nestmate hidden class");
        check(hc.getNestHost() == hc, "a non-nestmate hidden class is its own nest host");
        // The class is defined; the private access inside only fails when the
        // offending method is linked, so we assert the nest shape rather than
        // forcing an IllegalAccessError (whose exact site is unspecified).
        check(Callable.class.isAssignableFrom(hc), "non-nestmate still implements the interface");
        System.out.println("CK RJdkHidden nonNestmateOwnHost=" + (hc.getNestHost() == hc));
    }

    /** The nest itself must be introspectable and consistent. */
    static void nestShape() {
        check(Payload.class.getNestHost() == RJdkHidden.class, "Payload nest host");
        check(RJdkHidden.class.getNestHost() == RJdkHidden.class, "RJdkHidden is its own nest host");
        int members = RJdkHidden.class.getNestMembers().length;
        check(members >= 3, "nest members (host + Payload + Callable): " + members);
        check(RJdkHidden.class.isNestmateOf(Payload.class), "isNestmateOf");
        check(!RJdkHidden.class.isNestmateOf(String.class), "isNestmateOf(String) must be false");
        check(!RJdkHidden.class.isHidden() && !Payload.class.isHidden(),
                "ordinary classes are not hidden");
        System.out.println("CK RJdkHidden nestMembers=" + members);
    }

    public static void main(String[] args) throws Throwable {
        nestShape();
        defineNestmate();
        defineNonNestmate();
        System.out.println("CK RJdkHidden checks=" + checks);
        System.out.println("PASS RJdkHidden (" + checks + " checks)");
    }
}

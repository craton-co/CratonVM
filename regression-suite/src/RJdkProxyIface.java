import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandleProxies;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.WrongMethodTypeException;
import java.lang.reflect.UndeclaredThrowableException;
import java.util.ArrayList;
import java.util.List;

/**
 * JDK-only corpus: {@code MethodHandleProxies.asInterfaceInstance}, and the two
 * {@code ldc} constant-pool forms its generated proxy needs.
 *
 * <h2>Why this vector exists</h2>
 *
 * The 2026-08-12 stub census found {@code asInterfaceInstance} failing in BOTH
 * CratonVM modes, and differently in each:
 *
 * <ul>
 *   <li>compatible — {@code ClassCastException: java.lang.invoke.MethodHandle
 *       cannot be cast to Runnable}. The native shim
 *       ({@code lang_invoke.rs::register_p68_invoke_extras}) returns the
 *       METHOD HANDLE ITSELF as the "proxy", which is not an instance of the
 *       requested interface. That is a declared simplification, not a
 *       decoder bug.</li>
 *   <li>{@code --jdk-only} — {@code ClassFormatError: ldc: unsupported
 *       constant pool entry type at #26}. With the shim refused, the REAL
 *       {@code MethodHandleProxies} bytecode runs; it spins a proxy class
 *       whose {@code <init>} does {@code callerBoundTarget.asType(<MT>)} off
 *       an {@code ldc} of a {@code CONSTANT_MethodType}, and the interpreter
 *       refused that tag. (Reproduced by dumping the generated class with
 *       {@code -Djdk.invoke.MethodHandleProxies.dumpClassFiles} on HotSpot 25:
 *       {@code 15: ldc #27 // MethodType (Ljava/lang/String;)Ljava/lang/String;}
 *       — the index moves with the interface's descriptor, which is why the
 *       census saw #26 for {@code Runnable}.)</li>
 * </ul>
 *
 * <h2>What is asserted, and why NOT {@code != null}</h2>
 *
 * Two defects survived this year behind {@code != null} and length checks, so
 * nothing here is satisfied by a non-null stand-in:
 *
 * <ul>
 *   <li>the proxy must COMPUTE — {@code greet("bob")} is {@code "hi bob"} and
 *       {@code sub(10, 3)} is {@code 7} while {@code sub(3, 10)} is
 *       {@code -7}, so a handle bound to nothing, to the wrong member, or
 *       invoked with the arguments in the other order cannot answer;</li>
 *   <li>{@code wrapperInstanceTarget} must return the SAME handle object
 *       ({@code ==}), not an equal-looking one;</li>
 *   <li>{@code isWrapperInstance} carries its negative controls — a raw
 *       {@code MethodHandle}, a {@code String}, and {@code null} are NOT
 *       wrapper instances. The current compatible-mode shim answers
 *       {@code true} for the raw handle, so a blanket-true implementation
 *       fails here rather than passing;</li>
 *   <li>{@code Object} methods go through the proxy: {@code equals} is
 *       identity (two proxies over the SAME target are NOT equal),
 *       {@code hashCode} is the identity hash and is stable, {@code toString}
 *       is {@code Object}'s, and {@code getClass()} is a class that is neither
 *       the interface nor {@code MethodHandle};</li>
 *   <li>the generated proxy's exception table is exercised in both
 *       directions: a DECLARED checked exception propagates unchanged, an
 *       UNDECLARED one arrives wrapped in {@code UndeclaredThrowableException}
 *       with the original as its cause.</li>
 * </ul>
 *
 * <h2>The direct {@code ldc} vector</h2>
 *
 * {@code javac} never emits {@code ldc} of a {@code CONSTANT_MethodType} or a
 * {@code CONSTANT_MethodHandle} from ordinary source, so the last steps build
 * a two-method class with the {@code java.lang.classfile} API and define it
 * through {@code Lookup.defineClass}. It asks the decoder question directly:
 * if {@code ldcMethodType}/{@code ldcMethodHandle} are green and
 * {@code asInterfaceInstance} is red, the remaining defect is NOT in the
 * constant-pool decoder. All nine {@code CONSTANT_MethodHandle} reference
 * kinds are not reachable this way; {@code REF_invokeStatic} and
 * {@code REF_getStatic} are, and they cover both halves of Table 5.4.3.5-A
 * (the {@code Methodref} arm and the {@code Fieldref} arm).
 *
 * <p>Determinism: no identity hash VALUES are printed, no addresses, no
 * timing. The proxy's class name contains a JDK-chosen counter and suffix, so
 * only its SHAPE is asserted, never its text.
 *
 * <p>Structured as independent {@link #step steps} for the reason
 * {@code RJdkHandles} documents: a single straight-line run reports the first
 * failure and hides the rest, and this surface has several independent halves.
 */
public class RJdkProxyIface {
    static int checks;
    static int steps;
    static final List<String> failures = new ArrayList<>();

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    interface Body {
        void run() throws Throwable;
    }

    static void step(String name, Body body) {
        steps++;
        try {
            body.run();
        } catch (Throwable t) {
            failures.add(name);
            System.out.println("FAIL RJdkProxyIface step " + name + ": "
                    + t.getClass().getName() + ": " + t.getMessage());
        }
    }

    // ---- the functional interfaces the proxies are made for ---------------

    public interface Greeter {
        String greet(String who);
    }

    /** Non-commutative on purpose: it pins ARGUMENT ORDER, not just arity. */
    public interface Subtractor {
        int sub(int a, int b);
    }

    /** A void SAM: the generated body must `return` rather than `areturn`. */
    public interface Sink {
        void accept(String s);
    }

    /** Declares a checked exception, so the proxy gets a rethrow arm for it. */
    public interface Risky {
        void go() throws java.io.IOException;
    }

    /** Declares NOTHING, so a checked throw must be wrapped. */
    public interface Quiet {
        void go();
    }

    // ---- the targets --------------------------------------------------------

    public static String greet(String who) {
        return "hi " + who;
    }

    public static int sub(int a, int b) {
        return a - b;
    }

    public static final String SINK_MISS = "<unset>";
    static String sunk = SINK_MISS;

    public static void sink(String s) {
        sunk = "sank:" + s;
    }

    public static void throwsIo() throws java.io.IOException {
        throw new java.io.IOException("declared");
    }

    /** For the direct-ldc probe's REF_getStatic arm. */
    public static final String LDC_FIELD = "ldc-static-field";

    static MethodHandle greeterHandle() throws Throwable {
        return MethodHandles.lookup().findStatic(RJdkProxyIface.class, "greet",
                MethodType.methodType(String.class, String.class));
    }

    // ---- MethodHandleProxies -----------------------------------------------

    static void invokesTheTarget() throws Throwable {
        MethodHandle mh = greeterHandle();
        Greeter g = MethodHandleProxies.asInterfaceInstance(Greeter.class, mh);
        // The value only `greet` can produce. A stand-in with no invocable
        // body, or a handle bound to a different member, cannot answer it.
        check("hi bob".equals(g.greet("bob")), "proxy must invoke the target: got " + g.greet("bob"));
        check("hi ".equals(g.greet("")), "proxy must pass the argument through");

        MethodHandle sh = MethodHandles.lookup().findStatic(RJdkProxyIface.class, "sub",
                MethodType.methodType(int.class, int.class, int.class));
        Subtractor s = MethodHandleProxies.asInterfaceInstance(Subtractor.class, sh);
        check(s.sub(10, 3) == 7, "proxy must preserve argument ORDER: sub(10,3)");
        check(s.sub(3, 10) == -7, "proxy must preserve argument ORDER: sub(3,10)");
        System.out.println("CK RJdkProxyIface invoke ok");
    }

    static void voidSam() throws Throwable {
        sunk = SINK_MISS;
        MethodHandle mh = MethodHandles.lookup().findStatic(RJdkProxyIface.class, "sink",
                MethodType.methodType(void.class, String.class));
        Sink k = MethodHandleProxies.asInterfaceInstance(Sink.class, mh);
        k.accept("q");
        // The SIDE EFFECT is the observation: a void SAM has no return value
        // for a fabricated proxy to get accidentally right.
        check("sank:q".equals(sunk), "void SAM proxy must run the target, sunk=" + sunk);
        System.out.println("CK RJdkProxyIface voidsam ok");
    }

    static void identityAndClass() throws Throwable {
        MethodHandle mh = greeterHandle();
        Greeter a = MethodHandleProxies.asInterfaceInstance(Greeter.class, mh);
        Greeter b = MethodHandleProxies.asInterfaceInstance(Greeter.class, mh);

        check(Greeter.class.isInstance(a), "the proxy must be an instance of the interface");
        check(a.getClass() != Greeter.class, "the proxy's class is not the interface itself");
        check(!(a instanceof MethodHandle),
                "the proxy must not BE the MethodHandle (the compatible-mode shim's shape)");
        check(Greeter.class.isAssignableFrom(a.getClass()),
                "the proxy's class must implement the interface");
        // One hidden class per interface, many instances of it.
        check(a.getClass() == b.getClass(),
                "two proxies for the same interface share one implementation class");
        check(a != b, "each asInterfaceInstance call yields its own instance");
        System.out.println("CK RJdkProxyIface class ok interface=" + Greeter.class.getName()
                + " implementsIface=" + Greeter.class.isAssignableFrom(a.getClass()));
    }

    static void objectMethods() throws Throwable {
        MethodHandle mh = greeterHandle();
        Greeter a = MethodHandleProxies.asInterfaceInstance(Greeter.class, mh);
        Greeter b = MethodHandleProxies.asInterfaceInstance(Greeter.class, mh);

        // equals is IDENTITY: two proxies over the same target are distinct.
        check(a.equals(a), "proxy.equals(itself)");
        check(!a.equals(b), "two proxies over the same target must NOT be equal");
        check(!a.equals(null), "proxy.equals(null) is false");
        check(!a.equals("hi bob"), "proxy.equals(a String) is false");

        // hashCode is the identity hash, and is stable across calls.
        check(a.hashCode() == a.hashCode(), "proxy.hashCode() must be stable");
        check(a.hashCode() == System.identityHashCode(a),
                "proxy.hashCode() must be the identity hash");

        // toString is Object's: `<class name>@<hex hash>`. The hash is not
        // deterministic, so the SHAPE is what is asserted.
        String ts = a.toString();
        String expectedPrefix = a.getClass().getName() + "@";
        check(ts.startsWith(expectedPrefix),
                "proxy.toString() must be Object's, got " + ts);
        check(ts.equals(expectedPrefix + Integer.toHexString(a.hashCode())),
                "proxy.toString() must use the same hash it reports, got " + ts);
        System.out.println("CK RJdkProxyIface objectmethods ok distinct=" + (!a.equals(b)));
    }

    static void wrapperRoundTrip() throws Throwable {
        MethodHandle mh = greeterHandle();
        Greeter g = MethodHandleProxies.asInterfaceInstance(Greeter.class, mh);

        check(MethodHandleProxies.isWrapperInstance(g), "the proxy IS a wrapper instance");
        // The round trip is by IDENTITY. An equal-looking replacement handle
        // would satisfy a weaker check and would not be the round trip.
        check(MethodHandleProxies.wrapperInstanceTarget(g) == mh,
                "wrapperInstanceTarget must return the very handle that was passed in");
        check(MethodHandleProxies.wrapperInstanceType(g) == Greeter.class,
                "wrapperInstanceType must return the interface Class");
        // ...and the recovered target still computes.
        check("hi zed".equals((String) MethodHandleProxies.wrapperInstanceTarget(g).invoke("zed")),
                "the recovered target must still invoke");

        // NEGATIVE CONTROLS. Without these a blanket `true` passes the line
        // above -- which is exactly what the compatible-mode shim answers for
        // a raw MethodHandle.
        check(!MethodHandleProxies.isWrapperInstance(mh),
                "a raw MethodHandle is NOT a wrapper instance");
        check(!MethodHandleProxies.isWrapperInstance("hi bob"),
                "a String is NOT a wrapper instance");
        check(!MethodHandleProxies.isWrapperInstance(null),
                "null is NOT a wrapper instance");

        boolean threw = false;
        try {
            MethodHandle bogus = MethodHandleProxies.wrapperInstanceTarget("hi bob");
            check(bogus == null, "unreachable");
        } catch (IllegalArgumentException expected) {
            threw = true;
        }
        check(threw, "wrapperInstanceTarget on a non-wrapper must raise IllegalArgumentException");
        System.out.println("CK RJdkProxyIface wrapper ok type=" + MethodHandleProxies
                .wrapperInstanceType(g).getName());
    }

    static void exceptionTable() throws Throwable {
        MethodHandle io = MethodHandles.lookup().findStatic(RJdkProxyIface.class, "throwsIo",
                MethodType.methodType(void.class));

        // DECLARED: Risky.go() throws IOException, so it propagates unchanged.
        Risky r = MethodHandleProxies.asInterfaceInstance(Risky.class, io);
        boolean declared = false;
        try {
            r.go();
        } catch (java.io.IOException expected) {
            declared = "declared".equals(expected.getMessage());
        }
        check(declared, "a declared checked exception must propagate unchanged");

        // UNDECLARED: Quiet.go() throws nothing, so the same throw must be
        // wrapped -- and the cause must be the original.
        Quiet q = MethodHandleProxies.asInterfaceInstance(Quiet.class, io);
        boolean wrapped = false;
        try {
            q.go();
        } catch (UndeclaredThrowableException expected) {
            Throwable cause = expected.getCause();
            wrapped = cause instanceof java.io.IOException
                    && "declared".equals(cause.getMessage());
        }
        check(wrapped,
                "an undeclared checked exception must arrive as UndeclaredThrowableException");
        System.out.println("CK RJdkProxyIface exceptions ok");
    }

    static void refusals() throws Throwable {
        MethodHandle mh = greeterHandle();

        // Not an interface.
        boolean threw = false;
        try {
            Object bad = MethodHandleProxies.asInterfaceInstance(String.class, mh);
            check(bad == null, "unreachable");
        } catch (IllegalArgumentException expected) {
            threw = true;
        }
        check(threw, "asInterfaceInstance on a non-interface must raise IllegalArgumentException");

        // Wrong shape for the SAM: (String)String cannot become (int,int)int.
        threw = false;
        try {
            Subtractor bad = MethodHandleProxies.asInterfaceInstance(Subtractor.class, mh);
            check(bad == null, "unreachable");
        } catch (WrongMethodTypeException | ClassCastException expected) {
            threw = true;
        }
        check(threw, "asInterfaceInstance must refuse a handle the SAM cannot accept");

        // Null target.
        threw = false;
        try {
            Greeter bad = MethodHandleProxies.asInterfaceInstance(Greeter.class, null);
            check(bad == null, "unreachable");
        } catch (NullPointerException expected) {
            threw = true;
        }
        check(threw, "asInterfaceInstance(null target) must raise NullPointerException");
        System.out.println("CK RJdkProxyIface refusals ok");
    }

    // ---- the direct `ldc` vector -------------------------------------------

    static final String LDC_CLASS = "RJdkProxyIfaceLdcProbe";

    /**
     * A class with two static methods, each of which is a single {@code ldc}
     * of a constant-pool form {@code javac} does not emit:
     * {@code CONSTANT_MethodType} and {@code CONSTANT_MethodHandle}
     * (REF_invokeStatic), plus a third for the {@code Fieldref} half of
     * Table 5.4.3.5-A (REF_getStatic).
     */
    static byte[] ldcProbeBytes() {
        ClassDesc self = ClassDesc.of(LDC_CLASS);
        ClassDesc owner = ClassDesc.of("RJdkProxyIface");
        MethodTypeDesc greetDesc =
                MethodTypeDesc.ofDescriptor("(Ljava/lang/String;)Ljava/lang/String;");
        DirectMethodHandleDesc greetHandle = MethodHandleDesc.ofMethod(
                DirectMethodHandleDesc.Kind.STATIC, owner, "greet", greetDesc);
        DirectMethodHandleDesc fieldHandle = MethodHandleDesc.ofField(
                DirectMethodHandleDesc.Kind.STATIC_GETTER, owner, "LDC_FIELD",
                ConstantDescs.CD_String);
        int pubStatic = ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC;
        return ClassFile.of().build(self, clb -> {
            clb.withSuperclass(ConstantDescs.CD_Object);
            clb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_FINAL);
            clb.withMethodBody("mt", MethodTypeDesc.of(ConstantDescs.CD_MethodType), pubStatic,
                    cob -> cob.loadConstant(greetDesc).areturn());
            clb.withMethodBody("mh", MethodTypeDesc.of(ConstantDescs.CD_MethodHandle), pubStatic,
                    cob -> cob.loadConstant(greetHandle).areturn());
            clb.withMethodBody("fieldMh", MethodTypeDesc.of(ConstantDescs.CD_MethodHandle),
                    pubStatic, cob -> cob.loadConstant(fieldHandle).areturn());
        });
    }

    static Class<?> ldcProbeClass;

    static Class<?> ldcProbe() throws Throwable {
        if (ldcProbeClass == null) {
            ldcProbeClass = MethodHandles.lookup().defineClass(ldcProbeBytes());
        }
        return ldcProbeClass;
    }

    static void ldcMethodType() throws Throwable {
        MethodHandle mt = MethodHandles.lookup().findStatic(ldcProbe(), "mt",
                MethodType.methodType(MethodType.class));
        MethodType loaded = (MethodType) mt.invoke();
        // Not `!= null`: the loaded constant must be the type the descriptor
        // names, and it must be the INTERNED one, so `==` against a MethodType
        // built any other way holds (JVMS 5.4.3.5 / MethodType's javadoc).
        check(loaded == MethodType.methodType(String.class, String.class),
                "ldc CONSTANT_MethodType must yield the interned MethodType, got " + loaded);
        check(loaded.returnType() == String.class, "ldc MethodType return type");
        check(loaded.parameterCount() == 1 && loaded.parameterType(0) == String.class,
                "ldc MethodType parameter types");
        System.out.println("CK RJdkProxyIface ldc-methodtype " + loaded);
    }

    static void ldcMethodHandle() throws Throwable {
        MethodHandle f = MethodHandles.lookup().findStatic(ldcProbe(), "mh",
                MethodType.methodType(MethodHandle.class));
        MethodHandle loaded = (MethodHandle) f.invoke();
        // The handle must INVOKE, not merely exist.
        check("hi ldc".equals((String) loaded.invoke("ldc")),
                "ldc CONSTANT_MethodHandle (REF_invokeStatic) must yield an invocable handle");
        check(loaded.type().equals(MethodType.methodType(String.class, String.class)),
                "ldc MethodHandle type(), got " + loaded.type());

        MethodHandle g = MethodHandles.lookup().findStatic(ldcProbe(), "fieldMh",
                MethodType.methodType(MethodHandle.class));
        MethodHandle getter = (MethodHandle) g.invoke();
        check(LDC_FIELD.equals((String) getter.invoke()),
                "ldc CONSTANT_MethodHandle (REF_getStatic) must read the static field");
        System.out.println("CK RJdkProxyIface ldc-methodhandle " + loaded.type());
    }

    public static void main(String[] args) throws Throwable {
        step("invokesTheTarget", RJdkProxyIface::invokesTheTarget);
        step("voidSam", RJdkProxyIface::voidSam);
        step("identityAndClass", RJdkProxyIface::identityAndClass);
        step("objectMethods", RJdkProxyIface::objectMethods);
        step("wrapperRoundTrip", RJdkProxyIface::wrapperRoundTrip);
        step("exceptionTable", RJdkProxyIface::exceptionTable);
        step("refusals", RJdkProxyIface::refusals);
        step("ldcMethodType", RJdkProxyIface::ldcMethodType);
        step("ldcMethodHandle", RJdkProxyIface::ldcMethodHandle);
        System.out.println("CK RJdkProxyIface steps=" + steps);
        System.out.println("CK RJdkProxyIface checks=" + checks);
        if (!failures.isEmpty()) {
            throw new AssertionError(failures.size() + " of " + steps
                    + " steps failed: " + failures);
        }
        System.out.println("PASS RJdkProxyIface (" + checks + " checks, " + steps + " steps)");
    }
}

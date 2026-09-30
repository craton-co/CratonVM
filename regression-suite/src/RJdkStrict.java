import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.function.Function;

/**
 * JDK-only corpus: the MODE-DIVERGENT probes.
 *
 * Every other RJdk* vector asserts behaviour that is correct in BOTH
 * {@code --real-jdk} (compatible) and {@code --jdk-only} (strict) mode, so it
 * can be diffed against HotSpot in either. This one cannot: it probes exactly
 * the places where CratonVM's compatible mode deliberately FABRICATES a class,
 * and where strict mode must instead raise the specification-appropriate
 * error. HotSpot always behaves the strict way, so:
 *
 *   expected under --jdk-only : identical to HotSpot (this vector PASSES)
 *   expected under --real-jdk : DIVERGENT from HotSpot by design
 *
 * The runner therefore only schedules this class when CRATONVM_ARGS names
 * --jdk-only. Encoding "both expectations" for it means exactly that: the
 * strict expectation is HotSpot parity, and the compatible expectation is
 * "not asserted, because compatible mode is allowed to fabricate here".
 * See regression-suite/jdk-only-coverage.txt.
 *
 * Sources for each probe are the P0 rows of docs/jdk-only-runtime-services.md.
 */
public class RJdkStrict {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    /**
     * P0 "Synthetic class fallback policy": ClassManager::load_class falls back
     * to fabricating an empty class for these prefixes. Under --jdk-only every
     * one of them must be a ClassNotFoundException.
     */
    static final String[] ENTERPRISE_STUB_PROBES = {
        "org.jboss.logging.Logger",
        "org.jboss.modules.Module",
        "io.quarkus.runtime.Application",
        "io.smallrye.config.SmallRyeConfig",
        "org.jboss.as.server.Main",
        "io.quarkus.arc.Arc",
    };

    static void noFabricatedEnterpriseClasses() {
        ClassLoader loader = RJdkStrict.class.getClassLoader();
        List<String> fabricated = new ArrayList<>();
        for (String name : ENTERPRISE_STUB_PROBES) {
            boolean threw = false;
            try {
                Class<?> k = Class.forName(name, false, loader);
                fabricated.add(name + "->" + k.getName());
            } catch (ClassNotFoundException expected) {
                threw = true;
            } catch (NoClassDefFoundError expected) {
                // Also specification-appropriate for a resolution failure.
                threw = true;
            }
            check(threw, "strict mode fabricated a compatibility class for " + name);
        }
        check(fabricated.isEmpty(), "fabricated classes: " + fabricated);

        // The same through ClassLoader.loadClass, which is the path frameworks
        // actually use for capability probes.
        for (String name : ENTERPRISE_STUB_PROBES) {
            boolean threw = false;
            try {
                loader.loadClass(name);
            } catch (ClassNotFoundException expected) {
                threw = true;
            }
            check(threw, "loadClass fabricated a compatibility class for " + name);
        }
        System.out.println("CK RJdkStrict enterpriseStubs=0 probed="
                + ENTERPRISE_STUB_PROBES.length);
    }

    /**
     * P0 "Function.identity()": CratonVM carries a dedicated
     * java/util/function/Function$Identity stand-in with a hand-written field
     * table. Under --jdk-only the value must be a real generated lambda.
     */
    static void functionIdentityIsNotAStandIn() {
        Function<String, String> id = Function.identity();
        String name = id.getClass().getName();
        check(!name.equals("java.util.function.Function$Identity"),
                "Function.identity() returned the fabricated stand-in: " + name);
        // NB: a real generated lambda IS named after its defining class, so the
        // name legitimately begins "java.util.function.Function$$Lambda...".
        // What must not appear is the fabricated "$Identity" stand-in.
        check(!name.endsWith("$Identity"),
                "Function.identity() must not be an $Identity stand-in: " + name);
        check(name.contains("$$Lambda"),
                "Function.identity() must be a generated lambda class, got: " + name);
        check(id.getClass().isSynthetic(), "the identity lambda class must be synthetic");
        String s = "ref";
        check(id.apply(s) == s, "identity must return the same reference");

        // BEHAVIOURAL parity, which is what closure rule 3 actually asks for:
        // "executed from real bytecode (the native is deleted from the strict
        // path)". Registry-dump evidence for the deletion: the four
        // Function$Identity registrations present in the default registry are
        // ABSENT under --jdk-only. These rows are the other half — proof that
        // what answers instead is right.
        //
        // NOT asserted here, deliberately: andThen(null)/compose(null) must
        // throw NullPointerException, and under --jdk-only they do — but this
        // vector is scheduled in BOTH modes by SUITE=all, and in COMPATIBLE
        // mode they answer `no-throw`. Asserting them here turns a
        // compatible-mode defect into a red strict-mode vector, which is a
        // different claim from the one this vector makes. The defect is real
        // and is recorded separately (G84-1 N1) rather than hidden; it is the
        // sharpest available evidence that a stand-in is answering, so it
        // belongs in a compatible-mode vector, not this one.
        check("y!".equals(id.andThen((String v) -> v + "!").apply("y")),
                "identity.andThen must compose after");
        check("z?".equals(id.compose((String v) -> v + "?").apply("z")),
                "identity.compose must compose before");
        check(id.apply(null) == null, "identity must carry null through");
        check("u".equals(java.util.function.UnaryOperator.identity().apply("u")),
                "UnaryOperator.identity shares the contract");
        check(java.util.stream.Stream.of("p", "q").map(Function.identity())
                        .collect(java.util.stream.Collectors.toList()).equals(
                                java.util.List.of("p", "q")),
                "identity through a stream map");
        check(java.util.stream.Stream.of("a", "bb")
                        .collect(java.util.stream.Collectors.toMap(Function.identity(),
                                String::length))
                        .equals(java.util.Map.of("a", 1, "bb", 2)),
                "identity as a toMap key mapper — the commonest real use");

        // The stand-in class must not be loadable by name either.
        boolean threw = false;
        try {
            Class.forName("java.util.function.Function$Identity", false,
                    Function.class.getClassLoader());
        } catch (ClassNotFoundException expected) {
            threw = true;
        }
        check(threw, "java.util.function.Function$Identity must not exist");
        System.out.println("CK RJdkStrict identityIsLambda=true");
    }

    /**
     * P1 "ProcessHandle": is_native_backed_jdk_stub explicitly allows
     * java/lang/ProcessHandle and java/lang/ProcessHandle$Info to be fabricated
     * with a hand-written method table. Real boot bytes have a specific,
     * checkable shape that a hand-written table does not reproduce.
     */
    static void processHandleHasRealBytes() {
        check(ProcessHandle.class.isInterface(), "ProcessHandle must be an interface");
        check(ProcessHandle.Info.class.isInterface(), "ProcessHandle.Info must be an interface");
        check(Comparable.class.isAssignableFrom(ProcessHandle.class),
                "ProcessHandle extends Comparable");
        check(ProcessHandle.class.getModule().getName().equals("java.base"),
                "ProcessHandle must belong to java.base");
        check(ProcessHandle.class.getClassLoader() == null,
                "ProcessHandle must be defined to the boot loader");

        List<String> methods = new ArrayList<>();
        for (Method m : ProcessHandle.class.getDeclaredMethods()) {
            if (Modifier.isPublic(m.getModifiers()) && !m.isSynthetic()) {
                methods.add(m.getName());
            }
        }
        Collections.sort(methods);
        for (String required : new String[] { "allProcesses", "children", "compareTo",
            "current", "descendants", "destroy", "destroyForcibly", "info", "isAlive",
            "of", "onExit", "parent", "pid", "supportsNormalTermination" }) {
            check(methods.contains(required), "ProcessHandle is missing " + required
                    + "; declared: " + methods);
        }

        List<String> infoMethods = new ArrayList<>();
        for (Method m : ProcessHandle.Info.class.getDeclaredMethods()) {
            infoMethods.add(m.getName());
        }
        Collections.sort(infoMethods);
        check(infoMethods.equals(Arrays.asList("arguments", "command", "commandLine",
                "startInstant", "totalCpuDuration", "user")),
                "ProcessHandle.Info surface: " + infoMethods);
        System.out.println("CK RJdkStrict processHandleInfo=" + infoMethods);
    }

    /**
     * P0 "Native-first dispatch": concrete bytecode must win over a registered
     * compatibility native. A user subclass overriding a java.util method is
     * the cheapest way to see it: if a native shim answers instead of the
     * override, the wrong value comes back.
     */
    static void concreteBytecodeWins() {
        List<String> l = new CountingList();
        l.add("a");
        l.add("b");
        check(((CountingList) l).addCalls == 2,
                "the user override of add() must run, not a native shim");
        check(l.size() == 2, "the superclass state must still be correct");
        check(((CountingList) l).sizeCalls >= 1, "the user override of size() must run");

        java.util.Map<String, String> m = new CountingMap();
        m.put("k", "v");
        check(((CountingMap) m).putCalls == 1, "the user override of put() must run");
        check("v".equals(m.get("k")), "the map still works through the override");
        System.out.println("CK RJdkStrict overrideAdd=" + ((CountingList) l).addCalls
                + " overridePut=" + ((CountingMap) m).putCalls);
    }

    static final class CountingList extends java.util.ArrayList<String> {
        private static final long serialVersionUID = 1L;
        int addCalls;
        int sizeCalls;

        @Override
        public boolean add(String s) {
            addCalls++;
            return super.add(s);
        }

        @Override
        public int size() {
            sizeCalls++;
            return super.size();
        }
    }

    static final class CountingMap extends java.util.HashMap<String, String> {
        private static final long serialVersionUID = 1L;
        int putCalls;

        @Override
        public String put(String k, String v) {
            putCalls++;
            return super.put(k, v);
        }
    }

    /** Legitimately generated classes must still be allowed in strict mode. */
    static void generatedClassesStillAllowed() throws Throwable {
        // Arrays.
        check(int[].class.isArray() && int[].class.getComponentType() == int.class, "array class");
        check(java.lang.reflect.Array.newInstance(String.class, 2, 3).getClass()
                .getName().equals("[[Ljava.lang.String;"), "multi-dim array class");

        // Lambda.
        Runnable r = () -> { };
        check(r.getClass().getName().contains("$$Lambda"), "lambda class");

        // Proxy.
        Object p = java.lang.reflect.Proxy.newProxyInstance(
                RJdkStrict.class.getClassLoader(), new Class<?>[] { Runnable.class },
                (proxy, method, args) -> null);
        check(java.lang.reflect.Proxy.isProxyClass(p.getClass()), "proxy class");

        // Hidden class.
        byte[] bytes;
        try (java.io.InputStream in = RJdkStrict.class
                .getResourceAsStream("RJdkStrict$CountingMap.class")) {
            check(in != null, "own class bytes readable");
            java.io.ByteArrayOutputStream out = new java.io.ByteArrayOutputStream();
            byte[] buf = new byte[4096];
            int n;
            while ((n = in.read(buf)) > 0) {
                out.write(buf, 0, n);
            }
            bytes = out.toByteArray();
        }
        Class<?> hidden = java.lang.invoke.MethodHandles.lookup()
                .defineHiddenClass(bytes, true,
                        java.lang.invoke.MethodHandles.Lookup.ClassOption.NESTMATE)
                .lookupClass();
        check(hidden.isHidden(), "hidden class still definable under --jdk-only");

        // Reflection accessor generation (past the inflation threshold).
        Method size = java.util.ArrayList.class.getDeclaredMethod("size");
        java.util.ArrayList<String> list = new java.util.ArrayList<>();
        list.add("x");
        int acc = 0;
        for (int i = 0; i < 100; i++) {
            acc += (Integer) size.invoke(list);
        }
        check(acc == 100, "reflection accessors must keep working: " + acc);
        System.out.println("CK RJdkStrict generated=array,lambda,proxy,hidden,accessor");
    }

    /**
     * P0 "Native <clinit> over a real enum": a real JDK enum's constants must be
     * whatever its OWN declared fields say they are. CratonVM registers a native
     * <clinit> for java.lang.StackWalker$Option, and a registered native beats
     * real bytecode, so that native — not javac's initialiser — decides what the
     * constants are. W7-93.
     *
     * Deliberately NOT written as "Option has 4 constants": DROP_METHOD_INFO
     * arrived in JDK 22 and a hard-coded count is the exact mistake being
     * asserted against. Every check below is a SELF-CONSISTENCY check between
     * the class's declared static fields and what values()/valueOf()/
     * getEnumConstants() report, so it holds on any JDK and on HotSpot.
     */
    @SuppressWarnings({ "unchecked", "rawtypes" })
    static void realEnumsAreSelfConsistent() throws Throwable {
        for (Class<?> k : new Class<?>[] {
            java.lang.StackWalker.Option.class,
            java.time.DayOfWeek.class,
            java.nio.file.StandardOpenOption.class,
            java.lang.annotation.RetentionPolicy.class,
            java.util.concurrent.TimeUnit.class,
            Thread.State.class,
        }) {
            check(k.isEnum(), k.getName() + " must be an enum");

            // The declared constants, in declaration order == ordinal order.
            List<String> declared = new ArrayList<>();
            for (java.lang.reflect.Field f : k.getDeclaredFields()) {
                if (Modifier.isStatic(f.getModifiers()) && f.getType() == k) {
                    declared.add(f.getName());
                }
            }
            check(!declared.isEmpty(), k.getName() + " declares no enum constants");

            Object[] values = (Object[]) k.getMethod("values").invoke(null);
            check(values.length == declared.size(), k.getName() + ".values() has "
                    + values.length + " entries but the class declares "
                    + declared.size() + " constants " + declared);

            Object[] shared = k.getEnumConstants();
            check(shared != null && shared.length == declared.size(),
                    k.getName() + ".getEnumConstants() disagrees with the declared fields");

            java.lang.reflect.Method valueOf = k.getMethod("valueOf", String.class);
            for (int i = 0; i < declared.size(); i++) {
                String n = declared.get(i);
                java.lang.reflect.Field f = k.getDeclaredField(n);
                f.setAccessible(true);
                Object c = f.get(null);
                check(c != null, k.getName() + "." + n + " reads back null");
                // A non-null constant that never ran Enum.<init>(String,int) is
                // the harder half of this defect: every null-check passes while
                // name() is null and ordinal() is 0 for every constant.
                check(n.equals(((Enum<?>) c).name()),
                        k.getName() + "." + n + " has name() = " + ((Enum<?>) c).name());
                check(((Enum<?>) c).ordinal() == i,
                        k.getName() + "." + n + " has ordinal() = " + ((Enum<?>) c).ordinal()
                                + ", expected " + i);
                check(values[i] == c, k.getName() + ".values()[" + i + "] is not == " + n);
                check(shared[i] == c, k.getName() + ".getEnumConstants()[" + i
                        + "] is not == " + n);
                check(valueOf.invoke(null, n) == c,
                        k.getName() + ".valueOf(\"" + n + "\") is not == the constant");
                check(c.toString() != null,
                        k.getName() + "." + n + ".toString() must not be null");
            }

            // The shape that took down the real JCA: JceSecurityManager.<clinit>
            // ends in Set.of(Option.DROP_METHOD_INFO, Option.RETAIN_CLASS_REFERENCE),
            // and ImmutableCollections$Set12.<init> NPEs on a null element.
            check(java.util.Set.of(values).size() == declared.size(),
                    "Set.of(" + k.getName() + ".values()) must hold every constant");
            java.util.EnumSet<?> es = java.util.EnumSet.allOf(k.asSubclass(Enum.class));
            check(es.size() == declared.size(),
                    "EnumSet.allOf(" + k.getName() + ") = " + es.size()
                            + ", expected " + declared.size());
        }

        // StackWalker.getInstance(Set) is the call the JCA path actually makes.
        check(StackWalker.getInstance(java.util.Set.of(
                StackWalker.Option.RETAIN_CLASS_REFERENCE)) != null,
                "StackWalker.getInstance(Set) must return a walker");

        // A SECOND, different producer of the same defect: Thread$State's
        // values()/valueOf() are shadowed by natives that ran on the real-JDK
        // boot path (unlike Option, whose <clinit> is the shadowed method), so
        // the class's own constants were correct while values() handed back a
        // fresh instance per call. The loop above catches that, but only these
        // three shapes say WHY it matters, and each is independently green on
        // HotSpot.
        //
        // 1. values() is a defensive copy of $VALUES: a FRESH array whose
        //    ELEMENTS are stable. Minting fails the second half only.
        Thread.State[] v1 = Thread.State.values();
        Thread.State[] v2 = Thread.State.values();
        check(v1 != v2, "Thread.State.values() must return a fresh array per call");
        for (int i = 0; i < v1.length; i++) {
            check(v1[i] == v2[i],
                    "Thread.State.values()[" + i + "] must be the same object across calls");
        }
        // 2. The live producer: a native computes the current thread's state
        //    from the VM thread registry. It must return the class's own
        //    constant, because `getState() == State.RUNNABLE` is what every
        //    caller writes.
        Thread.State self = Thread.currentThread().getState();
        check(self == Thread.State.RUNNABLE,
                "Thread.currentThread().getState() must be == Thread.State.RUNNABLE, got " + self);
        check(Arrays.asList(Thread.State.values()).contains(self),
                "getState() must return one of Thread.State.values()");
        check(Thread.State.valueOf(self.name()) == self,
                "Thread.State.valueOf(getState().name()) must be == getState()");
        // 3. The ordinal half, kept honest about what it can see: MEASURED, an
        //    enum switch still selects the right arm for a MINTED constant,
        //    because javac's $SwitchMap is indexed by ordinal(), not identity.
        //    So this is a guard on ordinal/declaration agreement, NOT a second
        //    detector for the identity defect — checks 1 and 2 are the ones
        //    that fail when values() mints.
        check("NEW".equals(stateName(Thread.State.values()[0])),
                "switch over Thread.State.values()[0] must select NEW, got "
                        + stateName(Thread.State.values()[0]));
        check("TERMINATED".equals(stateName(Thread.State.valueOf("TERMINATED"))),
                "switch over Thread.State.valueOf(\"TERMINATED\") must select TERMINATED, got "
                        + stateName(Thread.State.valueOf("TERMINATED")));
        System.out.println("CK RJdkStrict enumSelfConsistent=6");
    }

    /** Forces the compiler to emit an enum switch (a $SwitchMap + tableswitch). */
    static String stateName(Thread.State s) {
        switch (s) {
            case NEW: return "NEW";
            case RUNNABLE: return "RUNNABLE";
            case BLOCKED: return "BLOCKED";
            case WAITING: return "WAITING";
            case TIMED_WAITING: return "TIMED_WAITING";
            case TERMINATED: return "TERMINATED";
            default: return "?";
        }
    }

    public static void main(String[] args) throws Throwable {
        noFabricatedEnterpriseClasses();
        functionIdentityIsNotAStandIn();
        processHandleHasRealBytes();
        concreteBytecodeWins();
        realEnumsAreSelfConsistent();
        generatedClassesStillAllowed();
        System.out.println("CK RJdkStrict checks=" + checks);
        System.out.println("PASS RJdkStrict (" + checks + " checks)");
    }
}

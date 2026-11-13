// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

// j27 wave 5 (lane PROXY): java.lang.reflect.Proxy behaviour while the
// ClassFile-API generator and the proxies' own calls warm into the JIT, and
// the interface-site shape of the class-set splice
// (CRATONVM_JIT_IR_CHA_INTERFACE_CLASS_SET_SPLICE=1, opt-in).
// Page: docs/feature-designs/native-dispatch-architecture.md
//
// Rows:
//   create        one proxy: its class's shape (final, public, Proxy super,
//                 interfaces in order, a named dynamic module, jdk.proxy*);
//   invoke        calls through the InvocationHandler: values, method names,
//                 declaring classes (Object's three methods come from Object);
//   default       a default method through InvocationHandler.invokeDefault;
//   declared      a declared checked exception passes through unwrapped;
//   undeclared    an undeclared checked one is wrapped in
//                 UndeclaredThrowableException (message null, cause the
//                 original);
//   runtime/error unchecked exceptions pass through;
//   nullprim/cce  a null for a primitive return, a wrong reference type;
//   invoke-warm   100 000 handler calls (the proxy's own body compiled);
//   undeclared-warm 20 000 wrapped throws from the compiled proxy body;
//   define        600 proxies through fresh loaders (the ClassFile API
//                 generator compiled), one call through each;
//   iface-index   the AbstractPoolEntry.index() shape: an interface call over
//                 six classes inheriting one body from a base that does not
//                 implement the interface, then a late class that overrides it.
//
//   java -cp <dir> J27ProxyRows > hs.txt
//   cratonvm --cp <dir> J27ProxyRows | diff - hs.txt
//     x {default, CRATONVM_JIT_IR_CHA_INTERFACE_CLASS_SET_SPLICE=1}
//     x {--real-jdk, --compatible}
//   engagement (switch on): CRATONVM_DBG_JITC=1 2>&1 | grep 'ir-cha-class-set J27ProxyRows\$Entry.index'
//   timing (optional): -Dj27.time=true adds `time ...` lines (not facts).
//
// Expected HotSpot 25 output (facts):
//   fact create proxy=true mods=public final super=java.lang.reflect.Proxy ifaces=J27ProxyRows$Greeter,java.lang.Runnable named=true pkg=true handler=true
//   fact invoke greet=hi x add=5 name=N calls=greet,add,name decl=J27ProxyRows$Greeter
//   fact object toString=H hashCode=42 equals=true decl=java.lang.Object,java.lang.Object,java.lang.Object
//   fact default isDefault=true value=default N
//   fact declared java.io.IOException:io
//   fact undeclared java.lang.reflect.UndeclaredThrowableException msg=null cause=java.lang.Exception:chk same=true
//   fact runtime java.lang.IllegalStateException:rt
//   fact error java.lang.AssertionError:err
//   fact nullprim java.lang.NullPointerException
//   fact cce java.lang.ClassCastException
//   fact invoke-warm sum=5000050000
//   fact undeclared-warm caught=20000 causes=20000
//   fact define n=600 distinct=600 sameLoader=true allProxy=true sum=179700
//   fact iface-index warm=1050000 late=721000
import java.io.IOException;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.lang.reflect.Proxy;
import java.lang.reflect.UndeclaredThrowableException;
import java.util.ArrayList;
import java.util.IdentityHashMap;
import java.util.List;

public class J27ProxyRows {
    public interface Greeter {
        String greet(String who);

        int add(int a, int b);

        String name();

        default String hello() {
            return "default " + name();
        }

        void io() throws IOException;

        void plain();
    }

    /** 0 checked, 1 runtime, 2 error: what `plain()` throws. */
    static int plainMode;
    /** `add` answers null (a primitive return). */
    static boolean addNull;
    /** `greet` answers an Integer (a wrong reference type). */
    static boolean greetWrong;
    static final List<String> CALLS = new ArrayList<>();
    static final List<String> DECLS = new ArrayList<>();
    static boolean record;

    static final class H implements InvocationHandler {
        @Override
        public Object invoke(Object proxy, Method m, Object[] args) throws Throwable {
            if (record) {
                CALLS.add(m.getName());
                DECLS.add(m.getDeclaringClass().getName());
            }
            switch (m.getName()) {
                case "greet":
                    return greetWrong ? (Object) Integer.valueOf(1) : "hi " + args[0];
                case "add":
                    return addNull ? null : (Integer) args[0] + (Integer) args[1];
                case "name":
                    return "N";
                case "hello":
                    return InvocationHandler.invokeDefault(proxy, m, args);
                case "io":
                    throw new IOException("io");
                case "plain":
                    if (plainMode == 0) {
                        throw new Exception("chk");
                    }
                    if (plainMode == 1) {
                        throw new IllegalStateException("rt");
                    }
                    throw new AssertionError("err");
                case "hashCode":
                    return 42;
                case "equals":
                    return proxy == args[0];
                case "toString":
                    return "H";
                default:
                    throw new IllegalStateException("unexpected " + m.getName());
            }
        }
    }

    static final class Loader extends ClassLoader {
        Loader(ClassLoader parent) {
            super(parent);
        }
    }

    static String thrown(Throwable t) {
        return t.getClass().getName() + ":" + t.getMessage();
    }

    static Greeter proxy(ClassLoader loader, InvocationHandler h, Class<?>... ifaces) {
        return (Greeter) Proxy.newProxyInstance(loader, ifaces, h);
    }

    static void create(InvocationHandler h) {
        Greeter g = proxy(J27ProxyRows.class.getClassLoader(), h, Greeter.class, Runnable.class);
        Class<?> c = g.getClass();
        StringBuilder ifaces = new StringBuilder();
        for (Class<?> i : c.getInterfaces()) {
            ifaces.append(ifaces.length() == 0 ? "" : ",").append(i.getName());
        }
        System.out.println("fact create proxy=" + Proxy.isProxyClass(c)
                + " mods=" + Modifier.toString(c.getModifiers())
                + " super=" + c.getSuperclass().getName()
                + " ifaces=" + ifaces
                + " named=" + c.getModule().isNamed()
                + " pkg=" + c.getPackageName().startsWith("jdk.proxy")
                + " handler=" + (Proxy.getInvocationHandler(g) == h));
    }

    static void invoke(Greeter g) {
        record = true;
        String greet = g.greet("x");
        int add = g.add(2, 3);
        String name = g.name();
        record = false;
        System.out.println("fact invoke greet=" + greet + " add=" + add + " name=" + name
                + " calls=" + String.join(",", CALLS) + " decl=" + DECLS.get(0));
        CALLS.clear();
        DECLS.clear();
        record = true;
        String s = g.toString();
        int hash = g.hashCode();
        boolean eq = g.equals(g);
        record = false;
        System.out.println("fact object toString=" + s + " hashCode=" + hash + " equals=" + eq
                + " decl=" + String.join(",", DECLS));
        CALLS.clear();
        DECLS.clear();
    }

    static void defaults(Greeter g) throws Exception {
        boolean isDefault = Greeter.class.getMethod("hello").isDefault();
        System.out.println("fact default isDefault=" + isDefault + " value=" + g.hello());
    }

    static void exceptions(Greeter g) {
        try {
            g.io();
            System.out.println("fact declared none");
        } catch (IOException e) {
            System.out.println("fact declared " + thrown(e));
        }
        plainMode = 0;
        try {
            g.plain();
            System.out.println("fact undeclared none");
        } catch (UndeclaredThrowableException e) {
            System.out.println("fact undeclared " + e.getClass().getName() + " msg=" + e.getMessage()
                    + " cause=" + thrown(e.getCause())
                    + " same=" + (e.getUndeclaredThrowable() == e.getCause()));
        }
        plainMode = 1;
        try {
            g.plain();
            System.out.println("fact runtime none");
        } catch (RuntimeException e) {
            System.out.println("fact runtime " + thrown(e));
        }
        plainMode = 2;
        try {
            g.plain();
            System.out.println("fact error none");
        } catch (Error e) {
            System.out.println("fact error " + thrown(e));
        }
        addNull = true;
        try {
            System.out.println("fact nullprim none " + g.add(1, 1));
        } catch (RuntimeException e) {
            System.out.println("fact nullprim " + e.getClass().getName());
        }
        addNull = false;
        greetWrong = true;
        try {
            System.out.println("fact cce none " + g.greet("y"));
        } catch (RuntimeException e) {
            System.out.println("fact cce " + e.getClass().getName());
        }
        greetWrong = false;
    }

    static long invokeWarm(Greeter g, int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += g.add(i, 1);
        }
        return sum;
    }

    static void undeclaredWarm(Greeter g, int n) {
        plainMode = 0;
        int caught = 0;
        int causes = 0;
        for (int i = 0; i < n; i++) {
            try {
                g.plain();
            } catch (UndeclaredThrowableException e) {
                caught++;
                if ("chk".equals(e.getCause().getMessage())) {
                    causes++;
                }
            }
        }
        System.out.println("fact undeclared-warm caught=" + caught + " causes=" + causes);
    }

    static void define(InvocationHandler h, int n, boolean time) {
        IdentityHashMap<Class<?>, Boolean> classes = new IdentityHashMap<>();
        boolean sameLoader = true;
        boolean allProxy = true;
        long sum = 0;
        long t0 = System.nanoTime();
        for (int k = 0; k < n; k++) {
            Loader loader = new Loader(J27ProxyRows.class.getClassLoader());
            Greeter g = proxy(loader, h, Greeter.class);
            Class<?> c = g.getClass();
            classes.put(c, Boolean.TRUE);
            sameLoader &= c.getClassLoader() == loader;
            allProxy &= Proxy.isProxyClass(c);
            sum += g.add(k, 0);
        }
        long t1 = System.nanoTime();
        System.out.println("fact define n=" + n + " distinct=" + classes.size()
                + " sameLoader=" + sameLoader + " allProxy=" + allProxy + " sum=" + sum);
        if (time) {
            System.out.println("time define-us-per=" + ((t1 - t0) / 1000 / n));
        }
    }

    // ---- the AbstractPoolEntry.index() shape -------------------------------

    public interface Entry {
        int index();
    }

    /** Declares the body, does NOT implement Entry (as AbstractPoolEntry). */
    abstract static class AbstractEntry {
        final int idx;

        AbstractEntry(int idx) {
            this.idx = idx;
        }

        public int index() {
            return idx;
        }
    }

    static final class E0 extends AbstractEntry implements Entry { E0(int i) { super(i); } }
    static final class E1 extends AbstractEntry implements Entry { E1(int i) { super(i); } }
    static final class E2 extends AbstractEntry implements Entry { E2(int i) { super(i); } }
    static final class E3 extends AbstractEntry implements Entry { E3(int i) { super(i); } }
    static final class E4 extends AbstractEntry implements Entry { E4(int i) { super(i); } }
    static final class E5 extends AbstractEntry implements Entry { E5(int i) { super(i); } }

    /** First loaded after the warm loop: overrides the inherited body. */
    static final class LateOverride extends AbstractEntry implements Entry {
        LateOverride(int i) {
            super(i);
        }

        @Override
        public int index() {
            return idx * 100;
        }
    }

    static long sumIndex(Entry[] entries, int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += entries[i % entries.length].index();
        }
        return sum;
    }

    static void ifaceIndex() {
        Entry[] six = {new E0(1), new E1(2), new E2(3), new E3(4), new E4(5), new E5(6)};
        long warm = 0;
        for (int rep = 0; rep < 10; rep++) {
            warm = sumIndex(six, 300_000);
        }
        Entry[] seven = new Entry[7];
        System.arraycopy(six, 0, seven, 0, 6);
        seven[6] = new LateOverride(7);
        long late = sumIndex(seven, 7_000);
        System.out.println("fact iface-index warm=" + warm + " late=" + late);
    }

    public static void main(String[] args) throws Exception {
        boolean time = Boolean.getBoolean("j27.time");
        H h = new H();
        create(h);
        Greeter g = proxy(J27ProxyRows.class.getClassLoader(), h, Greeter.class);
        invoke(g);
        defaults(g);
        exceptions(g);
        long sum = 0;
        for (int rep = 0; rep < 5; rep++) {
            sum = invokeWarm(g, 100_000);
        }
        System.out.println("fact invoke-warm sum=" + sum);
        undeclaredWarm(g, 20_000);
        define(h, 600, time);
        ifaceIndex();
    }
}

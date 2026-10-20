// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 50, lane L3 -- the evidence the proposal
// docs/internal/fixed-bugs/interpreter-i49-L3-proposal-one-caller-class-rule-over-the-walkers-capture-DONE-20261128.md
// asks for: `StackWalker.getCallerClass()` (and the JVM's
// `Reflection.getCallerClass`, through `MethodHandles.lookup()` and
// `Class.forName`) made from frames a name list answers differently from
// HotSpot's rule (JDK 25 `StackStreamFactory.CallerClassFinder`: skip hidden
// frames, `Method`, `Constructor`, `MethodAccessor` / `ConstructorAccessor`
// implementations and every `java.lang.invoke` class; throw
// `IllegalCallerException` when there is no caller frame).
//
// Rows (the first is repeated as the last, for a cold-VM gap):
//   direct              -- `who()` called from `main`;
//   method-invoke       -- `who()` called through `Method.invoke`;
//   ctor-newinstance    -- a constructor that asks, through `Constructor.newInstance`;
//   mh-lambda           -- a lambda body asks, invoked through `MethodHandle.invokeExact`;
//   mh-static           -- `who()` invoked through `MethodHandle.invokeExact`;
//   thread-run          -- a `Runnable` lambda asks, run by `Thread.run`;
//   bsm                 -- an indy bootstrap method asks (below it: the
//                          non-hidden `java.lang.invoke` linkage frames, then
//                          the class whose `invokedynamic` linked);
//   same-name-a/b       -- two classes named `Twin`, defined by two loaders,
//                          each asks; the answer's loader is the row's;
//   from-main           -- `getCallerClass()` called directly in `main`,
//                          whose caller is the launcher (no Java frame);
//   lookup-reflective   -- `MethodHandles.lookup()` called through `Method.invoke`;
//   forname-mh          -- `Class.forName(String)` through a `MethodHandle`
//                          (the caller is the lookup class, bound);
//   direct-again        -- `direct` again.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, measured; the same with -Xint)
// direct: L3W50CallerClassRule
// method-invoke: L3W50CallerClassRule
// ctor-newinstance: L3W50CallerClassRule
// mh-lambda: L3W50CallerClassRule
// mh-static: L3W50CallerClassRule
// thread-run: java.lang.Thread
// bsm: linked seen=L3W50CallerClassRuleIndy
// same-name-a: Twin loader=a
// same-name-b: Twin loader=b
// from-main: java.lang.IllegalCallerException: no caller frame: [L3W50CallerClassRule , null]
// lookup-reflective: L3W50CallerClassRule
// forname-mh: L3W50CallerClassRule$Asker
// direct-again: L3W50CallerClassRule
// HOTSPOT_EXPECTED_END
//
// CratonVM at the wave-50 base (both modes): every row but `from-main`
// matches; `from-main` printed `L3W50CallerClassRule` (the name list's
// fallback answers the only frame left, then `Object`). Since wave 50
// (`CRATONVM_CALLER_CLASS_FINDER`, `VmConfig::caller_class_finder_rule`) the
// registered `getCallerClass` answers by HotSpot's rule and the row matches.
// Positive control: `CRATONVM_DBG_CALLER=1` prints
// `[DBG_CALLER] getCallerClass (finder rule): <asker> -> <caller>`.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W50CallerClassRule`.
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicCallSiteDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.CallSite;
import java.lang.invoke.ConstantCallSite;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;
import java.util.function.Supplier;

public class L3W50CallerClassRule {
    static final StackWalker W = StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE);
    static final ClassDesc SELF = ClassDesc.of("L3W50CallerClassRule");
    static volatile String seen;

    interface Call {
        Object run() throws Throwable;
    }

    static void row(String name, Call call) {
        String out;
        try {
            Object r = call.run();
            out = String.valueOf(r);
        } catch (Throwable t) {
            out = t.getClass().getName();
        }
        System.out.println(name + ": " + out);
    }

    static String name(Class<?> c) {
        if (c == null) {
            return "null";
        }
        String n = c.getName();
        int slash = n.indexOf('/');
        return slash >= 0 ? n.substring(0, slash) + "/<hidden>" : n;
    }

    /** The caller of `who`'s caller. */
    public static String who() {
        return name(W.getCallerClass());
    }

    /** Called by `direct` so that `who`'s caller is this, and its caller `main`'s helper. */
    static String viaHelper() {
        return askHere();
    }

    static String askHere() {
        return name(W.getCallerClass());
    }

    public static class Asker {
        public final String answer;

        public Asker() {
            answer = name(W.getCallerClass());
        }
    }

    public static String asStatic() {
        return name(W.getCallerClass());
    }

    public static CallSite bsm(MethodHandles.Lookup l, String n, MethodType t) {
        seen = name(W.getCallerClass());
        return new ConstantCallSite(MethodHandles.constant(String.class, "linked"));
    }

    /** A class `Indy` with `static String go()` whose body is an indy to `bsm`. */
    static Class<?> indyClass() throws Throwable {
        DirectMethodHandleDesc boot = MethodHandleDesc.ofMethod(
                java.lang.constant.DirectMethodHandleDesc.Kind.STATIC, SELF, "bsm",
                MethodTypeDesc.of(ConstantDescs.CD_CallSite, ConstantDescs.CD_MethodHandles_Lookup,
                        ConstantDescs.CD_String, ConstantDescs.CD_MethodType));
        byte[] b = ClassFile.of().build(ClassDesc.of("L3W50CallerClassRuleIndy"), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("go", MethodTypeDesc.of(ConstantDescs.CD_String),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.invokedynamic(DynamicCallSiteDesc.of(boot, "x",
                            MethodTypeDesc.of(ConstantDescs.CD_String))).areturn());
        });
        return MethodHandles.lookup().defineClass(b);
    }

    /** A class `Twin` (one per loader) whose `ask()` calls `who()`. */
    static Class<?> twin(String tag) throws Throwable {
        byte[] b = ClassFile.of().build(ClassDesc.of("Twin"), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("ask", MethodTypeDesc.of(ConstantDescs.CD_String),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.invokestatic(SELF, "whoWithLoader", MethodTypeDesc.of(ConstantDescs.CD_String))
                            .areturn());
        });
        ClassLoader parent = L3W50CallerClassRule.class.getClassLoader();
        ClassLoader l = new ClassLoader(tag, parent) {
            @Override
            protected Class<?> findClass(String n) throws ClassNotFoundException {
                if (n.equals("Twin")) {
                    return defineClass(n, b, 0, b.length);
                }
                throw new ClassNotFoundException(n);
            }
        };
        return Class.forName("Twin", true, l);
    }

    public static String whoWithLoader() {
        Class<?> c = W.getCallerClass();
        ClassLoader l = c.getClassLoader();
        return name(c) + " loader=" + (l == null ? "boot" : l.getName());
    }

    public static void main(String[] args) throws Throwable {
        row("direct", L3W50CallerClassRule::viaHelper);
        Method m = L3W50CallerClassRule.class.getMethod("who");
        row("method-invoke", () -> m.invoke(null));
        row("ctor-newinstance", () -> Asker.class.getConstructor().newInstance().answer);
        row("mh-lambda", () -> {
            Supplier<String> s = () -> name(W.getCallerClass());
            MethodHandle h = MethodHandles.lookup().findVirtual(Supplier.class, "get",
                    MethodType.methodType(Object.class));
            return (Object) h.invokeExact(s);
        });
        row("mh-static", () -> {
            MethodHandle h = MethodHandles.lookup().findStatic(L3W50CallerClassRule.class, "asStatic",
                    MethodType.methodType(String.class));
            return (String) h.invokeExact();
        });
        row("thread-run", () -> {
            String[] out = new String[1];
            Thread t = new Thread(() -> out[0] = name(W.getCallerClass()));
            t.start();
            t.join();
            return out[0];
        });
        row("bsm", () -> {
            Class<?> c = indyClass();
            Object r = c.getMethod("go").invoke(null);
            return r + " seen=" + seen;
        });
        Class<?> ta = twin("a");
        Class<?> tb = twin("b");
        row("same-name-a", () -> ta.getMethod("ask").invoke(null));
        row("same-name-b", () -> tb.getMethod("ask").invoke(null));
        String fromMain;
        try {
            fromMain = name(W.getCallerClass());
        } catch (Throwable t) {
            fromMain = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println("from-main: " + fromMain);
        row("lookup-reflective", () -> {
            Method lm = MethodHandles.class.getMethod("lookup");
            return name(((MethodHandles.Lookup) lm.invoke(null)).lookupClass());
        });
        row("forname-mh", () -> {
            MethodHandle h = MethodHandles.lookup().findStatic(Class.class, "forName",
                    MethodType.methodType(Class.class, String.class));
            return name((Class<?>) h.invokeExact("L3W50CallerClassRule$Asker"));
        });
        row("direct-again", L3W50CallerClassRule::viaHelper);
    }
}

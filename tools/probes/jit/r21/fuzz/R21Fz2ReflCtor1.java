// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 3, lane fuzz2: REFLECT21-3, `Constructor.newInstance`
// through a compiled link (`CRATONVM_JIT_REFLECT_CTOR_LINK`, opt-in;
// `helpers::reflect_ctor_link_target` / `reflect_ctor_link_redirect`:
// compiled `new` via `jit_new_object_body`, arguments pinned across it, then
// `<init>` as a compiled invokespecial).
//
// Risk targeted, beyond wave 1's `R21FzReflInvoke1` (GC churn,
// InstantiationException, `<clinit>` on newInstance):
//  * the link allocates through `jit_new_object_body`: a FINALIZABLE class
//    must still be registered (finalize() runs for every instance), and a
//    java.lang.ref subclass must still be discovered (cleared and enqueued);
//  * the constructor's own frames: a stack trace taken inside `<init>`, and
//    a StackWalker with and without SHOW_REFLECT_FRAMES, must list HotSpot's
//    frames (`newInstance`, `newInstanceWithCaller`,
//    `DirectConstructorHandleAccessor.newInstance` / `invokeImpl`);
//  * exceptions out of `<init>`: unchecked and checked (both wrapped in
//    InvocationTargetException with the same cause identity), and an Error;
//  * argument conversion: widening (int -> long ctor parameter), unboxing,
//    null for a primitive, a wrong reference type, wrong count -- each IAE
//    text exactly HotSpot's;
//  * inner (non-static) class ctor with the outer instance, a varargs ctor,
//    a private ctor after setAccessible, an enum ctor (IAE "Cannot
//    reflectively create enum objects"), a class first initialized by the
//    hot site.
// Cold, hot (50k calls per ctor), a late subclass ctor (deopt), again.
//
// Run:
//   javac -d out R21Fz2ReflCtor1.java
//   java -Xmx256m -cp out R21Fz2ReflCtor1                                # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz2ReflCtor1
//   CRATONVM_JIT_REFLECT_CTOR_LINK=1 cratonvm -Xmx256m -cp out R21Fz2ReflCtor1
//   CRATONVM_JIT_REFLECT_CTOR_LINK=1 CRATONVM_JIT_MH_COMPILED_LINK_HIDDEN=1 cratonvm ...
//   CRATONVM_JIT_REFLECT_CTOR_LINK=1 cratonvm -XX:+UseZGC ... / -XX:+UseG1GC ...
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

import java.lang.ref.ReferenceQueue;
import java.lang.ref.WeakReference;
import java.lang.reflect.Constructor;
import java.lang.reflect.InvocationTargetException;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.stream.Collectors;

public class R21Fz2ReflCtor1 {
    static final int WARM = 50_000;
    static final AtomicInteger FINALIZED = new AtomicInteger();
    static final ReferenceQueue<Object> Q = new ReferenceQueue<>();
    static volatile String captured = "";

    static class Fin {
        final int v;

        public Fin(int v) {
            this.v = v;
        }

        @Override
        @SuppressWarnings("removal")
        protected void finalize() {
            FINALIZED.incrementAndGet();
        }
    }

    public static class WRef extends WeakReference<Object> {
        public WRef(Object r) {
            super(r, Q);
        }
    }

    public static class Pt {
        final long x;
        final Object tag;

        public Pt(long x, Object tag) {
            this.x = x;
            this.tag = tag;
        }

        public Pt(int mode) throws Exception {
            this.x = mode;
            this.tag = null;
            if (mode == 1) {
                throw new IllegalStateException("unchecked" + mode);
            } else if (mode == 2) {
                throw new java.io.IOException("checked" + mode);
            } else if (mode == 3) {
                throw new AssertionError("error" + mode);
            } else if (mode == 4) {
                captured = trace(new Throwable());
            } else if (mode == 5) {
                captured = walk(StackWalker.getInstance()) + " || "
                        + walk(StackWalker.getInstance(StackWalker.Option.SHOW_REFLECT_FRAMES));
            }
        }

        public Pt(String... parts) {
            this.x = parts.length;
            this.tag = String.join("+", parts);
        }

        private Pt(char c) {
            this.x = c;
            this.tag = "private";
        }
    }

    public class Inner {
        final int k;

        public Inner(int k) {
            this.k = k + base;
        }
    }

    int base = 1000;

    enum En { A }

    static class LazyInit {
        static final String STAMP;

        static {
            STAMP = "init" + Thread.currentThread().getName().length();
        }

        public LazyInit() {
        }
    }

    static class SubPt extends Pt {
        public SubPt(long x, Object tag) {
            super(x * 2, tag);
        }
    }

    static String trace(Throwable t) {
        StringBuilder sb = new StringBuilder();
        StackTraceElement[] st = t.getStackTrace();
        for (int i = 0; i < st.length && i < 7; i++) {
            sb.append(i == 0 ? "" : ",").append(st[i].getClassName())
                    .append('.').append(st[i].getMethodName());
        }
        return sb.toString();
    }

    static String walk(StackWalker w) {
        return w.walk(s -> s.limit(7).map(f -> f.getClassName() + "." + f.getMethodName())
                .collect(Collectors.joining(",")));
    }

    static String call(Constructor<?> c, Object... args) {
        try {
            Object o = c.newInstance(args);
            if (o instanceof Pt p) {
                return "Pt(" + p.x + "," + p.tag + ")";
            }
            if (o instanceof Inner in) {
                return "Inner(" + in.k + ")";
            }
            if (o instanceof Fin f) {
                return "Fin(" + f.v + ")";
            }
            return o.getClass().getSimpleName();
        } catch (InvocationTargetException e) {
            Throwable c2 = e.getCause();
            // A helpful-NPE text names locals: printed by class only.
            String cm = c2 instanceof NullPointerException ? "-" : c2.getMessage();
            return "ITE(" + c2.getClass().getSimpleName() + ":" + cm + ",msg=" + e.getMessage() + ")";
        } catch (IllegalArgumentException e) {
            return "IAE(" + e.getMessage() + ")";
        } catch (ReflectiveOperationException e) {
            return e.getClass().getSimpleName() + "(" + e.getMessage() + ")";
        }
    }

    static boolean waitFin(int want) {
        for (int i = 0; i < 400 && FINALIZED.get() < want; i++) {
            System.gc();
            System.runFinalization();
            try {
                Thread.sleep(2);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        }
        return FINALIZED.get() == want;
    }

    public static void main(String[] args) throws Exception {
        Constructor<Pt> pLong = Pt.class.getConstructor(long.class, Object.class);
        Constructor<Pt> pMode = Pt.class.getConstructor(int.class);
        Constructor<Pt> pVar = Pt.class.getConstructor(String[].class);
        Constructor<Pt> pPriv = Pt.class.getDeclaredConstructor(char.class);
        pPriv.setAccessible(true);
        Constructor<Inner> inner = Inner.class.getConstructor(R21Fz2ReflCtor1.class, int.class);
        Constructor<Fin> fin = Fin.class.getConstructor(int.class);
        Constructor<WRef> wref = WRef.class.getConstructor(Object.class);
        Constructor<En> en = En.class.getDeclaredConstructor(String.class, int.class);
        Constructor<SubPt> sub = SubPt.class.getConstructor(long.class, Object.class);
        R21Fz2ReflCtor1 outer = new R21Fz2ReflCtor1();

        for (int pass = 0; pass < 3; pass++) {
            String tag = pass == 0 ? "cold" : pass == 1 ? "hot" : "after";
            System.out.println("row " + tag + " | " + call(pLong, 5L, "a") + " " + call(pLong, 7, "widen") + " "
                    + call(pLong, Long.valueOf(9), null) + " " + call(pLong, null, "nullprim") + " "
                    + call(pLong, "str", "wrongtype") + " " + call(pLong, 1L) + " " + call(pMode, 0) + " "
                    + call(pMode, 1) + " " + call(pMode, 2) + " " + call(pMode, 3) + " "
                    + call(pVar, (Object) new String[] {"x", "y"}) + " " + call(pVar, (Object) null) + " "
                    + call(pPriv, 'q') + " " + call(inner, outer, 4) + " " + call(inner, null, 4) + " "
                    + call(en, "B", 1) + " " + call(fin, 3));
            call(pMode, 4);
            System.out.println("trace " + tag + " " + captured);
            call(pMode, 5);
            System.out.println("walk " + tag + " " + captured);
            if (pass == 0) {
                long h = 0;
                for (int r = 0; r < WARM; r++) {
                    h = h * 31 + call(pLong, (long) r, "w").length() + call(pMode, 0).length()
                            + call(pVar, (Object) new String[] {"z"}).length() + call(inner, outer, r & 7).length();
                    if ((r & 3) == 0) {
                        h = h * 31 + call(pMode, 1).length();
                    }
                }
                System.out.println("warm hash " + h);
                int fins = 20_000;
                for (int r = 0; r < fins; r++) {
                    call(fin, r);
                }
                System.out.println("finalized all " + waitFin(fins + 1));
                Object[] keep = new Object[20_000];
                for (int r = 0; r < keep.length; r++) {
                    keep[r] = wref.newInstance(new int[] {r});
                }
                int enq = 0;
                for (int i = 0; i < 400 && enq < keep.length; i++) {
                    System.gc();
                    while (Q.poll() != null) {
                        enq++;
                    }
                }
                int live = 0;
                for (Object o : keep) {
                    live += ((WRef) o).get() == null ? 0 : 1;
                }
                System.out.println("wref enqueued " + enq + " live " + live);
                // A class first initialized by the (now hot) reflective path.
                System.out.println("lazy " + call(LazyInit.class.getConstructor()) + " " + LazyInit.STAMP);
                // Deopt trigger: a subclass ctor through the same newInstance chain.
                System.out.println("late " + call(sub, 4L, "s") + " " + call(sub, 3, "w") + " " + call(sub, null, "n"));
            }
        }
    }
}

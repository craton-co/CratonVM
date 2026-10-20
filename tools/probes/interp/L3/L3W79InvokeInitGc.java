// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 79, lane L3
// (docs/internal/fixed-bugs/interpreter-i74-L3-method-invoke-reads-raw-references-after-the-declaring-class-initializer-FIXED-20261111.md):
// the first `Method.invoke` of a static method initializes its declaring
// class; that <clinit> allocates enough to collect, and the invoke then reads
// the argument array (and the declaring mirror) it took before the <clinit>.
// Each row uses a fresh class, so each first call runs a <clinit>. The
// arguments must arrive intact.
//
// Run --compatible and --jdk-only, default and --nojit, and with
// CRATONVM_DBG_GC_STRESS=64 (a collection every 64 allocations: the window
// becomes likely).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// static-object payload-1 len=9
// static-two payload-2|payload-3
// static-array 4:payload-4
// instance-alloc-instance payload-5 via Instance
// HOTSPOT_EXPECTED_END
import java.lang.reflect.Method;

public class L3W79InvokeInitGc {
    static Object churn() {
        Object[] keep = new Object[64];
        for (int i = 0; i < 200_000; i++) {
            keep[i & 63] = new byte[256];
        }
        return keep;
    }

    static class A {
        static final Object JUNK = churn();

        public static String m(Object o) {
            return o + " len=" + o.toString().length();
        }
    }

    static class B {
        static final Object JUNK = churn();

        public static String m(Object a, Object b) {
            return a + "|" + b;
        }
    }

    static class C {
        static final Object JUNK = churn();

        public static String m(String[] xs) {
            return xs.length + ":" + xs[xs.length - 1];
        }
    }

    static class Instance {
        static final Object JUNK = churn();
        String tag = "unset";

        public String m(Object o) {
            return o + " via " + getClass().getSimpleName();
        }
    }

    static String fresh(int n) {
        return new StringBuilder("payload-").append(n).toString();
    }

    public static void main(String[] args) throws Exception {
        Method a = A.class.getMethod("m", Object.class);
        System.out.println("static-object " + a.invoke(null, fresh(1)));

        Method b = B.class.getMethod("m", Object.class, Object.class);
        System.out.println("static-two " + b.invoke(null, fresh(2), fresh(3)));

        Method c = C.class.getMethod("m", String[].class);
        String[] xs = {fresh(0), fresh(0), fresh(0), fresh(4)};
        System.out.println("static-array " + c.invoke(null, (Object) xs));

        // The instance path after a <clinit> that collected (run by the
        // reflective constructor): receiver and argument read intact.
        Method im = Instance.class.getMethod("m", Object.class);
        Object inst = Instance.class.getDeclaredConstructor().newInstance();
        System.out.println("instance-alloc-instance " + im.invoke(inst, fresh(5)));
    }
}

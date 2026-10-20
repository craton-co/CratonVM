// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// probe -- Interpreter round i6 wave 133, lane L2. Decomposes the cost of
// `TypeVariable.getAnnotations()` (L1W125TypeVariableImplFamily's TIME row,
// about 8,000,000 ns/op under `cratonvm --nojit` against about 500,000 ns/op
// under HotSpot -Xint) into the steps the JDK body takes per call:
//
//   TypeVariableImpl.getAnnotations -> getDeclaredAnnotations
//     -> typeVarIndex (getGenericDeclaration().getTypeParameters())
//     -> TypeAnnotationParser.parseTypeVariableAnnotations
//        -> parseAllTypeAnnotations: Class.getRawTypeAnnotations,
//           Class.getConstantPool, a ByteBuffer walk over EVERY class-level
//           type annotation (not only this variable's), and per annotation
//           AnnotationParser.parseAnnotation2:
//             parseSig -> SignatureParser + Reifier +
//               CoreReflectionFactory.makeNamedType -> Class.forName(n, false, loader)
//             AnnotationType.getInstance, member values (ConstantPool.getUTF8At0)
//             annotationForMap -> Proxy.newProxyInstance
//        -> ArrayList.toArray(new Annotation[0])
//
// No memo in the JDK: every call re-parses (HotSpot is slow too). Rows are
// TIME lines; the orchestrator divides the cratonvm row by the HotSpot -Xint
// row, and the row whose ratio stands out of the general interpreter ratio
// (getName / typeParams) names the cost. Argument `full` runs only the full
// row (for a CRATONVM_DBG_INTERP_FRAMES census: frames / op = count / 4000,
// two reps of 2000; it prints the first two expected lines and sink>0=true).
//
// Run (TIME lines excluded from the comparison):
//   $J/javac -d out tools/probes/interp/L2/L2W133GetAnnotationsCost.java
//   $J/java -Xint -cp out L2W133GetAnnotationsCost
//   $B --jdk-only --nojit -cp out L2W133GetAnnotationsCost
//   $B --compatible --nojit -cp out L2W133GetAnnotationsCost
//   CRATONVM_DBG_INTERP_FRAMES=1 $B --jdk-only --nojit -cp out L2W133GetAnnotationsCost full
//   $B --jdk-only --nojit -cp out L2W133GetAnnotationsCost sample 1500   (Java-level sampler;
//     prints "full annotations=..." and "sink>0=true", the rest are TIME lines)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint identical; TIME lines filtered)
// full annotations=[@L2W133GetAnnotationsCost.Tp("v"), @L2W133GetAnnotationsCost.Reps({@L2W133GetAnnotationsCost.Rep(1), @L2W133GetAnnotationsCost.Rep(2)})]
// fresh top=true nested=true equal=true nestedEqual=true
// typeParams=[T, V]
// forNameApp=L2W133GetAnnotationsCost$Tp
// forNameJdk=java.util.Map
// proxy=true
// annProxyCall=v
// method=marked
// methodAnnotations=[@L2W133GetAnnotationsCost.Md("x")]
// field=map
// genericType=java.util.Map<T, V>
// byteBuffer=2
// toArray=3 class=[Ljava.lang.annotation.Annotation;
// sink>0=true
// HOTSPOT_EXPECTED_END

import java.lang.annotation.Annotation;
import java.lang.annotation.ElementType;
import java.lang.annotation.Repeatable;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;
import java.lang.reflect.Field;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;
import java.lang.reflect.TypeVariable;
import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Map;

public class L2W133GetAnnotationsCost {

    @Target(ElementType.TYPE_PARAMETER)
    @Retention(RetentionPolicy.RUNTIME)
    @interface Tp {
        String value() default "tp";
    }

    @Target(ElementType.TYPE_USE)
    @Retention(RetentionPolicy.RUNTIME)
    @interface Tu {
        String value() default "tu";
    }

    @Target({ElementType.TYPE_PARAMETER, ElementType.TYPE_USE})
    @Retention(RetentionPolicy.RUNTIME)
    @Repeatable(Reps.class)
    @interface Rep {
        int value();
    }

    @Target({ElementType.TYPE_PARAMETER, ElementType.TYPE_USE})
    @Retention(RetentionPolicy.RUNTIME)
    @interface Reps {
        Rep[] value();
    }

    @Target(ElementType.METHOD)
    @Retention(RetentionPolicy.RUNTIME)
    @interface Md {
        String value();
    }

    static class G<T extends Number, @Tp("v") @Rep(1) @Rep(2) V extends @Tu("b") List<? super T>> {
        Map<T, V> map;

        @Md("x")
        void marked() {
        }
    }

    static final class H implements InvocationHandler {
        public Object invoke(Object proxy, Method m, Object[] args) {
            return null;
        }
    }

    static long sink;

    static void time(String row, int n, long t0) {
        System.out.println("TIME " + row + " ns/op=" + (System.nanoTime() - t0) / n);
    }

    static volatile boolean stopWorker;

    /// `sample`: a Java-level sampler. A worker loops getAnnotations; this
    /// thread takes `Thread.getStackTrace` of it every 2 ms and prints the top
    /// self frames and the top inclusive frames (all TIME lines).
    static void sample(TypeVariable<?> v, int samplesWanted) throws Exception {
        Thread w = new Thread(() -> {
            while (!stopWorker) {
                sink += v.getAnnotations().length;
            }
        });
        w.start();
        Thread.sleep(300);
        Map<String, Integer> self = new java.util.HashMap<>();
        Map<String, Integer> incl = new java.util.HashMap<>();
        int samples = 0;
        for (int s = 0; s < samplesWanted; s++) {
            StackTraceElement[] st = w.getStackTrace();
            if (st.length > 0) {
                samples++;
                self.merge(st[0].getClassName() + "." + st[0].getMethodName(), 1, Integer::sum);
                java.util.Set<String> seen = new java.util.HashSet<>();
                for (StackTraceElement e : st) {
                    String k = e.getClassName() + "." + e.getMethodName();
                    if (seen.add(k)) {
                        incl.merge(k, 1, Integer::sum);
                    }
                }
            }
            Thread.sleep(2);
        }
        stopWorker = true;
        w.join();
        System.out.println("TIME sample samples=" + samples);
        self.entrySet().stream().sorted((x, y) -> y.getValue() - x.getValue()).limit(30)
                .forEach(e -> System.out.println("TIME sample self " + e.getValue() + " " + e.getKey()));
        incl.entrySet().stream().sorted((x, y) -> y.getValue() - x.getValue()).limit(50)
                .forEach(e -> System.out.println("TIME sample incl " + e.getValue() + " " + e.getKey()));
    }

    public static void main(String[] args) throws Exception {
        boolean onlyFull = args.length > 0 && args[0].equals("full");
        TypeVariable<?>[] gs = G.class.getTypeParameters();
        TypeVariable<?> v = gs[1];
        if (args.length > 0 && args[0].equals("sample")) {
            System.out.println("full annotations=" + Arrays.toString(v.getAnnotations()));
            sample(v, args.length > 1 ? Integer.parseInt(args[1]) : 1500);
            System.out.println("sink>0=" + (sink > 0));
            return;
        }
        ClassLoader loader = L2W133GetAnnotationsCost.class.getClassLoader();
        System.out.println("full annotations=" + Arrays.toString(v.getAnnotations()));
        // Why a VM-side memo of the parsed annotations is not an option: every
        // call answers fresh proxies, nested ones included.
        Annotation[] a1 = v.getAnnotations();
        Annotation[] a2 = v.getAnnotations();
        Rep r1 = ((Reps) a1[1]).value()[0];
        Rep r2 = ((Reps) a2[1]).value()[0];
        System.out.println("fresh top=" + (a1[0] != a2[0]) + " nested=" + (r1 != r2)
                + " equal=" + a1[0].equals(a2[0]) + " nestedEqual=" + r1.equals(r2));
        int na = 2000;
        for (int rep = 0; rep < 2; rep++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < na; i++) {
                sink += v.getAnnotations().length;
            }
            time("full rep=" + rep, na, t0);
        }
        if (onlyFull) {
            System.out.println("sink>0=" + (sink > 0));
            return;
        }
        int n = 20_000;

        System.out.println("typeParams=" + Arrays.toString(G.class.getTypeParameters()));
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            sink += G.class.getTypeParameters().length;
        }
        time("typeParams", n, t0);

        String tpName = L2W133GetAnnotationsCost.class.getName() + "$Tp";
        System.out.println("forNameApp=" + Class.forName(tpName, false, loader).getName());
        t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            sink += Class.forName(tpName, false, loader).hashCode() & 1;
        }
        time("forNameApp", n, t0);

        System.out.println("forNameJdk=" + Class.forName("java.util.Map", false, loader).getName());
        t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            sink += Class.forName("java.util.Map", false, loader).hashCode() & 1;
        }
        time("forNameJdk", n, t0);

        H h = new H();
        Class<?>[] ifaces = {Tp.class};
        System.out.println("proxy=" + Proxy.isProxyClass(Proxy.newProxyInstance(loader, ifaces, h).getClass()));
        t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            sink += Proxy.newProxyInstance(loader, ifaces, h) != null ? 1 : 0;
        }
        time("proxy", n, t0);

        Tp tp = v.getAnnotation(Tp.class);
        System.out.println("annProxyCall=" + tp.value());
        t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            sink += tp.value().length();
        }
        time("annProxyCall", n, t0);

        System.out.println("method=" + G.class.getDeclaredMethod("marked").getName());
        t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            sink += G.class.getDeclaredMethod("marked").getModifiers();
        }
        time("getDeclaredMethod", n, t0);

        System.out.println("methodAnnotations="
                + Arrays.toString(G.class.getDeclaredMethod("marked").getDeclaredAnnotations()));
        t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            sink += G.class.getDeclaredMethod("marked").getDeclaredAnnotations().length;
        }
        time("getDeclaredMethod+annotations", n, t0);

        System.out.println("field=" + G.class.getDeclaredField("map").getName());
        t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            sink += G.class.getDeclaredField("map").getModifiers();
        }
        time("getDeclaredField", n, t0);

        System.out.println("genericType=" + G.class.getDeclaredField("map").getGenericType());
        t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            Field f = G.class.getDeclaredField("map");
            sink += f.getGenericType().hashCode() & 1;
        }
        time("getDeclaredField+genericType", n, t0);

        byte[] bytes = new byte[64];
        bytes[1] = 1;
        bytes[2] = 1;
        ByteBuffer bb = ByteBuffer.wrap(bytes);
        System.out.println("byteBuffer=" + (bb.getShort() + bb.get() + bb.getInt()));
        int nb = 200_000;
        t0 = System.nanoTime();
        for (int i = 0; i < nb; i++) {
            bb.position(0);
            sink += bb.getShort() + bb.get() + bb.getInt();
        }
        time("byteBuffer 3 reads", nb, t0);

        Annotation[] three = {tp, tp, tp};
        List<Annotation> list = new ArrayList<>(3);
        list.addAll(Arrays.asList(three));
        Annotation[] arr = list.toArray(new Annotation[0]);
        System.out.println("toArray=" + arr.length + " class=" + arr.getClass().getName());
        t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            List<Annotation> l = new ArrayList<>(3);
            l.add(tp);
            l.add(tp);
            l.add(tp);
            sink += l.toArray(new Annotation[0]).length;
        }
        time("list toArray", n, t0);

        t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            sink += v.getName().length();
        }
        time("getName", n, t0);

        System.out.println("sink>0=" + (sink > 0));
    }
}

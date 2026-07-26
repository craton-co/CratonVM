import java.beans.BeanInfo;
import java.beans.Introspector;
import java.beans.MethodDescriptor;
import java.beans.PropertyDescriptor;

// SPB.9d (Session 117) repro: the ban's own note describes a real
// SEGFAULT where JIT-compiling java.beans.Introspector's method-sorting
// comparator chain (com.sun.beans.introspect.MethodInfo$MethodOrder.compare
// -> String.compareTo -> Method.getName/toString -> StringJoiner) returns
// inconsistent ordering, corrupting transient sort state and crashing in
// a downstream StringJoiner allocation. This is pure JDK machinery (no
// external jar needed) -- java.beans.Introspector.getBeanInfo(Class)
// drives exactly this comparator chain via java.util.Arrays.sort whenever
// it introspects a class's methods/properties. This probe repeatedly
// introspects classes with MANY methods/properties (to give the sort
// real multi-element comparator work each call) to exercise the exact
// code path under real, repeated JIT-eligible use.
public class BeanIntrospectorProbe {

    // Many getters/setters/plain methods to give Introspector's sort a
    // real multi-element comparator workload each call.
    public static class WideBean {
        public int getA() { return 0; } public void setA(int v) {}
        public int getB() { return 0; } public void setB(int v) {}
        public int getC() { return 0; } public void setC(int v) {}
        public int getD() { return 0; } public void setD(int v) {}
        public int getE() { return 0; } public void setE(int v) {}
        public int getF() { return 0; } public void setF(int v) {}
        public int getG() { return 0; } public void setG(int v) {}
        public int getH() { return 0; } public void setH(int v) {}
        public String getName() { return "x"; } public void setName(String v) {}
        public boolean isFlag() { return false; } public void setFlag(boolean v) {}
        public void doThing() {}
        public void doOtherThing(int x) {}
        public int compute(int a, int b) { return a + b; }
    }

    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        for (int i = 0; i < iterations; i++) {
            // Flush the Introspector's own cache each iteration so every
            // call really re-runs the sort/compare machinery instead of
            // hitting a cached BeanInfo after the first pass.
            Introspector.flushFromCaches(WideBean.class);
            BeanInfo info = Introspector.getBeanInfo(WideBean.class);
            PropertyDescriptor[] props = info.getPropertyDescriptors();
            MethodDescriptor[] methods = info.getMethodDescriptors();
            if (props.length < 10) {
                System.out.println("RESULT: FAIL at iteration " + i
                        + " -- expected >=10 properties, got " + props.length);
                System.exit(1);
            }
            if (methods.length < 20) {
                System.out.println("RESULT: FAIL at iteration " + i
                        + " -- expected >=20 methods, got " + methods.length);
                System.exit(1);
            }
            // Exercise the exact downstream toString/StringJoiner path
            // the ban's crash trace names.
            for (MethodDescriptor md : methods) {
                String s = md.toString();
                if (s == null || s.isEmpty()) {
                    System.out.println("RESULT: FAIL at iteration " + i
                            + " -- MethodDescriptor.toString() returned null/empty");
                    System.exit(1);
                }
            }
        }
        System.out.println("RESULT: OK -- " + iterations
                + " Introspector.getBeanInfo cycles (cache-flushed each time), all consistent");
    }
}

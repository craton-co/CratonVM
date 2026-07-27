package org.springframework.core;

import java.lang.reflect.Field;
import java.lang.reflect.Type;
import java.util.Comparator;
import java.util.List;
import java.util.function.Supplier;

// SPB.2 (Session 112 r8) repro: the ban's own note describes a JIT HANG
// (not a crash) in SerializableTypeWrapper.forTypeProvider's lambda SAM
// dispatch -- `lambda$forGenericInterfaces$<hash>$1(Class, int)` calling
// Class.getGenericInterfaces() after SerializableTypeWrapper.<clinit>'s
// heavy ConcurrentReferenceHashMap segment initialisation (16 segments x
// 10 maps = 160 segment ctor entries). This drives the real, public
// SerializableTypeWrapper.forField(Field) API on a field whose
// DECLARING CLASS implements several real generic interfaces (to give
// the "forGenericInterfaces" resolution real multi-interface work each
// call), then forces resolution of the returned wrapped Type via
// toString()/equals() (the wrapper is a lazy JDK dynamic proxy -- these
// calls are what actually trigger the lambda dispatch the ban describes).
//
// Run under an external `timeout` wrapper -- if this hangs, the process
// will be killed rather than blocking indefinitely, which is itself the
// decisive negative result (bug still present) rather than an unbounded
// wait.
public class SerializableTypeWrapperProbe {

    // Implements several real generic interfaces so
    // Class.getGenericInterfaces() has genuine multi-element work,
    // matching the ban's "forGenericInterfaces" description.
    static class MultiGenericHost
            implements Comparator<String>, Supplier<List<Integer>>, java.util.function.Function<String, Integer> {
        List<Integer> field;

        @Override
        public int compare(String a, String b) { return a.compareTo(b); }
        @Override
        public List<Integer> get() { return field; }
        @Override
        public Integer apply(String s) { return s.length(); }
    }

    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 5000;
        Field field = MultiGenericHost.class.getDeclaredField("field");
        for (int i = 0; i < iterations; i++) {
            Type wrapped = SerializableTypeWrapper.forField(field);
            if (wrapped == null) {
                System.out.println("RESULT: FAIL at iteration " + i + " -- forField returned null");
                System.exit(1);
            }
            // Force resolution of the lazy wrapper.
            String s = wrapped.toString();
            boolean eq = wrapped.equals(wrapped);
            int h = wrapped.hashCode();
            if (s == null || s.isEmpty() || !eq) {
                System.out.println("RESULT: FAIL at iteration " + i
                        + " -- wrapped Type resolution inconsistent (toString=" + s + ", equals=" + eq + ")");
                System.exit(1);
            }
            Type unwrapped = SerializableTypeWrapper.unwrap(wrapped);
            if (unwrapped == null) {
                System.out.println("RESULT: FAIL at iteration " + i + " -- unwrap returned null");
                System.exit(1);
            }
        }
        System.out.println("RESULT: OK -- " + iterations
                + " SerializableTypeWrapper.forField + resolve + unwrap cycles, all consistent");
    }
}

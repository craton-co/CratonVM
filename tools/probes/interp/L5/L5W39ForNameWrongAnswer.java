// Interpreter round i1, wave 39, lane L5 -- `Class.forName(name, init, L)`
// when `L.loadClass` returns `null`, or a class of ANOTHER name. HotSpot routes
// `forName` through `SystemDictionary` (`JVM_FindClassFromCaller` ->
// `find_class_from_class_loader`), which checks the loader's answer the same
// way a VM-initiated resolution does (JVMS §5.3.2).
//
// Found by lane L5's wave-39 review and fixed under `--jdk-only`
// (`native-builtins/src/lang_class.rs` `native_class_for_name`, the
// `loadClass` success arm). Before, from the code (not run), CratonVM
// returned whatever `loadClass` returned: `null` (so the row is a
// `java.lang.NullPointerException` from `getName`) and `java.lang.Object` for
// `p.Wanted` (`wrong forName=ok java.lang.Object`). `--compatible` keeps
// those two rows by design.
//
// Run (no setup):
//   javac -d out L5W39ForNameWrongAnswer.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W39ForNameWrongAnswer
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   null forName=java.lang.ClassNotFoundException: p/Wanted cause=null
//   wrong forName=java.lang.ClassNotFoundException: p/Wanted cause=null

public class L5W39ForNameWrongAnswer {
    static final class L extends ClassLoader {
        final String mode;

        L(String mode) {
            super(L5W39ForNameWrongAnswer.class.getClassLoader());
            this.mode = mode;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (name.equals("p.Wanted")) {
                return mode.equals("null") ? null : Object.class;
            }
            return super.loadClass(name, resolve);
        }
    }

    public static void main(String[] args) {
        for (String mode : new String[] {"null", "wrong"}) {
            try {
                Class<?> c = Class.forName("p.Wanted", false, new L(mode));
                System.out.println(mode + " forName=ok " + c.getName());
            } catch (Throwable t) {
                System.out.println(mode + " forName=" + t + " cause=" + t.getCause());
            }
        }
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 49, lane L7: `Throwable.getStackTrace()` fills
// each `StackTraceElement` with one batched field write (the four name/line
// fields and `declaringClassObject`), copies a repeated class's origin fields
// (module, version, loader name, format) in one batched read and write, and
// resolves the element class's layout once per array
// (`native-builtins/src/lang_misc.rs`, `fill_stack_trace_element_as`,
// `native_init_stack_trace_elements`). Every field the JDK reads back must be
// what HotSpot's `java_lang_StackTraceElement::fill_in` gives it. Rows: a
// trace through application frames (a repeated class: the origin copy), one
// through JDK frames (`java.base` module rows), a lambda frame, a static
// nested class, and `toString()` of each element (which reads `format`,
// `classLoaderName`, `moduleName`, `moduleVersion` and `declaringClassObject`
// through `computeFormat`). The first shape runs cold and again at the end.
//
// Expected (HotSpot 25.0.3, `java L7W49StackTraceElementFill`, and `-Xint`):
//   app [0] L7W49StackTraceElementFill.leaf file=L7W49StackTraceElementFill.java line=49 module=null version=null loader=app
//   app [1] L7W49StackTraceElementFill.down file=L7W49StackTraceElementFill.java line=56 module=null version=null loader=app
//   app [2] L7W49StackTraceElementFill.down file=L7W49StackTraceElementFill.java line=54 module=null version=null loader=app
//   app [3] L7W49StackTraceElementFill.down file=L7W49StackTraceElementFill.java line=54 module=null version=null loader=app
//   app [4] L7W49StackTraceElementFill.appShape file=L7W49StackTraceElementFill.java line=60 module=null version=null loader=app
//   app [5] L7W49StackTraceElementFill.main file=L7W49StackTraceElementFill.java line=91 module=null version=null loader=app
//   app str L7W49StackTraceElementFill.leaf(L7W49StackTraceElementFill.java:49)
//   app str L7W49StackTraceElementFill.down(L7W49StackTraceElementFill.java:56)
//   jdk [0] L7W49StackTraceElementFill.lambda$jdkShape$0 file=L7W49StackTraceElementFill.java line=66 module=null version=null loader=app
//   jdk [1] java.util.ArrayList.forEach file=ArrayList.java module=java.base version=true loader=null
//   jdk [2] L7W49StackTraceElementFill.jdkShape file=L7W49StackTraceElementFill.java line=66 module=null version=null loader=app
//   jdk str1 java.base/java.util.ArrayList.forEach(ArrayList.java)
//   nested [0] L7W49StackTraceElementFill$Nested.throwIt file=L7W49StackTraceElementFill.java line=79 module=null version=null loader=app
//   nested [1] L7W49StackTraceElementFill.nestedShape file=L7W49StackTraceElementFill.java line=87 module=null version=null loader=app
//   warm-app 6 L7W49StackTraceElementFill.leaf:49 main
//   printed L7W49StackTraceElementFill.leaf(L7W49StackTraceElementFill.java:49)
import java.util.ArrayList;
import java.util.List;

public class L7W49StackTraceElementFill {
    static String describe(StackTraceElement e, boolean withLine) {
        return e.getClassName() + "." + e.getMethodName()
                + " file=" + e.getFileName()
                + (withLine ? " line=" + e.getLineNumber() : "")
                + " module=" + e.getModuleName()
                + " version=" + (e.getModuleVersion() == null ? null : !e.getModuleVersion().isEmpty())
                + " loader=" + e.getClassLoaderName();
    }

    static StackTraceElement[] leaf() {
        return new RuntimeException("leaf").getStackTrace();
    }

    static StackTraceElement[] down(int n) {
        if (n > 0) {
            return down(n - 1);
        }
        return leaf();
    }

    static StackTraceElement[] appShape() {
        return down(2);
    }

    static StackTraceElement[] jdkShape() {
        List<Integer> list = new ArrayList<>(List.of(1));
        StackTraceElement[][] out = new StackTraceElement[1][];
        list.forEach(x -> out[0] = new IllegalStateException("jdk").getStackTrace());
        return out[0];
    }

    static void printApp(String label, StackTraceElement[] t) {
        for (int i = 0; i < t.length; i++) {
            System.out.println(label + " [" + i + "] " + describe(t[i], true));
        }
    }

    static final class Nested {
        static StackTraceElement[] throwIt() {
            try {
                throw new UnsupportedOperationException("nested");
            } catch (UnsupportedOperationException e) {
                return e.getStackTrace();
            }
        }
    }

    static StackTraceElement[] nestedShape() {
        return Nested.throwIt();
    }

    public static void main(String[] args) {
        StackTraceElement[] app = appShape();
        printApp("app", app);
        System.out.println("app str " + app[0]);
        System.out.println("app str " + app[1]);

        StackTraceElement[] jdk = jdkShape();
        // Keep the rows around the JDK frame: the lambda, forEach, the caller.
        int forEach = -1;
        for (int i = 0; i < jdk.length; i++) {
            if (jdk[i].getMethodName().equals("forEach")) {
                forEach = i;
            }
        }
        System.out.println("jdk [0] " + describe(jdk[forEach - 1], true));
        System.out.println("jdk [1] " + describe(jdk[forEach], false));
        System.out.println("jdk [2] " + describe(jdk[forEach + 1], true));
        String s = jdk[forEach].toString();
        System.out.println("jdk str1 " + s.substring(0, s.indexOf('(')) + "(" + jdk[forEach].getFileName() + ")");

        printApp("nested", java.util.Arrays.copyOf(nestedShape(), 2));

        StackTraceElement[] warm = null;
        for (int i = 0; i < 2000; i++) {
            warm = appShape();
        }
        System.out.println("warm-app " + warm.length + " " + warm[0].getClassName() + "." + warm[0].getMethodName()
                + ":" + warm[0].getLineNumber() + " " + warm[warm.length - 1].getMethodName());
        System.out.println("printed " + warm[0]);
    }
}

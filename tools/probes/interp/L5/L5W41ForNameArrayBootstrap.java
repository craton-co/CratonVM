// Interpreter round i1, wave 41, lane L5 -- `Class.forName(<array name>,
// init, null)`: an explicit null loader is the bootstrap loader, which sees
// no application class, so an array of one is `ClassNotFoundException` naming
// the ELEMENT (JVMS §5.3.3: the element is resolved by the same loader), even
// when the application loader has already made that array. An array of a
// bootstrap class resolves.
//
// Before wave 41 CratonVM (`lang_class.rs` `native_class_for_name`, from the
// code) answered every array name from the global store BEFORE the
// explicit-null-loader rule: `app-array=class [L...$Target;` and
// `app-array-2d=class [[L...$Target;`, in both modes. That is a genuine bug
// in `--compatible` too (a bootstrap-only lookup must not see application
// classes, as the non-array arm's comment says), so the fix applies to both
// modes.
//
// Run (no setup):
//   javac -d out L5W41ForNameArrayBootstrap.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W41ForNameArrayBootstrap
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim; the message is the array's INTERNAL name, measured with a
// packaged name too: `[Lp.q.X;` -> `ClassNotFoundException: [Lp/q/X;`):
//   made=[LL5W41ForNameArrayBootstrap$Target;
//   app-array=java.lang.ClassNotFoundException: [LL5W41ForNameArrayBootstrap$Target;
//   app-array-2d=java.lang.ClassNotFoundException: [[LL5W41ForNameArrayBootstrap$Target;
//   jdk-array=class [Ljava.lang.String;
//   prim-array=class [I

public class L5W41ForNameArrayBootstrap {
    static String row(String name) {
        try {
            return "class " + Class.forName(name, false, null).getName();
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    public static void main(String[] args) {
        // The application loader makes the array first.
        Target[] made = new Target[1];
        System.out.println("made=" + made.getClass().getName());
        System.out.println("app-array=" + row("[LL5W41ForNameArrayBootstrap$Target;"));
        System.out.println("app-array-2d=" + row("[[LL5W41ForNameArrayBootstrap$Target;"));
        System.out.println("jdk-array=" + row("[Ljava.lang.String;"));
        System.out.println("prim-array=" + row("[I"));
        System.out.println("missing=" + row("p.q.X"));
        System.out.println("missing-array=" + row("[Lp.q.X;"));
    }

    public static class Target {
    }
}

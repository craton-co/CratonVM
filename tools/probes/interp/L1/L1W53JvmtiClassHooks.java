// Interpreter round i2 wave 53, lane L1: the `ClassFileLoadHook` and
// `ClassLoad` events of the C JVMTI table, against HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-lacks-the-functions-and-events-agents-call-first-FIXED-20261007.md,
// "Next step (from wave 52)" item 1).
//
// The shim's `Agent_OnLoad` sets a `ClassFileLoadHook` and a `ClassLoad`
// callback; both report only the nested classes of this probe. The native
// `setHooks` enables (or disables) the two events for every thread, `rows`
// answers what the callbacks saw since the last call. The hook rewrites one
// class file (`Patched`: its constant "BEFORE" becomes "AFTER!" in a buffer
// from `Allocate`), so `patched=AFTER!` is the hook's bytes being defined.
// `Defined` is defined a second time by a loader of the probe's own
// (`Loader`), which the hook and the event name as `other`. An array class
// posts neither event. With the events disabled `Quiet` posts nothing;
// `Again` repeats `Target`'s rows at the end, so a first-call-only gap shows
// as a differing row. Then four rows of the table around them: the five
// events enabled for the current thread only (HotSpot refuses its global
// ones, ClassFileLoadHook and ThreadStart, with ILLEGAL_ARGUMENT 103),
// GetEnv for seven JVMTI versions (JNI_EVERSION -3 for 9.1, 10, 11.1, 12
// and 26), and whether GetLoadedClasses lists the probe class, Target,
// int[] and the primitive int (not). The onload row prints the agent's
// option string, NULL when -agentpath has no =.
//
// Needs the native shim tools/probes/interp/L1/L1W53JvmtiClassHooks.c,
// loaded both as an agent and with `System.load` (for the natives):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w53hooks.so tools/probes/interp/L1/L1W53JvmtiClassHooks.c
//   javac -d /tmp/l1w53hooks tools/probes/interp/L1/L1W53JvmtiClassHooks.java
//   java     -agentpath:/tmp/libl1w53hooks.so -cp /tmp/l1w53hooks L1W53JvmtiClassHooks /tmp/libl1w53hooks.so
//   cratonvm -agentpath:/tmp/libl1w53hooks.so -cp /tmp/l1w53hooks L1W53JvmtiClassHooks /tmp/libl1w53hooks.so
//
// Without the argument it prints only the usage line. The Rust port of the
// shim is tools/probes/interp/L1/rust-ports/L1W53JvmtiClassHooks (README in
// tools/probes/interp/L1/rust-ports/).
//
// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box with the Rust
// port of the C shim (same calls, same order, same output), three runs, the
// same each time:
//
//   onload: callbacks err=0 options=NULL
//   enable: cflh=0 load=0
//   hook L1W53JvmtiClassHooks$Target: redefined=null loader=app domain=set magic=ok phase=4
//   load LL1W53JvmtiClassHooks$Target;: status=0 thread=current loader=app
//   target=7
//   hook L1W53JvmtiClassHooks$Patched: redefined=null loader=app domain=set magic=ok phase=4 rewritten=1
//   load LL1W53JvmtiClassHooks$Patched;: status=0 thread=current loader=app
//   patched=AFTER!
//   array rows: 0
//   hook L1W53JvmtiClassHooks$Loader: redefined=null loader=app domain=set magic=ok phase=4
//   load LL1W53JvmtiClassHooks$Loader;: status=0 thread=current loader=app
//   hook L1W53JvmtiClassHooks$Defined: redefined=null loader=other domain=set magic=ok phase=4
//   load LL1W53JvmtiClassHooks$Defined;: status=0 thread=current loader=other
//   defined by loader: true
//   disable: cflh=0 load=0
//   quiet=3
//   quiet rows: 0
//   enable: cflh=0 load=0
//   hook L1W53JvmtiClassHooks$Again: redefined=null loader=app domain=set magic=ok phase=4
//   load LL1W53JvmtiClassHooks$Again;: status=0 thread=current loader=app
//   again=9
//   per thread: cflh=103 load=0 start=103 end=0 prepare=0
//   GetEnv: 9.0=0 9.1=-3 10.0=-3 11.1=-3 12.0=-3 13.5=0 26.0=-3
//   loaded classes: err=0 probe=1 target=1 int[]=1 int=0
//
// CratonVM before wave 53 (code read): `options=` (an empty string), then
// `enable: cflh=98 load=98`, no hook or load row, `patched=BEFORE`,
// `per thread: cflh=98 load=98 start=0 end=0 prepare=0`,
// `GetEnv: 9.0=0 9.1=0 10.0=-3 11.1=0 12.0=0 13.5=0 26.0=-3` and
// `loaded classes: err=98 ...`. Since wave 53 (`CRATONVM_JVMTI_C_CLASS_HOOKS`
// on, the default): HotSpot's lines (predicted from the code; with the switch
// off, the two events answer 98 as before and the other rows stay fixed). The hook rows are the
// positive control (`jvmti::native_env::post_class_file_load_hook`); unit
// tests `jvmti::native_env::tests::a_c_class_file_load_hook_rewrites_the_class_file`
// and `a_c_env_gets_class_load`.
public class L1W53JvmtiClassHooks {
    static native String setHooks(boolean on);

    static native String rows();

    static native String perThread();

    static native String envVersions();

    static native String loadedClasses(Class<?>[] queries);

    static class Target {
        static int value() {
            return 7;
        }
    }

    static class Patched {
        static String word() {
            return "BEFORE";
        }
    }

    static class Quiet {
        static int value() {
            return 3;
        }
    }

    static class Again {
        static int value() {
            return 9;
        }
    }

    static class Defined {
        static int value() {
            return 5;
        }
    }

    static class Loader extends ClassLoader {
        Loader() {
            super(null);
        }

        Class<?> define(String name, byte[] bytes) {
            return defineClass(name, bytes, 0, bytes.length);
        }
    }

    static void say(String label, Object value) {
        System.out.println(new StringBuilder(label).append(value).toString());
    }

    static int count(String rows) {
        int n = 0;
        for (int i = 0; i < rows.length(); i++) {
            if (rows.charAt(i) == '\n') {
                n++;
            }
        }
        return n;
    }

    static byte[] read(String resource) throws java.io.IOException {
        try (java.io.InputStream in = L1W53JvmtiClassHooks.class.getResourceAsStream(resource)) {
            return in.readAllBytes();
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W53JvmtiClassHooks <path of the shim library>");
            return;
        }
        System.load(args[0]);
        System.out.print(rows());
        say("enable: ", setHooks(true));
        int target = Target.value();
        System.out.print(rows());
        say("target=", target);
        String patched = Patched.word();
        System.out.print(rows());
        say("patched=", patched);
        Object[] array = new Target[2];
        say("array rows: ", count(rows()) + array.length - 2);
        byte[] bytes = read("L1W53JvmtiClassHooks$Defined.class");
        Loader loader = new Loader();
        Class<?> defined = loader.define("L1W53JvmtiClassHooks$Defined", bytes);
        System.out.print(rows());
        say("defined by loader: ", defined.getClassLoader() == loader);
        say("disable: ", setHooks(false));
        say("quiet=", Quiet.value());
        say("quiet rows: ", count(rows()));
        say("enable: ", setHooks(true));
        int again = Again.value();
        System.out.print(rows());
        say("again=", again);
        setHooks(false);
        say("per thread: ", perThread());
        say("GetEnv: ", envVersions());
        int[] ints = new int[1];
        Class<?>[] queries = {L1W53JvmtiClassHooks.class, Target.class, ints.getClass(), int.class};
        say("loaded classes: ", loadedClasses(queries));
    }
}

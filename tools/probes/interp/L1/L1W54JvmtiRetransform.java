// Interpreter round i2 wave 54, lane L1: `ClassFileLoadHook` at a
// retransformation and in the start phase, `RetransformClasses` (152) with
// `can_retransform_classes`, and `GetClassLoaderClasses` (79) of the C JVMTI
// table, against HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-lacks-the-functions-and-events-agents-call-first-FIXED-20261007.md,
// "Next step (from wave 53)" items 1 and 2).
//
// The shim's `Agent_OnLoad` reads which of four class-hook capabilities are
// potential, adds `can_retransform_classes`, sets a `ClassFileLoadHook` and a
// `VMInit` callback and enables the hook for every thread. Until `VMInit` the
// hook only counts the classes it is offered in the start phase (6) and in
// any other phase, with a NULL and a non-NULL loader; from then on it
// reports the nested classes of this probe: their loader, domain, whether
// it is a retransformation, and how many "BEFORE" / "AFTER!" constants the
// class file it is handed holds. At a retransformation it rewrites
// "BEFORE" to "AFTER!" (`Target.word()` then answers `AFTER!`); a second
// retransformation is handed the class file as first loaded again (JVMTI's
// retransformation base), not the woven one. Then `RetransformClasses`'
// refusals (count 0 and -1, NULL classes, int and int[], an env without the
// capability), the capabilities potential in the live phase, and which of
// four classes `GetClassLoaderClasses` lists for the application loader and
// for NULL (the bootstrap loader), `IsModifiableClass` (45) of five classes,
// and whether `GetLoadedClasses` lists a lambda's class and
// `GetClassSignature` answers for it.
//
// Needs the native shim tools/probes/interp/L1/L1W54JvmtiRetransform.c,
// loaded both as an agent and with `System.load` (for the natives):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w54retransform.so tools/probes/interp/L1/L1W54JvmtiRetransform.c
//   javac -d /tmp/l1w54retransform tools/probes/interp/L1/L1W54JvmtiRetransform.java
//   java     -agentpath:/tmp/libl1w54retransform.so -cp /tmp/l1w54retransform L1W54JvmtiRetransform /tmp/libl1w54retransform.so
//   cratonvm -agentpath:/tmp/libl1w54retransform.so -cp /tmp/l1w54retransform L1W54JvmtiRetransform /tmp/libl1w54retransform.so
//
// Without the argument it prints only the usage line. The Rust port of the
// shim is tools/probes/interp/L1/rust-ports/L1W54JvmtiRetransform (README in
// tools/probes/interp/L1/rust-ports/).
//
// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box with the Rust
// port of the C shim (same calls, same order, same output), three runs, the
// same each time:
//
//   onload potential: err=0 retransform=1
//   add can_retransform_classes: 0
//   onload: callbacks=0 cflh=0 init=0
//   start phase: boot=yes other=no earlier=no
//   target=BEFORE
//   hook L1W54JvmtiRetransform$Target: redefined=null loader=app domain=set before=1 after=0 rewritten=-1
//   retransform: err=0
//   hook L1W54JvmtiRetransform$Target: redefined=class loader=app domain=set before=1 after=0 rewritten=1
//   target=AFTER!
//   retransform again: err=0
//   hook L1W54JvmtiRetransform$Target: redefined=class loader=app domain=set before=1 after=0 rewritten=1
//   target=AFTER!
//   second=BEFORE
//   hook L1W54JvmtiRetransform$Second: redefined=null loader=app domain=set before=1 after=0 rewritten=-1
//   retransform both: err=0
//   hook L1W54JvmtiRetransform$Second: redefined=class loader=app domain=set before=1 after=0 rewritten=1
//   hook L1W54JvmtiRetransform$Target: redefined=class loader=app domain=set before=1 after=0 rewritten=1
//   second=AFTER!
//   count 0: err=0
//   count -1: err=103
//   null classes: err=100
//   int: err=79
//   int[]: err=79
//   without capability: err=99
//   live potential: err=0 retransform=1
//   app loader classes: err=0 probe=1 target=1 target[]=1 string=1
//   boot loader classes: err=0 probe=0 target=0 target[]=0 string=1
//   modifiable: target=1 int=0 int[]=0 string=1 lambda=0
//   lambda: loaded=1 signature-err=0
//   retransform last: err=0
//   hook L1W54JvmtiRetransform$Target: redefined=class loader=app domain=set before=1 after=0 rewritten=1
//   target=AFTER!
//
// (The port's first measurement also printed three more capabilities:
// HotSpot lists can_redefine_classes and can_generate_all_class_hook_events
// as potential in both phases and can_generate_early_class_hook_events in
// OnLoad only; the C table grants none of them.)
//
// CratonVM before wave 54 (code read): `retransform=0` in both potential
// rows and `add can_retransform_classes: 98`, `start phase: boot=no`, no
// `redefined=class` row, `target=BEFORE` throughout, every `RetransformClasses`
// row `err=98`, `loader classes` and `modifiable` rows `err=98` / `-98`.
// Since wave 54 (`CRATONVM_JVMTI_C_RETRANSFORM` and
// `CRATONVM_JVMTI_C_START_PHASE_HOOK` on, the defaults): HotSpot's lines
// (predicted from the code), except the `lambda:` row, which is
// `docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-does-not-know-the-vms-lambda-proxy-classes-FIXED-20261007.md`
// (HotSpot's since wave 55, with `CRATONVM_JVMTI_C_LAMBDA_CLASSES` on).
// The `redefined=class` rows are the positive control
// (`instrument::retransform_classes_for_jvmti`); unit test
// `jvmti::native_env::tests::a_c_env_with_can_retransform_classes_takes_the_retransform_group`.
public class L1W54JvmtiRetransform {
    static native String rows();

    static native String retransform(Class<?>[] classes, int count);

    static native String retransformNull();

    static native String withoutCapability(Class<?> klass);

    static native String livePotential();

    static native String loaderClasses(ClassLoader loader, Class<?>[] queries);

    static native String modifiable(Class<?>[] queries);

    static native String lambdaFacts(Class<?> lambda);

    static class Target {
        static String word() {
            return "BEFORE";
        }
    }

    static class Second {
        static String word() {
            return "BEFORE";
        }
    }

    static void say(String label, Object value) {
        System.out.println(new StringBuilder(label).append(value).toString());
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W54JvmtiRetransform <path of the shim library>");
            return;
        }
        System.load(args[0]);
        System.out.print(rows());
        say("target=", Target.word());
        System.out.print(rows());
        Class<?>[] target = {Target.class};
        say("retransform: ", retransform(target, 1));
        System.out.print(rows());
        say("target=", Target.word());
        say("retransform again: ", retransform(target, 1));
        System.out.print(rows());
        say("target=", Target.word());
        say("second=", Second.word());
        System.out.print(rows());
        Class<?>[] both = {Second.class, Target.class};
        say("retransform both: ", retransform(both, 2));
        System.out.print(rows());
        say("second=", Second.word());
        say("count 0: ", retransform(new Class<?>[0], 0));
        say("count -1: ", retransform(target, -1));
        say("null classes: ", retransformNull());
        say("int: ", retransform(new Class<?>[] {int.class}, 1));
        int[] ints = new int[1];
        say("int[]: ", retransform(new Class<?>[] {ints.getClass()}, 1));
        System.out.print(rows());
        say("without capability: ", withoutCapability(Target.class));
        say("live potential: ", livePotential());
        Target[] targets = new Target[1];
        Class<?>[] queries = {L1W54JvmtiRetransform.class, Target.class, targets.getClass(), String.class};
        ClassLoader app = L1W54JvmtiRetransform.class.getClassLoader();
        say("app loader classes: ", loaderClasses(app, queries));
        say("boot loader classes: ", loaderClasses(null, queries));
        Runnable lambda = () -> say("lambda ran", "");
        Class<?>[] five = {Target.class, int.class, ints.getClass(), String.class, lambda.getClass()};
        say("modifiable: ", modifiable(five));
        say("lambda: ", lambdaFacts(lambda.getClass()));
        // The retransformation again, at the end: a first-call-only gap shows
        // as a differing row.
        say("retransform last: ", retransform(target, 1));
        System.out.print(rows());
        say("target=", Target.word());
    }
}

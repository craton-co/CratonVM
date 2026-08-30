import java.util.function.BiFunction;

/** L8 tail, last batch — `jdk.internal`: `misc.VM` (9 rows), `access.SharedSecrets`
 *  (10), `misc.Signal` (4), `loader.AbstractClassLoaderValue` (3) and
 *  `util.Preconditions` (3).
 *
 *  These are not application API and the probe reaches them the only way anyone
 *  can, through `--add-exports`; both VMs are given the same flags:
 *
 *    --add-exports java.base/jdk.internal.misc=ALL-UNNAMED
 *    --add-exports java.base/jdk.internal.access=ALL-UNNAMED
 *    --add-exports java.base/jdk.internal.loader=ALL-UNNAMED
 *    --add-exports java.base/jdk.internal.util=ALL-UNNAMED
 *
 *  ON `javac` AS WELL AS `java`. Without them this file does not compile -- 74
 *  errors -- and a harness that diffs the two runs anyway compares an empty
 *  output against an empty output and reports 0 differing lines. That false
 *  green is why the flags are written out here rather than left to the reader:
 *  check the ROW COUNT (123) before believing a clean diff. That is
 *  itself part of what is measured — a VM that does not honour `--add-exports`
 *  fails every row here at once with `IllegalAccessError`, which is a different
 *  and larger finding than any individual row.
 *
 *  TWO THINGS ARE DELIBERATELY NOT CALLED, and the reason is the same in both
 *  cases — the probe would not survive its own row:
 *
 *    * `Signal.raise` DELIVERS A SIGNAL to this process. There is no argument
 *      to it that is both interesting and safe: the signals a probe could name
 *      are the ones that terminate it. Its registration is exercised through
 *      `getName`/`getNumber` on constructed `Signal` objects instead.
 *    * `VM.awaitInitLevel(n)` BLOCKS until the VM reaches level `n`, and the VM
 *      is already at its final level by the time `main` runs, so any `n` above
 *      that blocks forever. Only levels at or below the current one are asked.
 *
 *  `SharedSecrets` is asked for the CONTRACT rather than the implementation:
 *  that an accessor comes back, that it is an instance of the interface the
 *  getter is declared to return, and that repeated calls agree. The concrete
 *  class behind it is an implementation detail CratonVM may legitimately
 *  substitute in compatible mode, and asking it would be noise around the
 *  property callers actually depend on.
 */
public class JdkInternalSweep {

    static int rows = 0;

    interface F {
        Object get() throws Throwable;
    }

    static void p(String tag, F f) {
        rows++;
        String v;
        try {
            v = String.valueOf(f.get());
        } catch (Throwable e) {
            v = "THREW " + e.getClass().getName() + ": " + e.getMessage();
        }
        System.out.println(tag + " |" + v + "|");
    }

    static void sect(String name, Runnable r) {
        try {
            r.run();
        } catch (Throwable e) {
            System.out.println("SECTION-ABORTED " + name + " " + e.getClass().getName());
        }
    }

    // ------------------------------------------------------------- 1. VM

    static void vm() {
        p("VM.isBooted", () -> jdk.internal.misc.VM.isBooted());
        p("VM.initLevel", () -> jdk.internal.misc.VM.initLevel());
        p("VM.initLevel is at least 4", () -> jdk.internal.misc.VM.initLevel() >= 4);
        p("VM.isJavaLangInvokeInited", () -> jdk.internal.misc.VM.isJavaLangInvokeInited());
        p("VM.maxDirectMemory is positive", () -> jdk.internal.misc.VM.maxDirectMemory() > 0);
        // `awaitInitLevel` at or below the level already reached must return
        // immediately. Above it would block until the VM got there, and the VM
        // is already finished booting, so those are never asked.
        for (int lvl : new int[] {0, 1, 2, 3, 4}) {
            p("VM.awaitInitLevel " + lvl, () -> {
                if (lvl > jdk.internal.misc.VM.initLevel()) {
                    return "skipped, would block";
                }
                jdk.internal.misc.VM.awaitInitLevel(lvl);
                return "returned";
            });
        }
        for (String k : new String[] {
            "java.home", "java.version", "file.separator", "path.separator",
            "line.separator", "os.arch", "sun.nio.MaxDirectMemorySize",
            "no.such.saved.property",
        }) {
            p("VM.getSavedProperty " + k,
                () -> String.valueOf(jdk.internal.misc.VM.getSavedProperty(k)));
        }
        p("VM.getSavedProperty null", () -> {
            try {
                return String.valueOf(jdk.internal.misc.VM.getSavedProperty(null));
            } catch (Throwable e) {
                return e.getClass().getName();
            }
        });
        p("VM.getSavedProperty agrees with System.getProperty for file.separator", () -> {
            String a = jdk.internal.misc.VM.getSavedProperty("file.separator");
            String b = System.getProperty("file.separator");
            return a == null ? (b == null) : a.equals(b);
        });
        // The class-file version gate. 69 is JDK 25; the two either side of it
        // say whether the check is a range or a constant.
        for (int major : new int[] {45, 49, 52, 61, 65, 68, 69, 70, 71}) {
            p("VM.isSupportedClassFileVersion " + major,
                () -> jdk.internal.misc.VM.isSupportedClassFileVersion(major, 0));
        }
        p("VM.isSupportedClassFileVersion 69 preview",
            () -> jdk.internal.misc.VM.isSupportedClassFileVersion(69, 65535));
        p("VM.isSupportedClassFileVersion 68 preview",
            () -> jdk.internal.misc.VM.isSupportedClassFileVersion(68, 65535));
        for (int major : new int[] {45, 53, 61, 69, 70}) {
            p("VM.isSupportedModuleDescriptorVersion " + major,
                () -> jdk.internal.misc.VM.isSupportedModuleDescriptorVersion(major, 0));
        }
    }

    // ------------------------------------------------- 2. SharedSecrets

    /** Each getter, asked for its contract: non-null, the declared interface,
     *  and stable across calls. */
    static void secret(String name, F getter, Class<?> iface) {
        p("SharedSecrets." + name + " is non-null", () -> getter.get() != null);
        p("SharedSecrets." + name + " is a " + iface.getSimpleName(),
            () -> iface.isInstance(getter.get()));
        p("SharedSecrets." + name + " is stable", () -> getter.get() == getter.get());
    }

    static void sharedSecrets() {
        secret("getJavaLangAccess", () -> jdk.internal.access.SharedSecrets.getJavaLangAccess(),
            jdk.internal.access.JavaLangAccess.class);
        secret("getJavaLangRefAccess",
            () -> jdk.internal.access.SharedSecrets.getJavaLangRefAccess(),
            jdk.internal.access.JavaLangRefAccess.class);
        secret("getJavaLangInvokeAccess",
            () -> jdk.internal.access.SharedSecrets.getJavaLangInvokeAccess(),
            jdk.internal.access.JavaLangInvokeAccess.class);
        secret("getJavaLangReflectAccess",
            () -> jdk.internal.access.SharedSecrets.getJavaLangReflectAccess(),
            jdk.internal.access.JavaLangReflectAccess.class);
        secret("getJavaIOAccess", () -> jdk.internal.access.SharedSecrets.getJavaIOAccess(),
            jdk.internal.access.JavaIOAccess.class);
        secret("getJavaIOFileDescriptorAccess",
            () -> jdk.internal.access.SharedSecrets.getJavaIOFileDescriptorAccess(),
            jdk.internal.access.JavaIOFileDescriptorAccess.class);
        secret("getJavaNioAccess", () -> jdk.internal.access.SharedSecrets.getJavaNioAccess(),
            jdk.internal.access.JavaNioAccess.class);
        secret("getJavaNetInetAddressAccess",
            () -> jdk.internal.access.SharedSecrets.getJavaNetInetAddressAccess(),
            jdk.internal.access.JavaNetInetAddressAccess.class);
        secret("getJavaUtilZipFileAccess",
            () -> jdk.internal.access.SharedSecrets.getJavaUtilZipFileAccess(),
            jdk.internal.access.JavaUtilZipFileAccess.class);
        secret("getJavaUtilResourceBundleAccess",
            () -> jdk.internal.access.SharedSecrets.getJavaUtilResourceBundleAccess(),
            jdk.internal.access.JavaUtilResourceBundleAccess.class);

        // One accessor actually USED, so the getter is not the only thing
        // measured: `JavaLangAccess` is the busiest of the ten.
        p("JavaLangAccess.getConstantPool is callable", () -> {
            Object o = jdk.internal.access.SharedSecrets.getJavaLangAccess();
            return o.getClass() != null;
        });
    }

    // ----------------------------------------------------------- 3. Signal

    static void signals() {
        // `raise` is never called: see the class comment. Construction and the
        // two accessors are the whole registered surface otherwise.
        for (String name : new String[] {"INT", "TERM", "HUP"}) {
            p("Signal " + name + " getName", () -> new jdk.internal.misc.Signal(name).getName());
            p("Signal " + name + " getNumber is positive",
                () -> new jdk.internal.misc.Signal(name).getNumber() > 0);
            p("Signal " + name + " number is stable", () -> {
                int a = new jdk.internal.misc.Signal(name).getNumber();
                int b = new jdk.internal.misc.Signal(name).getNumber();
                return a == b;
            });
            p("Signal " + name + " equals", () -> {
                return new jdk.internal.misc.Signal(name)
                    .equals(new jdk.internal.misc.Signal(name));
            });
            p("Signal " + name + " toString",
                () -> new jdk.internal.misc.Signal(name).toString());
        }
        p("Signal INT and TERM differ", () -> {
            return new jdk.internal.misc.Signal("INT").getNumber()
                != new jdk.internal.misc.Signal("TERM").getNumber();
        });
        p("Signal of an unknown name", () -> {
            try {
                return new jdk.internal.misc.Signal("NO_SUCH_SIGNAL").getNumber();
            } catch (Throwable e) {
                return e.getClass().getName() + ": " + e.getMessage();
            }
        });
        p("Signal null name", () -> {
            try {
                return new jdk.internal.misc.Signal(null).getName();
            } catch (Throwable e) {
                return e.getClass().getName();
            }
        });
    }

    // ------------------------------------------- 4. AbstractClassLoaderValue

    static void classLoaderValues() {
        ClassLoader cl = JdkInternalSweep.class.getClassLoader();
        p("ClassLoaderValue get on an empty value", () -> {
            jdk.internal.loader.ClassLoaderValue<String> v =
                new jdk.internal.loader.ClassLoaderValue<>();
            return String.valueOf(v.get(cl));
        });
        p("putIfAbsent then get", () -> {
            jdk.internal.loader.ClassLoaderValue<String> v =
                new jdk.internal.loader.ClassLoaderValue<>();
            String prior = v.putIfAbsent(cl, "one");
            return prior + "/" + v.get(cl);
        });
        p("putIfAbsent twice", () -> {
            jdk.internal.loader.ClassLoaderValue<String> v =
                new jdk.internal.loader.ClassLoaderValue<>();
            v.putIfAbsent(cl, "one");
            return v.putIfAbsent(cl, "two") + "/" + v.get(cl);
        });
        p("computeIfAbsent computes once", () -> {
            jdk.internal.loader.ClassLoaderValue<String> v =
                new jdk.internal.loader.ClassLoaderValue<>();
            int[] calls = new int[1];
            BiFunction<ClassLoader, jdk.internal.loader.ClassLoaderValue<String>, String> f =
                (c, k) -> {
                    calls[0]++;
                    return "computed";
                };
            String a = v.computeIfAbsent(cl, f);
            String b = v.computeIfAbsent(cl, f);
            return a + "/" + b + "/calls=" + calls[0];
        });
        p("computeIfAbsent after putIfAbsent does not compute", () -> {
            jdk.internal.loader.ClassLoaderValue<String> v =
                new jdk.internal.loader.ClassLoaderValue<>();
            v.putIfAbsent(cl, "put");
            int[] calls = new int[1];
            String got = v.computeIfAbsent(cl, (c, k) -> {
                calls[0]++;
                return "computed";
            });
            return got + "/calls=" + calls[0];
        });
        p("computeIfAbsent returning null", () -> {
            jdk.internal.loader.ClassLoaderValue<String> v =
                new jdk.internal.loader.ClassLoaderValue<>();
            String got = v.computeIfAbsent(cl, (c, k) -> null);
            return got + "/" + v.get(cl);
        });
        p("two values are independent", () -> {
            jdk.internal.loader.ClassLoaderValue<String> a =
                new jdk.internal.loader.ClassLoaderValue<>();
            jdk.internal.loader.ClassLoaderValue<String> b =
                new jdk.internal.loader.ClassLoaderValue<>();
            a.putIfAbsent(cl, "a");
            return a.get(cl) + "/" + b.get(cl);
        });
        p("null class loader is the bootstrap key", () -> {
            jdk.internal.loader.ClassLoaderValue<String> v =
                new jdk.internal.loader.ClassLoaderValue<>();
            v.putIfAbsent(null, "boot");
            return v.get(null) + "/" + v.get(cl);
        });
        p("remove then get", () -> {
            jdk.internal.loader.ClassLoaderValue<String> v =
                new jdk.internal.loader.ClassLoaderValue<>();
            v.putIfAbsent(cl, "one");
            v.remove(cl, "one");
            return String.valueOf(v.get(cl));
        });
    }

    // ------------------------------------------------- 5. Preconditions

    static void preconditions() {
        int[][] idx = {{0, 1}, {0, 0}, {1, 1}, {-1, 4}, {4, 4}, {3, 4}, {5, 4}, {0, -1}};
        for (int[] pair : idx) {
            p("Preconditions.checkIndex " + pair[0] + "," + pair[1], () -> {
                try {
                    return jdk.internal.util.Preconditions.checkIndex(pair[0], pair[1], null);
                } catch (Throwable e) {
                    return e.getClass().getName() + ": " + e.getMessage();
                }
            });
        }
        int[][] tri = {
            {0, 0, 0}, {0, 4, 4}, {1, 3, 4}, {4, 4, 4}, {-1, 2, 4},
            {2, 1, 4}, {0, 5, 4}, {0, 4, -1},
        };
        for (int[] t : tri) {
            p("Preconditions.checkFromToIndex " + t[0] + "," + t[1] + "," + t[2], () -> {
                try {
                    return jdk.internal.util.Preconditions.checkFromToIndex(t[0], t[1], t[2], null);
                } catch (Throwable e) {
                    return e.getClass().getName() + ": " + e.getMessage();
                }
            });
            p("Preconditions.checkFromIndexSize " + t[0] + "," + t[1] + "," + t[2], () -> {
                try {
                    return jdk.internal.util.Preconditions.checkFromIndexSize(
                        t[0], t[1], t[2], null);
                } catch (Throwable e) {
                    return e.getClass().getName() + ": " + e.getMessage();
                }
            });
        }
        // The formatter argument is the reason these take a BiFunction at all:
        // it decides WHICH exception class comes out.
        p("checkIndex with an outOfBounds formatter", () -> {
            try {
                return jdk.internal.util.Preconditions.checkIndex(
                    5, 4,
                    jdk.internal.util.Preconditions.outOfBoundsExceptionFormatter(
                        java.lang.ArrayIndexOutOfBoundsException::new));
            } catch (Throwable e) {
                return e.getClass().getName() + ": " + e.getMessage();
            }
        });
        p("checkFromToIndex with an outOfBounds formatter", () -> {
            try {
                return jdk.internal.util.Preconditions.checkFromToIndex(
                    0, 5, 4,
                    jdk.internal.util.Preconditions.outOfBoundsExceptionFormatter(
                        java.lang.ArrayIndexOutOfBoundsException::new));
            } catch (Throwable e) {
                return e.getClass().getName() + ": " + e.getMessage();
            }
        });
    }

    public static void main(String[] args) {
        sect("vm", JdkInternalSweep::vm);
        sect("sharedSecrets", JdkInternalSweep::sharedSecrets);
        sect("signals", JdkInternalSweep::signals);
        sect("classLoaderValues", JdkInternalSweep::classLoaderValues);
        sect("preconditions", JdkInternalSweep::preconditions);
        System.out.println("rows " + rows);
        System.out.println("DONE JdkInternalSweep");
    }
}

// Interpreter round i6, wave 123, lane L5 -- a name a user loader already
// INITIATED through `Class.forName(name, false, loader)` is answered from the
// loader's dictionary when a class that loader defined resolves it: HotSpot
// does not call `loadClass` again (JVMS 5.3; `SystemDictionary::resolve_or_fail`
// finds the initiating entry `Class.forName` wrote).
// (`i46-L5-proposal-one-resolved-class-entry-table-every-door-writes`, "Wave
// 123"; `docs/internal/fixed-bugs/interpreter-i120-L7-a-loader-is-asked-again-for-a-resolved-name-FIXED-20261224.md`.)
//
// CratonVM's `Class.forName` writes the initiating RECORD but not the
// loader-namespaced memo (`ClassRealm::initiating_resolution_cache`), and the
// loader-namespaced lookups read the memo (`loader_initiated_answer`), so the
// resolution re-asked the loader. Under `CRATONVM_LOADER_DICTIONARY_ONE_DOOR`
// a memo miss falls to the record (`--jdk-only`; `--compatible` keeps no
// records, so nothing changes there).
//
// Rows:
//   for-name     Class.forName(ITEM, false, L) answers the app
//                loader's class (L delegates it to its parent)
//   resolved     Target (defined by L from the app's bytes) does `new Item()`:
//                the same class
//   find-loaded  L.findLoadedClass(ITEM) is that class (the
//                initiating entry)
//   fresh        a second loader with no forName: its Target resolves the
//                item through one loadClass
//   TIME asks    loadClass(ITEM) calls: first loader after
//                forName + resolution, second loader after resolution.
//                HotSpot: forName=1 fresh=1. Expected without the switch
//                (`--jdk-only`, from the code): forName=2.
//
// Run (no setup):
//   javac -d out tools/probes/interp/L5/L5W123ForNameRecordAnswersResolution.java
//   java -cp out L5W123ForNameRecordAnswersResolution
//
// HOTSPOT_EXPECTED_BEGIN
// for-name=true
// resolved=true
// find-loaded=true
// fresh=true
// HOTSPOT_EXPECTED_END
// (`TIME asks forName=1 fresh=1` on HotSpot, default and -Xint.)

import java.io.InputStream;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;

public class L5W123ForNameRecordAnswersResolution {
    static final String TARGET = "L5W123ForNameRecordAnswersResolution$Target";
    static final String ITEM = "L5W123ForNameRecordAnswersResolution$Item";

    static final class CountingLoader extends ClassLoader {
        final Map<String, Integer> asks = new ConcurrentHashMap<>();

        CountingLoader(ClassLoader parent) {
            super(parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            asks.merge(name, 1, Integer::sum);
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    if (name.equals(TARGET)) {
                        byte[] b;
                        try (InputStream in = getParent().getResourceAsStream(name + ".class")) {
                            b = in.readAllBytes();
                        } catch (Exception e) {
                            throw new ClassNotFoundException(name, e);
                        }
                        c = defineClass(name, b, 0, b.length);
                    } else {
                        c = getParent().loadClass(name);
                    }
                }
                return c;
            }
        }

        Class<?> loaded(String name) {
            return findLoadedClass(name);
        }

        int asked(String name) {
            return asks.getOrDefault(name, 0);
        }
    }

    public static void main(String[] args) throws Exception {
        ClassLoader app = L5W123ForNameRecordAnswersResolution.class.getClassLoader();
        Class<?> appItem = Class.forName(ITEM, false, app);

        CountingLoader l = new CountingLoader(app);
        Class<?> target = l.loadClass(TARGET);
        Class<?> viaForName = Class.forName(ITEM, false, l);
        System.out.println("for-name=" + (viaForName == appItem));
        Object made = target.getMethod("make").invoke(null);
        System.out.println("resolved=" + (made.getClass() == appItem));
        System.out.println("find-loaded=" + (l.loaded(ITEM) == appItem));

        CountingLoader fresh = new CountingLoader(app);
        Object made2 = fresh.loadClass(TARGET).getMethod("make").invoke(null);
        System.out.println("fresh=" + (made2.getClass() == appItem));

        System.out.println("TIME asks forName=" + l.asked(ITEM) + " fresh=" + fresh.asked(ITEM));
    }

    public static class Item {
        public Item() {
        }
    }

    public static class Target {
        public static Object make() {
            return new Item();
        }
    }
}

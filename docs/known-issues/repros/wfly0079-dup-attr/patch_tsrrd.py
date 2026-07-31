#!/usr/bin/env python3
"""Instrument WildFly's TransactionSubsystemRootResourceDefinition.registerAttributes.

Adds two things, both inert unless -Dcvm.dupattr.reps is set:

  * cvmDupAttrCanary(): repeats the exact set-build + 12-remove sequence N times
    inside the real parallel-extension-add thread, and reports any element that
    survived its remove(). That is the only way 'hornetq-store-enable-async-io'
    can be registered twice (WFLYCTL0043 inside WFLYCTL0079).
  * cvmVerifyRemoved(): the same check on the ONE real sequence, always on.
"""
import sys

SRC = sys.argv[1]

with open(SRC, "r", encoding="utf-8") as f:
    text = f.read()

ANCHOR = """    @Override
    public void registerAttributes(ManagementResourceRegistration resourceRegistration) {
        // Register all attributes except of the mutual ones
        Set<AttributeDefinition> attributesWithoutMutuals = new HashSet<>(Arrays.asList(add_attributes));"""
assert text.count(ANCHOR) == 1, "registerAttributes anchor not unique"

REPLACEMENT = """    // --- CratonVM WFLYCTL0079 canary (additive; inert without -Dcvm.dupattr.reps) ---
    private static final AttributeDefinition[] CVM_MUTUALS = {
        USE_HORNETQ_STORE_PARAM, USE_JOURNAL_STORE_PARAM, USE_JDBC_STORE,
        STATISTICS_ENABLED, DEFAULT_TIMEOUT, MAXIMUM_TIMEOUT, JDBC_STORE_DATASOURCE,
        PROCESS_ID_UUID, PROCESS_ID_SOCKET_BINDING, PROCESS_ID_SOCKET_MAX_PORTS,
        ENABLE_STATISTICS, HORNETQ_STORE_ENABLE_ASYNC_IO,
    };

    /**
     * Names that survived their remove() -- i.e. that would be registered twice.
     * Compares by name while ITERATING the set, exactly as registerAttributes'
     * own loop does, so a broken contains()/hash cannot mask the failure.
     */
    private static String cvmSurvivors(Set<AttributeDefinition> set) {
        StringBuilder sb = null;
        for (AttributeDefinition mutual : CVM_MUTUALS) {
            for (AttributeDefinition present : set) {
                if (present.getName().equals(mutual.getName())) {
                    if (sb == null) {
                        sb = new StringBuilder();
                    }
                    sb.append(mutual.getName())
                      .append("(same=").append(present == mutual)
                      .append(",eq=").append(present.equals(mutual))
                      .append(",h=").append(mutual.hashCode())
                      .append('/').append(present.hashCode())
                      .append(",nh=").append(mutual.getName().hashCode())
                      .append('/').append(present.getName().hashCode())
                      .append(") ");
                    break;
                }
            }
        }
        return sb == null ? null : sb.toString();
    }

    /**
     * NEGATIVE CONTROL. With -Dcvm.dupattr.selftest=1 the
     * hornetq-store-enable-async-io remove is skipped, leaving exactly the
     * state a real failure produces. Every canary mode MUST then report a
     * failure — otherwise a clean campaign proves nothing about the canary.
     */
    private static final boolean CVM_SELFTEST =
            System.getProperty("cvm.dupattr.selftest") != null;

    private static Set<AttributeDefinition> cvmBuildWithoutMutuals() {
        Set<AttributeDefinition> s = new HashSet<>(Arrays.asList(add_attributes));
        for (AttributeDefinition mutual : CVM_MUTUALS) {
            if (CVM_SELFTEST && mutual == HORNETQ_STORE_ENABLE_ASYNC_IO) {
                continue;
            }
            s.remove(mutual);
        }
        return s;
    }

    /**
     * A COLD replica of add_attributes: brand-new AttributeDefinition objects
     * whose names are brand-new String instances, so every rep starts with
     * young-gen objects and un-cached String hashes -- the state the one real
     * registerAttributes() call runs in, and the state a warm loop over the
     * shared statics can never revisit.
     */
    private static AttributeDefinition[] cvmFreshAttrs() {
        AttributeDefinition[] a = new AttributeDefinition[add_attributes.length];
        for (int i = 0; i < a.length; i++) {
            String n = new String(add_attributes[i].getName().toCharArray());
            a[i] = new SimpleAttributeDefinitionBuilder(n, ModelType.BOOLEAN)
                    .setAllowExpression(true).build();
        }
        return a;
    }

    private static String cvmSurvivorsFresh(Set<AttributeDefinition> set,
                                            AttributeDefinition[] fresh) {
        StringBuilder sb = null;
        for (AttributeDefinition mutual : CVM_MUTUALS) {
            for (AttributeDefinition present : set) {
                if (present.getName().equals(mutual.getName())) {
                    if (sb == null) {
                        sb = new StringBuilder();
                    }
                    sb.append(mutual.getName())
                      .append("(nh=").append(mutual.getName().hashCode())
                      .append('/').append(present.getName().hashCode())
                      .append(",ah=").append(mutual.hashCode())
                      .append('/').append(present.hashCode())
                      .append(",same=").append(present == fresh[cvmIndexOf(fresh, mutual)])
                      .append(") ");
                    break;
                }
            }
        }
        return sb == null ? null : sb.toString();
    }

    private static int cvmIndexOf(AttributeDefinition[] arr, AttributeDefinition want) {
        for (int i = 0; i < arr.length; i++) {
            if (arr[i].getName().equals(want.getName())) {
                return i;
            }
        }
        return 0;
    }

    /**
     * The OTHER way this name can be reported twice: a false-positive
     * containsKey() in ConcreteResourceRegistration.storeAttribute, whose
     * `attributes` field is a plain HashMap&lt;String, AttributeAccess&gt;.
     * Replays the exact registration order against a String-keyed HashMap.
     */
    private static String cvmRegistrySequence() {
        java.util.Map<String, Object> registered = new java.util.HashMap<>();
        Set<AttributeDefinition> loop = cvmBuildWithoutMutuals();
        for (AttributeDefinition def : loop) {
            String n = def.getName();
            if (registered.containsKey(n)) {
                return "loop:" + n;
            }
            registered.put(n, def);
        }
        for (AttributeDefinition mutual : CVM_MUTUALS) {
            String n = mutual.getName();
            if (registered.containsKey(n)) {
                return "explicit:" + n + " mapSize=" + registered.size();
            }
            registered.put(n, mutual);
        }
        if (registered.size() != add_attributes.length) {
            return "mapSize=" + registered.size() + " expected=" + add_attributes.length;
        }
        return null;
    }

    private static final java.util.concurrent.atomic.AtomicInteger CVM_BAD =
            new java.util.concurrent.atomic.AtomicInteger();

    /**
     * The real failure happens once, on ONE of ~40 threads that are all
     * initializing extensions at the same time. A single-threaded rep loop
     * reproduces the rate but not the concurrency -- and its later reps run
     * after every other extension has finished, in a quiet VM. Running the
     * canary on several threads restores the concurrent-allocation profile
     * and multiplies the rate at the same time.
     */
    private static void cvmDupAttrCanary() {
        int reps = Integer.getInteger("cvm.dupattr.reps", 0);
        if (reps <= 0) {
            return;
        }
        int nthreads = Integer.getInteger("cvm.dupattr.threads", 1);
        if (nthreads <= 1) {
            cvmCanaryLoop(reps);
        } else {
            Thread[] ts = new Thread[nthreads];
            for (int t = 0; t < nthreads; t++) {
                ts[t] = new Thread(() -> cvmCanaryLoop(reps), "cvm-dupattr-" + t);
                ts[t].start();
            }
            for (Thread t : ts) {
                try {
                    t.join();
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                }
            }
        }
        System.out.println("CVM-DUPATTR-CANARY done reps=" + reps
                + " threads=" + Math.max(nthreads, 1)
                + " bad=" + CVM_BAD.get()
                + " thread=" + Thread.currentThread().getName());
        System.out.flush();
    }

    private static void cvmCanaryLoop(int reps) {
        int expected = add_attributes.length - CVM_MUTUALS.length;
        int bad = 0;
        for (int i = 0; i < reps; i++) {
            if ((i % 3) == 2) {
                // registry-side: String-keyed HashMap containsKey/put, in the
                // order registerAttributes actually registers
                String dup = cvmRegistrySequence();
                if (dup != null) {
                    bad++;
                    System.out.println("CVM-DUPATTR-CANARY FAIL mode=registry rep=" + i
                            + " dup=" + dup
                            + " thread=" + Thread.currentThread().getName());
                    System.out.flush();
                }
            } else if ((i & 1) == 0) {
                // warm: the shared statics, exactly as registerAttributes uses them
                Set<AttributeDefinition> s = cvmBuildWithoutMutuals();
                String survivors = cvmSurvivors(s);
                if (survivors != null || s.size() != expected) {
                    bad++;
                    System.out.println("CVM-DUPATTR-CANARY FAIL mode=warm rep=" + i
                            + " size=" + s.size() + " expected=" + expected
                            + " survivors=" + survivors
                            + " thread=" + Thread.currentThread().getName());
                    System.out.flush();
                }
            } else {
                // cold: fresh objects + fresh name Strings every rep
                AttributeDefinition[] fresh = cvmFreshAttrs();
                Set<AttributeDefinition> s = new HashSet<>(Arrays.asList(fresh));
                for (AttributeDefinition mutual : CVM_MUTUALS) {
                    if (CVM_SELFTEST && mutual == HORNETQ_STORE_ENABLE_ASYNC_IO) {
                        continue;
                    }
                    s.remove(fresh[cvmIndexOf(fresh, mutual)]);
                }
                String survivors = cvmSurvivorsFresh(s, fresh);
                if (survivors != null || s.size() != expected) {
                    bad++;
                    System.out.println("CVM-DUPATTR-CANARY FAIL mode=cold rep=" + i
                            + " size=" + s.size() + " expected=" + expected
                            + " survivors=" + survivors
                            + " thread=" + Thread.currentThread().getName());
                    System.out.flush();
                }
            }
        }
        CVM_BAD.addAndGet(bad);
    }

    @Override
    public void registerAttributes(ManagementResourceRegistration resourceRegistration) {
        cvmDupAttrCanary();
        // Register all attributes except of the mutual ones
        Set<AttributeDefinition> attributesWithoutMutuals = new HashSet<>(Arrays.asList(add_attributes));"""

text = text.replace(ANCHOR, REPLACEMENT)

ANCHOR2 = """        OperationStepHandler writeHandler = new ReloadRequiredWriteAttributeHandler(attributesWithoutMutuals);
        for(final AttributeDefinition def : attributesWithoutMutuals) {"""
assert text.count(ANCHOR2) == 1, "writeHandler anchor not unique"
REPLACEMENT2 = """        if (CVM_SELFTEST) {
            // End-to-end negative control: put the attribute back, which is
            // precisely the state a failed remove() leaves. The boot must then
            // report CVM-DUPATTR-REAL FAIL *and* die with the real
            // WFLYCTL0043, proving both the canary and wfboot.sh's HIT
            // classification can actually fire.
            attributesWithoutMutuals.add(HORNETQ_STORE_ENABLE_ASYNC_IO);
        }
        String cvmSurvivors = cvmSurvivors(attributesWithoutMutuals);
        if (cvmSurvivors != null) {
            System.out.println("CVM-DUPATTR-REAL FAIL size=" + attributesWithoutMutuals.size()
                    + " survivors=" + cvmSurvivors
                    + " thread=" + Thread.currentThread().getName());
            System.out.flush();
        }

        OperationStepHandler writeHandler = new ReloadRequiredWriteAttributeHandler(attributesWithoutMutuals);
        for(final AttributeDefinition def : attributesWithoutMutuals) {"""
text = text.replace(ANCHOR2, REPLACEMENT2)

with open(SRC, "w", encoding="utf-8") as f:
    f.write(text)

print("patched", SRC)

# `WFLYCTL0079` / `WFLYCTL0043` duplicate-attribute reproduction harness

Everything needed to hunt the rare WildFly 32 boot failure

```
WFLYCTL0079 / WFLYCTL0043: An attribute named 'hornetq-store-enable-async-io'
is already registered at location '/subsystem=transactions'
```

See `docs/internal/fixed-suite-bugs/wildfly/wflyctl0079-duplicate-attribute-registration.md`
for the analysis. The short version: that message is reachable only if
`TransactionSubsystemRootResourceDefinition.registerAttributes()`'s
`attributesWithoutMutuals.remove(HORNETQ_STORE_ENABLE_ASYNC_IO)` failed to
delete its element, or if the registry's own `HashMap<String, AttributeAccess>`
answered `containsKey` positively for a name registered only once.

## Standalone probes (no WildFly needed)

```bash
javac -d classes WflyAttrSetProbe.java HashSetInitProbe.java
CRATONVM_JAVA_HOME=<real jdk> cratonvm -cp classes WflyAttrSetProbe 8 200000 2 8
CRATONVM_JAVA_HOME=<real jdk> cratonvm -cp classes HashSetInitProbe 20000 28
```

* `WflyAttrSetProbe` models `registerAttributes` exactly: build
  `new HashSet<>(Arrays.asList(add_attributes))`, run the 12 removes, then
  replay the registration order against a `HashMap<String, ?>`. Args:
  `<threads> <rounds> <mode 0=warm|1=cold|2=alternate> <garbageKB>`.
  Mode 1 rebuilds the attribute objects and their name `String`s every round,
  so the `String` hash cache starts cold exactly as it does in a real boot.
* `HashSetInitProbe` isolates `new HashSet<>(collection)` /
  `new LinkedHashSet<>(collection)` plus a remove-everything pass. Aim it at
  the constructor's rooting with
  `CRATONVM_MOVING_YOUNG=1 CRATONVM_DBG_FORCE_MOVING=1 CRATONVM_DBG_GC_STRESS=262144`.

Both are byte-clean on HotSpot; a `PROBE-FAILED` line is a CratonVM defect.

## Real WildFly boots

Needs a WildFly 32 distribution (the campaigns used
`/data/data/wildfly-dist-keep/wildfly-32.0.1.Final` on the Azure build host)
and a `JAVA_HOME`-shaped directory whose `bin/java` execs CratonVM with
`CRATONVM_JAVA_HOME` pointing at a real JDK.

```bash
./wfboot.sh <slot> <outdir> <cratonvm-binary> <canary-reps>   # one boot
./wfcampaign.sh <slot> <boots> <reps> <binary> <tag>          # N boots in a slot
./wfgcstat.sh                                                 # one boot + GC summary
```

`wfboot.sh` exits 3 on a hit (canary failure, `WFLYCTL0043`, or "already
registered") and copies the console log aside. Run one slot per ~1.2 GB of
free RAM; boots are ~30 s plus ~3 ms per canary rep.

## In-boot amplification

`patch_tsrrd.py` rewrites WildFly's own
`TransactionSubsystemRootResourceDefinition.java` to repeat the suspect
sequence `-Dcvm.dupattr.reps=N` times **inside the real
`parallel-extension-add` thread**, cycling three modes:

* `warm` — the shared static `add_attributes`, exactly as the real call uses them;
* `cold` — freshly built `AttributeDefinition`s with fresh name `String`s, so
  every rep has the young-gen/uncached-hash profile of the one real call;
* `registry` — the `HashMap<String, AttributeAccess>` `containsKey`/`put`
  sequence from `ConcreteResourceRegistration.storeAttribute`.

It also leaves an always-on check on the ONE real sequence
(`CVM-DUPATTR-REAL FAIL`), so an unamplified occurrence is still caught.

```bash
cp <wildfly-src>/transactions/src/main/java/org/jboss/as/txn/subsystem/\
TransactionSubsystemRootResourceDefinition.java patch/src/org/jboss/as/txn/subsystem/
python3 patch_tsrrd.py patch/src/org/jboss/as/txn/subsystem/TransactionSubsystemRootResourceDefinition.java
CP=$(find $WF/modules -name '*.jar' | tr '\n' ':')
javac -nowarn -proc:none -cp "$CP" -d patch/classes patch/src/org/jboss/as/txn/subsystem/*.java
./canary_toggle.sh on     # jars it and puts it ahead of the shipped jar in module.xml
./canary_toggle.sh off    # restores module.xml from module.xml.orig
```

Amplification is worth ~`reps`× per boot: at `reps=6000` one boot is worth
about 6000 registrations, against a historical failure rate near 1 per 1400
boots.

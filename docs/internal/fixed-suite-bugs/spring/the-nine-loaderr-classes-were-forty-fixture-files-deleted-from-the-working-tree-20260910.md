# The 9 `LOADERR` classes were 40 fixture files deleted from the Spring checkout's working tree — restored, and all 9 pass on both VMs

| | |
|---|---|
| **Status** | **RESOLVED 2026-09-10.** Fixture restored on the Azure host; 9/9 `OK` under CratonVM and under stock HotSpot 25, byte-identical counts. |
| **Supersedes** | `docs/known-issues/spring/not-a-bug-nine-loaderr-classes-are-a-removed-aoptarget-package-20260909.md`, whose disposition ("not a CratonVM bug") was right and whose **mechanism was wrong**. |
| **Scope** | `apps/spring-framework` on Azure `20.80.105.49` — the shared suite fixture, not the CratonVM tree. |

## What the 2026-09-09 page said, and what was actually true

That page concluded:

> The package was removed or relocated upstream at some point before this
> checkout's revision, and whatever generated the suite's class-discovery list
> did so from an older revision (or a stale cached list) that still names it.

Two checks contradict it:

```console
$ cd apps/spring-framework
$ git ls-tree -r --name-only HEAD | grep -c 'aop/target/'
23                                    # they are in HEAD. Nothing was removed upstream.

$ git status --short | grep -c '^ D'
40                                    # they are DELETED from the working tree, uncommitted
```

The class-discovery list was correct. The **fixture** was damaged: 40 tracked
files — 23 main sources, 12 test sources, 5 test resources — deleted from the
working tree of the Spring Framework checkout and never committed. Directory
mtime dates the deletion to **2026-09-01 22:11 UTC**; `git reflog` shows a
single `clone` and no branch movement, so nothing in git did this.

The 2026-09-09 page's own evidence was consistent with both stories and could
not separate them: `find` returned nothing because the files were gone from
disk, and HotSpot failed identically because a `.class` that does not exist
cannot be loaded by any VM. What it never asked was whether git still had them.

## The 40 files

```text
spring-aop/src/main/java/org/springframework/aop/target/**                (19)
spring-aop/src/main/java/org/springframework/aop/framework/autoproxy/target/**  (4)
spring-aop/src/test/java/org/springframework/aop/target/**                (8)
spring-aop/src/test/resources/org/springframework/aop/target/*.xml        (7)
spring-context/src/test/java/org/springframework/aop/target/CommonsPool2TargetSourceTests.java
spring-context/src/test/resources/org/springframework/aop/target/CommonsPool2TargetSourceTests-context.xml
```

## Why only nine classes broke, and not the whole AOP suite

`SingletonTargetSource` and `EmptyTargetSource` live in that package and are
reached by essentially all of Spring AOP, so their absence should have been
catastrophic. It was not, because the runner's classpath takes a module's own
*main* classes from `build/libs/<module>.jar`, and that jar was built
**2026-08-10**, three weeks before the deletion:

```console
$ unzip -l spring-aop/build/libs/spring-aop-7.1.0-SNAPSHOT.jar | grep -c 'aop/target/'
22
```

The jar still carries every main class. Only the *test* classes come from
`build/classes/java/test`, which was regenerated after the deletion and
therefore lost exactly the 9 test classes. That asymmetry is what made the
damage look like a tidy upstream package removal instead of a partial one.

**Worth keeping:** a stale build artefact hid a damaged source tree for nine
days. A fixture whose jars predate its sources cannot be used to argue that
the sources are intact.

## The repair

```bash
cd apps/spring-framework
git status --short | grep '^ D' | awk '{print $2}' > /tmp/deleted40.txt
git checkout -- $(cat /tmp/deleted40.txt)

# recompile just the restored test classes against Gradle's own dumped classpath
javac -nowarn -proc:none -d spring-aop/build/classes/java/test \
      -cp "$(cat spring-aop/build/cratonvm-testcp.txt)" \
      $(find spring-aop/src/test/java/org/springframework/aop/target -name '*.java')
javac -nowarn -proc:none -d spring-context/build/classes/java/test \
      -cp "$(cat spring-context/build/cratonvm-testcp.txt)" \
      spring-context/src/test/java/org/springframework/aop/target/CommonsPool2TargetSourceTests.java

# and stage the XML fixtures the tests load from the classpath
cp spring-aop/src/test/resources/org/springframework/aop/target/*.xml \
   spring-aop/build/resources/test/org/springframework/aop/target/
cp spring-context/src/test/resources/org/springframework/aop/target/*.xml \
   spring-context/build/resources/test/org/springframework/aop/target/
```

Both `javac` runs are clean — no errors, no warnings — which on its own says
the sources match the rest of the checkout.

## The result

```console
$ ./run-suite.sh hotspot --list aoptarget9.tsv --tag aoptarget9-hs
classes: OK=9
test-methods: found=29 passed=28 failed=0   sum-class-ms=8744   wall=9s

$ CRATONVM_BIN=cratonvm-springres-20260910 ./run-suite.sh run --list aoptarget9.tsv --tag aoptarget9-cvm
classes: OK=9
test-methods: found=29 passed=28 failed=0   sum-class-ms=9485   wall=13s
```

Identical class counts, identical method counts, identical pass counts. (The
one non-passing method of 29 is skipped on both VMs, not failed.)

## Census: the index was right everywhere else

The same question, asked of the whole index rather than of nine rows — for each
of the 2 848 indexed classes, does a `.class` exist under any module's
`build/classes/*/test`?

```bash
find . -path '*/build/classes/*/test/*' -name '*.class' ! -name '*$*' \
  | sed -E 's#.*/build/classes/[^/]+/test/##; s#\.class$##; s#/#.#g' | sort -u > present.txt
cut -f2 meta/all-classes.tsv | sort -u > indexed.txt
comm -23 indexed.txt present.txt
```

Exactly 9 rows come back, and they are exactly the 9 `LOADERR` classes. No
other index row is stale. Run this before blaming a `LOADERR` on anything else.

## Disposition

Not a CratonVM defect — the 2026-09-09 disposition stands. But the corrective
action it recommended was the wrong one: it asked for the 9 rows to be *pruned
from the discovery list*, which would have permanently retired nine passing
Spring tests from the measured population to work around damage in a checkout.
The right action was to restore the checkout, and the nine classes are back in
the suite.

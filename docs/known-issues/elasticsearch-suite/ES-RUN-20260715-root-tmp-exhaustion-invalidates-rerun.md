# ES-RUN-20260715: Root /tmp Exhaustion Invalidated the Non-Passed Rerun

**Status:** Open infrastructure blocker; not a CratonVM semantic regression

## Scope

The eight-shard rerun es-nonpassed-8shard-currentdev-20260715-134544-restart
selected the 2,649 Elasticsearch classes that had not passed previously. It was
built from CratonVM commit 8ea4eb3a779c1f02fa22bcd6d5485b4a56939069.

The recorded result of PASS=1, FAIL=2,648, HANG=0, and CRASH=0 is invalid for
runtime-regression counting.

## Evidence

- All eight runner shards completed normally, so this is not a runner crash.
- 2,573 failure rows have the same normalized CratonVM diagnostic:
  BootstrapForTesting.<clinit> failed because it was unable to create the
  Elasticsearch test temporary directory.
- The nested cause is java.nio.file.AccessDeniedException; 73 rows expose it
  directly as java.nio.file.AccessDeniedException: /tmp.
- During the build and run investigation, the Azure host root filesystem,
  including /tmp, was 100% full. The separate /data/data filesystem still had
  capacity.

Elasticsearch initializes BootstrapForTesting before most test classes. Its
attempt to create a test temporary directory therefore failed before the class
under test ran, producing the broad fail cascade.

## Attribution

This is an Azure-runner storage configuration failure. It does not demonstrate
that the post-1520b2b8 CratonVM sources regressed from 1,254 passing classes to
one passing class. Do not create per-class CratonVM bug notes from this result,
and do not use its 2,648 failures in residual counts.

## Required Rerun Conditions

1. Keep Cargo, temporary build files, and targets on /data/data.
2. Give the Java test processes a unique writable temporary directory on
   /data/data with -Djava.io.tmpdir=<run-workdir>/tmp.
3. Also set TMPDIR, TMP, and TEMP to that directory for shell and native-tool
   use.
4. Verify the root and temporary filesystems have free space before launching
   the eight shards.
5. Rerun exactly the 2,649 prior non-passed classes and replace this invalid
   measurement with the new PASS/FAIL/HANG/CRASH totals.

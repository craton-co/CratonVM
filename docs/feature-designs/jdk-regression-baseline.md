# Curated JDK regression baseline

The [baseline table](../../vm/tests/jdk-regression-baseline.md) is the current
contract for `vm/tests/jck_conformance.rs`. It lives with the test so source
packages retain the gate's input. Update the table and `BASELINE_FLOORS` together.

The curated suite is an engineering regression test, not Oracle's JCK and not
evidence of Java certification. Table checks run without a VM or compiled
fixtures; corpus execution requires the synthetic-library configuration and
compiled Java fixtures.

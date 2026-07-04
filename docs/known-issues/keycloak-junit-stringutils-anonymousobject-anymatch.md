# Keycloak JUnit 5: synthetic AnonymousObject IntStream.anyMatch missing

Status: open

Date observed: 2026-07-03

## Summary

After the PreviewFeatures native fix, 64 `tests/base` rows fail during JUnit
discovery with a CratonVM synthetic class method-resolution error:

```text
NoSuchMethodError method="cratonvm/synthetic/AnonymousObject$1.anyMatch(Ljava/util/function/IntPredicate;)Z [class not found on any classpath entry - synthetic stub, add the missing jar]"
caller="org/junit/platform/commons/util/StringUtils.containsWhitespace(Ljava/lang/String;)Z @pc=18"
```

The runner records these as `FAIL` because JUnit exits normally with discovery
issues rather than CratonVM aborting the process.

## Evidence

Run:

```text
craton-azure-nonpassed-dev-20260703-previewfeatures-fixed-01
```

Count:

```text
64 FAIL rows
```

Representative log:

```text
/home/victor/wt-keycloak-previewfeatures-suite-20260703-01/apps/keycloak-suite-runner/.suite/results/craton-azure-nonpassed-dev-20260703-previewfeatures-fixed-01/others-jit/logs/tests_base.org.keycloak.tests.actions.RequiredActionUpdateProfileTest.err.log
```

Excerpt:

```text
NoSuchMethodError method="cratonvm/synthetic/AnonymousObject$1.anyMatch(Ljava/util/function/IntPredicate;)Z [class not found on any classpath entry - synthetic stub, add the missing jar]"
caller="org/junit/platform/commons/util/StringUtils.containsWhitespace(Ljava/lang/String;)Z @pc=18"
[cratonvm] System.exit(1) called - process terminating
```

## Current conclusion

JUnit's `StringUtils.containsWhitespace(String)` uses the Java stream path for
character scanning. CratonVM materializes part of that path as
`cratonvm/synthetic/AnonymousObject$1`, but that synthetic object does not
provide the `anyMatch(IntPredicate)Z` method expected by the call site.

This points to an incomplete synthetic implementation for an `IntStream`-like
object, not to a missing Keycloak jar.

## Next steps

- Build a small HotSpot/CratonVM probe around
  `org.junit.platform.commons.util.StringUtils.containsWhitespace`.
- Trace the receiver class at the `anyMatch(IntPredicate)` call site.
- Implement the missing primitive-stream method or route this JDK/JUnit path
  through the real stream implementation instead of an anonymous synthetic stub.

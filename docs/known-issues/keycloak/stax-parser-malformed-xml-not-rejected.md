# `StaxParserUtilTest` — CratonVM doesn't reject malformed/mismatched XML element nesting that real StAX rejects

Status: open — genuine CratonVM-specific bug candidate; possibly related to `sisu-beanloading-staxxmlstreamexception-xml-processing-instruction.md` (both involve `javax.xml.stream` divergences)

Date observed: 2026-07-14 (4-shard `nonpassed-v3` rerun against `others.tsv`, branch fix/keycloak-nonpassed-rerun-v2-20260710, binary `cratonvm-nonpassed-v3-refresh-20260714.exe`)

## Summary

`saml-core :: org.keycloak.saml.common.util.StaxParserUtilTest` fails 2 methods that assert a *specific
exception is thrown* for malformed XML input, but under CratonVM no exception is thrown at all:

```
=> java.lang.AssertionError: Expected test to throw an instance of org.keycloak.saml.common.exceptions.ParsingException
   ...StaxParserUtilTest.testBypassElementBlockWrongPairing
```

```
=> java.lang.AssertionError: Expected test to throw an instance of javax.xml.stream.XMLStreamException
   ...StaxParserUtilTest.testBypassElementBlockNestedPrematureEnd
```

`testBypassElementBlockWrongPairing` and `testBypassElementBlockNestedPrematureEnd` — both names describe
malformed nested-element scenarios (mismatched open/close tags, premature end-of-element) that a spec-compliant
StAX reader should reject with `XMLStreamException` (or Keycloak's own `ParsingException` wrapping it). Under
CratonVM, `StaxParserUtil`'s element-bypass logic apparently accepts the malformed input silently instead of
raising an error.

## Possible relationship to the Sisu bean-loading StAX bug

`docs/known-issues/keycloak/sisu-beanloading-staxxmlstreamexception-xml-processing-instruction.md` documents a
different, deterministic `javax.xml.stream.XMLStreamException` divergence elsewhere in this same run (Sisu
bean-loading throwing an exception real HotSpot doesn't for a *well-formed* file). This doc's symptom is the
mirror image — CratonVM *not* throwing where it should for genuinely malformed input. Both point at the same
underlying suspect: CratonVM's `javax.xml.stream` (StAX) implementation has some correctness gap in how it
validates XML structure/well-formedness, in ways that push it in either direction (over-strict in one code path,
under-strict in another) relative to a real StAX implementation. Worth investigating together — the same StAX
class/parser is likely responsible for both symptoms.

## Next steps

1. Read `StaxParserUtilTest.testBypassElementBlockWrongPairing`/`testBypassElementBlockNestedPrematureEnd`
   (`saml-core` module) to see the exact malformed XML fixture each uses and which `StaxParserUtil` method
   (`bypassElementBlock` or similar) is under test.
2. Trace CratonVM's StAX `XMLStreamReader` implementation (native or bundled) for element-nesting/well-formedness
   validation — check whether it's a synthetic/simplified reader that doesn't fully validate structure, or a
   real-JDK reader whose validation is being bypassed/short-circuited by some CratonVM native call underneath it.
3. Cross-reference with the Sisu StAX bug above — if both are traceable to the same StAX reader implementation,
   fixing one may inform or fix the other.

## Repro

```
cd C:\craton\CratonVM-keycloak-nonpassed-v2-20260710
$jdk = '"C:\Program Files\Java\jdk-25"'
powershell -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm craton -Jit on -TimeoutSec 60 -Parallel 1 -RunName repro-stax-malformed -ClassList <(printf 'module\tclass\nsaml-core\torg.keycloak.saml.common.util.StaxParserUtilTest\n') -KeycloakRoot apps\keycloak -Exe target\release\cratonvm-nonpassed-v3-refresh-20260714.exe -JdkHome $jdk
```

## Evidence

`apps/keycloak-suite-runner/.suite/results/nonpassed-v3-shard1/all-jit/logs/saml-core.org.keycloak.saml.common.util.StaxParserUtilTest.out.log`,
2026-07-14 rerun with binary `cratonvm-nonpassed-v3-refresh-20260714.exe` built from `dev` at commit `e85f76d00`.

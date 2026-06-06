# keycloak-server-spi OtpPolicyTest — URI builds `%20`/`%2F` instead of decoded space/slash

## Symptom
Module `apps/keycloak/server-spi` (CratonVM 8 failures vs HotSpot 0). Two of them:
```
FAIL keyUriShouldBeValidForRealmDisplayNameWithColon(org.keycloak.models.OtpPolicyTest)
     :: expected:<Test[ ]Realm> but was:<Test[%20]Realm>
FAIL keyUriShouldBeValidForRealmDisplayNameWithSlash(org.keycloak.models.OtpPolicyTest)
     :: expected:<Test[/]Realm> but was:<Test[%2F]Realm>
```

CratonVM produces the **percent-encoded** form (`%20` for space, `%2F` for `/`)
where HotSpot produces the **decoded** character (` `, `/`). The test builds an
`otpauth://` key URI from a realm display name and asserts the human-readable
label.

## Why it's a CratonVM bug
Identical bytecode + classpath; HotSpot passes. So CratonVM's URI/URL handling
decodes (or fails to decode) percent-escapes differently. The most likely
culprit is `java.net.URI`/`URLDecoder`/`URLEncoder` or `URI.getPath()` /
`getQuery()` returning the raw-encoded form where the real JDK returns the
decoded form. This is in the same family as the **URI getter slot bug** already
fixed once (`reference_uri_getter_slot_bug` — native `URI.getFragment/getQuery`
read raw slots and surfaced wrong values for real-bytecode URIs). This looks
like the decode side of the same area.

## Reproduce
```
CP="apps/_test-harness;apps/keycloak/server-spi/target/classes;apps/keycloak/server-spi/target/test-classes;$(cat apps/keycloak/server-spi/cp.txt)"
target/release/java.exe --java-home "C:/Program Files/Java/jdk-25" -Xmx2g -cp "$CP" \
    org.junit.runner.JUnitCore org.keycloak.models.OtpPolicyTest
```

## What an agent should try next
1. Minimal probe under CratonVM:
   `URLDecoder.decode("Test%20Realm", "UTF-8")` → expect `Test Realm`;
   `new URI("otpauth://totp/Test%20Realm").getSchemeSpecificPart()` / `getPath()`.
   Find where `%20`/`%2F` survives instead of decoding.
2. Inspect `OtpPolicy.getKeyURI`/`TimeBasedOTP` to see which JDK call it relies
   on, then check that call's CratonVM implementation (native vs real bytecode).
3. Reuse the by-name slot fix pattern from `reference_uri_getter_slot_bug`.

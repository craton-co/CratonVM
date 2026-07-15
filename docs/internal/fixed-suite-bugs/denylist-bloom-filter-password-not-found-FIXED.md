# Keycloak denylist Bloom-filter membership report — CLOSED

Status: CLOSED on current `dev` (`4d9db567`, 2026-07-15). The reported false-negative path cannot be reproduced;
no CratonVM source change was required.

## Why the original report is obsolete

The report named `DenylistPasswordPolicyProviderTest` and a pre-built `.bloom` denylist file. The retained
current Keycloak 26.6.1 sources no longer contain that test or file format. The corresponding current component is
`BlacklistPasswordPolicyProviderFactory.FileBasedPasswordBlacklist`: it reads the plaintext blacklist, builds a
Guava `BloomFilter<String>` with UTF-8 strings, and the current test class is
`org.keycloak.policy.BlacklistPasswordPolicyProviderTest`.

## Verification

An isolated Azure release build of CratonVM from `4d9db567` completed successfully in 8m27s, using dedicated
target and temporary directories under `/data/data/cratonvm-denylist-bloom-20260715`.

Two focused checks were run against that dedicated binary and HotSpot JDK 25:

- A standalone Guava binary round-trip created a UTF-8 Bloom filter with four known passwords, serialized it with
  `BloomFilter.writeTo`, restored it with `BloomFilter.readFrom`, and checked every inserted password. Both VMs
  reported `DENYLIST_BLOOM_OK bytes=22 entries=4`; no false negative occurred.
- The current Keycloak `BlacklistPasswordPolicyProviderTest` was compiled into the isolated temporary output and
  run with the current installed `keycloak-server-spi-private` artifact. HotSpot: `OK (6 tests)`. CratonVM:
  `OK (6 tests)`. This covers the real file-backed blacklist’s known-password membership and its reload cases.

The earlier direct directory-classpath attempt was superseded by compiling the current test into the isolated
output and using the installed module artifact; it is not a denylist or Bloom-filter failure.

## Result

There is no current CratonVM divergence in Guava binary Bloom serialization, UTF-8 password hashing, or Keycloak's
current file-backed denylist membership implementation. The historical report is archived; re-open only with a
reproducible current Keycloak test and fixture.

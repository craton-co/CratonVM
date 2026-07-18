# spring-boot-security-saml2: OpenSAML Package version and unknown X.509 key algorithms

**Status: FIXED 2026-07-18**

## Root causes

`org.opensaml.core.Version.getVersion()` reads `Version.class.getPackage()
.getImplementationVersion()`. CratonVM's package native read only manifest main
attributes, even though OpenSAML publishes its `Implementation-Version` in the
named `Name: org/opensaml/core/` section. It also left the JDK 9+
`Package$VersionInfo` record empty, so the real accessor returned `null`.

The package native now resolves a missing/pseudo CodeSource through the class
path index, parses continuation lines plus named manifest sections, applies the
package section before main attributes, and materializes a GC-rooted
`Package$VersionInfo` in the real `Package.versionInfo` slot. It preserves the
real `NULL_VERSION_INFO` sentinel when no values exist.

The SAML metadata certificate intentionally uses an unknown SubjectPublicKeyInfo
OID. HotSpot's `X509Key.buildX509Key` relies on `KeyFactory.getInstance` to
throw `NoSuchAlgorithmException`, then constructs a generic `X509Key` fallback.
CratonVM instead created a synthetic factory with an `Unknown` algorithm and
later threw `InvalidKeySpecException`. `KeyFactory.getInstance` now rejects
unknown algorithms at the correct boundary; factory algorithm state is also
kept in a GC-stable side table.

## Regression coverage

- `lang_class::tests::t19_h10_manifest_uses_named_package_section_before_main_section`
- `lang_class::tests::t19_h10_get_package_manifest_writes_do_not_corrupt_module_or_package_info`
- `jca::key_factory::tests::unknown_keyfactory_algorithm_throws_from_get_instance`

## Spring Boot verification

Using the compiled `spring-boot-security-saml2` fixture with the uniquely named
release binary `cratonvm-saml2-package-x509-20260718-019f7606-r8.exe`:

| Mode | Class | Result |
|---|---|---|
| JIT | `Saml2RelyingPartyWebMvcTestIntegrationTests` | PASS, 1 test, 80.177s |
| JIT | `Saml2RelyingPartyAutoConfigurationTests` | PASS, 21 tests, 281.307s |
| --nojit | `Saml2RelyingPartyWebMvcTestIntegrationTests` | PASS, 1 test, 75.318s |
| --nojit | `Saml2RelyingPartyAutoConfigurationTests` | PASS, 21 tests, 286.864s |

The final combined no-JIT run encountered a timing outlier under parallel load;
the standalone class runs above are the authoritative no-JIT evidence. No
remaining failure was attributable to either fixed issue.

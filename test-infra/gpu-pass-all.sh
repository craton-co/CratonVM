#!/usr/bin/env bash
# One GPU-mode pass over every runnable suite (cratonvm-gpu variant only):
# commons-math, bouncycastle, keycloak, wildfly. Serial, no CPU contention.
set +e
ROOT="C:/craton/CratonVM"
cd "$ROOT" || exit 2

echo "######################## GPU PASS: commons-math ########################"
VARIANTS="cratonvm-gpu" TIMEOUT=1200 bash test-infra/regression-commons-math-4way.sh \
  "$ROOT/test-infra/suite-results/commons-math-gpu.tsv"

echo "######################## GPU PASS: bouncycastle ########################"
bash test-infra/regression-3suite-4way.sh --bc --variants "cratonvm-gpu" --timeout 400

echo "######################## GPU PASS: keycloak ########################"
KC="$ROOT/apps/keycloak"
KCCP="C:/craton/CratonVM/apps/keycloak/crypto/default/target/classes;C:/craton/CratonVM/apps/keycloak/crypto/default/target/test-classes;$(cat "$KC/crypto/default/cratonvm-cryptodef-cp.txt")"
OUT="$ROOT/test-infra/suite-results/keycloak-gpu.tsv" TIMEOUT=150 bash test-infra/regression-apps-4way.sh keycloak "$KCCP" "cratonvm-gpu" \
  org.keycloak.crypto.def.test.BCEcdhEsAlgorithmProviderTest \
  org.keycloak.crypto.def.test.BCECDSACryptoProviderTest \
  org.keycloak.crypto.def.test.DefaultCertificateIdentityExtractorTest \
  org.keycloak.crypto.def.test.DefaultCryptoAKPJWKTest \
  org.keycloak.crypto.def.test.DefaultCryptoHmacTest \
  org.keycloak.crypto.def.test.DefaultCryptoJWETest \
  org.keycloak.crypto.def.test.DefaultCryptoJWKSUtilsTest \
  org.keycloak.crypto.def.test.DefaultCryptoJWKTest \
  org.keycloak.crypto.def.test.DefaultCryptoKeyPairVerifierTest \
  org.keycloak.crypto.def.test.DefaultCryptoRSAVerifierTest \
  org.keycloak.crypto.def.test.DefaultCryptoUnitTest \
  org.keycloak.crypto.def.test.DefaultKeyStoreTypesTest \
  org.keycloak.crypto.def.test.DefaultSecureRandomTest \
  org.keycloak.crypto.def.test.PemUtilsBCTest \
  org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwsTest \
  org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtCreationAndSigningTest \
  org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtKeyBindingTest \
  org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtVerificationTest

echo "######################## GPU PASS: wildfly ########################"
WFCP="C:/craton/CratonVM/apps/wildfly/health/target/classes;C:/craton/CratonVM/apps/wildfly/health/target/test-classes;$(cat "$ROOT/apps/wildfly/health/cratonvm-health-cp.txt")"
OUT="$ROOT/test-infra/suite-results/wildfly-gpu.tsv" TIMEOUT=150 bash test-infra/regression-apps-4way.sh wildfly "$WFCP" "cratonvm-gpu" \
  org.wildfly.extension.health.HealthSubsystemTestCase

echo "######################## GPU PASS COMPLETE ########################"

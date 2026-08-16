#!/bin/bash
# Build a witness fixture for "OpenSSL SECLEVEL is stricter than the JDK":
#   - a 1024-bit RSA CA, and a 1024-bit RSA leaf for localhost signed by it
#   - a JDK image whose cacerts TRUSTS that CA (hard-linked copy, ~no disk)
# The JDK accepts a 1024-bit RSA chain (its floor is 1024); OpenSSL at its
# default security level 2 requires 2048 and refuses.
source /data/toolchain/env.sh
set -e
D=/data/weakca
rm -rf $D; mkdir -p $D; cd $D

# 1024-bit RSA CA, SHA-256 self-signature.
openssl req -x509 -newkey rsa:1024 -keyout ca.key -out ca.pem -days 3650 -nodes \
  -subj "/CN=CratonVM Weak Witness CA" -sha256 >/dev/null 2>&1

# 1024-bit RSA leaf for localhost, signed by that CA.
openssl req -newkey rsa:1024 -keyout leaf.key -out leaf.csr -nodes \
  -subj "/CN=localhost" -sha256 >/dev/null 2>&1
cat > ext.cnf <<'EOF'
basicConstraints=CA:FALSE
keyUsage=digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=DNS:localhost,IP:127.0.0.1
EOF
openssl x509 -req -in leaf.csr -CA ca.pem -CAkey ca.key -CAcreateserial \
  -out leaf.pem -days 3650 -sha256 -extfile ext.cnf >/dev/null 2>&1

echo "CA   : $(openssl x509 -in ca.pem -noout -subject -text | grep -E 'Subject:|Public-Key' | tr -s ' ')"
echo "LEAF : $(openssl x509 -in leaf.pem -noout -subject -text | grep -E 'Subject:|Public-Key|Signature Algorithm' | head -3 | tr -s ' ')"

# A JDK image that trusts the CA: hard-link the real one (cheap), then
# REPLACE lib/security/cacerts so the link is broken for that file only.
JDKSRC=/data/toolchain/jdk-25
JDKDST=$D/jdk25-weakca
cp -al $JDKSRC $JDKDST
rm -f $JDKDST/lib/security/cacerts
cp $JDKSRC/lib/security/cacerts $JDKDST/lib/security/cacerts
chmod u+w $JDKDST/lib/security/cacerts
keytool -importcert -noprompt -trustcacerts -alias weakwitness -file ca.pem \
  -keystore $JDKDST/lib/security/cacerts -storepass changeit >/dev/null 2>&1
echo "cacerts entries: real=$(keytool -list -keystore $JDKSRC/lib/security/cacerts -storepass changeit 2>/dev/null | grep -c 'trustedCertEntry') weakca=$(keytool -list -keystore $JDKDST/lib/security/cacerts -storepass changeit 2>/dev/null | grep -c 'trustedCertEntry')"
echo "JDKDST=$JDKDST"
du -sh $D
echo FIXTURE-READY

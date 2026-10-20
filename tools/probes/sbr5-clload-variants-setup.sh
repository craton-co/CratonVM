#!/bin/bash
# usage: sbr5-clload-variants-setup.sh <workdir> ; JAVA_HOME = a JDK 25. Builds mr.jar signed.jar sealsec.jar and dir/ under <workdir>.
set -e
W=$1; mkdir -p $W; cd $W; J=$JAVA_HOME/bin; rm -rf s o *.jar dir
mkdir -p s/mr s/mr25 s/sg s/sp s/dir o/mr o/mr25 o/sg o/sp dir
cat > s/mr/M.java <<X
package com.mr; public class M { public static String id() { return "base"; } }
X
cat > s/mr25/M.java <<X
package com.mr; public class M { public static String id() { return "v25"; } }
X
cat > s/sg/S.java <<X
package com.sg; public class S { public static String id() { return "S"; } }
X
cat > s/sp/P.java <<X
package com.sp; public class P { public static String id() { return "P"; } }
X
cat > s/sp/Q.java <<X
package com.sp; public class Q { public static String id() { return "Q"; } }
X
cat > s/dir/D.java <<X
package com.dir; public class D { public static String id() { return "D"; } }
X
$J/javac -d o/mr s/mr/M.java; $J/javac -d o/mr25 s/mr25/M.java; $J/javac -d o/sg s/sg/S.java; $J/javac -d o/sp s/sp/P.java s/sp/Q.java; $J/javac -d dir s/dir/D.java
mkdir -p o/mr/META-INF/versions/25/com/mr; cp o/mr25/com/mr/M.class o/mr/META-INF/versions/25/com/mr/M.class
printf "Manifest-Version: 1.0\nMulti-Release: true\n" > mfmr.txt
$J/jar --create --file mr.jar --manifest mfmr.txt -C o/mr .
$J/jar --create --file signed.jar -C o/sg .
$J/keytool -genkeypair -alias k -keyalg RSA -keysize 2048 -validity 30 -dname "CN=t" -keystore ks.p12 -storepass changeit -keypass changeit -storetype PKCS12 >/dev/null 2>&1
$J/jarsigner -keystore ks.p12 -storepass changeit signed.jar k >/dev/null 2>&1
printf "Manifest-Version: 1.0\nImplementation-Version: 3.1\n\nName: com/sp/\nSealed: true\nImplementation-Version: sec\n" > mfsec.txt
$J/jar --create --file sealsec.jar --manifest mfsec.txt -C o/sp .
echo "CP=mr.jar:signed.jar:sealsec.jar:dir"

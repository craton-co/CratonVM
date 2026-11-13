#!/bin/bash
# run-tomcat-class.sh <vm> <label> <class> [vm flags...]: one Tomcat 12 test class under JUnitCore, wall seconds + summary line.
# Host layout (64.177.43.150): apps/tomcat built in /root/CVM, classpath in /root/tc-cp.txt. HotSpot control: vm=java.
vm=$1; label=$2; cls=$3; shift 3
cd /root/CVM/apps/tomcat || exit 1
tmp=output/test-tmp-$label; mkdir -p "$tmp" output/build/logs-$label
start=$(date +%s.%N)
timeout 1500 "$vm" "$@" -Xmx2g -Djava.security.egd=file:/dev/./urandom -Dfile.encoding=UTF-8 \
  --add-opens=java.base/java.lang=ALL-UNNAMED --add-opens=java.base/java.io=ALL-UNNAMED \
  --add-opens=java.rmi/sun.rmi.transport=ALL-UNNAMED --add-opens=java.base/java.util=ALL-UNNAMED \
  --add-opens=java.base/java.util.concurrent=ALL-UNNAMED --enable-native-access=ALL-UNNAMED \
  -Dtomcat.test.basedir=output/build -Dtomcat.test.tomcatbuild=output/build \
  -Dtomcat.test.protocol=org.apache.coyote.http11.Http11NioProtocol -Dtomcat.test.accesslog=false \
  -Dtomcat.test.relaxTiming=false -Dtomcat.test.temp="$tmp" -Dtomcat.test.reports=output/build/logs-$label \
  -cp "$(cat /root/tc-cp.txt)" org.junit.runner.JUnitCore "$cls" > "/root/callov/tc-$label.log" 2>&1
rc=$?
end=$(date +%s.%N)
echo "$label $cls rc=$rc wall=$(echo "$end - $start" | bc) $(grep -E '^(OK|Tests run)' "/root/callov/tc-$label.log" | tail -1)"

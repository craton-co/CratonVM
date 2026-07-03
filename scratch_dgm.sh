#!/bin/bash
set -x
export PATH=~/jdk25/bin:$PATH
JAR=/home/victor/.gradle/caches/modules-2/files-2.1/org.apache.groovy/groovy/5.0.6/61f54ac4cac5d7099798578a32344f78111b932c/groovy-5.0.6.jar
mkdir -p /tmp/groovysrc
cd /tmp/groovysrc
jar xf "$JAR" 'org/codehaus/groovy/reflection/GeneratedMetaMethod$DgmMethodRecord.class'
find . -iname "*DgmMethodRecord*"
javap -p -c 'org.codehaus.groovy.reflection.GeneratedMetaMethod$DgmMethodRecord' > /tmp/dgm_javap.txt 2>&1
wc -l /tmp/dgm_javap.txt

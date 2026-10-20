#!/usr/bin/env bash
# RI.10 — Hibernate 6 + H2: insert then select one row.

source "$(dirname "$0")/common.sh"

MVN_CENTRAL="https://repo1.maven.org/maven2"
HIB_V=6.5.2.Final
H2_V=2.2.224
JPA_V=3.1.0
JANDEX_V=3.1.8
BYTEBUDDY_V=1.14.11
CLASSMATE_V=1.7.0
ANTLR_V=4.13.1
JAXB_IMPL_V=4.0.5
JAKARTA_INJECT_V=2.0.1
JAKARTA_XML_V=4.0.2
SLF4J_V="${SLF4J_VERSION:-2.0.13}"

dl() {
    local path="$1"
    smoke_download "$MVN_CENTRAL/$path" "$FIXTURE_CACHE/$(basename "$path")"
}

dl "org/hibernate/orm/hibernate-core/$HIB_V/hibernate-core-$HIB_V.jar"
dl "jakarta/persistence/jakarta.persistence-api/$JPA_V/jakarta.persistence-api-$JPA_V.jar"
dl "jakarta/transaction/jakarta.transaction-api/2.0.1/jakarta.transaction-api-2.0.1.jar"
dl "jakarta/xml/bind/jakarta.xml.bind-api/$JAKARTA_XML_V/jakarta.xml.bind-api-$JAKARTA_XML_V.jar"
dl "jakarta/inject/jakarta.inject-api/$JAKARTA_INJECT_V/jakarta.inject-api-$JAKARTA_INJECT_V.jar"
dl "io/smallrye/jandex/$JANDEX_V/jandex-$JANDEX_V.jar"
dl "net/bytebuddy/byte-buddy/$BYTEBUDDY_V/byte-buddy-$BYTEBUDDY_V.jar"
dl "com/fasterxml/classmate/$CLASSMATE_V/classmate-$CLASSMATE_V.jar"
dl "org/antlr/antlr4-runtime/$ANTLR_V/antlr4-runtime-$ANTLR_V.jar"
dl "com/h2database/h2/$H2_V/h2-$H2_V.jar"
dl "org/slf4j/slf4j-api/$SLF4J_V/slf4j-api-$SLF4J_V.jar"
dl "org/slf4j/slf4j-simple/$SLF4J_V/slf4j-simple-$SLF4J_V.jar"
dl "org/glassfish/jaxb/jaxb-runtime/$JAXB_IMPL_V/jaxb-runtime-$JAXB_IMPL_V.jar"

CP=$(ls "$FIXTURE_CACHE"/*.jar | tr '\n' ':')
FIX_DIR="$FIXTURE_CACHE/fixture"
mkdir -p "$FIX_DIR/META-INF"

cat > "$FIX_DIR/META-INF/persistence.xml" <<XML
<persistence xmlns="https://jakarta.ee/xml/ns/persistence" version="3.0">
  <persistence-unit name="smoke">
    <provider>org.hibernate.jpa.HibernatePersistenceProvider</provider>
    <class>Note</class>
    <properties>
      <property name="jakarta.persistence.jdbc.url" value="jdbc:h2:mem:smoke"/>
      <property name="jakarta.persistence.jdbc.user" value="sa"/>
      <property name="jakarta.persistence.jdbc.driver" value="org.h2.Driver"/>
      <property name="hibernate.hbm2ddl.auto" value="create"/>
    </properties>
  </persistence-unit>
</persistence>
XML

cat > "$FIX_DIR/Note.java" <<'JAVA'
import jakarta.persistence.*;
@Entity
public class Note {
    @Id @GeneratedValue public Long id;
    @Column public String text;
}
JAVA

cat > "$FIX_DIR/HibernateSmoke.java" <<'JAVA'
import jakarta.persistence.*;
public class HibernateSmoke {
    public static void main(String[] args) {
        EntityManagerFactory f = Persistence.createEntityManagerFactory("smoke");
        EntityManager em = f.createEntityManager();
        em.getTransaction().begin();
        Note n = new Note();
        n.text = "hello";
        em.persist(n);
        em.getTransaction().commit();
        Note found = em.find(Note.class, n.id);
        System.out.println("HIB_SMOKE_OK text=" + found.text);
        em.close();
        f.close();
    }
}
JAVA

"$JAVA_HOME_FOR_SMOKE/bin/javac" -cp "$CP" -d "$FIX_DIR" "$FIX_DIR/Note.java" "$FIX_DIR/HibernateSmoke.java"

SMOKE_TIMEOUT=600 smoke_run_cratonvm --Xmx 1g --classpath "$FIX_DIR:$CP" -- HibernateSmoke

smoke_require_signal "HIB_SMOKE_OK text=hello"

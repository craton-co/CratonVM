package org.jboss.acpprobe;

import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;

// A compile-only annotation in an ENTERPRISE-PREFIXED package.
//
// The prefix is the whole point. `is_enterprise_stub_prefix` (`org/jboss/`,
// `org/infinispan/`, `io/quarkus/`, ...) is what makes this VM FABRICATE a
// synthetic stand-in for a class that is on no classpath, and the fabricated
// stand-in is not an interface and has no superinterfaces. A compile-only
// annotation in the default package (`AcpGone`) is simply not found, which is
// why a fixture written there passes on a broken binary and proves nothing.
//
// This is the `org.jboss.marshalling.Externalize` shape that made
// `org.infinispan.query.remote.client.impl.QueryRequest` unmockable.
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.TYPE)
public @interface AcpGoneEnterprise {
    String value();
}

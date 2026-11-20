    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    const STANDALONE_MODULE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<module name="org.jboss.as.standalone" xmlns="urn:jboss:module:1.9">
    <properties>
        <property name="jboss.api" value="private"/>
        <property name="jboss.require-java-version" value="1.8"/>
    </properties>
    <main-class name="org.jboss.as.server.Main"/>
    <resources>
    </resources>
    <dependencies>
        <module name="jdk.security.auth"/>
        <module name="java.xml"/>
        <module name="org.jboss.logmanager" services="import"/>
        <module name="org.jboss.as.jmx" services="import"/>
        <module name="org.jboss.as.server" export="true"/>
        <module name="org.jboss.vfs" services="import"/>
        <module name="org.wildfly.security.elytron-private" services="import"/>
    </dependencies>
</module>
"#;

    #[test]
    fn parses_standalone_module_xml() {
        let mx = parse_module_xml_bytes(STANDALONE_MODULE_XML.as_bytes()).unwrap();
        assert_eq!(mx.name, "org.jboss.as.standalone");
        assert_eq!(mx.main_class.as_deref(), Some("org.jboss.as.server.Main"));
        assert_eq!(mx.properties.len(), 2);
        assert_eq!(
            mx.properties[0],
            ("jboss.api".to_string(), "private".to_string())
        );
        assert_eq!(mx.dependencies.len(), 7);
        assert_eq!(mx.dependencies[0].name, "jdk.security.auth");
        assert_eq!(mx.dependencies[2].services, ServicesDisposition::Import);
        assert!(mx.dependencies[4].export);
        assert!(mx.resource_roots.is_empty());
    }

    #[test]
    fn parses_module_alias_target_name() {
        let src = r#"<?xml version="1.0"?>
<module-alias name="org.jboss.as.modcluster" target-name="org.wildfly.extension.mod_cluster" xmlns="urn:jboss:module:1.9"/>"#;
        let mx = parse_module_xml_bytes(src.as_bytes()).unwrap();
        assert_eq!(mx.name, "org.jboss.as.modcluster");
        assert_eq!(
            mx.alias_target.as_deref(),
            Some("org.wildfly.extension.mod_cluster")
        );
    }

    #[test]
    fn parses_resource_roots() {
        let src = r#"<?xml version="1.0"?>
<module name="x" xmlns="urn:jboss:module:1.9">
  <resources>
    <resource-root path="a.jar"/>
    <resource-root path="b.jar" name="bee"/>
  </resources>
</module>"#;
        let mx = parse_module_xml_bytes(src.as_bytes()).unwrap();
        assert_eq!(mx.resource_roots.len(), 2);
        assert_eq!(mx.resource_roots[0].path, "a.jar");
        assert!(mx.resource_roots[0].name.is_none());
        assert_eq!(mx.resource_roots[1].name.as_deref(), Some("bee"));
    }

    #[test]
    fn parses_artifacts() {
        let src = r#"<?xml version="1.0"?>
<module name="x" xmlns="urn:jboss:module:1.9">
  <resources>
    <artifact name="org.wildfly.core:wildfly-process-controller:33.0.0.Beta1"/>
    <artifact name="io.netty:netty-transport-native-unix-common:4.1.133.Final:linux-x86_64"/>
  </resources>
</module>"#;
        let mx = parse_module_xml_bytes(src.as_bytes()).unwrap();
        assert_eq!(mx.artifacts.len(), 2);
        assert_eq!(
            mx.artifacts[0],
            "org.wildfly.core:wildfly-process-controller:33.0.0.Beta1"
        );
        assert_eq!(
            mx.artifacts[1],
            "io.netty:netty-transport-native-unix-common:4.1.133.Final:linux-x86_64"
        );
    }

    #[test]
    fn rejects_missing_name_attribute() {
        let src = r#"<?xml version="1.0"?><module xmlns="urn:jboss:module:1.9"/>"#;
        let err = parse_module_xml_bytes(src.as_bytes()).unwrap_err();
        assert!(matches!(err, ParseError::Malformed(_)));
    }

    #[test]
    fn rejects_invalid_utf8() {
        let bytes = [0xFF, 0xFE, 0xFD];
        let err = parse_module_xml_bytes(&bytes).unwrap_err();
        assert!(matches!(err, ParseError::Malformed(_)));
    }

    #[test]
    fn system_dependency_recognized() {
        let src = r#"<?xml version="1.0"?>
<module name="x" xmlns="urn:jboss:module:1.9">
  <dependencies>
    <system export="true"/>
  </dependencies>
</module>"#;
        let mx = parse_module_xml_bytes(src.as_bytes()).unwrap();
        assert_eq!(mx.dependencies.len(), 1);
        assert_eq!(mx.dependencies[0].kind, DependencyKind::System);
        assert!(mx.dependencies[0].export);
    }

    #[test]
    fn services_disposition_round_trips() {
        assert_eq!(
            ServicesDisposition::parse("import"),
            ServicesDisposition::Import
        );
        assert_eq!(
            ServicesDisposition::parse("export"),
            ServicesDisposition::Export
        );
        assert_eq!(
            ServicesDisposition::parse("none"),
            ServicesDisposition::None
        );
        assert_eq!(
            ServicesDisposition::parse("nonsense"),
            ServicesDisposition::None
        );
        assert_eq!(ServicesDisposition::Import.as_str(), "import");
    }

// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! RA.4 — JBoss Modules `module.xml` parser.
//!
//! `org.jboss.modules.xml.ModuleXmlParser.parseModuleXml(...)` is the
//! hot entry point that every JBoss Modules bootstrap hits. The Java
//! implementation uses a hand-rolled MXParser which depends on NIO
//! CharBuffer internals that our VM is still stabilizing. Routing the
//! call through a real Rust XML parser sidesteps the parser bug
//! entirely and produces a valid ModuleSpec via a small set of
//! `invoke` callbacks.

use std::fs;
use std::path::{Path, PathBuf};

use quick_xml::events::Event;
use quick_xml::reader::Reader;

use cratonvm_native_api::NativeContext;
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, VmError};
use cratonvm_types::Value;

#[derive(Debug, Default, Clone)]
pub struct ModuleXml {
    pub name: String,
    pub slot: Option<String>,
    pub alias_target: Option<String>,
    pub main_class: Option<String>,
    pub properties: Vec<(String, String)>,
    pub resource_roots: Vec<ResourceRoot>,
    pub artifacts: Vec<String>,
    pub dependencies: Vec<Dependency>,
}

#[derive(Debug, Clone)]
pub struct ResourceRoot {
    pub path: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Dependency {
    pub kind: DependencyKind,
    pub name: String,
    pub slot: Option<String>,
    pub export: bool,
    pub optional: bool,
    pub services: ServicesDisposition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyKind {
    Module,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServicesDisposition {
    None,
    Import,
    Export,
}

impl ServicesDisposition {
    fn parse(raw: &str) -> Self {
        match raw.trim() {
            "import" => ServicesDisposition::Import,
            "export" => ServicesDisposition::Export,
            _ => ServicesDisposition::None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            ServicesDisposition::None => "none",
            ServicesDisposition::Import => "import",
            ServicesDisposition::Export => "export",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("I/O error reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("XML parse error at offset {offset}: {message}")]
    Xml { offset: usize, message: String },
    #[error("malformed module.xml: {0}")]
    Malformed(String),
}

fn attr_str(attr: &quick_xml::events::attributes::Attribute) -> Result<String, ParseError> {
    let raw = attr.unescape_value().map_err(|e| ParseError::Xml {
        offset: 0,
        message: e.to_string(),
    })?;
    Ok(raw.to_string())
}

pub fn parse_module_xml(path: &Path) -> Result<ModuleXml, ParseError> {
    let bytes = fs::read(path).map_err(|e| ParseError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    parse_module_xml_bytes(&bytes)
}

pub fn parse_module_xml_bytes(bytes: &[u8]) -> Result<ModuleXml, ParseError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| ParseError::Malformed(format!("module.xml not valid UTF-8: {e}")))?;
    let mut reader = Reader::from_str(text);
    reader.trim_text(true);
    reader.expand_empty_elements(false);
    let mut mx = ModuleXml::default();
    let mut buf: Vec<u8> = Vec::new();
    let mut path_stack: Vec<String> = Vec::with_capacity(6);
    loop {
        let ev = reader
            .read_event_into(&mut buf)
            .map_err(|e| ParseError::Xml {
                offset: reader.buffer_position() as usize,
                message: e.to_string(),
            })?;
        let is_empty = matches!(&ev, Event::Empty(_));
        let is_end = matches!(&ev, Event::End(_));
        let is_eof = matches!(&ev, Event::Eof);
        match ev {
            Event::Start(e) | Event::Empty(e) => {
                let local_name_owned: String = {
                    let name = e.name();
                    let bytes = name.as_ref();
                    let local_start = bytes
                        .iter()
                        .position(|b| *b == b':')
                        .map(|i| i + 1)
                        .unwrap_or(0);
                    String::from_utf8_lossy(&bytes[local_start..]).into_owned()
                };
                match local_name_owned.as_str() {
                    "module" | "module-alias" => {
                        if path_stack.is_empty() {
                            for a in e.attributes().flatten() {
                                match a.key.as_ref() {
                                    b"name" => mx.name = attr_str(&a)?,
                                    b"slot" => mx.slot = Some(attr_str(&a)?),
                                    b"target-name" => mx.alias_target = Some(attr_str(&a)?),
                                    _ => {}
                                }
                            }
                        } else if path_stack.last().map(String::as_str) == Some("dependencies") {
                            let mut dep = Dependency {
                                kind: DependencyKind::Module,
                                name: String::new(),
                                slot: None,
                                export: false,
                                optional: false,
                                services: ServicesDisposition::None,
                            };
                            for a in e.attributes().flatten() {
                                match a.key.as_ref() {
                                    b"name" => dep.name = attr_str(&a)?,
                                    b"slot" => dep.slot = Some(attr_str(&a)?),
                                    b"export" => {
                                        dep.export = attr_str(&a)?.eq_ignore_ascii_case("true");
                                    }
                                    b"optional" => {
                                        dep.optional = attr_str(&a)?.eq_ignore_ascii_case("true");
                                    }
                                    b"services" => {
                                        dep.services = ServicesDisposition::parse(&attr_str(&a)?);
                                    }
                                    _ => {}
                                }
                            }
                            if !dep.name.is_empty() {
                                mx.dependencies.push(dep);
                            }
                        }
                    }
                    "main-class" => {
                        for a in e.attributes().flatten() {
                            if a.key.as_ref() == b"name" {
                                mx.main_class = Some(attr_str(&a)?);
                            }
                        }
                    }
                    "property" => {
                        let mut key = String::new();
                        let mut val = String::new();
                        for a in e.attributes().flatten() {
                            match a.key.as_ref() {
                                b"name" => key = attr_str(&a)?,
                                b"value" => val = attr_str(&a)?,
                                _ => {}
                            }
                        }
                        if !key.is_empty() {
                            mx.properties.push((key, val));
                        }
                    }
                    "resource-root" => {
                        let mut path = String::new();
                        let mut name: Option<String> = None;
                        for a in e.attributes().flatten() {
                            match a.key.as_ref() {
                                b"path" => path = attr_str(&a)?,
                                b"name" => name = Some(attr_str(&a)?),
                                _ => {}
                            }
                        }
                        if !path.is_empty() {
                            mx.resource_roots.push(ResourceRoot { path, name });
                        }
                    }
                    "artifact" => {
                        for a in e.attributes().flatten() {
                            if a.key.as_ref() == b"name" {
                                let name = attr_str(&a)?;
                                if !name.is_empty() {
                                    mx.artifacts.push(name);
                                }
                            }
                        }
                    }
                    "system" if path_stack.last().map(String::as_str) == Some("dependencies") => {
                        let mut dep = Dependency {
                            kind: DependencyKind::System,
                            name: String::new(),
                            slot: None,
                            export: false,
                            optional: false,
                            services: ServicesDisposition::None,
                        };
                        for a in e.attributes().flatten() {
                            if a.key.as_ref() == b"export" {
                                dep.export = attr_str(&a)?.eq_ignore_ascii_case("true");
                            }
                        }
                        mx.dependencies.push(dep);
                    }
                    _ => {}
                }
                if !is_empty {
                    path_stack.push(local_name_owned);
                }
            }
            _ => {}
        }
        if is_end {
            path_stack.pop();
        }
        if is_eof {
            break;
        }
        buf.clear();
    }
    if mx.name.is_empty() {
        return Err(ParseError::Malformed(
            "<module> element missing `name` attribute".to_string(),
        ));
    }
    Ok(mx)
}

pub fn build_module_spec_via_invoke(
    ctx: &mut dyn NativeContext,
    module_loader: Value,
    root_file: Value,
    mx: &ModuleXml,
) -> MethodCallResult {
    // Use the String overload of ModuleSpec.build — simpler than building
    // a ModuleIdentifier, and the resulting Builder behaves identically.
    let name_str = ctx.create_string(&mx.name);
    let builder_val = ctx.invoke(
        "org/jboss/modules/ModuleSpec",
        "build",
        "(Ljava/lang/String;)Lorg/jboss/modules/ModuleSpec$Builder;",
        &[Value::Object(Some(name_str))],
    )?;
    let builder = match builder_val {
        Some(Value::Object(Some(_))) => builder_val.unwrap(),
        _ => {
            return Err(MethodCallFailed::InternalError(VmError::Internal {
                message: "ModuleSpec.build returned null builder".to_string(),
            }));
        }
    };
    if let Some(main) = &mx.main_class {
        let main_str = ctx.create_string(main);
        if let Value::Object(Some(b)) = builder {
            ctx.invoke_virtual(
                b,
                "setMainClass",
                "(Ljava/lang/String;)Lorg/jboss/modules/ModuleSpec$Builder;",
                &[Value::Object(Some(main_str))],
            )?;
        }
    }
    // Skip properties — they're metadata only, not needed for resolution.
    for rr in &mx.resource_roots {
        let name_str = ctx.create_string(rr.name.as_deref().unwrap_or(&rr.path));
        let resolved_path = resolve_path_value(ctx, &root_file, &rr.path)?;
        let loader = ctx
            .invoke(
                "org/jboss/modules/ResourceLoaders",
                "createPathResourceLoader",
                "(Ljava/lang/String;Ljava/nio/file/Path;)Lorg/jboss/modules/ResourceLoader;",
                &[Value::Object(Some(name_str)), resolved_path],
            )
            .ok()
            .and_then(|o| o)
            .unwrap_or(Value::Object(None));
        let spec = ctx.invoke(
            "org/jboss/modules/ResourceLoaderSpec",
            "createResourceLoaderSpec",
            "(Lorg/jboss/modules/ResourceLoader;)Lorg/jboss/modules/ResourceLoaderSpec;",
            &[loader],
        )?;
        if let Some(s) = spec {
            if let Value::Object(Some(b)) = builder {
                ctx.invoke_virtual(
                    b,
                    "addResourceRoot",
                    "(Lorg/jboss/modules/ResourceLoaderSpec;)Lorg/jboss/modules/ModuleSpec$Builder;",
                    &[s],
                )?;
            }
        }
    }
    for dep in &mx.dependencies {
        match dep.kind {
            DependencyKind::Module => {
                let dep_id = module_identifier(ctx, &dep.name, dep.slot.as_deref())?;
                let spec = ctx.invoke(
                    "org/jboss/modules/DependencySpec",
                    "createModuleDependencySpec",
                    "(Lorg/jboss/modules/ModuleLoader;Lorg/jboss/modules/ModuleIdentifier;Z)Lorg/jboss/modules/DependencySpec;",
                    &[
                        module_loader,
                        dep_id,
                        Value::Int(if dep.optional { 1 } else { 0 }),
                    ],
                )?;
                if let Some(s) = spec {
                    if let Value::Object(Some(b)) = builder {
                        ctx.invoke_virtual(
                            b,
                            "addDependency",
                            "(Lorg/jboss/modules/DependencySpec;)Lorg/jboss/modules/ModuleSpec$Builder;",
                            &[s],
                        )?;
                    }
                }
            }
            DependencyKind::System => {
                let spec = ctx.invoke(
                    "org/jboss/modules/DependencySpec",
                    "createSystemDependencySpec",
                    "(Ljava/util/Set;)Lorg/jboss/modules/DependencySpec;",
                    &[Value::Object(None)],
                )?;
                if let Some(s) = spec {
                    if let Value::Object(Some(b)) = builder {
                        ctx.invoke_virtual(
                            b,
                            "addDependency",
                            "(Lorg/jboss/modules/DependencySpec;)Lorg/jboss/modules/ModuleSpec$Builder;",
                            &[s],
                        )?;
                    }
                }
            }
        }
    }
    let spec = if let Value::Object(Some(b)) = builder {
        ctx.invoke_virtual(b, "create", "()Lorg/jboss/modules/ModuleSpec;", &[])?
    } else {
        None
    };
    Ok(spec)
}

fn module_identifier(
    ctx: &mut dyn NativeContext,
    name: &str,
    slot: Option<&str>,
) -> Result<Value, MethodCallFailed> {
    let name_s = ctx.create_string(name);
    let slot_s = match slot {
        Some(s) => Value::Object(Some(ctx.create_string(s))),
        None => Value::Object(None),
    };
    let id = ctx.invoke(
        "org/jboss/modules/ModuleIdentifier",
        "create",
        "(Ljava/lang/String;Ljava/lang/String;)Lorg/jboss/modules/ModuleIdentifier;",
        &[Value::Object(Some(name_s)), slot_s],
    )?;
    id.ok_or_else(|| {
        MethodCallFailed::InternalError(VmError::Internal {
            message: "ModuleIdentifier.create returned null".to_string(),
        })
    })
}

fn resolve_path_value(
    ctx: &mut dyn NativeContext,
    root_file: &Value,
    rel: &str,
) -> Result<Value, MethodCallFailed> {
    let root_path = ctx.invoke(
        "java/io/File",
        "toPath",
        "()Ljava/nio/file/Path;",
        &[*root_file],
    )?;
    let root_path = root_path.unwrap_or(Value::Object(None));
    let rel_str = ctx.create_string(rel);
    let resolved = ctx.invoke(
        "java/nio/file/Path",
        "resolve",
        "(Ljava/lang/String;)Ljava/nio/file/Path;",
        &[root_path, Value::Object(Some(rel_str))],
    )?;
    Ok(resolved.unwrap_or(Value::Object(None)))
}

pub fn native_parse_module_xml(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if args.len() < 5 {
        return Err(MethodCallFailed::InternalError(VmError::Internal {
            message: format!(
                "ModuleXmlParser.parseModuleXml: expected 5 args, got {}",
                args.len()
            ),
        }));
    }
    // Signature: parseModuleXml(RRF, ModuleLoader, String name, File rootDir, File xmlFile)
    //            args[0]        args[1]           args[2]        args[3]       args[4]
    let module_loader = args[1];
    let root_file = args[3];
    let xml_file = args[4];
    let path_str_val = ctx.invoke(
        "java/io/File",
        "getAbsolutePath",
        "()Ljava/lang/String;",
        &[xml_file],
    )?;
    let path_str = match path_str_val {
        Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
        _ => {
            return Err(MethodCallFailed::InternalError(VmError::Internal {
                message: "moduleInfoFile.getAbsolutePath() returned null".to_string(),
            }));
        }
    };
    // Normalize Windows path: strip the `\\?\` extended-length prefix and
    // convert `/` separators to `\` so std::fs can open the file.
    let normalized: String = {
        #[cfg(windows)]
        {
            let stripped = path_str.strip_prefix(r"\\?\").unwrap_or(&path_str);
            stripped.replace('/', "\\")
        }
        #[cfg(not(windows))]
        {
            path_str.clone()
        }
    };
    let mx = parse_module_xml(Path::new(&normalized)).map_err(|e| {
        MethodCallFailed::InternalError(VmError::Internal {
            message: format!("parse_module_xml({normalized}): {e}"),
        })
    })?;
    build_module_spec_via_invoke(ctx, module_loader, root_file, &mx)
}

#[cfg(test)]
mod tests {
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
}

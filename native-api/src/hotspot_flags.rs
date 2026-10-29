// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A VM's resolved, modelled HotSpot options, shared by its diagnostic readers.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HotspotVmFlag {
    pub ty: &'static str,
    pub name: &'static str,
    pub value: String,
    pub kind: &'static str,
    pub origin: &'static str,
}

impl HotspotVmFlag {
    pub fn print_final(&self) -> String {
        print_final_line(self.ty, self.name, &self.value, self.kind, self.origin)
    }

    pub fn command_line(&self) -> String {
        if self.ty == "bool" {
            format!("-XX:{}{}", if self.value == "true" { "+" } else { "-" }, self.name)
        } else {
            format!("-XX:{}={}", self.name, self.value)
        }
    }

    /// JFR chooses the event type from the resolved HotSpot flag type.
    pub fn jfr_event_name(&self) -> Option<&'static str> {
        match self.ty {
            "bool" => Some("jdk.BooleanFlag"),
            "int" => Some("jdk.IntFlag"),
            "uint" => Some("jdk.UnsignedIntFlag"),
            "intx" => Some("jdk.LongFlag"),
            "uintx" | "size_t" | "uint64_t" => Some("jdk.UnsignedLongFlag"),
            "double" => Some("jdk.DoubleFlag"),
            "ccstr" | "ccstrlist" => Some("jdk.StringFlag"),
            _ => None,
        }
    }

    pub fn jfr_origin(&self) -> &'static str {
        match self.origin {
            "{command line}" => "Command line",
            "{management}" => "Management",
            "{ergonomic}" | "{command line, ergonomic}" => "Ergonomic",
            _ => "Default",
        }
    }

    pub fn jmm_origin(&self) -> i32 {
        match self.origin {
            "{command line}" => 2,
            "{management}" => 3,
            "{ergonomic}" | "{command line, ergonomic}" => 6,
            _ => 1,
        }
    }

    /// `com.sun.management.VMOption.Origin` for the public diagnostic view.
    pub fn jmx_origin(&self) -> &'static str {
        match self.origin {
            "{command line}" => "VM_CREATION",
            "{ergonomic}" | "{command line, ergonomic}" => "ERGONOMIC",
            "{default}" => "DEFAULT",
            // zt t24/x20: a value a `setVMOption` wrote (HotSpot's
            // `JVMFlagOrigin::MANAGEMENT`, `VMOption.Origin.MANAGEMENT`); it
            // fell to `ENVIRON_VAR`.
            "{management}" => "MANAGEMENT",
            _ => "ENVIRON_VAR",
        }
    }

    /// HotSpot's `JVMFlag::is_manageable()` for the modelled row (its
    /// PrintFlagsFinal kind column reads `{manageable}`). jmm's
    /// `GetVMGlobals` reports such a flag `writeable` and `external`
    /// (`JVMFlag::is_writeable()` / `is_external()` are both
    /// `is_manageable()` on a product build).
    pub fn is_manageable(&self) -> bool {
        self.kind == "{manageable}"
    }
}

pub fn print_final_line(ty: &str, name: &str, value: &str, kind: &str, origin: &str) -> String {
    fn fill_to(line: &mut String, pos: usize) {
        if line.len() < pos {
            let pad = pos - line.len();
            line.push_str(&" ".repeat(pad));
        } else {
            line.push(' ');
        }
    }
    let mut line = format!("{ty:>9}");
    fill_to(&mut line, 10);
    line.push_str(name);
    fill_to(&mut line, 50);
    line.push_str(" =");
    fill_to(&mut line, 53);
    line.push_str(value);
    fill_to(&mut line, 84);
    line.push_str(&format!("{kind:>20}"));
    fill_to(&mut line, 105);
    line.push_str(origin);
    line
}

#[cfg(test)]
mod gcj_resolved_flag_tests {
    use super::HotspotVmFlag;

    #[test]
    fn each_resolved_type_and_origin_selects_its_jfr_representation() {
        let mut flag = HotspotVmFlag { ty: "int", name: "MaxJavaStackTraceDepth",
            value: "23".into(), kind: "{product}", origin: "{command line}" };
        for (ty, event) in [("bool", "jdk.BooleanFlag"), ("int", "jdk.IntFlag"),
            ("uint", "jdk.UnsignedIntFlag"), ("intx", "jdk.LongFlag"),
            ("uintx", "jdk.UnsignedLongFlag"), ("size_t", "jdk.UnsignedLongFlag"),
            ("uint64_t", "jdk.UnsignedLongFlag"), ("double", "jdk.DoubleFlag"),
            ("ccstr", "jdk.StringFlag"), ("ccstrlist", "jdk.StringFlag")] {
            flag.ty = ty;
            assert_eq!(flag.jfr_event_name(), Some(event));
        }
        flag.ty = "unsupported";
        assert_eq!(flag.jfr_event_name(), None);
        for (origin, jfr) in [("{default}", "Default"), ("{command line}", "Command line"),
            ("{ergonomic}", "Ergonomic"), ("{management}", "Management")] {
            flag.origin = origin;
            assert_eq!(flag.jfr_origin(), jfr);
        }
        flag.origin = "{command line, ergonomic}";
        assert_eq!(flag.jfr_origin(), "Ergonomic");
        assert_eq!(flag.jmm_origin(), 6);
        assert_eq!(flag.jmx_origin(), "ERGONOMIC");
    }
}

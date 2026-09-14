// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use crate::class_reader_error::ClassReaderError;
use crate::field_type::FieldType;
use std::fmt;

/// Represents a parsed method descriptor (JVM spec 4.3.3).
///
/// A method descriptor encodes the parameter types and return type of a method.
/// For example: `(ILjava/lang/String;)V` means a method taking an int and a String,
/// returning void.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodDescriptor {
    pub parameters: Vec<FieldType>,
    pub return_type: Option<FieldType>,
}

impl MethodDescriptor {
    /// Parse a method descriptor string.
    ///
    /// Format: `(<ParameterDescriptor>*)<ReturnDescriptor>`
    /// where ReturnDescriptor is a FieldDescriptor or `V` (void).
    pub fn parse(descriptor: &str) -> Result<Self, ClassReaderError> {
        let bytes = descriptor.as_bytes();
        if bytes.is_empty() || bytes[0] != b'(' {
            return Err(ClassReaderError::InvalidMethodDescriptor {
                descriptor: descriptor.to_string(),
            });
        }

        let mut remaining = &descriptor[1..];
        let mut parameters = Vec::new();

        while !remaining.starts_with(')') {
            if remaining.is_empty() {
                return Err(ClassReaderError::InvalidMethodDescriptor {
                    descriptor: descriptor.to_string(),
                });
            }
            let (param_type, rest) = FieldType::parse_partial(remaining)?;
            parameters.push(param_type);
            remaining = rest;
        }

        // Skip the ')'
        remaining = &remaining[1..];

        let return_type = if remaining == "V" {
            None
        } else {
            Some(FieldType::parse(remaining)?)
        };

        Ok(Self {
            parameters,
            return_type,
        })
    }

    /// Returns the total number of stack slots required for the parameters.
    pub fn parameters_stack_slots(&self) -> usize {
        self.parameters.iter().map(|p| p.stack_slots()).sum()
    }
}

impl fmt::Display for MethodDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "(")?;
        for param in &self.parameters {
            write!(f, "{param}")?;
        }
        write!(f, ")")?;
        match &self.return_type {
            Some(rt) => write!(f, "{rt}"),
            None => write!(f, "V"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_void_no_args() {
        let desc = MethodDescriptor::parse("()V").unwrap();
        assert!(desc.parameters.is_empty());
        assert_eq!(desc.return_type, None);
    }

    #[test]
    fn parse_single_int_param() {
        let desc = MethodDescriptor::parse("(I)V").unwrap();
        assert_eq!(desc.parameters, vec![FieldType::Int]);
        assert_eq!(desc.return_type, None);
    }

    #[test]
    fn parse_multiple_params_with_return() {
        let desc = MethodDescriptor::parse("(ILjava/lang/String;)Ljava/lang/Object;").unwrap();
        assert_eq!(
            desc.parameters,
            vec![
                FieldType::Int,
                FieldType::Object("java/lang/String".to_string()),
            ]
        );
        assert_eq!(
            desc.return_type,
            Some(FieldType::Object("java/lang/Object".to_string()))
        );
    }

    #[test]
    fn parse_main_descriptor() {
        let desc = MethodDescriptor::parse("([Ljava/lang/String;)V").unwrap();
        assert_eq!(
            desc.parameters,
            vec![FieldType::Array(Box::new(FieldType::Object(
                "java/lang/String".to_string()
            )))]
        );
        assert_eq!(desc.return_type, None);
    }

    #[test]
    fn parameter_stack_slots() {
        let desc = MethodDescriptor::parse("(IJD)V").unwrap();
        assert_eq!(desc.parameters_stack_slots(), 5); // 1 + 2 + 2
    }

    #[test]
    fn display_roundtrip() {
        let descriptors = [
            "()V",
            "(I)I",
            "(ILjava/lang/String;)Ljava/lang/Object;",
            "([Ljava/lang/String;)V",
        ];
        for d in descriptors {
            let parsed = MethodDescriptor::parse(d).unwrap();
            assert_eq!(parsed.to_string(), d);
        }
    }
}

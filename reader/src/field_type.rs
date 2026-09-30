// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use crate::class_reader_error::ClassReaderError;
use std::fmt;

/// Represents a JVM field type descriptor (JVM spec 4.3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldType {
    /// `B` - signed byte
    Byte,
    /// `C` - Unicode character code point (BMP)
    Char,
    /// `D` - double-precision floating-point
    Double,
    /// `F` - single-precision floating-point
    Float,
    /// `I` - integer
    Int,
    /// `J` - long integer
    Long,
    /// `S` - signed short
    Short,
    /// `Z` - boolean (true or false)
    Boolean,
    /// `L<ClassName>;` - an instance of the named class
    Object(String),
    /// `[<ComponentType>` - one dimension of an array
    Array(Box<FieldType>),
}

impl FieldType {
    /// Parse a field type descriptor from a string.
    pub fn parse(descriptor: &str) -> Result<Self, ClassReaderError> {
        let (field_type, remaining) = Self::parse_partial(descriptor)?;
        if !remaining.is_empty() {
            return Err(ClassReaderError::InvalidTypeDescriptor {
                descriptor: descriptor.to_string(),
            });
        }
        Ok(field_type)
    }

    /// JVMS §4.3.2: an array type may have at most 255 dimensions. The cap
    /// also bounds recursion in `parse_partial`, preventing a stack-overflow
    /// DoS from an untrusted descriptor of nothing but `[` bytes.
    ///
    /// Canonical value lives in [`crate::limits::MAX_ARRAY_DIMENSIONS`].
    const MAX_ARRAY_DIMENSIONS: usize = crate::limits::MAX_ARRAY_DIMENSIONS;

    /// Parse a field type descriptor, returning the parsed type and the remaining unparsed string.
    pub fn parse_partial(descriptor: &str) -> Result<(Self, &str), ClassReaderError> {
        Self::parse_partial_depth(descriptor, 0)
    }

    fn parse_partial_depth(
        descriptor: &str,
        dimensions: usize,
    ) -> Result<(Self, &str), ClassReaderError> {
        let bytes = descriptor.as_bytes();
        if bytes.is_empty() {
            return Err(ClassReaderError::InvalidTypeDescriptor {
                descriptor: descriptor.to_string(),
            });
        }

        match bytes[0] {
            b'B' => Ok((FieldType::Byte, &descriptor[1..])),
            b'C' => Ok((FieldType::Char, &descriptor[1..])),
            b'D' => Ok((FieldType::Double, &descriptor[1..])),
            b'F' => Ok((FieldType::Float, &descriptor[1..])),
            b'I' => Ok((FieldType::Int, &descriptor[1..])),
            b'J' => Ok((FieldType::Long, &descriptor[1..])),
            b'S' => Ok((FieldType::Short, &descriptor[1..])),
            b'Z' => Ok((FieldType::Boolean, &descriptor[1..])),
            b'L' => {
                let end = descriptor.find(';').ok_or_else(|| {
                    ClassReaderError::InvalidTypeDescriptor {
                        descriptor: descriptor.to_string(),
                    }
                })?;
                let class_name = &descriptor[1..end];
                Ok((
                    FieldType::Object(class_name.to_string()),
                    &descriptor[end + 1..],
                ))
            }
            b'[' => {
                if dimensions >= Self::MAX_ARRAY_DIMENSIONS {
                    return Err(ClassReaderError::InvalidTypeDescriptor {
                        descriptor: descriptor.to_string(),
                    });
                }
                let (component_type, remaining) =
                    Self::parse_partial_depth(&descriptor[1..], dimensions + 1)?;
                Ok((FieldType::Array(Box::new(component_type)), remaining))
            }
            _ => Err(ClassReaderError::InvalidTypeDescriptor {
                descriptor: descriptor.to_string(),
            }),
        }
    }

    /// Returns the number of stack slots this type occupies (1 for most types, 2 for long/double).
    pub fn stack_slots(&self) -> usize {
        match self {
            FieldType::Long | FieldType::Double => 2,
            _ => 1,
        }
    }

    /// Returns true if this is a primitive type.
    pub fn is_primitive(&self) -> bool {
        matches!(
            self,
            FieldType::Byte
                | FieldType::Char
                | FieldType::Double
                | FieldType::Float
                | FieldType::Int
                | FieldType::Long
                | FieldType::Short
                | FieldType::Boolean
        )
    }

    /// Returns true if this is a reference type (object or array).
    pub fn is_reference(&self) -> bool {
        matches!(self, FieldType::Object(_) | FieldType::Array(_))
    }
}

impl fmt::Display for FieldType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FieldType::Byte => write!(f, "B"),
            FieldType::Char => write!(f, "C"),
            FieldType::Double => write!(f, "D"),
            FieldType::Float => write!(f, "F"),
            FieldType::Int => write!(f, "I"),
            FieldType::Long => write!(f, "J"),
            FieldType::Short => write!(f, "S"),
            FieldType::Boolean => write!(f, "Z"),
            FieldType::Object(name) => write!(f, "L{name};"),
            FieldType::Array(component) => write!(f, "[{component}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_primitive_types() {
        assert_eq!(FieldType::parse("I").unwrap(), FieldType::Int);
        assert_eq!(FieldType::parse("J").unwrap(), FieldType::Long);
        assert_eq!(FieldType::parse("Z").unwrap(), FieldType::Boolean);
    }

    #[test]
    fn parse_object_type() {
        assert_eq!(
            FieldType::parse("Ljava/lang/String;").unwrap(),
            FieldType::Object("java/lang/String".to_string())
        );
    }

    #[test]
    fn parse_array_type() {
        assert_eq!(
            FieldType::parse("[I").unwrap(),
            FieldType::Array(Box::new(FieldType::Int))
        );
    }

    #[test]
    fn parse_nested_array() {
        assert_eq!(
            FieldType::parse("[[Ljava/lang/Object;").unwrap(),
            FieldType::Array(Box::new(FieldType::Array(Box::new(FieldType::Object(
                "java/lang/Object".to_string()
            )))))
        );
    }

    #[test]
    fn stack_slots() {
        assert_eq!(FieldType::Int.stack_slots(), 1);
        assert_eq!(FieldType::Long.stack_slots(), 2);
        assert_eq!(FieldType::Double.stack_slots(), 2);
        assert_eq!(FieldType::Object("Foo".to_string()).stack_slots(), 1);
    }

    #[test]
    fn deeply_nested_array_is_rejected_not_overflow() {
        // 256 leading '[' exceeds the 255-dimension cap: must return an
        // error rather than recursing into a stack overflow.
        let descriptor = format!("{}I", "[".repeat(256));
        assert!(FieldType::parse(&descriptor).is_err());

        // Far past the cap stays an error (and does not panic/overflow).
        let huge = format!("{}I", "[".repeat(100_000));
        assert!(FieldType::parse(&huge).is_err());

        // Exactly 255 dimensions remains valid.
        let at_cap = format!("{}I", "[".repeat(255));
        assert!(FieldType::parse(&at_cap).is_ok());
    }

    #[test]
    fn display_roundtrip() {
        let types = ["I", "J", "Ljava/lang/String;", "[I", "[[D"];
        for desc in types {
            let parsed = FieldType::parse(desc).unwrap();
            assert_eq!(parsed.to_string(), desc);
        }
    }
}

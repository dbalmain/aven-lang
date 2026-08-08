use std::fmt;

macro_rules! define_builtin_types {
    (
        $(
            $variant:ident => {
                name: $name:literal,
                runtime_value: $runtime_value:literal,
                application_arity: $application_arity:expr,
                scalar: $scalar:literal $(,)?
            }
        ),+ $(,)?
    ) => {
        /// Stable identities for the builtin type names understood across the
        /// parser/checker/evaluator boundary.
        ///
        /// Source syntax and host registries still use strings at their
        /// boundaries; internal dispatch parses them once and matches this enum.
        /// The declaration below is the single inventory for names and builtin
        /// metadata, so adding a variant cannot leave a parallel list stale.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum BuiltinType {
            $($variant),+
        }

        impl BuiltinType {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub const fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $name),+
                }
            }

            pub fn from_name(name: &str) -> Option<Self> {
                match name {
                    $($name => Some(Self::$variant),)+
                    _ => None,
                }
            }

            /// Whether the evaluator materializes this builtin as a `Type`
            /// value in its intrinsic environment.
            pub const fn has_runtime_value(self) -> bool {
                match self {
                    $(Self::$variant => $runtime_value),+
                }
            }

            pub const fn application_arity(self) -> Option<usize> {
                match self {
                    $(Self::$variant => $application_arity),+
                }
            }

            pub const fn is_scalar(self) -> bool {
                match self {
                    $(Self::$variant => $scalar),+
                }
            }
        }
    };
}

define_builtin_types! {
    Array => { name: "Array", runtime_value: true, application_arity: Some(1), scalar: false },
    Bool => { name: "Bool", runtime_value: true, application_arity: None, scalar: true },
    Data => { name: "Data", runtime_value: true, application_arity: None, scalar: false },
    Float => { name: "Float", runtime_value: true, application_arity: None, scalar: true },
    Int => { name: "Int", runtime_value: true, application_arity: None, scalar: true },
    Json => { name: "Json", runtime_value: true, application_arity: None, scalar: false },
    JsonError => { name: "JsonError", runtime_value: false, application_arity: None, scalar: false },
    JsonEncodeError => { name: "JsonEncodeError", runtime_value: false, application_arity: None, scalar: false },
    Map => { name: "Map", runtime_value: true, application_arity: Some(2), scalar: false },
    Null => { name: "Null", runtime_value: true, application_arity: None, scalar: false },
    Result => { name: "Result", runtime_value: true, application_arity: Some(2), scalar: false },
    Set => { name: "Set", runtime_value: true, application_arity: Some(1), scalar: false },
    Stream => { name: "Stream", runtime_value: true, application_arity: Some(1), scalar: false },
    Text => { name: "Text", runtime_value: true, application_arity: None, scalar: true },
    Toml => { name: "Toml", runtime_value: true, application_arity: None, scalar: false },
    TomlError => { name: "TomlError", runtime_value: false, application_arity: None, scalar: false },
    TomlEncodeError => { name: "TomlEncodeError", runtime_value: false, application_arity: None, scalar: false },
    Type => { name: "Type", runtime_value: false, application_arity: None, scalar: false },
    Undefined => { name: "Undefined", runtime_value: true, application_arity: None, scalar: false },
    Unit => { name: "Unit", runtime_value: true, application_arity: None, scalar: false },
    Yaml => { name: "Yaml", runtime_value: true, application_arity: None, scalar: false },
    YamlError => { name: "YamlError", runtime_value: false, application_arity: None, scalar: false },
    YamlEncodeError => { name: "YamlEncodeError", runtime_value: false, application_arity: None, scalar: false },
}

impl fmt::Display for BuiltinType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_names_are_unique() {
        let names = BuiltinType::ALL
            .iter()
            .map(|builtin| builtin.name())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(names.len(), BuiltinType::ALL.len());
    }

    #[test]
    fn only_type_constructors_publish_application_arities() {
        assert_eq!(BuiltinType::Array.application_arity(), Some(1));
        assert_eq!(BuiltinType::Set.application_arity(), Some(1));
        assert_eq!(BuiltinType::Map.application_arity(), Some(2));
        assert_eq!(BuiltinType::Result.application_arity(), Some(2));
        assert_eq!(BuiltinType::Stream.application_arity(), Some(1));
        assert_eq!(BuiltinType::Text.application_arity(), None);
    }
}

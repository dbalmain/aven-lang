use std::fmt;

/// Stable identities for the builtin type names understood across the
/// parser/checker/evaluator boundary.
///
/// Source syntax and host registries still use strings at their boundaries;
/// internal dispatch should parse them once and match this enum so inventories
/// and constructor arities cannot drift between crates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BuiltinType {
    Array,
    Bool,
    Data,
    Float,
    Int,
    Json,
    JsonError,
    Map,
    Null,
    Result,
    Set,
    Text,
    Toml,
    TomlError,
    Type,
    Undefined,
    Unit,
    Yaml,
    YamlError,
}

impl BuiltinType {
    pub const ALL: &'static [Self] = &[
        Self::Array,
        Self::Bool,
        Self::Data,
        Self::Float,
        Self::Int,
        Self::Json,
        Self::JsonError,
        Self::Map,
        Self::Null,
        Self::Result,
        Self::Set,
        Self::Text,
        Self::Toml,
        Self::TomlError,
        Self::Type,
        Self::Undefined,
        Self::Unit,
        Self::Yaml,
        Self::YamlError,
    ];

    /// Builtins which are materialized as evaluator `Type` values.
    pub const RUNTIME_VALUES: &'static [Self] = &[
        Self::Array,
        Self::Bool,
        Self::Data,
        Self::Float,
        Self::Int,
        Self::Json,
        Self::Map,
        Self::Null,
        Self::Result,
        Self::Set,
        Self::Text,
        Self::Toml,
        Self::Undefined,
        Self::Unit,
        Self::Yaml,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Array => "Array",
            Self::Bool => "Bool",
            Self::Data => "Data",
            Self::Float => "Float",
            Self::Int => "Int",
            Self::Json => "Json",
            Self::JsonError => "JsonError",
            Self::Map => "Map",
            Self::Null => "Null",
            Self::Result => "Result",
            Self::Set => "Set",
            Self::Text => "Text",
            Self::Toml => "Toml",
            Self::TomlError => "TomlError",
            Self::Type => "Type",
            Self::Undefined => "Undefined",
            Self::Unit => "Unit",
            Self::Yaml => "Yaml",
            Self::YamlError => "YamlError",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "Array" => Some(Self::Array),
            "Bool" => Some(Self::Bool),
            "Data" => Some(Self::Data),
            "Float" => Some(Self::Float),
            "Int" => Some(Self::Int),
            "Json" => Some(Self::Json),
            "JsonError" => Some(Self::JsonError),
            "Map" => Some(Self::Map),
            "Null" => Some(Self::Null),
            "Result" => Some(Self::Result),
            "Set" => Some(Self::Set),
            "Text" => Some(Self::Text),
            "Toml" => Some(Self::Toml),
            "TomlError" => Some(Self::TomlError),
            "Type" => Some(Self::Type),
            "Undefined" => Some(Self::Undefined),
            "Unit" => Some(Self::Unit),
            "Yaml" => Some(Self::Yaml),
            "YamlError" => Some(Self::YamlError),
            _ => None,
        }
    }

    pub const fn application_arity(self) -> Option<usize> {
        match self {
            Self::Array | Self::Set => Some(1),
            Self::Map | Self::Result => Some(2),
            _ => None,
        }
    }

    pub const fn is_scalar(self) -> bool {
        matches!(self, Self::Bool | Self::Float | Self::Int | Self::Text)
    }
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
    fn builtin_names_round_trip_and_stay_unique() {
        let names = BuiltinType::ALL
            .iter()
            .map(|builtin| builtin.name())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(names.len(), BuiltinType::ALL.len());
        for builtin in BuiltinType::ALL {
            assert_eq!(BuiltinType::from_name(builtin.name()), Some(*builtin));
        }
    }

    #[test]
    fn only_type_constructors_publish_application_arities() {
        assert_eq!(BuiltinType::Array.application_arity(), Some(1));
        assert_eq!(BuiltinType::Set.application_arity(), Some(1));
        assert_eq!(BuiltinType::Map.application_arity(), Some(2));
        assert_eq!(BuiltinType::Result.application_arity(), Some(2));
        assert_eq!(BuiltinType::Text.application_arity(), None);
    }
}

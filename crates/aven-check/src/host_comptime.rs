use std::rc::Rc;

use aven_parser::Literal;

use crate::comptime;
use crate::ty::Type;

/// Public facade for values the checker can prove at compile time before
/// dispatching to a host-provided resolver.
#[derive(Debug, Clone, PartialEq)]
pub enum ComptimeArg {
    Text(String),
    Number(String),
    Bool(bool),
    Type(Type),
    LabelSet(Vec<String>),
}

impl ComptimeArg {
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(value) => Some(value),
            Self::Number(_) | Self::Bool(_) | Self::Type(_) | Self::LabelSet(_) => None,
        }
    }

    pub fn as_number(&self) -> Option<&str> {
        match self {
            Self::Number(value) => Some(value),
            Self::Text(_) | Self::Bool(_) | Self::Type(_) | Self::LabelSet(_) => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        let value = self.as_number()?;
        if value.bytes().any(|byte| matches!(byte, b'.' | b'e' | b'E')) {
            return None;
        }

        value.replace('_', "").parse().ok()
    }

    pub fn as_float(&self) -> Option<f64> {
        self.as_number()?.replace('_', "").parse().ok()
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            Self::Text(_) | Self::Number(_) | Self::Type(_) | Self::LabelSet(_) => None,
        }
    }

    pub fn as_type(&self) -> Option<&Type> {
        match self {
            Self::Type(value) => Some(value),
            Self::Text(_) | Self::Number(_) | Self::Bool(_) | Self::LabelSet(_) => None,
        }
    }

    pub fn as_label_set(&self) -> Option<&[String]> {
        match self {
            Self::LabelSet(value) => Some(value),
            Self::Text(_) | Self::Number(_) | Self::Bool(_) | Self::Type(_) => None,
        }
    }

    pub(crate) fn from_comptime_value(value: comptime::ComptimeValue) -> Self {
        match value {
            comptime::ComptimeValue::ReifiedType(ty) => Self::Type(ty),
            comptime::ComptimeValue::LabelSet(labels) => Self::LabelSet(labels),
            comptime::ComptimeValue::Bool(value) => Self::Bool(value),
            comptime::ComptimeValue::Literal(Literal::String(text)) => {
                Self::Text(decode_string_literal(&text))
            }
            comptime::ComptimeValue::Literal(Literal::Number(value)) => Self::Number(value),
            comptime::ComptimeValue::Literal(Literal::Regex(_) | Literal::Path(_)) => {
                unreachable!("runtime comptime evaluation does not produce regex/path literals")
            }
            comptime::ComptimeValue::Literal(Literal::Bool(value)) => Self::Bool(value),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComptimeError {
    pub message: String,
    pub code: Option<String>,
}

impl ComptimeError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: None,
        }
    }

    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }
}

pub trait HostComptimeFn {
    fn resolve(&self, args: &[ComptimeArg]) -> Result<Type, ComptimeError>;
}

#[derive(Clone)]
pub struct HostComptimeFnSpec {
    pub resolver: Rc<dyn HostComptimeFn>,
    pub comptime_params: Vec<usize>,
}

impl HostComptimeFnSpec {
    pub fn new(resolver: Rc<dyn HostComptimeFn>, comptime_params: Vec<usize>) -> Self {
        Self {
            resolver,
            comptime_params,
        }
    }
}

#[derive(Clone, Default)]
pub struct HostGlobals {
    pub types: Vec<(String, Type)>,
    pub comptime_fns: Vec<(String, HostComptimeFnSpec)>,
}

impl HostGlobals {
    pub fn new(
        types: Vec<(String, Type)>,
        comptime_fns: Vec<(String, HostComptimeFnSpec)>,
    ) -> Self {
        Self {
            types,
            comptime_fns,
        }
    }

    pub fn types_only(types: &[(String, Type)]) -> Self {
        Self {
            types: types.to_vec(),
            comptime_fns: Vec::new(),
        }
    }
}

fn decode_string_literal(text: &str) -> String {
    let inner = text
        .strip_prefix('"')
        .and_then(|stripped| stripped.strip_suffix('"'))
        .unwrap_or(text);

    decode_string_body(inner)
}

fn decode_string_body(text: &str) -> String {
    let mut decoded = String::new();
    let mut escaped = false;

    for ch in text.chars() {
        if escaped {
            decoded.push(match ch {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                '"' => '"',
                '\\' => '\\',
                other => other,
            });
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else {
            decoded.push(ch);
        }
    }

    if escaped {
        decoded.push('\\');
    }

    decoded
}

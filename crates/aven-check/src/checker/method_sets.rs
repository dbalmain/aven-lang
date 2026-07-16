use crate::ty::{Type, named_builtin};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MethodSignature {
    pub(crate) params: Vec<Type>,
    pub(crate) result: Type,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuiltinType {
    Bool,
    Float,
    Int,
    Text,
}

impl BuiltinType {
    fn from_type(ty: &Type) -> Option<Self> {
        let Type::Named(name) = ty else {
            return None;
        };
        match name.as_str() {
            "Bool" => Some(Self::Bool),
            "Float" => Some(Self::Float),
            "Int" => Some(Self::Int),
            "Text" => Some(Self::Text),
            _ => None,
        }
    }

    fn to_type(self) -> Type {
        let name = match self {
            Self::Bool => "Bool",
            Self::Float => "Float",
            Self::Int => "Int",
            Self::Text => "Text",
        };
        named_builtin(name)
    }
}

struct BuiltinMethodEntry {
    owner: BuiltinType,
    member: &'static str,
    params: &'static [BuiltinType],
    result: BuiltinType,
}

const INT_PARAM: &[BuiltinType] = &[BuiltinType::Int];
const FLOAT_PARAM: &[BuiltinType] = &[BuiltinType::Float];
const TEXT_PARAM: &[BuiltinType] = &[BuiltinType::Text];

const BUILTIN_METHODS: &[BuiltinMethodEntry] = &[
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: "+",
        params: INT_PARAM,
        result: BuiltinType::Int,
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: "-",
        params: INT_PARAM,
        result: BuiltinType::Int,
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: "*",
        params: INT_PARAM,
        result: BuiltinType::Int,
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: "/",
        params: INT_PARAM,
        result: BuiltinType::Int,
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: "%",
        params: INT_PARAM,
        result: BuiltinType::Int,
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: "^",
        params: INT_PARAM,
        result: BuiltinType::Int,
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: "<",
        params: INT_PARAM,
        result: BuiltinType::Bool,
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: "<=",
        params: INT_PARAM,
        result: BuiltinType::Bool,
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: ">",
        params: INT_PARAM,
        result: BuiltinType::Bool,
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: ">=",
        params: INT_PARAM,
        result: BuiltinType::Bool,
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "+",
        params: FLOAT_PARAM,
        result: BuiltinType::Float,
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "-",
        params: FLOAT_PARAM,
        result: BuiltinType::Float,
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "*",
        params: FLOAT_PARAM,
        result: BuiltinType::Float,
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "/",
        params: FLOAT_PARAM,
        result: BuiltinType::Float,
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "%",
        params: FLOAT_PARAM,
        result: BuiltinType::Float,
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "^",
        params: FLOAT_PARAM,
        result: BuiltinType::Float,
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "<",
        params: FLOAT_PARAM,
        result: BuiltinType::Bool,
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "<=",
        params: FLOAT_PARAM,
        result: BuiltinType::Bool,
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: ">",
        params: FLOAT_PARAM,
        result: BuiltinType::Bool,
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: ">=",
        params: FLOAT_PARAM,
        result: BuiltinType::Bool,
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Text,
        member: "+",
        params: TEXT_PARAM,
        result: BuiltinType::Text,
    },
];

/// Look up a method declared directly by an exact concrete builtin owner.
///
/// Operator-boundary widening and promotion happen before this query. This
/// keeps the method set canonical: `Int.+` remains `Int -> Int`, while a mixed
/// `Int + Float` operation selects `Float.+` after promotion.
pub(crate) fn builtin_method_signature(owner: &Type, member: &str) -> Option<MethodSignature> {
    let owner = BuiltinType::from_type(owner)?;
    let entry = BUILTIN_METHODS
        .iter()
        .find(|entry| entry.owner == owner && entry.member == member)?;
    Some(MethodSignature {
        params: entry.params.iter().map(|param| param.to_type()).collect(),
        result: entry.result.to_type(),
    })
}

/// Resolve a concrete builtin binary operation, including the closed numeric
/// promotion already supported by the checker.
pub(crate) fn resolve_builtin_operator_signature(
    left: &Type,
    member: &str,
    right: &Type,
) -> Option<MethodSignature> {
    if let Some(signature) = builtin_method_signature(left, member)
        && signature.params.first() == Some(right)
        && signature.params.len() == 1
    {
        return Some(signature);
    }

    let float = named_builtin("Float");
    if operator_operand_fits(left, &float) && operator_operand_fits(right, &float) {
        let signature = builtin_method_signature(&float, member)?;
        if signature.params.as_slice() == [float] {
            return Some(signature);
        }
    }

    None
}

fn operator_operand_fits(source: &Type, target: &Type) -> bool {
    source == target
        || matches!(
            (
                BuiltinType::from_type(source),
                BuiltinType::from_type(target)
            ),
            (Some(BuiltinType::Int), Some(BuiltinType::Float))
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARITHMETIC: &[&str] = &["+", "-", "*", "/", "%", "^"];
    const COMPARISONS: &[&str] = &["<", "<=", ">", ">="];

    #[test]
    fn table_contains_exact_current_builtin_operator_methods() {
        for (owner, arithmetic_result) in [("Int", "Int"), ("Float", "Float")] {
            for member in ARITHMETIC {
                assert_signature(owner, member, owner, arithmetic_result);
            }
            for member in COMPARISONS {
                assert_signature(owner, member, owner, "Bool");
            }
        }

        assert_signature("Text", "+", "Text", "Text");
        assert_eq!(BUILTIN_METHODS.len(), 21);
    }

    #[test]
    fn table_does_not_claim_unsupported_builtin_or_temporal_methods() {
        for owner in [
            "Bool", "Text", "Instant", "Duration", "Date", "Time", "DateTime",
        ] {
            for member in ARITHMETIC.iter().chain(COMPARISONS) {
                if owner == "Text" && *member == "+" {
                    continue;
                }
                assert_eq!(
                    builtin_method_signature(&named_builtin(owner), member),
                    None,
                    "{owner}.{member} unexpectedly exists"
                );
            }
        }
        assert_eq!(builtin_method_signature(&named_builtin("Int"), "=="), None);
    }

    #[test]
    fn concrete_resolution_applies_only_int_to_float_promotion() {
        let int = named_builtin("Int");
        let float = named_builtin("Float");

        for member in ARITHMETIC.iter().chain(COMPARISONS) {
            let expected_result = if COMPARISONS.contains(member) {
                named_builtin("Bool")
            } else {
                float.clone()
            };
            for (left, right) in [(&int, &float), (&float, &int)] {
                assert_eq!(
                    resolve_builtin_operator_signature(left, member, right),
                    Some(MethodSignature {
                        params: vec![float.clone()],
                        result: expected_result.clone(),
                    })
                );
            }
        }

        assert_eq!(
            resolve_builtin_operator_signature(&named_builtin("Text"), "+", &named_builtin("Int")),
            None
        );
    }

    fn assert_signature(owner: &str, member: &str, param: &str, result: &str) {
        assert_eq!(
            builtin_method_signature(&named_builtin(owner), member),
            Some(MethodSignature {
                params: vec![named_builtin(param)],
                result: named_builtin(result),
            })
        );
    }
}

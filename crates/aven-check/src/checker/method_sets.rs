use super::Checker;
use std::collections::{HashMap, HashSet};

use aven_parser::Literal;

use crate::ty::{
    LiteralBase, MethodPredicate, RowEntry, Type, literal_variant_base, map_type, named_builtin,
    type_variable_names,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MethodSignature {
    pub(crate) params: Vec<Type>,
    pub(crate) result: Type,
    pub(crate) predicates: Vec<MethodPredicate>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuiltinResult {
    Plain(BuiltinType),
    Optional(BuiltinType),
}

impl BuiltinResult {
    fn to_type(self) -> Type {
        match self {
            Self::Plain(ty) => ty.to_type(),
            Self::Optional(ty) => Type::Optional(Box::new(ty.to_type())),
        }
    }
}

struct BuiltinMethodEntry {
    owner: BuiltinType,
    member: &'static str,
    params: &'static [BuiltinType],
    result: BuiltinResult,
}

const INT_PARAM: &[BuiltinType] = &[BuiltinType::Int];
const FLOAT_PARAM: &[BuiltinType] = &[BuiltinType::Float];
const TEXT_PARAM: &[BuiltinType] = &[BuiltinType::Text];

const BUILTIN_METHODS: &[BuiltinMethodEntry] = &[
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: "+",
        params: INT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Int),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: "-",
        params: INT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Int),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: "*",
        params: INT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Int),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: "/",
        params: INT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Int),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: "%",
        params: INT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Int),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: "div",
        params: INT_PARAM,
        result: BuiltinResult::Optional(BuiltinType::Int),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: "mod",
        params: INT_PARAM,
        result: BuiltinResult::Optional(BuiltinType::Int),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: "^",
        params: INT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Int),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: "<",
        params: INT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Bool),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: "<=",
        params: INT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Bool),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: ">",
        params: INT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Bool),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: ">=",
        params: INT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Bool),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "+",
        params: FLOAT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Float),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "-",
        params: FLOAT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Float),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "*",
        params: FLOAT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Float),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "/",
        params: FLOAT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Float),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "%",
        params: FLOAT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Float),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "^",
        params: FLOAT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Float),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "<",
        params: FLOAT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Bool),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "<=",
        params: FLOAT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Bool),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: ">",
        params: FLOAT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Bool),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: ">=",
        params: FLOAT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Bool),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Text,
        member: "+",
        params: TEXT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Text),
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
        predicates: Vec::new(),
    })
}

impl Checker<'_> {
    /// Query one exact owner category. User families never fall back to a
    /// builtin or another structurally equal declaration.
    pub(crate) fn exact_method_signature(
        &mut self,
        owner: &Type,
        member: &str,
    ) -> Option<MethodSignature> {
        if let Type::SlotRecord { slots, .. } = owner {
            let ty = slots.entries.iter().find_map(|entry| match entry {
                RowEntry::Field { name, ty } if name == member => Some(ty),
                RowEntry::Field { .. } | RowEntry::Tag { .. } | RowEntry::Literal { .. } => None,
            })?;
            let Type::Function { params, result, .. } = ty else {
                return None;
            };
            return Some(MethodSignature {
                params: params.clone(),
                result: result.as_ref().clone(),
                predicates: Vec::new(),
            });
        }
        // A literal-typed receiver (`y = 7` infers the singleton `7`) carries
        // no methods of its own; it dispatches through its base builtin owner,
        // mirroring the widening operator resolution already performs.
        if let Type::Variant(row) = owner
            && let Some(base) = literal_variant_base(row)
        {
            let widened = match base {
                LiteralBase::Text => named_builtin("Text"),
                LiteralBase::Bool => named_builtin("Bool"),
                LiteralBase::Number => {
                    let is_float = row.entries.iter().any(|entry| {
                        matches!(
                            entry,
                            RowEntry::Literal { value: Literal::Number(number) }
                                if super::inference::is_float_literal_text(number)
                        )
                    });
                    named_builtin(if is_float { "Float" } else { "Int" })
                }
            };
            return self.exact_method_signature(&widened, member);
        }
        if let Type::Named(name) = owner
            && let Some(canonical) = self.named_family_aliases.get(name)
        {
            let signature = self.named_families.get(canonical)?.methods.get(member)?;
            return Some(MethodSignature {
                params: signature.params.clone(),
                result: signature.result.clone(),
                predicates: Vec::new(),
            });
        }
        if let Some(signature) = self.attached_builtin_method_signature(owner, member) {
            return Some(signature);
        }
        builtin_method_signature(owner, member)
    }

    fn attached_builtin_method_signature(
        &mut self,
        owner: &Type,
        member: &str,
    ) -> Option<MethodSignature> {
        let owner = map_type(owner, &mut |node| {
            let widened = super::constraints::widen_literal_method_owner(node);
            (&widened != node).then_some(widened)
        });
        let entry = self
            .builtin_methods
            .methods()
            .iter()
            .find(|entry| {
                entry.member == member
                    && owner_pattern_bindings(
                        &entry.owner,
                        &owner,
                        &entry.owner_variables.iter().cloned().collect(),
                    )
                    .is_some()
            })?
            .clone();
        let owner_variables = entry
            .owner_variables
            .iter()
            .cloned()
            .collect::<HashSet<_>>();
        let mut substitutions = owner_pattern_bindings(&entry.owner, &owner, &owner_variables)?;
        let mut method_variables = HashSet::new();
        for ty in entry.params.iter().chain(std::iter::once(&entry.result)) {
            method_variables.extend(type_variable_names(ty));
        }
        for constraint in &entry.constraints {
            method_variables.extend(type_variable_names(&constraint.candidate));
            for ty in constraint
                .params
                .iter()
                .chain(std::iter::once(&constraint.result))
            {
                method_variables.extend(type_variable_names(ty));
            }
        }
        method_variables.retain(|name| !owner_variables.contains(name));
        for variable in method_variables {
            substitutions.insert(variable, self.unifier.fresh());
        }
        let instantiate = |ty: &Type| {
            map_type(ty, &mut |node| match node {
                Type::Variable(name) => substitutions.get(name).cloned(),
                _ => None,
            })
        };
        let params = entry.params.iter().map(instantiate).collect();
        let result = instantiate(&entry.result);
        let predicates = entry
            .constraints
            .iter()
            .map(|constraint| MethodPredicate {
                candidate: instantiate(&constraint.candidate),
                member: constraint.member.clone(),
                params: constraint.params.iter().map(instantiate).collect(),
                result: instantiate(&constraint.result),
                operator_span: entry.member_span,
                binding: Some(entry.member.clone()),
                call_span: None,
                obligation_id: None,
            })
            .collect();
        Some(MethodSignature {
            params,
            result,
            predicates,
        })
    }

    pub(crate) fn attached_builtin_method_required_owner(
        &self,
        receiver: &Type,
        member: &str,
    ) -> Option<Type> {
        let receiver_head = builtin_owner_head(receiver)?;
        self.builtin_methods
            .methods()
            .iter()
            .find(|entry| {
                entry.member == member && builtin_owner_head(&entry.owner) == Some(receiver_head)
            })
            .map(|entry| entry.owner.clone())
    }
}

fn builtin_owner_head(ty: &Type) -> Option<&str> {
    match ty {
        Type::Named(name) => Some(name),
        Type::Apply { callee, .. } => match callee.as_ref() {
            Type::Named(name) => Some(name),
            _ => None,
        },
        _ => None,
    }
}

fn owner_pattern_bindings(
    pattern: &Type,
    receiver: &Type,
    variables: &HashSet<String>,
) -> Option<HashMap<String, Type>> {
    let mut bindings = HashMap::new();
    owner_pattern_matches(pattern, receiver, variables, &mut bindings).then_some(bindings)
}

fn owner_pattern_matches(
    pattern: &Type,
    receiver: &Type,
    variables: &HashSet<String>,
    bindings: &mut HashMap<String, Type>,
) -> bool {
    match pattern {
        Type::Variable(name) if variables.contains(name) => match bindings.get(name) {
            Some(bound) => bound == receiver,
            None => {
                bindings.insert(name.clone(), receiver.clone());
                true
            }
        },
        Type::Apply { callee, args } => {
            let Type::Apply {
                callee: receiver_callee,
                args: receiver_args,
            } = receiver
            else {
                return false;
            };
            args.len() == receiver_args.len()
                && owner_pattern_matches(callee, receiver_callee, variables, bindings)
                && args.iter().zip(receiver_args).all(|(pattern, receiver)| {
                    owner_pattern_matches(pattern, receiver, variables, bindings)
                })
        }
        Type::Optional(inner) => {
            matches!(receiver, Type::Optional(receiver) if owner_pattern_matches(inner, receiver, variables, bindings))
        }
        Type::Nullable(inner) => {
            matches!(receiver, Type::Nullable(receiver) if owner_pattern_matches(inner, receiver, variables, bindings))
        }
        Type::Tuple(items) => {
            let Type::Tuple(receiver_items) = receiver else {
                return false;
            };
            items.len() == receiver_items.len()
                && items.iter().zip(receiver_items).all(|(pattern, receiver)| {
                    owner_pattern_matches(pattern, receiver, variables, bindings)
                })
        }
        _ => pattern == receiver,
    }
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
        assert_optional_signature("Int", "div", "Int", "Int");
        assert_optional_signature("Int", "mod", "Int", "Int");
        assert_eq!(BUILTIN_METHODS.len(), 23);
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
                        predicates: Vec::new(),
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
                predicates: Vec::new(),
            })
        );
    }

    fn assert_optional_signature(owner: &str, member: &str, param: &str, result: &str) {
        assert_eq!(
            builtin_method_signature(&named_builtin(owner), member),
            Some(MethodSignature {
                params: vec![named_builtin(param)],
                result: Type::Optional(Box::new(named_builtin(result))),
                predicates: Vec::new(),
            })
        );
    }
}

use super::Checker;
use std::collections::{HashMap, HashSet};

use aven_core::{Diagnostic, Label, Span, codes};
use aven_parser::Literal;

use crate::env::TypeEnv;
use crate::ty::{
    LiteralBase, MethodPredicate, RowEntry, Type, literal_variant_base, map_type, named_builtin,
    record_fields, type_variable_names,
};
use crate::{MethodConstraint, NamedMethodOrigin, NamedMethodType};

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
const INT_PARAMS_2: &[BuiltinType] = &[BuiltinType::Int, BuiltinType::Int];
const FLOAT_PARAM: &[BuiltinType] = &[BuiltinType::Float];
const FLOAT_PARAMS_2: &[BuiltinType] = &[BuiltinType::Float, BuiltinType::Float];
const TEXT_PARAM: &[BuiltinType] = &[BuiltinType::Text];
const NO_PARAMS: &[BuiltinType] = &[];

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
        owner: BuiltinType::Float,
        member: "isFinite",
        params: NO_PARAMS,
        result: BuiltinResult::Plain(BuiltinType::Bool),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "isNaN",
        params: NO_PARAMS,
        result: BuiltinResult::Plain(BuiltinType::Bool),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "isInfinite",
        params: NO_PARAMS,
        result: BuiltinResult::Plain(BuiltinType::Bool),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "ieeeEquals",
        params: FLOAT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Bool),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Text,
        member: "+",
        params: TEXT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Text),
    },
    // The ambient display protocol: every scalar builtin renders itself.
    // Non-scalar shapes (collections, records, variants) answer `toText`
    // through `builtin_collection_method_type` instead.
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: "toText",
        params: NO_PARAMS,
        result: BuiltinResult::Plain(BuiltinType::Text),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: "toGrouped",
        params: TEXT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Text),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: "abs",
        params: NO_PARAMS,
        result: BuiltinResult::Plain(BuiltinType::Int),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: "min",
        params: INT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Int),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: "max",
        params: INT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Int),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: "clamp",
        params: INT_PARAMS_2,
        result: BuiltinResult::Plain(BuiltinType::Int),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: "pow",
        params: INT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Int),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: "sign",
        params: NO_PARAMS,
        result: BuiltinResult::Plain(BuiltinType::Int),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Int,
        member: "toFloat",
        params: NO_PARAMS,
        result: BuiltinResult::Plain(BuiltinType::Float),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "toText",
        params: NO_PARAMS,
        result: BuiltinResult::Plain(BuiltinType::Text),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "toFixed",
        params: INT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Text),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "abs",
        params: NO_PARAMS,
        result: BuiltinResult::Plain(BuiltinType::Float),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "min",
        params: FLOAT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Float),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "max",
        params: FLOAT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Float),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "clamp",
        params: FLOAT_PARAMS_2,
        result: BuiltinResult::Plain(BuiltinType::Float),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "pow",
        params: FLOAT_PARAM,
        result: BuiltinResult::Plain(BuiltinType::Float),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "round",
        params: NO_PARAMS,
        result: BuiltinResult::Plain(BuiltinType::Float),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "floor",
        params: NO_PARAMS,
        result: BuiltinResult::Plain(BuiltinType::Float),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "ceil",
        params: NO_PARAMS,
        result: BuiltinResult::Plain(BuiltinType::Float),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "truncate",
        params: NO_PARAMS,
        result: BuiltinResult::Plain(BuiltinType::Float),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Float,
        member: "sqrt",
        params: NO_PARAMS,
        result: BuiltinResult::Plain(BuiltinType::Float),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Bool,
        member: "toText",
        params: NO_PARAMS,
        result: BuiltinResult::Plain(BuiltinType::Text),
    },
    BuiltinMethodEntry {
        owner: BuiltinType::Text,
        member: "toText",
        params: NO_PARAMS,
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

/// Enumerate the declaration-time effective method environment for one exact
/// builtin owner. The returned schemes retain method-local variables; family
/// lookup freshens them later from the materialized descriptor.
pub(crate) fn effective_base_methods(
    checker: &Checker<'_>,
    owner: &Type,
) -> Vec<(String, NamedMethodType)> {
    let mut methods = Vec::new();
    let mut seen = HashSet::new();

    if let Some(fields) = record_fields(owner) {
        for field in fields {
            let Type::Function { params, result, .. } = field.ty else {
                continue;
            };
            if !seen.insert(field.name.clone()) {
                continue;
            }
            let result = *result;
            let variables = method_scheme_variables(&params, &result, &[]);
            methods.push((
                field.name.clone(),
                NamedMethodType {
                    params,
                    result,
                    constraints: Vec::new(),
                    variables,
                    origin: inherited_origin(owner, &field.name, Vec::new(), false),
                },
            ));
        }
    }

    if let Some(owner_kind) = BuiltinType::from_type(owner) {
        for entry in BUILTIN_METHODS
            .iter()
            .filter(|entry| entry.owner == owner_kind)
        {
            if !seen.insert(entry.member.to_owned()) {
                continue;
            }
            let params = entry.params.iter().map(|param| param.to_type()).collect();
            let result = entry.result.to_type();
            methods.push((
                entry.member.to_owned(),
                NamedMethodType {
                    params,
                    result,
                    constraints: Vec::new(),
                    variables: Vec::new(),
                    origin: inherited_origin(owner, entry.member, Vec::new(), false),
                },
            ));
        }
    }

    for entry in checker.builtin_methods.methods() {
        if seen.contains(&entry.member) {
            continue;
        }
        let owner_variables = entry
            .owner_variables
            .iter()
            .cloned()
            .collect::<HashSet<_>>();
        let Some(substitutions) = owner_pattern_bindings(&entry.owner, owner, &owner_variables)
        else {
            continue;
        };
        let instantiate_owner = |ty: &Type| {
            map_type(ty, &mut |node| match node {
                Type::Variable(name) => substitutions.get(name).cloned(),
                _ => None,
            })
        };
        let params = entry
            .params
            .iter()
            .map(instantiate_owner)
            .collect::<Vec<_>>();
        let result = instantiate_owner(&entry.result);
        let constraints = entry
            .constraints
            .iter()
            .map(|constraint| MethodConstraint {
                candidate: instantiate_owner(&constraint.candidate),
                member: constraint.member.clone(),
                params: constraint.params.iter().map(instantiate_owner).collect(),
                result: instantiate_owner(&constraint.result),
            })
            .collect::<Vec<_>>();
        let variables = method_scheme_variables(&params, &result, &constraints);
        seen.insert(entry.member.clone());
        methods.push((
            entry.member.clone(),
            NamedMethodType {
                params,
                result,
                constraints,
                variables,
                origin: inherited_origin(owner, &entry.member, Vec::new(), false),
            },
        ));
    }

    methods
}

fn inherited_origin(
    owner: &Type,
    member: &str,
    lifted_params: Vec<bool>,
    lifted_result: bool,
) -> NamedMethodOrigin {
    NamedMethodOrigin::Inherited {
        base_owner: owner.clone(),
        base_member: member.to_owned(),
        lifted_params,
        lifted_result,
    }
}

fn method_scheme_variables(
    params: &[Type],
    result: &Type,
    constraints: &[MethodConstraint],
) -> Vec<String> {
    let mut variables = HashSet::new();
    for ty in params.iter().chain(std::iter::once(result)) {
        variables.extend(type_variable_names(ty));
    }
    for constraint in constraints {
        variables.extend(type_variable_names(&constraint.candidate));
        for ty in constraint
            .params
            .iter()
            .chain(std::iter::once(&constraint.result))
        {
            variables.extend(type_variable_names(ty));
        }
    }
    let mut variables = variables.into_iter().collect::<Vec<_>>();
    variables.sort();
    variables
}

impl Checker<'_> {
    /// Owner name for the unbound method form `Owner.member` when the receiver
    /// is a type-static name: a named family alias, or a concrete scalar
    /// builtin (`Int` / `Float` / `Text` / `Bool`). Parameterized type
    /// constructors (`Array`, `Map`, user uppercase type functions, …) have no
    /// single concrete method value; callers detect them separately via
    /// [`Self::is_parameterized_type_constructor_name`] and report
    /// `type.unbound-method-parameterized-owner`.
    pub(crate) fn unbound_method_owner_name(&self, name: &str) -> Option<String> {
        if let Some(owner) = self.named_family_aliases.get(name) {
            return Some(owner.clone());
        }
        matches!(name, "Int" | "Float" | "Text" | "Bool").then(|| name.to_owned())
    }

    /// True when `name` is a type-level function / type constructor that takes
    /// parameters — the bare form cannot yield an unbound method value.
    ///
    /// Reuses the same classifications the checker already uses for type
    /// application: [`super::core::builtin_owner_arity`] for builtins with a
    /// positive arity, and [`Self::lookup_comptime_function_export`] for
    /// user-defined uppercase comptime type functions with at least one
    /// parameter. A local or top-level value binding that is not such a type
    /// function shadows the constructor and returns false.
    pub(crate) fn is_parameterized_type_constructor_name(&self, env: &TypeEnv, name: &str) -> bool {
        if env.get(name).is_some() {
            return false;
        }

        if let Some(export) = self.lookup_comptime_function_export(name)
            && name.chars().next().is_some_and(char::is_uppercase)
            && !export.params.is_empty()
        {
            return true;
        }

        // A non-type-function binding (including named-family providers) is a
        // value namespace, not the builtin type constructor of the same name.
        if self.bindings.contains_key(name) {
            return false;
        }

        matches!(super::core::builtin_owner_arity(name), Some(arity) if arity > 0)
    }

    pub(crate) fn report_unbound_method_parameterized_owner(
        &mut self,
        owner: &str,
        member: &str,
        span: Span,
    ) {
        self.push_unique_diagnostic(
            Diagnostic::error(format!(
                "cannot form unbound method `{member}` from parameterized type `{owner}`"
            ))
            .with_code(codes::ty::UNBOUND_METHOD_PARAMETERIZED_OWNER)
            .with_label(Label::primary(
                span,
                format!("`{owner}` takes type parameters"),
            ))
            .with_note(format!(
                "an unbound method value needs a concrete instantiation to bind against; call `{member}` on a value of type `{owner}(...)` instead"
            )),
        );
    }

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
            let substitutions = signature
                .variables
                .iter()
                .map(|name| (name.clone(), self.unifier.fresh()))
                .collect::<HashMap<_, _>>();
            let instantiate = |ty: &Type| {
                map_type(ty, &mut |node| match node {
                    Type::Variable(name) => substitutions.get(name).cloned(),
                    _ => None,
                })
            };
            return Some(MethodSignature {
                params: signature.params.iter().map(instantiate).collect(),
                result: instantiate(&signature.result),
                predicates: signature
                    .constraints
                    .iter()
                    .map(|constraint| MethodPredicate {
                        candidate: instantiate(&constraint.candidate),
                        member: constraint.member.clone(),
                        params: constraint.params.iter().map(instantiate).collect(),
                        result: instantiate(&constraint.result),
                        operator_span: Span::new(0, 0),
                        binding: Some(member.to_owned()),
                        call_span: None,
                        obligation_id: None,
                    })
                    .collect(),
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
        assert_nullary_signature("Float", "isFinite", "Bool");
        assert_nullary_signature("Float", "isNaN", "Bool");
        assert_nullary_signature("Float", "isInfinite", "Bool");
        assert_signature("Float", "ieeeEquals", "Float", "Bool");
        for owner in ["Int", "Float", "Bool", "Text"] {
            assert_nullary_signature(owner, "toText", "Text");
        }
        assert_signature("Int", "toGrouped", "Text", "Text");
        assert_signature("Float", "toFixed", "Int", "Text");
        assert_nullary_signature("Int", "abs", "Int");
        assert_signature("Int", "min", "Int", "Int");
        assert_signature("Int", "max", "Int", "Int");
        assert_nullary_signature("Int", "sign", "Int");
        assert_nullary_signature("Int", "toFloat", "Float");
        assert_signature("Int", "pow", "Int", "Int");
        assert_nullary_signature("Float", "abs", "Float");
        assert_signature("Float", "min", "Float", "Float");
        assert_signature("Float", "max", "Float", "Float");
        assert_signature("Float", "pow", "Float", "Float");
        assert_nullary_signature("Float", "round", "Float");
        assert_nullary_signature("Float", "floor", "Float");
        assert_nullary_signature("Float", "ceil", "Float");
        assert_nullary_signature("Float", "truncate", "Float");
        assert_nullary_signature("Float", "sqrt", "Float");
        // 33 prior + 7 Int + 10 Float
        assert_eq!(BUILTIN_METHODS.len(), 50);
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

    fn assert_nullary_signature(owner: &str, member: &str, result: &str) {
        assert_eq!(
            builtin_method_signature(&named_builtin(owner), member),
            Some(MethodSignature {
                params: vec![],
                result: named_builtin(result),
                predicates: Vec::new(),
            })
        );
    }
}

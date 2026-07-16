use std::collections::HashMap;
use std::rc::Rc;

use aven_check::{RecursiveTypeId, RowEntry, RowTail, Type};
use aven_eval::{
    RuntimeType, RuntimeTypeBindings, RuntimeTypeDescriptor, RuntimeTypeGraph, RuntimeTypeId,
    RuntimeVariantDescriptor, Value,
};
use aven_parser::Literal;

/// Convert checked recursive types into finite evaluator artifacts.
///
/// The graph stores graph-free one-level heads, while every recursive runtime
/// value carries an `Rc` to that graph. Parameterized type functions become
/// natives selecting the already-checked specialization instead of evaluating
/// their recursive source bodies eagerly.
pub(crate) fn runtime_type_bindings(
    type_definitions: &HashMap<String, Type>,
    recursive_type_unfoldings: &HashMap<RecursiveTypeId, Type>,
) -> RuntimeTypeBindings {
    if recursive_type_unfoldings.is_empty() {
        return RuntimeTypeBindings::default();
    }

    let identities = recursive_type_unfoldings
        .keys()
        .enumerate()
        .map(|(index, id)| {
            let runtime_id = RuntimeTypeId(index as u32);
            (*id, (runtime_id, Type::Recursive(*id).render()))
        })
        .collect::<HashMap<_, _>>();
    let graph = Rc::new(RuntimeTypeGraph::new(recursive_type_unfoldings.iter().map(
        |(id, head)| {
            let runtime_id = identities[id].0;
            (runtime_id, descriptor_from_type(head, &identities))
        },
    )));

    let mut bindings = RuntimeTypeBindings::default();
    for (name, ty) in type_definitions {
        if type_contains_recursive(ty) {
            bindings.insert(
                name.clone(),
                value_from_type(ty, &identities, Rc::clone(&graph)),
            );
        }
    }

    let mut functions: HashMap<String, HashMap<String, Value>> = HashMap::new();
    for (id, (runtime_id, display)) in &identities {
        let Some((name, _)) = display.split_once('(') else {
            continue;
        };
        functions.entry(name.to_owned()).or_default().insert(
            display.clone(),
            Value::recursive_type(
                *runtime_id,
                Type::Recursive(*id).render(),
                Rc::clone(&graph),
            ),
        );
    }
    for (name, specializations) in functions {
        let function_name = name.clone();
        bindings.insert(
            name,
            Value::native(move |args| {
                let key = format!(
                    "{function_name}({})",
                    args.iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                specializations.get(&key).cloned().ok_or_else(|| {
                    format!("runtime type specialization `{key}` was not produced by the checker")
                })
            }),
        );
    }

    bindings
}

fn value_from_type(
    ty: &Type,
    identities: &HashMap<RecursiveTypeId, (RuntimeTypeId, String)>,
    graph: Rc<RuntimeTypeGraph>,
) -> Value {
    match ty {
        Type::Named(name) => Value::named_type(name),
        Type::Recursive(id) => {
            let Some((runtime_id, display)) = identities.get(id) else {
                return Value::named_type(ty.render());
            };
            Value::recursive_type(*runtime_id, display, graph)
        }
        Type::Optional(inner) => Value::Type(RuntimeType::Optional(Box::new(value_from_type(
            inner, identities, graph,
        )))),
        Type::Nullable(inner) => Value::Type(RuntimeType::Nullable(Box::new(value_from_type(
            inner, identities, graph,
        )))),
        Type::Apply { callee, args }
            if matches!(callee.as_ref(), Type::Named(name) if name == "Array")
                && args.len() == 1 =>
        {
            Value::Type(RuntimeType::Array(Box::new(value_from_type(
                &args[0], identities, graph,
            ))))
        }
        Type::Apply { callee, args }
            if matches!(callee.as_ref(), Type::Named(name) if name == "Map") && args.len() == 2 =>
        {
            Value::Type(RuntimeType::Map(
                Box::new(value_from_type(&args[0], identities, Rc::clone(&graph))),
                Box::new(value_from_type(&args[1], identities, graph)),
            ))
        }
        Type::Record(row) if row.tail == RowTail::Closed => Value::record(
            row.entries
                .iter()
                .filter_map(|entry| match entry {
                    RowEntry::Field { name, ty } => Some((
                        name.clone(),
                        value_from_type(ty, identities, Rc::clone(&graph)),
                    )),
                    RowEntry::Tag { .. } | RowEntry::Literal { .. } => None,
                })
                .collect(),
        ),
        Type::Deferred
        | Type::Variable(_)
        | Type::Meta(_)
        | Type::Apply { .. }
        | Type::Function { .. }
        | Type::Tuple(_)
        | Type::Record(_)
        | Type::Variant(_) => Value::named_type(ty.render()),
    }
}

fn descriptor_from_type(
    ty: &Type,
    identities: &HashMap<RecursiveTypeId, (RuntimeTypeId, String)>,
) -> RuntimeTypeDescriptor {
    match ty {
        Type::Named(name) => RuntimeTypeDescriptor::Named(name.clone()),
        Type::Recursive(id) => identities.get(id).map_or_else(
            || RuntimeTypeDescriptor::Unsupported(ty.render()),
            |(runtime_id, display)| RuntimeTypeDescriptor::Recursive {
                id: *runtime_id,
                name: display.clone(),
            },
        ),
        Type::Optional(inner) => {
            RuntimeTypeDescriptor::Optional(Box::new(descriptor_from_type(inner, identities)))
        }
        Type::Nullable(inner) => {
            RuntimeTypeDescriptor::Nullable(Box::new(descriptor_from_type(inner, identities)))
        }
        Type::Apply { callee, args }
            if matches!(callee.as_ref(), Type::Named(name) if name == "Array")
                && args.len() == 1 =>
        {
            RuntimeTypeDescriptor::Array(Box::new(descriptor_from_type(&args[0], identities)))
        }
        Type::Apply { callee, args }
            if matches!(callee.as_ref(), Type::Named(name) if name == "Map") && args.len() == 2 =>
        {
            RuntimeTypeDescriptor::Map(
                Box::new(descriptor_from_type(&args[0], identities)),
                Box::new(descriptor_from_type(&args[1], identities)),
            )
        }
        Type::Tuple(items) => RuntimeTypeDescriptor::Tuple(
            items
                .iter()
                .map(|item| descriptor_from_type(item, identities))
                .collect(),
        ),
        Type::Record(row) if row.tail == RowTail::Closed => {
            let fields = row
                .entries
                .iter()
                .map(|entry| match entry {
                    RowEntry::Field { name, ty } => {
                        Some((name.clone(), descriptor_from_type(ty, identities)))
                    }
                    RowEntry::Tag { .. } | RowEntry::Literal { .. } => None,
                })
                .collect::<Option<Vec<_>>>();
            fields.map_or_else(
                || RuntimeTypeDescriptor::Unsupported(ty.render()),
                RuntimeTypeDescriptor::Record,
            )
        }
        Type::Variant(row) if row.tail == RowTail::Closed => RuntimeTypeDescriptor::Variant(
            row.entries
                .iter()
                .map(|entry| match entry {
                    RowEntry::Tag { name, payload } => RuntimeVariantDescriptor::Tag {
                        name: name.clone(),
                        payload: payload
                            .iter()
                            .map(|ty| descriptor_from_type(ty, identities))
                            .collect(),
                    },
                    RowEntry::Literal { value } => {
                        RuntimeVariantDescriptor::Literal(render_literal(value).to_owned())
                    }
                    RowEntry::Field { .. } => {
                        RuntimeVariantDescriptor::Literal("<record-field>".to_owned())
                    }
                })
                .collect(),
        ),
        Type::Deferred
        | Type::Variable(_)
        | Type::Meta(_)
        | Type::Apply { .. }
        | Type::Function { .. }
        | Type::Record(_)
        | Type::Variant(_) => RuntimeTypeDescriptor::Unsupported(ty.render()),
    }
}

fn type_contains_recursive(ty: &Type) -> bool {
    match ty {
        Type::Recursive(_) => true,
        Type::Apply { callee, args } => {
            type_contains_recursive(callee) || args.iter().any(type_contains_recursive)
        }
        Type::Function { params, result, .. } => {
            params.iter().any(type_contains_recursive) || type_contains_recursive(result)
        }
        Type::Optional(inner) | Type::Nullable(inner) => type_contains_recursive(inner),
        Type::Tuple(items) => items.iter().any(type_contains_recursive),
        Type::Record(row) | Type::Variant(row) => row.entries.iter().any(|entry| match entry {
            RowEntry::Field { ty, .. } => type_contains_recursive(ty),
            RowEntry::Tag { payload, .. } => payload.iter().any(type_contains_recursive),
            RowEntry::Literal { .. } => false,
        }),
        Type::Deferred | Type::Named(_) | Type::Variable(_) | Type::Meta(_) => false,
    }
}

fn render_literal(literal: &Literal) -> &str {
    match literal {
        Literal::Bool(true) => "true",
        Literal::Bool(false) => "false",
        Literal::Number(value) | Literal::String(value) | Literal::Regex(value) => value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameterized_recursive_binding_builds_a_finite_graph() {
        let parsed = aven_parser::parse_module(
            "Chain = (t: Type) => { value: t, next: ?Chain(t) }\n\
             Target = Chain(Int)\n\
             (Target, Chain(Int))\n",
        );
        let checked = aven_check::check_module(&parsed.module);
        assert!(
            checked.diagnostics.is_empty(),
            "program checks: {:?}",
            checked.diagnostics
        );

        let bindings = runtime_type_bindings(
            &checked.type_definitions,
            &checked.recursive_type_unfoldings,
        );
        let outcome = aven_eval::eval_module_with_globals_imports_and_runtime_types(
            &parsed.module,
            Vec::new(),
            &aven_eval::ModuleImports::default(),
            &bindings,
        );
        assert!(
            outcome.diagnostics.is_empty(),
            "program evaluates: {:?}",
            outcome.diagnostics
        );
        let Some(Value::Tuple(values)) = outcome.value else {
            panic!("program returns both runtime type artifacts");
        };
        let [target, selected] = values.as_slice() else {
            panic!("program returns two runtime type artifacts");
        };
        let Value::Type(RuntimeType::Recursive(reference)) = target else {
            panic!("named specialization is a recursive reference");
        };
        assert_eq!(reference.name.as_ref(), "Chain(Int)");
        assert_eq!(reference.graph.len(), 1);
        assert_eq!(selected, target);
    }

    #[test]
    fn applied_recursive_type_value_builds_its_own_finite_graph() {
        let parsed = aven_parser::parse_module(
            "Chain = (t: Type) => { value: t, next: ?Chain(t) }\n\
             target = Chain(Int)\n\
             target\n",
        );
        let checked = aven_check::check_module(&parsed.module);
        assert!(
            checked.diagnostics.is_empty(),
            "program checks: {:?}",
            checked.diagnostics
        );
        assert_eq!(checked.recursive_type_unfoldings.len(), 1);

        let bindings = runtime_type_bindings(
            &checked.type_definitions,
            &checked.recursive_type_unfoldings,
        );
        let outcome = aven_eval::eval_module_with_globals_imports_and_runtime_types(
            &parsed.module,
            Vec::new(),
            &aven_eval::ModuleImports::default(),
            &bindings,
        );
        assert!(
            outcome.diagnostics.is_empty(),
            "program evaluates: {:?}",
            outcome.diagnostics
        );
        let Some(Value::Type(RuntimeType::Recursive(reference))) = outcome.value else {
            panic!("applied specialization evaluates to a recursive type");
        };
        assert_eq!(reference.name.as_ref(), "Chain(Int)");
        assert_eq!(reference.graph.len(), 1);
    }
}

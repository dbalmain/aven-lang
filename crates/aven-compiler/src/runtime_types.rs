use std::collections::HashMap;
use std::rc::Rc;

use aven_check::{RecursiveTypeId, RowEntry, RowTail, Type};
use aven_eval::{
    RuntimeType, RuntimeTypeBindings, RuntimeTypeDescriptor, RuntimeTypeGraph, RuntimeTypeId,
    RuntimeVariantDescriptor, Value,
};
/// Convert checked types into canonical finite evaluator artifacts.
///
/// The graph stores graph-free one-level recursive heads, while every runtime
/// type value carries an `Rc` to that graph. Parameterized recursive type
/// functions become natives selecting the already-checked specialization
/// instead of evaluating their recursive source bodies eagerly.
pub(crate) fn runtime_type_bindings(
    type_definitions: &HashMap<String, Type>,
    recursive_type_unfoldings: &HashMap<RecursiveTypeId, Type>,
    named_family_aliases: &HashMap<String, String>,
) -> RuntimeTypeBindings {
    let identities = recursive_type_unfoldings
        .keys()
        .enumerate()
        .map(|(index, id)| {
            let runtime_id = RuntimeTypeId(index as u32);
            (*id, (runtime_id, Type::Recursive(*id).render()))
        })
        .collect::<HashMap<_, _>>();
    let graph = Rc::new(RuntimeTypeGraph::new(
        recursive_type_unfoldings.iter().filter_map(|(id, head)| {
            let runtime_id = identities[id].0;
            descriptor_from_type(head, &identities).map(|head| (runtime_id, head))
        }),
    ));

    let mut bindings = RuntimeTypeBindings::default();
    for (name, ty) in type_definitions {
        // Named families and their transparent aliases are runtime
        // constructors, not structural type artifacts. Their declarations are
        // materialized by the evaluator's named-family path.
        if named_family_aliases.contains_key(name) {
            continue;
        }
        if let Some(descriptor) = descriptor_from_type(ty, &identities) {
            let runtime_type = if descriptor_contains_recursive(&descriptor) {
                RuntimeType::with_graph(descriptor, Rc::clone(&graph))
            } else {
                RuntimeType::new(descriptor)
            };
            bindings.insert(name.clone(), Value::Type(runtime_type));
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

fn descriptor_from_type(
    ty: &Type,
    identities: &HashMap<RecursiveTypeId, (RuntimeTypeId, String)>,
) -> Option<RuntimeTypeDescriptor> {
    match ty {
        Type::Named(name) => Some(RuntimeTypeDescriptor::Named(name.clone())),
        Type::Recursive(id) => {
            identities
                .get(id)
                .map(|(runtime_id, display)| RuntimeTypeDescriptor::Recursive {
                    id: *runtime_id,
                    name: display.clone(),
                })
        }
        Type::Apply { callee, args } => Some(RuntimeTypeDescriptor::Apply {
            callee: Box::new(descriptor_from_type(callee, identities)?),
            args: args
                .iter()
                .map(|arg| descriptor_from_type(arg, identities))
                .collect::<Option<_>>()?,
        }),
        Type::Function {
            params,
            result,
            required,
        } => Some(RuntimeTypeDescriptor::Function {
            params: params
                .iter()
                .map(|param| descriptor_from_type(param, identities))
                .collect::<Option<_>>()?,
            result: Box::new(descriptor_from_type(result, identities)?),
            required: *required,
        }),
        Type::Optional(inner) => Some(RuntimeTypeDescriptor::Optional(Box::new(
            descriptor_from_type(inner, identities)?,
        ))),
        Type::Nullable(inner) => Some(RuntimeTypeDescriptor::Nullable(Box::new(
            descriptor_from_type(inner, identities)?,
        ))),
        Type::Tuple(items) => Some(RuntimeTypeDescriptor::Tuple(
            items
                .iter()
                .map(|item| descriptor_from_type(item, identities))
                .collect::<Option<_>>()?,
        )),
        Type::Record(row) => Some(RuntimeTypeDescriptor::Record(record_fields(
            row, identities,
        )?)),
        Type::SlotRecord { data, slots } => Some(RuntimeTypeDescriptor::SlotRecord {
            data: record_fields(data, identities)?,
            slots: record_fields(slots, identities)?,
        }),
        Type::Variant(row) if row.tail == RowTail::Closed => Some(RuntimeTypeDescriptor::Variant(
            row.entries
                .iter()
                .map(|entry| match entry {
                    RowEntry::Tag { name, payload } => Some(RuntimeVariantDescriptor::Tag {
                        name: name.clone(),
                        payload: payload
                            .iter()
                            .map(|ty| descriptor_from_type(ty, identities))
                            .collect::<Option<_>>()?,
                    }),
                    RowEntry::Literal { value } => {
                        Some(RuntimeVariantDescriptor::Literal(value.clone()))
                    }
                    RowEntry::Field { .. } => None,
                })
                .collect::<Option<_>>()?,
        )),
        Type::Error | Type::Deferred | Type::Variable(_) | Type::Meta(_) | Type::Variant(_) => None,
    }
}

fn record_fields(
    row: &aven_check::Row,
    identities: &HashMap<RecursiveTypeId, (RuntimeTypeId, String)>,
) -> Option<Vec<(String, RuntimeTypeDescriptor)>> {
    if row.tail != RowTail::Closed {
        return None;
    }
    row.entries
        .iter()
        .map(|entry| match entry {
            RowEntry::Field { name, ty } => {
                Some((name.clone(), descriptor_from_type(ty, identities)?))
            }
            RowEntry::Tag { .. } | RowEntry::Literal { .. } => None,
        })
        .collect()
}

fn descriptor_contains_recursive(descriptor: &RuntimeTypeDescriptor) -> bool {
    match descriptor {
        RuntimeTypeDescriptor::Recursive { .. } => true,
        RuntimeTypeDescriptor::Apply { callee, args } => {
            descriptor_contains_recursive(callee) || args.iter().any(descriptor_contains_recursive)
        }
        RuntimeTypeDescriptor::Function { params, result, .. } => {
            params.iter().any(descriptor_contains_recursive)
                || descriptor_contains_recursive(result)
        }
        RuntimeTypeDescriptor::Optional(inner) | RuntimeTypeDescriptor::Nullable(inner) => {
            descriptor_contains_recursive(inner)
        }
        RuntimeTypeDescriptor::Tuple(items) => items.iter().any(descriptor_contains_recursive),
        RuntimeTypeDescriptor::Record(fields) => fields
            .iter()
            .any(|(_, ty)| descriptor_contains_recursive(ty)),
        RuntimeTypeDescriptor::SlotRecord { data, slots } => data
            .iter()
            .chain(slots)
            .any(|(_, ty)| descriptor_contains_recursive(ty)),
        RuntimeTypeDescriptor::Variant(entries) => entries.iter().any(|entry| match entry {
            RuntimeVariantDescriptor::Tag { payload, .. } => {
                payload.iter().any(descriptor_contains_recursive)
            }
            RuntimeVariantDescriptor::Literal(_) => false,
        }),
        RuntimeTypeDescriptor::Named(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_closed_checked_type_shape_has_a_runtime_descriptor() {
        let closed_row = |entries| aven_check::Row {
            entries,
            tail: RowTail::Closed,
        };
        let variant = Type::Variant(closed_row(vec![
            RowEntry::Tag {
                name: "Ok".to_owned(),
                payload: vec![Type::Tuple(vec![
                    Type::Named("Int".to_owned()),
                    Type::Nullable(Box::new(Type::Named("Text".to_owned()))),
                ])],
            },
            RowEntry::Literal {
                value: aven_parser::Literal::Bool(false),
            },
        ]));
        let ty = Type::SlotRecord {
            data: Box::new(closed_row(vec![RowEntry::Field {
                name: "values".to_owned(),
                ty: Type::Apply {
                    callee: Box::new(Type::Named("Map".to_owned())),
                    args: vec![Type::Named("Text".to_owned()), variant],
                },
            }])),
            slots: Box::new(closed_row(vec![RowEntry::Field {
                name: "load".to_owned(),
                ty: Type::Function {
                    params: vec![Type::Optional(Box::new(Type::Named("Int".to_owned())))],
                    result: Box::new(Type::Record(closed_row(vec![RowEntry::Field {
                        name: "done".to_owned(),
                        ty: Type::Named("Bool".to_owned()),
                    }]))),
                    required: 0,
                },
            }])),
        };

        let descriptor = descriptor_from_type(&ty, &HashMap::new())
            .expect("all closed checked type forms are reifiable");
        assert!(matches!(
            descriptor,
            RuntimeTypeDescriptor::SlotRecord { data, slots }
                if data.len() == 1 && slots.len() == 1
        ));
    }

    #[test]
    fn non_recursive_aliases_do_not_inherit_an_unrelated_recursive_graph() {
        let parsed = aven_parser::parse_module(
            "Node = { next: ?Node }\n\
             Alias = Text\n\
             Alias == Text\n",
        );
        let checked = aven_check::check_module(&parsed.module);
        assert!(checked.diagnostics.is_empty(), "program checks");
        let bindings = runtime_type_bindings(
            &checked.type_definitions,
            &checked.recursive_type_unfoldings,
            &checked.named_family_aliases,
        );
        let outcome = aven_eval::eval_module_with_options(
            &parsed.module,
            aven_eval::EvalModuleOptions::default().with_runtime_types(&bindings),
        );

        assert!(outcome.diagnostics.is_empty(), "program evaluates");
        assert_eq!(outcome.value, Some(Value::Bool(true)));
    }

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
            &checked.named_family_aliases,
        );
        let outcome = aven_eval::eval_module_with_options(
            &parsed.module,
            aven_eval::EvalModuleOptions::default().with_runtime_types(&bindings),
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
        let Value::Type(reference) = target else {
            panic!("named specialization is a recursive reference");
        };
        assert!(matches!(
            reference.descriptor(),
            RuntimeTypeDescriptor::Recursive { name, .. } if name == "Chain(Int)"
        ));
        assert_eq!(reference.graph().len(), 1);
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
            &checked.named_family_aliases,
        );
        let outcome = aven_eval::eval_module_with_options(
            &parsed.module,
            aven_eval::EvalModuleOptions::default().with_runtime_types(&bindings),
        );
        assert!(
            outcome.diagnostics.is_empty(),
            "program evaluates: {:?}",
            outcome.diagnostics
        );
        let Some(Value::Type(reference)) = outcome.value else {
            panic!("applied specialization evaluates to a recursive type");
        };
        assert!(matches!(
            reference.descriptor(),
            RuntimeTypeDescriptor::Recursive { name, .. } if name == "Chain(Int)"
        ));
        assert_eq!(reference.graph().len(), 1);
    }
}

use super::checker_api;
use aven_parser::{ExprKind, Item, RecordEntry};
use std::collections::HashMap;
use std::collections::HashSet;

pub fn install(imports: &mut checker_api::ModuleImports) {
    let parsed = aven_parser::parse_module(include_str!("../../../aven-host/std/prelude.av"));
    assert!(parsed.diagnostics.is_empty());
    let checked = checker_api::check_module(&parsed.module);
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let Some(Item::Expr(exports)) = parsed.module.items.last() else {
        panic!("test prelude must end with its export record");
    };
    let ExprKind::Record(entries) = &exports.kind else {
        panic!("test prelude must export a literal record");
    };
    let names = entries
        .iter()
        .map(|entry| match entry {
            RecordEntry::Shorthand { name, .. } => name.clone(),
            _ => panic!("extend the test helper when prelude exports use computed fields"),
        })
        .collect::<HashSet<_>>();
    assert!(
        checked.named_families.is_empty()
            && checked.slot_reifications.is_empty()
            && checked.direct_slot_inits.is_empty()
            && checked.primitive_family_coercions.is_empty(),
        "test prelude needs runtime elaboration support"
    );
    let functions = parsed
        .module
        .items
        .iter()
        .filter_map(|item| {
            let Item::Binding(binding) = item else {
                return None;
            };
            if !names.contains(&binding.name) {
                return None;
            }
            let ExprKind::Lambda { params, body, .. } = &binding.value.kind else {
                return None;
            };
            Some((
                binding.name.clone(),
                checker_api::ComptimeExport::from_lambda(&binding.name, params, body),
            ))
        })
        .collect::<HashMap<_, _>>();
    let qualified = checked
        .top_level_qualified_types
        .into_iter()
        .filter(|(name, _)| names.contains(name))
        .collect();
    imports.set_prelude_exports(qualified, functions);
    imports.set_prelude_modules(vec![parsed.module], false);
}

pub fn imports() -> checker_api::ModuleImports {
    let mut imports = checker_api::ModuleImports::default();
    install(&mut imports);
    imports
}

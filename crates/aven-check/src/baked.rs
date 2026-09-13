//! Build-time checked modules and the exact context required to reuse them.
//!
//! The compiler owns graph ordering and feeds this snapshot the same imports
//! at build time and runtime. Source equality and a rebuild provide invalidation;
//! there is no runtime file cache. Host callbacks cannot be captured, so baking
//! declines any check that invokes one, even if it happens to return a type.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use aven_parser::{Module, ModuleRole};

use crate::{
    CheckOutput, ComptimeArg, ComptimeError, ComptimeModuleIdentity, HostComptimeFn,
    HostComptimeParam, HostGlobals, HostStatics, ModuleImports, Type,
    check_module_with_host_globals_and_imports_in_role,
};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct HostContext {
    types: Vec<(String, Type)>,
    type_definitions: Vec<(String, Type)>,
    statics: HostStatics,
    comptime_params: HashMap<String, Vec<HostComptimeParam>>,
    type_definition_module: ComptimeModuleIdentity,
}

impl HostContext {
    fn new(globals: &HostGlobals) -> Self {
        Self {
            types: globals.types.clone(),
            type_definitions: globals.type_definitions.clone(),
            statics: globals.statics.clone(),
            comptime_params: globals
                .comptime_fns
                .iter()
                .map(|(name, spec)| (name.clone(), spec.comptime_params.clone()))
                .collect(),
            type_definition_module: globals.type_definition_module.clone(),
        }
    }

    fn first_mismatch(&self, globals: &HostGlobals) -> Option<String> {
        named_list_mismatch("host.types", &self.types, &globals.types)
            .or_else(|| {
                named_list_mismatch(
                    "host.type_definitions",
                    &self.type_definitions,
                    &globals.type_definitions,
                )
            })
            .or_else(|| statics_mismatch(&self.statics, &globals.statics))
            .or_else(|| {
                (self.type_definition_module != globals.type_definition_module).then(|| {
                    format!(
                        "host.type_definition_module baked={:?} runtime={:?}",
                        self.type_definition_module, globals.type_definition_module
                    )
                })
            })
            .or_else(|| comptime_params_mismatch(&self.comptime_params, globals))
    }
}

/// A checked module with its complete, comparable checking inputs.
///
/// Serialize only artifacts produced by [`Self::check`]. The format is internal
/// to one build and must never be accepted from a runtime cache or untrusted
/// input. Recursive type identities are re-interned by semantic key on decode.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BakedCheck {
    source: String,
    host: HostContext,
    imports: ModuleImports,
    identity: ComptimeModuleIdentity,
    role: ModuleRole,
    output: CheckOutput,
}

impl BakedCheck {
    /// Check with callback sentinels. Equal types and callback parameter specs
    /// determine the same execution until a resolver is invoked; if no resolver
    /// was invoked, its implementation cannot have influenced the result.
    pub fn check(
        source: &str,
        module: &Module,
        globals: &HostGlobals,
        imports: &ModuleImports,
        identity: ComptimeModuleIdentity,
        role: ModuleRole,
    ) -> Option<Self> {
        let invoked = Rc::new(Cell::new(false));
        let mut guarded_globals = globals.clone();
        for (_, spec) in &mut guarded_globals.comptime_fns {
            spec.resolver = Rc::new(CallbackSentinel(Rc::clone(&invoked)));
        }
        let output = check_module_with_host_globals_and_imports_in_role(
            module,
            &guarded_globals,
            imports,
            identity.clone(),
            role,
        );
        (!invoked.get()).then(|| Self {
            source: source.to_owned(),
            host: HostContext::new(globals),
            imports: imports.clone(),
            identity,
            role,
            output,
        })
    }

    /// Consume this artifact only if every checking input matches. A custom
    /// host, prelude, ambient environment, module role or source falls back to
    /// ordinary checking. No best-effort or partial interface reuse is sound.
    pub fn into_checked(
        self,
        source: &str,
        globals: &HostGlobals,
        imports: &ModuleImports,
        identity: &ComptimeModuleIdentity,
        role: ModuleRole,
    ) -> Option<CheckOutput> {
        self.first_mismatch(source, globals, imports, identity, role)
            .is_none()
            .then_some(self.output)
    }

    /// First checking input that differs from this snapshot, for hit/miss tracing.
    pub fn first_mismatch(
        &self,
        source: &str,
        globals: &HostGlobals,
        imports: &ModuleImports,
        identity: &ComptimeModuleIdentity,
        role: ModuleRole,
    ) -> Option<String> {
        if self.source != source {
            return Some("source".into());
        }
        if let Some(field) = self.host.first_mismatch(globals) {
            return Some(field);
        }
        if let Some(field) = imports_first_mismatch(&self.imports, imports) {
            return Some(field);
        }
        if self.identity != *identity {
            return Some(format!(
                "identity baked={:?} runtime={:?}",
                self.identity, identity
            ));
        }
        if self.role != role {
            return Some(format!("role baked={:?} runtime={:?}", self.role, role));
        }
        None
    }
}

fn named_list_mismatch<T: PartialEq>(
    field: &str,
    baked: &[(String, T)],
    runtime: &[(String, T)],
) -> Option<String> {
    if baked == runtime {
        return None;
    }
    let baked_names: Vec<&str> = baked.iter().map(|(name, _)| name.as_str()).collect();
    let runtime_names: Vec<&str> = runtime.iter().map(|(name, _)| name.as_str()).collect();
    if baked_names != runtime_names {
        return Some(format!(
            "{field} names baked={baked_names:?} runtime={runtime_names:?}"
        ));
    }
    let name = baked
        .iter()
        .zip(runtime)
        .find_map(|((name, left), (_, right))| (left != right).then_some(name.as_str()))
        .unwrap_or("?");
    Some(format!("{field}[{name}] value"))
}

fn statics_mismatch(baked: &HostStatics, runtime: &HostStatics) -> Option<String> {
    named_list_mismatch("host.statics", baked, runtime).map(|mismatch| {
        if mismatch.ends_with(" value") {
            let type_name = mismatch
                .strip_prefix("host.statics[")
                .and_then(|rest| rest.strip_suffix("] value"))
                .unwrap_or("?");
            match (baked.iter().find(|(name, _)| name == type_name), runtime.iter().find(|(name, _)| name == type_name)) {
                (Some((_, left)), Some((_, right))) => named_list_mismatch(
                    &format!("host.statics[{type_name}]"),
                    left,
                    right,
                )
                .unwrap_or(mismatch),
                _ => mismatch,
            }
        } else {
            mismatch
        }
    })
}

fn comptime_params_mismatch(
    baked: &HashMap<String, Vec<HostComptimeParam>>,
    globals: &HostGlobals,
) -> Option<String> {
    let runtime: HashMap<&str, _> = globals
        .comptime_fns
        .iter()
        .map(|(name, spec)| (name.as_str(), &spec.comptime_params))
        .collect();
    let baked_refs: HashMap<&str, _> = baked
        .iter()
        .map(|(name, params)| (name.as_str(), params))
        .collect();
    if baked_refs == runtime {
        return None;
    }
    let mut baked_names: Vec<&str> = baked_refs.keys().copied().collect();
    baked_names.sort_unstable();
    let mut runtime_names: Vec<&str> = runtime.keys().copied().collect();
    runtime_names.sort_unstable();
    if baked_names != runtime_names {
        return Some(format!(
            "host.comptime_params names baked={baked_names:?} runtime={runtime_names:?}"
        ));
    }
    let name = baked_names
        .iter()
        .copied()
        .find(|name| baked_refs.get(name) != runtime.get(name))
        .unwrap_or("?");
    Some(format!("host.comptime_params[{name}] value"))
}

fn imports_first_mismatch(baked: &ModuleImports, runtime: &ModuleImports) -> Option<String> {
    if baked == runtime {
        return None;
    }
    if baked.types != runtime.types {
        return Some(map_mismatch(
            "imports.types",
            &baked.types,
            &runtime.types,
        ));
    }
    if baked.type_exports != runtime.type_exports {
        return Some(map_mismatch(
            "imports.type_exports",
            &baked.type_exports,
            &runtime.type_exports,
        ));
    }
    if baked.qualified_exports != runtime.qualified_exports {
        return Some(map_mismatch(
            "imports.qualified_exports",
            &baked.qualified_exports,
            &runtime.qualified_exports,
        ));
    }
    if baked.named_family_exports != runtime.named_family_exports {
        return Some(map_mismatch(
            "imports.named_family_exports",
            &baked.named_family_exports,
            &runtime.named_family_exports,
        ));
    }
    if baked.comptime_exports != runtime.comptime_exports {
        return Some(map_mismatch(
            "imports.comptime_exports",
            &baked.comptime_exports,
            &runtime.comptime_exports,
        ));
    }
    if baked.prelude_qualified_exports != runtime.prelude_qualified_exports {
        return Some(map_mismatch(
            "imports.prelude_qualified_exports",
            &baked.prelude_qualified_exports,
            &runtime.prelude_qualified_exports,
        ));
    }
    if baked.prelude_comptime_exports != runtime.prelude_comptime_exports {
        return Some(map_mismatch(
            "imports.prelude_comptime_exports",
            &baked.prelude_comptime_exports,
            &runtime.prelude_comptime_exports,
        ));
    }
    if baked.prelude_modules != runtime.prelude_modules {
        return Some(format!(
            "imports.prelude_modules len baked={} runtime={}",
            baked.prelude_modules.len(),
            runtime.prelude_modules.len()
        ));
    }
    if baked.prelude_requires_elaboration != runtime.prelude_requires_elaboration {
        return Some(format!(
            "imports.prelude_requires_elaboration baked={} runtime={}",
            baked.prelude_requires_elaboration, runtime.prelude_requires_elaboration
        ));
    }
    if baked.recursive_type_unfoldings != runtime.recursive_type_unfoldings {
        return Some(format!(
            "imports.recursive_type_unfoldings len baked={} runtime={}",
            baked.recursive_type_unfoldings.len(),
            runtime.recursive_type_unfoldings.len()
        ));
    }
    if baked.builtin_methods != runtime.builtin_methods {
        if baked.builtin_methods.methods != runtime.builtin_methods.methods {
            return Some(format!(
                "imports.builtin_methods.methods len baked={} runtime={}",
                baked.builtin_methods.methods.len(),
                runtime.builtin_methods.methods.len()
            ));
        }
        return Some(format!(
            "imports.builtin_methods.comptime_modules len baked={} runtime={}",
            baked.builtin_methods.comptime_modules.len(),
            runtime.builtin_methods.comptime_modules.len()
        ));
    }
    if baked.trusted_builtin_method_source != runtime.trusted_builtin_method_source {
        return Some(format!(
            "imports.trusted_builtin_method_source baked={} runtime={}",
            baked.trusted_builtin_method_source, runtime.trusted_builtin_method_source
        ));
    }
    Some("imports".into())
}

fn map_mismatch<K: Eq + std::hash::Hash + std::fmt::Debug, V: PartialEq>(
    field: &str,
    baked: &HashMap<K, V>,
    runtime: &HashMap<K, V>,
) -> String {
    if baked.len() != runtime.len() || baked.keys().any(|key| !runtime.contains_key(key)) {
        let mut baked_keys: Vec<_> = baked.keys().collect();
        baked_keys.sort_by(|left, right| format!("{left:?}").cmp(&format!("{right:?}")));
        let mut runtime_keys: Vec<_> = runtime.keys().collect();
        runtime_keys.sort_by(|left, right| format!("{left:?}").cmp(&format!("{right:?}")));
        return format!("{field} keys baked={baked_keys:?} runtime={runtime_keys:?}");
    }
    format!("{field} value")
}

struct CallbackSentinel(Rc<Cell<bool>>);

impl HostComptimeFn for CallbackSentinel {
    fn resolve(&self, _args: &[ComptimeArg]) -> Result<Type, ComptimeError> {
        self.0.set(true);
        Err(ComptimeError::new("host callback cannot be baked"))
    }
}

// JSON object keys cannot represent spans or semantic recursive type keys.
// Sequence entries preserve those keys without a second wire representation.
pub(crate) mod map_entries {
    use super::HashMap;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::hash::Hash;

    pub fn serialize<K: Serialize, V: Serialize, S: Serializer>(
        map: &HashMap<K, V>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        map.iter().collect::<Vec<_>>().serialize(serializer)
    }

    pub fn deserialize<
        'de,
        K: Deserialize<'de> + Eq + Hash,
        V: Deserialize<'de>,
        D: Deserializer<'de>,
    >(
        deserializer: D,
    ) -> Result<HashMap<K, V>, D::Error> {
        Ok(Vec::<(K, V)>::deserialize(deserializer)?
            .into_iter()
            .collect())
    }
}

pub(crate) mod integer {
    use aven_core::Int;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &Int, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(value)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Int, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HostComptimeFnSpec, build};
    use aven_parser::parse_module;

    #[test]
    fn checked_output_matches_fresh_and_rejects_context_changes() {
        // A same-source snapshot must not hide host-dependent type errors or
        // accept a different ambient/prelude/import environment.
        let source = "value: Text = external\n{ value }\n";
        let module = parse_module(source).module;
        let globals = HostGlobals::types_only(&[("external".into(), build::text())]);
        let imports = ModuleImports::default();
        let identity = ComptimeModuleIdentity::specifier("test");
        let baked = BakedCheck::check(
            source,
            &module,
            &globals,
            &imports,
            identity.clone(),
            ModuleRole::Dependency,
        )
        .expect("module never calls a host callback");
        let fresh = check_module_with_host_globals_and_imports_in_role(
            &module,
            &globals,
            &imports,
            identity.clone(),
            ModuleRole::Dependency,
        );
        assert_eq!(
            baked.clone().into_checked(
                source,
                &globals,
                &imports,
                &identity,
                ModuleRole::Dependency
            ),
            Some(fresh)
        );
        let different = HostGlobals::types_only(&[("external".into(), build::int())]);
        assert!(
            baked
                .clone()
                .into_checked(
                    source,
                    &different,
                    &imports,
                    &identity,
                    ModuleRole::Dependency
                )
                .is_none()
        );
        let mut different = imports.clone();
        different.insert("other", build::int());
        assert!(
            baked
                .clone()
                .into_checked(
                    source,
                    &globals,
                    &different,
                    &identity,
                    ModuleRole::Dependency
                )
                .is_none()
        );
        let mut different = imports.clone();
        different.set_trusted_builtin_method_source(true);
        assert!(
            baked
                .clone()
                .into_checked(
                    source,
                    &globals,
                    &different,
                    &identity,
                    ModuleRole::Dependency
                )
                .is_none()
        );
        assert!(
            baked
                .clone()
                .into_checked(
                    "{ value: 1 }",
                    &globals,
                    &imports,
                    &identity,
                    ModuleRole::Dependency
                )
                .is_none()
        );
        assert!(
            baked
                .clone()
                .into_checked(source, &globals, &imports, &identity, ModuleRole::Entry)
                .is_none()
        );
        assert!(
            baked
                .into_checked(
                    source,
                    &globals,
                    &imports,
                    &ComptimeModuleIdentity::Current,
                    ModuleRole::Dependency
                )
                .is_none()
        );
    }

    struct UnexpectedCallback;

    impl HostComptimeFn for UnexpectedCallback {
        fn resolve(&self, _args: &[ComptimeArg]) -> Result<Type, ComptimeError> {
            panic!("baking must never execute a host callback");
        }
    }

    #[test]
    fn callback_dependent_modules_are_not_baked() {
        // Equal host signatures cannot establish equal callback behavior.
        let globals = HostGlobals::new(
            vec![(
                "lookup".into(),
                build::function(vec![build::text()], build::text()),
            )],
            vec![(
                "lookup".into(),
                HostComptimeFnSpec::new(Rc::new(UnexpectedCallback), vec![0]),
            )],
        );
        let source = "value = lookup(\"key\")\n{ value }\n";
        assert!(
            BakedCheck::check(
                source,
                &parse_module(source).module,
                &globals,
                &ModuleImports::default(),
                ComptimeModuleIdentity::Current,
                ModuleRole::Entry
            )
            .is_none()
        );
    }
}

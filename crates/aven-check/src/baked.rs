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

    fn matches(&self, globals: &HostGlobals) -> bool {
        self.types == globals.types
            && self.type_definitions == globals.type_definitions
            && self.statics == globals.statics
            && self.type_definition_module == globals.type_definition_module
            && comptime_params_match(&self.comptime_params, globals)
    }
}

fn comptime_params_match(
    baked: &HashMap<String, Vec<HostComptimeParam>>,
    globals: &HostGlobals,
) -> bool {
    // Registration order on the run host need not match bake-time HashMap
    // insertion order. Zip-equality missed every module under `aven run`.
    let runtime: HashMap<&str, _> = globals
        .comptime_fns
        .iter()
        .map(|(name, spec)| (name.as_str(), &spec.comptime_params))
        .collect();
    let baked: HashMap<&str, _> = baked
        .iter()
        .map(|(name, params)| (name.as_str(), params))
        .collect();
    baked == runtime
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
        (self.source == source
            && self.host.matches(globals)
            && self.imports == *imports
            && self.identity == *identity
            && self.role == role)
            .then_some(self.output)
    }
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

    #[test]
    fn comptime_params_match_independent_of_registration_order() {
        // `aven run` registers host comptime functions in a different Vec
        // order than bake-time `standard_check_host_globals`. Zip-equality
        // against HashMap iteration missed every baked module on that path.
        struct Unused;
        impl HostComptimeFn for Unused {
            fn resolve(&self, _args: &[ComptimeArg]) -> Result<Type, ComptimeError> {
                panic!("unused callback");
            }
        }
        let globals = HostGlobals::new(
            vec![],
            vec![
                (
                    "first".into(),
                    HostComptimeFnSpec::new(Rc::new(Unused), vec![0]),
                ),
                (
                    "second".into(),
                    HostComptimeFnSpec::new(Rc::new(Unused), vec![1]),
                ),
            ],
        );
        let source = "{ 1 }\n";
        let baked = BakedCheck::check(
            source,
            &parse_module(source).module,
            &globals,
            &ModuleImports::default(),
            ComptimeModuleIdentity::specifier("test"),
            ModuleRole::Dependency,
        )
        .expect("module never calls a host callback");
        let mut reversed = globals.clone();
        reversed.comptime_fns.reverse();
        assert!(
            baked
                .into_checked(
                    source,
                    &reversed,
                    &ModuleImports::default(),
                    &ComptimeModuleIdentity::specifier("test"),
                    ModuleRole::Dependency,
                )
                .is_some()
        );
    }
}

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use aven_check::build;
use aven_compiler::{
    HostGlobals, ModuleRoots, SourceOverlay, check_path_with_host_globals,
    check_path_with_host_globals_and_overlay, check_path_with_host_globals_and_roots,
    eval_path_with_globals, eval_path_with_globals_and_roots,
};
use aven_core::codes;

#[test]
fn checks_diamond_graph_and_private_bindings() {
    let dir = TempDir::new("diamond-check");
    write(
        dir.path(),
        "d.av",
        "value = \"d\"\n_private = \"hidden\"\n{ value }\n",
    );
    write(dir.path(), "b.av", "d = import(\"./d\")\n{ b: d.value }\n");
    write(
        dir.path(),
        "c.av",
        "d = import(\"./d.av\")\n{ c: d.value }\n",
    );
    write(
        dir.path(),
        "main.av",
        "b = import(\"./b\")\nc = import(\"./c\")\n{ b: b.b, c: c.c }\n",
    );

    let output = check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
        .expect("check should load graph");

    assert_no_errors(&output.reports);
}

#[test]
fn disabled_capability_modules_explain_the_required_host_registration() {
    let dir = TempDir::new("disabled-capability");
    write(
        dir.path(),
        "main.av",
        "clock = import(\"std/clock\")\n{ clock }\n",
    );
    let roots = ModuleRoots::discover(&dir.path().join("main.av"))
        .with_library(
            "std",
            HashMap::from([("std".to_owned(), "{ version: \"test\" }")]),
        )
        .with_disabled_capability_module("std/clock", "clock", "register_clock");

    let output = check_path_with_host_globals_and_roots(
        &dir.path().join("main.av"),
        &HostGlobals::default(),
        &roots,
    )
    .expect("check should load graph");

    let messages = output
        .reports
        .iter()
        .flat_map(|report| &report.diagnostics)
        .map(|diagnostic| format!("{} {:?}", diagnostic.message, diagnostic.notes))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(messages.contains("clock capability"), "{messages}");
    assert!(messages.contains("register_clock"), "{messages}");
}

#[test]
fn library_only_globals_are_hidden_from_user_modules_but_visible_to_library_modules() {
    let dir = TempDir::new("library-only-globals");
    write(dir.path(), "main.av", "now()\n");
    let roots = ModuleRoots::discover(&dir.path().join("main.av"))
        .with_library("std", HashMap::from([("std/clock".to_owned(), "{ now }")]))
        .with_library_only_global_names(["now"]);
    let globals = HostGlobals::new(
        vec![("now".to_owned(), build::function(vec![], build::int()))],
        vec![],
    );

    let output =
        check_path_with_host_globals_and_roots(&dir.path().join("main.av"), &globals, &roots)
            .expect("check should load graph");
    assert!(
        output
            .reports
            .iter()
            .any(aven_core::DiagnosticReport::has_errors)
    );

    let output = eval_path_with_globals_and_roots(
        &dir.path().join("main.av"),
        vec![(
            "now".to_owned(),
            aven_eval::Value::native(|_| Ok(aven_eval::Value::Int(1))),
        )],
        &roots,
    )
    .expect("evaluation should load graph");
    assert!(
        output
            .reports
            .iter()
            .any(aven_core::DiagnosticReport::has_errors)
    );

    write(
        dir.path(),
        "main.av",
        "{ now } = import(\"std/clock\")\nnow()\n",
    );
    let output =
        check_path_with_host_globals_and_roots(&dir.path().join("main.av"), &globals, &roots)
            .expect("check should load graph");
    assert_no_errors(&output.reports);
}

#[test]
fn unexported_binding_is_not_on_import_record_type() {
    let dir = TempDir::new("private-binding");
    fs::create_dir_all(dir.path().join("lib")).expect("failed to create lib dir");
    write(
        dir.path(),
        "lib/text.av",
        "_helper = (x: Text): Text => x\njoin = \"ok\"\n{ join }\n",
    );
    write(
        dir.path(),
        "main.av",
        "lib = import(\"./lib/text\")\nvalue : Text = lib._helper(\"x\")\n{ value }\n",
    );

    let output = check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
        .expect("check should load graph");

    assert_has_code(&output.reports, codes::ty::MISSING_FIELD);
}

#[test]
fn record_pattern_can_select_from_import_record() {
    let dir = TempDir::new("selective-import");
    write(
        dir.path(),
        "text.av",
        "join = (x: Text): Text => x\n{ join }\n",
    );
    write(
        dir.path(),
        "main.av",
        "text = import(\"./text\")\nvalue : Text = text ?>\n  { join } => join(\"a\")\n{ value }\n",
    );

    let output = check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
        .expect("check should load graph");

    assert_no_errors(&output.reports);
}

#[test]
fn record_pattern_binding_selects_from_import_record() {
    let dir = TempDir::new("pattern-binding-import");
    write(
        dir.path(),
        "text.av",
        "join = (x: Text): Text => x\n{ join }\n",
    );
    write(
        dir.path(),
        "main.av",
        "{ join } = import(\"./text\")\nvalue: Text = join(\"a\")\n{ value }\n",
    );

    let output = check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
        .expect("check should load graph");

    assert_no_errors(&output.reports);
}

#[test]
fn module_type_exports_work_in_qualified_and_pattern_annotations() {
    let dir = TempDir::new("type-exports");
    write(
        dir.path(),
        "util.av",
        "User = { name: Text, age: Int }\ngreet = (u: User): Text => u.name\n{ greet, User }\n",
    );
    write(
        dir.path(),
        "main.av",
        "util = import(\"./util\")\nf = (u: util.User): Text => u.name\n{ f }\n",
    );
    let qualified =
        check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
            .expect("qualified type import should check");
    assert_no_errors(&qualified.reports);

    write(
        dir.path(),
        "pattern.av",
        "{ User } = import(\"./util\")\nf = (u: User): Text => u.name\n{ f }\n",
    );
    let pattern =
        check_path_with_host_globals(&dir.path().join("pattern.av"), &HostGlobals::default())
            .expect("type pattern import should check");
    assert_no_errors(&pattern.reports);

    write(
        dir.path(),
        "rename.av",
        "{ User -> Person } = import(\"./util\")\nf = (u: Person): Text => u.name\n{ f }\n",
    );
    let rename =
        check_path_with_host_globals(&dir.path().join("rename.av"), &HostGlobals::default())
            .expect("renamed type pattern import should check");
    assert_no_errors(&rename.reports);
}

#[test]
fn imported_type_cannot_be_renamed_to_a_reserved_type() {
    let dir = TempDir::new("reserved-imported-type");
    write(dir.path(), "util.av", "User = { name: Text }\n{ User }\n");
    write(
        dir.path(),
        "main.av",
        "{ User -> Text } = import(\"./util\")\nvalue : Text = \"Ada\"\n{ value }\n",
    );

    let output = check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
        .expect("check should load graph");

    assert_has_code(&output.reports, codes::name::RESERVED_TYPE);
}

#[test]
fn comptime_computed_type_alias_exports_through_modules() {
    let dir = TempDir::new("comptime-alias-export");
    write(
        dir.path(),
        "util.av",
        "User = { name: Text, age: Int }\n\
         partial = (object) => { keysOf(object) -> k; [k]: ?object[k] }\n\
         Partial = partial(User)\n\
         { User, Partial }\n",
    );
    write(
        dir.path(),
        "main.av",
        "util = import(\"./util\")\np: util.Partial = { name: \"Ada\" }\n{ p }\n",
    );
    let checked =
        check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
            .expect("comptime alias import should check");
    assert_no_errors(&checked.reports);

    // The exported alias is a real type, not a deferred placeholder: a
    // mismatched field is caught through the import.
    write(
        dir.path(),
        "bad.av",
        "util = import(\"./util\")\np: util.Partial = { name: 5 }\n{ p }\n",
    );
    let bad = check_path_with_host_globals(&dir.path().join("bad.av"), &HostGlobals::default())
        .expect("check should load graph");
    assert_has_code(&bad.reports, codes::ty::MISMATCH);
}

#[test]
fn local_comptime_fn_applied_to_imported_type_reifies() {
    // A pattern-imported type export must be visible while local comptime
    // bindings evaluate: `Draft = partial(User)` reifies to a concrete record
    // instead of silently deferring (which made checks against it vacuous).
    let dir = TempDir::new("imported-type-comptime-arg");
    write(
        dir.path(),
        "models.av",
        "User = { name: Text, email: Text }\n{ User }\n",
    );
    write(
        dir.path(),
        "main.av",
        "{ User } = import(\"./models\")\n\
         partial = (object) => { keysOf(object) -> k; [k]: ?object[k] }\n\
         Draft = partial(User)\n\
         complete = (draft: Draft): User => { name: \"anon\", email: \"a@b.c\", ..draft }\n\
         user = complete({ name: \"Dave\" })\n\
         { user }\n",
    );
    let checked =
        check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
            .expect("imported comptime argument should check");
    assert_no_errors(&checked.reports);

    // The reified type really constrains call sites: an unknown field errors.
    write(
        dir.path(),
        "bad.av",
        "{ User } = import(\"./models\")\n\
         partial = (object) => { keysOf(object) -> k; [k]: ?object[k] }\n\
         Draft = partial(User)\n\
         complete = (draft: Draft): User => { name: \"anon\", email: \"a@b.c\", ..draft }\n\
         user = complete({ bogus: 1 })\n\
         { user }\n",
    );
    let bad = check_path_with_host_globals(&dir.path().join("bad.av"), &HostGlobals::default())
        .expect("check should load graph");
    assert_has_code(&bad.reports, codes::ty::UNEXPECTED_FIELD);
}

#[test]
fn module_type_export_diagnostics_are_structured() {
    let dir = TempDir::new("type-export-diagnostics");
    write(
        dir.path(),
        "util.av",
        "value = 1\n{ value: value, User: value }\n",
    );
    write(
        dir.path(),
        "main.av",
        "util = import(\"./util\")\n{ util }\n",
    );
    let output = check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
        .expect("check should load graph");
    assert_has_code(&output.reports, codes::module::UPPERCASE_EXPORT_NOT_TYPE);

    write(
        dir.path(),
        "util_ok.av",
        "User = { name: Text }\n{ User }\n",
    );
    write(
        dir.path(),
        "unknown.av",
        "util = import(\"./util_ok\")\nx: util.Missing = { name: \"a\" }\n{ x }\n",
    );
    let unknown =
        check_path_with_host_globals(&dir.path().join("unknown.av"), &HostGlobals::default())
            .expect("check should load graph");
    assert_has_code(&unknown.reports, codes::ty::UNKNOWN_MODULE_TYPE);

    write(
        dir.path(),
        "non_import.av",
        "rec = { Text: 1 }\n{ Text } = rec\n{ Text }\n",
    );
    let non_import =
        check_path_with_host_globals(&dir.path().join("non_import.av"), &HostGlobals::default())
            .expect("check should load graph");
    assert_has_code(
        &non_import.reports,
        codes::ty::UPPERCASE_PATTERN_BINDER_UNSUPPORTED,
    );
}

#[test]
fn module_exporting_types_evaluates() {
    let dir = TempDir::new("type-export-eval");
    write(
        dir.path(),
        "util.av",
        "User = { name: Text, age: Int }\ngreet = (u: User): Text => u.name\n{ greet, User }\n",
    );
    write(
        dir.path(),
        "main.av",
        "{ User } = import(\"./util\")\nu = { name: \"Ada\", age: 1 }\n{ u, User }\n",
    );
    let output = eval_path_with_globals(&dir.path().join("main.av"), Vec::new())
        .expect("eval should load graph");
    assert_no_errors(&output.reports);
    assert!(output.value.is_some());
}

#[test]
fn spread_binding_opens_import_record() {
    let dir = TempDir::new("spread-binding-import");
    write(
        dir.path(),
        "text.av",
        "join = (x: Text): Text => x\n{ join }\n",
    );
    write(
        dir.path(),
        "main.av",
        "..import(\"./text\")\nvalue: Text = join(\"a\")\n{ value }\n",
    );

    let check = check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
        .expect("check should load graph");
    assert_no_errors(&check.reports);

    let eval = eval_path_with_globals(&dir.path().join("main.av"), Vec::new())
        .expect("eval should load graph");
    assert_no_errors(&eval.reports);
}

#[test]
fn uppercase_import_binding_reports_structured_diagnostic() {
    // The recorded 2026-07-09 trigger: a binding shadowing a builtin type name
    // (`Text = import(...)`) broke record spread when the imported signatures
    // mentioned the shadowed type. Uppercase module bindings are now outlawed
    // outright — uppercase is reserved for types.
    let dir = TempDir::new("uppercase-module-binding");
    write(
        dir.path(),
        "text.av",
        "join = (x: Text): Text => x\n{ join }\n",
    );
    write(
        dir.path(),
        "main.av",
        "Text = import(\"./text\")\n{ ..Text, extra: 1 }\n",
    );

    let output = check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
        .expect("check should load graph");

    assert_has_code(&output.reports, codes::name::UPPERCASE_MODULE_BINDING);
}

#[test]
fn lowercase_binding_spreads_through_type_mentioning_signatures() {
    // The inference half of the same bug: a module-value binding must never
    // enter the type-definitions map, so spreading it stays intact even when
    // the imported signatures mention builtin type names.
    let dir = TempDir::new("lowercase-binding-spread");
    write(
        dir.path(),
        "text.av",
        "join = (x: Text): Text => x\n{ join }\n",
    );
    write(
        dir.path(),
        "mid.av",
        "text = import(\"./text\")\n{ ..text, extra: 1 }\n",
    );
    write(
        dir.path(),
        "main.av",
        "mid = import(\"./mid\")\nvalue : Text = mid.join(\"a\")\nextra : Int = mid.extra\n{ value, extra }\n",
    );

    let output = check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
        .expect("check should load graph");

    assert_no_errors(&output.reports);
}

#[test]
fn source_overlay_beats_disk_content() {
    let dir = TempDir::new("overlay-check");
    write(dir.path(), "dep.av", "value = \"disk\"\n{ value }\n");
    write(
        dir.path(),
        "main.av",
        "dep = import(\"./dep\")\nvalue : Int = dep.value\n{ value }\n",
    );
    let mut overlay = SourceOverlay::new();
    overlay.insert(
        fs::canonicalize(dir.path().join("dep.av")).expect("dep path should canonicalize"),
        "value = 1\n{ value }\n".to_owned(),
    );

    let output = check_path_with_host_globals_and_overlay(
        &dir.path().join("main.av"),
        &HostGlobals::default(),
        &overlay,
    )
    .expect("check should load graph");

    assert_no_errors(&output.reports);
}

#[test]
fn eval_caches_diamond_dependency_once() {
    let dir = TempDir::new("diamond-eval");
    write(dir.path(), "d.av", "value = tick(\"d\")\n{ value }\n");
    write(
        dir.path(),
        "b.av",
        "d = import(\"./d\")\n{ value: d.value }\n",
    );
    write(
        dir.path(),
        "c.av",
        "d = import(\"./d\")\n{ value: d.value }\n",
    );
    write(
        dir.path(),
        "main.av",
        "b = import(\"./b\")\nc = import(\"./c\")\n{ b: b.value, c: c.value }\n",
    );
    let calls = Rc::new(RefCell::new(Vec::new()));
    let tick_calls = Rc::clone(&calls);
    let globals = vec![(
        "tick".to_owned(),
        aven_eval::Value::native(move |args| {
            let [aven_eval::Value::Text(value)] = args else {
                return Err("tick expects one Text".to_owned());
            };
            tick_calls.borrow_mut().push(value.clone());
            Ok(aven_eval::Value::Text(value.clone()))
        }),
    )];

    let output = eval_path_with_globals(&dir.path().join("main.av"), globals)
        .expect("eval should load graph");

    assert_no_errors(&output.reports);
    assert_eq!(calls.borrow().as_slice(), ["d"]);
}

#[test]
fn importer_own_errors_surface_alongside_dependency_errors() {
    let dir = TempDir::new("importer-recovery");
    write(dir.path(), "dep.av", "bad : Int = \"text\"\nx = 1\n{ x }\n");
    write(
        dir.path(),
        "main.av",
        "dep = import(\"./dep\")\ny : Text = 5\n{ y }\n",
    );

    let output = check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
        .expect("check should load graph");

    assert_has_code(&output.reports, codes::module::IMPORT_HAS_ERRORS);
    assert_has_code(&output.reports, codes::ty::MISMATCH);
}

#[test]
fn reports_import_cycle() {
    let dir = TempDir::new("cycle");
    write(dir.path(), "a.av", "b = import(\"./b\")\n{ b }\n");
    write(dir.path(), "b.av", "a = import(\"./a\")\n{ a }\n");

    let output = check_path_with_host_globals(&dir.path().join("a.av"), &HostGlobals::default())
        .expect("check should load graph");

    assert_has_code(&output.reports, codes::module::IMPORT_CYCLE);
}

#[test]
fn reports_missing_file() {
    let dir = TempDir::new("missing");
    write(
        dir.path(),
        "main.av",
        "missing = import(\"./missing\")\n{ missing }\n",
    );

    let output = check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
        .expect("check should load graph");

    assert_has_code(&output.reports, codes::module::NOT_FOUND);
}

#[test]
fn reports_non_record_final_expression() {
    let dir = TempDir::new("not-importable");
    write(dir.path(), "value.av", "value = 1\nvalue\n");
    write(
        dir.path(),
        "main.av",
        "value = import(\"./value\")\n{ value }\n",
    );

    let output = check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
        .expect("check should load graph");

    assert_has_code(&output.reports, codes::module::NOT_IMPORTABLE);
}

#[test]
fn reports_non_static_specifier() {
    let dir = TempDir::new("dynamic");
    write(
        dir.path(),
        "main.av",
        "name = \"./value\"\nvalue = import(name)\n{ value }\n",
    );

    let output = check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
        .expect("check should load graph");

    assert_has_code(&output.reports, codes::module::DYNAMIC_IMPORT);
}

#[test]
fn reports_unsupported_root() {
    let dir = TempDir::new("unsupported-root");
    write(dir.path(), "main.av", "std = import(\"std\")\n{ std }\n");

    let output = check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
        .expect("check should load graph");

    assert_has_code(&output.reports, codes::module::UNSUPPORTED_ROOT);
}

#[test]
fn project_root_import_discovers_aven_toml_from_nested_entry() {
    let dir = TempDir::new("project-root-aven-toml");
    write(dir.path(), "Aven.toml", "");
    write(dir.path(), "lib/util.av", "value = 1\n{ value }\n");
    write(
        dir.path(),
        "src/main.av",
        "util = import(\"$/lib/util\")\n{ value: util.value }\n",
    );
    let entry = dir.path().join("src/main.av");

    let checked = check_path_with_host_globals(&entry, &HostGlobals::default())
        .expect("check should load project-root import via Aven.toml discovery");
    assert_no_errors(&checked.reports);

    let evaluated = eval_path_with_globals(&entry, Vec::new())
        .expect("eval should load project-root import via Aven.toml discovery");
    assert_no_errors(&evaluated.reports);
}

#[test]
fn project_root_falls_back_to_entry_directory_without_aven_toml() {
    let dir = TempDir::new("project-root-fallback");
    write(dir.path(), "util.av", "value = 1\n{ value }\n");
    write(
        dir.path(),
        "main.av",
        "util = import(\"$/util\")\n{ value: util.value }\n",
    );

    let output = check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
        .expect("check should treat the entry directory as the project root");
    assert_no_errors(&output.reports);
}

#[test]
fn home_and_filesystem_roots_use_their_explicit_host_paths() {
    let dir = TempDir::new("home-filesystem-roots");
    let home = dir.path().join("home");
    write(&home, "util.av", "value = 1\n{ value }\n");
    let filesystem_file = dir.path().join("filesystem.av");
    write(
        dir.path(),
        "main.av",
        &format!(
            "home = import(\"~/util\")\nfs = import(\"//{}\")\n{{ home: home.value, fs: fs.value }}\n",
            filesystem_file
                .to_str()
                .expect("temporary paths are valid UTF-8")
                .trim_start_matches('/')
        ),
    );
    write(dir.path(), "filesystem.av", "value = 2\n{ value }\n");
    let roots = ModuleRoots {
        home: Some(home),
        filesystem: true,
        ..ModuleRoots::none()
    };

    let output = check_path_with_host_globals_and_roots(
        &dir.path().join("main.av"),
        &HostGlobals::default(),
        &roots,
    )
    .expect("check should load home and filesystem imports");
    assert_no_errors(&output.reports);
}

#[test]
fn unavailable_root_is_structured_and_bare_names_remain_unsupported() {
    let dir = TempDir::new("unavailable-root");
    write(
        dir.path(),
        "main.av",
        "a = import(\"$/missing\")\nb = import(\"~/missing\")\nc = import(\"//missing\")\nd = import(\"std\")\n{ a, b, c, d }\n",
    );
    let output = check_path_with_host_globals_and_roots(
        &dir.path().join("main.av"),
        &HostGlobals::default(),
        &ModuleRoots::none(),
    )
    .expect("check should report import diagnostics");
    assert_has_code(&output.reports, codes::module::ROOT_UNAVAILABLE);
    assert_has_code(&output.reports, codes::module::UNSUPPORTED_ROOT);
    let root_unavailable_count = output
        .reports
        .iter()
        .flat_map(|report| &report.diagnostics)
        .filter(|diagnostic| diagnostic.code.as_deref() == Some(codes::module::ROOT_UNAVAILABLE))
        .count();
    assert_eq!(root_unavailable_count, 3);
}

fn test_library_roots() -> ModuleRoots {
    ModuleRoots::none().with_library(
        "mylib",
        [
            ("mylib".to_owned(), "{ version: \"1.0\" }\n"),
            (
                "mylib/util".to_owned(),
                "extra = import(\"./extra\")\njoin = (x: Text): Text => extra.decorate(x)\n{ join }\n",
            ),
            (
                "mylib/extra".to_owned(),
                "decorate = (x: Text): Text => \"<${x}>\"\n{ decorate }\n",
            ),
        ]
        .into(),
    )
}

#[test]
fn bare_library_import_resolves_and_checks() {
    let dir = TempDir::new("library-check");
    write(
        dir.path(),
        "main.av",
        "lib = import(\"mylib\")\nutil = import(\"mylib/util\")\nvalue : Text = util.join(lib.version)\n{ value }\n",
    );

    let output = check_path_with_host_globals_and_roots(
        &dir.path().join("main.av"),
        &HostGlobals::default(),
        &test_library_roots(),
    )
    .expect("check should load library imports");

    assert_no_errors(&output.reports);
}

#[test]
fn bare_library_import_evaluates() {
    let dir = TempDir::new("library-eval");
    write(
        dir.path(),
        "main.av",
        "util = import(\"mylib/util\")\n{ value: util.join(\"a\") }\n",
    );

    let output = aven_compiler::eval_path_with_globals_and_roots(
        &dir.path().join("main.av"),
        Vec::new(),
        &test_library_roots(),
    )
    .expect("eval should load library imports");

    assert_no_errors(&output.reports);
    assert!(output.value.is_some());
}

#[test]
fn unregistered_library_reports_unsupported_root() {
    let dir = TempDir::new("library-unregistered");
    write(dir.path(), "main.av", "x = import(\"other\")\n{ x }\n");

    let output = check_path_with_host_globals_and_roots(
        &dir.path().join("main.av"),
        &HostGlobals::default(),
        &test_library_roots(),
    )
    .expect("check should report import diagnostics");

    assert_has_code(&output.reports, codes::module::UNSUPPORTED_ROOT);
}

#[test]
fn missing_library_module_reports_not_found() {
    let dir = TempDir::new("library-missing-module");
    write(dir.path(), "main.av", "x = import(\"mylib/nope\")\n{ x }\n");

    let output = check_path_with_host_globals_and_roots(
        &dir.path().join("main.av"),
        &HostGlobals::default(),
        &test_library_roots(),
    )
    .expect("check should report import diagnostics");

    assert_has_code(&output.reports, codes::module::NOT_FOUND);
}

#[test]
fn root_prefixed_import_inside_library_is_root_unavailable() {
    let dir = TempDir::new("library-root-prefixed");
    write(dir.path(), "main.av", "x = import(\"esc\")\n{ x }\n");
    let roots = ModuleRoots {
        project: Some(dir.path().to_path_buf()),
        filesystem: true,
        ..ModuleRoots::none()
    }
    .with_library(
        "esc",
        [("esc".to_owned(), "y = import(\"$/y\")\n{ y }\n")].into(),
    );

    let output = check_path_with_host_globals_and_roots(
        &dir.path().join("main.av"),
        &HostGlobals::default(),
        &roots,
    )
    .expect("check should report import diagnostics");

    assert_has_code(&output.reports, codes::module::ROOT_UNAVAILABLE);
}

fn write_library_diamond(dir: &TempDir) {
    write(
        dir.path(),
        "b.av",
        "counter = import(\"count/counter\")\n{ value: counter.value }\n",
    );
    write(
        dir.path(),
        "c.av",
        "counter = import(\"count/counter\")\n{ value: counter.value }\n",
    );
    write(
        dir.path(),
        "main.av",
        "b = import(\"./b\")\nc = import(\"./c\")\n{ b: b.value, c: c.value }\n",
    );
}

fn counter_library(source: &'static str) -> ModuleRoots {
    ModuleRoots::none().with_library("count", [("count/counter".to_owned(), source)].into())
}

#[test]
fn library_diamond_shares_one_node() {
    let dir = TempDir::new("library-diamond-check");
    write_library_diamond(&dir);

    let checked = check_path_with_host_globals_and_roots(
        &dir.path().join("main.av"),
        &HostGlobals::default(),
        &counter_library("value = 1\n{ value }\n"),
    )
    .expect("check should load graph");

    assert_no_errors(&checked.reports);
    let library_nodes = checked
        .nodes
        .iter()
        .filter(|node| node.file.name == "count/counter")
        .count();
    assert_eq!(library_nodes, 1, "two importers must share one node");
}

#[test]
fn library_diamond_evaluates_once() {
    let dir = TempDir::new("library-diamond-eval");
    write_library_diamond(&dir);
    let calls = Rc::new(RefCell::new(Vec::new()));
    let tick_calls = Rc::clone(&calls);
    let globals = vec![(
        "tick".to_owned(),
        aven_eval::Value::native(move |args| {
            let [aven_eval::Value::Text(value)] = args else {
                return Err("tick expects one Text".to_owned());
            };
            tick_calls.borrow_mut().push(value.clone());
            Ok(aven_eval::Value::Text(value.clone()))
        }),
    )];

    let evaluated = aven_compiler::eval_path_with_globals_and_roots(
        &dir.path().join("main.av"),
        globals,
        &counter_library("value = tick(\"counter\")\n{ value }\n"),
    )
    .expect("eval should load graph");

    assert_no_errors(&evaluated.reports);
    assert_eq!(calls.borrow().as_slice(), ["counter"]);
}

#[test]
fn export_provenance_records_punned_renamed_and_explicit_fields() {
    let dir = TempDir::new("export-provenance");
    let source = "join = (x: Text): Text => x\nBase = { join }\n{ join, ..Base, join -> renamed, explicit: join }\n";
    write(dir.path(), "main.av", source);
    let path = fs::canonicalize(dir.path().join("main.av")).expect("main path should canonicalize");

    let output = check_path_with_host_globals(&path, &HostGlobals::default())
        .expect("check should load graph");

    assert_no_errors(&output.reports);
    let node = output
        .nodes
        .iter()
        .find(|node| node.canonical_path == path)
        .expect("expected main node");
    assert_eq!(
        node.export_provenance["join"].definition_span,
        nth_span(source, "join", 0)
    );
    assert_eq!(
        node.export_provenance["renamed"].definition_span,
        nth_span(source, "join", 0)
    );
    assert_eq!(
        node.export_provenance["explicit"].definition_span,
        nth_span(source, "explicit", 0)
    );
}

#[test]
fn export_provenance_chases_static_import_spreads_transitively() {
    let dir = TempDir::new("export-spread-provenance");
    let dep_source = "value = 1\n{ value }\n";
    write(dir.path(), "dep.av", dep_source);
    write(dir.path(), "mid.av", "dep = import(\"./dep\")\n{ ..dep }\n");
    let mid_path =
        fs::canonicalize(dir.path().join("mid.av")).expect("mid path should canonicalize");
    let dep_path =
        fs::canonicalize(dir.path().join("dep.av")).expect("dep path should canonicalize");

    let output = check_path_with_host_globals(&mid_path, &HostGlobals::default())
        .expect("check should load graph");

    assert_no_errors(&output.reports);
    let node = output
        .nodes
        .iter()
        .find(|node| node.canonical_path == mid_path)
        .expect("expected mid node");
    let provenance = &node.export_provenance["value"];
    assert_eq!(provenance.canonical_path, dep_path);
    assert_eq!(provenance.definition_span, nth_span(dep_source, "value", 0));
}

#[test]
fn library_nodes_carry_a_generated_interface_in_export_order() {
    let dir = TempDir::new("library-interface");
    write(dir.path(), "main.av", "lib = import(\"lib\")\n{ lib }\n");
    let main_path =
        fs::canonicalize(dir.path().join("main.av")).expect("main path should canonicalize");
    let roots = ModuleRoots::discover(&main_path).with_library(
        "lib",
        HashMap::from([(
            "lib".to_owned(),
            "Id = Int\nfirst = (x: Int): Int => x\n{ first, Id }",
        )]),
    );

    let output =
        check_path_with_host_globals_and_roots(&main_path, &HostGlobals::default(), &roots)
            .expect("check should load graph");

    assert_no_errors(&output.reports);
    let node = output
        .nodes
        .iter()
        .find(|node| node.canonical_path == Path::new("lib:/"))
        .expect("expected library node");
    let interface = node
        .interface
        .as_ref()
        .expect("expected library node interface");
    assert_eq!(
        interface.text,
        "# lib — generated interface (shape view); not the implementation.\n\
         \n\
         first : Int -> Int\n\
         Id = Int\n"
    );
    for name in ["first", "Id"] {
        let span = interface.export_spans[name];
        assert_eq!(&interface.text[span.start..span.end], name);
    }

    let main_node = output
        .nodes
        .iter()
        .find(|node| node.canonical_path == main_path)
        .expect("expected main node");
    assert_eq!(main_node.interface, None, "file-backed nodes stay bare");
}

#[test]
fn cross_module_comptime_type_function_checks_fields() {
    let dir = TempDir::new("comptime-type-fn-export");
    write(
        dir.path(),
        "shapes.av",
        "pair = (@t) => { first: t, second: t }\n{ pair }\n",
    );
    write(
        dir.path(),
        "main_ok.av",
        "{ pair } = import(\"./shapes\")\np: pair(Int) = { first: 1, second: 2 }\n{ p }\n",
    );
    let ok = check_path_with_host_globals(&dir.path().join("main_ok.av"), &HostGlobals::default())
        .expect("matching pair(Int) should check");
    assert_no_errors(&ok.reports);

    let ran = eval_path_with_globals(&dir.path().join("main_ok.av"), vec![])
        .expect("matching pair(Int) should evaluate");
    assert_no_errors(&ran.reports);
    assert_eq!(
        ran.value.as_ref().map(ToString::to_string),
        Some("{ p: { first: 1, second: 2 } }".to_owned())
    );

    write(
        dir.path(),
        "main_bad.av",
        "{ pair } = import(\"./shapes\")\np: pair(Int) = { first: 1, second: \"two\" }\n{ p }\n",
    );
    let bad =
        check_path_with_host_globals(&dir.path().join("main_bad.av"), &HostGlobals::default())
            .expect("check should load graph");
    assert_has_code(&bad.reports, codes::ty::MISMATCH);

    // Standalone comptime binding of an imported type function matches local.
    write(
        dir.path(),
        "main_alias.av",
        "{ pair } = import(\"./shapes\")\nPairInt = pair(Int)\np: PairInt = { first: 1, second: 2 }\nq: PairInt = { first: 1, second: \"two\" }\n{ p }\n",
    );
    let alias =
        check_path_with_host_globals(&dir.path().join("main_alias.av"), &HostGlobals::default())
            .expect("check should load graph");
    assert_has_code(&alias.reports, codes::ty::MISMATCH);
}

#[test]
fn cross_module_unexpandable_imported_application_diagnoses() {
    let dir = TempDir::new("comptime-unexpandable-import");
    write(dir.path(), "ops.av", "add = (a, b) => a + b\n{ add }\n");
    write(
        dir.path(),
        "main.av",
        "{ add } = import(\"./ops\")\np: add(Int, Text) = 1\n{ p }\n",
    );
    let output = check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
        .expect("check should load graph");
    assert_has_code(&output.reports, codes::comptime::UNEXPANDABLE_IMPORT);

    // Runtime use of the same import must not pick up the new diagnostic.
    write(
        dir.path(),
        "runtime.av",
        "{ add } = import(\"./ops\")\nx = add(1, 2)\n{ x }\n",
    );
    let runtime =
        check_path_with_host_globals(&dir.path().join("runtime.av"), &HostGlobals::default())
            .expect("runtime import use should check");
    assert_no_errors(&runtime.reports);
}

#[test]
fn cross_module_comptime_type_function_reexport() {
    let dir = TempDir::new("comptime-type-fn-reexport");
    write(
        dir.path(),
        "shapes.av",
        "pair = (@t) => { first: t, second: t }\n{ pair }\n",
    );
    write(
        dir.path(),
        "mid.av",
        "{ pair } = import(\"./shapes\")\n{ pair }\n",
    );
    write(
        dir.path(),
        "main.av",
        "{ pair } = import(\"./mid\")\np: pair(Int) = { first: 1, second: \"two\" }\n{ p }\n",
    );
    let bad = check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
        .expect("check should load graph");
    assert_has_code(&bad.reports, codes::ty::MISMATCH);

    write(
        dir.path(),
        "main_ok.av",
        "{ pair } = import(\"./mid\")\np: pair(Int) = { first: 1, second: 2 }\n{ p }\n",
    );
    let ok = check_path_with_host_globals(&dir.path().join("main_ok.av"), &HostGlobals::default())
        .expect("re-exported pair(Int) should check");
    assert_no_errors(&ok.reports);
}

fn assert_no_errors(reports: &[aven_core::DiagnosticReport]) {
    assert!(
        !reports.iter().any(aven_core::DiagnosticReport::has_errors),
        "expected no errors, got {reports:#?}"
    );
}

fn assert_has_code(reports: &[aven_core::DiagnosticReport], code: &str) {
    assert!(
        reports
            .iter()
            .flat_map(|report| &report.diagnostics)
            .any(|diagnostic| diagnostic.code.as_deref() == Some(code)),
        "expected diagnostic code {code}, got {reports:#?}"
    );
}

fn nth_span(source: &str, needle: &str, index: usize) -> aven_core::Span {
    let start = source
        .match_indices(needle)
        .nth(index)
        .map(|(start, _)| start)
        .expect("expected source to contain needle");
    aven_core::Span::new(start, start + needle.len())
}

fn write(root: &Path, relative: &str, source: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("failed to create parent directory");
    }
    fs::write(path, source).expect("failed to write source file");
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before the Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "aven-compiler-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("failed to create temp directory");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

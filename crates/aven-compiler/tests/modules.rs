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
    eval_path_with_host_globals_and_roots,
};
use aven_core::codes;
use aven_eval::Value;
use aven_host::Host;

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
fn recursive_type_exports_carry_one_level_heads_to_importers() {
    let dir = TempDir::new("recursive-type-export-heads");
    write(
        dir.path(),
        "types.av",
        "Node = { next: ?Node }\n\
         List = (t: Type) => @{ @Nil, @Cons((t, List(t))) }\n\
         { Node, List }\n",
    );
    write(
        dir.path(),
        "main.av",
        "{ Node, List } = import(\"./types\")\n\
         Keys = keysOf(Node)\n\
         Tags = tagsOf(List(Int))\n\
         node: Node = {}\n\
         xs: List(Int) = @Nil\n\
         { node, xs }\n",
    );

    let output = check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
        .expect("recursive type imports should check");
    assert_no_errors(&output.reports);

    let main = output
        .nodes
        .iter()
        .find(|node| node.canonical_path == dir.path().join("main.av"))
        .expect("main module node");
    assert!(matches!(
        main.semantic.type_definitions.get("Node"),
        Some(aven_compiler::Type::Recursive(_))
    ));
    assert_eq!(
        main.semantic
            .type_definitions
            .get("Keys")
            .map(aven_compiler::Type::render),
        Some("\"next\"".to_owned())
    );
    assert_eq!(
        main.semantic
            .type_definitions
            .get("Tags")
            .map(aven_compiler::Type::render),
        Some("\"Cons\" | \"Nil\"".to_owned())
    );
    assert!(main.semantic.recursive_type_unfoldings.len() >= 2);
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
            "Id = Int\nfirst = (x: Int): Int => x\nPair = (t: Type) => { first: t, second: t }\n{ first, Id, Pair }",
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
         Id = Int\n\
         Pair : Type -> Type\n"
    );
    for name in ["first", "Id", "Pair"] {
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
fn cross_module_uppercase_comptime_type_function_checks_fields() {
    let dir = TempDir::new("uppercase-comptime-type-fn-export");
    write(
        dir.path(),
        "shapes.av",
        "Pair = (t: Type) => { first: t, second: t }\n{ Pair }\n",
    );
    write(
        dir.path(),
        "main_ok.av",
        "{ Pair } = import(\"./shapes\")\np: Pair(Int) = { first: 1, second: 2 }\n{ p }\n",
    );
    let ok = check_path_with_host_globals(&dir.path().join("main_ok.av"), &HostGlobals::default())
        .expect("matching imported Pair(Int) should check");
    assert_no_errors(&ok.reports);

    write(
        dir.path(),
        "main_bad.av",
        "{ Pair } = import(\"./shapes\")\np: Pair(Int) = { first: 1, second: \"two\" }\n{ p }\n",
    );
    let bad =
        check_path_with_host_globals(&dir.path().join("main_bad.av"), &HostGlobals::default())
            .expect("mismatched imported Pair(Int) should report a type error");
    assert_has_code(&bad.reports, codes::ty::MISMATCH);
    assert!(
        !bad.reports
            .iter()
            .flat_map(|report| &report.diagnostics)
            .any(|diagnostic| {
                diagnostic.code.as_deref() == Some(codes::module::UPPERCASE_EXPORT_NOT_TYPE)
            }),
        "uppercase comptime function export must not be treated as a non-type: {:#?}",
        bad.reports
    );

    write(
        dir.path(),
        "mid.av",
        "{ Pair } = import(\"./shapes\")\n{ Pair }\n",
    );
    write(
        dir.path(),
        "main_reexport.av",
        "{ Pair } = import(\"./mid\")\np: Pair(Int) = { first: 1, second: \"two\" }\n{ p }\n",
    );
    let reexport = check_path_with_host_globals(
        &dir.path().join("main_reexport.av"),
        &HostGlobals::default(),
    )
    .expect("re-exported Pair(Int) should resolve");
    assert_has_code(&reexport.reports, codes::ty::MISMATCH);
}

#[test]
fn imported_comptime_type_application_annotation_preserves_fields() {
    let dir = TempDir::new("imported-comptime-type-annotation-fields");
    write(
        dir.path(),
        "models.av",
        "Labelled = (t: Type) => { label: Text, value: t }\n{ Labelled }\n",
    );
    write(
        dir.path(),
        "main.av",
        concat!(
            "{ Labelled } = import(\"./models\")\n",
            "Tagged = Labelled(Int)\n",
            "answer: Tagged = { label: \"answer\", value: 42 }\n",
            "note: Labelled(Text) = { label: \"note\", value: \"hello\" }\n",
            "answerLabel: Text = answer.label\n",
            "answerValue: Int = answer.value\n",
            "noteLabel: Text = note.label\n",
            "noteValue: Text = note.value\n",
            "{ answerLabel, answerValue, noteLabel, noteValue }\n",
        ),
    );

    let output = check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
        .expect("imported direct type application should check");

    assert_no_errors(&output.reports);
}

#[test]
fn recursive_variant_sum_checks_and_runs() {
    let dir = TempDir::new("recursive-variant-sum");
    write(
        dir.path(),
        "main.av",
        concat!(
            "List = (t: Type) => @{ @Nil, @Cons((t, List(t))) }\n",
            "sum : (List(Int)) -> Int\n",
            "sum = (xs) =>\n",
            "  xs ?>\n",
            "    @Nil => 0\n",
            "    @Cons((n, rest)) => n + sum(rest)\n",
            "sum(@Cons((1, @Cons((2, @Nil)))))\n",
        ),
    );
    let path = dir.path().join("main.av");

    let checked = check_path_with_host_globals(&path, &HostGlobals::default())
        .expect("recursive List sum should check");
    assert_no_errors(&checked.reports);

    let ran = eval_path_with_globals(&path, vec![]).expect("recursive List sum should evaluate");
    assert_no_errors(&ran.reports);
    assert_eq!(
        ran.value.as_ref().map(ToString::to_string),
        Some("3".to_owned())
    );
}

#[test]
fn cross_module_recursive_type_function_constructs_matches_and_runs() {
    let dir = TempDir::new("recursive-type-fn-export");
    write(
        dir.path(),
        "lists.av",
        "List = (t: Type) => @{ @Nil, @Cons((t, List(t))) }\n{ List }\n",
    );
    write(
        dir.path(),
        "main.av",
        concat!(
            "{ List } = import(\"./lists\")\n",
            "xs: List(Int) = @Cons((1, @Cons((2, @Nil))))\n",
            "len : (List(Int)) -> Int\n",
            "len = (xs) => xs ?> @Nil => 0, @Cons((_, rest)) => 1 + len(rest)\n",
            "len(xs)\n",
        ),
    );

    let checked =
        check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
            .expect("imported recursive List should check");
    assert_no_errors(&checked.reports);

    let ran = eval_path_with_globals(&dir.path().join("main.av"), vec![])
        .expect("imported recursive List should evaluate");
    assert_no_errors(&ran.reports);
    assert_eq!(
        ran.value.as_ref().map(ToString::to_string),
        Some("2".to_owned())
    );
}

#[test]
fn imported_comptime_type_function_captures_its_home_module_aliases() {
    let dir = TempDir::new("comptime-type-fn-home-scope");
    write(
        dir.path(),
        "shapes.av",
        "Id = { id: Int }\nWithId = (t: Type) => { ..t, ..Id }\n{ WithId }\n",
    );
    write(
        dir.path(),
        "main.av",
        "{ WithId } = import(\"./shapes\")\nvalue: WithId({ name: Text }) = { id: 1, name: \"Aven\" }\n{ value }\n",
    );

    let checked =
        check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
            .expect("captured imported alias should resolve in its home module scope");
    assert_no_errors(&checked.reports);
}

#[test]
fn imported_comptime_sibling_does_not_alias_importer_function_of_same_name() {
    let dir = TempDir::new("comptime-type-fn-name-collision");
    write(
        dir.path(),
        "lib.av",
        "G = (t: Type) => { b: t }\nF = (t: Type) => G(t)\n{ F, G }\n",
    );
    write(
        dir.path(),
        "main.av",
        "{ F } = import(\"./lib\")\nG = (t: Type) => { a: t }\nmine: G(Int) = { a: 1 }\ntheirs: F(Int) = { b: 2 }\n{ mine, theirs }\n",
    );

    let checked =
        check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
            .expect("same-named comptime functions must not share specializations");
    assert_no_errors(&checked.reports);
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
fn recursive_runtime_targets_decode_encode_and_preserve_shape_errors() {
    let dir = TempDir::new("recursive-runtime-targets");
    let source = r#"Tree = { value: Int, children: Array(Tree) }
treeInput = "{\"value\":1,\"children\":[{\"value\":2,\"children\":[{\"value\":3,\"children\":[]}]}]}"
tree = Json.decode(treeInput, Tree)?!
treeEncoded = Json.encode(tree)?!
treeAgain = Json.decode(treeEncoded, Tree)?!

Chain = (t: Type) => { value: t, next: ?Chain(t) }
IntChain = Chain(Int)
chainInput = "{\"value\":1,\"next\":{\"value\":2,\"next\":{\"value\":3}}}"
chain = Json.decode(chainInput, IntChain)?!
chainAgain = Json.decode(Json.encode(chain)?!, IntChain)?!

A = { value: Int, b: ?B }
B = { label: Text, a: ?A }
mutualInput = "{\"value\":1,\"b\":{\"label\":\"x\",\"a\":{\"value\":2}}}"
mutual = Json.decode(mutualInput, A)?!
mutualAgain = Json.decode(Json.encode(mutual)?!, A)?!

malformed = Json.decode("{\"value\":1,\"children\":[{\"value\":2,\"children\":[{\"value\":\"bad\",\"children\":[]}]}]}", Tree)

{
  treeDepth: tree.children[0].children[0].value,
  treeRoundTrip: tree == treeAgain,
  treeEncoded: treeEncoded,
  chainDepth: chain.next.next.value,
  chainRoundTrip: chain == chainAgain,
  mutualDepth: mutual.b.a.value,
  mutualRoundTrip: mutual == mutualAgain,
  malformed: malformed,
}
"#;
    write(dir.path(), "main.av", source);

    let mut host = Host::new();
    host.register_json();
    let output = eval_with_host(&dir.path().join("main.av"), &host);
    assert_no_errors(&output.reports);
    let value = output
        .value
        .expect("program returns recursive decode results");

    assert_eq!(value_field(&value, "treeDepth"), &Value::Int(3));
    assert_eq!(value_field(&value, "treeRoundTrip"), &Value::Bool(true));
    assert_eq!(
        value_field(&value, "treeEncoded"),
        &Value::Text(
            r#"{"value":1,"children":[{"value":2,"children":[{"value":3,"children":[]}]}]}"#
                .to_owned()
        )
    );
    assert_eq!(value_field(&value, "chainDepth"), &Value::Int(3));
    assert_eq!(value_field(&value, "chainRoundTrip"), &Value::Bool(true));
    assert_eq!(value_field(&value, "mutualDepth"), &Value::Int(2));
    assert_eq!(value_field(&value, "mutualRoundTrip"), &Value::Bool(true));
    assert!(
        value_field(&value, "malformed")
            .to_string()
            .contains("$.children[0].children[0].value"),
        "nested malformed data preserves its path: {value:?}"
    );
}

#[test]
fn recursive_runtime_target_handles_fifty_json_levels() {
    let dir = TempDir::new("recursive-runtime-depth");
    let mut input = r#"{"value":50,"children":[]}"#.to_owned();
    for value in (0..50).rev() {
        input = format!(r#"{{"value":{value},"children":[{input}]}}"#);
    }
    let source = format!(
        "Tree = {{ value: Int, children: Array(Tree) }}\n\
         input = {input:?}\n\
         tree = Json.decode(input, Tree)?!\n\
         Json.encode(tree)?! == input\n"
    );
    write(dir.path(), "main.av", &source);

    let mut host = Host::new();
    host.register_json();
    let output = eval_with_host(&dir.path().join("main.av"), &host);
    assert_no_errors(&output.reports);
    assert_eq!(output.value, Some(Value::Bool(true)));
}

#[test]
fn recursive_runtime_targets_share_yaml_and_toml_decode_paths() {
    let dir = TempDir::new("recursive-runtime-formats");
    let source = r#"Tree = { value: Int, children: Array(Tree) }
yamlInput = "value: 1\nchildren:\n  - value: 2\n    children: []\n"
yamlTree = Yaml.decode(yamlInput, Tree)?!
yamlAgain = Yaml.decode(Yaml.encode(yamlTree)?!, Tree)?!

tomlInput = "value = 1\nchildren = []\n"
tomlTree = Toml.decode(tomlInput, Tree)?!
tomlAgain = Toml.decode(Toml.encode(tomlTree)?!, Tree)?!

{
  yamlDepth: yamlTree.children[0].value,
  yamlRoundTrip: yamlTree == yamlAgain,
  tomlEmpty: tomlTree.children == [],
  tomlRoundTrip: tomlTree == tomlAgain,
}
"#;
    write(dir.path(), "main.av", source);

    let mut host = Host::new();
    host.register_yaml();
    host.register_toml();
    let output = eval_with_host(&dir.path().join("main.av"), &host);
    assert_no_errors(&output.reports);
    let value = output
        .value
        .expect("program returns format round-trip results");

    assert_eq!(value_field(&value, "yamlDepth"), &Value::Int(2));
    assert_eq!(value_field(&value, "yamlRoundTrip"), &Value::Bool(true));
    assert_eq!(value_field(&value, "tomlEmpty"), &Value::Bool(true));
    assert_eq!(value_field(&value, "tomlRoundTrip"), &Value::Bool(true));
}

#[test]
fn recursive_variant_decode_keeps_the_existing_unsupported_wire_boundary() {
    let dir = TempDir::new("recursive-runtime-variant");
    write(
        dir.path(),
        "main.av",
        "List = @{ @Nil, @Cons((Int, List)) }\nJson.decode(\"null\", List)\n",
    );

    let mut host = Host::new();
    host.register_json();
    let output = eval_with_host(&dir.path().join("main.av"), &host);
    let messages = output
        .reports
        .iter()
        .flat_map(|report| &report.diagnostics)
        .flat_map(|diagnostic| {
            std::iter::once(diagnostic.message.as_str())
                .chain(diagnostic.labels.iter().map(|label| label.message.as_str()))
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        messages.contains("cannot decode target type"),
        "recursive variants fail cleanly until a tag wire form is decided: {output:#?}"
    );
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

#[test]
fn std_array_ordered_constraints_cross_module_boundaries_and_run() {
    let dir = TempDir::new("std-array-ordered");
    write(
        dir.path(),
        "good.av",
        concat!(
            "ints = [{ key: 3 }, { key: 1 }, { key: 2 }].sortBy((item) => item.key)\n",
            "floats = [{ key: 2.5 }, { key: 1.5 }].sortBy((item) => item.key)\n",
            "smallest: ?Float = [3.5, 1.5, 2.5].minimum()\n",
            "largest: ?Float = [3.5, 1.5, 2.5].maximum()\n",
            "{ ints, floats, smallest, largest }\n",
        ),
    );
    let good_path = dir.path().join("good.av");
    let roots = ModuleRoots::discover(&good_path)
        .with_library(aven_host::STD_LIBRARY_NAME, aven_host::std_library())
        .with_trusted_ambient_modules(aven_host::STD_AMBIENT_METHOD_MODULES.iter().copied());
    let checked =
        check_path_with_host_globals_and_roots(&good_path, &HostGlobals::default(), &roots)
            .expect("ordered std calls should check");
    assert_no_errors(&checked.reports);

    let ran = eval_path_with_globals_and_roots(&good_path, vec![], &roots)
        .expect("ordered std calls should run");
    assert_no_errors(&ran.reports);
    assert_eq!(
        ran.value.as_ref().map(ToString::to_string),
        Some(
            "{ ints: [{ key: 1 }, { key: 2 }, { key: 3 }], floats: [{ key: 1.5 }, { key: 2.5 }], smallest: 1.5, largest: 3.5 }"
                .to_owned()
        )
    );
}

#[test]
fn builtin_and_named_values_reify_into_pure_method_slot_targets() {
    let dir = TempDir::new("builtin-method-slot-reification");
    write(
        dir.path(),
        "main.av",
        concat!(
            "IntList = {\n",
            "  items: Array(Int)\n",
            "  minimum(): ?Int => .items.minimum()\n",
            "}\n",
            "Ordered = {\n",
            "  <(Self): Bool\n",
            "  ..\n",
            "}\n",
            "forgetArray = (values: Array(t)): { minimum(): ?t }\n",
            "  t: Ordered\n",
            "=> values\n",
            "array = [3, 1]\n",
            "list = IntList({ items: [4, 2] })\n",
            "containers: Array({ minimum(): ?Int }) = [array, list]\n",
            "minimums: Array(?Int) = containers.map((container) => container.minimum())\n",
            "sourceMinimum = array.minimum\n",
            "slotMinimums = containers.map((container) => container.minimum)\n",
            "viaSourceAccess = sourceMinimum()\n",
            "viaSlotAccess = slotMinimums.map((minimum) => minimum())\n",
            "explicit = [8, 6].to({ minimum(): ?Int })\n",
            "viaExplicit = explicit.minimum()\n",
            "readMinimum = (value: { minimum(): ?Int }): ?Int => value.minimum()\n",
            "viaArgument = readMinimum([9, 7])\n",
            "produce = (): { minimum(): ?Int } => [12, 10]\n",
            "viaReturn = produce().minimum()\n",
            "holder: { value: { minimum(): ?Int } } = { value: [14, 13] }\n",
            "viaField = holder.value.minimum()\n",
            "viaKnownGeneric: ?Int = forgetArray([16, 15]).minimum()\n",
            "{ minimums, viaSourceAccess, viaSlotAccess, viaExplicit, viaArgument, viaReturn, viaField, viaKnownGeneric }\n",
        ),
    );
    let path = dir.path().join("main.av");
    let roots = ModuleRoots::discover(&path)
        .with_library(aven_host::STD_LIBRARY_NAME, aven_host::std_library())
        .with_trusted_ambient_modules(aven_host::STD_AMBIENT_METHOD_MODULES.iter().copied());

    let checked = check_path_with_host_globals_and_roots(
        &path,
        &aven_host::standard_check_host_globals(),
        &roots,
    )
    .expect("builtin and named slot sources should check");
    assert_no_errors(&checked.reports);

    let ran = eval_path_with_host_globals_and_roots(
        &path,
        &aven_host::standard_check_host_globals(),
        vec![],
        &roots,
    )
    .expect("builtin and named slot sources should run");
    assert_no_errors(&ran.reports);
    assert_eq!(
        ran.value.as_ref().map(ToString::to_string),
        Some(
            concat!(
                "{ minimums: [1, 2], viaSourceAccess: 1, viaSlotAccess: [1, 2], ",
                "viaExplicit: 6, viaArgument: 7, viaReturn: 10, viaField: 13, ",
                "viaKnownGeneric: 15 }",
            )
            .to_owned()
        )
    );
}

#[test]
fn direct_slot_initializer_constructs_and_calls_pure_behavior_target() {
    let dir = TempDir::new("direct-slot-pure");
    write(
        dir.path(),
        "main.av",
        concat!(
            "Csv = { csv(): Text }\n",
            "value: Csv = { csv(): Text => \"a,b\" }\n",
            "value.csv()\n",
        ),
    );
    let checked =
        check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
            .expect("pure-behavior slot initializer should check");
    assert_no_errors(&checked.reports);

    let ran = eval_path_with_globals(&dir.path().join("main.av"), vec![])
        .expect("pure-behavior slot initializer should run");
    assert_no_errors(&ran.reports);
    assert_eq!(
        ran.value.as_ref().map(ToString::to_string),
        Some("a,b".to_owned())
    );
}

#[test]
fn direct_slot_initializer_captures_lexical_environment() {
    let dir = TempDir::new("direct-slot-capture");
    write(
        dir.path(),
        "main.av",
        concat!(
            "Csv = { csv(): Text }\n",
            "wrap = (parts: Array(Text)): Csv =>\n",
            "  joined = parts.joinWith(\",\")\n",
            "  { csv(): Text => joined }\n",
            "wrap([\"a\", \"b\"]).csv()\n",
        ),
    );
    let checked =
        check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
            .expect("lexical-capture slot initializer should check");
    assert_no_errors(&checked.reports);

    let ran = eval_path_with_globals(&dir.path().join("main.av"), vec![])
        .expect("lexical-capture slot initializer should run");
    assert_no_errors(&ran.reports);
    assert_eq!(
        ran.value.as_ref().map(ToString::to_string),
        Some("a,b".to_owned())
    );
}

#[test]
fn direct_slot_initializer_omitted_return_annotation_runs_end_to_end() {
    let dir = TempDir::new("direct-slot-omit-ret");
    write(
        dir.path(),
        "main.av",
        concat!(
            "Csv = { csv(): Text }\n",
            "annotated: Csv = { csv() => \"a\" }\n",
            "annotated.csv()\n",
        ),
    );
    let checked =
        check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
            .expect("annotationless slot body should check");
    assert_no_errors(&checked.reports);

    let ran = eval_path_with_globals(&dir.path().join("main.av"), vec![])
        .expect("annotationless slot body should run");
    assert_no_errors(&ran.reports);
    assert_eq!(
        ran.value.as_ref().map(ToString::to_string),
        Some("a".to_owned())
    );
}

#[test]
fn slot_record_returning_call_reannotation_runs_end_to_end() {
    let dir = TempDir::new("slot-call-reannot");
    write(
        dir.path(),
        "main.av",
        concat!(
            "Csv = { csv(): Text }\n",
            "wrap = (text: Text): Csv =>\n",
            "  { csv(): Text => text }\n",
            "result: Csv = wrap(\"hi\")\n",
            "result.csv()\n",
        ),
    );
    let checked =
        check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
            .expect("slot-returning call re-annotation should check");
    assert_no_errors(&checked.reports);

    let ran = eval_path_with_globals(&dir.path().join("main.av"), vec![])
        .expect("slot-returning call re-annotation should run");
    assert_no_errors(&ran.reports);
    assert_eq!(
        ran.value.as_ref().map(ToString::to_string),
        Some("hi".to_owned())
    );
}

#[test]
fn reified_value_returning_call_reannotation_runs_end_to_end() {
    let dir = TempDir::new("reified-call-reannot");
    write(
        dir.path(),
        "main.av",
        concat!(
            "Tags = Array(Text) {\n",
            "  csv(): Text => .joinWith(\",\")\n",
            "}\n",
            "Csv = { csv(): Text }\n",
            "make = (t: Tags): Csv => t\n",
            "result: Csv = make(Tags([\"a\", \"b\"]))\n",
            "result.csv()\n",
        ),
    );
    let checked =
        check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
            .expect("reified-returning call re-annotation should check");
    assert_no_errors(&checked.reports);

    let ran = eval_path_with_globals(&dir.path().join("main.av"), vec![])
        .expect("reified-returning call re-annotation should run");
    assert_no_errors(&ran.reports);
    assert_eq!(
        ran.value.as_ref().map(ToString::to_string),
        Some("a,b".to_owned())
    );
}

#[test]
fn direct_slot_initializer_reads_data_field_through_receiver() {
    let dir = TempDir::new("direct-slot-data");
    write(
        dir.path(),
        "main.av",
        concat!(
            "Queue = { limit: Int, display(): Text }\n",
            "queue: Queue = { limit: 2, display(): Text => \"queue of ${.limit}\" }\n",
            "queue.display()\n",
        ),
    );
    let checked =
        check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
            .expect("data-bearing slot initializer should check");
    assert_no_errors(&checked.reports);

    let ran = eval_path_with_globals(&dir.path().join("main.av"), vec![])
        .expect("data-bearing slot initializer should run");
    assert_no_errors(&ran.reports);
    assert_eq!(
        ran.value.as_ref().map(ToString::to_string),
        Some("queue of 2".to_owned())
    );
}

#[test]
fn direct_slot_initializer_transform_then_erase_runs_end_to_end() {
    // The load-bearing case: a generic transform that filters a container
    // family and returns a freshly-constructed slot record capturing the
    // filtered value.
    let dir = TempDir::new("direct-slot-transform");
    write(
        dir.path(),
        "main.av",
        concat!(
            "Tags = Array(Text) {\n",
            "  csv(): Text => .joinWith(\",\")\n",
            "}\n",
            "Csv = { csv(): Text }\n",
            "nonEmptyCsv = (values: t): Csv\n",
            "  t: {\n",
            "    filter(pred: (Text) -> Bool): t\n",
            "    csv(): Text\n",
            "    ..\n",
            "  }\n",
            "=>\n",
            "  nonEmpty = values.filter((value) => value != \"\")\n",
            "  {\n",
            "    csv(): Text => nonEmpty.csv()\n",
            "  }\n",
            "nonEmptyCsv(Tags([\"a\", \"\", \"b\"])).csv()\n",
        ),
    );
    let roots = ModuleRoots::discover(&dir.path().join("main.av"))
        .with_library(aven_host::STD_LIBRARY_NAME, aven_host::std_library())
        .with_trusted_ambient_modules(aven_host::STD_AMBIENT_METHOD_MODULES.iter().copied());
    let checked = check_path_with_host_globals_and_roots(
        &dir.path().join("main.av"),
        &aven_host::standard_check_host_globals(),
        &roots,
    )
    .expect("transform-then-erase program should check");
    assert_no_errors(&checked.reports);

    let ran = eval_path_with_host_globals_and_roots(
        &dir.path().join("main.av"),
        &aven_host::standard_check_host_globals(),
        vec![],
        &roots,
    )
    .expect("transform-then-erase program should run");
    assert_no_errors(&ran.reports);
    assert_eq!(
        ran.value.as_ref().map(ToString::to_string),
        Some("a,b".to_owned())
    );
}

#[test]
fn direct_slot_initializer_matches_reified_value_for_same_target() {
    // A pure-behavior target reached via direct initialization and via builtin
    // reification answers the same call identically.
    let dir = TempDir::new("direct-slot-parity");
    let roots = ModuleRoots::discover(&dir.path().join("main.av"))
        .with_library(aven_host::STD_LIBRARY_NAME, aven_host::std_library())
        .with_trusted_ambient_modules(aven_host::STD_AMBIENT_METHOD_MODULES.iter().copied());
    write(
        dir.path(),
        "main.av",
        concat!(
            "Joined = Array(Text) {\n",
            "  csv(): Text => .joinWith(\",\")\n",
            "}\n",
            "Csv = { csv(): Text }\n",
            "viaDirect: Csv = { csv(): Text => \"a,b\" }\n",
            "viaReified: Csv = Joined([\"a\", \"b\"])\n",
            "{ direct: viaDirect.csv(), reified: viaReified.csv() }\n",
        ),
    );
    let checked = check_path_with_host_globals_and_roots(
        &dir.path().join("main.av"),
        &aven_host::standard_check_host_globals(),
        &roots,
    )
    .expect("both slot-fill paths should check");
    assert_no_errors(&checked.reports);

    let ran = eval_path_with_host_globals_and_roots(
        &dir.path().join("main.av"),
        &aven_host::standard_check_host_globals(),
        vec![],
        &roots,
    )
    .expect("both slot-fill paths should run");
    assert_no_errors(&ran.reports);
    assert_eq!(
        ran.value.as_ref().map(ToString::to_string),
        Some("{ direct: \"a,b\", reified: \"a,b\" }".to_owned())
    );
}

#[test]
fn builtin_slot_reification_reports_unsatisfied_provider_predicate_at_boundary() {
    let dir = TempDir::new("builtin-slot-provider-predicate");
    write(
        dir.path(),
        "main.av",
        concat!(
            "Unordered = {\n",
            "  value: Int\n",
            "  inspect(): Int => .value\n",
            "}\n",
            "items = [Unordered({ value: 1 })]\n",
            "forgotten: { minimum(): ?Unordered } = items\n",
            "{ forgotten }\n",
        ),
    );
    let path = dir.path().join("main.av");
    let roots = ModuleRoots::discover(&path)
        .with_library(aven_host::STD_LIBRARY_NAME, aven_host::std_library())
        .with_trusted_ambient_modules(aven_host::STD_AMBIENT_METHOD_MODULES.iter().copied());
    let checked = check_path_with_host_globals_and_roots(
        &path,
        &aven_host::standard_check_host_globals(),
        &roots,
    )
    .expect("provider predicate failure should be a checked diagnostic");
    let diagnostics = checked
        .reports
        .iter()
        .flat_map(|report| &report.diagnostics)
        .collect::<Vec<_>>();

    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.message.contains("method `<` is missing")
                && diagnostic.labels.iter().any(|label| {
                    label.span
                        == aven_core::Span::new(
                            concat!(
                                "Unordered = {\n",
                                "  value: Int\n",
                                "  inspect(): Int => .value\n",
                                "}\n",
                                "items = [Unordered({ value: 1 })]\n",
                                "forgotten: { minimum(): ?Unordered } = "
                            )
                            .len(),
                            concat!(
                                "Unordered = {\n",
                                "  value: Int\n",
                                "  inspect(): Int => .value\n",
                                "}\n",
                                "items = [Unordered({ value: 1 })]\n",
                                "forgotten: { minimum(): ?Unordered } = items"
                            )
                            .len(),
                        )
                })
        }),
        "{diagnostics:?}"
    );
}

#[test]
fn imported_constrained_projection_from_named_family_field_checks_and_runs() {
    let dir = TempDir::new("named-family-imported-projection");
    write(
        dir.path(),
        "lib/util2.av",
        concat!(
            "Ordered = {\n",
            "  <(Self): Bool\n",
            "  ..\n",
            "}\n",
            "\n",
            "keep : (Array(t), (t) -> k) -> Array(t)\n",
            "keep = (xs: Array(t), _key: (t) -> k): Array(t)\n",
            "  k: Ordered\n",
            "=>\n",
            "  xs\n",
            "\n",
            "{ keep }\n",
        ),
    );
    write(
        dir.path(),
        "main.av",
        concat!(
            "{ keep } = import(\"./lib/util2\")\n",
            "Rank = {\n",
            "  label: Text\n",
            "  score: Int\n",
            "\n",
            "  <(other: Rank): Bool =>\n",
            "    .score < other.score\n",
            "}\n",
            "ranks = [Rank({ label: \"a\", score: 2 })]\n",
            "byScore = keep(ranks, (r) => r.score)\n",
            "{ byScore }\n",
        ),
    );
    let path = dir.path().join("main.av");

    let checked = check_path_with_host_globals(&path, &HostGlobals::default())
        .expect("named-family field projection through an imported scheme should check");
    assert_no_errors(&checked.reports);

    let ran = eval_path_with_globals(&path, vec![])
        .expect("named-family field projection through an imported scheme should run");
    assert_no_errors(&ran.reports);
    assert_eq!(
        ran.value.as_ref().map(ToString::to_string),
        Some("{ byScore: [{ label: \"a\", score: 2 }] }".to_owned())
    );
}

#[test]
fn std_array_sort_by_rejects_unsupported_text_keys_at_importer_calls() {
    let dir = TempDir::new("std-array-unordered");
    let file = "field.av";
    write(
        dir.path(),
        file,
        "[{ key: \"b\" }, { key: \"a\" }].sortBy((item) => item.key)\n",
    );
    let path = dir.path().join(file);
    let roots = ModuleRoots::discover(&path)
        .with_library(aven_host::STD_LIBRARY_NAME, aven_host::std_library())
        .with_trusted_ambient_modules(aven_host::STD_AMBIENT_METHOD_MODULES.iter().copied());
    let checked = check_path_with_host_globals_and_roots(&path, &HostGlobals::default(), &roots)
        .expect("unsupported key should produce a checker diagnostic");
    let messages = checked
        .reports
        .iter()
        .flat_map(|report| &report.diagnostics)
        .map(|diagnostic| diagnostic.message.as_str())
        .collect::<Vec<_>>();
    assert!(
        messages.iter().any(|message| {
            message.contains("`Text` does not satisfy") && message.contains("method `<` is missing")
        }),
        "expected qualified Ordered failure in {file}, got {:#?}",
        checked.reports
    );
    assert_has_code(&checked.reports, codes::ty::INVALID_OPERATOR_OPERANDS);

    let ran = eval_path_with_globals_and_roots(&path, vec![], &roots)
        .expect("run should stop at the qualified checker diagnostic");
    assert_has_code(&ran.reports, codes::ty::INVALID_OPERATOR_OPERANDS);
    assert!(
        !ran.reports
            .iter()
            .flat_map(|report| &report.diagnostics)
            .any(|diagnostic| diagnostic.code.as_deref() == Some(codes::runtime::TYPE_ERROR)),
        "unsupported key must not reach runtime in {file}: {:#?}",
        ran.reports
    );
}

#[test]
fn named_family_operator_provider_satisfies_std_minimum_and_runs() {
    let dir = TempDir::new("named-family-rank-row");
    write(
        dir.path(),
        "main.av",
        concat!(
            "RankRow = {\n",
            "  value: Int\n",
            "\n",
            "  <(other: RankRow): Bool =>\n",
            "    .value < other.value\n",
            "}\n",
            "\n",
            "RowRank = RankRow\n",
            "\n",
            "lowest = [\n",
            "  RankRow({ value: 3 })\n",
            "  RowRank({ value: 1 })\n",
            "].minimum()\n",
            "\n",
            "{ lowest }\n",
        ),
    );
    let path = dir.path().join("main.av");
    let roots = ModuleRoots::discover(&path)
        .with_library(aven_host::STD_LIBRARY_NAME, aven_host::std_library())
        .with_trusted_ambient_modules(aven_host::STD_AMBIENT_METHOD_MODULES.iter().copied());
    let globals = aven_host::standard_check_host_globals();

    let checked = check_path_with_host_globals_and_roots(&path, &globals, &roots)
        .expect("named operator provider should check");
    assert_no_errors(&checked.reports);

    let ran = eval_path_with_host_globals_and_roots(&path, &globals, vec![], &roots)
        .expect("named operator provider should run");
    assert_no_errors(&ran.reports);
    assert_eq!(
        ran.value.as_ref().map(ToString::to_string),
        Some("{ lowest: { value: 1 } }".to_owned())
    );
}

#[test]
fn named_family_nullary_method_satisfies_requirement_and_runs() {
    let dir = TempDir::new("named-family-ticket");
    write(
        dir.path(),
        "main.av",
        concat!(
            "Prioritised = {\n",
            "  priority(): Int\n",
            "  ..\n",
            "}\n",
            "priorityOf = (item: t): Int\n",
            "  t: Prioritised\n",
            "=>\n",
            "  item.priority()\n",
            "Ticket = {\n",
            "  severity: Int\n",
            "  priority(): Int =>\n",
            "    .severity\n",
            "}\n",
            "ticket = Ticket({ severity: 7 })\n",
            "result = priorityOf(ticket)\n",
            "{ result }\n",
        ),
    );
    let path = dir.path().join("main.av");

    let checked = check_path_with_host_globals(&path, &HostGlobals::default())
        .expect("named nullary method provider should check");
    assert_no_errors(&checked.reports);

    let ran =
        eval_path_with_globals(&path, vec![]).expect("named nullary method provider should run");
    assert_no_errors(&ran.reports);
    assert_eq!(
        ran.value.as_ref().map(ToString::to_string),
        Some("{ result: 7 }".to_owned())
    );
}

#[test]
fn named_family_multiple_indented_methods_check_and_run() {
    let dir = TempDir::new("named-family-multi-method");
    write(
        dir.path(),
        "main.av",
        concat!(
            "Ticket = {\n",
            "  severity: Int\n",
            "\n",
            "  priority(): Int =>\n",
            "    .severity * 10\n",
            "\n",
            "  double(): Int =>\n",
            "    .severity * 2\n",
            "}\n",
            "\n",
            "t = Ticket({ severity: 3 })\n",
            "\"${t.priority()} ${t.double()}\"\n",
        ),
    );
    let path = dir.path().join("main.av");

    let checked = check_path_with_host_globals(&path, &HostGlobals::default())
        .expect("multi-method named family should check");
    assert_no_errors(&checked.reports);

    let ran = eval_path_with_globals(&path, vec![]).expect("multi-method named family should run");
    assert_no_errors(&ran.reports);
    assert_eq!(
        ran.value.as_ref().map(ToString::to_string),
        Some("30 6".to_owned())
    );
}

#[test]
fn named_primitive_family_money_checks_and_runs_end_to_end() {
    let dir = TempDir::new("named-primitive-family-money");
    write(
        dir.path(),
        "main.av",
        concat!(
            "Money = Int {\n",
            "  toText(): Text =>\n",
            "    \"$${. / 100}.${. % 100}\"\n",
            "}\n",
            "price: Money = 2599\n",
            "singleton = 3\n",
            "fromSingleton: Money = singleton\n",
            "singletonLabel = fromSingleton.toText()\n",
            "tax = Money(150)\n",
            "total = price + tax\n",
            "label = total.toText()\n",
            "rendered = \"${total}\"\n",
            "debug = debugText(total)\n",
            "bound = total.toText\n",
            "boundLabel = bound()\n",
            "unbound = Money.toText\n",
            "unboundLabel = unbound(total)\n",
            "shown: { toText(): Text } = total\n",
            "shownLabel = shown.toText()\n",
            "cheap = [price, tax].minimum()\n",
            "mixed: Int = 1 + total\n",
            "brandedPlus: Money = total + 1\n",
            "brandedPlusLabel = brandedPlus.toText()\n",
            "squared: Int = Money(3) ^ Money(2)\n",
            "asInt: Int = total\n",
            "{ label, rendered, debug, singletonLabel, boundLabel, unboundLabel, shownLabel, cheap, mixed, brandedPlusLabel, squared, asInt }\n",
        ),
    );
    let path = dir.path().join("main.av");
    let roots = ModuleRoots::discover(&path)
        .with_library(aven_host::STD_LIBRARY_NAME, aven_host::std_library())
        .with_trusted_ambient_modules(aven_host::STD_AMBIENT_METHOD_MODULES.iter().copied());
    let globals = aven_host::standard_check_host_globals();

    let checked = check_path_with_host_globals_and_roots(&path, &globals, &roots)
        .expect("Money family should check");
    assert_no_errors(&checked.reports);

    let ran = eval_path_with_host_globals_and_roots(&path, &globals, vec![], &roots)
        .expect("Money family should run");
    assert_no_errors(&ran.reports);
    assert_eq!(
        ran.value.as_ref().map(ToString::to_string),
        Some(
            "{ label: \"$27.49\", rendered: \"$27.49\", debug: \"Money(2749)\", singletonLabel: \"$0.3\", boundLabel: \"$27.49\", unboundLabel: \"$27.49\", shownLabel: \"$27.49\", cheap: 150, mixed: 2750, brandedPlusLabel: \"$27.50\", squared: 9, asInt: 2749 }"
                .to_owned()
        )
    );
}

#[test]
fn named_primitive_family_compatible_override_replaces_inherited_body() {
    let dir = TempDir::new("named-primitive-family-override");
    write(
        dir.path(),
        "main.av",
        concat!(
            "Money = Int {\n",
            "  +(other: Money): Money => Money(99)\n",
            "}\n",
            "left = Money(1)\n",
            "right = Money(2)\n",
            "result: Int = left + right\n",
            "{ result }\n",
        ),
    );
    let path = dir.path().join("main.av");

    let checked = check_path_with_host_globals(&path, &HostGlobals::default())
        .expect("compatible primitive-family override should check");
    assert_no_errors(&checked.reports);

    let ran = eval_path_with_globals(&path, vec![])
        .expect("compatible primitive-family override should replace the inherited body");
    assert_no_errors(&ran.reports);
    assert_eq!(
        ran.value.as_ref().map(ToString::to_string),
        Some("{ result: 99 }".to_owned())
    );
}

#[test]
fn named_primitive_family_override_delegates_through_int_plus() {
    let dir = TempDir::new("named-primitive-family-delegate");
    write(
        dir.path(),
        "main.av",
        concat!(
            "Money = Int {\n",
            "  +(other: Money): Money =>\n",
            "    Money(Int.+(., other))\n",
            "}\n",
            "result = Money(2599) + Money(401)\n",
            "asInt: Int = result\n",
            "unbound = Int.+(3, 4)\n",
            "{ asInt, unbound }\n",
        ),
    );
    let path = dir.path().join("main.av");

    let checked = check_path_with_host_globals(&path, &HostGlobals::default())
        .expect("base-operator delegation should check");
    assert_no_errors(&checked.reports);

    let ran = eval_path_with_globals(&path, vec![])
        .expect("base-operator delegation should sum payloads");
    assert_no_errors(&ran.reports);
    assert_eq!(
        ran.value.as_ref().map(ToString::to_string),
        Some("{ asInt: 3000, unbound: 7 }".to_owned())
    );
}

#[test]
fn named_primitive_families_run_over_all_concrete_scalar_bases() {
    let dir = TempDir::new("named-primitive-family-scalar-bases");
    write(
        dir.path(),
        "main.av",
        concat!(
            "Ratio = Float {}\n",
            "Label = Text {}\n",
            "Flag = Bool {}\n",
            "ratio: Ratio = 1.5\n",
            "doubled: Float = ratio + ratio\n",
            "label: Label = \"mixed\"\n",
            "upper: Label = label.toUpper()\n",
            "upperText: Text = upper\n",
            "flag: Flag = true\n",
            "plainFlag: Bool = flag\n",
            "{ doubled, upperText, plainFlag }\n",
        ),
    );
    let path = dir.path().join("main.av");

    let checked = check_path_with_host_globals(&path, &HostGlobals::default())
        .expect("all concrete scalar primitive families should check");
    assert_no_errors(&checked.reports);

    let ran = eval_path_with_globals(&path, vec![])
        .expect("all concrete scalar primitive families should run");
    assert_no_errors(&ran.reports);
    assert_eq!(
        ran.value.as_ref().map(ToString::to_string),
        Some("{ doubled: 3.0, upperText: \"MIXED\", plainFlag: true }".to_owned())
    );
}

#[test]
fn named_primitive_family_container_base_checks_and_runs_end_to_end() {
    let dir = TempDir::new("named-primitive-family-tags");
    write(
        dir.path(),
        "main.av",
        concat!(
            "Tags = Array(Text) {\n",
            "  normalized(): Tags =>\n",
            "    Tags(.map((t) => t.trim()).filter((t) => t != \"\"))\n",
            "}\n",
            "tags = Tags([\" go \", \"\", \"rust\"])\n",
            "clean = tags.normalized()\n",
            "joined = clean.joinWith(\",\")\n",
            "kept: Tags = tags.filter((t) => t != \"\")\n",
            "keptJoined = kept.joinWith(\",\")\n",
            "widened: Array(Text) = clean\n",
            "count = clean.length()\n",
            "{ joined, keptJoined, count }\n",
        ),
    );
    let path = dir.path().join("main.av");
    let roots = ModuleRoots::discover(&path)
        .with_library(aven_host::STD_LIBRARY_NAME, aven_host::std_library())
        .with_trusted_ambient_modules(aven_host::STD_AMBIENT_METHOD_MODULES.iter().copied());
    let globals = aven_host::standard_check_host_globals();

    let checked = check_path_with_host_globals_and_roots(&path, &globals, &roots)
        .expect("Tags family should check");
    assert_no_errors(&checked.reports);

    let ran = eval_path_with_host_globals_and_roots(&path, &globals, vec![], &roots)
        .expect("Tags family should run");
    assert_no_errors(&ran.reports);
    assert_eq!(
        ran.value.as_ref().map(ToString::to_string),
        Some("{ joined: \"go,rust\", keptJoined: \" go ,rust\", count: 2 }".to_owned())
    );
}

#[test]
fn named_family_descriptor_crosses_module_boundary() {
    let dir = TempDir::new("named-family-import");
    write(
        dir.path(),
        "rank.av",
        concat!(
            "RankRow = {\n",
            "  value: Int\n",
            "\n",
            "  <(other: RankRow): Bool =>\n",
            "    .value < other.value\n",
            "}\n",
            "{ RankRow }\n",
        ),
    );
    write(
        dir.path(),
        "main.av",
        concat!(
            "{ RankRow } = import(\"./rank\")\n",
            "lowest = [\n",
            "  RankRow({ value: 4 })\n",
            "  RankRow({ value: 2 })\n",
            "].minimum()\n",
            "{ lowest }\n",
        ),
    );
    let path = dir.path().join("main.av");
    let roots = ModuleRoots::discover(&path)
        .with_library(aven_host::STD_LIBRARY_NAME, aven_host::std_library())
        .with_trusted_ambient_modules(aven_host::STD_AMBIENT_METHOD_MODULES.iter().copied());
    let globals = aven_host::standard_check_host_globals();

    let checked = check_path_with_host_globals_and_roots(&path, &globals, &roots)
        .expect("imported named-family descriptor should check");
    assert_no_errors(&checked.reports);

    let ran = eval_path_with_host_globals_and_roots(&path, &globals, vec![], &roots)
        .expect("imported named-family descriptor should run");
    assert_no_errors(&ran.reports);
    assert_eq!(
        ran.value.as_ref().map(ToString::to_string),
        Some("{ lowest: { value: 2 } }".to_owned())
    );
}

#[test]
fn named_primitive_family_descriptor_and_literal_brand_cross_module_boundary() {
    let dir = TempDir::new("named-primitive-family-import");
    write(
        dir.path(),
        "money.av",
        concat!(
            "Money = Int {\n",
            "  toText(): Text => \"$${. / 100}.${. % 100}\"\n",
            "}\n",
            "{ Money }\n",
        ),
    );
    write(
        dir.path(),
        "main.av",
        concat!(
            "{ Money } = import(\"./money\")\n",
            "price: Money = 2599\n",
            "{ label: price.toText() }\n",
        ),
    );
    let path = dir.path().join("main.av");

    let checked = check_path_with_host_globals(&path, &HostGlobals::default())
        .expect("imported primitive family should check");
    assert_no_errors(&checked.reports);

    let ran = eval_path_with_globals(&path, vec![])
        .expect("imported primitive family should retain its descriptor");
    assert_no_errors(&ran.reports);
    assert_eq!(
        ran.value.as_ref().map(ToString::to_string),
        Some("{ label: \"$25.99\" }".to_owned())
    );
}

#[test]
fn ambient_sort_by_is_receiver_first_for_named_rank_and_bound_values_run() {
    let dir = TempDir::new("builtin-method-rank");
    write(
        dir.path(),
        "main.av",
        concat!(
            "Rank = {\n",
            "  label: Text\n",
            "  score: Int\n",
            "\n",
            "  <(other: Rank): Bool => .score < other.score\n",
            "}\n",
            "ranks = [\n",
            "  Rank({ label: \"silver\", score: 2 })\n",
            "  Rank({ label: \"gold\", score: 1 })\n",
            "]\n",
            "sortRanks = ranks.sortBy\n",
            "sorted = sortRanks((rank) => rank.score)\n",
            "labels = sorted.map((rank) => rank.label)\n",
            "getLength = labels.length\n",
            "{ labels, length: getLength() }\n",
        ),
    );
    let path = dir.path().join("main.av");
    let roots = ModuleRoots::discover(&path)
        .with_library(aven_host::STD_LIBRARY_NAME, aven_host::std_library())
        .with_trusted_ambient_modules(aven_host::STD_AMBIENT_METHOD_MODULES.iter().copied());

    let checked = check_path_with_host_globals_and_roots(&path, &HostGlobals::default(), &roots)
        .expect("receiver-first ambient methods should check");
    assert_no_errors(&checked.reports);

    let ran = eval_path_with_globals_and_roots(&path, vec![], &roots)
        .expect("bound ambient methods should run");
    assert_no_errors(&ran.reports);
    assert_eq!(
        ran.value.as_ref().map(ToString::to_string),
        Some("{ labels: [\"gold\", \"silver\"], length: 2 }".to_owned())
    );
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

fn eval_with_host(path: &Path, host: &Host) -> aven_compiler::ModuleEvalOutput {
    eval_path_with_host_globals_and_roots(
        path,
        &host.check_host_globals(),
        host.eval_globals(),
        &ModuleRoots::discover(path),
    )
    .expect("evaluation should load the module")
}

fn value_field<'a>(value: &'a Value, name: &str) -> &'a Value {
    let Value::Record(fields) = value else {
        panic!("expected a record, got {value:?}");
    };
    fields
        .iter()
        .find_map(|(field_name, value)| (field_name == name).then_some(value))
        .unwrap_or_else(|| panic!("record has field `{name}`"))
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

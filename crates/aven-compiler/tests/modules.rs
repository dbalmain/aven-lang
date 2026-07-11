use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use aven_compiler::{
    HostGlobals, ModuleRoots, SourceOverlay, check_path_with_host_globals,
    check_path_with_host_globals_and_overlay, check_path_with_host_globals_and_roots,
    eval_path_with_globals,
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
    write(dir.path(), "b.av", "D = import(\"./d\")\n{ b: D.value }\n");
    write(
        dir.path(),
        "c.av",
        "D = import(\"./d.av\")\n{ c: D.value }\n",
    );
    write(
        dir.path(),
        "main.av",
        "B = import(\"./b\")\nC = import(\"./c\")\n{ b: B.b, c: C.c }\n",
    );

    let output = check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
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
        "Lib = import(\"./lib/text\")\nvalue : Text = Lib._helper(\"x\")\n{ value }\n",
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
        "Text = import(\"./text\")\nvalue : Text = Text ?>\n  { join } => join(\"a\")\n{ value }\n",
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
fn source_overlay_beats_disk_content() {
    let dir = TempDir::new("overlay-check");
    write(dir.path(), "dep.av", "value = \"disk\"\n{ value }\n");
    write(
        dir.path(),
        "main.av",
        "D = import(\"./dep\")\nvalue : Int = D.value\n{ value }\n",
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
        "D = import(\"./d\")\n{ value: D.value }\n",
    );
    write(
        dir.path(),
        "c.av",
        "D = import(\"./d\")\n{ value: D.value }\n",
    );
    write(
        dir.path(),
        "main.av",
        "B = import(\"./b\")\nC = import(\"./c\")\n{ b: B.value, c: C.value }\n",
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
        "D = import(\"./dep\")\ny : Text = 5\n{ y }\n",
    );

    let output = check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
        .expect("check should load graph");

    assert_has_code(&output.reports, codes::module::IMPORT_HAS_ERRORS);
    assert_has_code(&output.reports, codes::ty::MISMATCH);
}

#[test]
fn reports_import_cycle() {
    let dir = TempDir::new("cycle");
    write(dir.path(), "a.av", "B = import(\"./b\")\n{ B }\n");
    write(dir.path(), "b.av", "A = import(\"./a\")\n{ A }\n");

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
        "Missing = import(\"./missing\")\n{ Missing }\n",
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
        "Value = import(\"./value\")\n{ Value }\n",
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
        "name = \"./value\"\nValue = import(name)\n{ Value }\n",
    );

    let output = check_path_with_host_globals(&dir.path().join("main.av"), &HostGlobals::default())
        .expect("check should load graph");

    assert_has_code(&output.reports, codes::module::DYNAMIC_IMPORT);
}

#[test]
fn reports_unsupported_root() {
    let dir = TempDir::new("unsupported-root");
    write(dir.path(), "main.av", "Std = import(\"std\")\n{ Std }\n");

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
        "Util = import(\"$/lib/util\")\n{ value: Util.value }\n",
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
        "Util = import(\"$/util\")\n{ value: Util.value }\n",
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
            "Home = import(\"~/util\")\nFs = import(\"//{}\")\n{{ home: Home.value, fs: Fs.value }}\n",
            filesystem_file
                .to_str()
                .expect("temporary paths are valid UTF-8")
                .trim_start_matches('/')
        ),
    );
    write(dir.path(), "filesystem.av", "value = 2\n{ value }\n");
    let roots = ModuleRoots {
        project: None,
        home: Some(home),
        filesystem: true,
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
        "A = import(\"$/missing\")\nB = import(\"~/missing\")\nC = import(\"//missing\")\nD = import(\"std\")\n{ A, B, C, D }\n",
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
    write(dir.path(), "mid.av", "Dep = import(\"./dep\")\n{ ..Dep }\n");
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

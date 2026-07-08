use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use aven_compiler::{HostGlobals, check_path_with_host_globals, eval_path_with_globals};
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

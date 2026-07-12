//! Reusable platform IO: bare process streams, handles, and files.
//!
//! This module owns the runtime side of platform IO so any host wires it with
//! one call each ([`Host::register_bare_io`] / [`Host::register_std_streams`] /
//! [`Host::register_files`]) instead of rebuilding the natives inline. The
//! matching types live in the crate root ([`crate::io_write_type`],
//! [`crate::stdout_handle_type`], [`crate::file_type`], …) so the value and
//! type halves can't drift.
//!
//! **Bare tier** (`write` / `writeLine` / `readLine` / `readAll`): abort on a
//! real IO error (runtime diagnostic). Writes return the empty record;
//! `readLine` returns `?Text` with `undefined` on EOF.
//!
//! **Handle tier** (`stdout` / `stderr` / `stdin` / `stdio` / `File`): methods
//! return `Result` instead of aborting. Both tiers share the same write
//! primitive ([`write_text`]) and the same flush-before-read behavior.
//!
//! Files use drop-RAII auto-close: a handle is a closed record of method
//! closures sharing one `Rc<RefCell<FileState>>`, and the OS file stays open
//! until the last method closure drops. There is deliberately no `close` method
//! (it would reintroduce the use-after-close error class); a buffered writer is
//! flushed on `Drop`.

use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::rc::Rc;

use aven_check::{ComptimeArg, ComptimeError, HostComptimeFn, Type};
use aven_eval::Value;

use crate::Host;

impl Host {
    /// Register the bare process-stream IO natives `write`/`writeLine`/
    /// `readLine`/`readAll` (value + the crate's bare IO types). Writes return
    /// the empty record; real IO errors abort as a runtime diagnostic.
    /// `readLine` returns `undefined` on EOF.
    pub fn register_bare_io(&mut self) {
        self.register("write", bare_write_native(false), crate::io_write_type());
        self.register(
            "writeLine",
            bare_write_native(true),
            crate::io_write_line_type(),
        );
        self.register(
            "readLine",
            bare_read_line_native(),
            crate::io_read_line_type(),
        );
        self.register("readAll", bare_read_all_native(), crate::io_read_all_type());
    }

    /// Register the process-stream handles `stdout`/`stderr`/`stdin`/`stdio`
    /// (value + the crate's handle types). Their methods return `Result` rather
    /// than aborting on a real IO error.
    pub fn register_std_streams(&mut self) {
        self.register(
            "stdout",
            write_handle_value(WriteStream::Stdout),
            crate::stdout_handle_type(),
        );
        self.register(
            "stderr",
            write_handle_value(WriteStream::Stderr),
            crate::stderr_handle_type(),
        );
        self.register("stdin", stdin_handle_value(), crate::stdin_handle_type());
        self.register("stdio", stdio_handle_value(), crate::stdio_handle_type());
    }

    /// Register file IO: the single `File.open(path, mode)` where mode is a Text
    /// literal (`"r"`, `"w"`, `"a"`, or `"rw"`) at check time.
    pub fn register_files(&mut self) {
        self.register("File", file_value(), crate::file_type());
        self.register_comptime_resolver("File.open", vec![1], open_comptime_resolver());
    }
}

// --- bare process-stream natives ------------------------------------------

fn bare_write_native(newline: bool) -> Value {
    Value::native(move |args| {
        let name = if newline { "writeLine" } else { "write" };
        let text = io_text_arg(name, args)?;
        write_text(&mut io::stdout().lock(), text, newline).map_err(|error| error.to_string())?;
        Ok(empty_record_value())
    })
}

fn bare_read_line_native() -> Value {
    Value::native(|args| {
        if !args.is_empty() {
            return Err(format!("readLine expects 0 arguments, got {}", args.len()));
        }

        flush_stdout_before_read();

        let mut line = String::new();
        let bytes = io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|error| error.to_string())?;
        if bytes == 0 {
            return Ok(Value::Undefined);
        }
        strip_trailing_newline(&mut line);

        Ok(Value::Text(line))
    })
}

fn bare_read_all_native() -> Value {
    Value::native(|args| {
        if !args.is_empty() {
            return Err(format!("readAll expects 0 arguments, got {}", args.len()));
        }

        flush_stdout_before_read();

        let mut text = String::new();
        io::stdin()
            .lock()
            .read_to_string(&mut text)
            .map_err(|error| error.to_string())?;
        Ok(Value::Text(text))
    })
}

// --- shared Result/error construction -------------------------------------

/// Build the `@Ok(value)` Result tag a handle method returns on success.
pub(crate) fn ok_value(value: Value) -> Value {
    Value::Tag {
        name: "Ok".to_owned(),
        payload: vec![value],
    }
}

/// Build the `@Err(error)` Result tag a handle method returns on an IO error,
/// where `error` is the closed error-variant tag (e.g. `@BrokenPipe("...")`).
pub(crate) fn err_value(error: Value) -> Value {
    Value::Tag {
        name: "Err".to_owned(),
        payload: vec![error],
    }
}

/// Map an `io::Error` to a `WriteError` variant value carrying its message.
fn write_error_value(error: &io::Error) -> Value {
    let tag = match error.kind() {
        io::ErrorKind::BrokenPipe => "BrokenPipe",
        io::ErrorKind::PermissionDenied => "PermissionDenied",
        _ => "Other",
    };
    error_variant(tag, error)
}

/// Map an `io::Error` to a `ReadError` variant value carrying its message.
fn read_error_value(error: &io::Error) -> Value {
    let tag = match error.kind() {
        io::ErrorKind::UnexpectedEof => "UnexpectedEof",
        _ => "Other",
    };
    error_variant(tag, error)
}

/// Map an `io::Error` to an `IoError` variant value carrying its message. Used
/// by `flush` and `open`, so it maps the file-open kinds too.
fn io_error_value(error: &io::Error) -> Value {
    let tag = match error.kind() {
        io::ErrorKind::NotFound => "NotFound",
        io::ErrorKind::PermissionDenied => "PermissionDenied",
        io::ErrorKind::AlreadyExists => "AlreadyExists",
        io::ErrorKind::BrokenPipe => "BrokenPipe",
        _ => "Other",
    };
    error_variant(tag, error)
}

/// A single-tag error variant value `@Tag(message)`.
fn error_variant(tag: &str, error: &io::Error) -> Value {
    Value::Tag {
        name: tag.to_owned(),
        payload: vec![Value::Text(error.to_string())],
    }
}

fn empty_record_value() -> Value {
    Value::record(vec![])
}

/// Map a `read_line` outcome to the handle's `Result` value:
/// `Ok(0)` -> `@Ok(undefined)` (EOF), `Ok(_)` -> `@Ok(Text)`
/// (newline-stripped), `Err` -> `@Err(ReadError)`.
pub(crate) fn read_line_value(result: io::Result<usize>, mut line: String) -> Value {
    match result {
        Ok(0) => ok_value(Value::Undefined),
        Ok(_) => {
            strip_trailing_newline(&mut line);
            ok_value(Value::Text(line))
        }
        Err(error) => err_value(read_error_value(&error)),
    }
}

/// Map a `read_to_string` outcome: `Ok(_)` -> `@Ok(Text)`, `Err` ->
/// `@Err(ReadError)`.
pub(crate) fn read_all_value(result: io::Result<usize>, text: String) -> Value {
    match result {
        Ok(_) => ok_value(Value::Text(text)),
        Err(error) => err_value(read_error_value(&error)),
    }
}

fn write_text(writer: &mut impl Write, text: &str, newline: bool) -> io::Result<()> {
    if newline {
        writeln!(writer, "{text}")
    } else {
        write!(writer, "{text}")
    }
}

/// Strip a single trailing `\n` (and a preceding `\r`) from a line read with
/// `read_line`, matching shell line semantics.
fn strip_trailing_newline(line: &mut String) {
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
}

/// Flush pending stdout so a prompt written without a trailing newline is
/// visible before a blocking read on stdin.
fn flush_stdout_before_read() {
    let _ = io::stdout().flush();
}

fn io_text_arg<'a>(name: &str, args: &'a [Value]) -> Result<&'a str, String> {
    if args.len() != 1 {
        return Err(format!("{name} expects 1 argument, got {}", args.len()));
    }

    let Value::Text(text) = &args[0] else {
        return Err(format!(
            "{name} expects Text, got {}",
            aven_value_type_name(&args[0])
        ));
    };

    Ok(text)
}

pub(crate) fn aven_value_type_name(value: &Value) -> &'static str {
    match value {
        Value::Int(_) => "Int",
        Value::Float(_) => "Float",
        Value::Text(_) => "Text",
        Value::Bool(_) => "Bool",
        Value::Array(_) => "Array",
        Value::Tuple(_) => "Tuple",
        Value::Set(_) => "Set",
        Value::Map(_) => "Map",
        Value::Record(_) => "Record",
        Value::Tag { .. } => "Tag",
        Value::ResultMethod { .. } => "Function",
        Value::Closure(_) => "Function",
        Value::Native(_) => "Native",
        Value::Type(_) => "Type",
        Value::Undefined => "Undefined",
        Value::Null => "Null",
    }
}

// --- process-stream handles -----------------------------------------------

/// Which process stream a write/flush handle method targets.
#[derive(Debug, Clone, Copy)]
enum WriteStream {
    Stdout,
    Stderr,
}

/// A write-side handle (`stdout`/`stderr`): a closed record of `write`,
/// `writeLine`, and `flush` natives, each returning a `Result`.
fn write_handle_value(stream: WriteStream) -> Value {
    Value::record(vec![
        ("write".to_owned(), write_handle_native(stream, false)),
        ("writeLine".to_owned(), write_handle_native(stream, true)),
        ("flush".to_owned(), flush_handle_native(stream)),
    ])
}

/// The `stdin` handle: a closed record of read-side natives returning a
/// `Result`.
fn stdin_handle_value() -> Value {
    Value::record(vec![
        ("readLine".to_owned(), read_line_handle_native()),
        ("readAll".to_owned(), read_all_handle_native()),
    ])
}

/// The `stdio` handle: read- and write-side natives over stdout/stdin together.
fn stdio_handle_value() -> Value {
    Value::record(vec![
        (
            "write".to_owned(),
            write_handle_native(WriteStream::Stdout, false),
        ),
        (
            "writeLine".to_owned(),
            write_handle_native(WriteStream::Stdout, true),
        ),
        ("readLine".to_owned(), read_line_handle_native()),
        ("readAll".to_owned(), read_all_handle_native()),
        ("flush".to_owned(), flush_handle_native(WriteStream::Stdout)),
    ])
}

fn write_handle_native(stream: WriteStream, newline: bool) -> Value {
    Value::native(move |args| {
        let name = if newline { "writeLine" } else { "write" };
        let text = io_text_arg(name, args)?;
        let result = match stream {
            WriteStream::Stdout => write_text(&mut io::stdout().lock(), text, newline),
            WriteStream::Stderr => write_text(&mut io::stderr().lock(), text, newline),
        };
        Ok(match result {
            Ok(()) => ok_value(empty_record_value()),
            Err(error) => err_value(write_error_value(&error)),
        })
    })
}

fn flush_handle_native(stream: WriteStream) -> Value {
    Value::native(move |args| {
        if !args.is_empty() {
            return Err(format!("flush expects 0 arguments, got {}", args.len()));
        }

        let result = match stream {
            WriteStream::Stdout => io::stdout().flush(),
            WriteStream::Stderr => io::stderr().flush(),
        };
        Ok(match result {
            Ok(()) => ok_value(empty_record_value()),
            Err(error) => err_value(io_error_value(&error)),
        })
    })
}

fn read_line_handle_native() -> Value {
    Value::native(|args| {
        if !args.is_empty() {
            return Err(format!("readLine expects 0 arguments, got {}", args.len()));
        }

        flush_stdout_before_read();

        let mut line = String::new();
        let result = io::stdin().lock().read_line(&mut line);
        Ok(read_line_value(result, line))
    })
}

fn read_all_handle_native() -> Value {
    Value::native(|args| {
        if !args.is_empty() {
            return Err(format!("readAll expects 0 arguments, got {}", args.len()));
        }

        flush_stdout_before_read();

        let mut text = String::new();
        let result = io::stdin().lock().read_to_string(&mut text);
        Ok(read_all_value(result, text))
    })
}

// --- files ----------------------------------------------------------------

/// The open OS resource behind a file handle, shared by every method closure of
/// that handle through an `Rc<RefCell<_>>`.
///
/// `ReadWrite` keeps the raw `File`, not a buffered reader AND writer: one
/// `BufReader` and one `BufWriter` over a single file would maintain two
/// independent cursors over one OS offset, which is unsound. The raw file shares
/// a single cursor, so `flush` on a read+write handle is a no-op success and
/// `Drop` just closes it.
enum FileState {
    Read(BufReader<File>),
    Write(BufWriter<File>),
    ReadWrite(File),
}

impl Drop for FileState {
    fn drop(&mut self) {
        // Flush-on-close for the buffered writer. `Drop` can't return a result,
        // so a flush error here is swallowed — this is the documented best-effort
        // auto-close. The `Read`/`ReadWrite` arms just close the file.
        if let FileState::Write(writer) = self {
            let _ = writer.flush();
        }
    }
}

/// Which method record a freshly opened file exposes.
#[derive(Debug, Clone, Copy)]
enum HandleKind {
    Read,
    Write,
    ReadWrite,
}

struct OpenComptimeResolver;

impl HostComptimeFn for OpenComptimeResolver {
    fn resolve(&self, args: &[ComptimeArg]) -> Result<Type, ComptimeError> {
        let [mode] = args else {
            return Err(ComptimeError::new(
                "open resolver expects one compile-time mode argument",
            ));
        };
        let Some(mode) = mode.as_text() else {
            return Err(ComptimeError::new(
                "open mode must be a compile-time Text literal",
            ));
        };

        let handle = match mode {
            "r" => crate::stdin_handle_type(),
            "w" | "a" => crate::stdout_handle_type(),
            "rw" => crate::stdio_handle_type(),
            other => return Err(ComptimeError::new(format!("unknown open mode `{other}`"))),
        };

        Ok(crate::build::result(handle, crate::io_error_type()))
    }
}

pub(crate) fn open_comptime_resolver() -> Rc<dyn HostComptimeFn> {
    Rc::new(OpenComptimeResolver)
}

fn file_value() -> Value {
    Value::record(vec![("open".to_owned(), open_native())])
}

fn open_native() -> Value {
    Value::native(|args| {
        if args.len() != 2 {
            return Err(format!("open expects 2 arguments, got {}", args.len()));
        }

        let Value::Text(path) = &args[0] else {
            return Err(format!(
                "open expects a Text path, got {}",
                aven_value_type_name(&args[0])
            ));
        };
        let Value::Text(mode) = &args[1] else {
            return Err(format!(
                "open expects a Text mode, got {}",
                aven_value_type_name(&args[1])
            ));
        };

        Ok(match open_file(path, mode) {
            Ok(handle) => ok_value(handle),
            Err(error) => err_value(io_error_value(&error)),
        })
    })
}

/// Open `path` for `mode` and build the matching handle record. The `Rc` is
/// cloned into each method closure, so the file lives until the last method of
/// the returned record drops.
fn open_file(path: &str, mode: &str) -> io::Result<Value> {
    let (state, kind) = match mode {
        // Read: must exist, read-only.
        "r" => (
            FileState::Read(BufReader::new(File::open(path)?)),
            HandleKind::Read,
        ),
        // Write: create or truncate.
        "w" => {
            let file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(path)?;
            (FileState::Write(BufWriter::new(file)), HandleKind::Write)
        }
        // Append: create if absent, write at the end.
        "a" => {
            let file = OpenOptions::new().append(true).create(true).open(path)?;
            (FileState::Write(BufWriter::new(file)), HandleKind::Write)
        }
        // ReadWrite: open existing read+write, no create, no truncate.
        "rw" => {
            let file = OpenOptions::new().read(true).write(true).open(path)?;
            (FileState::ReadWrite(file), HandleKind::ReadWrite)
        }
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown open mode `{other}`"),
            ));
        }
    };

    let state = Rc::new(RefCell::new(state));
    Ok(file_handle_value(&state, kind))
}

fn file_handle_value(state: &Rc<RefCell<FileState>>, kind: HandleKind) -> Value {
    let fields = match kind {
        HandleKind::Read => vec![
            ("readLine".to_owned(), file_read_line_native(state)),
            ("readAll".to_owned(), file_read_all_native(state)),
        ],
        HandleKind::Write => vec![
            ("write".to_owned(), file_write_native(state, false)),
            ("writeLine".to_owned(), file_write_native(state, true)),
            ("flush".to_owned(), file_flush_native(state)),
        ],
        HandleKind::ReadWrite => vec![
            ("write".to_owned(), file_write_native(state, false)),
            ("writeLine".to_owned(), file_write_native(state, true)),
            ("readLine".to_owned(), file_read_line_native(state)),
            ("readAll".to_owned(), file_read_all_native(state)),
            ("flush".to_owned(), file_flush_native(state)),
        ],
    };
    Value::record(fields)
}

fn file_write_native(state: &Rc<RefCell<FileState>>, newline: bool) -> Value {
    let state = Rc::clone(state);
    Value::native(move |args| {
        let name = if newline { "writeLine" } else { "write" };
        let text = io_text_arg(name, args)?;
        let result = match &mut *state.borrow_mut() {
            FileState::Write(writer) => write_text(writer, text, newline),
            FileState::ReadWrite(file) => write_text(file, text, newline),
            // Unreachable: `write` is only wired for write-capable handles.
            FileState::Read(_) => Err(io::Error::other("write on a read-only handle")),
        };
        Ok(match result {
            Ok(()) => ok_value(empty_record_value()),
            Err(error) => err_value(write_error_value(&error)),
        })
    })
}

fn file_read_line_native(state: &Rc<RefCell<FileState>>) -> Value {
    let state = Rc::clone(state);
    Value::native(move |args| {
        if !args.is_empty() {
            return Err(format!("readLine expects 0 arguments, got {}", args.len()));
        }

        let mut line = String::new();
        let result = match &mut *state.borrow_mut() {
            FileState::Read(reader) => reader.read_line(&mut line),
            FileState::ReadWrite(file) => read_line_raw(file, &mut line),
            // Unreachable: `readLine` is only wired for read-capable handles.
            FileState::Write(_) => Err(io::Error::other("read on a write-only handle")),
        };
        Ok(read_line_value(result, line))
    })
}

fn file_read_all_native(state: &Rc<RefCell<FileState>>) -> Value {
    let state = Rc::clone(state);
    Value::native(move |args| {
        if !args.is_empty() {
            return Err(format!("readAll expects 0 arguments, got {}", args.len()));
        }

        let mut text = String::new();
        let result = match &mut *state.borrow_mut() {
            FileState::Read(reader) => reader.read_to_string(&mut text),
            FileState::ReadWrite(file) => file.read_to_string(&mut text),
            // Unreachable: `readAll` is only wired for read-capable handles.
            FileState::Write(_) => Err(io::Error::other("read on a write-only handle")),
        };
        Ok(read_all_value(result, text))
    })
}

fn file_flush_native(state: &Rc<RefCell<FileState>>) -> Value {
    let state = Rc::clone(state);
    Value::native(move |args| {
        if !args.is_empty() {
            return Err(format!("flush expects 0 arguments, got {}", args.len()));
        }

        let result = match &mut *state.borrow_mut() {
            FileState::Write(writer) => writer.flush(),
            // A raw read+write file is unbuffered: nothing to flush.
            FileState::ReadWrite(_) => Ok(()),
            // Unreachable: `flush` is only wired for write-capable handles.
            FileState::Read(_) => Err(io::Error::other("flush on a read-only handle")),
        };
        Ok(match result {
            Ok(()) => ok_value(empty_record_value()),
            Err(error) => err_value(io_error_value(&error)),
        })
    })
}

/// Read one line (up to and including `\n`) from a raw file without buffering
/// past it, so the file cursor stays correct for a following write. Used only
/// for the `ReadWrite` handle, which shares a single cursor.
fn read_line_raw(file: &mut File, line: &mut String) -> io::Result<usize> {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if file.read(&mut byte)? == 0 {
            break;
        }
        bytes.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    let read = bytes.len();
    if read > 0 {
        line.push_str(&String::from_utf8_lossy(&bytes));
    }
    Ok(read)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use aven_check::Type;
    use aven_core::{Span, codes};
    use aven_eval::eval_module_with_globals;
    use aven_parser::parse_module;

    use crate::Host;

    /// A unique OS-temp path that removes its file on drop.
    struct TempPath {
        path: PathBuf,
    }

    impl TempPath {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let mut path = std::env::temp_dir();
            path.push(format!("aven-io-{tag}-{nanos}-{unique}.txt"));
            Self { path }
        }

        fn as_str(&self) -> &str {
            self.path.to_str().expect("temp path is valid UTF-8")
        }
    }

    impl Drop for TempPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn file_host() -> Host {
        let mut host = Host::new();
        host.register_files();
        host
    }

    #[test]
    fn register_bare_io_exposes_expected_check_types() {
        let mut host = Host::new();
        host.register_bare_io();

        let check = host.check_globals();
        assert_eq!(
            check,
            vec![
                ("write".to_owned(), crate::io_write_type()),
                ("writeLine".to_owned(), crate::io_write_line_type()),
                ("readLine".to_owned(), crate::io_read_line_type()),
                ("readAll".to_owned(), crate::io_read_all_type()),
            ]
        );

        let eval = host.eval_globals();
        assert_eq!(
            eval.iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["write", "writeLine", "readLine", "readAll"]
        );
        for (name, value) in &eval {
            assert!(
                matches!(value, Value::Native(_)),
                "{name} is a native value"
            );
        }
    }

    /// Run `source` and return the final value, asserting no diagnostics.
    fn run(source: &str) -> Value {
        let parsed = parse_module(source);
        assert!(
            parsed.diagnostics.is_empty(),
            "program parses: {:?}",
            parsed.diagnostics
        );
        let outcome = eval_module_with_globals(&parsed.module, file_host().eval_globals());
        assert!(
            outcome.diagnostics.is_empty(),
            "program runs: {:?}",
            outcome.diagnostics
        );
        outcome.value.expect("program yields a value")
    }

    fn check_diagnostics(source: &str) -> Vec<aven_core::Diagnostic> {
        let parsed = parse_module(source);
        assert!(
            parsed.diagnostics.is_empty(),
            "program parses: {:?}",
            parsed.diagnostics
        );
        aven_check::check_module_with_host_globals(
            &parsed.module,
            &file_host().check_host_globals(),
        )
        .diagnostics
    }

    fn check_module(source: &str) -> aven_check::CheckOutput {
        let parsed = parse_module(source);
        assert!(parsed.diagnostics.is_empty(), "program parses");
        aven_check::check_module_with_host_globals(
            &parsed.module,
            &file_host().check_host_globals(),
        )
    }

    /// The inferred type recorded for binding `name`, or `None` when the checker
    /// recorded none (e.g. the binding resolved to `Deferred`).
    fn handle_binding_type(checked: &aven_check::CheckOutput, name: &str) -> Option<Type> {
        // The binding name leads the source, so its span is `0..name.len()`.
        checked
            .type_at(aven_core::Span::new(0, name.len()))
            .cloned()
    }

    fn binding_type(source: &str, name: &str) -> Type {
        let parsed = parse_module(source);
        assert!(parsed.diagnostics.is_empty(), "program parses");
        let checked = aven_check::check_module_with_host_globals(
            &parsed.module,
            &file_host().check_host_globals(),
        );
        assert!(
            checked.diagnostics.is_empty(),
            "program checks: {:?}",
            checked.diagnostics
        );
        let offset = source
            .find(name)
            .unwrap_or_else(|| panic!("source mentions `{name}`"));
        checked
            .type_at(aven_core::Span::new(offset, offset + name.len()))
            .unwrap_or_else(|| panic!("`{name}` has an inferred type"))
            .clone()
    }

    fn text_payload(value: &Value) -> &str {
        let Value::Text(text) = value else {
            panic!("expected Text, got {value:?}");
        };
        text
    }

    #[test]
    fn bare_open_is_not_a_host_global() {
        let diagnostics = check_diagnostics("handle = open(\"x\", \"r\")\n");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code.as_deref() == Some(codes::name::UNBOUND)),
            "bare open is no longer registered: {diagnostics:?}"
        );
    }

    #[test]
    fn open_read_resolves_handle_shape_from_mode() {
        let ty = binding_type("handle = File.open(\"x\", \"r\")\n", "handle");
        assert_eq!(
            ty,
            crate::build::result(crate::stdin_handle_type(), crate::io_error_type()),
            "File.open(_, \"r\") returns Result(stdin handle, IoError)"
        );
    }

    #[test]
    fn open_write_resolves_handle_shape_from_mode() {
        let ty = binding_type("handle = File.open(\"x\", \"w\")\n", "handle");
        assert_eq!(
            ty,
            crate::build::result(crate::stdout_handle_type(), crate::io_error_type()),
            "File.open(_, \"w\") returns Result(stdout handle, IoError)"
        );
    }

    #[test]
    fn open_readwrite_resolves_all_handle_methods() {
        let ty = binding_type("handle = File.open(\"x\", \"rw\")\n", "handle");
        assert_eq!(
            ty,
            crate::build::result(crate::stdio_handle_type(), crate::io_error_type()),
            "File.open(_, \"rw\") returns Result(stdio handle, IoError)"
        );
    }

    #[test]
    fn read_handle_lacks_write_method() {
        // `?!` unwraps the Result to the resolved read handle (no `write`), so
        // calling `write` on it is a missing-field error. The handle is bound
        // first because `?!.field` is not valid surface syntax.
        let diagnostics = check_diagnostics("h = File.open(\"x\", \"r\")?!\n_ = h.write(\"y\")\n");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code.as_deref() == Some("type.missing-field")),
            "read handle has no write: {diagnostics:?}"
        );
    }

    #[test]
    fn write_handle_lacks_read_method() {
        let diagnostics = check_diagnostics("h = File.open(\"x\", \"w\")?!\n_ = h.readLine()\n");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code.as_deref() == Some("type.missing-field")),
            "write handle has no readLine: {diagnostics:?}"
        );
    }

    #[test]
    fn open_with_a_non_text_mode_argument_reports_literal_union_error() {
        let source = "handle = File.open(\"x\", 5)\n";
        let bad = check_module(source);
        assert_eq!(
            bad.diagnostics
                .iter()
                .filter(|diagnostic| {
                    diagnostic.code.as_deref() == Some(codes::ty::LITERAL_NOT_IN_UNION)
                })
                .count(),
            1,
            "expected one literal-union membership diagnostic: {:?}",
            bad.diagnostics
        );
        let bad_arg_start = source.find('5').expect("source contains the bad argument");
        assert_eq!(
            bad.diagnostics[0].labels[0].span,
            Span::new(bad_arg_start, bad_arg_start + 1)
        );
        assert!(
            handle_binding_type(&bad, "handle").is_none(),
            "a bad comptime mode argument leaves the call deferred"
        );

        let good = check_module("handle = File.open(\"x\", \"r\")\n");
        assert_eq!(
            handle_binding_type(&good, "handle"),
            Some(crate::build::result(
                crate::stdin_handle_type(),
                crate::io_error_type()
            )),
            "the valid \"r\" call resolves to a handle Result"
        );
    }

    #[test]
    fn open_unknown_mode_reports_literal_union_error() {
        let diagnostics = check_diagnostics("handle = File.open(\"path\", \"x\")\n");
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.code.as_deref() == Some(codes::ty::LITERAL_NOT_IN_UNION)
            }),
            "unknown mode is rejected by the checker: {diagnostics:?}"
        );
        assert!(
            diagnostics.iter().all(|diagnostic| {
                diagnostic.code.as_deref() != Some(codes::comptime::HOST_FUNCTION)
            }),
            "unknown mode should not reach the host resolver: {diagnostics:?}"
        );
    }

    #[test]
    fn open_with_runtime_mode_reports_literal_union_error() {
        let source = "m : Text = \"r\"\nhandle = File.open(\"x\", m)\n";
        let checked = check_module(source);
        assert!(
            checked.diagnostics.iter().any(|diagnostic| {
                diagnostic.code.as_deref() == Some(codes::ty::WIDE_VALUE_INTO_LITERAL_UNION)
            }),
            "runtime mode should be rejected by the checker: {:?}",
            checked.diagnostics
        );
        let offset = source.find("handle").expect("source contains handle");
        assert_eq!(
            checked.type_at(Span::new(offset, offset + "handle".len())),
            None
        );
    }

    #[test]
    fn user_file_binding_shadowing_namespace_uses_user_field_type() {
        let source =
            "File = { open: (p : Text, m : Text) => p }\nhandle = File.open(\"x\", \"z\")\n";
        let checked = check_module(source);
        assert!(
            checked.diagnostics.is_empty(),
            "shadowed File should use the user field type: {:?}",
            checked.diagnostics
        );
        let offset = source.find("handle").expect("source contains handle");
        assert_eq!(
            checked.type_at(Span::new(offset, offset + "handle".len())),
            Some(&crate::build::text())
        );
    }

    #[test]
    fn raii_flushes_buffered_write_when_handle_drops() {
        // The headline guarantee: the buffered write lands because the scope-local
        // handle goes OUT OF SCOPE, with no explicit flush call anywhere.
        let path = TempPath::new("raii");
        // `writeIt`'s body is an indentation block; `h` is scoped to it and drops
        // when the call returns. No explicit flush anywhere — the buffered line
        // must land via `Drop for FileState`.
        let source = format!(
            "writeIt = () =>\n  h = File.open(\"{path}\", \"w\")?!\n  _ = h.writeLine(\"hello\")\n  {{}}\n_ = writeIt()\nr = File.open(\"{path}\", \"r\")?!\nr.readAll()?!\n",
            path = path.as_str(),
        );

        let value = run(&source);
        assert_eq!(
            text_payload(&value),
            "hello\n",
            "buffered line flushed on scope exit without an explicit flush"
        );
    }

    #[test]
    fn write_then_read_round_trips() {
        let path = TempPath::new("roundtrip");
        let source = format!(
            "w = File.open(\"{path}\", \"w\")?!\n_ = w.writeLine(\"line one\")?!\n_ = w.write(\"partial\")?!\n_ = w.flush()?!\nr = File.open(\"{path}\", \"r\")?!\nr.readAll()?!\n",
            path = path.as_str(),
        );
        let value = run(&source);
        assert_eq!(text_payload(&value), "line one\npartial");
    }

    #[test]
    fn append_adds_to_existing_file() {
        let path = TempPath::new("append");
        std::fs::write(path.as_str(), "first\n").expect("seed file");
        let source = format!(
            "a = File.open(\"{path}\", \"a\")?!\n_ = a.writeLine(\"second\")?!\n_ = a.flush()?!\nr = File.open(\"{path}\", \"r\")?!\nr.readAll()?!\n",
            path = path.as_str(),
        );
        let value = run(&source);
        assert_eq!(text_payload(&value), "first\nsecond\n");
    }

    #[test]
    fn read_write_reads_then_writes_existing_file() {
        let path = TempPath::new("readwrite");
        std::fs::write(path.as_str(), "head\n").expect("seed file");
        // Open read+write: read the first line, then append more at the cursor.
        let source = format!(
            "rw = File.open(\"{path}\", \"rw\")?!\nfirst = rw.readLine()?!\n_ = rw.write(\"tail\")?!\nreread = File.open(\"{path}\", \"r\")?!\n{{ first: first, all: reread.readAll()?! }}\n",
            path = path.as_str(),
        );
        let value = run(&source);
        let Value::Record(fields) = &value else {
            panic!("expected a record, got {value:?}");
        };
        let field = |name: &str| {
            fields
                .iter()
                .find_map(|(field_name, field_value)| (field_name == name).then_some(field_value))
                .unwrap_or_else(|| panic!("record has field `{name}`"))
        };
        assert_eq!(text_payload(field("first")), "head");
        assert_eq!(text_payload(field("all")), "head\ntail");
    }

    #[test]
    fn open_missing_file_is_not_found() {
        let path = TempPath::new("missing");
        let source = format!("File.open(\"{path}\", \"r\")\n", path = path.as_str());
        let value = run(&source);
        let Value::Tag { name, payload } = &value else {
            panic!("expected a Result tag, got {value:?}");
        };
        assert_eq!(name, "Err", "opening a missing file errs");
        let Value::Tag { name: kind, .. } = &payload[0] else {
            panic!("expected an IoError tag, got {:?}", payload[0]);
        };
        assert_eq!(kind, "NotFound");
    }

    #[test]
    fn open_unknown_runtime_mode_returns_error_result() {
        let path = TempPath::new("unknown-mode");
        let source = format!("File.open(\"{path}\", \"z\")\n", path = path.as_str());
        let value = run(&source);
        let Value::Tag { name, payload } = &value else {
            panic!("expected a Result tag, got {value:?}");
        };
        assert_eq!(name, "Err", "unknown runtime mode errs");
        let Value::Tag { name: kind, .. } = &payload[0] else {
            panic!("expected an IoError tag, got {:?}", payload[0]);
        };
        assert_eq!(kind, "Other");
    }

    #[test]
    fn file_write_handle_unifies_with_open_write_row() {
        use crate::build;

        // The same row-poly path that accepts `stdout`: a function on an open
        // `{ write | r }` accepts a `File.open(_, "w")?!` handle via width
        // subtyping. The `needsWrite` global is built by hand (like the P2a
        // stream test) so the param row is exact, not a deferred user
        // annotation.
        let write_method = build::function(
            vec![build::text()],
            build::result(build::empty_record(), crate::write_error_type()),
        );
        let mut globals = file_host().check_host_globals();
        globals.types.push((
            "needsWrite".to_owned(),
            build::function(
                vec![build::open_record(vec![("write", write_method)])],
                build::empty_record(),
            ),
        ));
        globals
            .types
            .push(("stdout".to_owned(), crate::stdout_handle_type()));

        for source in [
            "_ = needsWrite(File.open(\"x\", \"w\")?!)\n",
            "_ = needsWrite(stdout)\n",
        ] {
            let parsed = parse_module(source);
            assert!(parsed.diagnostics.is_empty(), "{source} parses");
            let checked = aven_check::check_module_with_host_globals(&parsed.module, &globals);
            assert!(
                checked.diagnostics.is_empty(),
                "{source} checks: {:?}",
                checked.diagnostics
            );
        }
    }
}

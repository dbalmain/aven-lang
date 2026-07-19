//! The display protocol: `toText` rendering and the structural `debugText`.
//!
//! `to_text` is the rendering behind interpolation (`${x}`), the ambient
//! `.toText()` method, and `aven run`'s final-value printing: a user or family
//! `toText` override wins, then the builtin rendering for the value's shape,
//! then the `debugText` rendering for renderless values (closures, methods,
//! host handles) — interpolation never fails. Propagation is homogeneous:
//! whichever rendering is called on a container is the one called on its
//! elements, so text inside a `toText`-rendered array is unquoted while
//! `debugText` quotes and escapes it.

use std::{cell::RefCell, fmt::Write as _};

use aven_core::{Diagnostic, Span};

use crate::{
    BuiltinMethodEnvironment, Closure, Eval, NamedMethodImplementation, Value, apply_callee_values,
    apply_closure_values, escape_string, flow_diagnostics, one_diagnostic, record_field_value,
    record_type_error,
};

/// The field host temporal records carry to identify their nominal kind. Kept
/// in lockstep with `aven-host`'s temporal module; the display protocol uses it
/// to render temporal values as their ISO text instead of raw record structure.
const TEMPORAL_KIND_FIELD: &str = "__temporal";

thread_local! {
    /// A family `toText` body can render a value derived from its receiver.
    /// Primitive-family arithmetic preserves the brand, so without this guard
    /// the conventional `Money = Int { toText() => "$${. / 100}" }` loops
    /// back into the same override. Re-entrant values use the inherited base
    /// rendering; unrelated families still dispatch normally.
    static ACTIVE_TO_TEXT_OWNERS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn with_active_to_text_owner<T>(
    owner: &str,
    render: impl FnOnce() -> Eval<T>,
) -> Eval<T> {
    ACTIVE_TO_TEXT_OWNERS.with(|owners| owners.borrow_mut().push(owner.to_owned()));
    let result = render();
    ACTIVE_TO_TEXT_OWNERS.with(|owners| {
        owners.borrow_mut().pop();
    });
    result
}

pub(crate) fn to_text_owner_is_active(owner: &str) -> bool {
    ACTIVE_TO_TEXT_OWNERS.with(|owners| owners.borrow().iter().any(|active| active == owner))
}

/// Render a value with the `toText` protocol: override, then builtin
/// rendering, then the `debugText` fallback. This is the rendering `aven run`
/// uses for final values, matching interpolation. It fails only when a user
/// `toText` override itself fails.
pub fn display_text(value: &Value) -> Result<String, Vec<Diagnostic>> {
    to_text(value, None, Span::new(0, 0)).map_err(flow_diagnostics)
}

/// The universal, non-overridable structural rendering: code-shaped, with
/// named/branded values wrapped in their constructor, text quoted and escaped,
/// floats always carrying a decimal point, and opaque markers for values with
/// no literal form. Never consults `toText` overrides.
pub fn debug_text(value: &Value) -> String {
    let mut out = String::new();
    write_debug(&mut out, value);
    out
}

/// Whether a value carries the ambient `toText` method (the builtin rendering
/// exists for its shape). Values outside this set — closures, methods, natives,
/// type values — only reach the `debugText` fallback through interpolation.
pub(crate) fn carries_ambient_to_text(value: &Value) -> bool {
    matches!(
        value,
        Value::Int(_)
            | Value::Float(_)
            | Value::Text(_)
            | Value::Bool(_)
            | Value::Array(_)
            | Value::Tuple(_)
            | Value::Set(_)
            | Value::Map(_)
            | Value::Record(_)
            | Value::NamedRecord { .. }
            | Value::BrandedPrimitive { .. }
            | Value::Tag { .. }
            | Value::Undefined
            | Value::Null
    )
}

/// The `toText` protocol rendering. `attachments` carries the builtin-method
/// attachment environment when one is in scope (interpolation), so an attached
/// `toText` on a builtin owner is honored like a family override.
pub(crate) fn to_text(
    value: &Value,
    attachments: Option<&BuiltinMethodEnvironment>,
    span: Span,
) -> Eval<String> {
    if let Some(text) = override_to_text(value, span, attachments)? {
        return Ok(text);
    }
    let mut out = String::new();
    write_to_text(&mut out, value, attachments, span)?;
    Ok(out)
}

/// Consult the user-defined `toText` carried by the value itself: a builtin
/// attachment, a family declaration, or a slot-record `toText` slot. Inherited
/// family methods are exactly the base builtin rendering, so they fall through
/// to it rather than re-dispatching.
fn override_to_text(
    value: &Value,
    span: Span,
    attachments: Option<&BuiltinMethodEnvironment>,
) -> Eval<Option<String>> {
    if let Some(closure) = attachments.and_then(|methods| methods.lookup(value, "toText")) {
        return apply_to_text_closure(closure, value, span).map(Some);
    }
    match value {
        Value::BrandedPrimitive { descriptor, .. } | Value::NamedRecord { descriptor, .. } => {
            if to_text_owner_is_active(&descriptor.owner) {
                return Ok(None);
            }
            match descriptor.methods.get("toText") {
                Some(NamedMethodImplementation::Declared(closure)) => {
                    with_active_to_text_owner(&descriptor.owner, || {
                        apply_to_text_closure(closure.clone(), value, span)
                    })
                    .map(Some)
                }
                Some(NamedMethodImplementation::Inherited(_)) | None => Ok(None),
            }
        }
        Value::SlotRecord { slots, .. } => match record_field_value(slots, "toText") {
            Some(slot) => {
                let result = apply_callee_values(slot.clone(), span, Vec::new(), span)?;
                expect_text(result, span).map(Some)
            }
            None => Ok(None),
        },
        _ => Ok(None),
    }
}

fn apply_to_text_closure(closure: Closure, receiver: &Value, span: Span) -> Eval<String> {
    let result = apply_closure_values(closure, vec![receiver.clone()], span)?;
    expect_text(result, span)
}

fn expect_text(value: Value, span: Span) -> Eval<String> {
    match value {
        Value::Text(text) => Ok(text),
        value => Err(one_diagnostic(record_type_error(
            span,
            "`toText` result",
            value.type_name(),
            "Text",
        ))),
    }
}

fn write_to_text(
    out: &mut String,
    value: &Value,
    attachments: Option<&BuiltinMethodEnvironment>,
    span: Span,
) -> Eval<()> {
    match value {
        Value::Int(value) => push_int(out, *value),
        Value::Float(value) => out.push_str(&float_text(*value)),
        Value::Text(text) => out.push_str(text),
        Value::Bool(value) => out.push_str(bool_text(*value)),
        Value::Array(items) => {
            write_to_text_sequence(out, "[", "]", items, attachments, span)?;
        }
        Value::Tuple(items) => {
            write_to_text_sequence(out, "(", ")", items, attachments, span)?;
        }
        Value::Set(items) => {
            write_to_text_braced(out, "@{", items.len(), |out| {
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    let element = to_text(item, attachments, span)?;
                    out.push_str(&element);
                }
                Ok(())
            })?;
        }
        Value::Map(entries) => {
            write_to_text_braced(out, "Map{", entries.len(), |out| {
                for (index, (key, value)) in entries.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    let key = to_text(key, attachments, span)?;
                    out.push_str(&key);
                    out.push_str(": ");
                    let value = to_text(value, attachments, span)?;
                    out.push_str(&value);
                }
                Ok(())
            })?;
        }
        Value::Record(fields) => {
            if let Some(iso) = temporal_iso_text(fields) {
                out.push_str(&iso);
                return Ok(());
            }
            write_to_text_record(out, fields, attachments, span)?;
        }
        Value::NamedRecord { fields, .. } => {
            write_to_text_record(out, fields, attachments, span)?;
        }
        Value::BrandedPrimitive { payload, .. } => {
            write_to_text(out, &payload.to_value(), attachments, span)?;
        }
        Value::Tag { name, payload } => {
            out.push('@');
            out.push_str(name);
            if !payload.is_empty() {
                write_to_text_sequence(out, "(", ")", payload, attachments, span)?;
            }
        }
        Value::Undefined => out.push_str("undefined"),
        Value::Null => out.push_str("null"),
        // Renderless values: the debugText rendering, so interpolation never
        // fails. Slot records without a `toText` slot land here too.
        Value::SlotRecord { .. }
        | Value::NamedFamily(_)
        | Value::NamedMethod { .. }
        | Value::UnboundNamedMethod { .. }
        | Value::ResultMethod { .. }
        | Value::Closure(_)
        | Value::Native(_)
        | Value::Type(_) => write_debug(out, value),
    }
    Ok(())
}

fn write_to_text_sequence(
    out: &mut String,
    open: &str,
    close: &str,
    items: &[Value],
    attachments: Option<&BuiltinMethodEnvironment>,
    span: Span,
) -> Eval<()> {
    out.push_str(open);
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        let element = to_text(item, attachments, span)?;
        out.push_str(&element);
    }
    out.push_str(close);
    Ok(())
}

fn write_to_text_record(
    out: &mut String,
    fields: &[(String, Value)],
    attachments: Option<&BuiltinMethodEnvironment>,
    span: Span,
) -> Eval<()> {
    write_to_text_braced(out, "{", fields.len(), |out| {
        for (index, (name, value)) in fields.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(name);
            out.push_str(": ");
            let value = to_text(value, attachments, span)?;
            out.push_str(&value);
        }
        Ok(())
    })
}

/// Write a `{`-family opener whose body is padded with one space on each side
/// when non-empty (`@{ 1, 2 }`, `{ a: 1 }`), matching the formatter's layout.
fn write_to_text_braced(
    out: &mut String,
    open: &str,
    len: usize,
    body: impl FnOnce(&mut String) -> Eval<()>,
) -> Eval<()> {
    out.push_str(open);
    if len > 0 {
        out.push(' ');
        body(out)?;
        out.push(' ');
    }
    out.push('}');
    Ok(())
}

fn write_debug(out: &mut String, value: &Value) {
    match value {
        Value::Int(value) => push_int(out, *value),
        Value::Float(value) => out.push_str(&float_text(*value)),
        Value::Text(text) => {
            out.push('"');
            out.push_str(&escape_string(text));
            out.push('"');
        }
        Value::Bool(value) => out.push_str(bool_text(*value)),
        Value::Array(items) => write_debug_sequence(out, "[", "]", items),
        Value::Tuple(items) => write_debug_sequence(out, "(", ")", items),
        Value::Set(items) => {
            write_debug_braced(out, "@{", items.len(), |out| {
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    write_debug(out, item);
                }
            });
        }
        Value::Map(entries) => {
            write_debug_braced(out, "Map{", entries.len(), |out| {
                for (index, (key, value)) in entries.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    write_debug(out, key);
                    out.push_str(": ");
                    write_debug(out, value);
                }
            });
        }
        Value::Record(fields) => {
            if let Some(iso) = temporal_debug_text(fields) {
                out.push_str(&iso);
                return;
            }
            write_debug_record(out, fields);
        }
        Value::SlotRecord { .. } => out.push_str("<slot-record>"),
        Value::NamedRecord { descriptor, fields } => {
            out.push_str(family_name(&descriptor.owner));
            out.push('(');
            write_debug_record(out, fields);
            out.push(')');
        }
        Value::BrandedPrimitive {
            descriptor,
            payload,
        } => {
            out.push_str(family_name(&descriptor.owner));
            out.push('(');
            write_debug(out, &payload.to_value());
            out.push(')');
        }
        Value::Tag { name, payload } => {
            out.push('@');
            out.push_str(name);
            if !payload.is_empty() {
                write_debug_sequence(out, "(", ")", payload);
            }
        }
        Value::NamedFamily(descriptor) => out.push_str(family_name(&descriptor.owner)),
        Value::NamedMethod { .. }
        | Value::UnboundNamedMethod { .. }
        | Value::ResultMethod { .. } => {
            out.push_str("<method>");
        }
        Value::Closure(_) => out.push_str("<function>"),
        Value::Native(_) => out.push_str("<native>"),
        Value::Type(ty) => {
            let _ = write!(out, "{ty}");
        }
        Value::Undefined => out.push_str("undefined"),
        Value::Null => out.push_str("null"),
    }
}

fn write_debug_sequence(out: &mut String, open: &str, close: &str, items: &[Value]) {
    out.push_str(open);
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        write_debug(out, item);
    }
    out.push_str(close);
}

fn write_debug_record(out: &mut String, fields: &[(String, Value)]) {
    write_debug_braced(out, "{", fields.len(), |out| {
        for (index, (name, value)) in fields.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(name);
            out.push_str(": ");
            write_debug(out, value);
        }
    });
}

fn write_debug_braced(out: &mut String, open: &str, len: usize, body: impl FnOnce(&mut String)) {
    out.push_str(open);
    if len > 0 {
        out.push(' ');
        body(out);
        out.push(' ');
    }
    out.push('}');
}

fn push_int(out: &mut String, value: i64) {
    let _ = write!(out, "{value}");
}

fn bool_text(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

/// Round-trippable float text: always a decimal point (`1.0`), or the words
/// `NaN` / `Infinity` / `-Infinity` for non-finite values.
pub(crate) fn float_text(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value == f64::INFINITY {
        return "Infinity".to_owned();
    }
    if value == f64::NEG_INFINITY {
        return "-Infinity".to_owned();
    }
    let mut text = value.to_string();
    if !text.contains('.') {
        text.push_str(".0");
    }
    text
}

/// The ISO rendering of a host temporal record (`Instant`, `Date`, ...): the
/// record's own zero-argument `format` native, recognized by the temporal kind
/// marker field. Non-temporal records return `None`.
fn temporal_iso_text(fields: &[(String, Value)]) -> Option<String> {
    let Some(Value::Text(_)) = record_field_value(fields, TEMPORAL_KIND_FIELD) else {
        return None;
    };
    let Some(Value::Native(format)) = record_field_value(fields, "format") else {
        return None;
    };
    match format(&[]) {
        Ok(Value::Text(text)) => Some(text),
        _ => None,
    }
}

/// The debug rendering of a temporal record: constructor-shaped around the ISO
/// text (`Instant("2026-07-19T00:00:00Z")`), since temporal values have no
/// literal form and their raw record structure is host-internal.
fn temporal_debug_text(fields: &[(String, Value)]) -> Option<String> {
    let Some(Value::Text(kind)) = record_field_value(fields, TEMPORAL_KIND_FIELD) else {
        return None;
    };
    let iso = temporal_iso_text(fields)?;
    Some(format!("{kind}(\"{iso}\")"))
}

/// Family descriptors use a module-qualified internal owner key. Structural
/// rendering exposes only the source-level family name.
fn family_name(owner: &str) -> &str {
    owner.rsplit('\0').next().unwrap_or(owner)
}

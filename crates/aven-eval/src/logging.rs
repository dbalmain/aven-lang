use std::rc::Rc;

use crate::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

impl Level {
    pub fn severity_number(self) -> u8 {
        match self {
            Self::Trace => 1,
            Self::Debug => 5,
            Self::Info => 9,
            Self::Warn => 13,
            Self::Error => 17,
            Self::Fatal => 21,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
            Self::Fatal => "fatal",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceContext {
    pub trace_id: String,
    pub span_id: String,
    pub trace_flags: String,
    pub trace_state: String,
}

pub struct LogRecord<'a> {
    pub level: Level,
    pub message: String,
    pub attributes: &'a [(String, Value)],
    pub trace: &'a TraceContext,
}

pub trait LogSink {
    fn emit(&self, record: &LogRecord<'_>);
}

#[derive(Clone)]
struct LoggerState {
    sink: Rc<dyn LogSink>,
    context: Rc<Vec<(String, Value)>>,
    trace: Rc<TraceContext>,
}

pub fn logger(sink: Rc<dyn LogSink>, trace: TraceContext) -> Value {
    logger_value(LoggerState {
        sink,
        context: Rc::new(Vec::new()),
        trace: Rc::new(trace),
    })
}

fn logger_value(state: LoggerState) -> Value {
    let mut fields = Vec::new();
    for level in [
        Level::Trace,
        Level::Debug,
        Level::Info,
        Level::Warn,
        Level::Error,
        Level::Fatal,
    ] {
        let method_state = state.clone();
        fields.push((
            level.as_str().to_owned(),
            Value::native(move |args| emit_level(&method_state, level, args)),
        ));
    }

    let child_state = state.clone();
    fields.push((
        "child".to_owned(),
        Value::native(move |args| child_logger(&child_state, args)),
    ));

    Value::record(fields)
}

fn emit_level(state: &LoggerState, level: Level, args: &[Value]) -> Result<Value, String> {
    if !(1..=2).contains(&args.len()) {
        return Err(format!(
            "log.{} expects (message: Text, fields?: Record), got {} argument(s)",
            level.as_str(),
            args.len()
        ));
    }

    let Value::Text(message) = &args[0] else {
        return Err(format!(
            "log.{} message must be Text, got {}",
            level.as_str(),
            args[0].type_name()
        ));
    };

    let mut attributes = (*state.context).clone();
    if let Some(fields) = args.get(1) {
        let Value::Record(fields) = fields else {
            return Err(format!(
                "log.{} fields must be Record, got {}",
                level.as_str(),
                fields.type_name()
            ));
        };
        merge_fields(&mut attributes, fields);
    }

    let record = LogRecord {
        level,
        message: message.clone(),
        attributes: &attributes,
        trace: state.trace.as_ref(),
    };
    state.sink.emit(&record);

    Ok(Value::unit())
}

fn child_logger(state: &LoggerState, args: &[Value]) -> Result<Value, String> {
    if args.len() != 1 {
        return Err(format!(
            "log.child expects (fields: Record), got {} argument(s)",
            args.len()
        ));
    }

    let Value::Record(fields) = &args[0] else {
        return Err(format!(
            "log.child fields must be Record, got {}",
            args[0].type_name()
        ));
    };

    let mut context = (*state.context).clone();
    let mut trace = (*state.trace).clone();

    // Child loggers keep the parent's span id unless spanId is supplied;
    // generating a fresh child span needs host randomness, which lives outside eval.
    for (name, value) in fields.iter() {
        if update_trace_field(&mut trace, name, value)? {
            continue;
        }
        insert_or_replace_field(&mut context, name.clone(), value.clone());
    }

    Ok(logger_value(LoggerState {
        sink: Rc::clone(&state.sink),
        context: Rc::new(context),
        trace: Rc::new(trace),
    }))
}

fn update_trace_field(trace: &mut TraceContext, name: &str, value: &Value) -> Result<bool, String> {
    match name {
        "traceId" => {
            let text = trace_field_text(name, value)?;
            validate_trace_id(&text)?;
            trace.trace_id = text;
            Ok(true)
        }
        "spanId" => {
            let text = trace_field_text(name, value)?;
            validate_span_id(&text)?;
            trace.span_id = text;
            Ok(true)
        }
        "traceFlags" => {
            let text = trace_field_text(name, value)?;
            validate_trace_flags(&text)?;
            trace.trace_flags = text;
            Ok(true)
        }
        "traceState" => {
            trace.trace_state = trace_field_text(name, value)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn trace_field_text(name: &str, value: &Value) -> Result<String, String> {
    let Value::Text(text) = value else {
        return Err(format!(
            "log.child field `{name}` must be Text when updating trace context, got {}",
            value.type_name()
        ));
    };
    Ok(text.clone())
}

fn validate_trace_id(value: &str) -> Result<(), String> {
    validate_w3c_hex(value, 32, "traceId")
}

fn validate_span_id(value: &str) -> Result<(), String> {
    validate_w3c_hex(value, 16, "spanId")
}

fn validate_trace_flags(value: &str) -> Result<(), String> {
    if value.len() == 2 && is_lower_hex(value) {
        Ok(())
    } else {
        Err("log.child field `traceFlags` must be 2 lowercase hex characters".to_owned())
    }
}

fn validate_w3c_hex(value: &str, len: usize, name: &str) -> Result<(), String> {
    if value.len() == len && is_lower_hex(value) && !value.bytes().all(|byte| byte == b'0') {
        Ok(())
    } else {
        Err(format!(
            "log.child field `{name}` must be {len} lowercase hex characters and not all zero"
        ))
    }
}

fn is_lower_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn merge_fields(target: &mut Vec<(String, Value)>, fields: &[(String, Value)]) {
    for (name, value) in fields {
        insert_or_replace_field(target, name.clone(), value.clone());
    }
}

fn insert_or_replace_field(fields: &mut Vec<(String, Value)>, name: String, value: Value) {
    if let Some(index) = fields.iter().position(|(field, _)| field == &name) {
        fields[index] = (name, value);
    } else {
        fields.push((name, value));
    }
}

//! Nominal temporal types: `Instant`, `Date`, `Time`, `DateTime`, `Duration`.
//!
//! Values are host-built records carrying a private `__temporal` kind marker so
//! codecs can recognize them without leaking third-party datetime types into
//! `aven-core`/`aven-check`/`aven-eval`. Accessors (`year`, `month`, …) are
//! plain `Int` fields; methods (`format`, `dateTime`, `instant`) are natives
//! closed over the same data. Companion statics (`Date.new`, `Instant.parse`,
//! `Date.compare`, …) live on the type name via the host statics table.
//!
//! ## Epoch representation
//!
//! `Instant` and `Duration` store **i64 nanoseconds** (epoch for Instant).
//! That range is roughly years 1678–2262 — enough for config/script clocks and
//! a single `Value::Int` field. Values outside the range fail at parse /
//! construction rather than silently wrapping. `i128` was rejected for this
//! slice: no public Aven integer wider than `Int` (`i64`), and codecs already
//! round-trip through that width.

use std::cmp::Ordering;

use aven_check::Type;
use aven_eval::Value;

use crate::Host;
use crate::io::{err_value, ok_value};
use crate::text_format::FormatTemporal;

/// Private record field that identifies a temporal host value for codecs.
pub(crate) const TEMPORAL_KIND_FIELD: &str = "__temporal";

const NANOS_PER_SECOND: i64 = 1_000_000_000;
const NANOS_PER_MINUTE: i64 = 60 * NANOS_PER_SECOND;
const NANOS_PER_HOUR: i64 = 60 * NANOS_PER_MINUTE;
const NANOS_PER_DAY: i64 = 24 * NANOS_PER_HOUR;

// --- pure calendar / timeline values --------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Date {
    pub year: i32,
    pub month: u8,
    pub day: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Time {
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub nanosecond: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct DateTime {
    pub date: Date,
    pub time: Time,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Instant {
    /// UTC epoch nanoseconds.
    pub nanos: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Duration {
    pub nanos: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TemporalError(String);

impl TemporalError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    fn message(&self) -> &str {
        &self.0
    }
}

impl Date {
    pub(crate) fn new(year: i32, month: i64, day: i64) -> Result<Self, TemporalError> {
        if !(1..=12).contains(&month) {
            return Err(TemporalError::new(format!("invalid month {month}")));
        }
        if !(1..=31).contains(&day) {
            return Err(TemporalError::new(format!("invalid day {day}")));
        }
        let month = month as u8;
        let day = day as u8;
        let max_day = days_in_month(year, month);
        if day > max_day {
            return Err(TemporalError::new(format!(
                "invalid day {day} for {year:04}-{month:02}"
            )));
        }
        Ok(Self { year, month, day })
    }

    pub(crate) fn format(self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    pub(crate) fn parse(text: &str) -> Result<Self, TemporalError> {
        let bytes = text.as_bytes();
        if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
            return Err(TemporalError::new(format!("invalid Date text `{text}`")));
        }
        let year: i32 = parse_signed(&text[..4])
            .ok_or_else(|| TemporalError::new(format!("invalid Date year in `{text}`")))?;
        let month: i64 = parse_u32_digits(&text[5..7])
            .map(i64::from)
            .ok_or_else(|| TemporalError::new(format!("invalid Date month in `{text}`")))?;
        let day: i64 = parse_u32_digits(&text[8..10])
            .map(i64::from)
            .ok_or_else(|| TemporalError::new(format!("invalid Date day in `{text}`")))?;
        Self::new(year, month, day)
    }
}

impl Time {
    pub(crate) fn new(
        hour: i64,
        minute: i64,
        second: i64,
        nanosecond: i64,
    ) -> Result<Self, TemporalError> {
        if !(0..=23).contains(&hour) {
            return Err(TemporalError::new(format!("invalid hour {hour}")));
        }
        if !(0..=59).contains(&minute) {
            return Err(TemporalError::new(format!("invalid minute {minute}")));
        }
        // Allow leap second 60.
        if !(0..=60).contains(&second) {
            return Err(TemporalError::new(format!("invalid second {second}")));
        }
        if !(0..NANOS_PER_SECOND).contains(&nanosecond) {
            return Err(TemporalError::new(format!(
                "invalid nanosecond {nanosecond}"
            )));
        }
        Ok(Self {
            hour: hour as u8,
            minute: minute as u8,
            second: second as u8,
            nanosecond: nanosecond as u32,
        })
    }

    pub(crate) fn format(self) -> String {
        if self.nanosecond == 0 {
            format!("{:02}:{:02}:{:02}", self.hour, self.minute, self.second)
        } else {
            let mut frac = format!("{:09}", self.nanosecond);
            while frac.ends_with('0') {
                frac.pop();
            }
            format!(
                "{:02}:{:02}:{:02}.{frac}",
                self.hour, self.minute, self.second
            )
        }
    }

    pub(crate) fn parse(text: &str) -> Result<Self, TemporalError> {
        parse_time(text)
    }

    fn nanos_of_day(self) -> i64 {
        i64::from(self.hour) * NANOS_PER_HOUR
            + i64::from(self.minute) * NANOS_PER_MINUTE
            + i64::from(self.second) * NANOS_PER_SECOND
            + i64::from(self.nanosecond)
    }
}

impl DateTime {
    pub(crate) fn of(date: Date, time: Time) -> Self {
        Self { date, time }
    }

    pub(crate) fn format(self) -> String {
        format!("{}T{}", self.date.format(), self.time.format())
    }

    pub(crate) fn parse(text: &str) -> Result<Self, TemporalError> {
        let (date_text, time_text) = split_date_time(text)?;
        Ok(Self::of(Date::parse(date_text)?, Time::parse(time_text)?))
    }

    pub(crate) fn instant(self, offset_minutes: i64) -> Result<Instant, TemporalError> {
        Instant::from_datetime(self, offset_minutes)
    }
}

impl Instant {
    pub(crate) fn from_nanos(nanos: i64) -> Self {
        Self { nanos }
    }

    pub(crate) fn format(self) -> String {
        let dt = self
            .date_time(0)
            .expect("Instant nanos always map to a civil DateTime at UTC");
        format!("{}Z", dt.format())
    }

    pub(crate) fn parse(text: &str) -> Result<Self, TemporalError> {
        let (datetime_text, offset_minutes) = split_instant(text)?;
        let datetime = DateTime::parse(datetime_text)?;
        Self::from_datetime(datetime, offset_minutes)
    }

    pub(crate) fn date_time(self, offset_minutes: i64) -> Result<DateTime, TemporalError> {
        let offset_nanos = offset_minutes
            .checked_mul(NANOS_PER_MINUTE)
            .ok_or_else(|| TemporalError::new("offset out of range"))?;
        let local_nanos = self
            .nanos
            .checked_add(offset_nanos)
            .ok_or_else(|| TemporalError::new("instant + offset out of range"))?;
        civil_from_epoch_nanos(local_nanos)
    }

    fn from_datetime(datetime: DateTime, offset_minutes: i64) -> Result<Self, TemporalError> {
        let day = days_from_civil(datetime.date.year, datetime.date.month, datetime.date.day);
        let day_nanos = day
            .checked_mul(NANOS_PER_DAY)
            .ok_or_else(|| TemporalError::new("date out of Instant range"))?;
        let local = day_nanos
            .checked_add(datetime.time.nanos_of_day())
            .ok_or_else(|| TemporalError::new("date-time out of Instant range"))?;
        let offset_nanos = offset_minutes
            .checked_mul(NANOS_PER_MINUTE)
            .ok_or_else(|| TemporalError::new("offset out of range"))?;
        let nanos = local
            .checked_sub(offset_nanos)
            .ok_or_else(|| TemporalError::new("instant out of range"))?;
        Ok(Self { nanos })
    }
}

impl Duration {
    pub(crate) fn of_seconds(seconds: i64) -> Result<Self, TemporalError> {
        let nanos = seconds
            .checked_mul(NANOS_PER_SECOND)
            .ok_or_else(|| TemporalError::new("Duration.ofSeconds overflow"))?;
        Ok(Self { nanos })
    }

    pub(crate) fn of_nanos(nanos: i64) -> Self {
        Self { nanos }
    }

    pub(crate) fn format(self) -> String {
        format_iso_duration(self.nanos)
    }

    pub(crate) fn parse(text: &str) -> Result<Self, TemporalError> {
        parse_iso_duration(text).map(Self::of_nanos)
    }
}

// --- calendar math (Howard Hinnant civil days) ----------------------------

fn is_leap(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i32, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days since 1970-01-01 (Unix epoch civil).
fn days_from_civil(year: i32, month: u8, day: u8) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = (y - era * 400) as u32;
    let mp = if month > 2 {
        u32::from(month) - 3
    } else {
        u32::from(month) + 9
    };
    let doy = (153 * mp + 2) / 5 + u32::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    i64::from(era) * 146_097 + i64::from(doe) - 719_468
}

fn civil_from_days(z: i64) -> Date {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = era * 400 + i64::from(yoe);
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    Date {
        year: year as i32,
        month: m as u8,
        day: d as u8,
    }
}

fn civil_from_epoch_nanos(nanos: i64) -> Result<DateTime, TemporalError> {
    let day = nanos.div_euclid(NANOS_PER_DAY);
    let mut time_nanos = nanos.rem_euclid(NANOS_PER_DAY);
    let hour = (time_nanos / NANOS_PER_HOUR) as u8;
    time_nanos %= NANOS_PER_HOUR;
    let minute = (time_nanos / NANOS_PER_MINUTE) as u8;
    time_nanos %= NANOS_PER_MINUTE;
    let second = (time_nanos / NANOS_PER_SECOND) as u8;
    let nanosecond = (time_nanos % NANOS_PER_SECOND) as u32;
    Ok(DateTime {
        date: civil_from_days(day),
        time: Time {
            hour,
            minute,
            second,
            nanosecond,
        },
    })
}

// --- text parse helpers ---------------------------------------------------

fn parse_u32_digits(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn parse_signed(text: &str) -> Option<i32> {
    text.parse().ok()
}

fn parse_time(text: &str) -> Result<Time, TemporalError> {
    let (hms, frac) = match text.split_once('.') {
        Some((hms, frac)) => (hms, Some(frac)),
        None => (text, None),
    };
    let parts: Vec<&str> = hms.split(':').collect();
    if parts.len() != 3 {
        return Err(TemporalError::new(format!("invalid Time text `{text}`")));
    }
    let hour = parse_u32_digits(parts[0])
        .ok_or_else(|| TemporalError::new(format!("invalid hour in `{text}`")))?;
    let minute = parse_u32_digits(parts[1])
        .ok_or_else(|| TemporalError::new(format!("invalid minute in `{text}`")))?;
    let second = parse_u32_digits(parts[2])
        .ok_or_else(|| TemporalError::new(format!("invalid second in `{text}`")))?;
    let nanosecond = match frac {
        None => 0,
        Some(frac) if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) => {
            return Err(TemporalError::new(format!(
                "invalid fractional seconds in `{text}`"
            )));
        }
        Some(frac) => {
            let mut digits = frac.as_bytes().to_vec();
            if digits.len() > 9 {
                digits.truncate(9);
            }
            while digits.len() < 9 {
                digits.push(b'0');
            }
            std::str::from_utf8(&digits)
                .ok()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| {
                    TemporalError::new(format!("invalid fractional seconds in `{text}`"))
                })?
        }
    };
    Time::new(
        i64::from(hour),
        i64::from(minute),
        i64::from(second),
        i64::from(nanosecond),
    )
}

fn split_date_time(text: &str) -> Result<(&str, &str), TemporalError> {
    if let Some((date, time)) = text.split_once('T') {
        return Ok((date, time));
    }
    if let Some((date, time)) = text.split_once('t') {
        return Ok((date, time));
    }
    if let Some((date, time)) = text.split_once(' ') {
        return Ok((date, time));
    }
    Err(TemporalError::new(format!(
        "invalid DateTime text `{text}`"
    )))
}

fn split_instant(text: &str) -> Result<(&str, i64), TemporalError> {
    if let Some(body) = text.strip_suffix('Z').or_else(|| text.strip_suffix('z')) {
        return Ok((body, 0));
    }
    let bytes = text.as_bytes();
    // Find the offset sign after the date (skip year sign if present).
    let search_from = if bytes.first() == Some(&b'-') { 1 } else { 0 };
    let mut offset_at = None;
    for (idx, byte) in bytes.iter().enumerate().skip(search_from) {
        if (*byte == b'+' || *byte == b'-') && idx > 10 {
            offset_at = Some(idx);
        }
    }
    let Some(offset_at) = offset_at else {
        return Err(TemporalError::new(format!(
            "Instant requires a UTC offset or Z; got `{text}`"
        )));
    };
    let body = &text[..offset_at];
    let offset_text = &text[offset_at..];
    let offset_minutes = parse_offset(offset_text)?;
    Ok((body, offset_minutes))
}

fn parse_offset(text: &str) -> Result<i64, TemporalError> {
    let (sign, rest) = match text.as_bytes().first() {
        Some(b'+') => (1_i64, &text[1..]),
        Some(b'-') => (-1_i64, &text[1..]),
        _ => {
            return Err(TemporalError::new(format!("invalid offset `{text}`")));
        }
    };
    let (hours, minutes) = if let Some((h, m)) = rest.split_once(':') {
        (h, m)
    } else if rest.len() == 4 {
        (&rest[..2], &rest[2..])
    } else if rest.len() == 2 {
        (rest, "00")
    } else {
        return Err(TemporalError::new(format!("invalid offset `{text}`")));
    };
    let hours = parse_u32_digits(hours)
        .ok_or_else(|| TemporalError::new(format!("invalid offset hours in `{text}`")))?;
    let minutes = parse_u32_digits(minutes)
        .ok_or_else(|| TemporalError::new(format!("invalid offset minutes in `{text}`")))?;
    if hours > 23 || minutes > 59 {
        return Err(TemporalError::new(format!("invalid offset `{text}`")));
    }
    Ok(sign * (i64::from(hours) * 60 + i64::from(minutes)))
}

fn format_iso_duration(nanos: i64) -> String {
    if nanos == 0 {
        return "PT0S".to_owned();
    }
    let negative = nanos < 0;
    let mut remaining = nanos.unsigned_abs();
    let days = remaining / (NANOS_PER_DAY as u64);
    remaining %= NANOS_PER_DAY as u64;
    let hours = remaining / (NANOS_PER_HOUR as u64);
    remaining %= NANOS_PER_HOUR as u64;
    let minutes = remaining / (NANOS_PER_MINUTE as u64);
    remaining %= NANOS_PER_MINUTE as u64;
    let seconds = remaining / (NANOS_PER_SECOND as u64);
    let frac = remaining % (NANOS_PER_SECOND as u64);

    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push('P');
    if days > 0 {
        out.push_str(&format!("{days}D"));
    }
    if hours > 0 || minutes > 0 || seconds > 0 || frac > 0 || days == 0 {
        out.push('T');
        if hours > 0 {
            out.push_str(&format!("{hours}H"));
        }
        if minutes > 0 {
            out.push_str(&format!("{minutes}M"));
        }
        if seconds > 0 || frac > 0 || (hours == 0 && minutes == 0 && days == 0) {
            if frac == 0 {
                out.push_str(&format!("{seconds}S"));
            } else {
                let mut frac_text = format!("{frac:09}");
                while frac_text.ends_with('0') {
                    frac_text.pop();
                }
                out.push_str(&format!("{seconds}.{frac_text}S"));
            }
        }
    }
    out
}

fn parse_iso_duration(text: &str) -> Result<i64, TemporalError> {
    let mut s = text;
    let negative = if let Some(rest) = s.strip_prefix('-') {
        s = rest;
        true
    } else {
        false
    };
    let Some(s) = s.strip_prefix('P').or_else(|| s.strip_prefix('p')) else {
        return Err(TemporalError::new(format!(
            "invalid Duration text `{text}`"
        )));
    };
    if s.is_empty() {
        return Err(TemporalError::new(format!(
            "invalid Duration text `{text}`"
        )));
    }

    let (date_part, time_part) = match s.split_once('T').or_else(|| s.split_once('t')) {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };

    let mut total: i64 = 0;
    if !date_part.is_empty() {
        total = total
            .checked_add(parse_duration_date_part(date_part)?)
            .ok_or_else(|| TemporalError::new("Duration overflow"))?;
    }
    if let Some(time_part) = time_part {
        if time_part.is_empty() {
            return Err(TemporalError::new(format!(
                "invalid Duration text `{text}`"
            )));
        }
        total = total
            .checked_add(parse_duration_time_part(time_part)?)
            .ok_or_else(|| TemporalError::new("Duration overflow"))?;
    } else if date_part.is_empty() {
        return Err(TemporalError::new(format!(
            "invalid Duration text `{text}`"
        )));
    }

    Ok(if negative { -total } else { total })
}

fn parse_duration_date_part(text: &str) -> Result<i64, TemporalError> {
    // Only days in this slice (no months/years — deferred with calendar arithmetic).
    let mut rest = text;
    let mut total: i64 = 0;
    while !rest.is_empty() {
        let (number, unit, next) = take_duration_number(rest)?;
        rest = next;
        let add = match unit {
            b'D' | b'd' => number
                .checked_mul(NANOS_PER_DAY)
                .ok_or_else(|| TemporalError::new("Duration overflow"))?,
            other => {
                return Err(TemporalError::new(format!(
                    "unsupported Duration date unit `{}`",
                    other as char
                )));
            }
        };
        total = total
            .checked_add(add)
            .ok_or_else(|| TemporalError::new("Duration overflow"))?;
    }
    Ok(total)
}

fn parse_duration_time_part(text: &str) -> Result<i64, TemporalError> {
    let mut rest = text;
    let mut total: i64 = 0;
    while !rest.is_empty() {
        let (number_text, unit, next) = take_duration_token(rest)?;
        rest = next;
        let add = match unit {
            b'H' | b'h' => {
                let n = parse_duration_int(number_text)?;
                n.checked_mul(NANOS_PER_HOUR)
                    .ok_or_else(|| TemporalError::new("Duration overflow"))?
            }
            b'M' | b'm' => {
                let n = parse_duration_int(number_text)?;
                n.checked_mul(NANOS_PER_MINUTE)
                    .ok_or_else(|| TemporalError::new("Duration overflow"))?
            }
            b'S' | b's' => parse_duration_seconds(number_text)?,
            other => {
                return Err(TemporalError::new(format!(
                    "unsupported Duration time unit `{}`",
                    other as char
                )));
            }
        };
        total = total
            .checked_add(add)
            .ok_or_else(|| TemporalError::new("Duration overflow"))?;
    }
    Ok(total)
}

fn take_duration_number(text: &str) -> Result<(i64, u8, &str), TemporalError> {
    let (number_text, unit, rest) = take_duration_token(text)?;
    let number = parse_duration_int(number_text)?;
    Ok((number, unit, rest))
}

fn take_duration_token(text: &str) -> Result<(&str, u8, &str), TemporalError> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
        i += 1;
    }
    if i == 0 || i >= bytes.len() {
        return Err(TemporalError::new(format!(
            "invalid Duration component `{text}`"
        )));
    }
    Ok((&text[..i], bytes[i], &text[i + 1..]))
}

fn parse_duration_int(text: &str) -> Result<i64, TemporalError> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(TemporalError::new(format!(
            "invalid Duration number `{text}`"
        )));
    }
    text.parse()
        .map_err(|_| TemporalError::new(format!("invalid Duration number `{text}`")))
}

fn parse_duration_seconds(text: &str) -> Result<i64, TemporalError> {
    if let Some((whole, frac)) = text.split_once('.') {
        let whole = if whole.is_empty() {
            0
        } else {
            parse_duration_int(whole)?
        };
        if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
            return Err(TemporalError::new(format!(
                "invalid Duration seconds `{text}`"
            )));
        }
        let mut digits = frac.as_bytes().to_vec();
        if digits.len() > 9 {
            digits.truncate(9);
        }
        while digits.len() < 9 {
            digits.push(b'0');
        }
        let nanos: i64 = std::str::from_utf8(&digits)
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| TemporalError::new(format!("invalid Duration seconds `{text}`")))?;
        whole
            .checked_mul(NANOS_PER_SECOND)
            .and_then(|s| s.checked_add(nanos))
            .ok_or_else(|| TemporalError::new("Duration overflow"))
    } else {
        parse_duration_int(text)?
            .checked_mul(NANOS_PER_SECOND)
            .ok_or_else(|| TemporalError::new("Duration overflow"))
    }
}

// --- Aven values ----------------------------------------------------------

fn text_error(message: impl Into<String>) -> Value {
    Value::Text(message.into())
}

fn compare_int(order: Ordering) -> Value {
    Value::Int(match order {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    })
}

fn kind_field(kind: &str) -> (String, Value) {
    (TEMPORAL_KIND_FIELD.to_owned(), Value::Text(kind.to_owned()))
}

fn record_field<'a>(fields: &'a [(String, Value)], name: &str) -> Option<&'a Value> {
    fields
        .iter()
        .find_map(|(field, value)| (field == name).then_some(value))
}

pub(crate) fn temporal_kind(value: &Value) -> Option<&str> {
    let Value::Record(fields) = value else {
        return None;
    };
    match record_field(fields, TEMPORAL_KIND_FIELD)? {
        Value::Text(kind) => Some(kind.as_str()),
        _ => None,
    }
}

pub(crate) fn date_from_value(value: &Value) -> Option<Date> {
    if temporal_kind(value) != Some("Date") {
        return None;
    }
    let Value::Record(fields) = value else {
        return None;
    };
    let year = match record_field(fields, "year")? {
        Value::Int(v) => *v as i32,
        _ => return None,
    };
    let month = match record_field(fields, "month")? {
        Value::Int(v) => u8::try_from(*v).ok()?,
        _ => return None,
    };
    let day = match record_field(fields, "day")? {
        Value::Int(v) => u8::try_from(*v).ok()?,
        _ => return None,
    };
    Some(Date { year, month, day })
}

pub(crate) fn time_from_value(value: &Value) -> Option<Time> {
    if temporal_kind(value) != Some("Time") {
        return None;
    }
    let Value::Record(fields) = value else {
        return None;
    };
    let hour = match record_field(fields, "hour")? {
        Value::Int(v) => u8::try_from(*v).ok()?,
        _ => return None,
    };
    let minute = match record_field(fields, "minute")? {
        Value::Int(v) => u8::try_from(*v).ok()?,
        _ => return None,
    };
    let second = match record_field(fields, "second")? {
        Value::Int(v) => u8::try_from(*v).ok()?,
        _ => return None,
    };
    let nanosecond = match record_field(fields, "nanosecond") {
        Some(Value::Int(v)) => u32::try_from(*v).ok()?,
        None => 0,
        _ => return None,
    };
    Some(Time {
        hour,
        minute,
        second,
        nanosecond,
    })
}

pub(crate) fn datetime_from_value(value: &Value) -> Option<DateTime> {
    if temporal_kind(value) != Some("DateTime") {
        return None;
    }
    let Value::Record(fields) = value else {
        return None;
    };
    let date = date_from_value(record_field(fields, "date")?)?;
    let time = time_from_value(record_field(fields, "time")?)?;
    Some(DateTime { date, time })
}

pub(crate) fn instant_from_value(value: &Value) -> Option<Instant> {
    if temporal_kind(value) != Some("Instant") {
        return None;
    }
    let Value::Record(fields) = value else {
        return None;
    };
    match record_field(fields, "nanos")? {
        Value::Int(nanos) => Some(Instant { nanos: *nanos }),
        _ => None,
    }
}

pub(crate) fn duration_from_value(value: &Value) -> Option<Duration> {
    if temporal_kind(value) != Some("Duration") {
        return None;
    }
    let Value::Record(fields) = value else {
        return None;
    };
    match record_field(fields, "nanos")? {
        Value::Int(nanos) => Some(Duration { nanos: *nanos }),
        _ => None,
    }
}

pub(crate) fn format_temporal_from_value(value: &Value) -> Option<FormatTemporal> {
    match temporal_kind(value)? {
        "Instant" => instant_from_value(value).map(FormatTemporal::Instant),
        "DateTime" => datetime_from_value(value).map(FormatTemporal::DateTime),
        "Date" => date_from_value(value).map(FormatTemporal::Date),
        "Time" => time_from_value(value).map(FormatTemporal::Time),
        // Duration is not a TOML native kind; emit as ISO text at the codec edge.
        "Duration" => None,
        _ => None,
    }
}

pub(crate) fn temporal_iso_text(value: &Value) -> Option<String> {
    match temporal_kind(value)? {
        "Instant" => instant_from_value(value).map(Instant::format),
        "DateTime" => datetime_from_value(value).map(DateTime::format),
        "Date" => date_from_value(value).map(Date::format),
        "Time" => time_from_value(value).map(Time::format),
        "Duration" => duration_from_value(value).map(Duration::format),
        _ => None,
    }
}

pub(crate) fn date_value(date: Date) -> Value {
    let year = date.year;
    let month = date.month;
    let day = date.day;
    Value::record(vec![
        kind_field("Date"),
        ("year".to_owned(), Value::Int(i64::from(year))),
        ("month".to_owned(), Value::Int(i64::from(month))),
        ("day".to_owned(), Value::Int(i64::from(day))),
        (
            "format".to_owned(),
            Value::native(move |args| {
                if !args.is_empty() {
                    return Err(format!(
                        "Date.format expects 0 arguments, got {}",
                        args.len()
                    ));
                }
                Ok(Value::Text(Date { year, month, day }.format()))
            }),
        ),
    ])
}

pub(crate) fn time_value(time: Time) -> Value {
    let hour = time.hour;
    let minute = time.minute;
    let second = time.second;
    let nanosecond = time.nanosecond;
    Value::record(vec![
        kind_field("Time"),
        ("hour".to_owned(), Value::Int(i64::from(hour))),
        ("minute".to_owned(), Value::Int(i64::from(minute))),
        ("second".to_owned(), Value::Int(i64::from(second))),
        ("nanosecond".to_owned(), Value::Int(i64::from(nanosecond))),
        (
            "format".to_owned(),
            Value::native(move |args| {
                if !args.is_empty() {
                    return Err(format!(
                        "Time.format expects 0 arguments, got {}",
                        args.len()
                    ));
                }
                Ok(Value::Text(
                    Time {
                        hour,
                        minute,
                        second,
                        nanosecond,
                    }
                    .format(),
                ))
            }),
        ),
    ])
}

pub(crate) fn datetime_value(datetime: DateTime) -> Value {
    let date = datetime.date;
    let time = datetime.time;
    Value::record(vec![
        kind_field("DateTime"),
        ("date".to_owned(), date_value(date)),
        ("time".to_owned(), time_value(time)),
        (
            "format".to_owned(),
            Value::native(move |args| {
                if !args.is_empty() {
                    return Err(format!(
                        "DateTime.format expects 0 arguments, got {}",
                        args.len()
                    ));
                }
                Ok(Value::Text(DateTime { date, time }.format()))
            }),
        ),
        (
            "instant".to_owned(),
            Value::native(move |args| {
                let [Value::Int(offset)] = args else {
                    return Err(format!(
                        "DateTime.instant expects 1 Int argument, got {}",
                        args.len()
                    ));
                };
                match (DateTime { date, time }).instant(*offset) {
                    Ok(instant) => Ok(instant_value(instant)),
                    Err(error) => Err(error.message().to_owned()),
                }
            }),
        ),
    ])
}

pub(crate) fn instant_value(instant: Instant) -> Value {
    let nanos = instant.nanos;
    Value::record(vec![
        kind_field("Instant"),
        ("nanos".to_owned(), Value::Int(nanos)),
        (
            "format".to_owned(),
            Value::native(move |args| {
                if !args.is_empty() {
                    return Err(format!(
                        "Instant.format expects 0 arguments, got {}",
                        args.len()
                    ));
                }
                Ok(Value::Text(Instant { nanos }.format()))
            }),
        ),
        (
            "dateTime".to_owned(),
            Value::native(move |args| {
                let [Value::Int(offset)] = args else {
                    return Err(format!(
                        "Instant.dateTime expects 1 Int argument, got {}",
                        args.len()
                    ));
                };
                match Instant::from_nanos(nanos).date_time(*offset) {
                    Ok(datetime) => Ok(datetime_value(datetime)),
                    Err(error) => Err(error.message().to_owned()),
                }
            }),
        ),
    ])
}

pub(crate) fn duration_value(duration: Duration) -> Value {
    let nanos = duration.nanos;
    Value::record(vec![
        kind_field("Duration"),
        ("nanos".to_owned(), Value::Int(nanos)),
        (
            "format".to_owned(),
            Value::native(move |args| {
                if !args.is_empty() {
                    return Err(format!(
                        "Duration.format expects 0 arguments, got {}",
                        args.len()
                    ));
                }
                Ok(Value::Text(Duration { nanos }.format()))
            }),
        ),
    ])
}

// --- types ----------------------------------------------------------------

fn date_value_type() -> Type {
    crate::build::record(vec![
        ("year", crate::build::int()),
        ("month", crate::build::int()),
        ("day", crate::build::int()),
        (
            "format",
            crate::build::function(vec![], crate::build::text()),
        ),
    ])
}

fn time_value_type() -> Type {
    crate::build::record(vec![
        ("hour", crate::build::int()),
        ("minute", crate::build::int()),
        ("second", crate::build::int()),
        ("nanosecond", crate::build::int()),
        (
            "format",
            crate::build::function(vec![], crate::build::text()),
        ),
    ])
}

fn datetime_value_type() -> Type {
    crate::build::record(vec![
        ("date", crate::build::named("Date")),
        ("time", crate::build::named("Time")),
        (
            "format",
            crate::build::function(vec![], crate::build::text()),
        ),
        (
            "instant",
            crate::build::function(vec![crate::build::int()], crate::build::named("Instant")),
        ),
    ])
}

fn instant_value_type() -> Type {
    crate::build::record(vec![
        (
            "format",
            crate::build::function(vec![], crate::build::text()),
        ),
        (
            "dateTime",
            crate::build::function(vec![crate::build::int()], crate::build::named("DateTime")),
        ),
    ])
}

fn duration_value_type() -> Type {
    crate::build::record(vec![(
        "format",
        crate::build::function(vec![], crate::build::text()),
    )])
}

fn parse_fn_type(ok: Type) -> Type {
    crate::build::function(
        vec![crate::build::text()],
        crate::build::result(ok, crate::build::text()),
    )
}

fn compare_fn_type(ty: Type) -> Type {
    crate::build::function(vec![ty.clone(), ty], crate::build::int())
}

pub(crate) fn date_statics() -> Vec<(String, Type)> {
    vec![
        (
            "new".to_owned(),
            crate::build::function(
                vec![
                    crate::build::int(),
                    crate::build::int(),
                    crate::build::int(),
                ],
                crate::build::result(crate::build::named("Date"), crate::build::text()),
            ),
        ),
        (
            "parse".to_owned(),
            parse_fn_type(crate::build::named("Date")),
        ),
        (
            "compare".to_owned(),
            compare_fn_type(crate::build::named("Date")),
        ),
    ]
}

pub(crate) fn time_statics() -> Vec<(String, Type)> {
    vec![
        (
            "new".to_owned(),
            crate::build::function(
                vec![
                    crate::build::int(),
                    crate::build::int(),
                    crate::build::int(),
                ],
                crate::build::result(crate::build::named("Time"), crate::build::text()),
            ),
        ),
        (
            "parse".to_owned(),
            parse_fn_type(crate::build::named("Time")),
        ),
        (
            "compare".to_owned(),
            compare_fn_type(crate::build::named("Time")),
        ),
    ]
}

pub(crate) fn datetime_statics() -> Vec<(String, Type)> {
    vec![
        (
            "of".to_owned(),
            crate::build::function(
                vec![crate::build::named("Date"), crate::build::named("Time")],
                crate::build::named("DateTime"),
            ),
        ),
        (
            "parse".to_owned(),
            parse_fn_type(crate::build::named("DateTime")),
        ),
        (
            "compare".to_owned(),
            compare_fn_type(crate::build::named("DateTime")),
        ),
    ]
}

pub(crate) fn instant_statics() -> Vec<(String, Type)> {
    vec![
        (
            "parse".to_owned(),
            parse_fn_type(crate::build::named("Instant")),
        ),
        (
            "compare".to_owned(),
            compare_fn_type(crate::build::named("Instant")),
        ),
    ]
}

pub(crate) fn duration_statics() -> Vec<(String, Type)> {
    vec![
        (
            "ofSeconds".to_owned(),
            crate::build::function(
                vec![crate::build::int()],
                crate::build::result(crate::build::named("Duration"), crate::build::text()),
            ),
        ),
        (
            "parse".to_owned(),
            parse_fn_type(crate::build::named("Duration")),
        ),
        (
            "compare".to_owned(),
            compare_fn_type(crate::build::named("Duration")),
        ),
    ]
}

// --- natives --------------------------------------------------------------

fn date_new_native() -> Value {
    Value::native(|args| {
        let [Value::Int(y), Value::Int(m), Value::Int(d)] = args else {
            return Err(format!(
                "Date.new expects 3 Int arguments, got {}",
                args.len()
            ));
        };
        Ok(match Date::new(*y as i32, *m, *d) {
            Ok(date) => ok_value(date_value(date)),
            Err(error) => err_value(text_error(error.message())),
        })
    })
}

fn date_parse_native() -> Value {
    Value::native(|args| {
        let [Value::Text(text)] = args else {
            return Err(format!(
                "Date.parse expects 1 Text argument, got {}",
                args.len()
            ));
        };
        Ok(match Date::parse(text) {
            Ok(date) => ok_value(date_value(date)),
            Err(error) => err_value(text_error(error.message())),
        })
    })
}

fn date_compare_native() -> Value {
    Value::native(|args| {
        if args.len() != 2 {
            return Err(format!(
                "Date.compare expects 2 arguments, got {}",
                args.len()
            ));
        }
        let left =
            date_from_value(&args[0]).ok_or_else(|| "Date.compare expected Date".to_owned())?;
        let right =
            date_from_value(&args[1]).ok_or_else(|| "Date.compare expected Date".to_owned())?;
        Ok(compare_int(left.cmp(&right)))
    })
}

fn time_new_native() -> Value {
    Value::native(|args| {
        let [Value::Int(h), Value::Int(m), Value::Int(s)] = args else {
            return Err(format!(
                "Time.new expects 3 Int arguments, got {}",
                args.len()
            ));
        };
        Ok(match Time::new(*h, *m, *s, 0) {
            Ok(time) => ok_value(time_value(time)),
            Err(error) => err_value(text_error(error.message())),
        })
    })
}

fn time_parse_native() -> Value {
    Value::native(|args| {
        let [Value::Text(text)] = args else {
            return Err(format!(
                "Time.parse expects 1 Text argument, got {}",
                args.len()
            ));
        };
        Ok(match Time::parse(text) {
            Ok(time) => ok_value(time_value(time)),
            Err(error) => err_value(text_error(error.message())),
        })
    })
}

fn time_compare_native() -> Value {
    Value::native(|args| {
        if args.len() != 2 {
            return Err(format!(
                "Time.compare expects 2 arguments, got {}",
                args.len()
            ));
        }
        let left =
            time_from_value(&args[0]).ok_or_else(|| "Time.compare expected Time".to_owned())?;
        let right =
            time_from_value(&args[1]).ok_or_else(|| "Time.compare expected Time".to_owned())?;
        Ok(compare_int(left.cmp(&right)))
    })
}

fn datetime_of_native() -> Value {
    Value::native(|args| {
        if args.len() != 2 {
            return Err(format!(
                "DateTime.of expects 2 arguments, got {}",
                args.len()
            ));
        }
        let date =
            date_from_value(&args[0]).ok_or_else(|| "DateTime.of expected Date".to_owned())?;
        let time =
            time_from_value(&args[1]).ok_or_else(|| "DateTime.of expected Time".to_owned())?;
        Ok(datetime_value(DateTime::of(date, time)))
    })
}

fn datetime_parse_native() -> Value {
    Value::native(|args| {
        let [Value::Text(text)] = args else {
            return Err(format!(
                "DateTime.parse expects 1 Text argument, got {}",
                args.len()
            ));
        };
        Ok(match DateTime::parse(text) {
            Ok(datetime) => ok_value(datetime_value(datetime)),
            Err(error) => err_value(text_error(error.message())),
        })
    })
}

fn datetime_compare_native() -> Value {
    Value::native(|args| {
        if args.len() != 2 {
            return Err(format!(
                "DateTime.compare expects 2 arguments, got {}",
                args.len()
            ));
        }
        let left = datetime_from_value(&args[0])
            .ok_or_else(|| "DateTime.compare expected DateTime".to_owned())?;
        let right = datetime_from_value(&args[1])
            .ok_or_else(|| "DateTime.compare expected DateTime".to_owned())?;
        Ok(compare_int(left.cmp(&right)))
    })
}

fn instant_parse_native() -> Value {
    Value::native(|args| {
        let [Value::Text(text)] = args else {
            return Err(format!(
                "Instant.parse expects 1 Text argument, got {}",
                args.len()
            ));
        };
        Ok(match Instant::parse(text) {
            Ok(instant) => ok_value(instant_value(instant)),
            Err(error) => err_value(text_error(error.message())),
        })
    })
}

fn instant_compare_native() -> Value {
    Value::native(|args| {
        if args.len() != 2 {
            return Err(format!(
                "Instant.compare expects 2 arguments, got {}",
                args.len()
            ));
        }
        let left = instant_from_value(&args[0])
            .ok_or_else(|| "Instant.compare expected Instant".to_owned())?;
        let right = instant_from_value(&args[1])
            .ok_or_else(|| "Instant.compare expected Instant".to_owned())?;
        Ok(compare_int(left.cmp(&right)))
    })
}

fn duration_of_seconds_native() -> Value {
    Value::native(|args| {
        let [Value::Int(seconds)] = args else {
            return Err(format!(
                "Duration.ofSeconds expects 1 Int argument, got {}",
                args.len()
            ));
        };
        Ok(match Duration::of_seconds(*seconds) {
            Ok(duration) => ok_value(duration_value(duration)),
            Err(error) => err_value(text_error(error.message())),
        })
    })
}

fn duration_parse_native() -> Value {
    Value::native(|args| {
        let [Value::Text(text)] = args else {
            return Err(format!(
                "Duration.parse expects 1 Text argument, got {}",
                args.len()
            ));
        };
        Ok(match Duration::parse(text) {
            Ok(duration) => ok_value(duration_value(duration)),
            Err(error) => err_value(text_error(error.message())),
        })
    })
}

fn duration_compare_native() -> Value {
    Value::native(|args| {
        if args.len() != 2 {
            return Err(format!(
                "Duration.compare expects 2 arguments, got {}",
                args.len()
            ));
        }
        let left = duration_from_value(&args[0])
            .ok_or_else(|| "Duration.compare expected Duration".to_owned())?;
        let right = duration_from_value(&args[1])
            .ok_or_else(|| "Duration.compare expected Duration".to_owned())?;
        Ok(compare_int(left.cmp(&right)))
    })
}

// --- registration ---------------------------------------------------------

impl Host {
    /// Register the five temporal types and their companion statics.
    pub fn register_temporals(&mut self) {
        self.register_type_with_statics(
            "Date",
            date_value_type(),
            vec![
                (
                    "new".to_owned(),
                    date_statics()[0].1.clone(),
                    date_new_native(),
                ),
                (
                    "parse".to_owned(),
                    date_statics()[1].1.clone(),
                    date_parse_native(),
                ),
                (
                    "compare".to_owned(),
                    date_statics()[2].1.clone(),
                    date_compare_native(),
                ),
            ],
        );
        self.register_type_with_statics(
            "Time",
            time_value_type(),
            vec![
                (
                    "new".to_owned(),
                    time_statics()[0].1.clone(),
                    time_new_native(),
                ),
                (
                    "parse".to_owned(),
                    time_statics()[1].1.clone(),
                    time_parse_native(),
                ),
                (
                    "compare".to_owned(),
                    time_statics()[2].1.clone(),
                    time_compare_native(),
                ),
            ],
        );
        self.register_type_with_statics(
            "DateTime",
            datetime_value_type(),
            vec![
                (
                    "of".to_owned(),
                    datetime_statics()[0].1.clone(),
                    datetime_of_native(),
                ),
                (
                    "parse".to_owned(),
                    datetime_statics()[1].1.clone(),
                    datetime_parse_native(),
                ),
                (
                    "compare".to_owned(),
                    datetime_statics()[2].1.clone(),
                    datetime_compare_native(),
                ),
            ],
        );
        self.register_type_with_statics(
            "Instant",
            instant_value_type(),
            vec![
                (
                    "parse".to_owned(),
                    instant_statics()[0].1.clone(),
                    instant_parse_native(),
                ),
                (
                    "compare".to_owned(),
                    instant_statics()[1].1.clone(),
                    instant_compare_native(),
                ),
            ],
        );
        self.register_type_with_statics(
            "Duration",
            duration_value_type(),
            vec![
                (
                    "ofSeconds".to_owned(),
                    duration_statics()[0].1.clone(),
                    duration_of_seconds_native(),
                ),
                (
                    "parse".to_owned(),
                    duration_statics()[1].1.clone(),
                    duration_parse_native(),
                ),
                (
                    "compare".to_owned(),
                    duration_statics()[2].1.clone(),
                    duration_compare_native(),
                ),
            ],
        );
    }
}

pub(crate) fn temporal_type_definitions() -> Vec<(String, Type)> {
    vec![
        ("Date".to_owned(), date_value_type()),
        ("Time".to_owned(), time_value_type()),
        ("DateTime".to_owned(), datetime_value_type()),
        ("Instant".to_owned(), instant_value_type()),
        ("Duration".to_owned(), duration_value_type()),
    ]
}

pub(crate) fn temporal_statics_table() -> Vec<(String, Vec<(String, Type)>)> {
    vec![
        ("Date".to_owned(), date_statics()),
        ("Time".to_owned(), time_statics()),
        ("DateTime".to_owned(), datetime_statics()),
        ("Instant".to_owned(), instant_statics()),
        ("Duration".to_owned(), duration_statics()),
    ]
}

/// Convert a TOML crate datetime into the host-internal temporal arm.
pub(crate) fn format_temporal_from_toml(
    dt: &::toml::value::Datetime,
) -> Result<FormatTemporal, String> {
    match (&dt.date, &dt.time, &dt.offset) {
        (Some(date), Some(time), Some(offset)) => {
            let date = Date::new(
                i32::from(date.year),
                i64::from(date.month),
                i64::from(date.day),
            )
            .map_err(|e| e.message().to_owned())?;
            let time = Time::new(
                i64::from(time.hour),
                i64::from(time.minute),
                i64::from(time.second.unwrap_or(0)),
                i64::from(time.nanosecond.unwrap_or(0)),
            )
            .map_err(|e| e.message().to_owned())?;
            let offset_minutes = match offset {
                ::toml::value::Offset::Z => 0,
                ::toml::value::Offset::Custom { minutes } => i64::from(*minutes),
            };
            Instant::from_datetime(DateTime { date, time }, offset_minutes)
                .map(FormatTemporal::Instant)
                .map_err(|e| e.message().to_owned())
        }
        (Some(date), Some(time), None) => {
            let date = Date::new(
                i32::from(date.year),
                i64::from(date.month),
                i64::from(date.day),
            )
            .map_err(|e| e.message().to_owned())?;
            let time = Time::new(
                i64::from(time.hour),
                i64::from(time.minute),
                i64::from(time.second.unwrap_or(0)),
                i64::from(time.nanosecond.unwrap_or(0)),
            )
            .map_err(|e| e.message().to_owned())?;
            Ok(FormatTemporal::DateTime(DateTime { date, time }))
        }
        (Some(date), None, None) => {
            let date = Date::new(
                i32::from(date.year),
                i64::from(date.month),
                i64::from(date.day),
            )
            .map_err(|e| e.message().to_owned())?;
            Ok(FormatTemporal::Date(date))
        }
        (None, Some(time), None) => {
            let time = Time::new(
                i64::from(time.hour),
                i64::from(time.minute),
                i64::from(time.second.unwrap_or(0)),
                i64::from(time.nanosecond.unwrap_or(0)),
            )
            .map_err(|e| e.message().to_owned())?;
            Ok(FormatTemporal::Time(time))
        }
        _ => Err(format!("unsupported TOML datetime shape: {dt}")),
    }
}

pub(crate) fn toml_datetime_from_format_temporal(
    temporal: FormatTemporal,
) -> Result<::toml::value::Datetime, String> {
    match temporal {
        FormatTemporal::Instant(instant) => {
            let dt = instant.date_time(0).map_err(|e| e.message().to_owned())?;
            Ok(::toml::value::Datetime {
                date: Some(::toml::value::Date {
                    year: u16::try_from(dt.date.year)
                        .map_err(|_| "year out of TOML range".to_owned())?,
                    month: dt.date.month,
                    day: dt.date.day,
                }),
                time: Some(::toml::value::Time {
                    hour: dt.time.hour,
                    minute: dt.time.minute,
                    second: Some(dt.time.second),
                    nanosecond: if dt.time.nanosecond == 0 {
                        None
                    } else {
                        Some(dt.time.nanosecond)
                    },
                }),
                offset: Some(::toml::value::Offset::Z),
            })
        }
        FormatTemporal::DateTime(dt) => Ok(::toml::value::Datetime {
            date: Some(::toml::value::Date {
                year: u16::try_from(dt.date.year)
                    .map_err(|_| "year out of TOML range".to_owned())?,
                month: dt.date.month,
                day: dt.date.day,
            }),
            time: Some(::toml::value::Time {
                hour: dt.time.hour,
                minute: dt.time.minute,
                second: Some(dt.time.second),
                nanosecond: if dt.time.nanosecond == 0 {
                    None
                } else {
                    Some(dt.time.nanosecond)
                },
            }),
            offset: None,
        }),
        FormatTemporal::Date(date) => Ok(::toml::value::Datetime {
            date: Some(::toml::value::Date {
                year: u16::try_from(date.year).map_err(|_| "year out of TOML range".to_owned())?,
                month: date.month,
                day: date.day,
            }),
            time: None,
            offset: None,
        }),
        FormatTemporal::Time(time) => Ok(::toml::value::Datetime {
            date: None,
            time: Some(::toml::value::Time {
                hour: time.hour,
                minute: time.minute,
                second: Some(time.second),
                nanosecond: if time.nanosecond == 0 {
                    None
                } else {
                    Some(time.nanosecond)
                },
            }),
            offset: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use aven_parser::parse_module;

    fn temporal_host() -> Host {
        let mut host = Host::new();
        host.register_temporals();
        host
    }

    fn run(source: &str) -> Value {
        let parsed = parse_module(source);
        assert!(
            parsed.diagnostics.is_empty(),
            "program parses: {:?}",
            parsed.diagnostics
        );
        let outcome =
            aven_eval::eval_module_with_globals(&parsed.module, temporal_host().eval_globals());
        assert!(
            outcome.diagnostics.is_empty(),
            "program runs: {:?}",
            outcome.diagnostics
        );
        outcome
            .value
            .unwrap_or_else(|| panic!("program yields a value"))
    }

    fn field<'a>(value: &'a Value, name: &str) -> &'a Value {
        let Value::Record(fields) = value else {
            panic!("expected a record, got {value:?}");
        };
        fields
            .iter()
            .find_map(|(field_name, field_value)| (field_name == name).then_some(field_value))
            .unwrap_or_else(|| panic!("record has field `{name}`"))
    }

    fn text(value: &Value) -> &str {
        let Value::Text(text) = value else {
            panic!("expected Text, got {value:?}");
        };
        text
    }

    fn ok_payload(value: &Value) -> &Value {
        let Value::Tag { name, payload } = value else {
            panic!("expected Result tag, got {value:?}");
        };
        assert_eq!(name, "Ok", "expected Ok, got {value:?}");
        payload.first().expect("Ok payload")
    }

    fn err_text(value: &Value) -> &str {
        let Value::Tag { name, payload } = value else {
            panic!("expected Result tag, got {value:?}");
        };
        assert_eq!(name, "Err", "expected Err, got {value:?}");
        text(payload.first().expect("Err payload"))
    }

    #[test]
    fn instant_offset_normalizes_to_utc() {
        let instant = Instant::parse("1979-05-27T09:00:00+10:00").expect("parses");
        assert_eq!(instant.format(), "1979-05-26T23:00:00Z");
    }

    #[test]
    fn parse_format_round_trips_all_five() {
        assert_eq!(
            Instant::parse("2026-07-11T09:30:00Z")
                .expect("instant")
                .format(),
            "2026-07-11T09:30:00Z"
        );
        assert_eq!(
            Date::parse("2026-07-11").expect("date").format(),
            "2026-07-11"
        );
        assert_eq!(Time::parse("09:30:00").expect("time").format(), "09:30:00");
        assert_eq!(
            Time::parse("09:30:00.5").expect("frac time").format(),
            "09:30:00.5"
        );
        assert_eq!(
            DateTime::parse("2026-07-11T09:30:00")
                .expect("datetime")
                .format(),
            "2026-07-11T09:30:00"
        );
        assert_eq!(Duration::parse("PT90M").expect("pt90m").format(), "PT1H30M");
        assert_eq!(
            Duration::parse("PT1H30M").expect("pt1h30m").format(),
            "PT1H30M"
        );
    }

    #[test]
    fn parse_rejects_garbage_and_invalid_components() {
        assert!(Instant::parse("not-a-date").is_err());
        assert!(Date::parse("2026-13-01").is_err());
        assert!(Date::parse("2026-02-30").is_err());
        assert!(Time::parse("24:00:00").is_err());
        assert!(DateTime::parse("2026-07-11").is_err());
        assert!(Duration::parse("90 minutes").is_err());
        // Instant without offset is rejected (no silent UTC).
        assert!(Instant::parse("2026-07-11T09:30:00").is_err());
    }

    #[test]
    fn date_rejects_month_13_and_feb_30() {
        assert!(Date::new(2026, 13, 1).is_err());
        assert!(Date::new(2026, 2, 30).is_err());
        assert!(Date::new(2024, 2, 29).is_ok());
        assert!(Date::new(2025, 2, 29).is_err());
    }

    #[test]
    fn time_rejects_hour_24() {
        assert!(Time::new(24, 0, 0, 0).is_err());
        assert!(Time::new(23, 59, 59, 0).is_ok());
    }

    #[test]
    fn duration_pt90m_round_trips() {
        let duration = Duration::parse("PT90M").expect("parses");
        assert_eq!(duration.nanos, 90 * NANOS_PER_MINUTE);
        assert_eq!(duration.format(), "PT1H30M");
    }

    #[test]
    fn fixed_offset_conversion_both_ways() {
        let instant = Instant::parse("2026-07-11T12:00:00Z").expect("parses");
        let local = instant.date_time(600).expect("offset");
        assert_eq!(local.format(), "2026-07-11T22:00:00");
        let back = local.instant(600).expect("anchor");
        assert_eq!(back.nanos, instant.nanos);
    }

    #[test]
    fn compare_orders() {
        let a = Date::parse("2020-01-01").expect("date a");
        let b = Date::parse("2020-01-02").expect("date b");
        assert_eq!(a.cmp(&b), Ordering::Less);
        let d1 = Duration::of_seconds(1).expect("1s");
        let d2 = Duration::of_seconds(2).expect("2s");
        assert_eq!(d1.cmp(&d2), Ordering::Less);
        let i1 = Instant::parse("2020-01-01T00:00:00Z").expect("i1");
        let i2 = Instant::parse("2020-01-01T00:00:01Z").expect("i2");
        assert_eq!(i1.cmp(&i2), Ordering::Less);
    }

    #[test]
    fn host_constructors_return_error_values() {
        let bad_month = run("Date.new(2026, 13, 1)\n");
        assert!(err_text(&bad_month).contains("month"));

        let bad_day = run("Date.new(2026, 2, 30)\n");
        assert!(err_text(&bad_day).contains("day"));

        let bad_hour = run("Time.new(24, 0, 0)\n");
        assert!(err_text(&bad_hour).contains("hour"));

        let ok = run("Date.new(2026, 7, 11)\n");
        let date = ok_payload(&ok);
        assert_eq!(field(date, "year"), &Value::Int(2026));
        assert_eq!(field(date, "month"), &Value::Int(7));
        assert_eq!(field(date, "day"), &Value::Int(11));
    }

    #[test]
    fn host_parse_format_and_compare() {
        let value = run("i = Instant.parse(\"1979-05-27T09:00:00+10:00\")?!\n\
             d = Date.parse(\"1979-05-27\")?!\n\
             t = Time.parse(\"07:32:00\")?!\n\
             dt = DateTime.of(d, t)\n\
             dur = Duration.ofSeconds(90)?!\n\
             {\n\
               instant: i.format(),\n\
               date: d.format(),\n\
               time: t.format(),\n\
               datetime: dt.format(),\n\
               duration: dur.format(),\n\
               dateCmp: Date.compare(d, Date.parse(\"1979-05-28\")?!),\n\
               instantCmp: Instant.compare(i, Instant.parse(\"1979-05-26T23:00:00Z\")?!),\n\
               local: i.dateTime(0).format(),\n\
               back: dt.instant(0).format()\n\
             }\n");

        assert_eq!(text(field(&value, "instant")), "1979-05-26T23:00:00Z");
        assert_eq!(text(field(&value, "date")), "1979-05-27");
        assert_eq!(text(field(&value, "time")), "07:32:00");
        assert_eq!(text(field(&value, "datetime")), "1979-05-27T07:32:00");
        assert_eq!(text(field(&value, "duration")), "PT1M30S");
        assert_eq!(field(&value, "dateCmp"), &Value::Int(-1));
        assert_eq!(field(&value, "instantCmp"), &Value::Int(0));
        assert_eq!(text(field(&value, "local")), "1979-05-26T23:00:00");
        assert_eq!(text(field(&value, "back")), "1979-05-27T07:32:00Z");
    }

    #[test]
    fn duration_of_seconds_and_parse() {
        let value = run("a = Duration.ofSeconds(5400)?!\n\
             b = Duration.parse(\"PT90M\")?!\n\
             {\n\
               a: a.format(),\n\
               b: b.format(),\n\
               cmp: Duration.compare(a, b)\n\
             }\n");
        assert_eq!(text(field(&value, "a")), "PT1H30M");
        assert_eq!(text(field(&value, "b")), "PT1H30M");
        assert_eq!(field(&value, "cmp"), &Value::Int(0));
    }
}

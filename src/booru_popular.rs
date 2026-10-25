//! Popular metadata keeps normalized display keywords separate from API input.
use super::*;
use time::{Date, Duration, Month, Weekday};

pub(super) fn posts(bytes: &[u8]) -> Result<Vec<Value>> {
    let invalid = || error("invalid booru popular response");
    let body: Value = serde_json::from_slice(bytes).map_err(|_| invalid())?;
    let body = match body {
        Value::Object(mut fields) => fields.remove("posts").ok_or_else(invalid)?,
        value => value,
    };
    match body {
        Value::Array(values) => Ok(values),
        Value::Object(values) if values.is_empty() => Ok(Vec::new()),
        Value::String(value) if value.is_empty() => Ok(Vec::new()),
        _ => Err(invalid()),
    }
}

pub(super) fn capture(site: Site, url: &Url) -> Option<Vec<(String, String)>> {
    if !site.danbooru_dialect() {
        return None;
    }
    let prefix = if url.path().starts_with("/explore/posts/popular") {
        "/explore/posts/popular"
    } else if site.dialect() == Dialect::Danbooru && url.path().starts_with("/popular") {
        "/popular"
    } else {
        return None;
    };
    let mut seen = std::collections::BTreeSet::new();
    Some(if url.path() == prefix {
        url.query()
            .unwrap_or_default()
            .split('&')
            .filter(|pair| pair.contains('='))
            .filter_map(|pair| url::form_urlencoded::parse(pair.as_bytes()).next())
            .filter_map(|(key, value)| {
                seen.insert(key.clone().into_owned())
                    .then(|| (key.into_owned(), value.into_owned()))
            })
            .collect()
    } else {
        Vec::new()
    })
}
fn invalid() -> crate::Error {
    error("invalid booru popular week date")
}
fn iso_date(value: &str) -> Result<Date> {
    if !value.is_ascii() {
        return Err(invalid());
    }
    let parse = |value: &str| {
        if !value.bytes().all(|c| c.is_ascii_digit()) {
            return Err(invalid());
        }
        value.parse::<u8>().map_err(|_| invalid())
    };
    if !value
        .get(..4)
        .is_some_and(|s| s.bytes().all(|c| c.is_ascii_digit()))
    {
        return Err(invalid());
    }
    let year = value
        .get(..4)
        .ok_or_else(invalid)?
        .parse::<i32>()
        .map_err(|_| invalid())?;
    if !(1..=9999).contains(&year) {
        return Err(invalid());
    }
    let rest = value.get(4..).ok_or_else(invalid)?;
    let date = if rest.starts_with('W') || rest.starts_with("-W") {
        let rest = rest
            .strip_prefix('-')
            .unwrap_or(rest)
            .strip_prefix('W')
            .ok_or_else(invalid)?;
        let (week, day) = match rest.len() {
            2 => (parse(rest)?, 1),
            3 if !value.contains('-') => (parse(&rest[..2])?, parse(&rest[2..])?),
            4 if rest.as_bytes()[2] == b'-' && value.as_bytes()[4] == b'-' => {
                (parse(&rest[..2])?, parse(&rest[3..])?)
            }
            _ => return Err(invalid()),
        };
        let day = match day {
            1 => Weekday::Monday,
            2 => Weekday::Tuesday,
            3 => Weekday::Wednesday,
            4 => Weekday::Thursday,
            5 => Weekday::Friday,
            6 => Weekday::Saturday,
            7 => Weekday::Sunday,
            _ => return Err(invalid()),
        };
        Date::from_iso_week_date(year, week, day).map_err(|_| invalid())?
    } else {
        let (month, day) = match rest.len() {
            4 => (parse(&rest[..2])?, parse(&rest[2..])?),
            6 if rest.as_bytes()[0] == b'-' && rest.as_bytes()[3] == b'-' => {
                (parse(&rest[1..3])?, parse(&rest[4..])?)
            }
            _ => return Err(invalid()),
        };
        Date::from_calendar_date(year, Month::try_from(month).map_err(|_| invalid())?, day)
            .map_err(|_| invalid())?
    };
    (1..=9999)
        .contains(&date.year())
        .then_some(date)
        .ok_or_else(invalid)
}
fn format(date: Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        date.month() as u8,
        date.day()
    )
}
#[cfg(any(unix, windows))]
fn today() -> Result<Date> {
    // Use the same C-runtime local calendar as Python date.today(). The reentrant
    // conversion avoids sharing libc's static tm buffer between extraction jobs.
    let mut local: libc::tm = unsafe { std::mem::zeroed() };
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    #[cfg(unix)]
    let valid = !unsafe { libc::localtime_r(&now, &mut local) }.is_null();
    #[cfg(windows)]
    let valid = unsafe { libc::localtime_s(&mut local, &now) } == 0;
    if !valid {
        return Err(error("unable to read local calendar date"));
    }
    Date::from_calendar_date(
        local.tm_year + 1900,
        Month::try_from((local.tm_mon + 1) as u8).map_err(|_| invalid())?,
        local.tm_mday as u8,
    )
    .map_err(|_| error("invalid local calendar date"))
}
#[cfg(not(any(unix, windows)))]
fn today() -> Result<Date> {
    Err(error("local calendar date is unsupported on this platform"))
}

pub(super) fn metadata(
    params: &[(String, String)],
    clock: Option<Date>,
) -> Result<Arc<serde_json::Map<String, Value>>> {
    let get = |key: &str| {
        params
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    };
    let scale = get("scale").unwrap_or("day");
    let mut date = match get("date").filter(|value| !value.is_empty()) {
        Some(value) => value.to_owned(),
        None => format(match clock {
            Some(date) => date,
            None => today()?,
        }),
    };
    if scale == "week" {
        let value = iso_date(&date)?;
        date = format(
            value
                .checked_sub(Duration::days(
                    value.weekday().number_days_from_monday().into(),
                ))
                .ok_or_else(invalid)?,
        );
    } else if scale == "month" {
        date = date
            .chars()
            .take(date.chars().count().saturating_sub(3))
            .collect();
    }
    Ok(Arc::new(serde_json::Map::from_iter([
        ("date".into(), date.into()),
        ("scale".into(), scale.into()),
    ])))
}

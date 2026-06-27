//! Parsing of Portage's `metadata/timestamp.chk` date string.
//!
//! Portage writes the file in `TIMESTAMP_FORMAT` (`%a, %d %b %Y %H:%M:%S +0000`),
//! for example `Sun, 21 Jun 2026 05:45:00 +0000`, and compares the local and
//! server copies as UTC epochs. This module parses that one fixed format into an
//! epoch in seconds without pulling in a date crate. Both the rsync freshness
//! probe and the git max-age check use it, so equal date strings yield equal
//! epochs.

/// Parse a `TIMESTAMP_FORMAT` date string into a UTC epoch in seconds.
///
/// The format is `%a, %d %b %Y %H:%M:%S +0000`: an English weekday abbreviation
/// and a comma, the day of month, an English month abbreviation, the four-digit
/// year, `HH:MM:SS`, and a fixed `+0000` offset. The weekday is not validated
/// against the date, matching Portage. Returns `None` on a malformed string.
pub(crate) fn parse_timestamp_format(s: &str) -> Option<i64> {
    let s = s.trim();
    // Drop the weekday and its comma, for example `Sun, `.
    let rest = s.split_once(", ").map(|(_, r)| r).unwrap_or(s);
    let mut fields = rest.split_whitespace();

    let day: i64 = fields.next()?.parse().ok()?;
    let month = month_number(fields.next()?)?;
    let year: i64 = fields.next()?.parse().ok()?;
    let hms = fields.next()?;
    // The `+0000` offset is fixed UTC and is not consulted.

    let mut parts = hms.split(':');
    let hour: i64 = parts.next()?.parse().ok()?;
    let minute: i64 = parts.next()?.parse().ok()?;
    let second: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }

    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Map an English three-letter month abbreviation to its 1-12 number.
fn month_number(name: &str) -> Option<i64> {
    let n = match name {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    Some(n)
}

/// Days since the Unix epoch (1970-01-01) for a civil date, via Howard Hinnant's
/// `days_from_civil` algorithm. Valid for the proleptic Gregorian calendar.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_strings_to_known_epochs() {
        // 2026-06-21 05:45:00 UTC.
        assert_eq!(
            parse_timestamp_format("Sun, 21 Jun 2026 05:45:00 +0000"),
            Some(1_782_020_700)
        );
        // The Unix epoch itself.
        assert_eq!(
            parse_timestamp_format("Thu, 01 Jan 1970 00:00:00 +0000"),
            Some(0)
        );
    }

    #[test]
    fn ordering_is_monotonic() {
        let earlier = parse_timestamp_format("Sun, 21 Jun 2026 05:45:00 +0000").unwrap();
        let later = parse_timestamp_format("Sun, 21 Jun 2026 05:45:01 +0000").unwrap();
        let next_day = parse_timestamp_format("Mon, 22 Jun 2026 05:45:00 +0000").unwrap();
        assert!(earlier < later);
        assert_eq!(next_day - earlier, 86_400);
    }

    #[test]
    fn rejects_malformed_strings() {
        assert_eq!(parse_timestamp_format("not a date"), None);
        assert_eq!(
            parse_timestamp_format("Sun, 21 Foo 2026 05:45:00 +0000"),
            None
        );
        assert_eq!(parse_timestamp_format("1781675100"), None);
        assert_eq!(parse_timestamp_format(""), None);
    }
}

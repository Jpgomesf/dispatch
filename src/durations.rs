use std::time::Duration;

/// Parse durations like "30s", "10m", "1.5h" (`<number><s|m|h>`, surrounding spaces allowed).
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let invalid = || format!("invalid duration {text:?}: expected <number><s|m|h>, e.g. '10m'");
    let trimmed = text.trim();
    let Some(unit) = trimmed.chars().last() else {
        return Err(invalid());
    };
    let seconds_per_unit = match unit {
        's' => 1.0,
        'm' => 60.0,
        'h' => 3600.0,
        _ => return Err(invalid()),
    };
    let number = trimmed[..trimmed.len() - 1].trim_end();
    if !is_decimal(number) {
        return Err(invalid());
    }
    let value: f64 = number.parse().map_err(|_| invalid())?;
    if value <= 0.0 {
        return Err(format!("invalid duration {text:?}: must be positive"));
    }
    Ok(Duration::from_secs_f64(value * seconds_per_unit))
}

/// The largest whole unit: `3h`, `15m`, `90s` (sub-second parts are dropped).
#[must_use]
pub fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds > 0 && seconds.is_multiple_of(3600) {
        format!("{}h", seconds / 3600)
    } else if seconds > 0 && seconds.is_multiple_of(60) {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

/// `\d+(\.\d+)?`
fn is_decimal(text: &str) -> bool {
    let mut parts = text.split('.');
    let digits = |part: Option<&str>| {
        part.is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    };
    let whole = parts.next();
    match (parts.next(), parts.next()) {
        (None, None) => digits(whole),
        (Some(fraction), None) => digits(whole) && digits(Some(fraction)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_durations() {
        let cases = [
            ("30s", 30.0),
            ("10m", 600.0),
            ("2h", 7200.0),
            ("1.5h", 5400.0),
            (" 5 m ", 300.0),
        ];
        for (text, seconds) in cases {
            assert_eq!(
                parse_duration(text),
                Ok(Duration::from_secs_f64(seconds)),
                "{text}"
            );
        }
    }

    #[test]
    fn formats_in_the_largest_whole_unit() {
        let cases = [(10_800, "3h"), (900, "15m"), (90, "90s"), (0, "0s")];
        for (seconds, text) in cases {
            assert_eq!(format_duration(Duration::from_secs(seconds)), text);
        }
    }

    #[test]
    fn rejects_invalid_durations() {
        for text in [
            "",
            "10",
            "m",
            "10d",
            "-5m",
            "0s",
            "ten minutes",
            "1.m",
            ".5m",
        ] {
            let error = parse_duration(text).unwrap_err();
            assert!(error.contains("invalid duration"), "{text}: {error}");
        }
    }
}

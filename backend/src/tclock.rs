//! The one T-clock text format.
//!
//! `MM:SS` under an hour, `H:MM:SS` from one hour and `D:HH:MM:SS` from one
//! day, with an optional fraction (`.c`, `.cc`, `.ccc`). The leading field
//! never wraps or truncates. Signed text is `T- ` before T-0 and `T+ ` from
//! it. Fractions truncate, so a countdown never shows 0 before it gets there.
//! The frontend (`src/telemetry_dashboard/tclock.rs`,
//! `station_ui/rendering/three/tclock.js`) and the watch apps produce the same
//! text; the frontend's `watch/test-vectors.json` `tclock_format` pins it.

/// Clock digits for `magnitude_ms`, with `fraction_digits` (at most 3) after the seconds.
fn tclock_body(magnitude_ms: u64, fraction_digits: u32) -> String {
    let digits = fraction_digits.min(3);
    let total_s = magnitude_ms / 1000;
    let (days, hours) = (total_s / 86_400, total_s / 3_600 % 24);
    let (minutes, seconds) = (total_s / 60 % 60, total_s % 60);
    let mut text = if days > 0 {
        format!("{days}:{hours:02}:{minutes:02}:{seconds:02}")
    } else if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    };
    if digits > 0 {
        let fraction = magnitude_ms % 1000 / 10u64.pow(3 - digits);
        text.push_str(&format!(".{fraction:0width$}", width = digits as usize));
    }
    text
}

/// `T- body` when `before_t0`, else `T+ body`.
pub(crate) fn format_tclock(before_t0: bool, magnitude_ms: u64, fraction_digits: u32) -> String {
    let sign = if before_t0 { '-' } else { '+' };
    format!("T{sign} {}", tclock_body(magnitude_ms, fraction_digits))
}

/// Signed milliseconds from T-0: negative is `T-`, zero and up is `T+`.
#[cfg(test)]
fn format_tclock_ms(ms_from_t0: i64, fraction_digits: u32) -> String {
    format_tclock(ms_from_t0 < 0, ms_from_t0.unsigned_abs(), fraction_digits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edges() {
        assert_eq!(format_tclock_ms(0, 2), "T+ 00:00.00");
        assert_eq!(format_tclock_ms(59_999, 2), "T+ 00:59.99");
        assert_eq!(format_tclock_ms(-59_999, 2), "T- 00:59.99");
        assert_eq!(format_tclock_ms(-59_999, 0), "T- 00:59");
        assert_eq!(format_tclock_ms(-1, 1), "T- 00:00.0");
        assert_eq!(format_tclock_ms(3_599_999, 1), "T+ 59:59.9");
        assert_eq!(format_tclock_ms(3_600_000, 0), "T+ 1:00:00");
        assert_eq!(format_tclock_ms(-3_600_000, 2), "T- 1:00:00.00");
        assert_eq!(format_tclock_ms(6_000_000, 0), "T+ 1:40:00");
        assert_eq!(format_tclock_ms(86_399_999, 0), "T+ 23:59:59");
        assert_eq!(format_tclock_ms(86_400_000, 0), "T+ 1:00:00:00");
        assert_eq!(format_tclock_ms(93_784_000, 0), "T+ 1:02:03:04");
        // 99 h and beyond move to days instead of wrapping at 99.
        assert_eq!(format_tclock_ms(99 * 3_600_000 + 61_000, 0), "T+ 4:03:01:01");
        assert_eq!(format_tclock_ms(-(400 * 86_400_000), 0), "T- 400:00:00:00");
        assert_eq!(tclock_body(1_234, 3), "00:01.234");
        assert_eq!(tclock_body(1_234, 9), "00:01.234");
    }

    #[test]
    fn integer_limits() {
        assert_eq!(format_tclock_ms(i64::MAX, 0), "T+ 106751991167:07:12:55");
        assert_eq!(format_tclock_ms(i64::MIN, 3), "T- 106751991167:07:12:55.808");
        assert_eq!(tclock_body(u64::MAX, 2), "213503982334:14:25:51.61");
        assert_eq!(tclock_body(u64::from(u32::MAX), 0), "49:17:02:47");
        assert_eq!(tclock_body(i32::MAX as u64, 0), "24:20:31:23");
    }
}

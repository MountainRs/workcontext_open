//! 时间格式化纯函数（无 chrono 依赖；详细设计 §12.7「存储 UTC 毫秒」）。
//!
//! DWS CLI 的时间窗参数是 `yyyy-MM-dd HH:mm:ss`（调研 §4.1/§4.2，整秒边界）；
//! 判读提示词里的 `t` 用 RFC3339 毫秒精度。两处都由 UTC 毫秒 + 调用方给的
//! 时区偏移（毫秒）确定性导出——本 crate 不读系统时区，保持可测试。

/// 公历天数 → 年月日（Howard Hinnant 的 civil_from_days，覆盖公元 1 年起）。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// UTC 毫秒 + 时区偏移（毫秒）→ `yyyy-MM-dd HH:mm:ss`（整秒，向下取整）。
pub fn format_datetime(ms: i64, tz_offset_ms: i64) -> String {
    let secs = (ms + tz_offset_ms).div_euclid(1000);
    let (y, mo, d) = civil_from_days(secs.div_euclid(86_400));
    let sod = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{mo:02}-{d:02} {:02}:{:02}:{:02}",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

/// UTC 毫秒 → RFC3339 字面量（`2026-11-22T09:12:00.123Z`，毫秒保留）。
pub fn format_rfc3339_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let (y, mo, d) = civil_from_days(secs.div_euclid(86_400));
    let sod = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )

}

/// 毫秒向上取整到整秒：用于批准窗口的起点——CLI 边界只有整秒精度，向上
/// 取整保证请求范围不会早于批准起点。
pub fn ceil_to_second(ms: i64) -> i64 {
    let floored = floor_to_second(ms);
    if floored == ms {
        ms
    } else {
        floored + 1000
    }
}

/// 毫秒向下取整到整秒（调研 §4.2：窗口边界用整秒）。
pub fn floor_to_second(ms: i64) -> i64 {
    ms.div_euclid(1000) * 1000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datetime_epoch_and_known_values() {
        assert_eq!(format_datetime(0, 0), "1970-01-01 00:00:00");
        // 已知锚点：2024-01-01T00:00:00Z = 1704067200s。
        assert_eq!(format_datetime(1_704_067_200_000, 0), "2024-01-01 00:00:00");
        // 2026-11-22T00:00:00Z = 1795305600s（19723 + 731 + 325 = 20779 天）。
        let ms = 1_795_305_600_000i64;
        assert_eq!(format_datetime(ms, 0), "2026-11-22 00:00:00");
        assert_eq!(format_datetime(ms + 8 * 3600 * 1000, 0), "2026-11-22 08:00:00");
        // 东八区偏移：UTC 2026-11-21 16:00 → 本地 2026-11-22 00:00
        assert_eq!(format_datetime(ms - 8 * 3600 * 1000, 8 * 3600 * 1000), "2026-11-22 00:00:00");
        // 闰日：2024-02-29T00:00:00Z = 1709164800s。
        assert_eq!(format_datetime(1_709_164_800_000, 0), "2024-02-29 00:00:00");
        // 跨年边界：2026-12-31T16:00:00Z = 2027-01-01（东八区）。
        assert_eq!(format_datetime(1_798_732_800_000, 8 * 3600 * 1000), "2027-01-01 00:00:00");
    }

    #[test]
    fn rfc3339_keeps_millis() {
        assert_eq!(format_rfc3339_ms(1_795_305_600_123), "2026-11-22T00:00:00.123Z");
        assert_eq!(format_rfc3339_ms(-1), "1969-12-31T23:59:59.999Z");
    }

    #[test]
    fn floor_is_second_aligned() {
        assert_eq!(floor_to_second(1_795_324_800_999), 1_795_324_800_000);
        assert_eq!(floor_to_second(-1_500), -2000);
    }

    #[test]
    fn ceil_never_precedes_the_approved_start() {
        let whole = 1_795_305_600_000;
        assert_eq!(ceil_to_second(whole), whole, "整秒不变");
        assert_eq!(ceil_to_second(whole + 1), whole + 1000, "非整秒向上取整");
        assert_eq!(ceil_to_second(whole + 999), whole + 1000);
        // 负时间轴同样不早于起点：-1501ms → -1000ms。
        assert_eq!(ceil_to_second(-1501), -1000);
    }
}

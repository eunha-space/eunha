//! `users.time_zone`, as Rails reads it: `ActiveSupport::TimeZone[name]`
//! accepts one of the friendly names in `ActiveSupport::TimeZone::MAPPING` or
//! any IANA identifier tzinfo knows, and Mastodon's `User` keeps a name only
//! when that lookup finds a zone (`normalizes :time_zone`). The times
//! Mastodon writes into a user's mail are in that zone, formatted
//! `:with_time_zone`.

use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::{OffsetComponents, OffsetName, Tz};

/// `ActiveSupport::TimeZone::MAPPING`, as of the Rails Mastodon 4.7.1 locks
/// (8.1.3.1): the friendly names the settings page offers, each with the
/// IANA zone it stands for.
pub const MAPPING: &[(&str, &str)] = &[
    ("International Date Line West", "Etc/GMT+12"),
    ("Midway Island", "Pacific/Midway"),
    ("American Samoa", "Pacific/Pago_Pago"),
    ("Hawaii", "Pacific/Honolulu"),
    ("Alaska", "America/Juneau"),
    ("Pacific Time (US & Canada)", "America/Los_Angeles"),
    ("Tijuana", "America/Tijuana"),
    ("Mountain Time (US & Canada)", "America/Denver"),
    ("Arizona", "America/Phoenix"),
    ("Chihuahua", "America/Chihuahua"),
    ("Mazatlan", "America/Mazatlan"),
    ("Central Time (US & Canada)", "America/Chicago"),
    ("Saskatchewan", "America/Regina"),
    ("Guadalajara", "America/Mexico_City"),
    ("Mexico City", "America/Mexico_City"),
    ("Monterrey", "America/Monterrey"),
    ("Central America", "America/Guatemala"),
    ("Eastern Time (US & Canada)", "America/New_York"),
    ("Indiana (East)", "America/Indiana/Indianapolis"),
    ("Bogota", "America/Bogota"),
    ("Lima", "America/Lima"),
    ("Quito", "America/Lima"),
    ("Atlantic Time (Canada)", "America/Halifax"),
    ("Caracas", "America/Caracas"),
    ("La Paz", "America/La_Paz"),
    ("Santiago", "America/Santiago"),
    ("Asuncion", "America/Asuncion"),
    ("Newfoundland", "America/St_Johns"),
    ("Brasilia", "America/Sao_Paulo"),
    ("Buenos Aires", "America/Argentina/Buenos_Aires"),
    ("Montevideo", "America/Montevideo"),
    ("Georgetown", "America/Guyana"),
    ("Puerto Rico", "America/Puerto_Rico"),
    ("Greenland", "America/Nuuk"),
    ("Mid-Atlantic", "Atlantic/South_Georgia"),
    ("Azores", "Atlantic/Azores"),
    ("Cape Verde Is.", "Atlantic/Cape_Verde"),
    ("Dublin", "Europe/Dublin"),
    ("Edinburgh", "Europe/London"),
    ("Lisbon", "Europe/Lisbon"),
    ("London", "Europe/London"),
    ("Casablanca", "Africa/Casablanca"),
    ("Monrovia", "Africa/Monrovia"),
    ("UTC", "Etc/UTC"),
    ("Belgrade", "Europe/Belgrade"),
    ("Bratislava", "Europe/Bratislava"),
    ("Budapest", "Europe/Budapest"),
    ("Ljubljana", "Europe/Ljubljana"),
    ("Prague", "Europe/Prague"),
    ("Sarajevo", "Europe/Sarajevo"),
    ("Skopje", "Europe/Skopje"),
    ("Warsaw", "Europe/Warsaw"),
    ("Zagreb", "Europe/Zagreb"),
    ("Brussels", "Europe/Brussels"),
    ("Copenhagen", "Europe/Copenhagen"),
    ("Madrid", "Europe/Madrid"),
    ("Paris", "Europe/Paris"),
    ("Amsterdam", "Europe/Amsterdam"),
    ("Berlin", "Europe/Berlin"),
    ("Bern", "Europe/Zurich"),
    ("Zurich", "Europe/Zurich"),
    ("Rome", "Europe/Rome"),
    ("Stockholm", "Europe/Stockholm"),
    ("Vienna", "Europe/Vienna"),
    ("West Central Africa", "Africa/Algiers"),
    ("Bucharest", "Europe/Bucharest"),
    ("Cairo", "Africa/Cairo"),
    ("Helsinki", "Europe/Helsinki"),
    ("Kyiv", "Europe/Kiev"),
    ("Riga", "Europe/Riga"),
    ("Sofia", "Europe/Sofia"),
    ("Tallinn", "Europe/Tallinn"),
    ("Vilnius", "Europe/Vilnius"),
    ("Athens", "Europe/Athens"),
    ("Istanbul", "Europe/Istanbul"),
    ("Minsk", "Europe/Minsk"),
    ("Jerusalem", "Asia/Jerusalem"),
    ("Harare", "Africa/Harare"),
    ("Pretoria", "Africa/Johannesburg"),
    ("Kaliningrad", "Europe/Kaliningrad"),
    ("Moscow", "Europe/Moscow"),
    ("St. Petersburg", "Europe/Moscow"),
    ("Volgograd", "Europe/Volgograd"),
    ("Samara", "Europe/Samara"),
    ("Kuwait", "Asia/Kuwait"),
    ("Riyadh", "Asia/Riyadh"),
    ("Nairobi", "Africa/Nairobi"),
    ("Baghdad", "Asia/Baghdad"),
    ("Tehran", "Asia/Tehran"),
    ("Abu Dhabi", "Asia/Muscat"),
    ("Muscat", "Asia/Muscat"),
    ("Baku", "Asia/Baku"),
    ("Tbilisi", "Asia/Tbilisi"),
    ("Yerevan", "Asia/Yerevan"),
    ("Kabul", "Asia/Kabul"),
    ("Ekaterinburg", "Asia/Yekaterinburg"),
    ("Islamabad", "Asia/Karachi"),
    ("Karachi", "Asia/Karachi"),
    ("Tashkent", "Asia/Tashkent"),
    ("Chennai", "Asia/Kolkata"),
    ("Kolkata", "Asia/Kolkata"),
    ("Mumbai", "Asia/Kolkata"),
    ("New Delhi", "Asia/Kolkata"),
    ("Kathmandu", "Asia/Kathmandu"),
    ("Dhaka", "Asia/Dhaka"),
    ("Sri Jayawardenepura", "Asia/Colombo"),
    ("Almaty", "Asia/Almaty"),
    ("Astana", "Asia/Almaty"),
    ("Novosibirsk", "Asia/Novosibirsk"),
    ("Rangoon", "Asia/Rangoon"),
    ("Bangkok", "Asia/Bangkok"),
    ("Hanoi", "Asia/Bangkok"),
    ("Jakarta", "Asia/Jakarta"),
    ("Krasnoyarsk", "Asia/Krasnoyarsk"),
    ("Beijing", "Asia/Shanghai"),
    ("Chongqing", "Asia/Chongqing"),
    ("Hong Kong", "Asia/Hong_Kong"),
    ("Urumqi", "Asia/Urumqi"),
    ("Kuala Lumpur", "Asia/Kuala_Lumpur"),
    ("Singapore", "Asia/Singapore"),
    ("Taipei", "Asia/Taipei"),
    ("Perth", "Australia/Perth"),
    ("Irkutsk", "Asia/Irkutsk"),
    ("Ulaanbaatar", "Asia/Ulaanbaatar"),
    ("Seoul", "Asia/Seoul"),
    ("Osaka", "Asia/Tokyo"),
    ("Sapporo", "Asia/Tokyo"),
    ("Tokyo", "Asia/Tokyo"),
    ("Yakutsk", "Asia/Yakutsk"),
    ("Darwin", "Australia/Darwin"),
    ("Adelaide", "Australia/Adelaide"),
    ("Canberra", "Australia/Canberra"),
    ("Melbourne", "Australia/Melbourne"),
    ("Sydney", "Australia/Sydney"),
    ("Brisbane", "Australia/Brisbane"),
    ("Hobart", "Australia/Hobart"),
    ("Vladivostok", "Asia/Vladivostok"),
    ("Guam", "Pacific/Guam"),
    ("Port Moresby", "Pacific/Port_Moresby"),
    ("Magadan", "Asia/Magadan"),
    ("Srednekolymsk", "Asia/Srednekolymsk"),
    ("Solomon Is.", "Pacific/Guadalcanal"),
    ("New Caledonia", "Pacific/Noumea"),
    ("Fiji", "Pacific/Fiji"),
    ("Kamchatka", "Asia/Kamchatka"),
    ("Marshall Is.", "Pacific/Majuro"),
    ("Auckland", "Pacific/Auckland"),
    ("Wellington", "Pacific/Auckland"),
    ("Nuku'alofa", "Pacific/Tongatapu"),
    ("Tokelau Is.", "Pacific/Fakaofo"),
    ("Chatham Is.", "Pacific/Chatham"),
    ("Samoa", "Pacific/Apia"),
];

/// `ActiveSupport::TimeZone[name]`'s zone: a friendly name's, or the IANA
/// zone of that identifier.
pub fn find(name: &str) -> Option<Tz> {
    let identifier = MAPPING
        .iter()
        .find(|(friendly, _)| *friendly == name)
        .map_or(name, |(_, iana)| iana);
    identifier.parse().ok()
}

/// `normalizes :time_zone, with: ->(time_zone) { ActiveSupport::TimeZone[time_zone].nil? ? nil : time_zone }`:
/// the name as given when Rails knows it, otherwise none.
pub fn normalize(name: Option<&str>) -> Option<String> {
    let name = name?;
    find(name).map(|_| name.to_owned())
}

/// One entry of the settings page's list, `time_zone_options`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Choice {
    /// What the form submits: the zone's IANA identifier (`tz.tzinfo.name`).
    pub value: &'static str,
    /// `"(GMT#{tz.now.formatted_offset}) #{tz.name}"`.
    pub label: String,
}

/// `SettingsHelper#time_zone_options`: `ActiveSupport::TimeZone.all`, in its
/// order (standard offset, then name), labelled with each zone's offset now.
pub fn choices(now: DateTime<Utc>) -> Vec<Choice> {
    let mut zones: Vec<(i64, &str, &str, String)> = MAPPING
        .iter()
        .filter_map(|(name, iana)| {
            let tz: Tz = iana.parse().ok()?;
            let offset = tz.offset_from_utc_datetime(&now.naive_utc());
            let base = offset.base_utc_offset().num_seconds();
            let total = base + offset.dst_offset().num_seconds();
            Some((base, *name, *iana, formatted_offset(total)))
        })
        .collect();
    zones.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    zones
        .into_iter()
        .map(|(_, name, iana, offset)| Choice {
            value: iana,
            label: format!("(GMT{offset}) {name}"),
        })
        .collect()
}

/// `ActiveSupport::TimeZone.seconds_to_utc_offset`, with the colon.
fn formatted_offset(seconds: i64) -> String {
    let sign = if seconds < 0 { '-' } else { '+' };
    let seconds = seconds.abs();
    format!("{sign}{:02}:{:02}", seconds / 3600, (seconds % 3600) / 60)
}

/// `l(time.in_time_zone(time_zone.presence), format: :with_time_zone)`: the
/// time in the user's zone, or UTC when they have none (or one Rails would
/// not know), as the locale's `time.formats.with_time_zone` writes it.
pub fn format_with_time_zone(time: DateTime<Utc>, time_zone: Option<&str>, locale: &str) -> String {
    let format = if locale.starts_with("ko") {
        "%Y-%m-%d %H:%M"
    } else {
        "%b %d, %Y, %H:%M"
    };
    match time_zone.filter(|z| !z.is_empty()).and_then(find) {
        Some(tz) => {
            let local = time.with_timezone(&tz);
            let abbreviation = local.offset().abbreviation().unwrap_or("UTC").to_owned();
            format!("{} {abbreviation}", local.format(format))
        }
        None => format!("{} UTC", time.format(format)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_rails_knows_are_kept() {
        assert_eq!(normalize(Some("Seoul")).as_deref(), Some("Seoul"));
        assert_eq!(normalize(Some("Asia/Seoul")).as_deref(), Some("Asia/Seoul"));
        assert_eq!(
            normalize(Some("America/Argentina/Jujuy")).as_deref(),
            Some("America/Argentina/Jujuy")
        );
        assert_eq!(normalize(Some("Mars/Olympus_Mons")), None);
        assert_eq!(normalize(Some("")), None);
        assert_eq!(normalize(None), None);
    }

    #[test]
    fn every_friendly_name_resolves() {
        for (name, iana) in MAPPING {
            assert!(find(name).is_some(), "{name} ({iana})");
        }
    }

    #[test]
    fn choices_follow_rails_order() {
        let now = DateTime::parse_from_rfc3339("2026-01-15T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let choices = choices(now);
        assert_eq!(choices.len(), MAPPING.len());
        assert_eq!(choices[0].label, "(GMT-12:00) International Date Line West");
        assert_eq!(choices[0].value, "Etc/GMT+12");
        assert_eq!(choices.last().unwrap().label, "(GMT+13:00) Tokelau Is.");
        let seoul = choices
            .iter()
            .find(|c| c.label.ends_with(" Seoul"))
            .unwrap();
        assert_eq!(seoul.label, "(GMT+09:00) Seoul");
        assert_eq!(seoul.value, "Asia/Seoul");
    }

    #[test]
    fn times_are_written_in_the_users_zone() {
        let time = DateTime::parse_from_rfc3339("2026-03-04T15:06:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            format_with_time_zone(time, None, "en"),
            "Mar 04, 2026, 15:06 UTC"
        );
        assert_eq!(
            format_with_time_zone(time, Some("Seoul"), "en"),
            "Mar 05, 2026, 00:06 KST"
        );
        assert_eq!(
            format_with_time_zone(time, Some("Asia/Seoul"), "ko"),
            "2026-03-05 00:06 KST"
        );
        assert_eq!(
            format_with_time_zone(time, Some("Nowhere"), "en"),
            "Mar 04, 2026, 15:06 UTC"
        );
    }
}

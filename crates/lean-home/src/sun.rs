//! Solar sunrise and sunset calculation, matching Home Assistant's `astral` 2.2
//! (`astral/sun.py` and `homeassistant/helpers/sun.py`).

use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};

use crate::config::{Core, Zone};

/// Apparent solar radius in degrees (32 arc minutes diameter / 2).
const SUN_APPARENT_RADIUS: f64 = 32.0 / (60.0 * 2.0);

/// Observer location and elevation above sea level (metres).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Observer {
    pub latitude: f64,
    pub longitude: f64,
    pub elevation: f64,
}

impl Observer {
    pub fn from_core(core: &Core) -> Self {
        Self {
            latitude: core.latitude.as_f64().unwrap_or(0.0),
            longitude: core.longitude.as_f64().unwrap_or(0.0),
            elevation: core.elevation.as_f64().unwrap_or(0.0),
        }
    }
}

/// Solar events supported by `sun` triggers and conditions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SunEvent {
    Sunrise,
    Sunset,
}

impl SunEvent {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "sunrise" => Ok(Self::Sunrise),
            "sunset" => Ok(Self::Sunset),
            other => Err(format!(
                "invalid sun event '{other}' (expected 'sunrise' or 'sunset')"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sunrise => "sunrise",
            Self::Sunset => "sunset",
        }
    }
}

/// Calculate sunrise time in UTC for `date` at `observer`, or `None` if the sun does not rise.
pub fn sunrise(observer: Observer, date: NaiveDate) -> Option<DateTime<Utc>> {
    time_of_transit(observer, date, 90.0 + SUN_APPARENT_RADIUS, true)
}

/// Calculate sunset time in UTC for `date` at `observer`, or `None` if the sun does not set.
pub fn sunset(observer: Observer, date: NaiveDate) -> Option<DateTime<Utc>> {
    time_of_transit(observer, date, 90.0 + SUN_APPARENT_RADIUS, false)
}

/// Calculate the specified solar event time in UTC for `date`, or `None` if it does not occur.
pub fn event_date(observer: Observer, event: SunEvent, date: NaiveDate) -> Option<DateTime<Utc>> {
    match event {
        SunEvent::Sunrise => sunrise(observer, date),
        SunEvent::Sunset => sunset(observer, date),
    }
}

/// Calculate the next occurrence of `event + offset` strictly after `utc_point_in_time`
/// (HA's `get_observer_astral_event_next`).
pub fn event_next(
    observer: Observer,
    event: SunEvent,
    utc_point_in_time: DateTime<Utc>,
    offset: Duration,
    tz: &Zone,
) -> Option<DateTime<Utc>> {
    let local_date = (utc_point_in_time + tz.offset(utc_point_in_time)).date_naive();
    for mod_days in -1..=366 {
        let Some(date) = local_date.checked_add_signed(Duration::days(mod_days)) else {
            continue;
        };
        if let Some(dt) = event_date(observer, event, date) {
            let candidate = dt + offset;
            if candidate > utc_point_in_time {
                return Some(candidate);
            }
        }
    }
    None
}

fn julianday(date: NaiveDate) -> f64 {
    let mut y = date.year() as f64;
    let mut m = date.month() as f64;
    let d = date.day() as f64;
    if m <= 2.0 {
        y -= 1.0;
        m += 12.0;
    }
    let a = (y / 100.0).floor();
    let b = 2.0 - a + (a / 4.0).floor();
    (365.25 * (y + 4716.0)).floor() + (30.6001 * (m + 1.0)).floor() + d + b - 1524.5
}

fn minutes_to_duration(minutes: f64) -> Duration {
    let d = (minutes / 1440.0).trunc() as i64;
    let rem_secs = (minutes - (d as f64) * 1440.0) * 60.0;
    let s = rem_secs.trunc() as i64;
    let us = ((rem_secs - s as f64) * 1_000_000.0).trunc() as i64;
    Duration::days(d) + Duration::seconds(s) + Duration::microseconds(us)
}

fn jday_to_jcentury(julianday: f64) -> f64 {
    (julianday - 2_451_545.0) / 36_525.0
}

fn jcentury_to_jday(juliancentury: f64) -> f64 {
    (juliancentury * 36_525.0) + 2_451_545.0
}

fn geom_mean_long_sun(juliancentury: f64) -> f64 {
    (280.46646 + juliancentury * (36_000.769_83 + 0.0003032 * juliancentury)).rem_euclid(360.0)
}

fn geom_mean_anomaly_sun(juliancentury: f64) -> f64 {
    357.52911 + juliancentury * (35_999.050_29 - 0.0001537 * juliancentury)
}

fn eccentric_location_earth_orbit(juliancentury: f64) -> f64 {
    0.016_708_634 - juliancentury * (0.000_042_037 + 0.000_000_126_7 * juliancentury)
}

fn sun_eq_of_center(juliancentury: f64) -> f64 {
    let mrad = geom_mean_anomaly_sun(juliancentury).to_radians();
    let sinm = mrad.sin();
    let sin2m = (2.0 * mrad).sin();
    let sin3m = (3.0 * mrad).sin();
    sinm * (1.914_602 - juliancentury * (0.004_817 + 0.000_014 * juliancentury))
        + sin2m * (0.019_993 - 0.000_101 * juliancentury)
        + sin3m * 0.000_289
}

fn sun_true_long(juliancentury: f64) -> f64 {
    geom_mean_long_sun(juliancentury) + sun_eq_of_center(juliancentury)
}

fn sun_apparent_long(juliancentury: f64) -> f64 {
    let true_long = sun_true_long(juliancentury);
    let omega = 125.04 - 1_934.136 * juliancentury;
    true_long - 0.00569 - 0.00478 * omega.to_radians().sin()
}

fn mean_obliquity_of_ecliptic(juliancentury: f64) -> f64 {
    let seconds =
        21.448 - juliancentury * (46.815 + juliancentury * (0.00059 - juliancentury * 0.001_813));
    23.0 + (26.0 + (seconds / 60.0)) / 60.0
}

fn obliquity_correction(juliancentury: f64) -> f64 {
    let e0 = mean_obliquity_of_ecliptic(juliancentury);
    let omega = 125.04 - 1_934.136 * juliancentury;
    e0 + 0.00256 * omega.to_radians().cos()
}

fn sun_declination(juliancentury: f64) -> f64 {
    let e = obliquity_correction(juliancentury).to_radians();
    let lambda = sun_apparent_long(juliancentury).to_radians();
    (e.sin() * lambda.sin()).asin().to_degrees()
}

fn var_y(juliancentury: f64) -> f64 {
    let epsilon = obliquity_correction(juliancentury);
    let y = (epsilon.to_radians() / 2.0).tan();
    y * y
}

fn eq_of_time(juliancentury: f64) -> f64 {
    let l0 = geom_mean_long_sun(juliancentury).to_radians();
    let e = eccentric_location_earth_orbit(juliancentury);
    let m = geom_mean_anomaly_sun(juliancentury).to_radians();
    let y = var_y(juliancentury);

    let sin2l0 = (2.0 * l0).sin();
    let sinm = m.sin();
    let cos2l0 = (2.0 * l0).cos();
    let sin4l0 = (4.0 * l0).sin();
    let sin2m = (2.0 * m).sin();

    let etime = y * sin2l0 - 2.0 * e * sinm + 4.0 * e * y * sinm * cos2l0
        - 0.5 * y * y * sin4l0
        - 1.25 * e * e * sin2m;

    etime.to_degrees() * 4.0
}

fn hour_angle(latitude: f64, declination: f64, zenith: f64, rising: bool) -> Option<f64> {
    let lat_rad = latitude.to_radians();
    let dec_rad = declination.to_radians();
    let zen_rad = zenith.to_radians();

    let h = (zen_rad.cos() - lat_rad.sin() * dec_rad.sin()) / (lat_rad.cos() * dec_rad.cos());
    if !(-1.0..=1.0).contains(&h) {
        return None;
    }
    let ha = h.acos();
    Some(if rising { ha } else { -ha })
}

fn adjust_to_horizon(elevation: f64) -> f64 {
    if elevation <= 0.0 {
        return 0.0;
    }
    let r = 6_356_900.0;
    (r / (r + elevation)).acos().to_degrees()
}

fn refraction_at_zenith(zenith: f64) -> f64 {
    let elevation = 90.0 - zenith;
    if elevation >= 85.0 {
        return 0.0;
    }
    let te = elevation.to_radians().tan();
    let correction = if elevation > 5.0 {
        58.1 / te - 0.07 / (te * te * te) + 0.000_086 / (te * te * te * te * te)
    } else if elevation > -0.575 {
        let step1 = -12.79 + elevation * 0.711;
        let step2 = 103.4 + elevation * step1;
        let step3 = -518.2 + elevation * step2;
        1735.0 + elevation * step3
    } else {
        -20.774 / te
    };
    correction / 3600.0
}

fn time_of_transit(
    observer: Observer,
    date: NaiveDate,
    zenith: f64,
    rising: bool,
) -> Option<DateTime<Utc>> {
    let latitude = observer.latitude.clamp(-89.8, 89.8);
    let adjustment_for_elevation = adjust_to_horizon(observer.elevation);
    let adjustment_for_refraction = refraction_at_zenith(zenith + adjustment_for_elevation);

    let jd = julianday(date);
    let t = jday_to_jcentury(jd);
    let solar_dec = sun_declination(t);
    let ha = hour_angle(
        latitude,
        solar_dec,
        zenith + adjustment_for_elevation - adjustment_for_refraction,
        rising,
    )?;

    let delta = -observer.longitude - ha.to_degrees();
    let time_utc = 720.0 + 4.0 * delta - eq_of_time(t);

    let t2 = jday_to_jcentury(jcentury_to_jday(t) + time_utc / 1440.0);
    let solar_dec2 = sun_declination(t2);
    let ha2 = hour_angle(
        latitude,
        solar_dec2,
        zenith + adjustment_for_elevation + adjustment_for_refraction,
        rising,
    )?;

    let delta2 = -observer.longitude - ha2.to_degrees();
    let time_utc2 = 720.0 + 4.0 * delta2 - eq_of_time(t2);

    let td = minutes_to_duration(time_utc2);
    let midnight = date.and_hms_opt(0, 0, 0)?.and_utc();
    Some(midnight + td)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn pr5_fixture() -> Value {
        let raw = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../compat/fixtures/step2/pr5-sun.json"
        ))
        .expect("read pr5-sun.json");
        serde_json::from_str(&raw).expect("parse pr5-sun.json")
    }

    #[test]
    fn matches_ha_astral_fixture() {
        let fix = pr5_fixture();
        let loc = &fix["location"];
        let london = Observer {
            latitude: loc["latitude"].as_f64().unwrap(),
            longitude: loc["longitude"].as_f64().unwrap(),
            elevation: loc["elevation"].as_f64().unwrap(),
        };
        let utc = Zone::utc();

        let d = NaiveDate::from_ymd_opt(2026, 11, 24).unwrap();
        let expected_sr: DateTime<Utc> = fix["london_2026_11_24"]["sunrise"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let expected_ss: DateTime<Utc> = fix["london_2026_11_24"]["sunset"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(sunrise(london, d).unwrap(), expected_sr);
        assert_eq!(sunset(london, d).unwrap(), expected_ss);

        let austin = Observer {
            latitude: 30.2672,
            longitude: -97.7431,
            elevation: 150.0,
        };
        let austin_ref: DateTime<Utc> = "2026-11-24T12:00:00Z".parse().unwrap();
        assert_eq!(
            event_next(
                austin,
                SunEvent::Sunrise,
                austin_ref,
                Duration::zero(),
                &utc
            )
            .unwrap(),
            fix["austin_next_from_12_utc"]["sunrise"]
                .as_str()
                .unwrap()
                .parse::<DateTime<Utc>>()
                .unwrap()
        );
        assert_eq!(
            event_next(austin, SunEvent::Sunset, austin_ref, Duration::zero(), &utc).unwrap(),
            fix["austin_next_from_12_utc"]["sunset"]
                .as_str()
                .unwrap()
                .parse::<DateTime<Utc>>()
                .unwrap()
        );

        for case in fix["astral_next"].as_array().unwrap() {
            let t: DateTime<Utc> = case["utc_point_in_time"].as_str().unwrap().parse().unwrap();
            let ev = SunEvent::parse(case["event"].as_str().unwrap()).unwrap();
            let off = Duration::seconds(case["offset_seconds"].as_i64().unwrap());
            let expected: DateTime<Utc> = case["next"].as_str().unwrap().parse().unwrap();
            let got = event_next(london, ev, t, off, &utc).unwrap();
            assert_eq!(got, expected, "case {case}");
        }
    }

    #[test]
    fn polar_night_returns_none_for_date_and_finds_next() {
        let svalbard = Observer {
            latitude: 78.2232,
            longitude: 15.6267,
            elevation: 10.0,
        };
        let winter = NaiveDate::from_ymd_opt(2026, 11, 24).unwrap();
        assert!(sunrise(svalbard, winter).is_none());
        assert!(sunset(svalbard, winter).is_none());
        let next_sr = event_next(
            svalbard,
            SunEvent::Sunrise,
            winter.and_hms_opt(12, 0, 0).unwrap().and_utc(),
            Duration::zero(),
            &Zone::utc(),
        );
        assert!(next_sr.is_some());
    }
}

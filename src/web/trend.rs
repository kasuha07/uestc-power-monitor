//! Daily snapshots and consumption from chronological samples, using bounded memory.
use crate::db::PowerRecord;
use chrono::{DateTime, Duration, NaiveDate, TimeZone};
use chrono_tz::Tz;
use serde::Serialize;
use std::collections::BTreeMap;

const WINDOW_DAYS: i64 = 30;
const RATE_DAYS: i64 = 7;

#[derive(Serialize)]
pub(super) struct TrendDay {
    pub date: NaiveDate,
    pub money: f64,
    pub energy: f64,
    pub used_money: f64,
    pub used_energy: f64,
    pub recharged: bool,
    #[serde(skip)]
    complete: bool,
}

#[derive(Serialize)]
pub(super) struct TrendEstimate {
    pub daily_money: f64,
    pub daily_energy: f64,
    pub days_remaining: f64,
}

#[derive(Serialize)]
pub(super) struct Trend {
    pub days: Vec<TrendDay>,
    pub estimate: Option<TrendEstimate>,
}

struct Sample {
    at: DateTime<Tz>,
    money: f64,
    energy: f64,
}

#[derive(Default)]
struct Day {
    last: Option<Sample>,
    used_money: f64,
    used_energy: f64,
    coverage: Option<(DateTime<Tz>, DateTime<Tz>)>,
    recharged: bool,
}

/// Find the first valid local minute, including zones whose DST skips midnight.
fn day_start(tz: Tz, date: NaiveDate) -> Option<DateTime<Tz>> {
    let midnight = date.and_hms_opt(0, 0, 0)?;
    (0..=48 * 60).find_map(|minute| {
        let local = midnight.checked_add_signed(Duration::minutes(minute))?;
        tz.from_local_datetime(&local).earliest()
    })
}

pub(super) fn window_start(tz: Tz, today: NaiveDate) -> Option<DateTime<Tz>> {
    day_start(
        tz,
        today.checked_sub_signed(Duration::days(WINDOW_DAYS - 1))?,
    )
}

fn seconds_between(start: DateTime<Tz>, end: DateTime<Tz>) -> f64 {
    let duration = end - start;
    duration.num_seconds() as f64 + duration.subsec_nanos() as f64 / 1_000_000_000.0
}

/// Retains one snapshot per day and the previous sample, regardless of sampling rate.
pub(super) struct TrendBuilder {
    tz: Tz,
    today: NaiveDate,
    start: DateTime<Tz>,
    days: BTreeMap<NaiveDate, Day>,
    previous: Option<Sample>,
    latest_money: Option<f64>,
}

impl TrendBuilder {
    pub fn new(tz: Tz, today: NaiveDate) -> Option<Self> {
        Some(Self {
            tz,
            today,
            start: window_start(tz, today)?,
            days: BTreeMap::new(),
            previous: None,
            latest_money: None,
        })
    }

    pub fn push(&mut self, record: &PowerRecord) {
        let Ok(at) = DateTime::parse_from_rfc3339(&record.created_at) else {
            return;
        };
        let at = at.with_timezone(&self.tz);
        if !record.remaining_money.is_finite()
            || !record.remaining_energy.is_finite()
            || at.date_naive() > self.today
            || self
                .previous
                .as_ref()
                .is_some_and(|previous| at < previous.at)
        {
            return;
        }
        let sample = Sample {
            at,
            money: record.remaining_money,
            energy: record.remaining_energy,
        };
        if let Some(previous) = &self.previous {
            let money = previous.money - sample.money;
            // Sum observed drops rather than offsetting consumption against recharged energy.
            let energy = (previous.energy - sample.energy).max(0.0);
            let recharged = money < 0.0 || sample.energy > previous.energy;
            let seconds = seconds_between(previous.at, at);
            let mut cursor = previous.at.max(self.start);
            if seconds == 0.0 && at >= self.start {
                let day = self.days.entry(at.date_naive()).or_default();
                day.used_money += money;
                day.used_energy += energy;
                day.recharged |= recharged;
            }
            while cursor < at {
                let date = cursor.date_naive();
                let end = if date == at.date_naive() {
                    at
                } else {
                    let Some(end) = date.succ_opt().and_then(|next| day_start(self.tz, next))
                    else {
                        break;
                    };
                    end
                };
                let covered = seconds_between(cursor, end);
                let fraction = if seconds > 0.0 {
                    covered / seconds
                } else {
                    0.0
                };
                let day = self.days.entry(date).or_default();
                day.used_money += money * fraction;
                day.used_energy += energy * fraction;
                day.coverage.get_or_insert((cursor, end)).1 = end;
                // A recharge somewhere in a cross-midnight interval taints all affected days.
                day.recharged |= recharged;
                cursor = end;
            }
            if recharged && at >= self.start {
                self.days.entry(at.date_naive()).or_default().recharged = true;
            }
        }
        if at >= self.start {
            self.latest_money = Some(sample.money);
            self.days.entry(at.date_naive()).or_default().last = Some(Sample {
                at,
                money: sample.money,
                energy: sample.energy,
            });
        }
        self.previous = Some(sample);
    }

    pub fn finish(self) -> Trend {
        let days: Vec<_> = self
            .days
            .into_iter()
            .filter_map(|(date, day)| {
                // Chart points always represent an actual sample, never an interpolated balance.
                let last = day.last?;
                let boundaries = day_start(self.tz, date)
                    .zip(date.succ_opt().and_then(|next| day_start(self.tz, next)))
                    .zip(day.coverage);
                Some(TrendDay {
                    date,
                    money: last.money,
                    energy: last.energy,
                    used_money: day.used_money,
                    used_energy: day.used_energy,
                    recharged: day.recharged,
                    complete: boundaries
                        .is_some_and(|((start, end), (from, until))| from <= start && until >= end),
                })
            })
            .collect();
        let estimate = estimate(&days, self.latest_money, self.today);
        Trend { days, estimate }
    }
}

fn positive(value: f64) -> bool {
    value.is_finite() && value > 0.0
}

fn estimate(days: &[TrendDay], money: Option<f64>, today: NaiveDate) -> Option<TrendEstimate> {
    let money = money?;
    let complete: Vec<_> = days
        .iter()
        .filter(|day| day.date < today && day.complete)
        .collect();
    let since = today.checked_sub_signed(Duration::days(RATE_DAYS))?;
    let recent: Vec<_> = complete
        .iter()
        .copied()
        .filter(|day| day.date >= since)
        .collect();
    let clean: Vec<_> = recent
        .iter()
        .copied()
        .filter(|day| !day.recharged)
        .collect();
    let (daily_money, daily_energy) = if !clean.is_empty() {
        let count = clean.len() as f64;
        (
            clean.iter().map(|day| day.used_money).sum::<f64>() / count,
            clean.iter().map(|day| day.used_energy).sum::<f64>() / count,
        )
    } else {
        let priced: Vec<_> = complete
            .iter()
            .copied()
            .filter(|day| !day.recharged)
            .collect();
        let spent = priced.iter().map(|day| day.used_money).sum::<f64>();
        let used = priced.iter().map(|day| day.used_energy).sum::<f64>();
        if recent.is_empty() || !positive(spent) || !positive(used) {
            return None;
        }
        let daily_energy =
            recent.iter().map(|day| day.used_energy).sum::<f64>() / recent.len() as f64;
        (daily_energy * spent / used, daily_energy)
    };
    if !positive(daily_money) || !positive(daily_energy) {
        return None;
    }
    let days_remaining = money.max(0.0) / daily_money;
    if !days_remaining.is_finite() {
        return None;
    }
    Some(TrendEstimate {
        daily_money,
        daily_energy,
        days_remaining,
    })
}

#[cfg(test)]
pub(super) fn build_trend(records: &[PowerRecord], tz: Tz, today: NaiveDate) -> Trend {
    let mut builder = TrendBuilder::new(tz, today).unwrap();
    for record in records {
        builder.push(record);
    }
    builder.finish()
}

//! Calendar buckets for the Chat history sidebar.
//!
//! Grouping is a pure function of local `created_at` plus an injected now.
//! Favorites are exclusive: they never appear in a time bucket.

use chrono::{Datelike, Days, Local, TimeZone as _};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum HistorySectionKind {
    Favorites,
    Today,
    Yesterday,
    ThisWeek,
    ThisMonth,
    Earlier,
}

impl HistorySectionKind {
    pub(super) fn i18n_key(self) -> &'static str {
        match self {
            Self::Favorites => "sidebar.group_favorites",
            Self::Today => "sidebar.group_today",
            Self::Yesterday => "sidebar.group_yesterday",
            Self::ThisWeek => "sidebar.group_this_week",
            Self::ThisMonth => "sidebar.group_this_month",
            Self::Earlier => "sidebar.group_earlier",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TimeBucket {
    Today,
    Yesterday,
    ThisWeek,
    ThisMonth,
    Earlier,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum HistoryRow<P, S> {
    Pending(P),
    Catalog(S),
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct HistorySection<T> {
    pub kind: HistorySectionKind,
    pub rows: Vec<T>,
}

/// Assign a time bucket using the caller's local calendar.
pub(super) fn assign_time_bucket(now_millis: i64, created_at: i64) -> TimeBucket {
    let Some(now) = Local.timestamp_millis_opt(now_millis).single() else {
        return TimeBucket::Earlier;
    };
    let Some(created) = Local.timestamp_millis_opt(created_at).single() else {
        return TimeBucket::Earlier;
    };
    let today = now.date_naive();
    let created_date = created.date_naive();
    if created_date == today {
        return TimeBucket::Today;
    }
    if today.checked_sub_days(Days::new(1)) == Some(created_date) {
        return TimeBucket::Yesterday;
    }
    let week_start = today
        .checked_sub_days(Days::new(u64::from(today.weekday().num_days_from_monday())))
        .unwrap_or(today);
    if created_date >= week_start {
        TimeBucket::ThisWeek
    } else if created.year() == now.year() && created.month() == now.month() {
        TimeBucket::ThisMonth
    } else {
        TimeBucket::Earlier
    }
}

/// Build visible sections. Empty buckets are omitted. Pending rows always
/// belong to Today and are listed before that day's catalog rows.
pub(super) fn history_sections<P, S>(
    now_millis: i64,
    pending: impl IntoIterator<Item = P>,
    favorites: impl IntoIterator<Item = S>,
    timeline: impl IntoIterator<Item = S>,
    created_at: impl Fn(&S) -> i64,
) -> Vec<HistorySection<HistoryRow<P, S>>> {
    let mut sections = Vec::new();
    let favorite_rows: Vec<_> = favorites.into_iter().map(HistoryRow::Catalog).collect();
    if !favorite_rows.is_empty() {
        sections.push(HistorySection {
            kind: HistorySectionKind::Favorites,
            rows: favorite_rows,
        });
    }

    let mut today = Vec::new();
    let mut yesterday = Vec::new();
    let mut this_week = Vec::new();
    let mut this_month = Vec::new();
    let mut earlier = Vec::new();
    today.extend(pending.into_iter().map(HistoryRow::Pending));
    for summary in timeline {
        let bucket = assign_time_bucket(now_millis, created_at(&summary));
        let row = HistoryRow::Catalog(summary);
        match bucket {
            TimeBucket::Today => today.push(row),
            TimeBucket::Yesterday => yesterday.push(row),
            TimeBucket::ThisWeek => this_week.push(row),
            TimeBucket::ThisMonth => this_month.push(row),
            TimeBucket::Earlier => earlier.push(row),
        }
    }

    for (kind, rows) in [
        (HistorySectionKind::Today, today),
        (HistorySectionKind::Yesterday, yesterday),
        (HistorySectionKind::ThisWeek, this_week),
        (HistorySectionKind::ThisMonth, this_month),
        (HistorySectionKind::Earlier, earlier),
    ] {
        if !rows.is_empty() {
            sections.push(HistorySection { kind, rows });
        }
    }
    sections
}

#[cfg(test)]
mod tests {
    use super::{HistoryRow, HistorySectionKind, TimeBucket, assign_time_bucket, history_sections};
    use chrono::{Local, TimeZone as _};

    fn local_millis(year: i32, month: u32, day: u32, hour: u32) -> i64 {
        Local
            .with_ymd_and_hms(year, month, day, hour, 0, 0)
            .single()
            .expect("valid local datetime")
            .timestamp_millis()
    }

    #[test]
    fn today_and_yesterday_win_over_week_and_month() {
        // 2026-03-04 is a Wednesday.
        let now = local_millis(2026, 3, 4, 15);
        assert_eq!(
            assign_time_bucket(now, local_millis(2026, 3, 4, 9)),
            TimeBucket::Today
        );
        assert_eq!(
            assign_time_bucket(now, local_millis(2026, 3, 3, 18)),
            TimeBucket::Yesterday
        );
        assert_eq!(
            assign_time_bucket(now, local_millis(2026, 3, 2, 12)),
            TimeBucket::ThisWeek
        );
    }

    #[test]
    fn monday_yesterday_is_sunday_not_this_week() {
        // 2026-03-02 is a Monday.
        let now = local_millis(2026, 3, 2, 10);
        assert_eq!(
            assign_time_bucket(now, local_millis(2026, 3, 1, 22)),
            TimeBucket::Yesterday
        );
        assert_eq!(
            assign_time_bucket(now, local_millis(2026, 2, 27, 12)),
            TimeBucket::Earlier
        );
    }

    #[test]
    fn month_start_yesterday_is_previous_month() {
        // 2026-04-01 is a Wednesday.
        let now = local_millis(2026, 4, 1, 9);
        assert_eq!(
            assign_time_bucket(now, local_millis(2026, 3, 31, 20)),
            TimeBucket::Yesterday
        );
        assert_eq!(
            assign_time_bucket(now, local_millis(2026, 3, 20, 12)),
            TimeBucket::Earlier
        );
        assert_eq!(
            assign_time_bucket(now, local_millis(2026, 4, 1, 8)),
            TimeBucket::Today
        );
    }

    #[test]
    fn this_month_excludes_closer_buckets() {
        // 2026-03-20 is a Friday.
        let now = local_millis(2026, 3, 20, 12);
        assert_eq!(
            assign_time_bucket(now, local_millis(2026, 3, 8, 12)),
            TimeBucket::ThisMonth
        );
    }

    #[test]
    fn sections_omit_empty_buckets_and_keep_favorites_exclusive() {
        let now = local_millis(2026, 3, 4, 15);
        let sections = history_sections(
            now,
            ["draft"],
            ["starred"],
            ["today-chat", "older"],
            |title: &&str| match *title {
                "today-chat" => local_millis(2026, 3, 4, 8),
                _ => local_millis(2026, 1, 2, 8),
            },
        );
        let kinds: Vec<_> = sections.iter().map(|section| section.kind).collect();
        assert_eq!(
            kinds,
            [
                HistorySectionKind::Favorites,
                HistorySectionKind::Today,
                HistorySectionKind::Earlier
            ]
        );
        assert_eq!(sections[0].rows, vec![HistoryRow::Catalog("starred")]);
        assert_eq!(
            sections[1].rows,
            vec![
                HistoryRow::Pending("draft"),
                HistoryRow::Catalog("today-chat")
            ]
        );
        assert!(
            sections
                .iter()
                .all(|section| section.kind != HistorySectionKind::Yesterday)
        );
    }

    #[test]
    fn empty_favorites_omits_the_pinned_section() {
        let now = local_millis(2026, 3, 4, 15);
        let sections = history_sections(
            now,
            Vec::<&str>::new(),
            Vec::<&str>::new(),
            ["today-chat"],
            |_: &&str| local_millis(2026, 3, 4, 8),
        );
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].kind, HistorySectionKind::Today);
    }
}

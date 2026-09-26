//! Pure helpers ported from `server/lib/scheduled-tasks/runtime.js`:
//! `computeNextRunAt`, `formatScheduledSessionTitle`,
//! `parseScheduledCommandPrompt`, `expandCommandGoalObjective`.
//!
//! DateTime math is fixed-offset (DST-free) via `super::timeutil`; the zone's
//! offset is resolved at the evaluation instant (gap vs luxon noted in
//! PORT-MANIFEST.md).

use crate::projects::{Schedule, ScheduledTask};

use super::cron::CronExpr;
use super::timeutil::{
    Civil, civil_from_utc_ms, days_in_month, format_stamp, local_zone_name, offset_or_utc,
    utc_ms_from_civil,
};

pub const TASK_DUE_SLACK_MS: i64 = 5_000;
pub const TASK_TITLE_MAX_LENGTH: usize = 120;

fn parse_time_parts(time: &str) -> Option<(u32, u32)> {
    let bytes = time.as_bytes();
    if bytes.len() != 5 || bytes[2] != b':' {
        return None;
    }
    let h1 = bytes[0].checked_sub(b'0')?;
    let h2 = bytes[1].checked_sub(b'0')?;
    let m1 = bytes[3].checked_sub(b'0')?;
    let m2 = bytes[4].checked_sub(b'0')?;
    if h1 > 9 || h2 > 9 || m1 > 9 || m2 > 9 {
        return None;
    }
    let hour = h1 * 10 + h2;
    let minute = m1 * 10 + m2;
    if hour <= 23 && minute <= 59 {
        Some((hour as u32, minute as u32))
    } else {
        None
    }
}

fn valid_time(time: &str) -> bool {
    parse_time_parts(time).is_some()
}

/// `resolveScheduleTimes`: valid times only, de-duplicated, sorted.
fn resolve_schedule_times(times: &[String]) -> Vec<String> {
    let mut filtered: Vec<String> = times.iter().filter(|t| valid_time(t)).cloned().collect();
    filtered.sort();
    filtered.dedup();
    filtered
}

fn valid_date(date: &str) -> bool {
    let bytes = date.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let Ok(year) = date[..4].parse::<i64>() else {
        return false;
    };
    let Ok(month) = date[5..7].parse::<u32>() else {
        return false;
    };
    let Ok(day) = date[8..10].parse::<u32>() else {
        return false;
    };
    if !(1..=12).contains(&month) {
        return false;
    }
    (1..=days_in_month(year, month)).contains(&day)
}

fn schedule_zone(schedule: &Schedule) -> String {
    match schedule.timezone() {
        tz if !tz.trim().is_empty() => tz.trim().to_string(),
        _ => local_zone_name(),
    }
}

/// Apply an `HH:mm` time to a civil date (zeroing seconds), returning UTC ms.
fn apply_time_to_date(base: &Civil, time: &str, offset_seconds: i64) -> Option<i64> {
    let (hour, minute) = parse_time_parts(time)?;
    let candidate = Civil {
        hour,
        minute,
        second: 0,
        ..*base
    };
    Some(utc_ms_from_civil(&candidate, offset_seconds))
}

/// `computeNextRunAt(task, nowMs)`: next dispatch instant for an enabled
/// task, or `None` (JS `null`) for disabled tasks, invalid schedules, past
/// one-time slots, and invalid cron expressions.
pub fn compute_next_run_at(task: &ScheduledTask, now_ms: i64) -> Option<i64> {
    if !task.enabled {
        return None;
    }
    let zone = schedule_zone(&task.schedule);
    let offset = offset_or_utc(&zone, now_ms);
    let now = civil_from_utc_ms(now_ms, offset);
    let min_allowed = now_ms + TASK_DUE_SLACK_MS;

    match &task.schedule {
        Schedule::Daily { times, .. } => {
            let times = resolve_schedule_times(times);
            if times.is_empty() {
                return None;
            }
            let today = Civil {
                hour: 0,
                minute: 0,
                second: 0,
                ..now
            };
            for time in &times {
                if let Some(candidate) = apply_time_to_date(&today, time, offset)
                    && candidate > min_allowed
                {
                    return Some(candidate);
                }
            }
            // Tomorrow at the earliest configured time.
            let tomorrow_days = now.days() + 1;
            let (y, m, d) = super::timeutil::civil_from_days(tomorrow_days);
            let tomorrow = Civil {
                year: y,
                month: m,
                day: d,
                hour: 0,
                minute: 0,
                second: 0,
            };
            apply_time_to_date(&tomorrow, &times[0], offset)
        }
        Schedule::Weekly {
            times, weekdays, ..
        } => {
            if weekdays.is_empty() {
                return None;
            }
            let times = resolve_schedule_times(times);
            if times.is_empty() {
                return None;
            }
            for day_offset in 0..=14i64 {
                let (year, month, day_of_month) =
                    super::timeutil::civil_from_days(now.days() + day_offset);
                let day = Civil {
                    year,
                    month,
                    day: day_of_month,
                    hour: 0,
                    minute: 0,
                    second: 0,
                };
                let weekday = super::timeutil::weekday_from_days(now.days() + day_offset);
                if !weekdays.contains(&(weekday as u8)) {
                    continue;
                }
                for time in &times {
                    if let Some(candidate) = apply_time_to_date(&day, time, offset)
                        && candidate > min_allowed
                    {
                        return Some(candidate);
                    }
                }
            }
            None
        }
        Schedule::Once { date, time, .. } => {
            if !valid_date(date) || !valid_time(time) {
                return None;
            }
            let (year, month, day) = {
                let y: i64 = date[..4].parse().ok()?;
                let m: u32 = date[5..7].parse().ok()?;
                let d: u32 = date[8..10].parse().ok()?;
                (y, m, d)
            };
            let (hour, minute) = parse_time_parts(time)?;
            let candidate = Civil {
                year,
                month,
                day,
                hour,
                minute,
                second: 0,
            };
            let ms = utc_ms_from_civil(&candidate, offset);
            if ms <= min_allowed {
                return None;
            }
            Some(ms)
        }
        Schedule::Cron { cron, .. } => {
            let expr = CronExpr::parse(cron).ok()?;
            expr.next_after(now_ms, offset)
        }
    }
}

/// `formatScheduledSessionTitle`: "<task name> yyyy-LL-dd HH:mm" with the
/// name clamped so the full title stays within 120 characters.
pub fn format_scheduled_session_title(task: &ScheduledTask, now_ms: i64) -> String {
    let zone = schedule_zone(&task.schedule);
    let offset = offset_or_utc(&zone, now_ms);
    let stamp = format_stamp(now_ms, offset);
    let task_name = {
        let trimmed = task.name.trim();
        if trimmed.is_empty() {
            "Scheduled task"
        } else {
            trimmed
        }
    };
    let suffix = format!(" {stamp}");
    let max_task_name_length = TASK_TITLE_MAX_LENGTH
        .saturating_sub(suffix.chars().count())
        .max(1);
    let trimmed_name: String = task_name.chars().take(max_task_name_length).collect();
    format!("{trimmed_name}{suffix}")
}

/// `parseScheduledCommandPrompt`: `{"command", "arguments"}` when the prompt
/// starts with a slash command, else `None`.
pub fn parse_scheduled_command_prompt(prompt: &str) -> Option<(String, String)> {
    let trimmed = prompt.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let first_line = trimmed.split(['\n', '\r']).next().unwrap_or("");
    let mut tokens = first_line.split_whitespace();
    let head = tokens.next().unwrap_or("");
    let command = head[1..].trim();
    if command.is_empty() {
        return None;
    }
    let arguments = tokens.collect::<Vec<_>>().join(" ");
    Some((command.to_string(), arguments.trim().to_string()))
}

/// `expandCommandGoalObjective`: `$ARGUMENTS` / `$1..$N` substitution into a
/// command template; positional args are quoted-token split and the highest
/// position absorbs the remainder.
pub fn expand_command_goal_objective(
    template: Option<&str>,
    arguments_text: &str,
) -> Option<String> {
    let template = template?;
    if template.trim().is_empty() {
        return None;
    }
    if template.contains("$ARGUMENTS") {
        return Some(template.replace("$ARGUMENTS", arguments_text));
    }

    let positions: Vec<usize> = template
        .match_indices('$')
        .filter_map(|(i, _)| {
            let rest = &template[i + 1..];
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                None
            } else {
                digits.parse::<usize>().ok()
            }
        })
        .collect();
    if !positions.is_empty() {
        let parsed_arguments = split_quoted_arguments(arguments_text);
        let last_position = *positions.iter().max()?;
        let mut result = String::with_capacity(template.len());
        let mut rest = template;
        while let Some(idx) = rest.find('$') {
            result.push_str(&rest[..idx]);
            let after = &rest[idx + 1..];
            let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                result.push('$');
                rest = after;
                continue;
            }
            let position: usize = digits.parse().ok()?;
            let replacement: String = if position == last_position {
                parsed_arguments
                    .iter()
                    .skip(position.saturating_sub(1))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                parsed_arguments
                    .get(position.saturating_sub(1))
                    .cloned()
                    .unwrap_or_default()
            };
            result.push_str(&replacement);
            rest = &after[digits.len()..];
        }
        result.push_str(rest);
        return Some(result);
    }

    Some(if arguments_text.is_empty() {
        template.to_string()
    } else {
        format!("{template}\n\n{arguments_text}")
    })
}

fn split_quoted_arguments(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '"' || c == '\'' {
            let quote = c;
            let start = i + 1;
            let mut j = start;
            while j < chars.len() && chars[j] != quote {
                j += 1;
            }
            out.push(chars[start..j.min(chars.len())].iter().collect());
            i = (j + 1).min(chars.len());
        } else if c.is_whitespace() {
            i += 1;
        } else {
            let start = i;
            while i < chars.len() && !chars[i].is_whitespace() {
                i += 1;
            }
            out.push(chars[start..i].iter().collect());
        }
    }
    out
}

/// `safeErrorMessage`: trimmed, non-empty, clamped.
pub fn safe_error_message(raw: &str, max_length: usize) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "Unknown error".to_string();
    }
    if trimmed.chars().count() > max_length {
        trimmed.chars().take(max_length).collect()
    } else {
        trimmed.to_string()
    }
}

/// `buildGoalIntroText` from `session-goal/create.js` — the synthetic goal
/// reminder part injected when a task runs with goal mode enabled.
pub fn build_goal_intro_text(token_budget: Option<u64>) -> String {
    let budget_line = token_budget
        .map(|budget| format!(" A token budget of {budget} tokens applies to this goal."))
        .unwrap_or_default();
    "<system-reminder>\n".to_string()
        + "Goal mode is active for this session. The user message above defines the goal objective. "
        + "Work toward it across turns; whenever you stop before the objective is verifiably complete, the system will automatically prompt you to continue. "
        + "Progress is evaluated independently after each turn, so end every turn with a clear, factual statement of what is done, what was verified, and what remains."
        + &budget_line
        + "\n</system-reminder>"
}

#[cfg(test)]
mod tests {
    use crate::projects::{Execution, TaskState};

    use super::*;

    fn make_task(schedule: Schedule, enabled: bool) -> ScheduledTask {
        ScheduledTask {
            id: "task-1".into(),
            name: "Daily Sync".into(),
            enabled,
            schedule,
            execution: Execution {
                prompt: "Summarize open issues".into(),
                provider_id: Some("openai".into()),
                model_id: Some("gpt-4o".into()),
                model_role: None,
                variant: None,
                agent: None,
                goal_enabled: None,
                goal_token_budget: None,
                permission_auto_accept: None,
            },
            state: TaskState::default(),
            loop_file: None,
        }
    }

    fn utc(y: i64, m: u32, d: u32, h: u32, mi: u32) -> i64 {
        utc_ms_from_civil(
            &Civil {
                year: y,
                month: m,
                day: d,
                hour: h,
                minute: mi,
                second: 0,
            },
            0,
        )
    }

    #[test]
    fn computes_next_daily_run_in_timezone() {
        let now = utc(2025, 1, 1, 8, 0);
        let task = make_task(
            Schedule::Daily {
                times: vec!["09:30".into()],
                timezone: "UTC".into(),
            },
            true,
        );
        assert_eq!(
            compute_next_run_at(&task, now),
            Some(utc(2025, 1, 1, 9, 30))
        );
    }

    #[test]
    fn disabled_task_has_no_next_run() {
        let task = make_task(
            Schedule::Daily {
                times: vec!["09:30".into()],
                timezone: "UTC".into(),
            },
            false,
        );
        assert_eq!(compute_next_run_at(&task, utc(2025, 1, 1, 8, 0)), None);
    }

    #[test]
    fn computes_weekly_next_run_using_weekdays() {
        // Monday 2025-01-06 10:00 UTC → Wednesday 09:00.
        let task = make_task(
            Schedule::Weekly {
                times: vec!["09:00".into()],
                weekdays: vec![1, 3],
                timezone: "UTC".into(),
            },
            true,
        );
        assert_eq!(
            compute_next_run_at(&task, utc(2025, 1, 6, 10, 0)),
            Some(utc(2025, 1, 8, 9, 0))
        );
    }

    #[test]
    fn picks_nearest_time_from_multiple_daily_times() {
        let task = make_task(
            Schedule::Daily {
                times: vec!["09:15".into(), "09:45".into(), "18:00".into()],
                timezone: "UTC".into(),
            },
            true,
        );
        assert_eq!(
            compute_next_run_at(&task, utc(2025, 1, 1, 9, 20)),
            Some(utc(2025, 1, 1, 9, 45))
        );
    }

    #[test]
    fn daily_time_inside_due_slack_rolls_to_tomorrow() {
        // 09:00:02 is within the 5s slack of 09:00 → next is tomorrow 09:00.
        let task = make_task(
            Schedule::Daily {
                times: vec!["09:00".into()],
                timezone: "UTC".into(),
            },
            true,
        );
        let now = utc(2025, 1, 1, 9, 0) + 2_000;
        assert_eq!(compute_next_run_at(&task, now), Some(utc(2025, 1, 2, 9, 0)));
    }

    #[test]
    fn computes_one_time_next_run_for_future_date() {
        let task = make_task(
            Schedule::Once {
                date: "2026-04-16".into(),
                time: "13:30".into(),
                timezone: "UTC".into(),
            },
            true,
        );
        assert_eq!(
            compute_next_run_at(&task, utc(2026, 4, 15, 10, 0)),
            Some(utc(2026, 4, 16, 13, 30))
        );
    }

    #[test]
    fn returns_null_for_past_one_time_schedule() {
        let task = make_task(
            Schedule::Once {
                date: "2026-04-16".into(),
                time: "13:30".into(),
                timezone: "UTC".into(),
            },
            true,
        );
        assert_eq!(compute_next_run_at(&task, utc(2026, 4, 16, 14, 0)), None);
    }

    #[test]
    fn rejects_invalid_once_dates() {
        for date in ["2026-02-30", "2026-13-01", "not-a-date"] {
            let task = make_task(
                Schedule::Once {
                    date: date.into(),
                    time: "13:30".into(),
                    timezone: "UTC".into(),
                },
                true,
            );
            assert_eq!(
                compute_next_run_at(&task, utc(2026, 1, 1, 0, 0)),
                None,
                "{date}"
            );
        }
    }

    #[test]
    fn computes_cron_next_run() {
        let task = make_task(
            Schedule::Cron {
                cron: "*/5 * * * *".into(),
                timezone: "UTC".into(),
            },
            true,
        );
        assert_eq!(
            compute_next_run_at(&task, utc(2026, 1, 1, 14, 3)),
            Some(utc(2026, 1, 1, 14, 5))
        );
        // Invalid cron → null (JS catch path).
        let bad = make_task(
            Schedule::Cron {
                cron: "not a cron".into(),
                timezone: "UTC".into(),
            },
            true,
        );
        assert_eq!(compute_next_run_at(&bad, utc(2026, 1, 1, 0, 0)), None);
    }

    #[test]
    fn formats_session_title_with_timestamp_suffix() {
        let mut task = make_task(
            Schedule::Cron {
                cron: "* * * * *".into(),
                timezone: "UTC".into(),
            },
            true,
        );
        task.name = "Morning Sync".into();
        assert_eq!(
            format_scheduled_session_title(&task, utc(2025, 3, 10, 7, 5)),
            "Morning Sync 2025-03-10 07:05"
        );
        // Long names are clamped so the total stays ≤ 120 chars.
        task.name = "N".repeat(200);
        let title = format_scheduled_session_title(&task, utc(2025, 3, 10, 7, 5));
        assert!(title.chars().count() <= 120);
        assert!(title.ends_with(" 2025-03-10 07:05"));
        // Empty name falls back.
        task.name = "   ".into();
        assert!(
            format_scheduled_session_title(&task, utc(2025, 3, 10, 7, 5))
                .starts_with("Scheduled task ")
        );
    }

    #[test]
    fn parses_slash_command_prompt() {
        assert_eq!(
            parse_scheduled_command_prompt("/review src/components"),
            Some(("review".into(), "src/components".into()))
        );
        assert_eq!(
            parse_scheduled_command_prompt("/multi one   two\r\nsecond line"),
            Some(("multi".into(), "one two".into()))
        );
        assert_eq!(
            parse_scheduled_command_prompt("Summarize open issues"),
            None
        );
        assert_eq!(parse_scheduled_command_prompt("/"), None);
    }

    #[test]
    fn expands_command_arguments_into_the_goal_objective() {
        assert_eq!(
            expand_command_goal_objective(
                Some("Run the issue pipeline for $ARGUMENTS. Verify $ARGUMENTS is represented by the PR."),
                "LIN-123 --draft"
            )
            .as_deref(),
            Some("Run the issue pipeline for LIN-123 --draft. Verify LIN-123 --draft is represented by the PR.")
        );
        assert_eq!(expand_command_goal_objective(None, "LIN-123"), None);
        assert_eq!(
            expand_command_goal_objective(Some("Move $1 to $2"), "\"src old\" dist extra")
                .as_deref(),
            Some("Move src old to dist extra")
        );
        assert_eq!(
            expand_command_goal_objective(Some("Review the requested scope."), "auth module")
                .as_deref(),
            Some("Review the requested scope.\n\nauth module")
        );
        assert_eq!(
            expand_command_goal_objective(Some("Review the requested scope."), "").as_deref(),
            Some("Review the requested scope.")
        );
    }

    #[test]
    fn safe_error_message_trims_and_clamps() {
        assert_eq!(safe_error_message("  boom  ", 100), "boom");
        assert_eq!(safe_error_message("   ", 100), "Unknown error");
        let long = "x".repeat(3000);
        assert_eq!(safe_error_message(&long, 2000).chars().count(), 2000);
    }

    #[test]
    fn goal_intro_text_matches_js_shape() {
        assert_eq!(
            build_goal_intro_text(None),
            "<system-reminder>\nGoal mode is active for this session. The user message above defines the goal objective. Work toward it across turns; whenever you stop before the objective is verifiably complete, the system will automatically prompt you to continue. Progress is evaluated independently after each turn, so end every turn with a clear, factual statement of what is done, what was verified, and what remains.\n</system-reminder>"
        );
        assert!(
            build_goal_intro_text(Some(1000))
                .contains("A token budget of 1000 tokens applies to this goal.")
        );
    }
}

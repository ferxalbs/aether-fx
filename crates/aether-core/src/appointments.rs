use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};

use crate::{AppointmentId, BoundedText, CoreError, CoreResult};

/// Maximum bytes retained for an appointment title.
pub const MAX_APPOINTMENT_TITLE_BYTES: usize = 256;
/// Maximum bytes retained for an appointment location.
pub const MAX_APPOINTMENT_LOCATION_BYTES: usize = 512;
/// Maximum bytes retained for appointment notes.
pub const MAX_APPOINTMENT_NOTES_BYTES: usize = 4 * 1024;
/// Maximum bytes retained for one attendee label.
pub const MAX_APPOINTMENT_ATTENDEE_BYTES: usize = 320;
/// Maximum attendees accepted by the local appointment workflow.
pub const MAX_APPOINTMENT_ATTENDEES: usize = 16;
/// Smallest accepted appointment duration in minutes.
pub const MIN_APPOINTMENT_DURATION_MINUTES: u16 = 1;
/// Largest accepted appointment duration in minutes.
pub const MAX_APPOINTMENT_DURATION_MINUTES: u16 = 24 * 60;

/// Fields collected by the interactive appointment form before normalization.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AppointmentDraft {
    /// Human-readable appointment title.
    pub title: String,
    /// Local calendar date in `YYYY-MM-DD` form.
    pub date: String,
    /// Local clock time in `HH:MM` form.
    pub time: String,
    /// Explicit signed UTC offset in `+HH:MM` or `-HH:MM` form.
    pub utc_offset: String,
    /// Duration entered as decimal minutes.
    pub duration_minutes: String,
    /// Optional bounded location label.
    pub location: Option<String>,
    /// Optional bounded notes.
    pub notes: Option<String>,
    /// Optional bounded attendee labels.
    pub attendees: Vec<String>,
}

/// A validated appointment stored in private AETHER state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Appointment {
    /// Locally generated appointment identity.
    pub id: AppointmentId,
    /// Non-empty bounded title.
    pub title: BoundedText,
    /// Canonical UTC RFC 3339 start instant.
    pub starts_at: String,
    /// Explicit offset used when the appointment was entered, in minutes.
    pub utc_offset_minutes: i32,
    /// Duration in minutes.
    pub duration_minutes: u16,
    /// Optional bounded location.
    pub location: Option<BoundedText>,
    /// Optional bounded notes.
    pub notes: Option<BoundedText>,
    /// Optional bounded attendee labels.
    pub attendees: Vec<BoundedText>,
    /// Canonical UTC creation timestamp.
    pub created_at: String,
}

impl AppointmentDraft {
    /// Validate and normalize the form fields into a provider-neutral appointment.
    pub fn normalize(
        &self,
        id: AppointmentId,
        created_at: OffsetDateTime,
    ) -> CoreResult<Appointment> {
        let title =
            required_text(&self.title, MAX_APPOINTMENT_TITLE_BYTES, "appointment title", false)?;
        let (starts_at, utc_offset_minutes) =
            parse_start(&self.date, &self.time, &self.utc_offset)?;
        let duration_minutes = self.duration_minutes.trim().parse::<u16>().map_err(|_| {
            CoreError::invalid("appointment duration", "duration must be an integer in minutes")
        })?;
        if !(MIN_APPOINTMENT_DURATION_MINUTES..=MAX_APPOINTMENT_DURATION_MINUTES)
            .contains(&duration_minutes)
        {
            return Err(CoreError::invalid(
                "appointment duration",
                "duration must be between 1 and 1440 minutes",
            ));
        }

        let location = optional_text(
            self.location.as_deref(),
            MAX_APPOINTMENT_LOCATION_BYTES,
            "appointment location",
            false,
        )?;
        let notes = optional_text(
            self.notes.as_deref(),
            MAX_APPOINTMENT_NOTES_BYTES,
            "appointment notes",
            true,
        )?;
        if self.attendees.len() > MAX_APPOINTMENT_ATTENDEES {
            return Err(CoreError::invalid(
                "appointment attendees",
                "no more than 16 attendees are allowed",
            ));
        }
        let attendees = self
            .attendees
            .iter()
            .map(|attendee| {
                required_text(
                    attendee,
                    MAX_APPOINTMENT_ATTENDEE_BYTES,
                    "appointment attendee",
                    false,
                )
            })
            .collect::<CoreResult<Vec<_>>>()?;

        let created_at = created_at
            .to_offset(UtcOffset::UTC)
            .format(&Rfc3339)
            .map_err(|error| CoreError::invalid("appointment creation time", error.to_string()))?;

        let appointment = Appointment {
            id,
            title,
            starts_at,
            utc_offset_minutes,
            duration_minutes,
            location,
            notes,
            attendees,
            created_at,
        };
        appointment.validate()?;
        Ok(appointment)
    }
}

impl Appointment {
    /// Validate a stored appointment loaded from private state.
    pub fn validate(&self) -> CoreResult<()> {
        AppointmentId::new(self.id.as_str().to_owned())?;
        validate_bounded(&self.title, MAX_APPOINTMENT_TITLE_BYTES, "appointment title", false)?;
        if !(-1439..=1439).contains(&self.utc_offset_minutes) {
            return Err(CoreError::invalid(
                "appointment UTC offset",
                "offset must be between -23:59 and +23:59",
            ));
        }
        if !(MIN_APPOINTMENT_DURATION_MINUTES..=MAX_APPOINTMENT_DURATION_MINUTES)
            .contains(&self.duration_minutes)
        {
            return Err(CoreError::invalid(
                "appointment duration",
                "duration must be between 1 and 1440 minutes",
            ));
        }
        let starts_at = OffsetDateTime::parse(&self.starts_at, &Rfc3339)
            .map_err(|_| CoreError::invalid("appointment start", "start instant is invalid"))?;
        let canonical_start = starts_at
            .to_offset(UtcOffset::UTC)
            .format(&Rfc3339)
            .map_err(|error| CoreError::invalid("appointment start", error.to_string()))?;
        if canonical_start != self.starts_at {
            return Err(CoreError::invalid(
                "appointment start",
                "start instant is not canonical UTC RFC 3339",
            ));
        }
        let created_at = OffsetDateTime::parse(&self.created_at, &Rfc3339).map_err(|_| {
            CoreError::invalid("appointment creation time", "creation time is invalid")
        })?;
        let canonical_created_at = created_at
            .to_offset(UtcOffset::UTC)
            .format(&Rfc3339)
            .map_err(|error| CoreError::invalid("appointment creation time", error.to_string()))?;
        if canonical_created_at != self.created_at {
            return Err(CoreError::invalid(
                "appointment creation time",
                "creation time is not canonical UTC RFC 3339",
            ));
        }
        validate_optional_bounded(
            self.location.as_ref(),
            MAX_APPOINTMENT_LOCATION_BYTES,
            "appointment location",
            false,
        )?;
        validate_optional_bounded(
            self.notes.as_ref(),
            MAX_APPOINTMENT_NOTES_BYTES,
            "appointment notes",
            true,
        )?;
        if self.attendees.len() > MAX_APPOINTMENT_ATTENDEES {
            return Err(CoreError::invalid(
                "appointment attendees",
                "no more than 16 attendees are allowed",
            ));
        }
        for attendee in &self.attendees {
            validate_bounded(
                attendee,
                MAX_APPOINTMENT_ATTENDEE_BYTES,
                "appointment attendee",
                false,
            )?;
        }
        Ok(())
    }
}

fn parse_start(date: &str, clock: &str, offset: &str) -> CoreResult<(String, i32)> {
    let date = date.trim();
    let clock = clock.trim();
    let offset = offset.trim();
    if !matches_shape(date, 10, &[4, 7], b'-')
        || !date.bytes().all(|byte| byte.is_ascii_digit() || byte == b'-')
    {
        return Err(CoreError::invalid("appointment date", "date must use YYYY-MM-DD"));
    }
    if !matches_shape(clock, 5, &[2], b':')
        || !clock.bytes().all(|byte| byte.is_ascii_digit() || byte == b':')
    {
        return Err(CoreError::invalid("appointment time", "time must use HH:MM"));
    }
    if offset.len() != 6
        || !matches!(offset.as_bytes().first(), Some(b'+' | b'-'))
        || offset.as_bytes().get(3) != Some(&b':')
        || !offset
            .bytes()
            .enumerate()
            .all(|(index, byte)| index == 0 || index == 3 || byte.is_ascii_digit())
    {
        return Err(CoreError::invalid(
            "appointment UTC offset",
            "offset must use +HH:MM or -HH:MM",
        ));
    }
    let offset_hours = offset[1..3]
        .parse::<i32>()
        .map_err(|_| CoreError::invalid("appointment UTC offset", "offset hours are invalid"))?;
    let offset_minutes = offset[4..6]
        .parse::<i32>()
        .map_err(|_| CoreError::invalid("appointment UTC offset", "offset minutes are invalid"))?;
    if offset_hours > 23 || offset_minutes > 59 {
        return Err(CoreError::invalid(
            "appointment UTC offset",
            "offset must be between -23:59 and +23:59",
        ));
    }
    let sign = if offset.as_bytes()[0] == b'-' { -1 } else { 1 };
    let utc_offset_minutes = sign * (offset_hours * 60 + offset_minutes);
    let parsed = OffsetDateTime::parse(&format!("{date}T{clock}:00{offset}"), &Rfc3339)
        .map_err(|_| CoreError::invalid("appointment start", "date and time are not valid"))?;
    let starts_at = parsed
        .to_offset(UtcOffset::UTC)
        .format(&Rfc3339)
        .map_err(|error| CoreError::invalid("appointment start", error.to_string()))?;
    Ok((starts_at, utc_offset_minutes))
}

fn matches_shape(value: &str, length: usize, separators: &[usize], separator: u8) -> bool {
    value.len() == length
        && separators.iter().all(|index| value.as_bytes().get(*index) == Some(&separator))
}

fn required_text(
    value: &str,
    max_bytes: usize,
    operation: &str,
    allow_newlines: bool,
) -> CoreResult<BoundedText> {
    let value = value.trim();
    if value.is_empty() {
        return Err(CoreError::invalid(operation, "value must not be empty"));
    }
    if value.len() > max_bytes {
        return Err(CoreError::invalid(operation, "value exceeds its byte limit"));
    }
    if value
        .chars()
        .any(|character| character.is_control() && !(allow_newlines && character == '\n'))
    {
        return Err(CoreError::invalid(operation, "value contains terminal control characters"));
    }
    Ok(BoundedText::new(value, max_bytes))
}

fn optional_text(
    value: Option<&str>,
    max_bytes: usize,
    operation: &str,
    allow_newlines: bool,
) -> CoreResult<Option<BoundedText>> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    required_text(value, max_bytes, operation, allow_newlines).map(Some)
}

fn validate_optional_bounded(
    value: Option<&BoundedText>,
    max_bytes: usize,
    operation: &str,
    allow_newlines: bool,
) -> CoreResult<()> {
    if let Some(value) = value {
        validate_bounded(value, max_bytes, operation, allow_newlines)?;
    }
    Ok(())
}

fn validate_bounded(
    value: &BoundedText,
    max_bytes: usize,
    operation: &str,
    allow_newlines: bool,
) -> CoreResult<()> {
    let text = value.as_str();
    if text.trim().is_empty() {
        return Err(CoreError::invalid(operation, "value must not be empty"));
    }
    if text.len() > max_bytes {
        return Err(CoreError::invalid(operation, "value exceeds its byte limit"));
    }
    if text
        .chars()
        .any(|character| character.is_control() && !(allow_newlines && character == '\n'))
    {
        return Err(CoreError::invalid(operation, "value contains terminal control characters"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> AppointmentDraft {
        AppointmentDraft {
            title: "Design review".to_owned(),
            date: "2026-09-03".to_owned(),
            time: "14:30".to_owned(),
            utc_offset: "-05:00".to_owned(),
            duration_minutes: "45".to_owned(),
            location: Some("Studio".to_owned()),
            notes: Some("Bring the latest flow".to_owned()),
            attendees: vec!["Alex".to_owned()],
        }
    }

    #[test]
    fn normalizes_explicit_local_time_to_utc() {
        let appointment = draft()
            .normalize(
                AppointmentId::new("appointment-1").unwrap(),
                OffsetDateTime::parse("2026-09-03T12:00:00Z", &Rfc3339).unwrap(),
            )
            .unwrap();
        assert_eq!(appointment.starts_at, "2026-09-03T19:30:00Z");
        assert_eq!(appointment.utc_offset_minutes, -300);
        assert_eq!(appointment.duration_minutes, 45);
    }

    #[test]
    fn rejects_invalid_time_and_duration() {
        let mut invalid = draft();
        invalid.date = "2026-02-30".to_owned();
        assert!(
            invalid
                .normalize(AppointmentId::new("appointment-1").unwrap(), OffsetDateTime::now_utc(),)
                .is_err()
        );

        invalid = draft();
        invalid.duration_minutes = "0".to_owned();
        assert!(
            invalid
                .normalize(AppointmentId::new("appointment-1").unwrap(), OffsetDateTime::now_utc(),)
                .is_err()
        );

        invalid = draft();
        invalid.utc_offset = "UTC".to_owned();
        assert!(
            invalid
                .normalize(AppointmentId::new("appointment-1").unwrap(), OffsetDateTime::now_utc(),)
                .is_err()
        );

        invalid = draft();
        invalid.title.clear();
        assert!(
            invalid
                .normalize(AppointmentId::new("appointment-1").unwrap(), OffsetDateTime::now_utc(),)
                .is_err()
        );

        invalid = draft();
        invalid.attendees =
            (0..=MAX_APPOINTMENT_ATTENDEES).map(|index| format!("attendee-{index}")).collect();
        assert!(
            invalid
                .normalize(AppointmentId::new("appointment-1").unwrap(), OffsetDateTime::now_utc(),)
                .is_err()
        );
    }
}

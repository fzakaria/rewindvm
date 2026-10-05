//! The step typed into the step readout: a step number, written the way
//! the app prints them or plain, or a distance from the playhead.

/// Characters a typed step may group its digits with: the app's own
/// thousands separator, and the ones people type.
const SEPARATORS: [char; 4] = [',', '_', ' ', '\u{202f}'];

/// Why a typed step was not taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepEntryError {
    /// Nothing was typed.
    Empty,
    /// What was typed is not a number.
    NotANumber,
    /// The step is past the run's last one.
    PastEnd { total: u64 },
}

impl std::fmt::Display for StepEntryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StepEntryError::Empty => write!(f, "type a step"),
            StepEntryError::NotANumber => write!(f, "not a step number"),
            StepEntryError::PastEnd { total } => {
                write!(f, "past the end at {}", crate::describe::thousands(*total))
            }
        }
    }
}

/// The step `text` names, with the playhead at `at` in a run of `total`
/// steps. A number is a step; `+n` and `-n` move that far from the
/// playhead, stopping at either end, as stepping does.
pub fn parse_step(text: &str, at: u64, total: u64) -> Result<u64, StepEntryError> {
    let text: String = text
        .trim()
        .chars()
        .filter(|c| !SEPARATORS.contains(c))
        .collect();
    if text.is_empty() {
        return Err(StepEntryError::Empty);
    }

    // A distance from the playhead.
    let number = |digits: &str| {
        if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
            return Err(StepEntryError::NotANumber);
        }
        digits
            .parse::<u64>()
            .map_err(|_| StepEntryError::NotANumber)
    };
    if let Some(ahead) = text.strip_prefix('+') {
        return Ok(at.saturating_add(number(ahead)?).min(total));
    }
    if let Some(behind) = text.strip_prefix('-') {
        return Ok(at.saturating_sub(number(behind)?));
    }

    // A step of the run.
    let step = number(&text)?;
    if step > total {
        return Err(StepEntryError::PastEnd { total });
    }
    Ok(step)
}

#[cfg(test)]
mod tests {
    // Typed steps parsed with the playhead at 1,000 in a 5,000 step run.
    use super::*;

    const AT: u64 = 1_000;
    const TOTAL: u64 = 5_000;

    #[test]
    fn a_number_is_a_step_however_its_digits_are_grouped() {
        // Plain digits, the app's own commas, and the separators people
        // type all name the same step.
        assert_eq!(parse_step("3495", AT, TOTAL), Ok(3_495));
        assert_eq!(parse_step(" 3,495 ", AT, TOTAL), Ok(3_495));
        assert_eq!(parse_step("3_495", AT, TOTAL), Ok(3_495));
        assert_eq!(parse_step("0", AT, TOTAL), Ok(0));
        assert_eq!(parse_step("5000", AT, TOTAL), Ok(TOTAL));
    }

    #[test]
    fn a_sign_moves_from_the_playhead_and_stops_at_the_ends() {
        // +n and -n are distances from the playhead, stopped at the run's
        // first and last steps.
        assert_eq!(parse_step("+250", AT, TOTAL), Ok(1_250));
        assert_eq!(parse_step("-250", AT, TOTAL), Ok(750));
        assert_eq!(parse_step("-2,000", AT, TOTAL), Ok(0));
        assert_eq!(parse_step("+9,000", AT, TOTAL), Ok(TOTAL));
    }

    #[test]
    fn anything_else_says_why_it_was_not_taken() {
        // Nothing typed, words, a bare sign and a step past the end are
        // refused, each with its reason.
        assert_eq!(parse_step("  ", AT, TOTAL), Err(StepEntryError::Empty));
        assert_eq!(
            parse_step("end", AT, TOTAL),
            Err(StepEntryError::NotANumber)
        );
        assert_eq!(parse_step("+", AT, TOTAL), Err(StepEntryError::NotANumber));
        assert_eq!(
            parse_step("1.5", AT, TOTAL),
            Err(StepEntryError::NotANumber)
        );
        assert_eq!(
            parse_step("5001", AT, TOTAL),
            Err(StepEntryError::PastEnd { total: TOTAL })
        );
    }
}

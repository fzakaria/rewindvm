//! How rewind says why a command failed: in words for a person, or as a
//! line of JSON for a program (see [`rewind_trace::failure`]).

use rewind_core::run::WentAnotherWay;
use rewind_trace::failure::{Failure, Kind};

use crate::color;

/// How a failure is said, on standard error.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Format {
    /// Words after `rewind:`.
    #[default]
    Text,
    /// A line of JSON naming the failure's kind, with the words.
    Json,
}

/// The failure `error` is: the kind of the first error in its chain that
/// has one, under any context added on the way out, and every error's
/// words, as the text form gives them.
pub(crate) fn of(error: &anyhow::Error) -> Failure {
    let kind = error
        .chain()
        .find_map(|e| e.downcast_ref::<WentAnotherWay>())
        .map_or(Kind::Other, |w| Kind::WentAnotherWay {
            run: w.run.clone(),
            step: w.step,
        });
    Failure {
        kind,
        message: format!("{error:#}"),
    }
}

/// Says why the command failed, on standard error.
pub(crate) fn report(format: Format, error: &anyhow::Error) {
    match format {
        Format::Text => eprintln!("{} {error:#}", color::stderr().error_prefix()),
        Format::Json => eprintln!("{}", of(error).line()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use rewind_trace::manifest::RunId;

    #[test]
    fn a_failure_is_named_by_its_kind_under_its_context() {
        // `rewind check` adds why it gave up to a replay that went another
        // way; the kind is still the replay's, and the words are all of it.
        let run = RunId::parse("6380bb57fcb451da").unwrap();
        let replay = WentAnotherWay::new(&run, 311, rewind_core::VERSION);
        let error = Err::<(), _>(replay)
            .context("no schedule decides this")
            .unwrap_err();
        let failure = of(&error);
        assert_eq!(failure.kind, Kind::WentAnotherWay { run, step: 311 });
        assert!(
            failure.message.starts_with(
                "no schedule decides this: replaying run 6380bb57fcb451da went another way at step 311"
            ),
            "{}",
            failure.message
        );
    }

    #[test]
    fn any_other_failure_is_other() {
        let failure = of(&anyhow::anyhow!("the run has no process 7"));
        assert_eq!(failure.kind, Kind::Other);
        assert_eq!(failure.message, "the run has no process 7");
    }
}

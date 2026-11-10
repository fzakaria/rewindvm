//! Whether a run passed, and when it did not, why.
//!
//! The engine judges a run once, when the run ends, and writes the verdict
//! into the manifest beside the outcome. `rewind ls`, `check` and `show`,
//! the desktop app and the browser viewer read that field and never judge
//! the run again, so a job that breaks an `always` assertion and exits 0
//! is failed everywhere, not failed in one pane and passed in another.

use serde::{Deserialize, Serialize};

use crate::Trace;
use crate::ending::ExitStatus;
use crate::stop::Stop;

/// A run's verdict, as its manifest records it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Verdict {
    /// The run has not ended, so there is no verdict yet: the engine is
    /// executing it, or was killed before the run ended. The run's
    /// executing lock tells the two apart.
    Unfinished,
    /// The job exited 0 having created every output and broken no
    /// assertion, before the time limit.
    Passed,
    /// The run failed, for each of `reasons`, in the order the engine
    /// checks them.
    Failed { reasons: Vec<Reason> },
}

/// Why a run failed. A run can fail for more than one reason at once: a
/// job can break an assertion and then exit 2.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "why", rename_all = "snake_case")]
pub enum Reason {
    /// The machine reached its time limit before the job ended, which may
    /// be the host's doing as much as the job's.
    TimedOut,
    /// The job exited with a code other than 0.
    Exited { code: u32 },
    /// A signal killed the job.
    Killed { signal: u32, core: bool },
    /// The machine stopped before init reported the job's end, however it
    /// stopped: a triple fault, a halt, a stall with no timer armed, or a
    /// power-off init never reported through.
    Crashed,
    /// The job exited 0 without creating every output it was to, which
    /// nix-daemon counts as a failed build.
    MissingOutputs { paths: Vec<String> },
    /// The job broke what it asserted with the Rewind SDK: an `always`
    /// that failed, or an `unreachable` it reached, in step order.
    FailedAssertions { assertions: Vec<FailedAssertion> },
}

/// One thing the job asserted with the Rewind SDK and broke.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedAssertion {
    pub step: u64,
    pub pid: u32,
    pub tid: u32,
    /// What broke, in words: `always "even" failed: byte 221 is odd`.
    pub text: String,
}

impl Verdict {
    /// The verdict of a run whose machine stopped as `stop` and whose job
    /// init reported with wait status `status`, given `outputs`, the
    /// outputs the job was to create, and the run's trace. Every reason
    /// the run fails for is listed: a timeout, then how the job ended,
    /// then the outputs an exit of 0 did not create, then the assertions
    /// the job broke.
    pub fn judge(stop: &Stop, status: Option<i32>, outputs: &[String], trace: &Trace) -> Verdict {
        let mut reasons = Vec::new();

        // A timeout fails the run whatever init had reported.
        if stop.timeout().is_some() {
            reasons.push(Reason::TimedOut);
        }

        // How the job ended, when init reported its end at all.
        let exit = status.map(ExitStatus::from_wait);
        match exit {
            Some(ExitStatus::Code(0)) => {}
            Some(ExitStatus::Code(code)) => reasons.push(Reason::Exited { code }),
            Some(ExitStatus::Signal { signo, core }) => reasons.push(Reason::Killed {
                signal: signo,
                core,
            }),
            None if reasons.is_empty() => reasons.push(Reason::Crashed),
            None => {}
        }

        // Init hashes the outputs only after an exit of 0, so only then
        // is an output init never hashed one the job did not create.
        if exit.is_some_and(ExitStatus::success) {
            let built = trace.outputs();
            let missing: Vec<String> = outputs
                .iter()
                .filter(|path| !built.iter().any(|(p, _)| p == *path))
                .cloned()
                .collect();
            if !missing.is_empty() {
                reasons.push(Reason::MissingOutputs { paths: missing });
            }
        }

        // What the job asserted and broke, however it exited.
        let assertions = trace.failed_assertions();
        if !assertions.is_empty() {
            reasons.push(Reason::FailedAssertions { assertions });
        }

        if reasons.is_empty() {
            return Verdict::Passed;
        }
        Verdict::Failed { reasons }
    }

    /// Whether the run passed.
    pub fn passed(&self) -> bool {
        *self == Verdict::Passed
    }

    /// Whether the run failed. A run that has not ended neither passed
    /// nor failed.
    pub fn failed(&self) -> bool {
        matches!(self, Verdict::Failed { .. })
    }

    /// Whether the run has ended, and so has a verdict.
    pub fn is_final(&self) -> bool {
        !matches!(self, Verdict::Unfinished)
    }

    /// Why the run failed; none for a run that passed or has not ended.
    pub fn reasons(&self) -> &[Reason] {
        match self {
            Verdict::Failed { reasons } => reasons,
            Verdict::Unfinished | Verdict::Passed => &[],
        }
    }

    /// Whether the run was stopped at its time limit.
    pub fn timed_out(&self) -> bool {
        self.reasons().contains(&Reason::TimedOut)
    }

    /// The outputs the job was to create and did not.
    pub fn missing_outputs(&self) -> &[String] {
        self.reasons()
            .iter()
            .find_map(|r| match r {
                Reason::MissingOutputs { paths } => Some(paths.as_slice()),
                _ => None,
            })
            .unwrap_or(&[])
    }

    /// What the job asserted with the Rewind SDK and broke.
    pub fn failed_assertions(&self) -> &[FailedAssertion] {
        self.reasons()
            .iter()
            .find_map(|r| match r {
                Reason::FailedAssertions { assertions } => Some(assertions.as_slice()),
                _ => None,
            })
            .unwrap_or(&[])
    }
}

#[cfg(test)]
mod tests {
    // The verdict of a run from each way it can fail, judged over traces
    // built by hand, and the field as a manifest carries it.
    use super::*;
    use crate::stop::{Doing, Timeout};
    use crate::{Event, EventKind};

    const SIGSEGV: u32 = 11;
    const OUT: &str = "/nix/store/0000000000000000000000000000000b-x";
    const DEV: &str = "/nix/store/0000000000000000000000000000000c-x-dev";

    /// A mark `pid` wrote at `step`.
    fn mark(step: u64, pid: u32, text: &str) -> Event {
        Event {
            step,
            pid,
            tid: pid,
            kind: EventKind::Mark { text: text.into() },
        }
    }

    /// The trace of a job that built `OUT` and asserted as `sdk` says.
    fn trace(sdk: &[&str]) -> Trace {
        let mut events = vec![mark(
            90,
            rewind_init::INIT_PID,
            &format!("rewind-output {OUT} sha256:abcd"),
        )];
        for (i, line) in sdk.iter().enumerate() {
            events.push(mark(10 + i as u64, 7, line));
        }
        events.sort_by_key(|e| e.step);
        Trace {
            events,
            ..Default::default()
        }
    }

    fn hung() -> Stop {
        Stop::TimedOut(Timeout {
            since_exit_ms: 0,
            doing: Doing::MakingExits,
        })
    }

    #[test]
    fn a_job_that_exits_zero_with_its_outputs_and_no_broken_assertion_passes() {
        // Exit 0, every output built, and an `always` that held.
        let t = trace(&["rewind-sdk 1 always pass even"]);
        let verdict = Verdict::judge(&Stop::PoweredOff, Some(0), &[OUT.into()], &t);
        assert_eq!(verdict, Verdict::Passed);
        assert!(verdict.passed() && !verdict.failed() && verdict.is_final());
        assert!(verdict.reasons().is_empty());
    }

    #[test]
    fn each_way_a_run_fails_is_a_reason() {
        // A nonzero exit, a signal, a machine that stopped before init
        // reported the job's end, and a timeout each fail the run on their
        // own.
        let t = trace(&[]);
        let judge = |stop: &Stop, status| Verdict::judge(stop, status, &[], &t);
        assert_eq!(
            judge(&Stop::PoweredOff, Some(2 << 8)).reasons(),
            [Reason::Exited { code: 2 }]
        );
        assert_eq!(
            judge(&Stop::PoweredOff, Some(SIGSEGV as i32 | 0x80)).reasons(),
            [Reason::Killed {
                signal: SIGSEGV,
                core: true
            }]
        );
        assert_eq!(judge(&Stop::TripleFault, None).reasons(), [Reason::Crashed]);
        assert_eq!(judge(&Stop::PoweredOff, None).reasons(), [Reason::Crashed]);
        let hung = judge(&hung(), None);
        assert_eq!(hung.reasons(), [Reason::TimedOut]);
        assert!(hung.timed_out() && hung.failed());
    }

    #[test]
    fn a_timeout_fails_the_run_whatever_init_reported() {
        // A job that exited 0 inside a machine stopped at its time limit
        // timed out, and is not also taken as crashed.
        let t = trace(&[]);
        let verdict = Verdict::judge(&hung(), Some(0), &[OUT.into()], &t);
        assert_eq!(verdict.reasons(), [Reason::TimedOut]);
    }

    #[test]
    fn an_output_init_never_hashed_is_missing_only_after_an_exit_of_zero() {
        // Exit 0 without `DEV` built fails for that output; exit 1 fails
        // for the exit alone, since init hashes nothing after a failure.
        let t = trace(&[]);
        let outputs = [OUT.to_string(), DEV.to_string()];
        let unbuilt = Verdict::judge(&Stop::PoweredOff, Some(0), &outputs, &t);
        assert_eq!(
            unbuilt.reasons(),
            [Reason::MissingOutputs {
                paths: vec![DEV.into()]
            }]
        );
        assert_eq!(unbuilt.missing_outputs(), [DEV.to_string()]);
        let failed = Verdict::judge(&Stop::PoweredOff, Some(1 << 8), &outputs, &t);
        assert_eq!(failed.reasons(), [Reason::Exited { code: 1 }]);
        assert!(failed.missing_outputs().is_empty());
    }

    #[test]
    fn a_broken_assertion_fails_a_job_that_exited_zero() {
        // An `always` that failed and an `unreachable` reached fail the
        // run with each in its words and step; a `sometimes` missed and a
        // test that failed do not.
        let t = trace(&[
            "rewind-sdk 1 always fail even byte 221 is odd",
            "rewind-sdk 1 sometimes miss huge-byte",
            "rewind-sdk 1 test-end fail coin flip",
            "rewind-sdk 1 unreachable closed pool closed",
        ]);
        let verdict = Verdict::judge(&Stop::PoweredOff, Some(0), &[OUT.into()], &t);
        assert_eq!(
            verdict.reasons(),
            [Reason::FailedAssertions {
                assertions: vec![
                    FailedAssertion {
                        step: 10,
                        pid: 7,
                        tid: 7,
                        text: "always \"even\" failed: byte 221 is odd".into(),
                    },
                    FailedAssertion {
                        step: 13,
                        pid: 7,
                        tid: 7,
                        text: "unreachable \"closed\" reached: pool closed".into(),
                    },
                ]
            }]
        );
        assert_eq!(verdict.failed_assertions().len(), 2);

        // Init's own marks are not the job's assertions.
        let init = trace(&[]);
        let mut init = init;
        init.events.push(mark(
            20,
            rewind_init::INIT_PID,
            "rewind-sdk 1 always fail posing",
        ));
        let verdict = Verdict::judge(&Stop::PoweredOff, Some(0), &[OUT.into()], &init);
        assert_eq!(verdict, Verdict::Passed);
    }

    #[test]
    fn reasons_add_up_in_the_order_they_are_checked() {
        // A job that broke an assertion, then exited 2, inside a machine
        // stopped at its time limit: all three, timeout first.
        let t = trace(&["rewind-sdk 1 always fail even"]);
        let verdict = Verdict::judge(&hung(), Some(2 << 8), &[], &t);
        let kinds: Vec<&str> = verdict
            .reasons()
            .iter()
            .map(|r| match r {
                Reason::TimedOut => "timed-out",
                Reason::Exited { .. } => "exited",
                Reason::FailedAssertions { .. } => "assertions",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["timed-out", "exited", "assertions"]);
    }

    #[test]
    fn a_verdict_reads_back_from_its_json() {
        // The states and reasons as a manifest carries them, tagged by
        // name, so a program reading the manifest can tell them apart.
        let unfinished = serde_json::to_value(Verdict::Unfinished).unwrap();
        assert_eq!(unfinished, serde_json::json!({"state": "unfinished"}));
        assert!(!Verdict::Unfinished.is_final());
        assert!(!Verdict::Unfinished.passed() && !Verdict::Unfinished.failed());

        let failed = Verdict::Failed {
            reasons: vec![
                Reason::Exited { code: 2 },
                Reason::MissingOutputs {
                    paths: vec![DEV.into()],
                },
            ],
        };
        let json = serde_json::to_value(&failed).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "state": "failed",
                "reasons": [
                    {"why": "exited", "code": 2},
                    {"why": "missing_outputs", "paths": [DEV]},
                ],
            })
        );
        assert_eq!(serde_json::from_value::<Verdict>(json).unwrap(), failed);
    }
}

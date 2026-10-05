//! The JSON objects `--json` prints, for programs such as the desktop app.

use anyhow::Result;
use rewind_core::Run;
use rewind_core::compare::{Comparison, Source, Verdict};
use rewind_trace::Trace;
use serde_json::{Value, json};

use crate::show::{self, KernelThreads};

/// A run: its id, name and directory, the run and step it was forked from,
/// how it ended, and the outputs its job hashed and left uncreated.
pub fn run(run: &Run) -> Result<Value> {
    let m = &run.manifest;
    let (status, hashed) = show::outcome_key(run)?;
    let missing = show::missing_outputs(&m.spec.job.outputs, &hashed);
    let outputs: Vec<Value> = hashed
        .iter()
        .filter_map(|h| h.split_once(' '))
        .map(|(path, hash)| json!({ "path": path, "hash": hash }))
        .collect();
    let outcome = m.outcome.as_ref();
    Ok(json!({
        "id": m.id,
        "name": m.name,
        "dir": run.dir,
        "parent": m.parent.as_ref().map(|(id, step)| json!({ "run": id, "step": step })),
        "status": status,
        "ending": outcome.map(|o| show::ending(&o.stop, status, &missing)),
        "steps": outcome.map(|o| o.step),
        "stop": outcome.map(|o| o.stop.as_str()),
        "first_difference": m.first_difference,
        "recorded_by": m.recorded_by,
        "outputs": outputs,
        "missing": missing,
    }))
}

/// Where two runs first differ, with the first event that differs on each
/// side; null for identical runs.
pub fn divergence(left: &Trace, right: &Trace) -> Value {
    let Some(d) = left.divergence(right) else {
        return Value::Null;
    };
    json!({
        "index": d.index,
        "left_step": d.left_step,
        "right_step": d.right_step,
        "left_event": left.events.get(d.index),
        "right_event": right.events.get(d.index),
    })
}

/// Where the program `argv` first behaves differently in two runs, with
/// its first event that differs on each side; null when its events agree.
pub fn program_divergence(left: &Trace, right: &Trace, argv: &[String]) -> Value {
    let Some(d) = left.divergence_in(right, argv) else {
        return Value::Null;
    };
    json!({
        "program": argv,
        "left_event": d.left_event().map(|(i, _)| &left.events[i]),
        "right_event": d.right_event().map(|(i, _)| &right.events[i]),
    })
}

/// The processes alive at `step`, as `rewind ps` lists them: each with its
/// parent, its command line, and the threads besides its main one alive
/// then.
pub fn processes(trace: &Trace, step: u64, kernel: KernelThreads) -> Value {
    let processes: Vec<Value> = show::alive(trace, step, kernel)
        .iter()
        .map(|p| {
            let threads: Vec<u32> = p
                .threads
                .iter()
                .filter(|(_, start, end)| *start <= step && end.is_none_or(|e| step < e))
                .map(|(tid, _, _)| *tid)
                .collect();
            json!({
                "pid": p.pid,
                "parent": p.parent,
                "argv": p.argv,
                "execd": p.execd,
                "threads": threads,
            })
        })
        .collect();
    json!({ "step": step, "processes": processes })
}

/// What each source said about an output.
pub fn comparisons(comparisons: &[Comparison]) -> Value {
    let each: Vec<Value> = comparisons
        .iter()
        .map(|c| {
            let source = match &c.source {
                Source::Store => "store",
                Source::Cache(url) => url.as_str(),
            };
            let (verdict, why) = match &c.verdict {
                Verdict::Matches => ("matches", None),
                Verdict::Differs => ("differs", None),
                Verdict::Absent => ("absent", None),
                Verdict::NeedsCredentials => ("needs-credentials", None),
                Verdict::Unreachable(why) => ("unreachable", Some(why)),
            };
            match why {
                Some(why) => json!({ "source": source, "verdict": verdict, "why": why }),
                None => json!({ "source": source, "verdict": verdict }),
            }
        })
        .collect();
    Value::Array(each)
}

#[cfg(test)]
mod tests {
    // The objects, from traces and comparisons built by hand.
    use super::*;
    use rewind_trace::{Event, EventKind};

    fn write(step: u64, pid: u32, text: &str) -> Event {
        Event {
            step,
            pid,
            tid: pid,
            kind: EventKind::Output {
                fd: 1,
                bytes: text.as_bytes().to_vec(),
            },
        }
    }

    #[test]
    fn a_divergence_names_the_first_events_that_differ() {
        // Two traces that agree on their first write and differ on the
        // second: the index, the steps and both events. The same trace
        // twice has none.
        let left = Trace {
            events: vec![write(3, 7, "a"), write(5, 7, "b")],
        };
        let right = Trace {
            events: vec![write(3, 7, "a"), write(6, 7, "c")],
        };
        let d = divergence(&left, &right);
        assert_eq!(d["index"], 1);
        assert_eq!(d["left_step"], 5);
        assert_eq!(d["right_step"], 6);
        assert_eq!(d["left_event"]["step"], 5);
        assert_eq!(d["right_event"]["step"], 6);
        assert_eq!(divergence(&left, &left), Value::Null);
    }

    #[test]
    fn processes_are_those_alive_at_the_step() {
        // init forks 7, which execs and exits at 20: at 10 both are alive,
        // at 30 only init.
        let event = |step, pid, kind| Event {
            step,
            pid,
            tid: pid,
            kind,
        };
        let trace = Trace {
            events: vec![
                event(
                    1,
                    1,
                    EventKind::Exec {
                        filename: "/init".into(),
                        argv: vec!["/init".into()],
                        old_pid: 0,
                    },
                ),
                event(
                    5,
                    1,
                    EventKind::Fork {
                        child: 7,
                        thread: false,
                    },
                ),
                event(
                    6,
                    7,
                    EventKind::Exec {
                        filename: "/bin/make".into(),
                        argv: vec!["make".into(), "check".into()],
                        old_pid: 0,
                    },
                ),
                event(
                    20,
                    7,
                    EventKind::Exit {
                        status: 0,
                        comm: "make".into(),
                        thread: false,
                    },
                ),
            ],
        };
        let at = |step| processes(&trace, step, KernelThreads::Hide);
        let pids = |v: &Value| -> Vec<u64> {
            v["processes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["pid"].as_u64().unwrap())
                .collect()
        };
        let ten = at(10);
        assert_eq!(ten["step"], 10);
        assert_eq!(pids(&ten), vec![1, 7]);
        assert_eq!(ten["processes"][1]["parent"], 1);
        assert_eq!(ten["processes"][1]["argv"], json!(["make", "check"]));
        assert_eq!(pids(&at(30)), vec![1]);
    }

    #[test]
    fn each_source_says_its_verdict_in_a_word() {
        // The store and a cache by its URL, each verdict in the words the
        // object uses, with why a cache could not be asked.
        let got = comparisons(&[
            Comparison {
                source: Source::Store,
                verdict: Verdict::Matches,
            },
            Comparison {
                source: Source::Cache("https://cache.nixos.org".into()),
                verdict: Verdict::Unreachable("timeout".into()),
            },
        ]);
        assert_eq!(
            got,
            json!([
                {"source": "store", "verdict": "matches"},
                {"source": "https://cache.nixos.org", "verdict": "unreachable", "why": "timeout"},
            ])
        );
    }
}

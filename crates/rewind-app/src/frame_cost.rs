//! How long the work the panels redo every frame takes on real data:
//! the owner's runs and one of the larger traces among them. The test
//! is ignored, since the data is only on the owner's machine; run it
//! with `cargo test --lib frame_cost -- --ignored --nocapture`.

#[cfg(test)]
mod tests {
    // Times one frame's worth of the model-level work behind the start
    // screen's rows, the Files list, the Threads tab's picked card and
    // the process card, against the runs in the engine's runs directory
    // and the run RUN names among them.
    use crate::engine::runs_dir;
    use crate::family::{families, scan};
    use crate::model::LogFilter;
    use crate::proc_card::{Pick, proc_card};
    use crate::run::Run;
    use std::time::Instant;

    /// The run measured, a 16 MB trace with thousands of file events.
    const RUN: &str = "c3160c232e56c485";
    /// How many rows a list shows at once on a tall window.
    const VISIBLE_ROWS: usize = 40;

    #[test]
    #[ignore = "needs the owner's runs"]
    fn one_frame_of_each_panel() {
        let runs = runs_dir().unwrap();
        let entries = scan(&runs);
        let recent = families(entries);
        println!("{} families", recent.len());

        // The start screen: every family's row, as render_recent builds
        // them.
        let started = Instant::now();
        let mut rows = 0;
        for family in &recent {
            if !family.matches("") {
                continue;
            }
            let (headline, _) = family.headline();
            let _ = family.modified();
            let summary = (family.runs.len() > 1).then(|| family.summary());
            rows += headline.len() + summary.map_or(0, |s| s.len());
        }
        println!("recent rows: {:?} ({rows})", started.elapsed());
        let started = Instant::now();
        let runs_held: usize = recent.iter().map(|f| f.clone().runs.len()).sum();
        println!("recent clones: {:?} ({runs_held} runs)", started.elapsed());

        let run = Run::open(&runs.join(RUN)).unwrap();
        let t = &run.timeline;
        println!(
            "{} events, {} files, {} rows, {} output lines",
            t.trace.events.len(),
            t.files.len(),
            t.rows.len(),
            t.lines(LogFilter::Output).len()
        );

        // The Files list: has_files_under for each visible row.
        let started = Instant::now();
        let mut dirs = 0;
        for file in t.files.iter().rev().take(VISIBLE_ROWS) {
            if t.has_files_under(&file.path) {
                dirs += 1;
            }
        }
        println!("files rows: {:?} ({dirs} dirs)", started.elapsed());

        // The process card for the process with the most output.
        let (pid, _) = t
            .procs
            .iter()
            .map(|p| {
                let lines = t
                    .lines(LogFilter::Output)
                    .iter()
                    .filter(|l| l.pid == p.pid)
                    .count();
                (p.pid, lines)
            })
            .max_by_key(|(_, lines)| *lines)
            .unwrap();
        let row = t
            .rows
            .iter()
            .find(|r| r.pid == pid && r.tid == pid)
            .unwrap();
        let pick = Pick::of(row);
        let started = Instant::now();
        let card = proc_card(t, pick, t.total).unwrap();
        println!(
            "proc card: {:?} ({} lines, {} later)",
            started.elapsed(),
            card.output.len(),
            card.later_lines
        );

        // The Threads tab's picked card: an event looked up by step for
        // each listed event, as lanes_lines does.
        let steps: Vec<u64> = t
            .trace
            .events
            .iter()
            .rev()
            .take(VISIBLE_ROWS)
            .map(|e| e.step)
            .collect();
        let started = Instant::now();
        let mut found = 0;
        for step in &steps {
            let at = t.trace.events.partition_point(|e| e.step < *step);
            if t.trace.events.get(at).is_some_and(|e| e.step == *step) {
                found += 1;
            }
        }
        println!("lanes events: {:?} ({found})", started.elapsed());
    }
}

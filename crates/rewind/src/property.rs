//! The properties `rewind check` judges each run it tries by.
//!
//! A property says whether a finished run is correct and, when it is not,
//! why and at which step. The search looks for a schedule whose verdict
//! differs from the unperturbed run's, then narrows it to the step that
//! decides it. With no property given, check asks for the same outcome as
//! the unperturbed run, which is what it asked before properties existed.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use rewind_core::inspect::{self, Inspection};
use rewind_core::{Home, Run};
use rewind_trace::ending::Ending;
use rewind_trace::properties::{self as trace_properties, Finding, Glob};
use rewind_trace::sdk::{SdkMark, TestResult};

use crate::show::{self, OutcomeKey};

/// The word a property is named by on the command line.
const SAME_OUTCOME: &str = "same-outcome";
const EXIT_ZERO: &str = "exit-zero";
const NO_CRASH: &str = "no-crash";
const NO_ORPHANS: &str = "no-orphans";
const SINGLE_WRITER: &str = "single-writer";
const ATOMIC_REPLACE: &str = "atomic-replace";
const NO_READ_WHILE_WRITTEN: &str = "no-read-while-written";
const WRITES_WITHIN: &str = "writes-within";
const SDK: &str = "sdk";
const JUNIT: &str = "junit";
const SCRIPT: &str = "script";

/// Every property as the command line writes it, for the message that
/// refuses an unknown one.
const ALL: &str = "same-outcome, exit-zero, no-crash, no-orphans, single-writer=GLOB, \
     atomic-replace=GLOB, no-read-while-written=GLOB, writes-within=DIR:DIR, sdk, junit=GLOB \
     and script=PATH";

/// What separates the directories of writes-within.
const DIR_SEPARATOR: char = ':';

/// What starts a comment line in a properties file.
const COMMENT: char = '#';

/// What a property script's exit status means.
const SCRIPT_HOLDS: i32 = 0;
const SCRIPT_VIOLATED: i32 = 1;

/// The environment a property script reads its run from.
const ENV_RUN: &str = "REWIND_RUN";
const ENV_BASE: &str = "REWIND_BASE";
const ENV_END: &str = "REWIND_END";

/// A property of a finished run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Property {
    /// The run ends as the unperturbed run did: the same wait status and
    /// every output with the same hash.
    SameOutcome,
    /// The job exits 0 having made every output, before the time limit.
    ExitZero,
    /// No process of the job crashed (see [`trace_properties::no_crash`]).
    NoCrash,
    /// Every process the job started had exited when the job did.
    NoOrphans,
    /// No two of the job's processes wrote a file matching the glob at
    /// once.
    SingleWriter(Glob),
    /// No file matching the glob that the run had written was truncated
    /// in place.
    AtomicReplace(Glob),
    /// No file matching the glob was opened for reading while another
    /// process was writing it.
    NoReadWhileWritten(Glob),
    /// The job wrote files only under these directories.
    WritesWithin(Vec<String>),
    /// What the job asserted with the Rewind SDK held: no `always` failed,
    /// no `unreachable` was reached, and every test that passed in the
    /// unperturbed run passed (see [`rewind_trace::sdk`]).
    Sdk,
    /// Every test that passed in the unperturbed run passed, as the JUnit
    /// XML reports the job wrote to files matching the glob say, and
    /// every report parses.
    Junit(Glob),
    /// A program on this machine judges the run: exit 0 means the
    /// property holds, 1 that it is violated, and any other ending is an
    /// error that stops the search.
    Script(PathBuf),
}

impl FromStr for Property {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Property> {
        let (name, arg) = match s.split_once('=') {
            Some((name, arg)) => (name, Some(arg)),
            None => (s, None),
        };
        let arg = arg.filter(|a| !a.is_empty());
        match (name, arg) {
            (SAME_OUTCOME, None) => Ok(Property::SameOutcome),
            (EXIT_ZERO, None) => Ok(Property::ExitZero),
            (NO_CRASH, None) => Ok(Property::NoCrash),
            (NO_ORPHANS, None) => Ok(Property::NoOrphans),
            (SDK, None) => Ok(Property::Sdk),
            (SINGLE_WRITER, Some(glob)) => Ok(Property::SingleWriter(Glob::new(glob))),
            (ATOMIC_REPLACE, Some(glob)) => Ok(Property::AtomicReplace(Glob::new(glob))),
            (NO_READ_WHILE_WRITTEN, Some(glob)) => {
                Ok(Property::NoReadWhileWritten(Glob::new(glob)))
            }
            (JUNIT, Some(glob)) => Ok(Property::Junit(Glob::new(glob))),
            (WRITES_WITHIN, Some(dirs)) => {
                let dirs: Vec<String> = dirs.split(DIR_SEPARATOR).map(str::to_string).collect();
                if let Some(relative) = dirs.iter().find(|d| !d.starts_with('/')) {
                    bail!("property {WRITES_WITHIN} takes absolute directories, not {relative}");
                }
                Ok(Property::WritesWithin(dirs))
            }
            (SCRIPT, Some(path)) => Ok(Property::Script(PathBuf::from(path))),
            (SAME_OUTCOME | EXIT_ZERO | NO_CRASH | NO_ORPHANS | SDK, Some(_)) => {
                bail!("property {name} takes no argument")
            }
            (SINGLE_WRITER | ATOMIC_REPLACE | NO_READ_WHILE_WRITTEN | JUNIT, None) => {
                bail!("property {name} needs the files it is about: {name}=GLOB")
            }
            (WRITES_WITHIN, None) => bail!("property {name} needs directories: {name}=DIR:DIR"),
            (SCRIPT, None) => bail!("property script needs a program: script=PATH"),
            _ => bail!("no property {name}; the properties are {ALL}"),
        }
    }
}

impl fmt::Display for Property {
    /// The property as the command line names it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Property::SameOutcome => write!(f, "{SAME_OUTCOME}"),
            Property::ExitZero => write!(f, "{EXIT_ZERO}"),
            Property::NoCrash => write!(f, "{NO_CRASH}"),
            Property::NoOrphans => write!(f, "{NO_ORPHANS}"),
            Property::Sdk => write!(f, "{SDK}"),
            Property::SingleWriter(glob) => write!(f, "{SINGLE_WRITER}={}", glob.as_str()),
            Property::AtomicReplace(glob) => write!(f, "{ATOMIC_REPLACE}={}", glob.as_str()),
            Property::NoReadWhileWritten(glob) => {
                write!(f, "{NO_READ_WHILE_WRITTEN}={}", glob.as_str())
            }
            Property::Junit(glob) => write!(f, "{JUNIT}={}", glob.as_str()),
            Property::WritesWithin(dirs) => {
                let dirs = dirs.join(&DIR_SEPARATOR.to_string());
                write!(f, "{WRITES_WITHIN}={dirs}")
            }
            Property::Script(path) => write!(f, "{SCRIPT}={}", path.display()),
        }
    }
}

impl Property {
    /// The short name a report calls the property by: a script by its
    /// file name without its extension, and a property that takes an
    /// argument without it.
    pub(crate) fn name(&self) -> String {
        match self {
            Property::Script(path) => path.file_stem().map_or_else(
                || path.display().to_string(),
                |s| s.to_string_lossy().into_owned(),
            ),
            other => {
                let spec = other.to_string();
                spec.split_once('=')
                    .map_or(spec.clone(), |(name, _)| name.to_string())
            }
        }
    }

    /// The properties a file lists, one a line, without blank lines and
    /// lines starting with `#`. A script's path is taken from the file's
    /// directory, so a repository's file can name the scripts beside it.
    pub(crate) fn read_file(path: &Path) -> Result<Vec<Property>> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading properties from {}", path.display()))?;
        // A file named without a directory is in the current one, and a
        // script beside it is ./name there, not a name to look up on PATH.
        let dir = match path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir,
            _ => Path::new("."),
        };
        let mut properties = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with(COMMENT) {
                continue;
            }
            let property: Property = line
                .parse()
                .with_context(|| format!("{}:{}", path.display(), i + 1))?;
            properties.push(match property {
                Property::Script(script) if script.is_relative() => {
                    Property::Script(dir.join(script))
                }
                other => other,
            });
        }
        Ok(properties)
    }
}

/// Why a run violates a property: the property, the step to look at when
/// there is one, and what went wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Violation {
    pub property: String,
    pub step: Option<u64>,
    pub why: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.step {
            Some(step) => write!(f, "step {step}: {}", self.why),
            None => write!(f, "{}", self.why),
        }
    }
}

/// The properties of one search, and the unperturbed run they compare
/// with.
pub(crate) struct Judge<'a> {
    home: &'a Home,
    base_id: String,
    base_key: OutcomeKey,
    base_ending: Ending,
    /// How each test the unperturbed run marked with the SDK ended.
    base_tests: BTreeMap<String, TestResult>,
    /// How each test in the unperturbed run's JUnit reports ended, by the
    /// glob of the junit property that read them.
    base_junit: HashMap<String, BTreeMap<String, TestResult>>,
    properties: Vec<Property>,
}

impl<'a> Judge<'a> {
    /// A judge of `properties`, or of the same outcome as `base` when
    /// there are none.
    pub(crate) fn new(home: &'a Home, base: &Run, properties: Vec<Property>) -> Result<Judge<'a>> {
        let properties = if properties.is_empty() {
            vec![Property::SameOutcome]
        } else {
            properties
        };
        let base_key = show::outcome_key(base)?;
        let base_tests = base
            .trace()?
            .sdk_marks()
            .filter_map(|(_, mark)| match mark {
                SdkMark::TestEnd { name, result } => Some((name, result)),
                _ => None,
            })
            .collect();
        // Each junit pattern must find a report in the base run: one that
        // finds none would hold on every run and hide the tests.
        let mut base_junit = HashMap::new();
        for property in &properties {
            let Property::Junit(glob) = property else {
                continue;
            };
            let results =
                junit_reports(home, base, glob)?.into_base(&base.manifest.id.to_string())?;
            if results.is_empty() {
                bail!(
                    "{JUNIT}={} matched no JUnit report schedule 0 wrote; a pattern without a / \
                     matches a file name, as TEST-*.xml does, * stays within one directory and \
                     ** crosses them",
                    glob.as_str()
                );
            }
            base_junit.insert(glob.as_str().to_string(), results);
        }
        Ok(Judge {
            home,
            base_id: base.manifest.id.to_string(),
            base_ending: ending(base, &base_key),
            base_key,
            base_tests,
            base_junit,
            properties,
        })
    }

    /// Whether the search asks only for the same outcome as the
    /// unperturbed run, as check did before properties.
    pub(crate) fn same_outcome_only(&self) -> bool {
        self.properties == [Property::SameOutcome]
    }

    /// Whether a property reads a run's files where its job exited: junit
    /// reads its reports there, and a script is given that step.
    pub(crate) fn reads_files_at_exit(&self) -> bool {
        self.properties
            .iter()
            .any(|p| matches!(p, Property::Junit(_) | Property::Script(_)))
    }

    /// The properties, as the command line names them.
    pub(crate) fn properties(&self) -> &[Property] {
        &self.properties
    }

    /// The unperturbed run's outcome, which `same-outcome` compares with.
    pub(crate) fn base_key(&self) -> &OutcomeKey {
        &self.base_key
    }

    /// Every property `run` violates, in the order given; none when all
    /// hold.
    pub(crate) fn violations(&self, run: &Run) -> Result<Vec<Violation>> {
        let mut found = Vec::new();
        for property in &self.properties {
            if let Some(v) = self.judge(property, run)? {
                found.push(v);
            }
        }
        Ok(found)
    }

    /// Whether `run` violates `property`, and how.
    fn judge(&self, property: &Property, run: &Run) -> Result<Option<Violation>> {
        let violation = |step: Option<u64>, why: String| Violation {
            property: property.name(),
            step,
            why,
        };
        let found = |finding: Option<Finding>| finding.map(|f| violation(Some(f.step), f.why));
        match property {
            // The outcome against the unperturbed run's.
            Property::SameOutcome => {
                let key = show::outcome_key(run)?;
                if key == self.base_key {
                    return Ok(None);
                }
                Ok(Some(violation(
                    end_step(run).ok(),
                    format!(
                        "ended {}, where {} ended {}",
                        ending(run, &key),
                        self.base_id,
                        self.base_ending
                    ),
                )))
            }

            // The job's own ending.
            Property::ExitZero => {
                let key = show::outcome_key(run)?;
                let ended = ending(run, &key);
                if ended.passed() {
                    return Ok(None);
                }
                Ok(Some(violation(
                    end_step(run).ok(),
                    format!("ended {ended}"),
                )))
            }

            // What the trace says the job's processes did.
            Property::NoCrash => Ok(found(trace_properties::no_crash(run.trace()?))),
            Property::NoOrphans => Ok(found(trace_properties::no_orphans(run.trace()?))),
            Property::SingleWriter(glob) => {
                Ok(found(trace_properties::single_writer(run.trace()?, glob)))
            }
            Property::AtomicReplace(glob) => {
                Ok(found(trace_properties::atomic_replace(run.trace()?, glob)))
            }
            Property::NoReadWhileWritten(glob) => Ok(found(
                trace_properties::no_read_while_written(run.trace()?, glob),
            )),
            Property::WritesWithin(dirs) => {
                Ok(found(trace_properties::writes_within(run.trace()?, dirs)))
            }

            // What the job asserted, and its tests against the unperturbed
            // run's.
            Property::Sdk => Ok(self
                .sdk_violation(run)?
                .map(|(step, why)| violation(step, why))),

            // The tests in the job's reports against the unperturbed run's.
            Property::Junit(glob) => {
                let base = &self.base_junit[glob.as_str()];
                let here = junit_reports(self.home, run, glob)?;
                let end = end_step(run)?;
                Ok(here.violation(base).map(|why| violation(Some(end), why)))
            }

            // A program's verdict on the run.
            Property::Script(path) => {
                let end = end_step(run)?;
                let output = script_command(path, self.home, run, &self.base_id, end)?
                    .output()
                    .with_context(|| format!("running property script {}", path.display()))?;
                let stdout = String::from_utf8_lossy(&output.stdout);
                match output.status.code() {
                    Some(SCRIPT_HOLDS) => Ok(None),
                    Some(SCRIPT_VIOLATED) => {
                        let (step, why) = script_witness(&stdout);
                        Ok(Some(violation(step, why)))
                    }
                    _ => bail!(
                        "property script {} failed on run {} ({}): {}",
                        path.display(),
                        run.manifest.id,
                        output.status,
                        String::from_utf8_lossy(&output.stderr).trim()
                    ),
                }
            }
        }
    }
}

impl Judge<'_> {
    /// The first thing `run` broke that it asserted with the SDK, by step:
    /// an `always` that failed, an `unreachable` it reached, or a test
    /// that passed in the unperturbed run and failed or never finished in
    /// this one.
    fn sdk_violation(&self, run: &Run) -> Result<Option<(Option<u64>, String)>> {
        let mut broken: Vec<(Option<u64>, String)> = Vec::new();
        let mut begun: HashMap<String, u64> = HashMap::new();
        let mut ended: HashMap<String, (u64, TestResult)> = HashMap::new();
        for (event, mark) in run.trace()?.sdk_marks() {
            match mark {
                SdkMark::Always {
                    id,
                    held: false,
                    message,
                } => broken.push((
                    Some(event.step),
                    said(&format!("always \"{id}\" failed"), &message),
                )),
                SdkMark::Unreachable { id, message } => {
                    broken.push((
                        Some(event.step),
                        said(&format!("unreachable \"{id}\" reached"), &message),
                    ));
                }
                SdkMark::TestBegin { name } => {
                    begun.entry(name).or_insert(event.step);
                }
                SdkMark::TestEnd { name, result } => {
                    ended.insert(name, (event.step, result));
                }
                SdkMark::Always { .. } | SdkMark::Sometimes { .. } => {}
            }
        }

        // The tests the unperturbed run passed, as this run ended them.
        for (name, base_result) in &self.base_tests {
            if *base_result != TestResult::Pass {
                continue;
            }
            match ended.get(name) {
                Some((_, TestResult::Pass | TestResult::Skip)) => {}
                Some((step, TestResult::Fail)) => broken.push((
                    Some(*step),
                    format!("test {name} passed in the unperturbed run and failed here"),
                )),
                None => broken.push((
                    begun.get(name).copied(),
                    format!("test {name} passed in the unperturbed run and did not finish here"),
                )),
            }
        }
        Ok(broken
            .into_iter()
            .min_by_key(|(step, _)| step.unwrap_or(u64::MAX)))
    }
}

/// The first test, by name, that passed in `base` and failed or has no
/// result in `here`.
fn regressed(
    base: &BTreeMap<String, TestResult>,
    here: &BTreeMap<String, TestResult>,
) -> Option<String> {
    base.iter()
        .filter(|(_, result)| **result == TestResult::Pass)
        .find_map(|(name, _)| match here.get(name) {
            Some(TestResult::Pass | TestResult::Skip) => None,
            Some(TestResult::Fail) => Some(format!(
                "test {name} passed in the unperturbed run and failed here"
            )),
            None => Some(format!(
                "test {name} passed in the unperturbed run and has no result here"
            )),
        })
}

/// What the JUnit reports of one run say.
#[derive(Debug, Default)]
struct Reports {
    /// How each test ended, by name.
    results: BTreeMap<String, TestResult>,
    /// The reports that are not XML a parser accepts, each by its path
    /// with the parser's error, in the order read. A test runner killed
    /// while it wrote a report leaves one.
    unreadable: Vec<(String, anyhow::Error)>,
}

impl Reports {
    /// The reports in `sections`, each a report's path and its bytes.
    fn read(sections: &[(String, &[u8])]) -> Reports {
        let mut reports = Reports::default();
        for (path, xml) in sections {
            match crate::junit::results(&String::from_utf8_lossy(xml)) {
                Ok(results) => reports.results.extend(results),
                Err(error) => reports.unreadable.push((path.clone(), error)),
            }
        }
        reports
    }

    /// The results, for the unperturbed run `run_id`, which every other
    /// run is compared with: a report of that run that does not parse is
    /// an error.
    fn into_base(self, run_id: &str) -> Result<BTreeMap<String, TestResult>> {
        match self.unreadable.into_iter().next() {
            Some((path, error)) => Err(error.context(format!("{path} in run {run_id}"))),
            None => Ok(self.results),
        }
    }

    /// Why a run with these reports, other than the unperturbed run,
    /// violates the junit property, where `base` is the unperturbed
    /// run's results: the first report that does not parse, by its path
    /// with the parser's error, else the first test that regressed. The
    /// unperturbed run wrote reports that parse, so a report that does
    /// not is something the run's schedule did.
    fn violation(&self, base: &BTreeMap<String, TestResult>) -> Option<String> {
        if let Some((path, error)) = self.unreadable.first() {
            return Some(format!(
                "JUnit report {path} cannot be read: {}",
                error.root_cause()
            ));
        }
        regressed(base, &self.results)
    }
}

/// The JUnit reports `run`'s job wrote to files matching `glob`, read at
/// the job's exit.
fn junit_reports(home: &Home, run: &Run, glob: &Glob) -> Result<Reports> {
    let files = trace_properties::written_files(run.trace()?, glob);
    if files.is_empty() {
        return Ok(Reports::default());
    }
    let end = end_step(run)?;
    let bytes = match inspect::files(home, run, end, None, &files)? {
        Inspection::Contents(bytes) => bytes,
        Inspection::NotFound(message) | Inspection::Failed(message) => {
            bail!(
                "reading the JUnit reports of run {}: {message}",
                run.manifest.id
            )
        }
    };
    let sections = rewind_init::sections(&bytes)
        .with_context(|| format!("reading the JUnit reports of run {}", run.manifest.id))?;
    Ok(Reports::read(&sections))
}

/// `what`, followed by what the program said about it when it said
/// anything.
fn said(what: &str, message: &str) -> String {
    if message.is_empty() {
        what.to_string()
    } else {
        format!("{what}: {message}")
    }
}

/// How many of a search's runs checked one `sometimes` condition, and how
/// many of those made it true.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Reach {
    pub runs: u64,
    pub hits: u64,
}

/// The `sometimes` conditions of a search: each id any run checked, and
/// how many runs reached it and made it true.
#[derive(Default)]
pub(crate) struct Coverage {
    sometimes: BTreeMap<String, Reach>,
}

impl Coverage {
    /// Adds the `sometimes` checks `run` made.
    pub(crate) fn add(&mut self, run: &Run) -> Result<()> {
        self.add_marks(run.trace()?.sdk_marks().map(|(_, mark)| mark));
        Ok(())
    }

    /// Adds one run's `sometimes` checks, from its SDK marks. The run
    /// counts once per id, and as a hit if any check of the id was true.
    fn add_marks(&mut self, marks: impl IntoIterator<Item = SdkMark>) {
        let mut checked: BTreeMap<String, bool> = BTreeMap::new();
        for mark in marks {
            let SdkMark::Sometimes { id, hit } = mark else {
                continue;
            };
            *checked.entry(id).or_default() |= hit;
        }

        for (id, hit) in checked {
            let reach = self.sometimes.entry(id).or_default();
            reach.runs += 1;
            if hit {
                reach.hits += 1;
            }
        }
    }

    /// The ids no run made true, in order.
    pub(crate) fn never(&self) -> Vec<&str> {
        self.sometimes
            .iter()
            .filter(|(_, reach)| reach.hits == 0)
            .map(|(id, _)| id.as_str())
            .collect()
    }

    /// The ids some run made true, in order, with how often.
    pub(crate) fn reached(&self) -> Vec<(&str, Reach)> {
        self.sometimes
            .iter()
            .filter(|(_, reach)| reach.hits > 0)
            .map(|(id, reach)| (id.as_str(), *reach))
            .collect()
    }
}

/// Whether `run` wrote any SDK mark, so the search judges them.
pub(crate) fn writes_sdk_marks(run: &Run) -> Result<bool> {
    Ok(run.trace()?.sdk_marks().next().is_some())
}

/// How `run` ended, as a run's line says it.
fn ending(run: &Run, key: &OutcomeKey) -> Ending {
    let missing = show::missing_outputs(&run.manifest.spec.job.outputs, &key.outputs);
    match &run.manifest.outcome {
        Some(o) => Ending::of(&o.stop, key.status, &missing),
        None => Ending::NoStatus,
    }
}

/// The step a property script reads a run's files at: the job's exit,
/// where the job's files are as it left them, else the run's last.
pub(crate) fn end_step(run: &Run) -> Result<u64> {
    match run.trace()?.job_exit() {
        Some(exit) => Ok(exit.step),
        None => run.last_step(),
    }
}

/// The command that runs property script `path` on `run`: the run, the
/// unperturbed run and the step to read files at in its environment, the
/// same Rewind home, and this rewind first on PATH, so the `rewind` it
/// calls reads the same runs.
fn script_command(path: &Path, home: &Home, run: &Run, base_id: &str, end: u64) -> Result<Command> {
    let mut command = Command::new(path);
    command
        .env(ENV_RUN, run.manifest.id.to_string())
        .env(ENV_BASE, base_id)
        .env(ENV_END, end.to_string())
        .env(rewind_core::home::ENV_HOME, home.root())
        .stdin(Stdio::null());

    // This rewind's directory first on PATH.
    let exe = std::env::current_exe().context("finding this rewind")?;
    if let Some(dir) = exe.parent() {
        let rest = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(
            std::iter::once(dir.to_path_buf()).chain(std::env::split_paths(&rest)),
        )?;
        command.env("PATH", path);
    }
    Ok(command)
}

/// What a script that found its property violated said: its first line,
/// and the step that line names when it reads `step N: why`.
fn script_witness(stdout: &str) -> (Option<u64>, String) {
    const STEP_PREFIX: &str = "step ";
    const NO_REASON: &str = "violated, without saying why";

    let Some(line) = stdout.lines().map(str::trim).find(|l| !l.is_empty()) else {
        return (None, NO_REASON.to_string());
    };
    let named = line
        .strip_prefix(STEP_PREFIX)
        .and_then(|rest| rest.split_once(':'))
        .and_then(|(n, why)| Some((n.trim().parse::<u64>().ok()?, why.trim())));
    match named {
        Some((step, why)) if !why.is_empty() => (Some(step), why.to_string()),
        Some((step, _)) => (Some(step), NO_REASON.to_string()),
        None => (None, line.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each property parses from the form the command line takes, and
    // prints back to it.
    #[test]
    fn properties_parse_from_the_command_line() {
        for (given, property) in [
            ("same-outcome", Property::SameOutcome),
            ("exit-zero", Property::ExitZero),
            ("no-crash", Property::NoCrash),
            ("no-orphans", Property::NoOrphans),
            ("sdk", Property::Sdk),
            (
                "junit=/build/target/surefire-reports/*.xml",
                Property::Junit(Glob::new("/build/target/surefire-reports/*.xml")),
            ),
            (
                "single-writer=*.log",
                Property::SingleWriter(Glob::new("*.log")),
            ),
            (
                "atomic-replace=*.conf",
                Property::AtomicReplace(Glob::new("*.conf")),
            ),
            (
                "no-read-while-written=*.conf",
                Property::NoReadWhileWritten(Glob::new("*.conf")),
            ),
            (
                "writes-within=/build:/tmp",
                Property::WritesWithin(vec!["/build".into(), "/tmp".into()]),
            ),
            (
                "script=./adds-up.sh",
                Property::Script(PathBuf::from("./adds-up.sh")),
            ),
        ] {
            assert_eq!(given.parse::<Property>().unwrap(), property);
            assert_eq!(property.to_string(), given);
        }
    }

    // A misspelled property, an argument where none is taken, and a
    // script without a program are refused with the reason.
    #[test]
    fn a_bad_property_is_refused() {
        let error = |s: &str| format!("{:#}", s.parse::<Property>().unwrap_err());
        assert!(error("exit-0").contains("no property exit-0"));
        assert!(error("exit-zero=1").contains("takes no argument"));
        assert!(error("script").contains("script=PATH"));
        assert!(error("script=").contains("script=PATH"));
        assert!(error("atomic-replace").contains("atomic-replace=GLOB"));
        assert!(error("writes-within=build").contains("absolute directories, not build"));
    }

    // A properties file skips blanks and comments, and takes a script's
    // relative path from the file's directory.
    #[test]
    fn a_properties_file_lists_one_a_line() {
        let dir = std::env::temp_dir().join(format!("rewind-properties-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("rewind.properties");
        std::fs::write(
            &file,
            "# what correct means\nno-crash\n\n  script=checks/adds-up.sh\n",
        )
        .unwrap();
        assert_eq!(
            Property::read_file(&file).unwrap(),
            vec![
                Property::NoCrash,
                Property::Script(dir.join("checks/adds-up.sh")),
            ]
        );

        // A file in the current directory takes its scripts from ./, so
        // a script named alone is not looked up on PATH.
        let here = Path::new("rewind-properties-here.properties");
        std::fs::write(here, "script=adds-up.sh\n").unwrap();
        assert_eq!(
            Property::read_file(here).unwrap(),
            vec![Property::Script(PathBuf::from("./adds-up.sh"))]
        );
        std::fs::remove_file(here).unwrap();

        std::fs::write(&file, "no-crash\nexit-0\n").unwrap();
        let error = format!("{:#}", Property::read_file(&file).unwrap_err());
        assert!(error.contains("rewind.properties:2"), "{error}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // A schedule that kills the test runner while it writes a report
    // leaves a report cut short. In a run other than the unperturbed one
    // that report is a violation that names the file and the parser's
    // error, ahead of the tests it no longer reports, and the reports
    // that do parse are still read. In the unperturbed run it is an
    // error that names the file and the run.
    #[test]
    fn a_report_cut_short_violates_the_junit_property() {
        const WHOLE: &str = r#"<testsuite><testcase classname="a" name="adds"/></testsuite>"#;
        const CUT: &str = r#"<testsuite><testcase classname="b" name="su"#;
        let sections: Vec<(String, &[u8])> = vec![
            ("/build/reports/TEST-a.xml".into(), WHOLE.as_bytes()),
            ("/build/reports/TEST-b.xml".into(), CUT.as_bytes()),
        ];
        let base: BTreeMap<String, TestResult> = [
            ("a.adds".to_string(), TestResult::Pass),
            ("b.subtracts".to_string(), TestResult::Pass),
        ]
        .into();

        let reports = Reports::read(&sections);
        assert_eq!(reports.results.get("a.adds"), Some(&TestResult::Pass));
        let why = reports.violation(&base).unwrap();
        assert!(
            why.starts_with("JUnit report /build/reports/TEST-b.xml cannot be read: "),
            "{why}"
        );

        // Reports that all parse are judged by their tests alone.
        let whole = Reports::read(&sections[..1]);
        assert_eq!(
            whole.violation(&base).unwrap(),
            "test b.subtracts passed in the unperturbed run and has no result here"
        );

        // The unperturbed run's own report cut short stops the command.
        let error = format!("{:#}", reports.into_base("1a2b3c4d").unwrap_err());
        assert!(
            error.starts_with("/build/reports/TEST-b.xml in run 1a2b3c4d: parsing a JUnit XML"),
            "{error}"
        );
    }

    // A script is named by its file name, without directory or
    // extension.
    #[test]
    fn a_script_is_named_by_its_file() {
        let p: Property = "script=properties/report-adds-up.sh".parse().unwrap();
        assert_eq!(p.name(), "report-adds-up");
        assert_eq!(Property::ExitZero.name(), "exit-zero");
        assert_eq!(
            Property::AtomicReplace(Glob::new("*.conf")).name(),
            "atomic-replace"
        );
    }

    // A script's first line is its reason, and a line that starts with
    // `step N:` also names the step.
    #[test]
    fn a_script_says_why_and_where() {
        assert_eq!(
            script_witness("step 4021: the report is short\nmore\n"),
            (Some(4021), "the report is short".to_string())
        );
        assert_eq!(
            script_witness("\n  total 19, expected 20\n"),
            (None, "total 19, expected 20".to_string())
        );
        assert_eq!(
            script_witness(""),
            (None, "violated, without saying why".to_string())
        );
        assert_eq!(
            script_witness("step later: no number"),
            (None, "step later: no number".to_string())
        );
    }

    // A search counts, for each `sometimes` id, the runs that checked it
    // and the runs that made it true: a run counts once however often it
    // checked, and as a hit if any of its checks was true.
    #[test]
    fn coverage_counts_the_runs_that_reach_and_hit_a_sometimes() {
        let sometimes = |id: &str, hit| SdkMark::Sometimes {
            id: id.to_string(),
            hit,
        };
        let mut coverage = Coverage::default();
        coverage.add_marks([sometimes("contended", false), sometimes("contended", true)]);
        coverage.add_marks([sometimes("contended", false), sometimes("huge", false)]);
        coverage.add_marks([sometimes("contended", true)]);

        assert_eq!(coverage.never(), vec!["huge"]);
        assert_eq!(
            coverage.reached(),
            vec![("contended", Reach { runs: 3, hits: 2 })]
        );
    }
}

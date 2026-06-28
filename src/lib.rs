use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use owo_colors::OwoColorize;
use std::collections::VecDeque;
use std::env;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use thiserror::Error;
use time::{Duration, OffsetDateTime, format_description};

const LOG_RETENTION_DAYS: i64 = 7;

#[derive(Debug, Parser)]
#[command(name = "daily-ups")]
#[command(about = "Update local toolchains and projects")]
pub struct Cli {
    #[arg(long)]
    pub dry_run: bool,

    #[arg(long)]
    pub deep: bool,

    #[arg(long, default_value_t = 4)]
    pub jobs: usize,

    #[arg(long)]
    pub verbose_log: bool,
}

#[derive(Debug, Error)]
pub enum AppError {
    #[error("HOME is not set")]
    HomeMissing,
    #[error("--jobs must be at least 1")]
    InvalidJobs,
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("time format error: {0}")]
    TimeFormat(#[from] time::error::Format),
    #[error("time format description error: {0}")]
    TimeFormatDescription(#[from] time::error::InvalidFormatDescription),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepStatus {
    Success,
    Failure(String),
    Skipped(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepRecord {
    name: String,
    status: StepStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSummary {
    pub successes: Vec<String>,
    pub failures: Vec<String>,
    pub skipped: Vec<String>,
    pub log_path: Option<PathBuf>,
}

impl RunSummary {
    pub fn exit_code(&self) -> i32 {
        if self.failures.is_empty() && self.skipped.is_empty() {
            0
        } else {
            1
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CommandSpec {
    program: &'static str,
    args: &'static [&'static str],
}

#[derive(Debug, Clone)]
struct WorkGroup {
    name: &'static str,
    commands: Vec<CommandSpec>,
}

#[derive(Debug, Clone)]
struct ProjectTask {
    name: &'static str,
    dir: PathBuf,
    commands: Vec<CommandSpec>,
}

#[derive(Debug)]
struct WorkOutput {
    records: Vec<StepRecord>,
    log: String,
}

struct Logger {
    file: Option<File>,
    path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
enum Tone {
    Header,
    Success,
    Failure,
    Skipped,
    Muted,
}

pub fn run(cli: Cli) -> Result<i32, AppError> {
    let home = home_dir()?;
    let summary = run_with_home(cli, home)?;
    Ok(summary.exit_code())
}

pub fn run_with_home(cli: Cli, home: PathBuf) -> Result<RunSummary, AppError> {
    let jobs = validate_jobs(cli.jobs)?;
    let (mut logger, log_failure) = Logger::new(&home)?;
    let mut records = Vec::new();

    logger.line_colored("daily-ups", Tone::Header);
    logger.line(&format!("started: {}", timestamp()?));
    let mode = if cli.dry_run {
        format!("Mode: dry-run, project jobs: {jobs}")
    } else if cli.deep {
        format!("Mode: deep, project jobs: {jobs}")
    } else {
        format!("Mode: default, project jobs: {jobs}")
    };
    let log_mode = if cli.verbose_log {
        format!("{mode}, verbose log")
    } else {
        mode
    };
    logger.line_colored(&log_mode, Tone::Muted);

    if let Some(record) = log_failure {
        records.push(record);
    }

    logger.line("");
    logger.line("");
    logger.line_colored("Update order", Tone::Header);
    for group in tool_groups() {
        logger.line_colored(&format!("  - {}", group.name), Tone::Muted);
    }
    logger.line_colored(
        &format!("  - Projects after toolchains (jobs: {jobs})"),
        Tone::Muted,
    );

    let outputs = run_parallel(cli.dry_run, cli.deep, cli.verbose_log, jobs, &home);

    for output in outputs {
        if cli.dry_run {
            logger.emit(&output.log);
        } else {
            logger.write_log(&output.log);
        }
        records.extend(output.records);
    }

    let summary = summarize(records, logger.path.clone());
    print_summary(&mut logger, &summary, cli.dry_run);

    Ok(summary)
}

fn home_dir() -> Result<PathBuf, AppError> {
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or(AppError::HomeMissing)
}

fn validate_jobs(jobs: usize) -> Result<usize, AppError> {
    if jobs == 0 {
        Err(AppError::InvalidJobs)
    } else {
        Ok(jobs)
    }
}

fn timestamp() -> Result<String, AppError> {
    timestamp_at(OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc()))
}

fn timestamp_at(time: OffsetDateTime) -> Result<String, AppError> {
    let format =
        format_description::parse_borrowed::<2>("[year]-[month]-[day]_[hour]-[minute]-[second]")?;
    Ok(time.format(&format)?)
}

fn retention_cutoff_log_name() -> Result<String, AppError> {
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    Ok(format!(
        "{}.log",
        timestamp_at(now - Duration::days(LOG_RETENTION_DAYS))?
    ))
}

impl Logger {
    fn new(home: &Path) -> Result<(Self, Option<StepRecord>), AppError> {
        let log_dir = home.join("Library").join("Logs").join("daily-ups");
        let log_name = format!("{}.log", timestamp()?);
        let cutoff_log_name = retention_cutoff_log_name()?;
        let log_path = log_dir.join(log_name);

        match fs::create_dir_all(&log_dir).and_then(|()| {
            let _ = prune_old_logs(&log_dir, &cutoff_log_name);
            File::create(&log_path)
        }) {
            Ok(file) => Ok((
                Self {
                    file: Some(file),
                    path: Some(log_path),
                },
                None,
            )),
            Err(error) => Ok((
                Self {
                    file: None,
                    path: None,
                },
                Some(StepRecord::failure(
                    "Logging",
                    format!(
                        "could not create log file under {}: {error}",
                        log_dir.display()
                    ),
                )),
            )),
        }
    }

    fn line(&mut self, line: &str) {
        self.line_with_terminal(line, line);
    }

    fn line_colored(&mut self, line: &str, tone: Tone) {
        let terminal = styled(line, tone);
        self.line_with_terminal(line, &terminal);
    }

    fn line_with_terminal(&mut self, plain: &str, terminal: &str) {
        println!("{terminal}");
        let _ = io::stdout().flush();

        if let Some(file) = self.file.as_mut() {
            let _ = writeln!(file, "{plain}");
            let _ = file.flush();
        }
    }

    fn emit(&mut self, text: &str) {
        print!("{text}");
        let _ = io::stdout().flush();
        self.write_log(text);
    }

    fn write_log(&mut self, text: &str) {
        if let Some(file) = self.file.as_mut() {
            let _ = file.write_all(text.as_bytes());
            let _ = file.flush();
        }
    }
}

fn prune_old_logs(log_dir: &Path, cutoff_log_name: &str) -> io::Result<()> {
    for entry in fs::read_dir(log_dir)? {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };

        if is_daily_log_name(name) && name < cutoff_log_name && path.is_file() {
            let _ = fs::remove_file(path);
        }
    }

    Ok(())
}

fn is_daily_log_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 23
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'_'
        && bytes[13] == b'-'
        && bytes[16] == b'-'
        && &bytes[19..] == b".log"
        && [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18]
            .into_iter()
            .all(|index| bytes[index].is_ascii_digit())
}

fn styled(text: &str, tone: Tone) -> String {
    match tone {
        Tone::Header => text.bold().cyan().to_string(),
        Tone::Success => text.green().to_string(),
        Tone::Failure => text.red().to_string(),
        Tone::Skipped => text.yellow().to_string(),
        Tone::Muted => text.dimmed().to_string(),
    }
}

impl StepRecord {
    fn success(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: StepStatus::Success,
        }
    }

    fn failure(name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: StepStatus::Failure(reason.into()),
        }
    }

    fn skipped(name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: StepStatus::Skipped(reason.into()),
        }
    }
}

fn summarize(records: Vec<StepRecord>, log_path: Option<PathBuf>) -> RunSummary {
    let mut successes = Vec::new();
    let mut failures = Vec::new();
    let mut skipped = Vec::new();

    for record in records {
        match record.status {
            StepStatus::Success => successes.push(record.name),
            StepStatus::Failure(reason) => failures.push(format!("{}: {reason}", record.name)),
            StepStatus::Skipped(reason) => skipped.push(format!("{}: {reason}", record.name)),
        }
    }

    RunSummary {
        successes,
        failures,
        skipped,
        log_path,
    }
}

fn print_summary(logger: &mut Logger, summary: &RunSummary, dry_run: bool) {
    logger.line("");
    logger.line_colored("Summary", Tone::Header);

    if dry_run {
        print_list(logger, "Would run:", &summary.successes, Tone::Muted);
    } else {
        print_list(logger, "Succeeded:", &summary.successes, Tone::Success);
    }

    print_list(logger, "Failed:", &summary.failures, Tone::Failure);
    print_list(logger, "Skipped:", &summary.skipped, Tone::Skipped);

    match summary.log_path.as_ref() {
        Some(path) => logger.line_colored(&format!("Log: {}", path.display()), Tone::Muted),
        None => logger.line_colored("Log: unavailable", Tone::Failure),
    }
}

fn print_list(logger: &mut Logger, title: &str, values: &[String], tone: Tone) {
    logger.line_colored(title, tone);
    if values.is_empty() {
        logger.line_colored("  none", Tone::Muted);
        return;
    }

    for value in values {
        logger.line_colored(&format!("  - {value}"), tone);
    }
}

fn run_parallel(
    dry_run: bool,
    deep: bool,
    verbose_log: bool,
    jobs: usize,
    home: &Path,
) -> Vec<WorkOutput> {
    run_workflows(
        tool_groups(),
        project_tasks(home, deep),
        dry_run,
        verbose_log,
        jobs,
    )
}

fn run_workflows(
    tool_groups: Vec<WorkGroup>,
    projects: Vec<ProjectTask>,
    dry_run: bool,
    verbose_log: bool,
    jobs: usize,
) -> Vec<WorkOutput> {
    let progress = MultiProgress::with_draw_target(ProgressDrawTarget::stderr());
    let style = progress_style();

    let tool_handles = tool_groups
        .into_iter()
        .enumerate()
        .map(|(index, group)| {
            let bar = progress.add(ProgressBar::new(group.commands.len() as u64));
            bar.set_style(style.clone());
            bar.set_prefix(group.name.to_string());
            bar.set_message("queued");
            thread::spawn(move || (index, run_work_group(group, dry_run, verbose_log, bar)))
        })
        .collect::<Vec<_>>();

    let mut outputs = join_indexed_outputs(tool_handles);

    let project_bars = projects
        .into_iter()
        .map(|project| {
            let bar = progress.add(ProgressBar::new(project.commands.len() as u64));
            bar.set_style(style.clone());
            bar.set_prefix(project.name.to_string());
            bar.set_message("queued");
            (project, bar)
        })
        .collect::<Vec<_>>();
    outputs.push(run_projects(project_bars, jobs, dry_run, verbose_log));
    outputs
}

fn progress_style() -> ProgressStyle {
    match ProgressStyle::with_template(
        "{spinner:.cyan} {prefix:.bold.dim} {bar:24.cyan/blue} {pos}/{len} {msg}",
    ) {
        Ok(style) => style.progress_chars("=>-"),
        Err(_) => ProgressStyle::default_bar(),
    }
}

fn join_indexed_outputs(handles: Vec<thread::JoinHandle<(usize, WorkOutput)>>) -> Vec<WorkOutput> {
    let mut outputs = Vec::with_capacity(handles.len());

    for handle in handles {
        match handle.join() {
            Ok(output) => outputs.push(output),
            Err(_) => outputs.push((
                usize::MAX,
                panic_output("Parallel group", "worker panicked"),
            )),
        }
    }

    outputs.sort_by_key(|(index, _)| *index);
    outputs.into_iter().map(|(_, output)| output).collect()
}

fn panic_output(name: &'static str, reason: &'static str) -> WorkOutput {
    WorkOutput {
        records: vec![StepRecord::failure(name, reason)],
        log: format!("\n===> {name}\nFAIL: {reason}\n"),
    }
}

fn run_work_group(
    group: WorkGroup,
    dry_run: bool,
    verbose_log: bool,
    bar: ProgressBar,
) -> WorkOutput {
    let mut log = String::new();
    log.push_str(&format!("\n===> {}\n", group.name));

    let record = run_commands(
        group.name,
        None,
        &group.commands,
        dry_run,
        verbose_log,
        &mut log,
        Some(&bar),
    );
    finish_bar(&bar, group.name, &record.status);

    WorkOutput {
        records: vec![record],
        log,
    }
}

fn run_projects(
    projects: Vec<(ProjectTask, ProgressBar)>,
    jobs: usize,
    dry_run: bool,
    verbose_log: bool,
) -> WorkOutput {
    let mut indexed_outputs = Vec::with_capacity(projects.len());
    let worker_count = jobs.min(projects.len());
    let queue = Arc::new(Mutex::new(
        projects.into_iter().enumerate().collect::<VecDeque<_>>(),
    ));
    let handles = (0..worker_count)
        .map(|_| {
            let queue = Arc::clone(&queue);
            thread::spawn(move || {
                let mut outputs = Vec::new();
                loop {
                    let next = queue.lock().expect("project queue poisoned").pop_front();
                    let Some((project_index, (project, bar))) = next else {
                        break;
                    };
                    outputs.push((
                        project_index,
                        run_project(project, dry_run, verbose_log, bar),
                    ));
                }
                outputs
            })
        })
        .collect::<Vec<_>>();

    for handle in handles {
        match handle.join() {
            Ok(outputs) => indexed_outputs.extend(outputs),
            Err(_) => {
                indexed_outputs.push((usize::MAX, panic_output("Project", "worker panicked")))
            }
        }
    }

    indexed_outputs.sort_by_key(|(index, _)| *index);

    let mut records = Vec::new();
    let mut log = String::new();
    log.push_str("\n===> Projects\n");

    for (_, output) in indexed_outputs {
        records.extend(output.records);
        log.push_str(&output.log);
    }

    WorkOutput { records, log }
}

fn run_project(
    project: ProjectTask,
    dry_run: bool,
    verbose_log: bool,
    bar: ProgressBar,
) -> WorkOutput {
    let mut log = String::new();
    log.push_str(&format!("\n--- {}\n", project.name));
    log.push_str(&format!("Directory: {}\n", project.dir.display()));

    if !project.dir.is_dir() {
        let reason = format!("missing directory: {}", project.dir.display());
        log.push_str(&format!("SKIP: {reason}\n"));
        let record = StepRecord::skipped(project.name, reason);
        finish_bar(&bar, project.name, &record.status);
        return WorkOutput {
            records: vec![record],
            log,
        };
    }

    match is_git_repository(&project.dir) {
        Ok(true) => {}
        Ok(false) => {
            let reason = format!("not a git repository: {}", project.dir.display());
            log.push_str(&format!("SKIP: {reason}\n"));
            let record = StepRecord::skipped(project.name, reason);
            finish_bar(&bar, project.name, &record.status);
            return WorkOutput {
                records: vec![record],
                log,
            };
        }
        Err(reason) => {
            log.push_str(&format!("FAIL: {reason}\n"));
            let record = StepRecord::failure(project.name, reason);
            finish_bar(&bar, project.name, &record.status);
            return WorkOutput {
                records: vec![record],
                log,
            };
        }
    }

    match local_change_count(&project.dir) {
        Ok(0) => {}
        Ok(count) => {
            let reason = format!("{count} local change(s)");
            log.push_str(&format!("SKIP: {reason}\n"));
            let record = StepRecord::skipped(project.name, reason);
            finish_bar(&bar, project.name, &record.status);
            return WorkOutput {
                records: vec![record],
                log,
            };
        }
        Err(reason) => {
            log.push_str(&format!("FAIL: {reason}\n"));
            let record = StepRecord::failure(project.name, reason);
            finish_bar(&bar, project.name, &record.status);
            return WorkOutput {
                records: vec![record],
                log,
            };
        }
    }

    let record = run_commands(
        project.name,
        Some(project.dir.as_path()),
        &project.commands,
        dry_run,
        verbose_log,
        &mut log,
        Some(&bar),
    );
    finish_bar(&bar, project.name, &record.status);

    WorkOutput {
        records: vec![record],
        log,
    }
}

fn run_commands(
    name: &'static str,
    current_dir: Option<&Path>,
    commands: &[CommandSpec],
    dry_run: bool,
    verbose_log: bool,
    log: &mut String,
    bar: Option<&ProgressBar>,
) -> StepRecord {
    for command in commands {
        if let Some(bar) = bar {
            bar.set_message(command.display());
        }
        log.push_str(&format!("> {}\n", command.display()));

        if dry_run {
            if let Some(bar) = bar {
                bar.inc(1);
            }
            continue;
        }

        match run_command(command, current_dir) {
            Ok(output) => {
                if verbose_log {
                    log.push_str(&output);
                }
                if let Some(bar) = bar {
                    bar.inc(1);
                }
            }
            Err(failure) => {
                if !failure.output.is_empty() {
                    log.push_str(&failure.output);
                }
                log.push_str(&format!("FAIL: {}\n", failure.reason));
                return StepRecord::failure(name, failure.reason);
            }
        }
    }

    log.push_str("OK\n");
    StepRecord::success(name)
}

fn finish_bar(bar: &ProgressBar, name: &str, status: &StepStatus) {
    match status {
        StepStatus::Success => bar.finish_with_message(format!("{} {name}", "OK".green())),
        StepStatus::Failure(_) => bar.finish_with_message(format!("{} {name}", "FAIL".red())),
        StepStatus::Skipped(_) => bar.finish_with_message(format!("{} {name}", "SKIP".yellow())),
    }
}

#[derive(Debug)]
struct CommandFailure {
    reason: String,
    output: String,
}

fn run_command(
    command: &CommandSpec,
    current_dir: Option<&Path>,
) -> Result<String, CommandFailure> {
    let mut process = Command::new(command.program);
    process.args(command.args);

    if let Some(dir) = current_dir {
        process.current_dir(dir);
    }

    let output = process.output().map_err(|error| CommandFailure {
        reason: format!("failed to start `{}`: {error}", command.display()),
        output: String::new(),
    })?;

    let mut log = String::new();
    for bytes in [&output.stdout, &output.stderr] {
        if bytes.is_empty() {
            continue;
        }

        log.push_str(&String::from_utf8_lossy(bytes));
        if !log.ends_with('\n') {
            log.push('\n');
        }
    }

    if output.status.success() {
        Ok(log)
    } else {
        let code = output
            .status
            .code()
            .map_or_else(|| "signal".to_string(), |code| code.to_string());
        Err(CommandFailure {
            reason: format!("`{}` exited with {code}", command.display()),
            output: log,
        })
    }
}

fn is_git_repository(dir: &Path) -> Result<bool, String> {
    let output = Command::new("git")
        .args(["-C"])
        .arg(dir)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map_err(|error| format!("failed to start `git rev-parse`: {error}"))?;

    Ok(output.status.success())
}

fn local_change_count(dir: &Path) -> Result<usize, String> {
    let output = Command::new("git")
        .args(["-C"])
        .arg(dir)
        .args(["status", "--porcelain"])
        .output()
        .map_err(|error| format!("failed to start `git status`: {error}"))?;

    if !output.status.success() {
        let code = output
            .status
            .code()
            .map_or_else(|| "signal".to_string(), |code| code.to_string());
        return Err(format!("`git status --porcelain` exited with {code}"));
    }

    Ok(String::from_utf8_lossy(&output.stdout).lines().count())
}

impl CommandSpec {
    fn display(&self) -> String {
        let mut parts = Vec::with_capacity(self.args.len() + 1);
        parts.push(self.program.to_string());
        parts.extend(self.args.iter().map(|arg| (*arg).to_string()));
        parts.join(" ")
    }
}

fn tool_groups() -> Vec<WorkGroup> {
    vec![
        WorkGroup {
            name: "Homebrew",
            commands: vec![
                CommandSpec {
                    program: "brew",
                    args: &["update"],
                },
                CommandSpec {
                    program: "brew",
                    args: &["upgrade", "-y"],
                },
            ],
        },
        WorkGroup {
            name: "Rust",
            commands: vec![CommandSpec {
                program: "rustup",
                args: &["update"],
            }],
        },
        WorkGroup {
            name: "Node",
            commands: vec![CommandSpec {
                program: "pnpm",
                args: &["up", "-g"],
            }],
        },
        WorkGroup {
            name: "Global skills",
            commands: vec![CommandSpec {
                program: "bunx",
                args: &["skills", "update", "--global"],
            }],
        },
        WorkGroup {
            name: ".NET tools",
            commands: vec![CommandSpec {
                program: "dotnet",
                args: &["updatealltools"],
            }],
        },
        WorkGroup {
            name: "Cargo tools",
            commands: vec![CommandSpec {
                program: "cargo",
                args: &["install-update", "-a"],
            }],
        },
        WorkGroup {
            name: "Go tools",
            commands: vec![
                CommandSpec {
                    program: "go",
                    args: &["install", "github.com/go-delve/delve/cmd/dlv@latest"],
                },
                CommandSpec {
                    program: "go",
                    args: &["install", "honnef.co/go/tools/cmd/staticcheck@latest"],
                },
                CommandSpec {
                    program: "go",
                    args: &["install", "golang.org/x/perf/cmd/benchstat@latest"],
                },
            ],
        },
    ]
}

fn project_tasks(home: &Path, deep: bool) -> Vec<ProjectTask> {
    vec![
        ProjectTask {
            name: "project: lrs",
            dir: home
                .join("Documents")
                .join("source")
                .join("rust")
                .join("lrs"),
            commands: rust_project_commands(deep),
        },
        ProjectTask {
            name: "project: axes",
            dir: home
                .join("Documents")
                .join("source")
                .join("rust")
                .join("axes"),
            commands: rust_project_commands(deep),
        },
        ProjectTask {
            name: "project: lcsSln",
            dir: home.join("Documents").join("source").join("lcsSln"),
            commands: dotnet_project_commands(deep),
        },
        ProjectTask {
            name: "project: lpy",
            dir: home.join("Documents").join("source").join("lpy"),
            commands: vec![CommandSpec {
                program: "uv",
                args: &["sync", "-U"],
            }],
        },
        ProjectTask {
            name: "project: ponytail",
            dir: home.join("Documents").join("source").join("ponytail"),
            commands: vec![CommandSpec {
                program: "git",
                args: &["pull"],
            }],
        },
    ]
}

fn rust_project_commands(deep: bool) -> Vec<CommandSpec> {
    let mut commands = vec![CommandSpec {
        program: "cargo",
        args: &["update"],
    }];

    if deep {
        commands.push(CommandSpec {
            program: "cargo",
            args: &["clean"],
        });
    }

    commands.push(CommandSpec {
        program: "cargo",
        args: &["build"],
    });

    commands
}

fn dotnet_project_commands(deep: bool) -> Vec<CommandSpec> {
    let mut commands = vec![CommandSpec {
        program: "dotnet",
        args: &["outdated", "-u", "-i"],
    }];

    if deep {
        commands.push(CommandSpec {
            program: "dotnet",
            args: &["clean"],
        });
    }

    commands.push(CommandSpec {
        program: "dotnet",
        args: &["build"],
    });
    commands.push(CommandSpec {
        program: "dotnet",
        args: &["build", "-c", "Release"],
    });

    commands
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn rejects_zero_jobs() {
        assert!(matches!(validate_jobs(0), Err(AppError::InvalidJobs)));
    }

    #[test]
    fn defaults_to_four_project_jobs() {
        let cli = Cli::parse_from(["daily-ups"]);

        assert_eq!(cli.jobs, 4);
    }

    #[test]
    fn command_display_includes_arguments() {
        let command = CommandSpec {
            program: "dotnet",
            args: &["build", "-c", "Release"],
        };

        assert_eq!(command.display(), "dotnet build -c Release");
    }

    #[test]
    fn global_skills_are_reported_as_their_own_group() {
        let groups = tool_groups();
        let global_skills = groups
            .iter()
            .find(|group| group.name == "Global skills")
            .expect("global skills group exists");

        assert_eq!(global_skills.commands.len(), 1);
        assert_eq!(
            global_skills.commands[0].display(),
            "bunx skills update --global"
        );

        let node = groups
            .iter()
            .find(|group| group.name == "Node")
            .expect("node group exists");
        assert!(
            !node
                .commands
                .iter()
                .any(|command| command.display().contains("skills update"))
        );
    }

    #[test]
    fn deep_mode_adds_clean_commands() {
        let rust_commands = rust_project_commands(true)
            .into_iter()
            .map(|command| command.display())
            .collect::<Vec<_>>();
        let dotnet_commands = dotnet_project_commands(true)
            .into_iter()
            .map(|command| command.display())
            .collect::<Vec<_>>();

        assert!(rust_commands.contains(&"cargo clean".to_string()));
        assert!(dotnet_commands.contains(&"dotnet clean".to_string()));
    }

    #[test]
    fn default_mode_omits_clean_commands() {
        let rust_commands = rust_project_commands(false)
            .into_iter()
            .map(|command| command.display())
            .collect::<Vec<_>>();
        let dotnet_commands = dotnet_project_commands(false)
            .into_iter()
            .map(|command| command.display())
            .collect::<Vec<_>>();

        assert!(!rust_commands.contains(&"cargo clean".to_string()));
        assert!(!dotnet_commands.contains(&"dotnet clean".to_string()));
    }

    #[test]
    fn ponytail_project_only_pulls() {
        let home = Path::new("/Users/example");
        let ponytail = project_tasks(home, false)
            .into_iter()
            .find(|project| project.name == "project: ponytail")
            .expect("ponytail project exists");

        assert_eq!(
            ponytail.dir,
            Path::new("/Users/example")
                .join("Documents")
                .join("source")
                .join("ponytail")
        );
        assert_eq!(ponytail.commands.len(), 1);
        assert_eq!(ponytail.commands[0].display(), "git pull");
    }

    #[test]
    fn dry_run_with_missing_project_dirs_skips_projects() {
        let home = tempdir().expect("create temp home");
        let cli = Cli {
            dry_run: true,
            deep: false,
            jobs: 2,
            verbose_log: false,
        };

        let summary = run_with_home(cli, home.path().to_path_buf()).expect("run dry-run");

        assert_eq!(summary.skipped.len(), 5);
        assert_eq!(summary.exit_code(), 1);
    }

    #[test]
    fn dirty_project_is_skipped() {
        let home = tempdir().expect("create temp home");
        let project_dir = home
            .path()
            .join("Documents")
            .join("source")
            .join("rust")
            .join("lrs");
        fs::create_dir_all(&project_dir).expect("create project dir");
        git(&project_dir, &["init"]);
        fs::write(project_dir.join("dirty.txt"), "changed").expect("write dirty file");

        let output = run_project(
            ProjectTask {
                name: "project: lrs",
                dir: project_dir,
                commands: rust_project_commands(false),
            },
            true,
            false,
            hidden_bar(2),
        );

        assert!(matches!(
            output.records.first().map(|record| &record.status),
            Some(StepStatus::Skipped(reason)) if reason == "1 local change(s)"
        ));
    }

    #[test]
    fn project_jobs_refill_when_one_finishes() {
        use std::time::{Duration, Instant};

        const FAST: &[&str] = &["-c", "sleep 0.05"];
        const SLOW: &[&str] = &["-c", "sleep 0.5"];

        let home = tempdir().expect("create temp home");
        let projects = vec![
            test_project(home.path(), "project: slow-1", "slow-1", SLOW),
            test_project(home.path(), "project: fast-1", "fast-1", FAST),
            test_project(home.path(), "project: fast-2", "fast-2", FAST),
            test_project(home.path(), "project: fast-3", "fast-3", FAST),
            test_project(home.path(), "project: slow-2", "slow-2", SLOW),
        ];

        let started = Instant::now();
        let output = run_projects(projects, 4, false, false);

        assert!(
            started.elapsed() < Duration::from_millis(900),
            "projects waited for the whole first batch before refilling"
        );
        assert_eq!(output.records.len(), 5);
        assert!(
            output
                .records
                .iter()
                .all(|record| record.status == StepStatus::Success)
        );
    }

    #[test]
    fn projects_start_after_tool_groups_finish() {
        let home = tempdir().expect("create temp home");
        let marker = home.path().join("tool.done");
        let project_dir = home.path().join("project");
        fs::create_dir_all(&project_dir).expect("create project dir");
        git(&project_dir, &["init"]);

        let outputs = run_workflows(
            vec![WorkGroup {
                name: "slow tool",
                commands: vec![CommandSpec {
                    program: "sh",
                    args: sh_args(format!("sleep 0.2; touch {}", shell_quote(&marker))),
                }],
            }],
            vec![ProjectTask {
                name: "project: waits",
                dir: project_dir,
                commands: vec![CommandSpec {
                    program: "sh",
                    args: sh_args(format!("test -f {}", shell_quote(&marker))),
                }],
            }],
            false,
            false,
            1,
        );

        let statuses = outputs
            .into_iter()
            .flat_map(|output| output.records)
            .map(|record| record.status)
            .collect::<Vec<_>>();

        assert_eq!(statuses, vec![StepStatus::Success, StepStatus::Success]);
    }

    #[test]
    fn default_log_omits_success_output_but_keeps_failure_output() {
        let dir = tempdir().expect("create temp dir");
        let dir_text = dir.path().display().to_string();
        let mut success_log = String::new();

        let success = run_commands(
            "success",
            Some(dir.path()),
            &[CommandSpec {
                program: "pwd",
                args: &[],
            }],
            false,
            false,
            &mut success_log,
            None,
        );

        assert_eq!(success.status, StepStatus::Success);
        assert!(success_log.contains("> pwd\n"));
        assert!(success_log.contains("OK\n"));
        assert!(!success_log.contains(&dir_text));

        let mut failure_log = String::new();
        let failure = run_commands(
            "failure",
            Some(dir.path()),
            &[CommandSpec {
                program: "sh",
                args: &["-c", "pwd; exit 9"],
            }],
            false,
            false,
            &mut failure_log,
            None,
        );

        assert!(matches!(failure.status, StepStatus::Failure(_)));
        assert!(failure_log.contains(&dir_text));
        assert!(failure_log.contains("FAIL: `sh -c pwd; exit 9` exited with 9\n"));
    }

    #[test]
    fn verbose_log_keeps_success_output() {
        let dir = tempdir().expect("create temp dir");
        let mut log = String::new();

        let record = run_commands(
            "success",
            Some(dir.path()),
            &[CommandSpec {
                program: "pwd",
                args: &[],
            }],
            false,
            true,
            &mut log,
            None,
        );

        assert_eq!(record.status, StepStatus::Success);
        assert!(log.contains(&dir.path().display().to_string()));
    }

    #[test]
    fn prunes_logs_older_than_cutoff() {
        let dir = tempdir().expect("create temp dir");
        let old_log = dir.path().join("2026-06-18_10-00-00.log");
        let cutoff_log = dir.path().join("2026-06-19_10-00-00.log");
        let new_log = dir.path().join("2026-06-20_10-00-00.log");
        let other_log = dir.path().join("notes.log");

        fs::write(&old_log, "old").expect("write old log");
        fs::write(&cutoff_log, "cutoff").expect("write cutoff log");
        fs::write(&new_log, "new").expect("write new log");
        fs::write(&other_log, "other").expect("write other log");

        prune_old_logs(dir.path(), "2026-06-19_10-00-00.log").expect("prune logs");

        assert!(!old_log.exists());
        assert!(cutoff_log.exists());
        assert!(new_log.exists());
        assert!(other_log.exists());
    }

    #[test]
    fn unavailable_log_path_is_reported_as_failure() {
        let home = tempdir().expect("create temp home");
        let file_home = home.path().join("not-a-directory");
        fs::write(&file_home, "x").expect("write file home");

        let (_logger, failure) = Logger::new(&file_home).expect("create logger");

        assert!(matches!(
            failure.map(|record| record.status),
            Some(StepStatus::Failure(_))
        ));
    }

    fn test_project(
        root: &Path,
        name: &'static str,
        dir_name: &str,
        args: &'static [&'static str],
    ) -> (ProjectTask, ProgressBar) {
        let dir = root.join(dir_name);
        fs::create_dir_all(&dir).expect("create project dir");
        git(&dir, &["init"]);
        (
            ProjectTask {
                name,
                dir,
                commands: vec![CommandSpec {
                    program: "sh",
                    args,
                }],
            },
            hidden_bar(1),
        )
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .expect("run git");
        assert!(status.success(), "git command failed: {args:?}");
    }

    fn hidden_bar(len: u64) -> ProgressBar {
        let bar = ProgressBar::hidden();
        bar.set_length(len);
        bar
    }

    fn sh_args(command: String) -> &'static [&'static str] {
        let command = Box::leak(command.into_boxed_str());
        Box::leak(vec!["-c", command].into_boxed_slice())
    }

    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
    }
}

//! Option parsing compatible with libmakepkg `parseopts` (owner: agent B).
use crate::actions::Op;
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub op: Op,
    pub arch: Option<String>,
    /// Empty = use `pacman-conf CacheDir`.
    pub cachedirs: Vec<PathBuf>,
    pub force: bool,
    /// From -i (commas or stdin) and -u (pacman -Qq), already merged.
    pub blacklist: Vec<String>,
    pub keep: usize,
    pub min_atime: Option<i64>,
    pub min_mtime: Option<i64>,
    pub nocolor: bool,
    pub null: bool,
    pub quiet: bool,
    pub verbose: u8,
    /// Positional args.
    pub whitelist: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    Run(Config),
    /// Text to print on stdout, exit 0.
    Help(String),
    Version(String),
}

/// Failure: exact message(s) and exit code the script produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    /// Lines for stderr as the script prints them (parseopts lines are raw,
    /// `die` lines are passed through output::error formatting by the caller when `is_die`).
    pub message: String,
    pub is_die: bool,
    pub code: i32,
}

/// Environment hooks so parsing is testable.
pub trait Env {
    fn stdin_is_tty(&self) -> bool;
    fn read_stdin(&mut self) -> String;
    /// `pacman -Qq` output lines, or None on failure / empty.
    fn installed_packages(&mut self) -> Option<Vec<String>>;
    /// `date -d TEXT +%s` equivalent; None when unparsable.
    fn parse_date(&self, text: &str) -> Option<i64>;
    /// Basename of argv[0] (parseopts messages use it).
    fn progname(&self) -> String;
}

/// Parse args (without argv[0]) including the script's post-parse sanity checks
/// on operations (none / more than one). Filesystem checks are NOT done here.
pub fn parse(args: &[OsString], env: &mut dyn Env) -> Result<Parsed, CliError> {
    let _ = (args, env);
    todo!("agent B")
}

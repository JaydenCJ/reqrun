//! Command-line interface: argument parsing (std only) and the top-level
//! run that maps outcomes to exit codes.
//!
//! Exit codes: `0` everything passed, `1` at least one request failed or
//! errored, `2` usage / parse / environment problems.

use crate::report::{self, Style};
use crate::runner::{self, Options, Status};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

const USAGE: &str = "\
reqrun — run JetBrains-style .http request files with assertions

USAGE:
    reqrun [OPTIONS] <FILE>...

ARGS:
    <FILE>...             one or more .http files, run in order

OPTIONS:
    -e, --env <NAME>      environment from http-client.env.json (+ private file)
        --env-file <PATH> explicit env file (default: next to each .http file)
        --var <K=V>       set/override a variable (repeatable)
    -r, --request <NAME>  run only the named request(s) (repeatable)
        --timeout <DUR>   per-request timeout, e.g. 5s, 500ms, 2m (default 30s)
        --strict          fail requests without assertions when status >= 400
        --fail-fast       stop at the first failure; remaining requests skip
        --dry-run         resolve variables and print requests, send nothing
    -l, --list            list request names and methods without running
        --report <PATH>   write a JUnit XML report
    -v, --verbose         show response heads and passing checks
        --no-color        disable ANSI colors (also honors NO_COLOR)
    -h, --help            print this help
    -V, --version         print version

EXIT CODES:
    0 all requests passed    1 failures or errors    2 usage/parse/env error
";

#[derive(Debug)]
pub struct Cli {
    pub files: Vec<PathBuf>,
    pub options: Options,
    pub list: bool,
    pub report_path: Option<PathBuf>,
    pub verbose: bool,
    pub no_color: bool,
}

/// What the argument parser decided.
#[derive(Debug)]
pub enum Parsed {
    Run(Box<Cli>),
    Help,
    Version,
}

/// Parse `argv[1..]`. Errors are usage errors (exit 2).
pub fn parse_args(args: &[String]) -> Result<Parsed, String> {
    let mut cli = Cli {
        files: Vec::new(),
        options: Options::default(),
        list: false,
        report_path: None,
        verbose: false,
        no_color: false,
    };
    let mut it = args.iter().peekable();
    while let Some(arg) = it.next() {
        let mut value_for = |flag: &str| -> Result<String, String> {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match arg.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "-V" | "--version" => return Ok(Parsed::Version),
            "-e" | "--env" => cli.options.env = Some(value_for(arg)?),
            "--env-file" => cli.options.env_file = Some(PathBuf::from(value_for(arg)?)),
            "--var" => {
                let pair = value_for(arg)?;
                let (k, v) = pair
                    .split_once('=')
                    .ok_or_else(|| format!("--var needs name=value, got '{pair}'"))?;
                cli.options
                    .overrides
                    .push((k.trim().to_string(), v.to_string()));
            }
            "-r" | "--request" => cli.options.filter.push(value_for(arg)?),
            "--timeout" => cli.options.timeout = parse_duration(&value_for(arg)?)?,
            "--strict" => cli.options.strict = true,
            "--fail-fast" => cli.options.fail_fast = true,
            "--dry-run" => cli.options.dry_run = true,
            "-l" | "--list" => cli.list = true,
            "--report" => cli.report_path = Some(PathBuf::from(value_for(arg)?)),
            "-v" | "--verbose" => cli.verbose = true,
            "--no-color" => cli.no_color = true,
            flag if flag.starts_with('-') => {
                return Err(format!("unknown option '{flag}' (see --help)"))
            }
            file => cli.files.push(PathBuf::from(file)),
        }
    }
    if cli.files.is_empty() {
        return Err("no .http files given (see --help)".into());
    }
    Ok(Parsed::Run(Box::new(cli)))
}

/// Durations like `500ms`, `5s`, `2m`; a bare number means seconds.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let (number, unit) = match text.find(|c: char| !c.is_ascii_digit()) {
        Some(idx) => (&text[..idx], &text[idx..]),
        None => (text, "s"),
    };
    let n: u64 = number
        .parse()
        .map_err(|_| format!("invalid duration '{text}'"))?;
    match unit {
        "ms" => Ok(Duration::from_millis(n)),
        "s" => Ok(Duration::from_secs(n)),
        "m" => Ok(Duration::from_secs(n * 60)),
        _ => Err(format!("invalid duration unit '{unit}' (use ms, s or m)")),
    }
}

/// Entry point used by `main`; returns the process exit code.
pub fn run(args: &[String]) -> i32 {
    let cli = match parse_args(args) {
        Ok(Parsed::Help) => {
            print!("{USAGE}");
            return 0;
        }
        Ok(Parsed::Version) => {
            println!("reqrun {}", env!("CARGO_PKG_VERSION"));
            return 0;
        }
        Ok(Parsed::Run(cli)) => cli,
        Err(e) => {
            eprintln!("reqrun: {e}");
            return 2;
        }
    };

    let color =
        !cli.no_color && std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal();
    let style = Style::colored(color);

    if cli.list {
        match runner::list_requests(&cli.files) {
            Ok(listing) => {
                for (path, names) in listing {
                    println!("{path}");
                    for name in names {
                        println!("  {name}");
                    }
                }
                return 0;
            }
            Err(e) => {
                eprintln!("reqrun: {e}");
                return 2;
            }
        }
    }

    let results = match runner::run_files(&cli.files, &cli.options) {
        Ok(results) => results,
        Err(e) => {
            eprintln!("reqrun: {e}");
            return 2;
        }
    };

    if cli.options.dry_run {
        for file in &results {
            for r in &file.results {
                match (&r.rendered, &r.error) {
                    (Some(rendered), _) => {
                        println!("### {} ({})", r.name, file.path);
                        println!("{rendered}");
                        println!();
                    }
                    (None, Some(err)) => {
                        println!("### {} ({})", r.name, file.path);
                        println!("ERROR: {err}");
                        println!();
                    }
                    _ => {}
                }
            }
        }
    } else {
        print!("{}", report::console(&results, &style, cli.verbose));
    }

    if let Some(path) = &cli.report_path {
        if let Err(e) = std::fs::write(path, report::junit(&results)) {
            eprintln!("reqrun: cannot write report {}: {e}", path.display());
            return 2;
        }
    }

    let bad = results
        .iter()
        .flat_map(|f| &f.results)
        .any(|r| matches!(r.status, Status::Failed | Status::Error));
    if bad {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    fn parsed(args: &[&str]) -> Cli {
        match parse_args(&argv(args)).expect("parse failed") {
            Parsed::Run(cli) => *cli,
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn parses_files_and_flags() {
        let cli = parsed(&[
            "a.http",
            "b.http",
            "--env",
            "dev",
            "--var",
            "k=v",
            "--var",
            "x=1=2",
            "-r",
            "login",
            "--strict",
            "--fail-fast",
            "--report",
            "out.xml",
        ]);
        assert_eq!(cli.files.len(), 2);
        assert_eq!(cli.options.env.as_deref(), Some("dev"));
        assert_eq!(
            cli.options.overrides,
            vec![
                ("k".to_string(), "v".to_string()),
                ("x".to_string(), "1=2".to_string()) // value may contain '='
            ]
        );
        assert_eq!(cli.options.filter, vec!["login"]);
        assert!(cli.options.strict && cli.options.fail_fast);
        assert_eq!(cli.report_path.unwrap().to_str(), Some("out.xml"));
    }

    #[test]
    fn help_and_version_short_circuit() {
        assert!(matches!(parse_args(&argv(&["--help"])), Ok(Parsed::Help)));
        assert!(matches!(
            parse_args(&argv(&["a.http", "-V"])),
            Ok(Parsed::Version)
        ));
    }

    #[test]
    fn usage_errors_are_descriptive() {
        let err = parse_args(&argv(&["--strict"])).unwrap_err();
        assert!(err.contains("no .http files"), "got: {err}");
        let err = parse_args(&argv(&["a.http", "--frobnicate"])).unwrap_err();
        assert!(err.contains("--frobnicate"), "got: {err}");
        let err = parse_args(&argv(&["a.http", "--var", "novalue"])).unwrap_err();
        assert!(err.contains("name=value"), "got: {err}");
        let err = parse_args(&argv(&["a.http", "--env"])).unwrap_err();
        assert!(err.contains("needs a value"), "got: {err}");
    }

    #[test]
    fn durations_parse_with_units() {
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("5s").unwrap(), Duration::from_secs(5));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("7").unwrap(), Duration::from_secs(7));
        assert!(parse_duration("5h").is_err());
        assert!(parse_duration("abc").is_err());
    }
}

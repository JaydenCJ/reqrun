//! Result presentation: the console report and the JUnit XML export.

use crate::runner::{FileResult, Status};
use std::fmt::Write as _;
use std::time::Duration;

/// ANSI palette; every field is empty when color is off, so formatting code
/// never branches on it.
pub struct Style {
    pub green: &'static str,
    pub red: &'static str,
    pub yellow: &'static str,
    pub dim: &'static str,
    pub bold: &'static str,
    pub reset: &'static str,
}

impl Style {
    pub fn colored(enabled: bool) -> Style {
        if enabled {
            Style {
                green: "\x1b[32m",
                red: "\x1b[31m",
                yellow: "\x1b[33m",
                dim: "\x1b[2m",
                bold: "\x1b[1m",
                reset: "\x1b[0m",
            }
        } else {
            Style {
                green: "",
                red: "",
                yellow: "",
                dim: "",
                bold: "",
                reset: "",
            }
        }
    }
}

fn fmt_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms >= 1000 {
        format!("{:.1}s", d.as_secs_f64())
    } else {
        format!("{ms}ms")
    }
}

/// Render the human report for all files. `verbose` adds response heads and
/// per-assertion lines even for passing requests.
pub fn console(results: &[FileResult], style: &Style, verbose: bool) -> String {
    let mut out = String::new();
    for file in results {
        let _ = writeln!(out, "{}{}{}", style.bold, file.path, style.reset);
        for r in &file.results {
            let (mark, color) = match r.status {
                Status::Passed => ("PASS", style.green),
                Status::Failed => ("FAIL", style.red),
                Status::Error => ("ERROR", style.red),
                Status::Skipped => ("SKIP", style.yellow),
            };
            let http = match r.http_status {
                Some(code) => format!(
                    " {}({} {}, {}){}",
                    style.dim,
                    code,
                    r.http_reason,
                    fmt_duration(r.duration),
                    style.reset
                ),
                None => String::new(),
            };
            let checks = if r.assertions.is_empty() {
                String::new()
            } else {
                let passed = r.assertions.iter().filter(|a| a.passed).count();
                format!(
                    " {}[{passed}/{} checks]{}",
                    style.dim,
                    r.assertions.len(),
                    style.reset
                )
            };
            let _ = writeln!(
                out,
                "  {color}{mark:<5}{} {}{http}{checks}",
                style.reset, r.name
            );
            if let Some(err) = &r.error {
                let _ = writeln!(out, "        {}{}{}", style.red, err, style.reset);
            }
            for a in &r.assertions {
                if a.passed && !verbose {
                    continue;
                }
                let (sym, color) = if a.passed {
                    ("ok", style.green)
                } else {
                    ("not ok", style.red)
                };
                let scope = a
                    .test
                    .as_ref()
                    .map(|t| format!("{t}: "))
                    .unwrap_or_default();
                let _ = writeln!(
                    out,
                    "        {color}{sym}{} {scope}{}",
                    style.reset, a.message
                );
            }
            for log in &r.logs {
                let _ = writeln!(out, "        {}log: {log}{}", style.dim, style.reset);
            }
            if verbose {
                if let Some(rendered) = &r.rendered {
                    for line in rendered.lines() {
                        let _ = writeln!(out, "        {}| {line}{}", style.dim, style.reset);
                    }
                }
            }
        }
    }
    out.push_str(&summary(results, style));
    out
}

/// One-line closing summary, colored by the worst outcome.
pub fn summary(results: &[FileResult], style: &Style) -> String {
    let mut totals = (0usize, 0usize, 0usize, 0usize);
    let mut checks = 0usize;
    for file in results {
        let c = file.counts();
        totals.0 += c.0;
        totals.1 += c.1;
        totals.2 += c.2;
        totals.3 += c.3;
        checks += file
            .results
            .iter()
            .map(|r| r.assertions.len())
            .sum::<usize>();
    }
    let ran = totals.0 + totals.1 + totals.2;
    let color = if totals.1 + totals.2 > 0 {
        style.red
    } else {
        style.green
    };
    let mut line = format!(
        "{color}{} request(s): {} passed, {} failed",
        ran, totals.0, totals.1
    );
    if totals.2 > 0 {
        let _ = write!(line, ", {} errored", totals.2);
    }
    if totals.3 > 0 {
        let _ = write!(line, ", {} skipped", totals.3);
    }
    let _ = writeln!(line, " — {checks} check(s){}", style.reset);
    line
}

/// JUnit XML for CI systems: one `<testsuite>` per file, one `<testcase>`
/// per request. Failed assertions become `<failure>`, errors `<error>`,
/// skipped requests `<skipped/>`.
pub fn junit(results: &[FileResult]) -> String {
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites>\n");
    for file in results {
        let (_, failed, errored, skipped) = file.counts();
        let time: f64 = file.results.iter().map(|r| r.duration.as_secs_f64()).sum();
        let _ = writeln!(
            out,
            "  <testsuite name=\"{}\" tests=\"{}\" failures=\"{}\" errors=\"{}\" skipped=\"{}\" time=\"{:.3}\">",
            xml_escape(&file.path),
            file.results.len(),
            failed,
            errored,
            skipped,
            time
        );
        for r in &file.results {
            let _ = write!(
                out,
                "    <testcase name=\"{}\" classname=\"{}\" time=\"{:.3}\"",
                xml_escape(&r.name),
                xml_escape(&file.path),
                r.duration.as_secs_f64()
            );
            match r.status {
                Status::Passed => {
                    out.push_str("/>\n");
                }
                Status::Skipped => {
                    out.push_str(">\n      <skipped/>\n    </testcase>\n");
                }
                Status::Failed => {
                    out.push_str(">\n");
                    for a in r.assertions.iter().filter(|a| !a.passed) {
                        let _ = writeln!(
                            out,
                            "      <failure message=\"{}\"/>",
                            xml_escape(&a.message)
                        );
                    }
                    out.push_str("    </testcase>\n");
                }
                Status::Error => {
                    let _ = writeln!(
                        out,
                        ">\n      <error message=\"{}\"/>\n    </testcase>",
                        xml_escape(r.error.as_deref().unwrap_or("unknown error"))
                    );
                }
            }
        }
        out.push_str("  </testsuite>\n");
    }
    out.push_str("</testsuites>\n");
    out
}

fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::RequestResult;
    use crate::script::Assertion;

    fn plain() -> Style {
        Style::colored(false)
    }

    fn result(name: &str, status: Status) -> RequestResult {
        RequestResult {
            name: name.to_string(),
            method: "GET".to_string(),
            url: "http://example.test/".to_string(),
            status,
            http_status: match status {
                Status::Passed => Some(200),
                Status::Failed => Some(500),
                _ => None,
            },
            http_reason: "OK".to_string(),
            duration: Duration::from_millis(12),
            assertions: Vec::new(),
            error: if status == Status::Error {
                Some("cannot connect to 127.0.0.1:1".to_string())
            } else {
                None
            },
            logs: Vec::new(),
            rendered: None,
        }
    }

    fn fixture() -> Vec<FileResult> {
        let mut failed = result("bad", Status::Failed);
        failed.assertions.push(Assertion {
            test: Some("checks".to_string()),
            passed: false,
            message: "assert failed: response.status === 200 & <x>".to_string(),
        });
        failed.assertions.push(Assertion {
            test: None,
            passed: true,
            message: "response.body.ok".to_string(),
        });
        vec![FileResult {
            path: "api.http".to_string(),
            results: vec![
                result("good", Status::Passed),
                failed,
                result("broken", Status::Error),
                result("later", Status::Skipped),
            ],
        }]
    }

    #[test]
    fn console_shows_status_marks_and_failure_details() {
        let text = console(&fixture(), &plain(), false);
        assert!(text.contains("PASS  good"));
        assert!(text.contains("FAIL  bad"));
        assert!(text.contains("ERROR broken"));
        assert!(text.contains("SKIP  later"));
        assert!(text.contains("not ok checks: assert failed"));
        assert!(text.contains("cannot connect"));
        // Passing assertions stay quiet without --verbose.
        assert!(!text.contains("ok response.body.ok"));
        // Colored output wraps the status marks in ANSI codes.
        let colored = console(&fixture(), &Style::colored(true), false);
        assert!(colored.contains("\x1b[32mPASS"));
        assert!(colored.contains("\x1b[31mFAIL"));
    }

    #[test]
    fn verbose_console_shows_passing_checks() {
        let text = console(&fixture(), &plain(), true);
        assert!(text.contains("ok response.body.ok"));
    }

    #[test]
    fn summary_counts_everything() {
        let text = summary(&fixture(), &plain());
        assert!(
            text.contains("3 request(s): 1 passed, 1 failed, 1 errored, 1 skipped — 2 check(s)"),
            "got: {text}"
        );
    }

    #[test]
    fn junit_structure_counts_and_escaping() {
        let xml = junit(&fixture());
        assert!(xml.starts_with("<?xml version=\"1.0\""));
        assert!(xml.contains(
            "<testsuite name=\"api.http\" tests=\"4\" failures=\"1\" errors=\"1\" skipped=\"1\""
        ));
        assert!(xml.contains("<testcase name=\"good\" classname=\"api.http\""));
        assert!(
            xml.contains("&amp; &lt;x&gt;"),
            "XML special chars must be escaped"
        );
        assert!(xml.contains("<skipped/>"));
        assert!(xml.contains("<error message=\"cannot connect"));
        assert!(xml.ends_with("</testsuites>\n"));
        // Duration formatting used in both reports.
        assert_eq!(fmt_duration(Duration::from_millis(12)), "12ms");
        assert_eq!(fmt_duration(Duration::from_millis(1500)), "1.5s");
    }
}

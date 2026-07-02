//! Conformance run results: human summary plus machine-readable JSON and JUnit XML, so CI and
//! AI agents can consume the outcome and self-correct against it.

use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    Skip,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Fail => "fail",
            Status::Skip => "skip",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Check {
    /// Stable check id, e.g. `L2/float32-be-bigword-42_5` or `B6-write-verb`.
    pub id: String,
    pub name: String,
    pub status: Status,
    /// Failure reason or informational detail.
    pub detail: Option<String>,
}

#[derive(Debug, Default)]
pub struct Layer {
    pub name: String,
    pub checks: Vec<Check>,
}

impl Layer {
    pub fn new(name: &str) -> Layer {
        Layer {
            name: name.to_string(),
            checks: Vec::new(),
        }
    }

    pub fn pass(&mut self, id: &str, name: &str, detail: Option<String>) {
        self.push(id, name, Status::Pass, detail);
    }

    pub fn fail(&mut self, id: &str, name: &str, detail: String) {
        self.push(id, name, Status::Fail, Some(detail));
    }

    pub fn skip(&mut self, id: &str, name: &str, detail: String) {
        self.push(id, name, Status::Skip, Some(detail));
    }

    pub fn check(&mut self, id: &str, name: &str, result: Result<Option<String>, String>) {
        match result {
            Ok(detail) => self.pass(id, name, detail),
            Err(reason) => self.fail(id, name, reason),
        }
    }

    fn push(&mut self, id: &str, name: &str, status: Status, detail: Option<String>) {
        self.checks.push(Check {
            id: id.to_string(),
            name: name.to_string(),
            status,
            detail,
        });
    }
}

#[derive(Debug, Default)]
pub struct Report {
    pub protocol: String,
    pub layers: Vec<Layer>,
}

impl Report {
    pub fn new(protocol: &str) -> Report {
        Report {
            protocol: protocol.to_string(),
            layers: Vec::new(),
        }
    }

    pub fn failures(&self) -> usize {
        self.count(Status::Fail)
    }

    fn count(&self, status: Status) -> usize {
        self.layers
            .iter()
            .flat_map(|l| &l.checks)
            .filter(|c| c.status == status)
            .count()
    }

    /// True when every executed check passed (skips do not fail a run).
    pub fn conformant(&self) -> bool {
        self.failures() == 0
    }

    /// Human summary printed to stdout.
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        for layer in &self.layers {
            out.push_str(&format!("\n{}\n", layer.name));
            for c in &layer.checks {
                let mark = match c.status {
                    Status::Pass => "PASS",
                    Status::Fail => "FAIL",
                    Status::Skip => "skip",
                };
                out.push_str(&format!("  [{mark}] {} — {}\n", c.id, c.name));
                if c.status != Status::Pass {
                    if let Some(d) = &c.detail {
                        for line in d.lines() {
                            out.push_str(&format!("         {line}\n"));
                        }
                    }
                }
            }
        }
        out.push_str(&format!(
            "\n{}: {} passed, {} failed, {} skipped\n",
            if self.conformant() {
                "CONFORMANT"
            } else {
                "NOT CONFORMANT"
            },
            self.count(Status::Pass),
            self.count(Status::Fail),
            self.count(Status::Skip),
        ));
        out
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "protocol": self.protocol,
            "conformant": self.conformant(),
            "summary": {
                "passed": self.count(Status::Pass),
                "failed": self.count(Status::Fail),
                "skipped": self.count(Status::Skip),
            },
            "layers": self.layers.iter().map(|l| serde_json::json!({
                "name": l.name,
                "checks": l.checks.iter().map(|c| serde_json::json!({
                    "id": c.id,
                    "name": c.name,
                    "status": c.status.as_str(),
                    "detail": c.detail,
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        })
    }

    /// JUnit XML: one `<testsuite>` per layer, one `<testcase>` per check.
    pub fn to_junit(&self) -> String {
        let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites>\n");
        for layer in &self.layers {
            let failures = layer.checks.iter().filter(|c| c.status == Status::Fail).count();
            let skipped = layer.checks.iter().filter(|c| c.status == Status::Skip).count();
            xml.push_str(&format!(
                "  <testsuite name=\"{}\" tests=\"{}\" failures=\"{}\" errors=\"0\" skipped=\"{}\">\n",
                escape(&layer.name),
                layer.checks.len(),
                failures,
                skipped
            ));
            for c in &layer.checks {
                xml.push_str(&format!(
                    "    <testcase classname=\"{}\" name=\"{}\"",
                    escape(&layer.name),
                    escape(&format!("{}: {}", c.id, c.name))
                ));
                match c.status {
                    Status::Pass => xml.push_str("/>\n"),
                    Status::Fail => {
                        xml.push_str(&format!(
                            ">\n      <failure message=\"{}\"/>\n    </testcase>\n",
                            escape(c.detail.as_deref().unwrap_or("failed"))
                        ));
                    }
                    Status::Skip => {
                        xml.push_str(&format!(
                            ">\n      <skipped message=\"{}\"/>\n    </testcase>\n",
                            escape(c.detail.as_deref().unwrap_or("skipped"))
                        ));
                    }
                }
            }
            xml.push_str("  </testsuite>\n");
        }
        xml.push_str("</testsuites>\n");
        xml
    }

    pub fn write_json(&self, path: &Path) -> Result<(), String> {
        write_file(path, &serde_json::to_string_pretty(&self.to_json()).unwrap())
    }

    pub fn write_junit(&self, path: &Path) -> Result<(), String> {
        write_file(path, &self.to_junit())
    }
}

fn write_file(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
    }
    let mut f = std::fs::File::create(path).map_err(|e| format!("create {}: {e}", path.display()))?;
    f.write_all(contents.as_bytes())
        .map_err(|e| format!("write {}: {e}", path.display()))
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_report() -> Report {
        let mut report = Report::new("modbus");
        let mut layer = Layer::new("Layer 2 — decode conformance");
        layer.pass("L2/ok", "a passing vector", None);
        layer.fail("L2/bad", "a failing <vector>", "expected \"1\", got 2".into());
        layer.skip("L2/skipped", "not advertised", "datatype not in manifest".into());
        report.layers.push(layer);
        report
    }

    #[test]
    fn conformance_requires_zero_failures() {
        let report = sample_report();
        assert!(!report.conformant());
        assert_eq!(report.failures(), 1);
    }

    #[test]
    fn junit_is_escaped_and_counts_match() {
        let xml = sample_report().to_junit();
        assert!(xml.contains("tests=\"3\" failures=\"1\" errors=\"0\" skipped=\"1\""));
        assert!(xml.contains("a failing &lt;vector&gt;"));
        assert!(xml.contains("expected &quot;1&quot;, got 2"));
    }

    #[test]
    fn json_summary_counts() {
        let json = sample_report().to_json();
        assert_eq!(json["summary"]["passed"], 1);
        assert_eq!(json["summary"]["failed"], 1);
        assert_eq!(json["summary"]["skipped"], 1);
        assert_eq!(json["conformant"], false);
    }
}

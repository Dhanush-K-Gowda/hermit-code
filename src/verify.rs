//! Detect and run the project's check command ("does the code still work?").

use std::path::Path;

/// Pick a build/test command based on files in the workspace root.
pub fn detect(root: &Path) -> Option<String> {
    let has = |f: &str| root.join(f).exists();
    if has("Cargo.toml") {
        return Some("cargo check --all-targets --quiet && cargo test --quiet".into());
    }
    if has("go.mod") {
        return Some("go build ./... && go test ./...".into());
    }
    if has("package.json") {
        let pkg = std::fs::read_to_string(root.join("package.json")).unwrap_or_default();
        let v: serde_json::Value = serde_json::from_str(&pkg).unwrap_or_default();
        let test = v["scripts"]["test"].as_str().unwrap_or("");
        let has_test = !test.is_empty() && !test.contains("no test specified");
        let has_build = v["scripts"]["build"].is_string();
        let mut parts = Vec::new();
        if has("tsconfig.json") && !has_build {
            parts.push("npx --no-install tsc --noEmit");
        }
        if has_build {
            parts.push("npm run build --silent");
        }
        if has_test {
            parts.push("npm test --silent");
        }
        return (!parts.is_empty()).then(|| parts.join(" && "));
    }
    if has("pyproject.toml") || has("pytest.ini") || has("setup.py") || has("tests") {
        return Some("python3 -m pytest -q -x".into());
    }
    if has("Makefile") {
        let mk = std::fs::read_to_string(root.join("Makefile")).unwrap_or_default();
        if mk.lines().any(|l| l.starts_with("test:")) {
            return Some("make test".into());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_by_marker_files() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(detect(d.path()), None);
        std::fs::write(d.path().join("package.json"), r#"{"scripts":{"test":"echo \"Error: no test specified\""}}"#).unwrap();
        assert_eq!(detect(d.path()), None);
        std::fs::write(d.path().join("package.json"), r#"{"scripts":{"test":"vitest run","build":"tsc"}}"#).unwrap();
        assert_eq!(detect(d.path()).unwrap(), "npm run build --silent && npm test --silent");
        std::fs::write(d.path().join("Cargo.toml"), "").unwrap();
        assert!(detect(d.path()).unwrap().starts_with("cargo check"));
    }
}

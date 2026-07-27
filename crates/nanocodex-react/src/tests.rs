use std::fs;

use nanocodex_tools::{
    DEFAULT_TOOL_OUTPUT_TOKENS, Tool, ToolContext, ToolInput, ToolOutputBody, ToolOutputContent,
    ToolRuntime, Tools,
};
use serde_json::value::to_raw_value;

use crate::{
    ReactAnalysisFailureKind, ReactDoctor, ReactDoctorError, ReactDoctorTool, ReactReport,
    ReactRule,
};

const FINDINGS: &str = r#"
import React, { useEffect } from "react";

export function Findings({ items, theme, html }) {
  useEffect(async () => {
    await refresh();
  }, []);

  return (
    <Theme.Provider value={{ theme }}>
      <button>Save</button>
      <img src="/logo.png" />
      <div dangerouslySetInnerHTML={{ __html: html }} />
      {items.map((item, index) => <span key={index}>{item.name}</span>)}
    </Theme.Provider>
  );
}
"#;

const CLEAN: &str = r#"
import { useEffect, useMemo } from "react";

export function Clean({ items, theme }) {
  useEffect(() => {
    async function refreshData() {
      await refresh();
    }
    void refreshData();
  }, []);
  const value = useMemo(() => ({ theme }), [theme]);
  return (
    <Theme.Provider value={value}>
      <button type="button">Save</button>
      <img src="/logo.png" alt="" />
      {items.map((item) => <span key={item.id}>{item.name}</span>)}
    </Theme.Provider>
  );
}
"#;

#[test]
fn reports_typed_high_signal_react_findings() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("Findings.tsx"), FINDINGS).unwrap();
    let doctor = ReactDoctor::builder(directory.path()).build().unwrap();

    let report = doctor.analyze().unwrap();
    let rules = report
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.rule)
        .collect::<Vec<_>>();

    assert_eq!(report.analyzed_files, 1);
    assert_eq!(
        rules,
        vec![
            ReactRule::AsyncEffectCallback,
            ReactRule::UnstableContextValue,
            ReactRule::ButtonMissingType,
            ReactRule::ImageMissingAlt,
            ReactRule::UnsafeRawHtml,
            ReactRule::ArrayIndexKey,
        ]
    );
    assert!(
        report
            .diagnostics
            .iter()
            .all(|diagnostic| diagnostic.span.line > 0 && diagnostic.span.column > 0)
    );
    assert!(!report.diagnostics_truncated);
    assert!(report.failures.is_empty());
}

#[test]
fn accepts_the_corresponding_stable_patterns() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("Clean.tsx"), CLEAN).unwrap();
    let doctor = ReactDoctor::builder(directory.path()).build().unwrap();

    let report = doctor.analyze().unwrap();

    assert_eq!(report.analyzed_files, 1);
    assert!(report.diagnostics.is_empty());
    assert!(report.failures.is_empty());
}

#[test]
fn configured_root_is_a_hard_boundary() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("root");
    fs::create_dir(&root).unwrap();
    let doctor = ReactDoctor::builder(&root).build().unwrap();

    let error = doctor.analyze_path("..").unwrap_err();

    assert!(matches!(error, ReactDoctorError::OutsideRoot { .. }));
}

#[test]
fn file_limit_counts_every_candidate_even_when_parsing_fails() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("a.tsx"), "<").unwrap();
    fs::write(directory.path().join("b.tsx"), CLEAN).unwrap();
    let doctor = ReactDoctor::builder(directory.path())
        .max_files(1)
        .build()
        .unwrap();

    let error = doctor.analyze().unwrap_err();

    assert!(matches!(
        error,
        ReactDoctorError::FileLimit { maximum: 1, .. }
    ));
}

#[test]
fn file_size_limit_is_enforced_before_parsing() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("large.tsx"), "x".repeat(17)).unwrap();
    let doctor = ReactDoctor::builder(directory.path())
        .max_file_bytes(16)
        .build()
        .unwrap();

    let report = doctor.analyze().unwrap();

    assert_eq!(report.analyzed_files, 0);
    assert_eq!(report.failures.len(), 1);
    assert_eq!(report.failures[0].kind, ReactAnalysisFailureKind::TooLarge);
}

#[tokio::test]
async fn ordinary_tool_returns_the_same_typed_report() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("Findings.tsx"), FINDINGS).unwrap();
    let doctor = ReactDoctor::builder(directory.path()).build().unwrap();
    let tool = ReactDoctorTool::new(doctor);

    let execution = tool
        .execute(
            ToolInput::Function(to_raw_value(&serde_json::json!({})).unwrap()),
            context(),
        )
        .await
        .unwrap();
    let ToolOutputBody::Text(output) = execution.output else {
        panic!("expected text tool output");
    };
    let report: ReactReport = serde_json::from_str(&output).unwrap();

    assert_eq!(report.diagnostics.len(), 6);
}

#[tokio::test]
async fn tool_is_callable_and_composable_in_code_mode() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("Findings.tsx"), FINDINGS).unwrap();
    let doctor = ReactDoctor::builder(directory.path()).build().unwrap();
    let tools = Tools::builder()
        .without_defaults()
        .tool(ReactDoctorTool::new(doctor))
        .build()
        .unwrap();
    let runtime = ToolRuntime::new_with_tools(directory.path(), None, None, &tools);

    let execution = runtime
        .execute_code(
            r"
const report = await tools.react_doctor({});
const byRule = Object.fromEntries(
  report.diagnostics.map((diagnostic) => [diagnostic.rule, diagnostic.span.line]),
);
text({
  analyzedFiles: report.analyzedFiles,
  findingCount: report.diagnostics.length,
  byRule,
});
",
            context(),
        )
        .await;

    assert!(execution.success);
    assert_eq!(execution.nested_calls.len(), 1);
    assert_eq!(execution.nested_calls[0].name, "react_doctor");
    let ToolOutputBody::Content(content) = execution.output else {
        panic!("expected Code Mode content output");
    };
    let Some(ToolOutputContent::InputText { text }) = content.last() else {
        panic!("expected final Code Mode text output");
    };
    let summary: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(summary["analyzedFiles"], 1);
    assert_eq!(summary["findingCount"], 6);
    assert_eq!(summary["byRule"]["async_effect_callback"], 5);
}

fn context() -> ToolContext<'static> {
    ToolContext {
        model: "react-doctor-test",
        session_id: "react-doctor-test",
        call_id: "react-doctor-test",
        history: &[],
        output_token_budget: DEFAULT_TOOL_OUTPUT_TOKENS,
    }
}

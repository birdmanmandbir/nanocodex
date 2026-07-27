use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

use ignore::WalkBuilder;
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, CallExpression, Expression, JSXAttribute, JSXAttributeItem, JSXAttributeValue,
    JSXElementName, JSXOpeningElement,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType, Span};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const DEFAULT_MAX_FILES: usize = 20_000;
const DEFAULT_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const DEFAULT_MAX_DIAGNOSTICS: usize = 10_000;
const MAX_FAILURE_MESSAGE_BYTES: usize = 4 * 1024;

/// A source-level React rule implemented by Nanocodex.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ReactRule {
    ArrayIndexKey,
    AsyncEffectCallback,
    ButtonMissingType,
    ImageMissingAlt,
    UnstableContextValue,
    UnsafeRawHtml,
}

/// Diagnostic severity.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReactSeverity {
    Warning,
    Error,
}

/// Diagnostic category.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReactCategory {
    Accessibility,
    Correctness,
    Performance,
    Security,
}

/// Exact UTF-8 source range and one-indexed line/column coordinates.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReactSourceSpan {
    pub start_byte: u32,
    pub end_byte: u32,
    pub line: u32,
    pub column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

/// One actionable source diagnostic.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReactDiagnostic {
    pub path: PathBuf,
    pub rule: ReactRule,
    pub severity: ReactSeverity,
    pub category: ReactCategory,
    pub span: ReactSourceSpan,
    pub message: String,
    pub help: String,
}

/// A file failure that did not prevent the remaining project from being analyzed.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReactAnalysisFailure {
    pub path: PathBuf,
    pub kind: ReactAnalysisFailureKind,
    pub message: String,
}

/// Classification for a recoverable per-file analysis failure.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReactAnalysisFailureKind {
    Read,
    Syntax,
    TooLarge,
    Walk,
}

/// Complete bounded report for one source tree.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReactReport {
    pub root: PathBuf,
    pub analyzed_files: usize,
    pub diagnostics: Vec<ReactDiagnostic>,
    pub failures: Vec<ReactAnalysisFailure>,
    pub diagnostics_truncated: bool,
}

/// Reusable, immutable React analyzer.
#[derive(Clone, Debug)]
pub struct ReactDoctor {
    root: Arc<PathBuf>,
    max_files: usize,
    max_file_bytes: u64,
    max_diagnostics: usize,
}

impl ReactDoctor {
    /// Starts a bounded analyzer builder rooted at `root`.
    pub fn builder(root: impl Into<PathBuf>) -> ReactDoctorBuilder {
        ReactDoctorBuilder {
            root: root.into(),
            max_files: DEFAULT_MAX_FILES,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_diagnostics: DEFAULT_MAX_DIAGNOSTICS,
        }
    }

    /// Analyzes the complete configured root.
    ///
    /// # Errors
    ///
    /// Returns an error when the root can no longer be resolved or a configured
    /// hard file-count limit is exceeded.
    pub fn analyze(&self) -> Result<ReactReport, ReactDoctorError> {
        self.analyze_path(".")
    }

    /// Analyzes one file or subtree relative to the configured root.
    ///
    /// # Errors
    ///
    /// Returns an error when the target does not exist, escapes the configured
    /// root, or exceeds the configured hard file-count limit.
    pub fn analyze_path(&self, path: impl AsRef<Path>) -> Result<ReactReport, ReactDoctorError> {
        let target = canonicalize_target(&self.root, path.as_ref())?;
        let relative_target = target
            .strip_prefix(self.root.as_path())
            .unwrap_or(Path::new("."))
            .to_path_buf();
        let mut report = ReactReport {
            root: if relative_target.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                relative_target
            },
            analyzed_files: 0,
            diagnostics: Vec::new(),
            failures: Vec::new(),
            diagnostics_truncated: false,
        };

        let mut builder = WalkBuilder::new(&target);
        builder
            .standard_filters(true)
            .follow_links(false)
            .hidden(false);
        let mut source_files_seen = 0_usize;
        for entry in builder.build() {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    report.failures.push(ReactAnalysisFailure {
                        path: report.root.clone(),
                        kind: ReactAnalysisFailureKind::Walk,
                        message: bounded_message(error.to_string()),
                    });
                    continue;
                }
            };
            let Some(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_file() || !is_source_file(entry.path()) {
                continue;
            }
            if source_files_seen == self.max_files {
                return Err(ReactDoctorError::FileLimit {
                    target,
                    maximum: self.max_files,
                });
            }
            source_files_seen += 1;
            self.analyze_file(entry.path(), &mut report);
        }

        report.diagnostics.sort_by(|left, right| {
            (&left.path, left.span.start_byte, left.rule).cmp(&(
                &right.path,
                right.span.start_byte,
                right.rule,
            ))
        });
        report
            .failures
            .sort_by(|left, right| left.path.cmp(&right.path));
        Ok(report)
    }

    fn analyze_file(&self, path: &Path, report: &mut ReactReport) {
        let relative_path = path
            .strip_prefix(self.root.as_path())
            .unwrap_or(path)
            .to_path_buf();
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) => {
                report.failures.push(ReactAnalysisFailure {
                    path: relative_path,
                    kind: ReactAnalysisFailureKind::Read,
                    message: bounded_message(error.to_string()),
                });
                return;
            }
        };
        if metadata.len() > self.max_file_bytes {
            report.failures.push(ReactAnalysisFailure {
                path: relative_path,
                kind: ReactAnalysisFailureKind::TooLarge,
                message: format!(
                    "file is {} bytes; configured maximum is {} bytes",
                    metadata.len(),
                    self.max_file_bytes
                ),
            });
            return;
        }
        let mut source = String::new();
        let read = fs::File::open(path).and_then(|file| {
            file.take(self.max_file_bytes.saturating_add(1))
                .read_to_string(&mut source)
        });
        if let Err(error) = read {
            report.failures.push(ReactAnalysisFailure {
                path: relative_path,
                kind: ReactAnalysisFailureKind::Read,
                message: bounded_message(error.to_string()),
            });
            return;
        }
        if u64::try_from(source.len()).unwrap_or(u64::MAX) > self.max_file_bytes {
            report.failures.push(ReactAnalysisFailure {
                path: relative_path,
                kind: ReactAnalysisFailureKind::TooLarge,
                message: format!(
                    "file grew beyond the configured maximum of {} bytes while being read",
                    self.max_file_bytes
                ),
            });
            return;
        }
        let Ok(source_type) = SourceType::from_path(path) else {
            return;
        };
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, &source, source_type).parse();
        if !parsed.diagnostics.is_empty() {
            let message = parsed
                .diagnostics
                .into_iter()
                .map(|diagnostic| diagnostic.to_string())
                .collect::<Vec<_>>()
                .join("\n");
            report.failures.push(ReactAnalysisFailure {
                path: relative_path,
                kind: ReactAnalysisFailureKind::Syntax,
                message: bounded_message(message),
            });
            return;
        }

        report.analyzed_files += 1;
        let remaining = self
            .max_diagnostics
            .saturating_sub(report.diagnostics.len());
        if remaining == 0 {
            report.diagnostics_truncated = true;
            return;
        }
        let mut visitor = RuleVisitor::new(&relative_path, &source, remaining);
        visitor.visit_program(&parsed.program);
        report.diagnostics_truncated |= visitor.truncated;
        report.diagnostics.extend(visitor.diagnostics);
    }
}

/// Builder for [`ReactDoctor`] policy.
#[derive(Clone, Debug)]
pub struct ReactDoctorBuilder {
    root: PathBuf,
    max_files: usize,
    max_file_bytes: u64,
    max_diagnostics: usize,
}

impl ReactDoctorBuilder {
    /// Sets the hard source-file limit.
    #[must_use]
    pub const fn max_files(mut self, maximum: usize) -> Self {
        self.max_files = maximum;
        self
    }

    /// Sets the maximum accepted size for one source file.
    #[must_use]
    pub const fn max_file_bytes(mut self, maximum: u64) -> Self {
        self.max_file_bytes = maximum;
        self
    }

    /// Sets the maximum diagnostics retained in one report.
    #[must_use]
    pub const fn max_diagnostics(mut self, maximum: usize) -> Self {
        self.max_diagnostics = maximum;
        self
    }

    /// Resolves the root and builds an immutable reusable analyzer.
    ///
    /// # Errors
    ///
    /// Returns an error when a limit is zero, the root cannot be resolved, or
    /// the root is not a directory.
    pub fn build(self) -> Result<ReactDoctor, ReactDoctorBuildError> {
        if self.max_files == 0 || self.max_file_bytes == 0 || self.max_diagnostics == 0 {
            return Err(ReactDoctorBuildError::ZeroLimit);
        }
        let root =
            fs::canonicalize(&self.root).map_err(|source| ReactDoctorBuildError::ResolveRoot {
                root: self.root,
                source,
            })?;
        if !root.is_dir() {
            return Err(ReactDoctorBuildError::RootNotDirectory(root));
        }
        Ok(ReactDoctor {
            root: Arc::new(root),
            max_files: self.max_files,
            max_file_bytes: self.max_file_bytes,
            max_diagnostics: self.max_diagnostics,
        })
    }
}

/// Error building a React analyzer.
#[derive(Debug, thiserror::Error)]
pub enum ReactDoctorBuildError {
    #[error("React analyzer limits must be greater than zero")]
    ZeroLimit,

    #[error("failed to resolve React analyzer root {}", root.display())]
    ResolveRoot {
        root: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("React analyzer root is not a directory: {}", .0.display())]
    RootNotDirectory(PathBuf),
}

/// Error running a React analysis.
#[derive(Debug, thiserror::Error)]
pub enum ReactDoctorError {
    #[error("failed to resolve React analysis target {}", path.display())]
    ResolveTarget {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "React analysis target {} escapes configured root {}",
        target.display(),
        root.display()
    )]
    OutsideRoot { root: PathBuf, target: PathBuf },

    #[error(
        "React analysis target {} exceeded the configured limit of {maximum} source files",
        target.display()
    )]
    FileLimit { target: PathBuf, maximum: usize },
}

fn canonicalize_target(root: &Path, path: &Path) -> Result<PathBuf, ReactDoctorError> {
    let unresolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let target =
        fs::canonicalize(&unresolved).map_err(|source| ReactDoctorError::ResolveTarget {
            path: unresolved,
            source,
        })?;
    if !target.starts_with(root) {
        return Err(ReactDoctorError::OutsideRoot {
            root: root.to_path_buf(),
            target,
        });
    }
    Ok(target)
}

fn is_source_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "mts" | "cts"
            )
        })
}

fn bounded_message(message: String) -> String {
    if message.len() <= MAX_FAILURE_MESSAGE_BYTES {
        return message;
    }
    let mut end = MAX_FAILURE_MESSAGE_BYTES;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &message[..end])
}

struct RuleVisitor<'a> {
    path: &'a Path,
    source: &'a str,
    line_starts: Vec<u32>,
    maximum: usize,
    diagnostics: Vec<ReactDiagnostic>,
    truncated: bool,
}

impl<'a> RuleVisitor<'a> {
    fn new(path: &'a Path, source: &'a str, maximum: usize) -> Self {
        let mut line_starts = vec![0];
        line_starts.extend(source.bytes().enumerate().filter_map(|(index, byte)| {
            (byte == b'\n')
                .then(|| u32::try_from(index + 1).ok())
                .flatten()
        }));
        Self {
            path,
            source,
            line_starts,
            maximum,
            diagnostics: Vec::new(),
            truncated: false,
        }
    }

    fn report(&mut self, rule: ReactRule, span: Span) {
        if self.diagnostics.len() == self.maximum {
            self.truncated = true;
            return;
        }
        let (severity, category, message, help) = rule_metadata(rule);
        self.diagnostics.push(ReactDiagnostic {
            path: self.path.to_path_buf(),
            rule,
            severity,
            category,
            span: source_span(span, &self.line_starts, self.source.len()),
            message: message.to_owned(),
            help: help.to_owned(),
        });
    }
}

impl<'a> Visit<'a> for RuleVisitor<'_> {
    fn visit_jsx_opening_element(&mut self, element: &JSXOpeningElement<'a>) {
        if let Some(name) = native_element_name(&element.name) {
            match name {
                "button" if !has_attribute(element, "type") => {
                    self.report(ReactRule::ButtonMissingType, element.name.span());
                }
                "img" if !has_attribute(element, "alt") => {
                    self.report(ReactRule::ImageMissingAlt, element.name.span());
                }
                _ => {}
            }
        }
        if let Some(attribute) = find_attribute(element, "dangerouslySetInnerHTML") {
            self.report(ReactRule::UnsafeRawHtml, attribute.span);
        }
        if element.name.to_string().ends_with(".Provider")
            && let Some(attribute) = find_attribute(element, "value")
            && attribute_value_is_fresh_identity(attribute)
        {
            self.report(ReactRule::UnstableContextValue, attribute.span);
        }
        walk::walk_jsx_opening_element(self, element);
    }

    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        if is_effect_call(&call.callee)
            && let Some(callback) = call.arguments.first().and_then(Argument::as_expression)
            && expression_is_async_function(callback)
        {
            self.report(ReactRule::AsyncEffectCallback, callback.span());
        }
        if is_map_call(&call.callee)
            && let Some((callback, index_name)) = map_callback_and_index(call)
        {
            let mut visitor = ArrayIndexKeyVisitor {
                index_name,
                spans: Vec::new(),
            };
            visitor.visit_expression(callback);
            for span in visitor.spans {
                self.report(ReactRule::ArrayIndexKey, span);
            }
        }
        walk::walk_call_expression(self, call);
    }
}

struct ArrayIndexKeyVisitor<'a> {
    index_name: &'a str,
    spans: Vec<Span>,
}

impl<'a> Visit<'a> for ArrayIndexKeyVisitor<'a> {
    fn visit_jsx_opening_element(&mut self, element: &JSXOpeningElement<'a>) {
        if let Some(attribute) = find_attribute(element, "key")
            && attribute_expression(attribute)
                .is_some_and(|expression| expression.is_specific_id(self.index_name))
        {
            self.spans.push(attribute.span);
        }
        walk::walk_jsx_opening_element(self, element);
    }
}

fn native_element_name<'a>(name: &'a JSXElementName<'a>) -> Option<&'a str> {
    match name {
        JSXElementName::Identifier(identifier)
            if identifier
                .name
                .as_str()
                .starts_with(|character: char| character.is_ascii_lowercase()) =>
        {
            Some(identifier.name.as_str())
        }
        _ => None,
    }
}

fn find_attribute<'a>(
    element: &'a JSXOpeningElement<'a>,
    name: &str,
) -> Option<&'a JSXAttribute<'a>> {
    element
        .attributes
        .iter()
        .filter_map(JSXAttributeItem::as_attribute)
        .find(|attribute| attribute.is_identifier(name))
}

fn has_attribute(element: &JSXOpeningElement<'_>, name: &str) -> bool {
    find_attribute(element, name).is_some()
        || element
            .attributes
            .iter()
            .any(|attribute| matches!(attribute, JSXAttributeItem::SpreadAttribute(_)))
}

fn attribute_expression<'a>(attribute: &'a JSXAttribute<'a>) -> Option<&'a Expression<'a>> {
    let JSXAttributeValue::ExpressionContainer(container) = attribute.value.as_ref()? else {
        return None;
    };
    container.expression.as_expression()
}

fn attribute_value_is_fresh_identity(attribute: &JSXAttribute<'_>) -> bool {
    attribute_expression(attribute).is_some_and(|expression| {
        matches!(
            expression.without_parentheses(),
            Expression::ObjectExpression(_)
                | Expression::ArrayExpression(_)
                | Expression::ArrowFunctionExpression(_)
                | Expression::FunctionExpression(_)
        )
    })
}

fn is_effect_call(callee: &Expression<'_>) -> bool {
    const EFFECTS: [&str; 3] = ["useEffect", "useInsertionEffect", "useLayoutEffect"];
    callee
        .get_identifier_reference()
        .is_some_and(|identifier| EFFECTS.contains(&identifier.name.as_str()))
        || callee.get_member_expr().is_some_and(|member| {
            member.object().is_specific_id("React")
                && member
                    .static_property_name()
                    .is_some_and(|name| EFFECTS.contains(&name))
        })
}

fn expression_is_async_function(expression: &Expression<'_>) -> bool {
    match expression.without_parentheses() {
        Expression::ArrowFunctionExpression(function) => function.r#async,
        Expression::FunctionExpression(function) => function.r#async,
        _ => false,
    }
}

fn is_map_call(callee: &Expression<'_>) -> bool {
    callee
        .get_member_expr()
        .and_then(oxc_ast::ast::MemberExpression::static_property_name)
        == Some("map")
}

fn map_callback_and_index<'a>(
    call: &'a CallExpression<'a>,
) -> Option<(&'a Expression<'a>, &'a str)> {
    let callback = call.arguments.first()?.as_expression()?;
    let params = match callback.without_parentheses() {
        Expression::ArrowFunctionExpression(function) => &function.params,
        Expression::FunctionExpression(function) => &function.params,
        _ => return None,
    };
    let index_name = params.items.get(1)?.pattern.get_identifier_name()?;
    Some((callback, index_name.as_str()))
}

fn source_span(span: Span, line_starts: &[u32], source_length: usize) -> ReactSourceSpan {
    let source_length = u32::try_from(source_length).unwrap_or(u32::MAX);
    let start = span.start.min(source_length);
    let end = span.end.min(source_length);
    let (line, column) = line_column(start, line_starts);
    let (end_line, end_column) = line_column(end, line_starts);
    ReactSourceSpan {
        start_byte: start,
        end_byte: end,
        line,
        column,
        end_line,
        end_column,
    }
}

fn line_column(offset: u32, line_starts: &[u32]) -> (u32, u32) {
    let line_index = line_starts.partition_point(|line_start| *line_start <= offset) - 1;
    (
        u32::try_from(line_index).unwrap_or(u32::MAX - 1) + 1,
        offset - line_starts[line_index] + 1,
    )
}

fn rule_metadata(rule: ReactRule) -> (ReactSeverity, ReactCategory, &'static str, &'static str) {
    match rule {
        ReactRule::ArrayIndexKey => (
            ReactSeverity::Warning,
            ReactCategory::Correctness,
            "An array position is used as a React key.",
            "Use an identifier that remains attached to the item when the list is inserted, removed, or reordered.",
        ),
        ReactRule::AsyncEffectCallback => (
            ReactSeverity::Error,
            ReactCategory::Correctness,
            "An effect callback is async and therefore returns a Promise.",
            "Start async work inside the effect and return only a synchronous cleanup function.",
        ),
        ReactRule::ButtonMissingType => (
            ReactSeverity::Warning,
            ReactCategory::Correctness,
            "A native button has no explicit type and defaults to form submission.",
            "Set type=\"button\", type=\"submit\", or type=\"reset\" deliberately.",
        ),
        ReactRule::ImageMissingAlt => (
            ReactSeverity::Error,
            ReactCategory::Accessibility,
            "A native image has no alt text.",
            "Add meaningful alt text, or alt=\"\" when the image is purely decorative.",
        ),
        ReactRule::UnstableContextValue => (
            ReactSeverity::Warning,
            ReactCategory::Performance,
            "A Context provider receives a new object, array, or function on every render.",
            "Move the value outside render or memoize it when its dependencies are unchanged.",
        ),
        ReactRule::UnsafeRawHtml => (
            ReactSeverity::Warning,
            ReactCategory::Security,
            "Raw HTML enters the rendered document through dangerouslySetInnerHTML.",
            "Prefer React children; otherwise keep sanitization explicit and verify the complete data path.",
        ),
    }
}

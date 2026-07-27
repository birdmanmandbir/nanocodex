//! Fast, bounded React source analysis implemented in Rust.
//!
//! [`ReactDoctor`] is the library API. [`ReactDoctorTool`] exposes the same
//! typed report as an ordinary Nanocodex Code Mode tool.

#![deny(missing_docs, rustdoc::broken_intra_doc_links)]

mod analyzer;
#[cfg(test)]
mod tests;
mod tool;

pub use analyzer::{
    ReactAnalysisFailure, ReactAnalysisFailureKind, ReactCategory, ReactDiagnostic, ReactDoctor,
    ReactDoctorBuildError, ReactDoctorBuilder, ReactDoctorError, ReactReport, ReactRule,
    ReactSeverity, ReactSourceSpan,
};
pub use tool::ReactDoctorTool;

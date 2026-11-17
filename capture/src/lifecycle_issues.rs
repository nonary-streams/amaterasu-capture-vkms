//! Stream failures this capture provider reports first-hand.
//!
//! The video service opens a capture stream with an `operation_id` it derived
//! from its own stream operation and bound to the launch. A refused open, the
//! first failure of a stream's data path and the fallbacks this provider
//! chooses for a stream are reported under that operation with this module
//! as the publisher.
use crate::{frames::StreamFault, snapshot::LayoutFallback, threading::CapturePriority};
use amaterasu_module_sdk::{
    stream_lifecycle::{LifecycleLayer, StreamIssue, StreamIssueOutbox, StreamReport},
    stream_warnings::WarningValue,
};

/// Failure codes that mean the display output went away or changed under
/// the capture rather than the capture itself failing.
const DISPLAY_CODES: &[&str] = &[
    "output_not_found",
    "output_disconnected",
    "output_inactive",
    "output_unavailable",
    "output_replaced",
];

/// The capture thread's priority, where it fell short of the critical level.
const PRIORITY_PHASE: &str = "vkms.thread_priority";

/// A capture failure in `phase` with this provider's `code`.
#[track_caller]
pub(crate) fn capture_failure(operation: &str, phase: &str, code: &str) -> StreamIssue {
    if DISPLAY_CODES.contains(&code) {
        output_lost(operation, phase, code)
    } else {
        StreamIssue::critical(operation, LifecycleLayer::Capture, phase, code)
    }
}

/// The fault that ended a stream's delivery. A lost output is the display's
/// failure whatever code the re-resolve reported.
#[track_caller]
pub(crate) fn stream_fault(operation: &str, fault: StreamFault) -> StreamIssue {
    let phase = match fault {
        StreamFault::HelperFailed | StreamFault::PermissionDenied => "vkms.export",
        StreamFault::GpuSnapshotFailed => "vkms.snapshot",
        StreamFault::OutputLost(_) => "vkms.resolve",
    };
    match fault {
        StreamFault::OutputLost(code) => output_lost(operation, phase, code),
        fault => StreamIssue::critical(operation, LifecycleLayer::Capture, phase, fault.code()),
    }
}

/// The capture thread runs below the critical level the provider asked for.
#[track_caller]
pub(crate) fn priority_fallback(operation: &str, priority: CapturePriority) -> Option<StreamIssue> {
    let (code, fallback, effective) = match priority {
        CapturePriority::Critical => return None,
        CapturePriority::Limited { nice } => {
            ("capture_priority_limited", "rlimit_nice", Some(nice))
        }
        CapturePriority::Unchanged => ("capture_priority_unavailable", "default_priority", None),
    };
    let issue = StreamIssue::demoted(
        operation,
        LifecycleLayer::Capture,
        PRIORITY_PHASE,
        code,
        fallback,
    )
    .with_values(
        Some(WarningValue::Integer(i64::from(
            crate::threading::CRITICAL_NICE,
        ))),
        effective.map(|nice| WarningValue::Integer(i64::from(nice))),
    );
    Some(issue)
}

#[track_caller]
pub(crate) fn snapshot_layout_fallback(operation: &str, layout: LayoutFallback) -> StreamIssue {
    let fallback = match layout {
        LayoutFallback::Scanout => "renderable_scanout",
        LayoutFallback::RenderOnly => "render_only",
    };
    StreamIssue::demoted(
        operation,
        LifecycleLayer::Capture,
        "vkms.snapshot",
        "snapshot_layout_fallback",
        fallback,
    )
    .because("renderable_linear_layout_unavailable")
    .with_values(
        Some(WarningValue::Label("renderable_linear".into())),
        Some(WarningValue::Label(fallback.into())),
    )
}

#[track_caller]
fn output_lost(operation: &str, phase: &str, code: &str) -> StreamIssue {
    StreamIssue::critical(
        operation,
        LifecycleLayer::Capture,
        phase,
        "capture_output_lost",
    )
    .caused_by(LifecycleLayer::Display)
    .because(code)
}

/// Move queued reports between outboxes, keeping their order.
pub(crate) fn transfer(from: &mut StreamIssueOutbox, to: &mut StreamIssueOutbox) {
    for report in from.drain() {
        match report {
            StreamReport::Issue(issue) => to.push(issue),
            StreamReport::Binding(binding) => to.push_binding(binding),
        }
    }
}

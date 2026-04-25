//! Indexing-race retry for `Client::rename`.
//!
//! rust-analyzer's `textDocument/rename` is *racy on a cold workspace*.
//! Until indexing completes, the server can signal "not ready yet" in
//! at least four distinct LSP shapes — each one observed in CI as PR-B
//! tightened the e2e test:
//!
//!   1. `LspError(-32602)` with `"No references found at position"` —
//!      common on warm hosts.
//!   2. `Ok(WorkspaceEdit { ... })` whose [`WorkspaceEdit::flatten`] is
//!      empty — the `result: null` case (LSP-spec valid for "I cannot
//!      rename here"), normalised by `Client::rename`.
//!   3. `LspError(-32801)` `ContentModified` — RA invalidated the
//!      in-flight request because its internal state changed.
//!   4. `LspError(-32802)` `RequestCancelled` — same family.
//!
//! There is no standard LSP notification we can wait on:
//! `experimental/serverStatusNotification` would work but requires
//! declaring an experimental capability that ties us to RA's
//! particular flavour of the protocol. Polling with a bounded retry
//! is the pragmatic alternative — the only one that survives across
//! RA versions and across hosts of varying warmth.
//!
//! This module owns the retry policy. `Client::rename` stays a
//! single-shot transport call; production callers
//! (`aa-patch::typed_rename`'s `OneShotResolver`, `aa-ra-client`'s
//! `Session::rename`) wrap their first call to RA with
//! [`retry_rename_until_indexed`] so a cold workspace never causes a
//! spurious "rename failed" diagnostic. The `tracing` crate is not on
//! `aa-ra-client`'s dep list, so we surface progress to stderr via a
//! `Vec<RetryAttempt>` returned alongside the result; callers that
//! want logging plumb the field through to their own logger.

use std::time::{Duration, Instant};

use crate::client::{Client, ClientError, RenameRequest};
use crate::types::WorkspaceEdit;

/// Result of a single rename attempt — opaque to the caller; only
/// the policy (`classify_indexing_signal`) cares about the variants.
enum IndexingSignal {
    /// The rename returned a non-empty edit set. We're done.
    Ready,
    /// A retryable signal that RA hasn't finished indexing.
    /// Carries a short tag for the timeout diagnostic.
    NotReady(&'static str),
    /// Anything else — pass through to the caller without retry.
    HardFailure,
}

/// One observation made by the retry loop. Returned alongside the
/// final result so a caller that wants progress logs can plumb them
/// through their own observability layer (this crate stays
/// dep-light and doesn't pull in `tracing`).
#[derive(Debug, Clone)]
pub struct RetryAttempt {
    pub attempt: u32,
    /// Short tag identifying which "not ready" signal we saw. Empty
    /// on the final successful attempt.
    pub reason: &'static str,
}

/// Outcome of a [`retry_rename_until_indexed`] call.
#[derive(Debug)]
pub struct RetryOutcome {
    pub edit: Result<WorkspaceEdit, ClientError>,
    pub attempts: Vec<RetryAttempt>,
}

/// Default poll interval between rename attempts. Picked to be
/// short enough that a fast warm host doesn't see noticeable
/// latency, long enough that a slow cold host doesn't drown
/// rust-analyzer in requests it would only invalidate again.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Generic retry over any closure that performs one rename attempt
/// against `Client`. Used by both [`retry_rename_until_indexed`]
/// (which sends `didOpen` + `rename`) and [`retry_rename_at_until_indexed`]
/// (which assumes the file is already in-sync via `Session::sync`).
/// Splitting the attempt closure out keeps the retry policy in one
/// place while letting callers choose their LSP request shape.
pub fn retry_with<F>(
    client: &mut Client,
    deadline: Instant,
    poll_interval: Duration,
    mut do_one: F,
) -> RetryOutcome
where
    F: FnMut(&mut Client) -> Result<WorkspaceEdit, ClientError>,
{
    let mut attempts: Vec<RetryAttempt> = Vec::new();
    let mut count: u32 = 0;
    loop {
        count += 1;
        let result = do_one(client);
        let signal = classify_indexing_signal(&result);
        match signal {
            IndexingSignal::Ready => {
                attempts.push(RetryAttempt {
                    attempt: count,
                    reason: "",
                });
                return RetryOutcome {
                    edit: result,
                    attempts,
                };
            }
            IndexingSignal::NotReady(reason) => {
                attempts.push(RetryAttempt {
                    attempt: count,
                    reason,
                });
                if Instant::now() >= deadline {
                    // Surface as `Timeout` so callers see "RA never
                    // finished indexing in your budget" rather than
                    // the last underlying LSP payload, which is
                    // meaningless to a user. The Duration carried
                    // here is symbolic — `ClientError::Timeout`'s
                    // prod display path already conveys "your
                    // configured budget elapsed".
                    return RetryOutcome {
                        edit: Err(ClientError::Timeout(Duration::from_millis(0))),
                        attempts,
                    };
                }
                std::thread::sleep(poll_interval);
            }
            IndexingSignal::HardFailure => {
                attempts.push(RetryAttempt {
                    attempt: count,
                    reason: "hard-failure",
                });
                return RetryOutcome {
                    edit: result,
                    attempts,
                };
            }
        }
    }
}

/// Retry [`Client::rename`] until rust-analyzer has finished indexing
/// — or `deadline` is reached. The full contract is in the module
/// docstring. Use this from callers that own a fresh [`Client`]
/// (e.g. `aa-patch::typed_rename::resolve` per-call).
pub fn retry_rename_until_indexed(
    client: &mut Client,
    file: &std::path::Path,
    line: u32,
    character: u32,
    new_name: &str,
    deadline: Instant,
    poll_interval: Duration,
) -> RetryOutcome {
    retry_with(client, deadline, poll_interval, |c| {
        c.rename(RenameRequest {
            file,
            line,
            character,
            new_name,
        })
    })
}

/// Retry [`Client::rename_at`] until rust-analyzer has finished
/// indexing — same retry posture as [`retry_rename_until_indexed`]
/// but skips the `didOpen` round-trip. Use this from callers that
/// own a long-lived session and have already synced the document
/// (e.g. `aa-ra-client::Session::sync_and_rename`).
pub fn retry_rename_at_until_indexed(
    client: &mut Client,
    file: &std::path::Path,
    line: u32,
    character: u32,
    new_name: &str,
    deadline: Instant,
    poll_interval: Duration,
) -> RetryOutcome {
    retry_with(client, deadline, poll_interval, |c| {
        c.rename_at(file, line, character, new_name)
    })
}

/// LSP error codes RA emits while still warming up. Each of these
/// is documented in the LSP spec as "the server should retry"
/// territory; treating them as terminal would race the indexing
/// every time a cold workspace meets a slow host.
fn classify_indexing_signal(result: &Result<WorkspaceEdit, ClientError>) -> IndexingSignal {
    match result {
        Ok(edit) => {
            if edit.flatten().is_empty() {
                IndexingSignal::NotReady("empty WorkspaceEdit (likely result: null)")
            } else {
                IndexingSignal::Ready
            }
        }
        Err(ClientError::LspError(payload)) => {
            // Read the payload structurally rather than substring-
            // matching the human-readable text, which would drift
            // across RA versions. Any code we don't recognise falls
            // through to HardFailure so a genuine bug doesn't hide
            // under the retry timeout.
            let parsed: serde_json::Value =
                serde_json::from_str(payload).unwrap_or(serde_json::Value::Null);
            let code = parsed.get("code").and_then(|v| v.as_i64());
            let message = parsed.get("message").and_then(|v| v.as_str()).unwrap_or("");
            match code {
                Some(-32602) if message.contains("No references found") => {
                    IndexingSignal::NotReady("-32602 No references found")
                }
                Some(-32801) => IndexingSignal::NotReady("-32801 ContentModified"),
                Some(-32802) => IndexingSignal::NotReady("-32802 RequestCancelled"),
                _ => IndexingSignal::HardFailure,
            }
        }
        Err(_) => IndexingSignal::HardFailure,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_empty() -> Result<WorkspaceEdit, ClientError> {
        Ok(WorkspaceEdit::default())
    }

    fn lsp_err(code: i64, message: &str) -> Result<WorkspaceEdit, ClientError> {
        Err(ClientError::LspError(format!(
            r#"{{"code":{code},"message":"{message}"}}"#
        )))
    }

    #[test]
    fn classifies_no_references_as_not_ready() {
        match classify_indexing_signal(&lsp_err(-32602, "No references found at position")) {
            IndexingSignal::NotReady(_) => {}
            _ => panic!("expected NotReady"),
        }
    }

    #[test]
    fn classifies_content_modified_as_not_ready() {
        match classify_indexing_signal(&lsp_err(-32801, "content modified")) {
            IndexingSignal::NotReady(_) => {}
            _ => panic!("expected NotReady"),
        }
    }

    #[test]
    fn classifies_request_cancelled_as_not_ready() {
        match classify_indexing_signal(&lsp_err(-32802, "request cancelled")) {
            IndexingSignal::NotReady(_) => {}
            _ => panic!("expected NotReady"),
        }
    }

    #[test]
    fn classifies_empty_edit_as_not_ready() {
        match classify_indexing_signal(&ok_empty()) {
            IndexingSignal::NotReady(_) => {}
            _ => panic!("expected NotReady"),
        }
    }

    #[test]
    fn classifies_unknown_lsp_error_as_hard_failure() {
        match classify_indexing_signal(&lsp_err(-99999, "something else")) {
            IndexingSignal::HardFailure => {}
            _ => panic!("expected HardFailure"),
        }
    }

    #[test]
    fn classifies_no_references_with_unrelated_message_as_hard_failure() {
        // -32602 is also used for genuine invalid params; only the
        // "No references found" sub-case is the indexing race.
        match classify_indexing_signal(&lsp_err(-32602, "missing required field")) {
            IndexingSignal::HardFailure => {}
            _ => panic!("expected HardFailure"),
        }
    }
}

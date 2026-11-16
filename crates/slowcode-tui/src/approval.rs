//! TUI approval bridge.
//!
//! Requests are rendered as escaped text. The provider never interprets
//! preview content as terminal control sequences.

use std::sync::Arc;

use slowcode_kernel::agent::{
    ApprovalFuture, ApprovalProvider, ApprovalProviderError, ApprovalRequest,
};
use slowcode_kernel::session::ApprovalOutcome;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// One approval request sent to the renderer.
pub struct ApprovalMessage {
    pub id: String,
    pub tool_name: String,
    pub reason: Option<String>,
    pub preview: String,
    pub respond: oneshot::Sender<ApprovalOutcome>,
}

/// Sends approval requests to the TUI event loop.
#[derive(Clone)]
pub struct TuiApprovalProvider {
    sender: mpsc::UnboundedSender<ApprovalMessage>,
}

impl TuiApprovalProvider {
    pub fn new(sender: mpsc::UnboundedSender<ApprovalMessage>) -> Arc<Self> {
        Arc::new(Self { sender })
    }
}

impl ApprovalProvider for TuiApprovalProvider {
    fn request(
        &self,
        request: ApprovalRequest,
        cancellation: CancellationToken,
    ) -> ApprovalFuture<'_> {
        let sender = self.sender.clone();
        Box::pin(async move {
            let (respond, reply) = oneshot::channel();
            let message = ApprovalMessage {
                id: request.id().to_string(),
                tool_name: request.tool_name().to_owned(),
                reason: request.reason().map(str::to_owned),
                preview: request.preview().to_owned(),
                respond,
            };
            if sender.send(message).is_err() {
                return Ok(ApprovalOutcome::Unavailable);
            }
            tokio::select! {
                _ = cancellation.cancelled() => Ok(ApprovalOutcome::Cancelled),
                decision = reply => Ok(decision.unwrap_or(ApprovalOutcome::Unavailable)),
            }
        })
    }
}

#[allow(dead_code)]
fn unavailable() -> Result<ApprovalOutcome, ApprovalProviderError> {
    Ok(ApprovalOutcome::Unavailable)
}

//! Placeholder handlers for every [`JobKind`] — they log and succeed so
//! the enqueue → claim → execute → ack path works end-to-end before the
//! real implementations land in later epics.

use serde_json::Value;

use crate::{
    jobs::{JobContext, JobHandler, JobKind, Registry},
    Result,
};

struct Stub(JobKind);

#[async_trait::async_trait]
impl JobHandler for Stub {
    async fn run(&self, _ctx: &JobContext, payload: &Value) -> Result<()> {
        // EmailSend payloads carry bearer-grade secrets (verification
        // tokens) — never log them.
        if self.0 == JobKind::EmailSend {
            tracing::info!(
                kind = self.0.as_str(),
                "stub handler — email sending not implemented yet"
            );
        } else {
            tracing::info!(kind = self.0.as_str(), payload = %payload, "stub handler — not implemented yet");
        }
        Ok(())
    }
}

/// Register a stub handler for every [`JobKind`].
pub fn register_all(registry: &mut Registry) {
    for kind in JobKind::ALL {
        registry.register(*kind, Stub(*kind));
    }
}

//! `NotificationFanout` registration (epic 16). Registered after the
//! stub set in `cli::start_worker`, so the real handler wins the
//! handler map.

use crate::jobs::{JobKind, Registry};

/// Wire the handler into the job registry.
pub(crate) fn register(registry: &mut Registry) {
    registry.register(
        JobKind::NotificationFanout,
        super::fanout::NotificationFanoutJob,
    );
}

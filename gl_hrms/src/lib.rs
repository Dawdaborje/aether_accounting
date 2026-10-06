//! Hands the events HR announces to the ledger.

use aether_sdk::events::Received;
use aether_sdk::prelude::*;

handler! {
    /// Runs as the kernel for each event this plugin listens to. The ledger decides what, if anything, to post.
    fn forward(received: Received) -> Value {
        plugins::call(
            "gl",
            "on_posting_event",
            &json!({ "event": received.event, "source": received.source, "payload": received.payload, "emitted_by": received.emitted_by, "depth": received.depth }),
        )
    }
}

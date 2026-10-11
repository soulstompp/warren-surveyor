// Copyright (c) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The notices the server sends a session while one of its statements runs. The client library
//! hands each one over only as a `tracing` event on `sqlx::postgres::notice`, so a layer of the
//! program's subscriber catches them, and keeps them only while the future `catch` arms is polled:
//! a notice from another session, or from the same session at another time, is not kept.

use std::cell::RefCell;
use std::future::Future;

use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

/// Where the client library sends the server's notices.
pub const TARGET: &str = "sqlx::postgres::notice";

tokio::task_local! {
    static CAUGHT: RefCell<Vec<String>>;
}

struct Catch;

impl<S: Subscriber> Layer<S> for Catch {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let _ = CAUGHT.try_with(|caught| {
            let mut m = Message(None);
            event.record(&mut m);
            if let Some(m) = m.0 {
                caught.borrow_mut().push(m);
            }
        });
    }
}

/// A notice's text, from the event's `message` field.
struct Message(Option<String>);

impl Visit for Message {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0 = Some(value.to_string());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" && self.0.is_none() {
            self.0 = Some(format!("{value:?}"));
        }
    }
}

/// The layer that catches the notices: it sees the notices, of every severity, and nothing else.
pub fn layer<S>() -> impl Layer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    Catch.with_filter(Targets::new().with_target(TARGET, LevelFilter::TRACE))
}

/// Runs `fut`, and returns its output with every notice caught while it was polled, in order.
pub async fn catch<F: Future>(fut: F) -> (F::Output, Vec<String>) {
    CAUGHT
        .scope(RefCell::new(Vec::new()), async move {
            let out = fut.await;
            (out, CAUGHT.with(RefCell::take))
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::prelude::*;

    fn subscriber() -> impl Subscriber + Send + Sync {
        tracing_subscriber::registry().with(layer())
    }

    #[tokio::test]
    async fn a_notice_is_kept_only_while_the_future_catch_arms_is_polled() {
        let _log = tracing::subscriber::set_default(subscriber());
        tracing::info!(target: TARGET, message = "before");
        let ((), caught) = catch(async {
            tracing::info!(target: TARGET, message = "first");
            tokio::task::yield_now().await;
            tracing::info!(target: "warren_bench::other", message = "no notice");
            tracing::warn!(target: TARGET, message = "second");
        })
        .await;
        tracing::info!(target: TARGET, message = "after");
        assert_eq!(caught, ["first", "second"]);
    }

    #[tokio::test]
    async fn a_notice_sent_beside_the_armed_future_in_the_same_task_is_not_kept() {
        // as the watchdog reads the control session beside the timed statement
        let _log = tracing::subscriber::set_default(subscriber());
        let armed = catch(async {
            tokio::task::yield_now().await;
            tracing::info!(target: TARGET, message = "inside");
        });
        let beside = async {
            tracing::info!(target: TARGET, message = "beside");
            tokio::task::yield_now().await;
            tracing::info!(target: TARGET, message = "beside again");
        };
        let (((), caught), ()) = tokio::join!(armed, beside);
        assert_eq!(caught, ["inside"]);
    }
}

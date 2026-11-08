//! Spans for the store's queries.
//!
//! [`Traced`] wraps a [`Backend`] and opens one `store <method>` span per
//! call, with the `by.store.backend` (`sqlite` or `postgres`) and
//! `by.store.method` attributes and an error status when the call failed.
//! There are thousands of queries in an operation, so the spans are off
//! unless the operator turned them on (`BRANCHYARD_TRACE_STORE=1` with
//! spans exported), and even then only a query made inside a span of the
//! trace gets one: a query on a thread with no span in scope would start a
//! trace of its own for every call. They are of the target
//! [`branchyard_support::trace::STORE_TARGET`], which the telemetry layer
//! bridges only while the switch is on; with it off a call is a flag read.
//!
//! The wrapper is written out method by method; the trait has no default
//! methods, so a method added to it does not compile here until it is
//! forwarded.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use branchyard_support::trace;
use serde_json::Value;

use crate::state::{
    Acquired, Backend, Begun, FeedRow, Fence, LeaseRow, Owner, ProcessRow, Record, ReservationRow,
    SteerRow, StepRow,
};
use crate::{Error, Message, RecordedEvent, SteerState};

/// A [`Backend`] whose calls are spans when store spans are on.
pub(crate) struct Traced {
    backend: &'static str,
    inner: Arc<dyn Backend>,
}

impl fmt::Debug for Traced {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

impl Traced {
    /// `inner`, named `backend` in the spans' attributes.
    pub(crate) fn wrap(backend: &'static str, inner: Arc<dyn Backend>) -> Arc<dyn Backend> {
        Arc::new(Traced { backend, inner })
    }

    fn query<T>(
        &self,
        method: &'static str,
        run: impl FnOnce() -> Result<T, Error>,
    ) -> Result<T, Error> {
        if !trace::store_spans() || tracing::Span::current().is_none() {
            return run();
        }
        let span = tracing::info_span!(
            target: trace::STORE_TARGET,
            "store",
            otel.name = format!("store {method}"),
            "by.store.backend" = self.backend,
            "by.store.method" = method,
            otel.status_description = tracing::field::Empty,
        );
        let result = span.in_scope(run);
        if let Err(error) = &result {
            let why: String = error.to_string().chars().take(200).collect();
            span.record("otel.status_description", why.as_str());
        }
        result
    }
}

impl Backend for Traced {
    fn reserve(&self, name: &str, owner: &Owner) -> Result<bool, Error> {
        self.query("reserve", || self.inner.reserve(name, owner))
    }

    fn release(&self, name: &str) -> Result<(), Error> {
        self.query("release", || self.inner.release(name))
    }

    fn reservations(&self) -> Result<Vec<ReservationRow>, Error> {
        self.query("reservations", || self.inner.reservations())
    }

    fn reclaim(&self, row: &ReservationRow) -> Result<bool, Error> {
        self.query("reclaim", || self.inner.reclaim(row))
    }

    fn taken(&self, name: &str) -> Result<bool, Error> {
        self.query("taken", || self.inner.taken(name))
    }

    fn read(&self, name: &str) -> Result<Option<Record>, Error> {
        self.query("read", || self.inner.read(name))
    }

    fn list(&self) -> Result<Vec<Record>, Error> {
        self.query("list", || self.inner.list())
    }

    fn write(&self, record: &Record, fence: Option<&Fence>) -> Result<(), Error> {
        self.query("write", || self.inner.write(record, fence))
    }

    fn add_child(&self, parent: &str, child: &str) -> Result<(), Error> {
        self.query("add_child", || self.inner.add_child(parent, child))
    }

    fn delete(&self, name: &str) -> Result<(), Error> {
        self.query("delete", || self.inner.delete(name))
    }

    fn acquire(&self, record: &Record, owner: &Owner, ttl: Duration) -> Result<Acquired, Error> {
        self.query("acquire", || self.inner.acquire(record, owner, ttl))
    }

    fn renew(&self, fence: &Fence, ttl: Duration) -> Result<(), Error> {
        self.query("renew", || self.inner.renew(fence, ttl))
    }

    fn finish(
        &self,
        fence: &Fence,
        record: Option<&Record>,
        event: Option<&RecordedEvent>,
    ) -> Result<(), Error> {
        self.query("finish", || self.inner.finish(fence, record, event))
    }

    fn leases(&self) -> Result<Vec<LeaseRow>, Error> {
        self.query("leases", || self.inner.leases())
    }

    fn incarnation(&self, name: &str) -> Result<Option<i64>, Error> {
        self.query("incarnation", || self.inner.incarnation(name))
    }

    fn label(&self) -> String {
        self.inner.label()
    }

    fn clock_ms(&self) -> Result<u64, Error> {
        self.query("clock_ms", || self.inner.clock_ms())
    }

    fn take_over(
        &self,
        lease: &LeaseRow,
        owner: &Owner,
        ttl: Duration,
    ) -> Result<Option<Fence>, Error> {
        self.query("take_over", || self.inner.take_over(lease, owner, ttl))
    }

    fn set_deadline(&self, fence: &Fence, deadline_ms: Option<u64>) -> Result<(), Error> {
        self.query("set_deadline", || {
            self.inner.set_deadline(fence, deadline_ms)
        })
    }

    fn begin_step(
        &self,
        fence: &Fence,
        turn: u64,
        step: &str,
        intent: &Value,
    ) -> Result<Begun, Error> {
        self.query("begin_step", || {
            self.inner.begin_step(fence, turn, step, intent)
        })
    }

    fn finish_step(
        &self,
        fence: &Fence,
        turn: u64,
        step: &str,
        outcome: &Value,
    ) -> Result<(), Error> {
        self.query("finish_step", || {
            self.inner.finish_step(fence, turn, step, outcome)
        })
    }

    fn abandon_step(&self, fence: &Fence, turn: u64, step: &str) -> Result<(), Error> {
        self.query("abandon_step", || {
            self.inner.abandon_step(fence, turn, step)
        })
    }

    fn begin_step_delivering(
        &self,
        fence: &Fence,
        turn: u64,
        step: &str,
        intent: &Value,
        deliver: &[u64],
    ) -> Result<Begun, Error> {
        self.query("begin_step_delivering", || {
            self.inner
                .begin_step_delivering(fence, turn, step, intent, deliver)
        })
    }

    fn abandon_step_delivering(
        &self,
        fence: &Fence,
        turn: u64,
        step: &str,
        deliver: &[u64],
    ) -> Result<(), Error> {
        self.query("abandon_step_delivering", || {
            self.inner
                .abandon_step_delivering(fence, turn, step, deliver)
        })
    }

    fn steps(&self, name: &str, turn: u64) -> Result<Vec<StepRow>, Error> {
        self.query("steps", || self.inner.steps(name, turn))
    }

    fn execution_host(&self, name: &str) -> Result<Option<String>, Error> {
        self.query("execution_host", || self.inner.execution_host(name))
    }

    fn record_process(&self, fence: &Fence, process: &ProcessRow) -> Result<(), Error> {
        self.query("record_process", || {
            self.inner.record_process(fence, process)
        })
    }

    fn processes(&self, name: &str, turn: u64) -> Result<Vec<ProcessRow>, Error> {
        self.query("processes", || self.inner.processes(name, turn))
    }

    fn request_cancel(&self, name: &str, by: &str, subtree: bool) -> Result<bool, Error> {
        self.query("request_cancel", || {
            self.inner.request_cancel(name, by, subtree)
        })
    }

    fn cancel_requested(&self, fence: &Fence) -> Result<Option<String>, Error> {
        self.query("cancel_requested", || self.inner.cancel_requested(fence))
    }

    fn request_steer(
        &self,
        name: &str,
        by: &str,
        text: &str,
        message: Option<u64>,
    ) -> Result<Option<u64>, Error> {
        self.query("request_steer", || {
            self.inner.request_steer(name, by, text, message)
        })
    }

    fn pending_steers(&self, fence: &Fence) -> Result<Vec<SteerRow>, Error> {
        self.query("pending_steers", || self.inner.pending_steers(fence))
    }

    fn settle_steer(
        &self,
        fence: &Fence,
        id: u64,
        state: &SteerState,
    ) -> Result<Option<u64>, Error> {
        self.query("settle_steer", || self.inner.settle_steer(fence, id, state))
    }

    fn steer(&self, name: &str, id: u64) -> Result<Option<SteerRow>, Error> {
        self.query("steer", || self.inner.steer(name, id))
    }

    fn append(
        &self,
        name: &str,
        event: &RecordedEvent,
        fence: Option<&Fence>,
    ) -> Result<u64, Error> {
        self.query("append", || self.inner.append(name, event, fence))
    }

    fn events_since(
        &self,
        name: &str,
        after: u64,
        limit: usize,
    ) -> Result<Vec<(u64, RecordedEvent)>, Error> {
        self.query("events_since", || {
            self.inner.events_since(name, after, limit)
        })
    }

    fn event_count(&self, name: &str) -> Result<u64, Error> {
        self.query("event_count", || self.inner.event_count(name))
    }

    fn feed_since(&self, after: u64, limit: usize) -> Result<Vec<FeedRow>, Error> {
        self.query("feed_since", || self.inner.feed_since(after, limit))
    }

    fn head(&self) -> Result<u64, Error> {
        self.query("head", || self.inner.head())
    }

    fn send_message(&self, message: &Message, key: Option<&str>) -> Result<Message, Error> {
        self.query("send_message", || self.inner.send_message(message, key))
    }

    fn message_by_key(&self, key: &str) -> Result<Option<Message>, Error> {
        self.query("message_by_key", || self.inner.message_by_key(key))
    }

    fn commit_handoff(&self, commit: &crate::handoff::HandoffCommit) -> Result<Message, Error> {
        self.query("commit_handoff", || self.inner.commit_handoff(commit))
    }

    fn message(&self, id: u64) -> Result<Option<Message>, Error> {
        self.query("message", || self.inner.message(id))
    }

    fn inbox(&self, to: &str) -> Result<Vec<Message>, Error> {
        self.query("inbox", || self.inner.inbox(to))
    }

    fn inbox_for_turn(&self, to: &str, turn: u64) -> Result<Vec<Message>, Error> {
        self.query("inbox_for_turn", || self.inner.inbox_for_turn(to, turn))
    }

    fn mark_delivered(&self, ids: &[u64]) -> Result<(), Error> {
        self.query("mark_delivered", || self.inner.mark_delivered(ids))
    }

    fn answer_to(&self, question_id: u64) -> Result<Option<Message>, Error> {
        self.query("answer_to", || self.inner.answer_to(question_id))
    }

    fn unanswered(&self) -> Result<Vec<Message>, Error> {
        self.query("unanswered", || self.inner.unanswered())
    }

    fn message_steer(&self, id: u64) -> Result<Option<u64>, Error> {
        self.query("message_steer", || self.inner.message_steer(id))
    }

    fn set_awaiting(&self, id: u64, until_ms: Option<u64>) -> Result<(), Error> {
        self.query("set_awaiting", || self.inner.set_awaiting(id, until_ms))
    }

    fn awaiting_answer(&self, from: &str, now_ms: u64) -> Result<bool, Error> {
        self.query("awaiting_answer", || {
            self.inner.awaiting_answer(from, now_ms)
        })
    }
}

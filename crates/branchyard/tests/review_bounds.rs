//! What a read of a branch's changes and reviews may cost: review paths are
//! literal, and the work for a long history of reviews is bounded. Fake
//! agent, local repository; hermetic.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use branchyard::{DiffSide, Feedback, FeedbackItem, FeedbackSource, MAX_ADDRESSED_REVIEWS};
use common::Fixture;

fn review_of(commit: &str, path: &str, line: u32) -> Feedback {
    Feedback {
        source: FeedbackSource::Review,
        reference: Some(commit.to_owned()),
        items: vec![FeedbackItem {
            path: Some(path.to_owned()),
            line: Some(line),
            side: Some(DiffSide::New),
            body: "why?".into(),
            ..FeedbackItem::default()
        }],
        prompt: None,
    }
}

fn candidate(f: &Fixture, name: &str) -> String {
    f.yard
        .branch(name)
        .unwrap()
        .info()
        .candidate
        .clone()
        .unwrap()
        .commit
}

#[test]
fn a_stored_path_is_a_literal_path_not_a_pathspec() {
    let f = Fixture::new();
    let branch = f.task("WRITE b.txt=one").name("b").run().unwrap();
    let first = candidate(&f, "b");
    // A review whose path is a glob that would match every file.
    branch
        .record_feedback("review", review_of(&first, "*", 1))
        .unwrap();
    branch
        .record_feedback("review", review_of(&first, "b.txt", 1))
        .unwrap();
    f.yard
        .branch("b")
        .unwrap()
        .send("WRITE b.txt=two", f.options())
        .unwrap();
    let reviews = f.yard.branch("b").unwrap().reviews().unwrap();
    let now = candidate(&f, "b");
    assert_ne!(now, first);
    assert_eq!(reviews.reviews.len(), 2);
    assert_eq!(
        reviews.reviews[0].comments[0].addressed_in, None,
        "`*` was read as a pathspec"
    );
    assert_eq!(
        reviews.reviews[1].comments[0].addressed_in.as_deref(),
        Some(now.as_str())
    );
}

#[test]
fn only_the_latest_reviews_are_checked_for_being_addressed() {
    let f = Fixture::new();
    let branch = f.task("WRITE b.txt=one").name("b").run().unwrap();
    let first = candidate(&f, "b");
    for _ in 0..=MAX_ADDRESSED_REVIEWS {
        branch
            .record_feedback("review", review_of(&first, "b.txt", 1))
            .unwrap();
    }
    f.yard
        .branch("b")
        .unwrap()
        .send("WRITE b.txt=two", f.options())
        .unwrap();
    let reviews = f.yard.branch("b").unwrap().reviews().unwrap();
    assert_eq!(reviews.reviews.len(), MAX_ADDRESSED_REVIEWS + 1);
    let marked = |i: usize| reviews.reviews[i].comments[0].addressed_in.is_some();
    assert!(!marked(0), "the oldest review was checked");
    assert!(marked(1) && marked(MAX_ADDRESSED_REVIEWS));
}

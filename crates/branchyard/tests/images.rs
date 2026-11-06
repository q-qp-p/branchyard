//! The router and toolchain images (docs/images.md): a sandboxed candidate
//! is excluded, with its reason, when its image's labels say the harness is
//! not in it; an image without Branchyard labels excludes nothing and the
//! route says it could not tell. Labels come from a fixed source: no docker,
//! no registry, no network. No model is called.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use branchyard::images::{ImageLabelSource, ImageLabels};
use branchyard::{
    ContainerOptions, Fleet, FleetCandidate, FleetEntry, Provider, RouteOptions, TaskOptions,
};
use common::fake_agent;
use common::Fixture;

/// Answers every image with the same labels, and counts nothing else.
#[derive(Debug)]
struct Labels(Result<Option<BTreeMap<String, String>>, String>);

impl ImageLabelSource for Labels {
    fn labels(&self, _: &str) -> Result<Option<BTreeMap<String, String>>, String> {
        self.0.clone()
    }
}

fn image() -> String {
    format!("example.test/toolchain:1@sha256:{}", "a".repeat(64))
}

fn fleet() -> Fleet {
    let mut agent = FleetCandidate::new("gemini-cli");
    agent.command = Some(vec![fake_agent().display().to_string()]);
    Fleet {
        entries: [(
            "default".to_owned(),
            FleetEntry {
                candidates: vec![
                    FleetCandidate::new("codex"),
                    FleetCandidate::new("goose"),
                    agent,
                ],
                ..FleetEntry::default()
            },
        )]
        .into_iter()
        .collect(),
    }
}

fn sandboxed() -> TaskOptions {
    TaskOptions {
        provider: Some(Provider::container(ContainerOptions {
            image: image(),
            ..ContainerOptions::default()
        })),
        // Only the image decides here, not composition.
        unapproved_tools: true,
        ..TaskOptions::default()
    }
}

fn route(f: &Fixture, options: &TaskOptions, labels: Labels) -> branchyard::Route {
    let how = RouteOptions {
        seed: Some(1),
        images: Some(Arc::new(labels)),
        ..RouteOptions::default()
    };
    f.yard
        .route("fix the bug", options, &fleet(), &how, Some(3))
        .unwrap()
}

fn toolchain() -> BTreeMap<String, String> {
    ImageLabels {
        harnesses: [("codex".to_owned(), "0.157.1".to_owned())].into(),
        evidence: Default::default(),
        by: Some("0.4.0".into()),
        anvil: None,
    }
    .all()
}

#[test]
fn a_harness_the_images_labels_lack_is_excluded_with_the_reason() {
    let f = Fixture::new();
    let route = route(&f, &sandboxed(), Labels(Ok(Some(toolchain()))));
    let excluded: Vec<(&str, &str)> = route
        .excluded
        .iter()
        .map(|e| (e.candidate.harness.as_str(), e.reason.as_str()))
        .collect();
    assert_eq!(excluded.len(), 1, "{excluded:?}");
    assert_eq!(excluded[0].0, "goose");
    assert!(
        excluded[0]
            .1
            .contains(&format!("goose is not in the image {}", image())),
        "{excluded:?}"
    );
    assert!(
        excluded[0].1.contains("holds codex 0.157.1"),
        "{excluded:?}"
    );
    assert!(excluded[0].1.contains("by images build --harness goose"));
    // The harness it holds, and the candidate with a command of its own
    // (checked as a path in the image, not by labels), are picked.
    let picked: Vec<&str> = route
        .picks
        .iter()
        .map(|p| p.candidate.harness.as_str())
        .collect();
    assert!(
        picked.contains(&"codex") && picked.contains(&"gemini-cli"),
        "{picked:?}"
    );
    assert!(route.notes.is_empty(), "{:?}", route.notes);
}

#[test]
fn an_image_without_branchyard_labels_excludes_nothing_and_the_route_says_so() {
    let f = Fixture::new();
    let plain: BTreeMap<String, String> = [("maintainer".to_owned(), "me".to_owned())]
        .into_iter()
        .collect();
    for labels in [
        Labels(Ok(Some(plain))),
        Labels(Ok(None)),
        Labels(Err("registry down".into())),
    ] {
        let route = route(&f, &sandboxed(), labels);
        assert!(route.excluded.is_empty(), "{:?}", route.excluded);
        // Once per image and harness asked about, not once per attempt.
        assert_eq!(route.notes.len(), 2, "{:?}", route.notes);
        assert!(route.notes[0].contains(&image()), "{:?}", route.notes);
        assert!(
            route
                .notes
                .iter()
                .any(|n| n.contains("whether it holds codex is not known")),
            "{:?}",
            route.notes
        );
    }
    // The reason an image could not be read is in the note.
    let route = route(&f, &sandboxed(), Labels(Err("registry down".into())));
    assert!(
        route.notes[0].contains("registry down"),
        "{:?}",
        route.notes
    );
}

#[test]
fn a_task_with_its_own_command_or_no_sandbox_is_not_checked_against_an_image() {
    let f = Fixture::new();
    let commanded = TaskOptions {
        command: Some(vec![fake_agent().display().to_string()]),
        ..sandboxed()
    };
    let r = route(&f, &commanded, Labels(Ok(Some(toolchain()))));
    assert!(r.excluded.is_empty() && r.notes.is_empty(), "{r:?}");
    // A local candidate list is not asked about images at all: the source
    // would exclude goose, and it does not.
    let local = TaskOptions {
        provider: None,
        unapproved_tools: true,
        command: Some(vec![fake_agent().display().to_string()]),
        ..TaskOptions::default()
    };
    let r = route(&f, &local, Labels(Ok(Some(toolchain()))));
    assert!(r.excluded.is_empty() && r.notes.is_empty(), "{r:?}");
}

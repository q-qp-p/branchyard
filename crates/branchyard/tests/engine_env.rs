//! The environment a harness inherits, in a test binary of its own: the
//! engine drops the nested-session markers (`CLAUDECODE`) and a Branchyard
//! remote from what it starts a harness with, and keeps the rest; an
//! isolated harness also loses an Anthropic key but keeps Claude Code's
//! Vertex AI selectors. The test gives its own process a `BRANCHYARD_REMOTE`
//! for the engine to drop, and those variables; set beside other tests they
//! would reach everything those start.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use common::{fake_agent, text, Fixture};

#[test]
fn harnesses_inherit_the_environment_without_nested_session_markers() {
    // The harness must not inherit this remote from the engine's process.
    // The variable is process-wide, so this test is alone in its binary.
    std::env::set_var("BRANCHYARD_REMOTE", "http://127.0.0.1:9");
    for (name, value) in [
        ("CLAUDE_CODE_USE_VERTEX", "1"),
        ("ANTHROPIC_VERTEX_PROJECT_ID", "by-test-project"),
        ("CLOUD_ML_REGION", "us-east5"),
        ("ANTHROPIC_API_KEY", "sk-ant-by-test-inherited"),
    ] {
        std::env::set_var(name, value);
    }
    let f = Fixture::new();
    let prompt = "ENV CLAUDECODE BRANCHYARD_REMOTE BY_TEST_VISIBLE CLAUDE_CODE_USE_VERTEX \
                  ANTHROPIC_VERTEX_PROJECT_ID CLOUD_ML_REGION ANTHROPIC_API_KEY";
    let vertex = "CLAUDE_CODE_USE_VERTEX=1\nANTHROPIC_VERTEX_PROJECT_ID=by-test-project\n\
                  CLOUD_ML_REGION=us-east5\n";
    let shared = f.task(prompt).name("shared").run().unwrap();
    let home = std::env::var("HOME").unwrap();
    assert_eq!(
        text(&shared.events().unwrap()),
        format!(
            "HOME={home}\nCLAUDECODE unset\nBRANCHYARD_REMOTE unset\nBY_TEST_VISIBLE=yes\n\
             {vertex}ANTHROPIC_API_KEY=sk-ant-by-test-inherited\n"
        )
    );

    let isolated = f
        .task(prompt)
        .name("isolated")
        .isolated(true)
        .run()
        .unwrap();
    let private = f.root.join(".branchyard/homes/isolated");
    assert!(private.is_dir());
    assert_eq!(
        text(&isolated.events().unwrap()),
        format!(
            "HOME={}\nCLAUDECODE unset\nBRANCHYARD_REMOTE unset\nBY_TEST_VISIBLE=yes\n\
             {vertex}ANTHROPIC_API_KEY unset\n",
            private.display()
        )
    );
    f.yard.remove("isolated").unwrap();
    assert!(!private.exists());
    assert!(fake_agent().is_file());
}

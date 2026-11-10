//! Snapshots of the files written by `loom-agent config init` and the GUI, so
//! changes to configuration defaults show up in review.

use loom_config::{AgentConfig, ClientConfig};

#[test]
fn agent_yaml_template() {
    insta::assert_snapshot!(AgentConfig::default().to_yaml());
}

#[test]
fn client_yaml_template() {
    insta::assert_snapshot!(ClientConfig::default().to_yaml());
}

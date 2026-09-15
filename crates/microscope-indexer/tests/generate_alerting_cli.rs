use std::{path::PathBuf, process::Command};

fn workspace_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "microscope-generate-alerting-{}-{name}",
        std::process::id()
    ))
}

#[test]
fn an_invalid_config_stops_the_generator_before_grafana_provisions_anything() {
    let unparseable = ("unparseable", "program_id = \"x\"\n[[alerts]\n");
    let unusable_program_id = (
        "unusable-program-id",
        "program_id = \"not-a-pubkey\"\nidl_path = \"idl/program.json\"\n",
    );

    for (name, contents) in [unparseable, unusable_program_id] {
        let workspace = workspace_path(name);
        let config_path = workspace.join("microscope.toml");
        let alerting_dir = workspace.join("alerting");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(&config_path, contents).unwrap();

        let output = Command::new(env!("CARGO_BIN_EXE_microscope-indexer"))
            .arg("generate-alerting")
            .arg(&config_path)
            .arg(&alerting_dir)
            .arg(workspace.join("dashboards"))
            .output()
            .expect("the generator binary runs");

        assert!(
            !output.status.success(),
            "the {name} config must fail the alerting-config service instead of leaving Grafana unprovisioned"
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("failed to load config"));
        assert!(!alerting_dir.join("microscope.json").exists());

        std::fs::remove_dir_all(&workspace).unwrap();
    }
}

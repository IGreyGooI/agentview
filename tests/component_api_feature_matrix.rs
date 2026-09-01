use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn cargo_check(manifest: &Path, target: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO"))
        .arg("check")
        .arg("--manifest-path")
        .arg(manifest)
        .arg("--target-dir")
        .arg(target)
        .args(arguments)
        .output()
        .expect("downstream cargo check must run")
}

fn fixture_manifest(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/feature_matrix")
        .join(name)
        .join("Cargo.toml")
}

fn isolated_target(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

fn diagnostics(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn disabled_diagnostic_matches(
    output_diagnostics: &str,
    surface: &str,
    required_diagnostics: &[&str],
) -> bool {
    output_diagnostics.contains(surface)
        && required_diagnostics
            .iter()
            .all(|required| output_diagnostics.contains(required))
}

#[test]
fn disabled_constructor_diagnostic_rejects_an_incompatible_public_new() {
    let incompatible_constructors = [
        (
            "ComponentHost",
            "error[E0061]: this function takes 3 arguments but 2 arguments were supplied\n\
             ComponentHost::<Props>::new",
        ),
        (
            "ExternalApplication",
            "error[E0308]: arguments to this function are incorrect\n\
             ExternalApplication::new",
        ),
    ];

    for (owner, incompatible_new) in incompatible_constructors {
        assert!(!disabled_diagnostic_matches(
            incompatible_new,
            owner,
            MISSING_NEW_DIAGNOSTICS
        ));
    }
}

const MISSING_NEW_DIAGNOSTICS: &[&str] =
    &["error[E0599]", "no function or associated item named `new`"];

const LEGACY_PROBES: &[(&str, &str, &str, &[&str])] = &[
    ("provider_port", "ProviderPort", "ProviderPort", &[]),
    (
        "application_host",
        "ApplicationHost",
        "ApplicationHost",
        &[],
    ),
    (
        "component_reaction_runtime",
        "ComponentReactionRuntime",
        "ComponentReactionRuntime",
        &[],
    ),
    ("prelude_event_input", "EventInput", "EventInput", &[]),
    (
        "prelude_event_listener",
        "EventListener",
        "EventListener",
        &[],
    ),
    (
        "prelude_component_events",
        "ComponentEvents",
        "ComponentEvents",
        &[],
    ),
    ("authoring_event_input", "EventInput", "EventInput", &[]),
    (
        "authoring_event_listener",
        "EventListener",
        "EventListener",
        &[],
    ),
    (
        "component_host_new",
        "ComponentHost",
        "ComponentHost::<Props>::new",
        MISSING_NEW_DIAGNOSTICS,
    ),
    (
        "external_application_new",
        "ExternalApplication",
        "ExternalApplication::new",
        MISSING_NEW_DIAGNOSTICS,
    ),
];

fn assert_deprecation(output: &Output, probe: &str, diagnostic_surface: &str) {
    let output_diagnostics = diagnostics(output);
    assert!(
        output.status.success(),
        "legacy `{probe}` probe must compile with compatibility enabled:\n{output_diagnostics}"
    );
    assert!(
        output_diagnostics.contains("deprecated")
            && output_diagnostics.contains(diagnostic_surface),
        "enabled legacy probe must emit a deprecation naming `{diagnostic_surface}`:\n{output_diagnostics}"
    );
    if probe.ends_with("_new") {
        assert!(
            output_diagnostics.contains("deprecated associated function"),
            "constructor probe must deprecate the associated `new` function itself:\n{output_diagnostics}"
        );
    }
}

#[test]
fn native_consumer_compiles_without_default_features() {
    let output = cargo_check(
        &fixture_manifest("native_consumer"),
        &isolated_target("native-consumer"),
        &["--no-default-features"],
    );

    assert!(
        output.status.success(),
        "native downstream consumer must compile without defaults:\n{}",
        diagnostics(&output)
    );
}

#[test]
fn legacy_consumer_requires_feature_and_reports_deprecations_when_enabled() {
    let manifest = fixture_manifest("legacy_consumer");
    let target = isolated_target("legacy-consumer");
    for &(probe, surface, diagnostic_surface, required_disabled_diagnostics) in LEGACY_PROBES {
        let disabled = cargo_check(
            &manifest,
            &target,
            &["--no-default-features", "--bin", probe],
        );
        let disabled_diagnostics = diagnostics(&disabled);
        assert!(
            !disabled.status.success(),
            "legacy `{surface}` probe unexpectedly compiled without the forwarded feature"
        );
        assert!(
            disabled_diagnostic_matches(
                &disabled_diagnostics,
                surface,
                required_disabled_diagnostics,
            ),
            "disabled legacy probe diagnostic must name `{surface}` and contain \
             {required_disabled_diagnostics:?}:\n{disabled_diagnostics}"
        );

        let enabled = cargo_check(
            &manifest,
            &target,
            &[
                "--no-default-features",
                "--features",
                "legacy-provider-port",
                "--bin",
                probe,
            ],
        );
        assert_deprecation(&enabled, probe, diagnostic_surface);
    }
}

#[test]
fn legacy_consumer_compiles_through_agentview_default_feature() {
    let manifest = fixture_manifest("legacy_default_consumer");
    let target = isolated_target("legacy-default-consumer");

    for &(probe, _, diagnostic_surface, _) in LEGACY_PROBES {
        let enabled_by_default = cargo_check(&manifest, &target, &["--bin", probe]);
        assert_deprecation(&enabled_by_default, probe, diagnostic_surface);
    }
}

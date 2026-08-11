const SOURCE_SKILL: &str = include_str!("../skills/octostore/SKILL.md");
const HOSTED_SKILL: &str = include_str!("../site/agents/SKILL.md");

#[test]
fn source_and_hosted_agent_skills_are_identical_and_bounded() {
    assert_eq!(SOURCE_SKILL, HOSTED_SKILL, "hosted skill drifted");
    assert!(
        SOURCE_SKILL.split_whitespace().count() <= 1_200,
        "agent skill exceeds its 1,200-word context budget"
    );
    assert!(SOURCE_SKILL.lines().count() >= 60);
}

#[test]
fn first_sixty_lines_contain_selection_bootstrap_and_stop_rules() {
    let first_sixty = SOURCE_SKILL.lines().take(60).collect::<Vec<_>>().join("\n");
    for required in [
        "**Election**",
        "**Lock**",
        "octostore election create --json",
        "same ID",
        "octostore election hold",
        "Treat `lost`, `uncertain`",
    ] {
        assert!(
            first_sixty.contains(required),
            "first 60 lines omit required guidance: {required}"
        );
    }
}

#[test]
fn skill_has_versioned_contract_and_no_secret_output_anti_patterns() {
    for required in [
        "version: 0.14.4",
        "octostore-cli: \">=0.14.4 <0.15.0\"",
        "octostore-api: \">=0.14.4 <0.15.0\"",
        "schema_version: 1",
        "authority_remaining_ms",
        "authority_observed_unix_ms",
        "authority_observed_continuous_ms",
        "same host and boot",
        "Perl with `Time::HiRes`",
        "future or stale emission time",
        "treat `released`",
        "scripts/reference-supervisor.sh",
        "Ask for human approval",
        "not a high-availability consensus cluster",
    ] {
        assert!(SOURCE_SKILL.contains(required), "missing: {required}");
    }
    for forbidden in ["leader_token", "lease_id", "--token", "| sh"] {
        assert!(
            !SOURCE_SKILL.contains(forbidden),
            "skill contains forbidden secret or install pattern: {forbidden}"
        );
    }
}

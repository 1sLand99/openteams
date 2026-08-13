#[test]
fn agent_runtime_does_not_special_case_adapter_probe_semantics() {
    let source = include_str!("../src/services/agent_runtime.rs");
    let production = source
        .split_once("#[cfg(test)]\nmod tests")
        .map_or(source, |(production, _)| production);
    for forbidden in [
        "runner == BaseCodingAgent::Hermes",
        "runner == BaseCodingAgent::DeepseekHarness",
        "RunnerDiscoveryOutcome::Hermes",
        "executors::executors::hermes",
        "executors::executors::deepseek_harness",
        "provider_needs_setup",
    ] {
        assert!(
            !production.contains(forbidden),
            "agent_runtime must consume executor capabilities instead of `{forbidden}`"
        );
    }
}

use crate::models::{CandidateDecision, ResearchCandidate};

pub fn candidate_matrix() -> Vec<ResearchCandidate> {
    vec![
        ResearchCandidate {
            name: "Headroom".into(),
            category: "Prompt optimization".into(),
            repository: "https://github.com/chopratejas/headroom".into(),
            runtime: "Python".into(),
            license: "Confirm during packaging".into(),
            local_only_fit: "Excellent fit as the default local optimization stage".into(),
            install_method: "Pinned package install inside Headroom-managed Python".into(),
            maintenance: "Core v1 dependency".into(),
            decision: CandidateDecision::Include,
            notes: "Mandatory optimizer stage for every configured client.".into(),
        },
        ResearchCandidate {
            name: "Vitals".into(),
            category: "Project health".into(),
            repository: "https://github.com/chopratejas/vitals".into(),
            runtime: "Python".into(),
            license: "Confirm during packaging".into(),
            local_only_fit: "Strong fit for scheduled project scans".into(),
            install_method: "Pinned package install inside Headroom-managed Python".into(),
            maintenance: "Track alongside Headroom".into(),
            decision: CandidateDecision::Include,
            notes: "Primary project health and code analysis tool in v1.".into(),
        },
        ResearchCandidate {
            name: "claw-compactor".into(),
            category: "Prompt optimization".into(),
            repository: "https://github.com/aeromomo/claw-compactor".into(),
            runtime: "Python".into(),
            license: "Confirm during packaging".into(),
            local_only_fit: "Promising if adapter IO is stable".into(),
            install_method: "Optional install in managed Python".into(),
            maintenance: "Medium".into(),
            decision: CandidateDecision::Research,
            notes: "Candidate optional stage if it stays within the pipeline contract.".into(),
        },
        ResearchCandidate {
            name: "rtk".into(),
            category: "Token optimization".into(),
            repository: "https://github.com/rtk-ai/rtk".into(),
            runtime: "Rust binary".into(),
            license: "Confirm during packaging".into(),
            local_only_fit: "Strong fit for local shell output compression and Claude hooks".into(),
            install_method: "Pinned binary download into Headroom-managed storage".into(),
            maintenance: "Track alongside Claude Code integration".into(),
            decision: CandidateDecision::Include,
            notes:
                "Installed by default so Claude Code bash commands are auto-rewritten through RTK."
                    .into(),
        },
        ResearchCandidate {
            name: "claude-cognitive".into(),
            category: "Workflow enhancement".into(),
            repository: "https://github.com/GMaN1911/claude-cognitive".into(),
            runtime: "Outside v1 policy".into(),
            license: "Confirm during packaging".into(),
            local_only_fit: "Too shell-profile oriented for Headroom v1".into(),
            install_method: "Manual external setup".into(),
            maintenance: "Medium".into(),
            decision: CandidateDecision::Defer,
            notes: "Deferred because it does not fit the Python-only runtime boundary.".into(),
        },
    ]
}

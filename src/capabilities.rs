use serde::Serialize;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SupportLevel {
    Supported,
    Partial,
    Unsupported,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Verification {
    RuntimeTested,
    CompileChecked,
    NotVerified,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BoundaryStatus {
    Configured,
    Present,
    NotAttempted,
    Unsupported,
}
#[derive(Debug, Serialize)]
pub(crate) struct CapabilityRecord {
    pub id: String,
    pub support: SupportLevel,
    pub verification: Verification,
    pub scope: String,
    pub reason: String,
}
#[derive(Debug, Serialize)]
pub(crate) struct DiscoveryBoundary {
    pub id: String,
    pub status: BoundaryStatus,
    pub scope: String,
    pub reason: String,
}
#[derive(Debug, Serialize)]
pub(crate) struct CapabilityReport {
    pub capabilities: Vec<CapabilityRecord>,
    pub boundaries: Vec<DiscoveryBoundary>,
}

struct Declaration {
    id: &'static str,
    legacy_keys: &'static [&'static str],
    support: SupportLevel,
    verification: Verification,
    scope: &'static str,
    reason: &'static str,
}

fn target_aware_verification() -> Verification {
    if cfg!(target_os = "linux") {
        Verification::RuntimeTested
    } else {
        Verification::CompileChecked
    }
}

fn declaration_table() -> Vec<Declaration> {
    let v = target_aware_verification();
    vec![
        Declaration { id: "library.configured", legacy_keys: &[], support: SupportLevel::Supported, verification: v, scope: "configured library", reason: "reads only the configured canonical library" },
        Declaration { id: "git.relationships", legacy_keys: &[], support: SupportLevel::Supported, verification: v, scope: "subscriptions/publications", reason: "reports existing Git relationships without fetching" },
        Declaration { id: "local.adoption", legacy_keys: &[], support: SupportLevel::Supported, verification: v, scope: "local adoption", reason: "reports existing explicit local adoption records" },
        Declaration { id: "harness.links", legacy_keys: &[], support: SupportLevel::Supported, verification: v, scope: "existing harness links", reason: "reports recorded links with bounded read-only health checks; never creates or probes arbitrary links" },
        Declaration { id: "worker.startup", legacy_keys: &["startup"], support: SupportLevel::Partial, verification: v, scope: "native startup provider", reason: "native runtime tested only on Linux; macOS and Windows are compile-checked" },
        Declaration { id: "registry.discovery", legacy_keys: &["registries", "registry_integration"], support: SupportLevel::Unsupported, verification: Verification::NotVerified, scope: "registries", reason: "registry clients and provider activation are not implemented" },
        Declaration { id: "harness.discovery", legacy_keys: &["harness_discovery", "harness_filtering", "harness_reload"], support: SupportLevel::Unsupported, verification: Verification::NotVerified, scope: "arbitrary harness roots", reason: "automatic discovery/filtering/reload is not implemented; inventory only checks recorded links" },
        Declaration { id: "personal-library.sync", legacy_keys: &["full_tui"], support: SupportLevel::Unsupported, verification: Verification::NotVerified, scope: "personal library", reason: "personal-library synchronization is not implemented" },
        Declaration { id: "autonomous-curation", legacy_keys: &["hermes_autonomous_curation"], support: SupportLevel::Unsupported, verification: Verification::NotVerified, scope: "Hermes curation", reason: "autonomous curation eligibility and write-back are not verified" },
    ]
}

pub(crate) fn legacy_capability_value(key: &str) -> &'static str {
    declaration_table()
        .into_iter()
        .find(|d| d.legacy_keys.contains(&key))
        .map(|d| match d.support {
            SupportLevel::Supported => "supported",
            SupportLevel::Partial => "partial",
            SupportLevel::Unsupported => "unsupported",
            SupportLevel::Unknown => "unknown",
        })
        .unwrap_or("unsupported")
}

pub(crate) fn capability_report(library: &str, initialized: bool) -> CapabilityReport {
    let capabilities = declaration_table()
        .into_iter()
        .map(|d| CapabilityRecord {
            id: d.id.into(),
            support: d.support,
            verification: d.verification,
            scope: d.scope.into(),
            reason: d.reason.into(),
        })
        .collect();
    CapabilityReport { capabilities, boundaries: vec![
        DiscoveryBoundary { id: "library.configured".into(), status: if initialized { BoundaryStatus::Configured } else { BoundaryStatus::NotAttempted }, scope: library.into(), reason: "configured path only; no arbitrary root scan".into() },
        DiscoveryBoundary { id: "git.subscriptions".into(), status: BoundaryStatus::Present, scope: "existing state".into(), reason: "reports persisted subscriptions only".into() },
        DiscoveryBoundary { id: "git.publications".into(), status: BoundaryStatus::Present, scope: "existing state".into(), reason: "reports persisted publications only".into() },
        DiscoveryBoundary { id: "local.adoptions".into(), status: BoundaryStatus::Present, scope: "existing state".into(), reason: "reports persisted local adoptions only".into() },
        DiscoveryBoundary { id: "harness.links".into(), status: BoundaryStatus::Present, scope: "recorded harness links".into(), reason: "bounded read-only existence, symlink, and target health checks only; no arbitrary root scan or mutation".into() },
        DiscoveryBoundary { id: "registry.discovery".into(), status: BoundaryStatus::Unsupported, scope: "registries".into(), reason: "not attempted; unsupported".into() },
        DiscoveryBoundary { id: "harness-root.auto-discovery".into(), status: BoundaryStatus::NotAttempted, scope: "arbitrary harness roots".into(), reason: "not attempted; scanning is intentionally disabled".into() },
        DiscoveryBoundary { id: "personal-library.sync".into(), status: BoundaryStatus::Unsupported, scope: "personal library".into(), reason: "not attempted; unsupported".into() },
        DiscoveryBoundary { id: "autonomous-curation".into(), status: BoundaryStatus::Unsupported, scope: "Hermes".into(), reason: "not attempted; unsupported".into() },
    ] }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn declarations_have_stable_typed_boundaries() {
        let r = capability_report("/library", false);
        assert_eq!(legacy_capability_value("harness_discovery"), "unsupported");
        assert!(declaration_table()
            .iter()
            .any(|d| d.legacy_keys.contains(&"harness_discovery")));
        assert_eq!(r.capabilities[0].id, "library.configured");
        assert!(r
            .capabilities
            .iter()
            .any(|c| c.id == "registry.discovery" && c.support == SupportLevel::Unsupported));
        assert!(r
            .boundaries
            .iter()
            .any(|b| b.id == "harness-root.auto-discovery"
                && b.status == BoundaryStatus::NotAttempted));
    }
    #[test]
    fn legacy_startup_projects_typed_worker_startup() {
        let report = capability_report("/library", false);
        let typed = report
            .capabilities
            .iter()
            .find(|c| c.id == "worker.startup")
            .unwrap();
        let typed_value = match typed.support {
            SupportLevel::Supported => "supported",
            SupportLevel::Partial => "partial",
            SupportLevel::Unsupported => "unsupported",
            SupportLevel::Unknown => "unknown",
        };
        assert_eq!(legacy_capability_value("startup"), typed_value);
    }

    #[test]
    fn platform_support_does_not_overclaim_native_runtime() {
        let r = capability_report("/library", false);
        let c = r
            .capabilities
            .iter()
            .find(|c| c.id == "worker.startup")
            .unwrap();
        assert!(matches!(
            c.verification,
            Verification::RuntimeTested | Verification::CompileChecked
        ));
        assert!(c.reason.contains("native"));
    }
}

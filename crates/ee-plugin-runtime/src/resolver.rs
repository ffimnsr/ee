//! Service dependency resolution and deterministic activation order.
//!
//! Resolution is pure: it consumes validated manifests and returns an order plus
//! blocked plugins. A blocked plugin never prevents unrelated plugins from
//! activating.

use std::collections::{BTreeMap, BTreeSet};

use ee_plugin_api::error::ErrorClass;
use ee_plugin_api::ids::ServiceId;
use ee_plugin_api::manifest::ManifestV2;

use crate::diagnostics::{Diagnostic, DiagnosticPhase};

/// Deterministic resolution result.
#[derive(Debug, Clone, Default)]
pub struct Resolution {
    /// Plugin names in activation order.
    pub order: Vec<String>,
    /// Service id to the single provider that survived resolution.
    pub providers: BTreeMap<ServiceId, String>,
    /// Diagnostics for plugins that cannot activate.
    pub blocked: Vec<Diagnostic>,
}

impl Resolution {
    pub fn is_blocked(&self, name: &str) -> bool {
        self.blocked.iter().any(|diagnostic| diagnostic.plugin.as_deref() == Some(name))
    }
}

/// Resolve every manifest into one deterministic activation order.
pub fn resolve(manifests: &BTreeMap<String, ManifestV2>) -> Resolution {
    resolve_with_priority(manifests, &BTreeSet::new())
}

/// Resolve with a set of names that should activate first among ready nodes.
///
/// Dependencies still win: a prioritized plugin only moves ahead of plugins
/// whose dependencies are already satisfied.
pub fn resolve_with_priority(
    manifests: &BTreeMap<String, ManifestV2>,
    priority: &BTreeSet<String>,
) -> Resolution {
    let mut blocked: BTreeMap<String, Diagnostic> = BTreeMap::new();

    // 1. Duplicate service providers: the lexicographically first provider
    //    wins and the duplicate plugin is blocked.
    let mut owners: BTreeMap<ServiceId, Vec<String>> = BTreeMap::new();
    for (name, manifest) in manifests {
        for raw in &manifest.provides.services {
            match raw.parse::<ServiceId>() {
                Ok(service) => owners.entry(service).or_default().push(name.clone()),
                Err(_) => {
                    blocked.insert(
                        name.clone(),
                        dependency_diagnostic(
                            name,
                            format!("invalid provided service identifier `{raw}`"),
                        ),
                    );
                }
            }
        }
    }
    for (service, providers) in &owners {
        if providers.len() < 2 {
            continue;
        }
        for duplicate in providers.iter().skip(1) {
            blocked.entry(duplicate.clone()).or_insert_with(|| {
                dependency_diagnostic(
                    duplicate,
                    format!("service `{service}` is also provided by `{}`", providers[0]),
                )
            });
        }
    }

    // 2. Propagate blocking through required services until stable.
    loop {
        let mut changed = false;
        for (name, manifest) in manifests {
            if blocked.contains_key(name) {
                continue;
            }
            for raw in &manifest.requires.services {
                let Ok(service) = raw.parse::<ServiceId>() else {
                    blocked.insert(
                        name.clone(),
                        dependency_diagnostic(
                            name,
                            format!("invalid required service identifier `{raw}`"),
                        ),
                    );
                    changed = true;
                    break;
                };
                match owners.get(&service) {
                    None => {
                        blocked.insert(
                            name.clone(),
                            dependency_diagnostic(
                                name,
                                format!("required service `{service}` is not provided"),
                            ),
                        );
                        changed = true;
                        break;
                    }
                    Some(providers) => {
                        if providers.iter().all(|provider| blocked.contains_key(provider)) {
                            blocked.insert(
                                name.clone(),
                                dependency_diagnostic(
                                    name,
                                    format!("required service `{service}` provider is blocked"),
                                ),
                            );
                            changed = true;
                            break;
                        }
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }

    // 3. Build the provider map and dependency edges from surviving plugins.
    let mut providers = BTreeMap::new();
    for (service, service_providers) in &owners {
        if let Some(provider) =
            service_providers.iter().find(|provider| !blocked.contains_key(*provider))
        {
            providers.insert(service.clone(), provider.clone());
        }
    }

    let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut incoming: BTreeMap<String, usize> = BTreeMap::new();
    for name in manifests.keys() {
        if blocked.contains_key(name) {
            continue;
        }
        edges.insert(name.clone(), BTreeSet::new());
        incoming.insert(name.clone(), 0);
    }

    for (name, manifest) in manifests {
        if blocked.contains_key(name) {
            continue;
        }
        let mut required: Vec<&String> = manifest
            .requires
            .services
            .iter()
            .filter_map(|raw| raw.parse::<ServiceId>().ok())
            .filter_map(|service| providers.get(&service))
            .filter(|provider| *provider != name)
            .collect();
        let mut optional: Vec<&String> = manifest
            .requires
            .optional_services
            .iter()
            .filter_map(|raw| raw.parse::<ServiceId>().ok())
            .filter_map(|service| providers.get(&service))
            .filter(|provider| *provider != name)
            .collect();
        required.sort();
        required.dedup();
        optional.sort();
        optional.dedup();
        for provider in required.into_iter().chain(optional) {
            if edges.get_mut(provider).is_some_and(|dependents| dependents.insert(name.clone())) {
                *incoming.get_mut(name).expect("known plugin") += 1;
            }
        }
    }

    // 4. Kahn's algorithm with a sorted ready set for deterministic output.
    let mut ready: BTreeSet<String> =
        incoming.iter().filter(|(_, count)| **count == 0).map(|(name, _)| name.clone()).collect();
    let mut order = Vec::with_capacity(ready.len());
    while let Some(name) =
        ready.iter().min_by_key(|name| (!priority.contains(*name), (*name).clone())).cloned()
    {
        ready.remove(&name);
        order.push(name.clone());
        if let Some(dependents) = edges.get(&name) {
            for dependent in dependents {
                let count = incoming.get_mut(dependent).expect("known dependent");
                *count -= 1;
                if *count == 0 {
                    ready.insert(dependent.clone());
                }
            }
        }
    }

    // 5. Anything left is in, or depends on, a cycle.
    for name in sorted_names(manifests) {
        if blocked.contains_key(&name) || order.contains(&name) {
            continue;
        }
        blocked.insert(
            name.clone(),
            dependency_diagnostic(&name, "dependency cycle prevents activation".to_owned()),
        );
    }

    let mut blocked: Vec<Diagnostic> = blocked.into_values().collect();
    blocked.sort_by(|left, right| {
        left.plugin
            .cmp(&right.plugin)
            .then(left.code.cmp(&right.code))
            .then(left.message.cmp(&right.message))
    });

    Resolution { order, providers, blocked }
}

fn sorted_names(manifests: &BTreeMap<String, ManifestV2>) -> Vec<String> {
    manifests.keys().cloned().collect()
}

fn dependency_diagnostic(plugin: &str, message: String) -> Diagnostic {
    Diagnostic::new(ErrorClass::Dependency, "plugin.dependency", DiagnosticPhase::Resolve, message)
        .with_plugin(plugin)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ee_plugin_api::manifest::{
        Activation, HostTarget, ManifestV2, PluginKind, Provides, Requires,
    };

    use super::*;

    fn manifest(name: &str, requires: &[&str], optional: &[&str], provides: &[&str]) -> ManifestV2 {
        ManifestV2 {
            manifest_version: 2,
            name: name.to_owned(),
            version: "1.0.0".to_owned(),
            api: "ee-plugin/2".to_owned(),
            targets: vec![HostTarget::Editor],
            kind: PluginKind::Builtin,
            entry: format!("builtin:{name}"),
            launch: Default::default(),
            activation: Activation { on_startup: true, ..Default::default() },
            requires: Requires {
                services: requires.iter().map(|value| (*value).to_owned()).collect(),
                optional_services: optional.iter().map(|value| (*value).to_owned()).collect(),
            },
            provides: Provides {
                services: provides.iter().map(|value| (*value).to_owned()).collect(),
            },
            permissions: Default::default(),
            limits: Default::default(),
            contributes: Default::default(),
            config: Default::default(),
        }
    }

    fn set(manifests: Vec<ManifestV2>) -> BTreeMap<String, ManifestV2> {
        manifests.into_iter().map(|manifest| (manifest.name.clone(), manifest)).collect()
    }

    #[test]
    fn orders_dependencies_before_dependents() {
        let resolution = resolve(&set(vec![
            manifest("c", &["svc.b@1"], &[], &[]),
            manifest("b", &["svc.a@1"], &[], &["svc.b@1"]),
            manifest("a", &[], &[], &["svc.a@1"]),
        ]));
        assert!(resolution.blocked.is_empty());
        assert_eq!(resolution.order, vec!["a", "b", "c"]);
    }

    #[test]
    fn deterministic_for_independent_plugins() {
        let manifests = set(vec![
            manifest("z", &[], &[], &[]),
            manifest("a", &[], &[], &[]),
            manifest("m", &[], &[], &[]),
        ]);
        assert_eq!(resolve(&manifests).order, vec!["a", "m", "z"]);
    }

    #[test]
    fn missing_required_service_blocks_plugin() {
        let resolution = resolve(&set(vec![manifest("demo", &["svc.missing@1"], &[], &[])]));
        assert!(resolution.order.is_empty());
        assert_eq!(resolution.blocked.len(), 1);
        assert!(resolution.blocked[0].message.contains("not provided"));
    }

    #[test]
    fn missing_optional_service_is_allowed() {
        let resolution = resolve(&set(vec![manifest("demo", &[], &["svc.optional@1"], &[])]));
        assert_eq!(resolution.order, vec!["demo"]);
        assert!(resolution.blocked.is_empty());
    }

    #[test]
    fn cycle_blocks_every_member() {
        let resolution = resolve(&set(vec![
            manifest("a", &["svc.b@1"], &[], &["svc.a@1"]),
            manifest("b", &["svc.a@1"], &[], &["svc.b@1"]),
        ]));
        assert!(resolution.order.is_empty());
        assert_eq!(resolution.blocked.len(), 2);
        assert!(resolution.blocked[0].message.contains("cycle"));
    }

    #[test]
    fn builtin_priority_wins_among_ready_plugins() {
        let manifests = set(vec![manifest("a", &[], &[], &[]), manifest("z", &[], &[], &[])]);
        let priority = BTreeSet::from(["z".to_owned()]);
        let resolution = resolve_with_priority(&manifests, &priority);
        assert_eq!(resolution.order, vec!["z", "a"]);
    }

    #[test]
    fn duplicate_provider_blocks_second_provider() {
        let resolution = resolve(&set(vec![
            manifest("a", &[], &[], &["svc.shared@1"]),
            manifest("b", &[], &[], &["svc.shared@1"]),
        ]));
        assert_eq!(resolution.order, vec!["a"]);
        assert_eq!(resolution.blocked.len(), 1);
        assert_eq!(resolution.blocked[0].plugin.as_deref(), Some("b"));
        assert!(resolution.blocked[0].message.contains("also provided"));
    }

    #[test]
    fn dependent_of_blocked_provider_is_blocked() {
        let resolution = resolve(&set(vec![
            manifest("a", &["svc.missing@1"], &[], &["svc.shared@1"]),
            manifest("b", &[], &[], &[]),
            manifest("c", &["svc.shared@1"], &[], &[]),
        ]));
        assert_eq!(resolution.order, vec!["b"]);
        let blocked: Vec<_> =
            resolution.blocked.iter().filter_map(|item| item.plugin.as_deref()).collect();
        assert_eq!(blocked, vec!["a", "c"]);
    }
}

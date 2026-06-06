use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppProject {
    pub api_version: String,
    pub kind: String,
    pub metadata: AppProjectMetadata,
    pub spec: AppProjectSpec,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppProjectMetadata {
    pub name: String,
    pub namespace: String,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppProjectSpec {
    pub destinations: BTreeSet<AppProjectDestination>,
    pub cluster_resource_whitelist: BTreeSet<AppProjectClusterResourceWhitelist>,
    pub source_repos: BTreeSet<String>,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize, Eq, Hash, Ord, PartialOrd)]
#[serde(rename_all = "camelCase")]
pub struct AppProjectDestination {
    pub name: String,
    pub namespace: String,
    pub server: String,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize, Eq, Hash, Ord, PartialOrd)]
#[serde(rename_all = "camelCase")]
pub struct AppProjectClusterResourceWhitelist {
    pub group: String,
    pub kind: String,
}

impl AppProject {
    pub fn new(name: String, namespace: String) -> AppProject {
        return AppProject {
            api_version: "argoproj.io/v1alpha1".to_string(),
            kind: "AppProject".to_string(),
            metadata: AppProjectMetadata { name, namespace },
            spec: AppProjectSpec {
                destinations: BTreeSet::new(),
                source_repos: BTreeSet::new(),
                cluster_resource_whitelist: BTreeSet::new(),
            },
        };
    }
}

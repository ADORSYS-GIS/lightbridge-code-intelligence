//! External systems the control plane owns the credentials for, so the untrusted per-task Job never
//! does: GitHub (App auth + per-task token mint + review write-back), GitLab (per-project token +
//! review write-back), Bitbucket (per-repo App Password + review write-back), Kubernetes (Job
//! manifests), and Neo4j (structural-graph writes).

pub(crate) mod bitbucket;
pub(crate) mod github;
pub(crate) mod gitlab;
pub(crate) mod k8s;
pub(crate) mod neo4j;
pub(crate) mod platform;

export const PROTOCOL_VERSION = "resync.v1";

export const workspaceStates = Object.freeze([
  "MATERIALIZING",
  "CURRENT",
  "REMOTE_AHEAD",
  "WAITING_FOR_LEASES",
  "RECONCILING",
  "CONFLICTED",
  "OFFLINE",
  "FAILED"
]);

export const durabilityStates = Object.freeze([
  "NO_LOCAL_CHANGE",
  "DIRTY_UNCHECKPOINTED",
  "COMMITTED_UNPUBLISHED",
  "PUBLISHING",
  "REMOTELY_DURABLE"
]);

export function assertCatalog(value) {
  if (!value || value.protocol !== PROTOCOL_VERSION || !Number.isInteger(value.catalog_generation)) {
    throw new Error("server returned an incompatible catalog");
  }
  if (!Array.isArray(value.projects)) throw new Error("catalog.projects must be an array");
  value.projects = value.projects.map(input => {
    const project = { ...input };
    // Read the engineering-preview single-branch catalog long enough for a
    // rolling hosted/client upgrade. New clients never emit this shape.
    project.default_branch ||= project.sync_branch;
    if (!project.advertised_heads) {
      project.advertised_heads = project.advertised_head_oid
        ? [{ name: project.default_branch, oid: project.advertised_head_oid }]
        : [];
    }
    for (const field of ["project_id", "remote_url", "default_branch"]) {
      if (typeof project[field] !== "string" || !project[field]) throw new Error(`catalog project is missing ${field}`);
    }
    if (!Array.isArray(project.advertised_heads)) throw new Error("catalog project advertised_heads must be an array");
    const names = new Set();
    for (const head of project.advertised_heads) {
      if (!head || typeof head.name !== "string" || !head.name || names.has(head.name)) {
        throw new Error("catalog project has an invalid or duplicate branch name");
      }
      if (!/^[0-9a-f]{40,64}$/.test(head.oid)) throw new Error("catalog project has an invalid advertised head OID");
      names.add(head.name);
    }
    delete project.sync_branch;
    delete project.advertised_head_oid;
    return project;
  });
  return value;
}

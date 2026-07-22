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
  for (const project of value.projects) {
    for (const field of ["project_id", "remote_url", "sync_branch"]) {
      if (typeof project[field] !== "string" || !project[field]) throw new Error(`catalog project is missing ${field}`);
    }
    if (project.advertised_head_oid != null && !/^[0-9a-f]{40,64}$/.test(project.advertised_head_oid)) {
      throw new Error("catalog project has an invalid advertised_head_oid");
    }
  }
  return value;
}

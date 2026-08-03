pub mod cli;
pub mod config;
pub mod credentials;
pub mod daemon;
pub mod git_state;
mod hook_dispatcher;
pub mod identity;
pub mod named_lease;
pub mod peer;
pub mod process;
pub mod protocol;
pub mod provider;
pub mod publication;
pub mod remote;
pub mod rpc;
pub mod state;
pub mod transaction;
pub mod workspace;

pub use protocol::{
    Catalog, CatalogProject, NamedLease, NamedLeaseListResponse, NamedLeaseMutationResponse,
    NamedLeaseOutcome, NamedLeasePolicy, PROTOCOL_VERSION,
};

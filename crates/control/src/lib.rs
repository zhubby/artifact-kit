pub mod api;
pub mod auth;
pub mod crypto;
pub mod error;
pub mod grpc;
pub mod ota;
pub mod pki;
pub mod recipe;
pub mod scheduler;
pub mod settings;
pub mod source;
pub mod state;
pub mod storage;

pub mod protocol {
    tonic::include_proto!("artifactkit.agent.v1");
}

// These imported modules are generated by build.rs from protobuf
// definitions in /protobuf/

// The allows are to avoid clippy warnings in the generated code, see:
// https://github.com/stepancheg/rust-protobuf/pull/332
#[allow(bare_trait_objects)]
#[allow(renamed_and_removed_lints)]
mod raft;
#[allow(bare_trait_objects)]
#[allow(renamed_and_removed_lints)]
mod raft_grpc;

pub use self::raft::*;
pub use self::raft_grpc::*;

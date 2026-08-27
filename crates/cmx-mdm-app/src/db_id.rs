//! 请求库路由——已收编至 `cmx-engine-kit::dbid`（唯一真源）。
//!
//! 本模块保留为 re-export shim：handlers 既有 `crate::db_id::resolve_db_id*` 引用零改动。
//! 真源见 `../cmx-container/crates/libs/cmx-engine-kit/src/dbid.rs`。

pub use cmx_engine_kit::dbid::*;

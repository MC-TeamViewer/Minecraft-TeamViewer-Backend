pub mod admin;
pub mod cert;
pub mod compress;
pub mod config;
pub mod frame;
pub mod metrics;
pub mod proto;
pub mod protocol_compat;
pub mod proxy_ip;
pub mod quic_transport;
pub mod relationship_store;
pub mod relay;
#[cfg(feature = "memory-debug")]
pub mod resource_debug;
pub mod tab_history;
pub mod transport;
pub mod web;
pub mod web_transport;

#[cfg(feature = "memory-debug")]
#[global_allocator]
static MEMORY_DEBUG_ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(feature = "memory-debug")]
#[allow(non_upper_case_globals)]
#[unsafe(export_name = "_rjem_malloc_conf")]
pub static memory_debug_malloc_conf: &[u8] = b"prof:true,prof_active:true,lg_prof_sample:19\0";

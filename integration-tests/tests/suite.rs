// Allocator parity with the production binary (node/bin/src/main.rs): the load benches
// allocate and free millions of small objects per second across threads — glibc malloc's
// arena contention at those rates costs double-digit percent and distorts every number.
#[cfg(target_family = "unix")]
#[global_allocator]
static GLOBAL_ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

mod consensus_node;
mod load;
mod node;
mod protocol;
mod rpc;
mod upgrade;

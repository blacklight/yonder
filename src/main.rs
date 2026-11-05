// mimalloc as the global allocator: the default glibc malloc's
// per-thread arenas hoard freed pages from JSON/federation churn and
// show up as permanently elevated RSS. mimalloc reclaims aggressively.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    yonder::cli::main().await
}

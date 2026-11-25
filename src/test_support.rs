//! Test-only helpers shared by modules.

use std::sync::OnceLock;

/// songbird's global mixer scheduler is spawned with `tokio::spawn` on whichever runtime first
/// asks for it. Each `#[tokio::test]` has its own runtime, so when the test that created it ends,
/// the scheduler dies and any later test that opens a voice driver panics with `SendError`
/// (which one depends on test order, so it shows up in CI only). Create it once on a runtime
/// that lives for the whole test process. Call this before building a `Songbird` in a test.
pub fn songbird_scheduler() {
    static READY: OnceLock<()> = OnceLock::new();
    READY.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("test-songbird-scheduler".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .build()
                    .expect("songbird test runtime");
                rt.block_on(async {
                    let _ = songbird::driver::get_default_scheduler();
                    let _ = tx.send(());
                    // Keep the runtime (and the scheduler task on it) alive forever.
                    std::future::pending::<()>().await;
                });
            })
            .expect("songbird test thread");
        rx.recv().expect("songbird scheduler ready");
    });
}

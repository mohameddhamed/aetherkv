use aetherkv::{Engine, Options};
use std::env;
use std::process::{Command, Stdio};
use tempfile::tempdir;

const CHILD_ENV_VAR: &str = "AETHERKV_CRASH_TEST_CHILD";
const NUM_WRITES: usize = 2000;

#[test]
fn test_sigkill_crash_consistency() {
    if env::var(CHILD_ENV_VAR).is_ok() {
        run_child_worker();
        return;
    }

    let dir = tempdir().unwrap();
    let db_path = dir.path().to_path_buf();

    // 1. Spawn child process that performs heavy writes with WAL sync
    let exe = env::current_exe().unwrap();
    let mut child = Command::new(exe)
        .arg("--nocapture")
        .arg("test_sigkill_crash_consistency")
        .env(CHILD_ENV_VAR, "1")
        .env("AETHERKV_DB_DIR", db_path.to_str().unwrap())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn child worker");

    // 2. Let child run for a moment, then abruptly terminate it with SIGKILL
    std::thread::sleep(std::time::Duration::from_millis(60));

    unsafe {
        #[cfg(unix)]
        libc::kill(child.id() as libc::pid_t, libc::SIGKILL);
    }

    let _ = child.wait();

    // 3. Re-open engine on the crashed directory
    let mut opts = Options::new(&db_path);
    opts.sync_wal_on_write = true;

    let engine = Engine::open(opts).expect("Engine recovery must succeed after abrupt SIGKILL");

    // 4. Verify that all recovered entries are valid and uncorrupted
    let mut recovered_count = 0;
    for i in 0..NUM_WRITES {
        let key = format!("durability_key_{:06}", i);
        if let Ok(Some(val)) = engine.get(key.as_bytes()) {
            let expected_val = format!("durability_val_{:06}", i);
            assert_eq!(val, expected_val.as_bytes());
            recovered_count += 1;
        }
    }

    println!(
        "Crash recovery verified: recovered {} writes committed before SIGKILL",
        recovered_count
    );
    assert!(
        recovered_count > 0,
        "Expected at least some writes to be recovered"
    );
}

fn run_child_worker() {
    let db_dir = env::var("AETHERKV_DB_DIR").unwrap();
    let mut opts = Options::new(db_dir);
    opts.sync_wal_on_write = true; // Each write is synced to WAL

    let engine = Engine::open(opts).unwrap();
    for i in 0..NUM_WRITES {
        let key = format!("durability_key_{:06}", i);
        let val = format!("durability_val_{:06}", i);
        engine.put(key.as_bytes(), val.as_bytes()).unwrap();
    }
}

use aetherkv::{Engine, Options};
use hdrhistogram::Histogram;
use rand::Rng;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::tempdir;

fn main() {
    println!("==========================================================================");
    println!(" AetherKV Open-Loop Poisson Latency Benchmark (Coordinated Omission Free) ");
    println!("==========================================================================");

    let dir = tempdir().unwrap();
    let opts = Options::new(dir.path());
    let engine = Engine::open(opts).expect("Engine open failed");

    // Benchmark parameters
    let target_iops = 100_000.0; // 100K ops/sec per worker thread
    let duration = Duration::from_secs(3);
    let num_threads = 4;

    let stop_signal = Arc::new(AtomicBool::new(false));
    let total_ops = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();
    let start_time = Instant::now();

    for thread_id in 0..num_threads {
        let eng = Arc::clone(&engine);
        let stop = Arc::clone(&stop_signal);
        let ops_counter = Arc::clone(&total_ops);

        handles.push(thread::spawn(move || {
            let mut hist = Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).unwrap();
            let mut rng = rand::thread_rng();

            let target_interval_micros = 1_000_000.0 / target_iops;
            let mut scheduled_time = Instant::now();
            let mut local_ops = 0u64;

            while !stop.load(Ordering::Relaxed) {
                // Generate Poisson inter-arrival delta: -ln(U) * interval
                let u: f64 = rng.gen_range(0.0001..1.0);
                let delta_micros = (-u.ln() * target_interval_micros).max(1.0) as u64;
                scheduled_time += Duration::from_micros(delta_micros);

                let now = Instant::now();
                if scheduled_time > now {
                    thread::sleep(scheduled_time - now);
                }

                // Issue write operation
                let key = format!("poisson_{:02}_{:08}", thread_id, local_ops);
                let val = format!("payload_{:02}_{:08}", thread_id, local_ops);
                eng.put(key.as_bytes(), val.as_bytes()).unwrap();

                // True End-to-End Latency = Completion Time - Scheduled Time
                let completion_time = Instant::now();
                let latency_micros = completion_time
                    .saturating_duration_since(scheduled_time)
                    .as_micros() as u64;

                let _ = hist.record(latency_micros.max(1));
                local_ops += 1;
            }

            ops_counter.fetch_add(local_ops, Ordering::Relaxed);
            hist
        }));
    }

    thread::sleep(duration);
    stop_signal.store(true, Ordering::Relaxed);

    let mut combined_hist = Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).unwrap();
    for h in handles {
        let hist = h.join().unwrap();
        combined_hist.add(&hist).unwrap();
    }

    let elapsed = start_time.elapsed();
    let total_completed = total_ops.load(Ordering::Relaxed);
    let sustained_iops = (total_completed as f64) / elapsed.as_secs_f64();

    println!("\nBenchmark Results (Open-Loop Poisson Arrival):");
    println!("  Total Writes Completed: {}", total_completed);
    println!("  Elapsed Time:           {:.2?}", elapsed);
    println!("  Sustained Throughput:   {:.2} IOPS", sustained_iops);
    println!("--------------------------------------------------------------------------");
    println!("  Latency Percentiles (True Wait + Service Time):");
    println!("    P50   (Median):  {:>6} µs", combined_hist.value_at_quantile(0.50));
    println!("    P90:             {:>6} µs", combined_hist.value_at_quantile(0.90));
    println!("    P99:             {:>6} µs", combined_hist.value_at_quantile(0.99));
    println!("    P99.9:           {:>6} µs", combined_hist.value_at_quantile(0.999));
    println!("    Max:             {:>6} µs", combined_hist.max());
    println!("==========================================================================\n");
}

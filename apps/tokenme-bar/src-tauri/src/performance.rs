use std::collections::HashSet;
use std::io;
use std::sync::{mpsc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(15);

struct Sampler {
    stop: mpsc::Sender<()>,
    thread: JoinHandle<()>,
}

static SAMPLER: OnceLock<Mutex<Option<Sampler>>> = OnceLock::new();

pub fn set_enabled(enabled: bool) -> io::Result<()> {
    if enabled {
        let mut slot = SAMPLER.get_or_init(|| Mutex::new(None)).lock().unwrap_or_else(|e| e.into_inner());
        if slot.as_ref().is_some_and(|sampler| !sampler.thread.is_finished()) { return Ok(()); }
        if let Some(finished) = slot.take() { let _ = finished.thread.join(); }
        let (stop, stop_rx) = mpsc::channel();
        let thread = thread::Builder::new().name("tokenme-performance".into()).spawn(move || sample_loop(stop_rx))?;
        *slot = Some(Sampler { stop, thread });
        crate::logging::info("performance: sampling enabled interval_secs=15 metrics=process_cpu_pct,rss_bytes");
    } else {
        let sampler = SAMPLER.get_or_init(|| Mutex::new(None)).lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(sampler) = sampler {
            let _ = sampler.stop.send(());
            if sampler.thread.join().is_err() {
                crate::logging::error("performance: sampler thread panicked");
            }
            crate::logging::info("performance: sampling disabled");
        }
    }
    Ok(())
}

fn sample_loop(stop: mpsc::Receiver<()>) {
    let main_pid = Pid::from_u32(std::process::id());
    let mut system = System::new();
    loop {
        match sample_process_family(&mut system, main_pid) {
            Some(sample) => crate::logging::info(&format!(
                "performance: sample main_pid={} processes={} main_cpu_pct={:.1} child_cpu_pct_sum={:.1} main_rss_bytes={} child_rss_sum_bytes={}",
                main_pid.as_u32(), sample.processes, sample.main_cpu_pct, sample.child_cpu_pct_sum,
                sample.main_rss_bytes, sample.child_rss_sum_bytes,
            )),
            None => crate::logging::error("performance: current process sample unavailable"),
        }
        match stop.recv_timeout(SAMPLE_INTERVAL) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

struct ProcessFamilySample {
    processes: usize,
    main_cpu_pct: f32,
    child_cpu_pct_sum: f32,
    main_rss_bytes: u64,
    child_rss_sum_bytes: u64,
}

fn sample_process_family(system: &mut System, main_pid: Pid) -> Option<ProcessFamilySample> {
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    system.process(main_pid)?;
    let mut family = HashSet::from([main_pid]);
    loop {
        let mut found_child = false;
        for (pid, process) in system.processes() {
            if process.parent().is_some_and(|parent| family.contains(&parent)) && family.insert(*pid) {
                found_child = true;
            }
        }
        if !found_child { break; }
    }
    let pids: Vec<_> = family.iter().copied().collect();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&pids),
        true,
        ProcessRefreshKind::nothing().with_cpu().with_memory(),
    );
    let main = system.process(main_pid)?;
    let mut child_cpu_pct_sum = 0.0;
    let mut child_rss_sum_bytes = 0u64;
    for pid in family.iter().filter(|pid| **pid != main_pid) {
        if let Some(process) = system.process(*pid) {
            child_cpu_pct_sum += process.cpu_usage();
            child_rss_sum_bytes = child_rss_sum_bytes.saturating_add(process.memory());
        }
    }
    Some(ProcessFamilySample {
        processes: family.len(),
        main_cpu_pct: main.cpu_usage(),
        child_cpu_pct_sum,
        main_rss_bytes: main.memory(),
        child_rss_sum_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_current_process_cpu_and_resident_memory() {
        let pid = Pid::from_u32(std::process::id());
        let mut system = System::new();
        sample_process_family(&mut system, pid).expect("current process is present");
        thread::sleep(Duration::from_millis(250));
        let sample = sample_process_family(&mut system, pid).expect("current process is present");
        assert!(sample.processes >= 1);
        assert!(sample.main_cpu_pct.is_finite());
        assert!(sample.main_rss_bytes > 0);
    }
}

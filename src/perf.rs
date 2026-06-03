use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};

/// Live performance metrics for the F1 panel. CPU and memory are sampled in
/// a background thread; the per-thread latency / fps counters are updated
/// from the capture and preview hot paths directly.
pub struct PerfMetrics {
    pub cpu_percent: AtomicU64,
    pub memory_mb: AtomicU64,
    pub system: Mutex<SystemSnapshot>,
}

#[derive(Default, Clone, Copy)]
pub struct SystemSnapshot {
    pub total_cpu_percent: f32,
    pub used_memory_mb: u64,
    pub total_memory_mb: u64,
}

impl PerfMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            cpu_percent: AtomicU64::new(0),
            memory_mb: AtomicU64::new(0),
            system: Mutex::new(SystemSnapshot::default()),
        })
    }

    pub fn cpu_percent(&self) -> f32 {
        f32::from_bits(self.cpu_percent.load(Ordering::Relaxed) as u32)
    }

    pub fn memory_mb(&self) -> u64 {
        self.memory_mb.load(Ordering::Relaxed)
    }

    pub fn system(&self) -> SystemSnapshot {
        *self.system.lock()
    }
}

pub fn spawn_sampler(metrics: Arc<PerfMetrics>) {
    let pid = Pid::from_u32(std::process::id());
    let _ = std::thread::Builder::new()
        .name("perf-sampler".into())
        .spawn(move || {
            let mut sys = System::new_with_specifics(
                RefreshKind::new()
                    .with_processes(ProcessRefreshKind::everything())
                    .with_memory(sysinfo::MemoryRefreshKind::everything())
                    .with_cpu(sysinfo::CpuRefreshKind::everything()),
            );
            // sysinfo needs two reads to compute CPU%, so prime once.
            sys.refresh_all();
            std::thread::sleep(Duration::from_millis(500));
            loop {
                sys.refresh_cpu_all();
                sys.refresh_memory();
                sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);

                let snapshot = SystemSnapshot {
                    total_cpu_percent: sys.global_cpu_usage(),
                    used_memory_mb: sys.used_memory() / 1024 / 1024,
                    total_memory_mb: sys.total_memory() / 1024 / 1024,
                };
                *metrics.system.lock() = snapshot;

                if let Some(p) = sys.process(pid) {
                    let cpu = p.cpu_usage();
                    metrics
                        .cpu_percent
                        .store(cpu.to_bits() as u64, Ordering::Relaxed);
                    let mem_mb = p.memory() / 1024 / 1024;
                    metrics.memory_mb.store(mem_mb, Ordering::Relaxed);
                }

                std::thread::sleep(Duration::from_secs(1));
            }
        });
}


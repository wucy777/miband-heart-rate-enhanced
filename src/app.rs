//! 全局共享上下文。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::watch;

use crate::ble::{self, SnapshotTx};
use crate::config::{self, Config, Paths};
use crate::logbus::LogBus;

/// 所有任务共享的上下文。克隆成本 = 几个 Arc。
#[derive(Clone)]
pub struct App {
    pub paths: Arc<Paths>,
    pub config: Arc<Mutex<Config>>,
    pub logs: LogBus,
    pub state: SnapshotTx,
    /// 置为 true 表示请求退出程序。
    pub shutdown: watch::Sender<bool>,
    /// 置为 true 表示请求 BLE 任务放下当前设备、重新搜索。
    rescan: Arc<AtomicBool>,
    /// 本次启动是否按「首次使用」处理。
    pub first_run: bool,
    /// 「首次使用」的原因说明。
    pub first_run_reason: &'static str,
}

impl App {
    pub fn new(
        paths: Arc<Paths>,
        config: Config,
        logs: LogBus,
        state: SnapshotTx,
        shutdown: watch::Sender<bool>,
        first_run: bool,
        first_run_reason: &'static str,
    ) -> Self {
        Self {
            paths,
            config: Arc::new(Mutex::new(config)),
            logs,
            state,
            shutdown,
            rescan: Arc::new(AtomicBool::new(false)),
            first_run,
            first_run_reason,
        }
    }

    pub fn config_snapshot(&self) -> Config {
        (*lock(&self.config)).clone()
    }

    /// 改配置并立即落盘，返回改完之后的完整配置。
    pub fn update_config<F>(&self, edit: F) -> Config
    where
        F: FnOnce(&mut Config),
    {
        let updated = {
            let mut guard = lock(&self.config);
            edit(&mut guard);
            (*guard).clone().normalized()
        };
        self.persist(&updated);
        updated
    }

    pub fn persist(&self, config: &Config) {
        if let Err(err) = config::save(&self.paths, config) {
            self.logs.error(err);
        }
    }

    pub fn request_shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    /// 请求 BLE 任务放下当前设备、重新搜索（解绑时用）。
    pub fn request_rescan(&self) {
        self.rescan.store(true, Ordering::SeqCst);
    }

    /// 取出并清空「重新搜索」请求。
    pub fn take_rescan(&self) -> bool {
        self.rescan.swap(false, Ordering::SeqCst)
    }

    /// 更新状态字段（保留心率与设备信息）。
    pub fn update_status(&self, phase: &'static str, detail: impl Into<String>) {
        let detail = detail.into();
        ble::mutate_snapshot(&self.state, |snapshot| {
            snapshot.status.phase = phase;
            snapshot.status.detail = detail;
        });
    }
}

/// 加锁 helper：互斥锁中毒时照样把数据交出来，不让一处 panic 把整个程序拖死。
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

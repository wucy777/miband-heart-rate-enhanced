//! 日志总线：环形历史 + 广播推送。
//!
//! 设计目标（对应项目第一优先级：低开销）：
//! - 历史是一个定长环形缓冲，内存占用恒定。
//! - 推送走 `broadcast`，没有订阅者时直接丢弃，绝不阻塞核心任务。
//! - 慢消费者只会拿到 `Lagged`，由它自己去读历史，不会拖慢生产者。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::sync::broadcast;

/// 保留的历史日志条数。
const HISTORY_CAPACITY: usize = 500;
/// 广播通道容量；超出后最旧的被丢弃。
const BROADCAST_CAPACITY: usize = 256;

#[derive(Clone, Debug, Serialize)]
pub struct LogLine {
    pub seq: u64,
    pub level: &'static str,
    pub msg: String,
}

#[derive(Clone)]
pub struct LogBus {
    inner: Arc<Inner>,
}

struct Inner {
    state: Mutex<State>,
    tx: broadcast::Sender<LogLine>,
}

struct State {
    seq: u64,
    history: VecDeque<LogLine>,
}

impl LogBus {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    seq: 0,
                    history: VecDeque::with_capacity(64),
                }),
                tx,
            }),
        }
    }

    pub fn info(&self, msg: impl Into<String>) {
        self.push("info", msg.into());
    }

    pub fn warn(&self, msg: impl Into<String>) {
        self.push("warn", msg.into());
    }

    pub fn error(&self, msg: impl Into<String>) {
        self.push("error", msg.into());
    }

    fn push(&self, level: &'static str, msg: String) {
        let line = {
            // 互斥锁中毒（某处 panic）时仍然继续工作，避免日志反过来把程序拖死。
            let mut state = lock(&self.inner.state);
            state.seq += 1;
            let line = LogLine {
                seq: state.seq,
                level,
                msg,
            };
            if state.history.len() >= HISTORY_CAPACITY {
                state.history.pop_front();
            }
            state.history.push_back(line.clone());
            line
        };

        // 只在 debug 构建里回显到控制台：release 是 windows_subsystem = "windows"，
        // 没有控制台，println! 反而可能 panic。
        #[cfg(debug_assertions)]
        println!("[{}] {}", line.level, line.msg);

        // 没有订阅者时返回 Err，直接忽略。
        let _ = self.inner.tx.send(line);
    }

    /// 取出序号大于 `since` 的历史日志。
    pub fn since(&self, since: u64) -> Vec<LogLine> {
        let state = lock(&self.inner.state);
        state
            .history
            .iter()
            .filter(|line| line.seq > since)
            .cloned()
            .collect()
    }

    /// 当前最新的日志序号（还没有日志时是 0）。
    pub fn latest_seq(&self) -> u64 {
        let state = lock(&self.inner.state);
        state.seq
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LogLine> {
        self.inner.tx.subscribe()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

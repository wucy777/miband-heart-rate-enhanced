//! BLE 侧：搜索手环、订阅心率广播、断连判定与自动重连。
//!
//! 相比上游版本，这里修掉了两个关键问题：
//!
//! 1. **断连不恢复**。上游用 `while let Some(Ok(hr)) = updates.next().await` 等通知。
//!    手环关掉心率广播后 GATT 连接还在，通知流既不结束也不来数据，`next()` 会永久
//!    阻塞，函数永不返回，外层那圈「重新搜索」的代码根本执行不到 —— 表现就是界面
//!    永远停在最后一帧。
//!
//! 2. **Windows 上不会真的断开**。`bluest` 在 Windows 后端的 `connect_device` /
//!    `disconnect_device` 是空操作，连接由系统托管，只有把 `Device` 及其子对象全部
//!    drop 掉连接才会释放。所以每次重连前都显式 drop，保证拿到的是全新的 GATT 会话。
//!
//! ## 断连是怎么判定的
//!
//! **不能用「有没有收到心率数据」当判据。** 没戴手环时传感器本来就不上报，手环能安静
//! 一分钟以上；按数据超时去重连，一分钟就要把蓝牙拆了重连四次，既费电又毫无意义。
//!
//! 这里用的是三条互相独立的信号：
//!
//! | 信号 | 手段 | 代价 | 含义 |
//! |---|---|---|---|
//! | 蓝牙链路 | `Device::is_connected()` | 本地属性，无 BLE 流量 | 断了就是真断了 |
//! | 订阅是否还在 | `Characteristic::is_notifying()` | 一次真实 CCCD 读 | 设备把订阅清掉了 = 停了广播 |
//! | 设备是否还活着 | `is_notifying()` 报错 | 同上 | 链路已不可用 |
//!
//! 没戴手环时 CCCD 仍然是 Notify，程序**什么都不做**，只把画面置成 `--`；
//! 只有设备真的清了订阅才重新订阅一次（轻量），只有链路断了才完整重连。

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bluest::{Adapter, Device, Uuid};
use futures_lite::stream::StreamExt;
use serde::Serialize;
use tokio::sync::watch;

use crate::app::{lock, App};
use crate::config::Config;

/// 心率服务 Heart Rate Service。
pub const HRS_UUID: Uuid = bluest::btuuid::bluetooth_uuid_from_u16(0x180D);
/// 心率测量特征 Heart Rate Measurement。
pub const HRM_UUID: Uuid = bluest::btuuid::bluetooth_uuid_from_u16(0x2A37);

/// 单次等待的上限。
///
/// 用它把「等通知」「等扫描结果」切成小段，这样每段醒来时都能顺便检查停止请求、
/// 重新搜索请求和蓝牙连接状态；1 秒一次的定时唤醒开销可以忽略，比忙等划算得多。
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// 收不到数据时的探测间隔。
///
/// 只在「已经安静了一段时间」之后才探测，所以数据正常流动时没有任何额外流量。
/// 探测是一次真实的 CCCD 读，10 秒一次的开销远小于重连。
const PROBE_INTERVAL: Duration = Duration::from_secs(10);

/// 一次心率读数。
#[derive(Clone, Debug, Serialize)]
pub struct HeartRate {
    pub value: u16,
    /// 手环是否报告佩戴状态；`None` 表示该手环不支持佩戴检测。
    pub sensor_contact: Option<bool>,
    /// true = 当前没有实时数据（断连中、正在搜索、或者没戴手环）。
    pub stale: bool,
}

impl Default for HeartRate {
    fn default() -> Self {
        Self {
            value: 0,
            sensor_contact: None,
            stale: true,
        }
    }
}

/// 当前运行状态。
#[derive(Clone, Debug, Serialize)]
pub struct Status {
    /// starting / no_adapter / scanning / connecting / connected / retrying / stopped
    pub phase: &'static str,
    pub device_name: Option<String>,
    pub device_id: Option<String>,
    pub detail: String,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            phase: "starting",
            device_name: None,
            device_id: None,
            detail: String::new(),
        }
    }
}

/// 前端一次能拿到的全部实时状态。
#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub heart_rate: HeartRate,
    pub status: Status,
}

pub type SnapshotTx = watch::Sender<Snapshot>;
pub type SnapshotRx = watch::Receiver<Snapshot>;

pub fn channel() -> (SnapshotTx, SnapshotRx) {
    watch::channel(Snapshot {
        heart_rate: HeartRate::default(),
        status: Status::default(),
    })
}

/// 改快照字段并推送给所有等待中的长轮询。
pub fn mutate_snapshot<F>(tx: &SnapshotTx, edit: F)
where
    F: FnOnce(&mut Snapshot),
{
    let mut snapshot = (*tx.borrow()).clone();
    edit(&mut snapshot);
    let _ = tx.send(snapshot);
}

/// 标记「当前没有实时数据」，让画面显示 `--` 而不是停在最后一帧假数据上。
///
/// 停止程序时也会调用它（见 `web::handle_stop`），所以是 `pub`。
pub fn mark_stale(tx: &SnapshotTx) {
    let current = (*tx.borrow()).clone();
    if current.heart_rate.stale {
        return;
    }
    mutate_snapshot(tx, |snapshot| {
        snapshot.heart_rate.stale = true;
        snapshot.heart_rate.sensor_contact = None;
    });
}

fn update_status(tx: &SnapshotTx, phase: &'static str, detail: impl Into<String>) {
    let detail = detail.into();
    mutate_snapshot(tx, |snapshot| {
        snapshot.status.phase = phase;
        snapshot.status.detail = detail;
    });
}

fn set_device_info(tx: &SnapshotTx, name: Option<String>, id: Option<String>) {
    mutate_snapshot(tx, |snapshot| {
        snapshot.status.device_name = name;
        snapshot.status.device_id = id;
    });
}

/// 配置里记录的设备身份。用户点「绑定当前设备」之后才会有值。
#[derive(Clone, Debug)]
pub struct Binding {
    pub id: Option<String>,
    pub name: Option<String>,
}

impl Binding {
    pub fn is_bound(&self) -> bool {
        self.id.is_some()
    }

    /// 这个设备是不是绑定目标。未绑定时任何设备都算匹配。
    fn matches(&self, device_id: &str, device_name: &str) -> bool {
        let Some(bound_id) = self.id.as_deref() else {
            return true;
        };
        if bound_id == device_id {
            return true;
        }
        // Windows 的设备 ID 在重新配对后有可能变，所以再按名字兜一次。
        !device_name.is_empty() && self.name.as_deref() == Some(device_name)
    }

    pub fn describe(&self) -> String {
        match (self.id.as_deref(), self.name.as_deref()) {
            (Some(id), Some(name)) => format!("{name} ({id})"),
            (Some(id), None) => id.to_string(),
            _ => "未绑定".to_string(),
        }
    }
}

enum FindOutcome {
    Found(Device),
    NotFound(String),
    Shutdown,
}

/// BLE 主循环：搜索 -> 连接 -> 收数据 -> 断了就回到搜索。
pub async fn run(app: App, mut shutdown: watch::Receiver<bool>) {
    app.logs.info("蓝牙心率服务已启动");

    loop {
        if shutdown_requested(&shutdown) {
            break;
        }

        let (stale_after, retry_interval, binding) = settings(&app.config);

        let adapter = match acquire_adapter(&app, &mut shutdown).await {
            Some(adapter) => adapter,
            None => break,
        };

        match find_device(&adapter, &app, &binding, &mut shutdown).await {
            FindOutcome::Shutdown => break,
            FindOutcome::NotFound(err) => {
                app.logs.warn(format!("搜索中断：{err}"));
                mark_stale(&app.state);
                update_status(&app.state, "retrying", "正在重新搜索设备");
                if wait_or_shutdown(retry_interval, &mut shutdown).await {
                    break;
                }
            }
            FindOutcome::Found(device) => {
                let device_id = device.id().to_string();
                let device_name = device
                    .name_async()
                    .await
                    .unwrap_or_else(|_| String::new());

                set_device_info(
                    &app.state,
                    Some(display_name(&device_name).to_string()),
                    Some(device_id.clone()),
                );
                update_status(&app.state, "connecting", "正在连接");

                if !binding.is_bound() {
                    app.logs.info(format!(
                        "尚未绑定设备，先使用：{} ({device_id})。要固定下来请在控制台点「绑定当前设备」",
                        display_name(&device_name)
                    ));
                }

                let result =
                    handle_device(&adapter, &device, &app, stale_after, &mut shutdown).await;

                // Windows 上必须把所有 GATT 对象丢干净，连接才会真正释放。
                let _ = adapter.disconnect_device(&device).await;
                drop(device);

                mark_stale(&app.state);

                match result {
                    Ok(()) => {
                        if shutdown_requested(&shutdown) {
                            break;
                        }
                        app.logs.info("已停止接收心率数据");
                    }
                    Err(err) => {
                        app.logs.warn(format!("心率连接中断：{err}"));
                        update_status(&app.state, "retrying", "连接中断，正在重新搜索");
                    }
                }

                if wait_or_shutdown(retry_interval, &mut shutdown).await {
                    break;
                }
            }
        }
    }

    mark_stale(&app.state);
    update_status(&app.state, "stopped", "程序已停止");
    app.logs.info("蓝牙心率服务已停止");
}

/// 读取当前生效的设置。
fn settings(config: &Arc<Mutex<Config>>) -> (Duration, Duration, Binding) {
    let config = lock(config);
    (
        Duration::from_secs(config.stale_after_secs.max(3)),
        Duration::from_secs(config.retry_interval_secs.max(1)),
        Binding {
            id: config.device_id.clone(),
            name: config.device_name.clone(),
        },
    )
}

fn shutdown_requested(shutdown: &watch::Receiver<bool>) -> bool {
    let requested = *shutdown.borrow();
    requested
}

/// 等待一段时间，中途收到停止请求就立刻返回 true。
async fn wait_or_shutdown(total: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    let deadline = Instant::now() + total;
    loop {
        if shutdown_requested(shutdown) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        tokio::time::sleep((deadline - now).min(POLL_INTERVAL)).await;
    }
}

fn display_name(name: &str) -> &str {
    if name.is_empty() {
        "(未知设备)"
    } else {
        name
    }
}

/// 拿到可用的蓝牙适配器；没有就每 10 秒重试一次（上游这里是直接 panic）。
async fn acquire_adapter(app: &App, shutdown: &mut watch::Receiver<bool>) -> Option<Adapter> {
    loop {
        if shutdown_requested(shutdown) {
            return None;
        }

        match Adapter::default().await {
            Some(adapter) => match adapter.wait_available().await {
                Ok(()) => {
                    app.logs.info("蓝牙适配器就绪");
                    return Some(adapter);
                }
                Err(err) => {
                    app.logs.error(format!("蓝牙适配器不可用：{err}"));
                    update_status(&app.state, "no_adapter", "蓝牙适配器不可用，请确认系统蓝牙已打开");
                }
            },
            None => {
                app.logs.error("未找到蓝牙适配器，请确认系统蓝牙已打开");
                update_status(&app.state, "no_adapter", "未找到蓝牙适配器，请确认系统蓝牙已打开");
            }
        }

        app.logs.info("10 秒后重试");
        if wait_or_shutdown(Duration::from_secs(10), shutdown).await {
            return None;
        }
    }
}

/// 找一个可用的心率设备。
///
/// - **已绑定**：只认配置里记下的那台设备，其它蓝牙心率设备一律不连，
///   所以周围有别的设备在广播也不会被带跑。
/// - **未绑定**（首次使用，或者刚点了「解绑当前设备」）：用 `adapter.scan()`，
///   也就是只认正在广播心率的设备，然后交给用户在控制台决定要不要绑定。
async fn find_device(
    adapter: &Adapter,
    app: &App,
    binding: &Binding,
    shutdown: &mut watch::Receiver<bool>,
) -> FindOutcome {
    let logs = &app.logs;

    update_status(&app.state, "scanning", "正在搜索手环");

    // 已绑定时先看系统里已经连上的设备，命中就直接用，省掉一次扫描。
    // 已连接的心率设备不一定还在广播，所以这一步不能省。
    if binding.is_bound() {
        match adapter.connected_devices_with_services(&[HRS_UUID]).await {
            Ok(devices) => {
                for device in devices {
                    let device_id = device.id().to_string();
                    let name = device.name_async().await.unwrap_or_default();
                    if binding.matches(&device_id, &name) {
                        return FindOutcome::Found(device);
                    }
                }
            }
            Err(err) => logs.warn(format!("查询已连接设备失败：{err}")),
        }
        logs.info(format!("开始扫描（只接受已绑定设备 {}）", binding.describe()));
    } else {
        logs.info("开始扫描心率广播设备（尚未绑定，发现即可用）");
    }

    if !binding.is_bound() {
        let mut scan = match adapter.scan(&[HRS_UUID]).await {
            Ok(scan) => scan,
            Err(err) => return FindOutcome::NotFound(err.to_string()),
        };
        loop {
            if shutdown_requested(shutdown) {
                return FindOutcome::Shutdown;
            }
            if app.take_rescan() {
                return FindOutcome::NotFound("收到重新搜索请求".to_string());
            }
            match tokio::time::timeout(POLL_INTERVAL, scan.next()).await {
                Ok(Some(advertising)) => {
                    let device = advertising.device;
                    let device_id = device.id().to_string();
                    let name = device.name_async().await.unwrap_or_default();
                    logs.info(format!(
                        "发现设备：{} ({device_id})",
                        display_name(&name)
                    ));
                    return FindOutcome::Found(device);
                }
                Ok(None) => return FindOutcome::NotFound("扫描流已结束".to_string()),
                Err(_elapsed) => {}
            }
        }
    } else {
        let mut scan = match adapter.discover_devices(&[HRS_UUID]).await {
            Ok(scan) => scan,
            Err(err) => return FindOutcome::NotFound(err.to_string()),
        };
        // 扫描流可能重复上报同一个设备，去重一下免得日志被刷屏。
        let mut seen: HashSet<String> = HashSet::new();
        loop {
            if shutdown_requested(shutdown) {
                return FindOutcome::Shutdown;
            }
            if app.take_rescan() {
                return FindOutcome::NotFound("收到重新搜索请求".to_string());
            }
            match tokio::time::timeout(POLL_INTERVAL, scan.next()).await {
                Ok(Some(Ok(device))) => {
                    let device_id = device.id().to_string();
                    let name = device.name_async().await.unwrap_or_default();
                    if seen.insert(device_id.clone()) {
                        logs.info(format!(
                            "发现设备：{} ({device_id})",
                            display_name(&name)
                        ));
                    }
                    if binding.matches(&device_id, &name) {
                        if binding.id.as_deref() != Some(device_id.as_str()) {
                            logs.info("（按设备名匹配到已绑定设备）");
                        }
                        return FindOutcome::Found(device);
                    }
                }
                Ok(Some(Err(err))) => return FindOutcome::NotFound(err.to_string()),
                Ok(None) => return FindOutcome::NotFound("扫描流已结束".to_string()),
                Err(_elapsed) => {}
            }
        }
    }
}

/// 连接设备并持续接收心率通知，直到**链路真的断了**才返回错误。
///
/// 两层循环：
/// - 外层「订阅循环」：服务发现只做一次，之后每次进来只重新写一次 CCCD（`notify()`），
///   很轻。
/// - 内层「数据循环」：收数据。收不到数据不会退出，只有蓝牙连接断开、设备对 GATT 读
///   无响应、通知流报错/结束，才会返回错误触发完整重连。
async fn handle_device(
    adapter: &Adapter,
    device: &Device,
    app: &App,
    stale_after: Duration,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<(), String> {
    if !device.is_connected().await {
        adapter
            .connect_device(device)
            .await
            .map_err(|err| err.to_string())?;
    }

    // 服务发现每次连接只做一次：Windows 后端用的是 Uncached，比较重。
    let services = device
        .discover_services_with_uuid(HRS_UUID)
        .await
        .map_err(|err| err.to_string())?;
    let service = services
        .into_iter()
        .next()
        .ok_or_else(|| "设备上没有心率服务 (0x180D)".to_string())?;

    let characteristics = service
        .discover_characteristics_with_uuid(HRM_UUID)
        .await
        .map_err(|err| err.to_string())?;
    let characteristic = characteristics
        .into_iter()
        .next()
        .ok_or_else(|| "心率服务上没有心率测量特征 (0x2A37)".to_string())?;

    loop {
        // 订阅循环：每次只写一次 CCCD。
        let mut updates = characteristic
            .notify()
            .await
            .map_err(|err| err.to_string())?;

        app.logs.info("已订阅心率通知，等待数据");
        update_status(&app.state, "connected", "已连接，正在接收心率数据");

        let mut last_data = Instant::now();
        let mut next_probe = stale_after;
        let mut announced_silence = false;

        loop {
            if shutdown_requested(shutdown) {
                return Ok(());
            }

            match tokio::time::timeout(POLL_INTERVAL, updates.next()).await {
                Ok(Some(Ok(data))) => {
                    last_data = Instant::now();
                    next_probe = stale_after;
                    if announced_silence {
                        announced_silence = false;
                        app.logs.info("重新收到心率数据");
                    }
                    match parse_heart_rate(&data) {
                        Ok(heart_rate) => {
                            let value = heart_rate.value;
                            let suffix = match heart_rate.sensor_contact {
                                Some(true) => "（已佩戴）",
                                Some(false) => "（未佩戴）",
                                None => "",
                            };
                            mutate_snapshot(&app.state, |snapshot| {
                                snapshot.heart_rate = heart_rate;
                            });
                            app.logs.info(format!("心率 {value} bpm{suffix}"));
                        }
                        Err(err) => app.logs.warn(format!("忽略无法解析的数据：{err}")),
                    }
                }
                Ok(Some(Err(err))) => return Err(format!("通知流错误：{err}")),
                Ok(None) => return Err("通知流已结束".to_string()),
                Err(_elapsed) => {
                    // 断连判定看蓝牙连接状态（本地属性读取，没有 BLE 流量），
                    // 而不是看有没有数据 —— 没戴手环时传感器本来就不上报。
                    if !device.is_connected().await {
                        return Err("蓝牙连接已断开".to_string());
                    }
                    if app.take_rescan() {
                        return Err("收到重新搜索请求".to_string());
                    }

                    let silent = last_data.elapsed();
                    if silent < stale_after {
                        continue;
                    }

                    if !announced_silence {
                        // 只在刚进入「没数据」状态时推一次：之后每秒都推的话，
                        // 控制台挂着的状态长轮询会被无谓地唤醒一次，白白多跑请求。
                        announced_silence = true;
                        app.logs.info(format!(
                            "{} 秒没有收到心率数据（蓝牙仍连着，通常是没戴手环），画面显示 --",
                            stale_after.as_secs()
                        ));
                        mark_stale(&app.state);
                        update_status(&app.state, "connected", "已连接，但没有心率数据");
                    }

                    if silent >= next_probe {
                        next_probe = silent + PROBE_INTERVAL;
                        // 真实读一次设备上的 CCCD，一次读区分三种情况：
                        //   还是 Notify -> 只是没数据（没戴手环），什么都不用做
                        //   被清成 None -> 设备那边停了广播，重新订阅一次即可
                        //   读失败      -> 链路已经不可用，交给外层完整重连
                        match characteristic.is_notifying().await {
                            Ok(true) => {}
                            Ok(false) => {
                                app.logs.info("设备已清除通知订阅，重新订阅");
                                break;
                            }
                            Err(err) => return Err(format!("设备无响应：{err}")),
                        }
                    }
                }
            }
        }
    }
}

/// 按蓝牙心率测量特征的格式解析一包数据。
fn parse_heart_rate(data: &[u8]) -> Result<HeartRate, String> {
    let flag = *data.first().ok_or_else(|| "数据为空".to_string())?;

    // bit0：心率值格式，0 = u8，1 = u16。
    let mut value = *data.get(1).ok_or_else(|| "缺少心率字节".to_string())? as u16;
    if flag & 0b0000_0001 != 0 {
        value |= (*data.get(2).ok_or_else(|| "缺少心率高字节".to_string())? as u16) << 8;
    }

    // bit2：是否支持佩戴检测；bit1：佩戴状态。
    let sensor_contact = if flag & 0b0000_0100 != 0 {
        Some(flag & 0b0000_0010 != 0)
    } else {
        None
    };

    Ok(HeartRate {
        value,
        sensor_contact,
        stale: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_u8_format() {
        let heart_rate = parse_heart_rate(&[0b0000_0000, 72]).unwrap();
        assert_eq!(heart_rate.value, 72);
        assert_eq!(heart_rate.sensor_contact, None);
        assert!(!heart_rate.stale);
    }

    #[test]
    fn parse_u16_format_with_contact() {
        // bit0 = u16，bit1 = 已佩戴，bit2 = 支持佩戴检测
        let heart_rate = parse_heart_rate(&[0b0000_0111, 0x2C, 0x01]).unwrap();
        assert_eq!(heart_rate.value, 0x012C);
        assert_eq!(heart_rate.sensor_contact, Some(true));
    }

    #[test]
    fn parse_short_packet_is_error() {
        assert!(parse_heart_rate(&[]).is_err());
        assert!(parse_heart_rate(&[0b0000_0001, 0x2C]).is_err());
    }

    #[test]
    fn unbound_matches_anything() {
        let binding = Binding {
            id: None,
            name: None,
        };
        assert!(!binding.is_bound());
        assert!(binding.matches("AA:BB", "Mi Band"));
    }

    #[test]
    fn bound_matches_by_id_then_name() {
        let binding = Binding {
            id: Some("AA:BB".to_string()),
            name: Some("Mi Smart Band 10".to_string()),
        };
        assert!(binding.is_bound());
        assert!(binding.matches("AA:BB", "别的名字"));
        assert!(binding.matches("CC:DD", "Mi Smart Band 10"));
        assert!(!binding.matches("CC:DD", "别的手环"));
        // 名字未知时不能靠名字误匹配
        assert!(!binding.matches("CC:DD", ""));
    }
}

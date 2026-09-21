//! 配置文件的读写，以及「首次使用」的判定。
//!
//! 规则：
//! - 配置文件与自包含 OBS 页面都放在 exe 同目录，程序可整体移动。
//! - 配置文件不存在、读不出来、或者 JSON 格式错误，一律按首次使用处理：
//!   写回默认配置，并让前端弹出首次使用提示。

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 配置文件名（放在 exe 同目录）。
pub const CONFIG_FILE_NAME: &str = "config.json";
/// 给 OBS 用的自包含页面文件名（放在 exe 同目录）。
pub const OBS_PAGE_FILE_NAME: &str = "obs-heart-rate.html";

/// 默认监听端口。
pub const DEFAULT_PORT: u16 = 3030;

/// 程序配置。
///
/// `#[serde(default)]` 让缺失字段回落到默认值，因此老版本配置文件、或者只写了
/// 一部分字段的配置文件都能正常读取；只有类型不匹配、JSON 语法错误才会判定为损坏。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// HTTP 监听端口（改完需要重启程序才生效）。
    pub port: u16,
    /// 绑定的手环设备 ID。只有用户在控制台点「绑定当前设备」后才会写入。
    pub device_id: Option<String>,
    /// 绑定时记录下来的设备名，用于界面显示和人工核对。
    pub device_name: Option<String>,
    /// 多久收不到心率数据就把画面置为「无数据」（秒）。
    ///
    /// 只影响显示：没戴手环时传感器本来就不上报，这不是断连，不会因此重连。
    pub stale_after_secs: u64,
    /// 真正断连后重新搜索前的等待时间（秒），用于避免空转烧 CPU。
    pub retry_interval_secs: u64,
    /// 首次使用提示是否已被用户确认过。
    pub first_run_notice_shown: bool,
    /// 是否已经在 exe 同目录生成过 OBS 自包含页面。
    pub obs_page_generated: bool,
    /// 生成 OBS 页面时使用的端口；端口变了就要重新生成。
    pub obs_page_port: u16,
    /// 程序启动时是否自动打开控制台页面。
    pub open_browser_on_start: bool,
    /// 点「停止程序」时是否需要二次确认。关闭后点一下就直接停止。
    pub confirm_before_stop: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            device_id: None,
            device_name: None,
            stale_after_secs: 15,
            retry_interval_secs: 3,
            first_run_notice_shown: false,
            obs_page_generated: false,
            obs_page_port: 0,
            open_browser_on_start: true,
            confirm_before_stop: true,
        }
    }
}

impl Config {
    /// 把明显不合理的值收敛到可用范围，避免手改配置文件把程序搞挂。
    pub fn normalized(mut self) -> Self {
        if self.port == 0 {
            self.port = DEFAULT_PORT;
        }
        self.stale_after_secs = self.stale_after_secs.clamp(3, 3600);
        self.retry_interval_secs = self.retry_interval_secs.clamp(1, 3600);

        // 设备 ID 空 = 没绑定，设备名也就没有意义了。
        if is_blank(self.device_id.as_deref()) {
            self.device_id = None;
            self.device_name = None;
        } else if is_blank(self.device_name.as_deref()) {
            self.device_name = None;
        }

        self
    }
}

fn is_blank(value: Option<&str>) -> bool {
    value.map(str::trim).map_or(true, |text| text.is_empty())
}

/// 与本程序相关的几个路径，全部基于 exe 所在目录。
#[derive(Debug, Clone)]
pub struct Paths {
    pub exe_dir: PathBuf,
    pub config: PathBuf,
    pub obs_page: PathBuf,
}

impl Paths {
    pub fn resolve() -> Self {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."));

        Self {
            config: exe_dir.join(CONFIG_FILE_NAME),
            obs_page: exe_dir.join(OBS_PAGE_FILE_NAME),
            exe_dir,
        }
    }
}

/// 加载结果。
pub struct Loaded {
    pub config: Config,
    /// 是否按「首次使用」处理（配置文件缺失或损坏）。
    pub first_run: bool,
    /// 给前端提示用的说明文案。
    pub reason: &'static str,
    /// 首次写入默认配置时的错误（如果有）。
    pub save_error: Option<String>,
}

/// 读取配置；文件缺失或损坏时生成默认配置并写回。
pub fn load_or_init(paths: &Paths) -> Loaded {
    let (config, first_run, reason) = match fs::read_to_string(&paths.config) {
        Ok(text) => match serde_json::from_str::<Config>(&text) {
            Ok(config) => (config.normalized(), false, ""),
            Err(_) => (
                Config::default(),
                true,
                "配置文件格式错误，已按首次使用处理并重置为默认配置",
            ),
        },
        Err(_) => (
            Config::default(),
            true,
            "未找到配置文件，已按首次使用处理并生成默认配置",
        ),
    };

    let save_error = if first_run {
        save(paths, &config).err()
    } else {
        None
    };

    Loaded {
        config,
        first_run,
        reason,
        save_error,
    }
}

/// 把配置写回磁盘。
pub fn save(paths: &Paths, config: &Config) -> Result<(), String> {
    let text = serde_json::to_string_pretty(config).map_err(|err| err.to_string())?;
    fs::write(&paths.config, text)
        .map_err(|err| format!("写入配置文件 {} 失败：{err}", paths.config.display()))
}

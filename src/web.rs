//! HTTP 接口。
//!
//! 路由一览：
//!
//! | 方法 | 路径 | 说明 |
//! |---|---|---|
//! | GET | `/` | 给 OBS 用的画面（通过 URL 访问时） |
//! | GET | `/favicon.ico` | 图标（与 exe 图标同一份，嵌在二进制里） |
//! | GET | `/heartrate` | 长轮询：有新心率才返回，服务端 push，无轮询间隔 |
//! | GET | `/heartrate.js` | 同上，JSONP 形式，作为 `file://` 页面 fetch 被拦时的兜底 |
//! | GET | `/control` | 控制台页面（日志 + 状态 + 设置） |
//! | GET | `/api/state` | 当前状态；带 `?since=` 时长轮询 |
//! | GET | `/api/logs` | 日志；长轮询增量拉取 |
//! | GET | `/api/config` | 读取配置与路径信息 |
//! | POST | `/api/config` | 修改配置 |
//! | POST | `/api/bind` | 把当前连接的设备记为绑定目标 |
//! | POST | `/api/unbind` | 解绑当前设备并重新搜索 |
//! | POST | `/api/stop` | 停止程序 |
//! | POST | `/api/notice-ack` | 确认首次使用提示 |
//! | POST | `/api/regen-obs-page` | 重新生成 OBS 自包含页面 |
//!
//! 所有 POST 都必须带 `x-miband: 1` 请求头。这个头不在 CORS 允许列表里，浏览器对
//! 跨源请求会先发预检，预检失败请求根本发不出去，因此可以挡掉网页对本地接口的
//! CSRF（比如别的网页偷偷 POST `/api/stop` 把程序关掉）。

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use warp::{Filter, Rejection, Reply};

use crate::app::App;
use crate::ble::{HeartRate, Snapshot, Status};
use crate::config::Config;
use crate::logbus::LogLine;

const INDEX_HTML: &str = include_str!("../web/index.html");
const CONTROL_HTML: &str = include_str!("../web/control.html");
/// 图标直接嵌进二进制：多 12 KB，换来浏览器标签页和 exe 文件同一个图标。
const FAVICON: &[u8] = include_bytes!("../assets/icon.ico");

const APP_NAME: &str = "miband-heart-rate";
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 长轮询最长挂起时间（秒）。
const MAX_WAIT_SECS: u64 = 120;

/// 通过 URL 访问 `/` 时用的页面：模板 + favicon 引用。
///
/// favicon 只注入在服务端这一版里 —— 生成的本地文件是被 OBS 当浏览器源加载的，
/// 没有标签页，没必要让它去请求图标。
fn index_page() -> &'static str {
    static PAGE: OnceLock<String> = OnceLock::new();
    PAGE.get_or_init(|| {
        INDEX_HTML.replace(
            crate::obs_page::CONFIG_MARKER,
            "<link rel=\"icon\" href=\"/favicon.ico\" />",
        )
    })
}

/// 组装全部路由。
///
/// 返回类型用 `BoxedFilter<(impl warp::Reply,)>`：`.or()` 会把每个分支的 `Extract`
/// 包成一层嵌套的 `Either<...>`，11 个分支嵌套下来类型根本写不出来，而 `boxed()` 之后
/// 只需要声明「里面是个 Reply」就够了。这是 warp 官方文档里的写法
/// （`warp::filters::BoxedFilter` 的示例）。代价是每个请求多一次虚函数调用，可以忽略。
pub fn routes(app: App) -> warp::filters::BoxedFilter<(impl warp::Reply,)> {
    let index = warp::get()
        .and(warp::path::end())
        .map(|| warp::reply::html(index_page()).into_response());

    let favicon = warp::get()
        .and(warp::path!("favicon.ico"))
        .map(|| {
            warp::reply::with_header(
                warp::reply::with_header(FAVICON, "content-type", "image/x-icon"),
                "cache-control",
                "public, max-age=86400",
            )
            .into_response()
        });

    let control = warp::get()
        .and(warp::path!("control"))
        .map(|| warp::reply::html(CONTROL_HTML).into_response());

    let heartrate = warp::get()
        .and(warp::path!("heartrate"))
        .and(with_app(app.clone()))
        .and_then(handle_heartrate);

    let heartrate_js = warp::get()
        .and(warp::path!("heartrate.js"))
        .and(with_app(app.clone()))
        .and_then(handle_heartrate_js);

    let state = warp::get()
        .and(warp::path!("api" / "state"))
        .and(warp::query::<WaitQuery>())
        .and(with_app(app.clone()))
        .and_then(handle_state);

    let logs = warp::get()
        .and(warp::path!("api" / "logs"))
        .and(warp::query::<LogQuery>())
        .and(with_app(app.clone()))
        .and_then(handle_logs);

    let config_get = warp::get()
        .and(warp::path!("api" / "config"))
        .and(with_app(app.clone()))
        .and_then(handle_config_get);

    let config_post = warp::post()
        .and(warp::path!("api" / "config"))
        .and(mutation_guard())
        .and(warp::body::json())
        .and(with_app(app.clone()))
        .and_then(handle_config_post);

    let stop = warp::post()
        .and(warp::path!("api" / "stop"))
        .and(mutation_guard())
        .and(with_app(app.clone()))
        .and_then(handle_stop);

    let bind = warp::post()
        .and(warp::path!("api" / "bind"))
        .and(mutation_guard())
        .and(with_app(app.clone()))
        .and_then(handle_bind);

    let unbind = warp::post()
        .and(warp::path!("api" / "unbind"))
        .and(mutation_guard())
        .and(with_app(app.clone()))
        .and_then(handle_unbind);

    let notice_ack = warp::post()
        .and(warp::path!("api" / "notice-ack"))
        .and(mutation_guard())
        .and(with_app(app.clone()))
        .and_then(handle_notice_ack);

    let regen = warp::post()
        .and(warp::path!("api" / "regen-obs-page"))
        .and(mutation_guard())
        .and(with_app(app.clone()))
        .and_then(handle_regen_obs_page);

    index
        .or(favicon)
        .or(control)
        .or(heartrate)
        .or(heartrate_js)
        .or(state)
        .or(logs)
        .or(config_get)
        .or(config_post)
        .or(bind)
        .or(unbind)
        .or(stop)
        .or(notice_ack)
        .or(regen)
        .boxed()
}

/// 给只读接口加上跨源放行头。
///
/// OBS 指向的是本地 `file://` 页面，它的 Origin 是 `null`，跨源 fetch 必须靠这个响应头
/// 放行。（`file://` 读**本地文件**才需要 `--allow-file-access-from-files`，跨源 fetch
/// http 不需要。）
///
/// 这里不用 `warp::cors()`：它会把 filter 的 `Extract` 变成不好命名的 `Either` 类型，
/// 而且我们本来也不需要处理预检 —— 没有预检放行，跨源的 POST 反而更难过。
fn allow_any_origin(mut response: warp::reply::Response) -> warp::reply::Response {
    response.headers_mut().insert(
        "access-control-allow-origin",
        warp::http::HeaderValue::from_static("*"),
    );
    response
}

/// 共享 `App` 的过滤器。
fn with_app(app: App) -> impl Filter<Extract = (App,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || app.clone())
}

/// 改动类接口的防跨站请求头。
fn mutation_guard() -> impl Filter<Extract = (), Error = Rejection> + Copy {
    warp::header::exact("x-miband", "1")
}

fn json_response<T: Serialize>(value: &T) -> warp::reply::Response {
    warp::reply::json(value).into_response()
}

// ---------------------------------------------------------------- 心率

/// 长轮询：挂起直到有新数据。
///
/// 这是整套方案延迟最低的做法 —— 服务端一有数据立刻回包，客户端拿到就再挂一次，
/// 中间没有固定轮询间隔，也就不用 WebSocket / SSE 那套额外开销。
async fn handle_heartrate(app: App) -> Result<warp::reply::Response, Rejection> {
    let mut rx = app.state.subscribe();
    let _ = rx.borrow_and_update();
    if rx.changed().await.is_err() {
        return Err(warp::reject::not_found());
    }
    let heart_rate = (*rx.borrow()).heart_rate.clone();
    Ok(allow_any_origin(json_response(&heart_rate)))
}

/// 和 `/heartrate` 等价，只是包成 JSONP，供 `file://` 页面在 fetch 被 CEF 拦掉时兜底。
async fn handle_heartrate_js(app: App) -> Result<warp::reply::Response, Rejection> {
    let mut rx = app.state.subscribe();
    let _ = rx.borrow_and_update();
    if rx.changed().await.is_err() {
        return Err(warp::reject::not_found());
    }
    let heart_rate = (*rx.borrow()).heart_rate.clone();
    let payload = serde_json::to_string(&heart_rate).map_err(|_| warp::reject::not_found())?;
    let body = format!("window.__hr&&window.__hr({payload});");
    Ok(allow_any_origin(
        warp::reply::with_header(
            body,
            "content-type",
            "application/javascript; charset=utf-8",
        )
        .into_response(),
    ))
}

// ---------------------------------------------------------------- 状态

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct WaitQuery {
    /// 带上它就变成阻塞式长轮询（等到下一次变化才回包）。
    since: Option<u64>,
}

#[derive(Debug, Serialize)]
struct StateResponse {
    app: &'static str,
    version: &'static str,
    heart_rate: HeartRate,
    status: Status,
    /// 配置里记下的绑定目标。`status.device_id` 是当前实际连接的设备，
    /// 两者不一致时控制台会提示可以「绑定当前设备」。
    bound_device_id: Option<String>,
    bound_device_name: Option<String>,
}

impl StateResponse {
    fn build(app: &App, snapshot: Snapshot) -> Self {
        let config = app.config_snapshot();
        Self {
            app: APP_NAME,
            version: APP_VERSION,
            heart_rate: snapshot.heart_rate,
            status: snapshot.status,
            bound_device_id: config.device_id,
            bound_device_name: config.device_name,
        }
    }
}

async fn handle_state(query: WaitQuery, app: App) -> Result<warp::reply::Response, Rejection> {
    let mut rx = app.state.subscribe();
    if query.since.is_some() {
        let _ = rx.borrow_and_update();
        let _ = rx.changed().await;
    }
    let snapshot = (*rx.borrow()).clone();
    Ok(allow_any_origin(json_response(&StateResponse::build(
        &app, snapshot,
    ))))
}

// ---------------------------------------------------------------- 日志

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LogQuery {
    since: Option<u64>,
    wait: Option<u64>,
}

#[derive(Debug, Serialize)]
struct LogsResponse {
    lines: Vec<LogLine>,
    latest: u64,
}

async fn handle_logs(query: LogQuery, app: App) -> Result<warp::reply::Response, Rejection> {
    // 程序重启后日志序号会归零，客户端还拿着旧序号的话要把它拉回来，
    // 否则会永远等不到「比它更新」的日志。
    let mut since = query.since.unwrap_or(0);
    if since > app.logs.latest_seq() {
        since = 0;
    }

    let wait = Duration::from_secs(query.wait.unwrap_or(25).min(MAX_WAIT_SECS));
    let deadline = Instant::now() + wait;

    // 先订阅再读历史，避免两者之间产生空档漏掉日志。
    let mut rx = app.logs.subscribe();

    loop {
        let lines = app.logs.since(since);
        if !lines.is_empty() {
            let latest = lines.last().map(|line| line.seq).unwrap_or(since);
            return Ok(json_response(&LogsResponse { lines, latest }));
        }

        let now = Instant::now();
        if now >= deadline {
            return Ok(json_response(&LogsResponse {
                lines: Vec::new(),
                latest: since,
            }));
        }

        match tokio::time::timeout(deadline - now, rx.recv()).await {
            // 有新日志 / 落后太多（去读历史就够了）
            Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
            // 发送端没了，直接回空包，别在这里空转
            Ok(Err(broadcast::error::RecvError::Closed)) => {
                return Ok(json_response(&LogsResponse {
                    lines: Vec::new(),
                    latest: since,
                }));
            }
            // 等超时，回到循环顶部返回空包
            Err(_elapsed) => {}
        }
    }
}

// ---------------------------------------------------------------- 配置

#[derive(Debug, Serialize)]
struct ConfigResponse {
    app: &'static str,
    version: &'static str,
    config: Config,
    config_path: String,
    obs_page_path: String,
    obs_page_exists: bool,
    first_run: bool,
    first_run_reason: &'static str,
}

async fn handle_config_get(app: App) -> Result<warp::reply::Response, Rejection> {
    let response = ConfigResponse {
        app: APP_NAME,
        version: APP_VERSION,
        config: app.config_snapshot(),
        config_path: app.paths.config.display().to_string(),
        obs_page_path: app.paths.obs_page.display().to_string(),
        obs_page_exists: app.paths.obs_page.exists(),
        first_run: app.first_run,
        first_run_reason: app.first_run_reason,
    };
    Ok(json_response(&response))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ConfigPatch {
    port: Option<u16>,
    stale_after_secs: Option<u64>,
    retry_interval_secs: Option<u64>,
    open_browser_on_start: Option<bool>,
    confirm_before_stop: Option<bool>,
}

async fn handle_config_post(
    patch: ConfigPatch,
    app: App,
) -> Result<warp::reply::Response, Rejection> {
    let updated = app.update_config(move |config| {
        if let Some(port) = patch.port {
            if port != 0 {
                config.port = port;
            }
        }
        if let Some(secs) = patch.stale_after_secs {
            config.stale_after_secs = secs;
        }
        if let Some(secs) = patch.retry_interval_secs {
            config.retry_interval_secs = secs;
        }
        if let Some(open) = patch.open_browser_on_start {
            config.open_browser_on_start = open;
        }
        if let Some(confirm) = patch.confirm_before_stop {
            config.confirm_before_stop = confirm;
        }
    });
    app.logs.info("配置已更新");
    Ok(json_response(&updated))
}

// ---------------------------------------------------------------- 动作

/// 改动类接口的统一返回。
#[derive(Debug, Serialize)]
struct ActionResponse {
    ok: bool,
    message: String,
}

impl ActionResponse {
    fn ok(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: message.into(),
        }
    }

    fn failed(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: message.into(),
        }
    }
}

async fn handle_stop(app: App) -> Result<warp::reply::Response, Rejection> {
    app.logs.info("收到停止请求，程序即将退出");

    // 必须在这里同步把「无数据 + 已停止」发出去。
    //
    // OBS 那边挂着一个 /heartrate 长轮询，进程一退这个连接就直接断了，页面只会
    // 看到 fetch 失败、继续显示最后一次心率值。先推 stale=true，长轮询立刻返回，
    // 画面上的数字才会变成 `--`。（不能指望 BLE 任务收尾时再推：它最多要 1 秒才轮到
    // 下一次轮询，而退出看门狗只等 0.8 秒。）
    crate::ble::mark_stale(&app.state);
    app.update_status("stopped", "程序已停止");

    app.request_shutdown();
    Ok(json_response(&ActionResponse::ok("程序即将退出")))
}

/// 把当前正在连接的设备记为绑定目标。
///
/// 不做自动绑定：只有用户明确点了按钮才写进配置，避免「连上谁就绑谁」。
async fn handle_bind(app: App) -> Result<warp::reply::Response, Rejection> {
    let snapshot = (*app.state.borrow()).clone();
    let Some(device_id) = snapshot.status.device_id.clone() else {
        app.logs.warn("绑定失败：当前没有已连接的设备");
        return Ok(json_response(&ActionResponse::failed(
            "当前没有已连接的设备，先让程序连上手环再绑定",
        )));
    };
    let device_name = snapshot.status.device_name.clone();

    app.update_config(|config| {
        config.device_id = Some(device_id.clone());
        config.device_name = device_name.clone();
    });

    let label = match device_name.as_deref() {
        Some(name) => format!("{name} ({device_id})"),
        None => device_id.clone(),
    };
    app.logs.info(format!("已绑定设备：{label}"));
    Ok(json_response(&ActionResponse::ok(format!(
        "已绑定：{label}"
    ))))
}

/// 解绑当前设备，并让 BLE 任务放下它重新搜索。
async fn handle_unbind(app: App) -> Result<warp::reply::Response, Rejection> {
    app.update_config(|config| {
        config.device_id = None;
        config.device_name = None;
    });
    app.logs.warn(
        "已解绑当前设备，正在重新搜索。要换成另一只手环，请先关掉不想绑定的那台的心率广播",
    );
    app.update_status("retrying", "已解绑，正在重新搜索");
    app.request_rescan();
    Ok(json_response(&ActionResponse::ok("已解绑，正在重新搜索")))
}

async fn handle_notice_ack(app: App) -> Result<warp::reply::Response, Rejection> {
    app.update_config(|config| config.first_run_notice_shown = true);
    Ok(json_response(&ActionResponse::ok("已确认")))
}

async fn handle_regen_obs_page(app: App) -> Result<warp::reply::Response, Rejection> {
    let port = app.config_snapshot().port;
    match crate::obs_page::generate(&app.paths, port) {
        Ok(()) => {
            app.update_config(|config| {
                config.obs_page_generated = true;
                config.obs_page_port = port;
            });
            let path = app.paths.obs_page.display().to_string();
            app.logs
                .info(format!("已重新生成 OBS 浏览器源本地 html 文件：{path}"));
            Ok(json_response(&ActionResponse::ok(format!(
                "已重新生成：{path}"
            ))))
        }
        Err(err) => {
            app.logs.error(err.clone());
            Ok(warp::reply::with_status(
                json_response(&ActionResponse::failed(err)),
                warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            )
            .into_response())
        }
    }
}

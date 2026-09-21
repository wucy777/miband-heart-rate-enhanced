//! MiBand Heart Rate for OBS —— 启动、单实例与任务装配。
//!
//! 启动流程：
//! 1. 读 exe 同目录的 `config.json`，缺失或损坏就按首次使用处理并写回默认配置。
//! 2. 需要时生成同目录的 OBS 自包含页面。
//! 3. 抢占 HTTP 端口：抢到就是主实例，抢不到说明已经在跑，只帮忙打开控制台页面。
//! 4. 打开控制台页面（模拟原来「双击 exe 弹出窗口」的体验），核心不绑定任何窗口。

// release 构建不要控制台窗口；debug 保留控制台，方便看日志排错。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod ble;
mod config;
mod logbus;
mod obs_page;
mod web;

use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use app::App;
use config::Paths;
use logbus::LogBus;

#[tokio::main]
async fn main() {
    let paths = Arc::new(Paths::resolve());
    let loaded = config::load_or_init(&paths);
    let logs = LogBus::new();

    if let Some(err) = loaded.save_error.as_ref() {
        logs.error(err.clone());
    }

    let (state_tx, state_rx) = ble::channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let app = App::new(
        paths.clone(),
        loaded.config,
        logs.clone(),
        state_tx,
        shutdown_tx,
        loaded.first_run,
        loaded.reason,
    );

    // 这个接收端必须活着：tokio 的 watch 在「没有接收端」时 `send` 会直接丢弃新值，
    // 那样在第一个网页连上来之前产生的状态更新就全丢了。
    let _state_keepalive = state_rx;

    if app.first_run {
        app.logs.warn(app.first_run_reason);
    }

    ensure_obs_page(&app);

    let port = app.config_snapshot().port;
    let control_url = format!("http://127.0.0.1:{port}/control");
    let socket_addr: SocketAddr = ([127, 0, 0, 1], port).into();

    let server = match warp::serve(web::routes(app.clone()))
        .try_bind_with_graceful_shutdown(socket_addr, wait_for_shutdown(shutdown_rx.clone()))
    {
        Ok((_addr, server)) => server,
        Err(err) => {
            // 端口被占：几乎可以肯定是本程序的另一个实例已经在跑了。
            // 按要求不多开核心，但仍然帮用户把控制台页面打开。
            logs.warn(format!("监听 {socket_addr} 失败：{err}"));
            if port_is_open(socket_addr) {
                logs.info("检测到程序已在运行，只打开控制台页面");
                open_browser(&control_url);
            } else {
                logs.error("端口被占用但连不上，程序退出");
            }
            return;
        }
    };

    app.logs.info(format!(
        "程序目录：{}",
        app.paths.exe_dir.display()
    ));
    app.logs.info(format!("控制台页面：{control_url}"));
    app.logs.info(format!(
        "OBS 自包含页面：{}",
        app.paths.obs_page.display()
    ));

    // 退出看门狗。
    //
    // 控制台页面会挂着长轮询请求，warp 的优雅关闭会一直等这些请求结束，所以不能只靠
    // 优雅关闭来退出。这里在收到停止请求后留 0.8 秒把 HTTP 响应发出去，然后直接结束
    // 进程（本地回环，这点时间足够）。
    //
    // 注意：终态（stale / stopped）是在 web::handle_stop 里同步推送的，不依赖这段时间，
    // 否则 BLE 任务最多要 1 秒才轮到下一次轮询，进程先退就会丢掉终态。
    {
        let rx = shutdown_rx.clone();
        tokio::spawn(async move {
            wait_for_shutdown(rx).await;
            tokio::time::sleep(Duration::from_millis(800)).await;
            std::process::exit(0);
        });
    }

    let ble_task = tokio::spawn(ble::run(app.clone(), shutdown_rx.clone()));

    if app.config_snapshot().open_browser_on_start {
        open_browser(&control_url);
    }

    server.await;

    app.request_shutdown();
    let _ = tokio::time::timeout(Duration::from_secs(2), ble_task).await;
    std::process::exit(0);
}

/// 等到停止信号（或者信号发送端消失）。
async fn wait_for_shutdown(mut shutdown: watch::Receiver<bool>) {
    loop {
        let done = *shutdown.borrow();
        if done {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

/// 首次使用、或者端口变了的时候（重新）生成 OBS 自包含页面。
fn ensure_obs_page(app: &App) {
    let config = app.config_snapshot();
    if config.obs_page_generated && config.obs_page_port == config.port {
        return;
    }

    match obs_page::generate(&app.paths, config.port) {
        Ok(()) => {
            let port = config.port;
            app.update_config(|config| {
                config.obs_page_generated = true;
                config.obs_page_port = port;
            });
            app.logs.info(format!(
                "已生成 OBS 自包含页面：{}",
                app.paths.obs_page.display()
            ));
        }
        Err(err) => app.logs.error(err),
    }
}

fn port_is_open(addr: SocketAddr) -> bool {
    TcpStream::connect_timeout(&addr, Duration::from_millis(600)).is_ok()
}

#[cfg(target_os = "windows")]
fn open_browser(url: &str) {
    use std::os::windows::process::CommandExt;

    // CREATE_NO_WINDOW：不要弹出 cmd 黑框。
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

#[cfg(target_os = "macos")]
fn open_browser(url: &str) {
    let _ = std::process::Command::new("open").arg(url).spawn();
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_browser(url: &str) {
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

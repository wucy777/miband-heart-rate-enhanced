# MiBand Heart Rate for OBS

基于 [Tnze/miband-heart-rate](https://github.com/Tnze/miband-heart-rate) 的 for-OBS 二次开发版本。
原作者：Tnze。遵循 MIT 协议，见 [LICENSE](LICENSE)。

读取小米手环「心率广播」，通过本地 HTTP 提供给 OBS 浏览器源。

- 平台：Windows 10/11、macOS/iOS、Linux（底层 [`bluest`](https://crates.io/crates/bluest)）；发布版只提供 Windows x64。
- 手环：小米手环（9 Pro / 10 均可），需在手环设置里打开「心率广播」。

## 使用

1. 在手环上打开「心率广播」。
2. 双击 `miband-heart-rate.exe`。程序会在自身同目录生成文件，并打开控制台页面
   `http://127.0.0.1:3030/control`。
3. 在 OBS 里添加「浏览器」源，指向 exe 同目录的 `obs-heart-rate.html`。

程序在 exe 同目录生成两个文件：

| 文件 | 说明 |
|---|---|
| `obs-heart-rate.html` | 自包含页面，供 OBS 浏览器源使用。可以移动或重命名，移动后在 OBS 里重新选一次路径即可。 |
| `config.json` | 程序配置文件。必须与程序在同一目录，暂不支持移动。 |

## 配置

`config.json` 缺失或格式错误时按首次使用处理，重置为默认值。

| 字段 | 默认值 | 说明 |
|---|---|---|
| `port` | `3030` | HTTP 监听端口，改完需重启程序 |
| `device_id` | `null` | 绑定的手环设备 ID，`null` = 未绑定 |
| `device_name` | `null` | 绑定时记下的设备名 |
| `stale_after_secs` | `15` | 收不到心率多久把画面显示成 `--`，只影响显示 |
| `retry_interval_secs` | `3` | 蓝牙断开后隔多久重新搜索 |
| `first_run_notice_shown` | `false` | 首次使用提示是否已确认 |
| `obs_page_generated` | `false` | 是否已生成过自包含页面 |
| `obs_page_port` | `0` | 生成页面时使用的端口 |
| `open_browser_on_start` | `true` | 启动时是否自动打开控制台页面 |
| `confirm_before_stop` | `true` | 点「停止程序」是否需要二次确认 |

除 `device_id` / `device_name` 外都可以在控制台页面上改。

## HTTP 接口

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/` | OBS 画面（通过 URL 访问时） |
| GET | `/favicon.ico` | 图标，与 exe 图标同一份 |
| GET | `/heartrate` | 长轮询，返回 `{"value":72,"sensor_contact":true,"stale":false}` |
| GET | `/heartrate.js` | 同上，JSONP 形式 |
| GET | `/control` | 控制台页面 |
| GET | `/api/state` | 当前状态，带 `?since=` 时长轮询 |
| GET | `/api/logs` | 日志长轮询，`?since=<seq>&wait=<秒>` |
| GET | `/api/config` | 读取配置与路径信息 |
| POST | `/api/config` | 修改配置 |
| POST | `/api/bind` | 把当前连接的设备记为绑定目标 |
| POST | `/api/unbind` | 解绑当前设备并重新搜索 |
| POST | `/api/stop` | 停止程序 |
| POST | `/api/notice-ack` | 确认首次使用提示 |
| POST | `/api/regen-obs-page` | 重新生成 OBS 浏览器源本地 html 文件 |

所有 POST 都必须带 `x-miband: 1` 请求头。

## 代码变动

相对上游 `Tnze/miband-heart-rate` 0.1.0：

- 新增 `obs-heart-rate.html` 自包含页面，OBS 浏览器源指向本地文件，不受程序与 OBS 启动顺序影响。
- 新增 `/control` 控制台：实时日志、运行状态、设置与操作按钮；不再创建控制台窗口，核心不绑定窗口；单实例运行。
- 新增手环绑定：`POST /api/bind`、`POST /api/unbind`，绑定信息存入 `config.json`。
- 新增 `config.json`：端口、绑定设备、无数据判定、重连间隔、启动行为、停止确认。
- 新增 exe 图标与版本资源（`assets/icon.ico` + `build.rs`）；同一份图标也通过 `/favicon.ico` 提供给 `/` 与 `/control` 页面。
- 心率通知的可用性判定改为读取蓝牙连接状态（`Device::is_connected`）与设备 CCCD 订阅状态
  （`Characteristic::is_notifying`）；服务发现每次连接只执行一次。
- `/heartrate` 响应由裸数字改为 JSON 对象（`value` / `sensor_contact` / `stale`）。
- 修复上游三处 panic 路径：无蓝牙适配器、适配器不可用、扫描流结束。
- 依赖与构建：`warp` 关闭未使用的默认特性；`futures-lite` 由 2.6 调整为 1.13 与 `bluest` 统一；发布构建启用 LTO 与 strip。
- 源码拆分为 `main` / `app` / `config` / `logbus` / `ble` / `web` / `obs_page` 七个模块。

## 构建

```bash
cargo build --release
```

产物在 `target/release/miband-heart-rate.exe`，单文件、无运行时依赖。

## 协议

MIT，见 [LICENSE](LICENSE)。

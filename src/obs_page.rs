//! 生成给 OBS 用的自包含 HTML 页面。
//!
//! 为什么需要它：OBS 的浏览器源在加载的那一刻如果连不上服务，这个源就会一直停在
//! 失败状态、之后不再重试；而只要加载成功过一次，它就会一直重试同一个地址。所以
//! 「先开 OBS 再开本程序」必然黑屏。
//!
//! 解决办法是让 OBS 指向一个本地 `file://` 页面：本地文件永远加载成功，页面内部再
//! 用长轮询去取心率。这样启动顺序怎么排都不会出问题，而且延迟和直接指向 URL 一样
//! （都是服务端 push，没有轮询间隔）。
//!
//! 页面内容直接复用 `web/index.html`，只在 `<!--HR_CONFIG-->` 处注入一段配置脚本，
//! 保证 OBS 画面只有一份实现、不会两边跑偏。

use std::fs;

use crate::config::Paths;

const TEMPLATE: &str = include_str!("../web/index.html");

/// 模板里的注入点。生成本地页面时注入长轮询地址，通过 URL 访问 `/` 时注入 favicon 引用。
pub const CONFIG_MARKER: &str = "<!--HR_CONFIG-->";

/// 生成页面内容。
pub fn render(port: u16) -> String {
    let config = format!(
        "<script>\n\
         /* 由程序自动生成，勿手改；改动会在下次重新生成时被覆盖。 */\n\
         window.__HR_URL = \"http://127.0.0.1:{port}/heartrate\";\n\
         window.__HR_JSONP = \"http://127.0.0.1:{port}/heartrate.js\";\n\
         window.__HR_LOCAL_FILE = true;\n\
         </script>"
    );
    TEMPLATE.replace(CONFIG_MARKER, &config)
}

/// 把页面写到 exe 同目录。
pub fn generate(paths: &Paths, port: u16) -> Result<(), String> {
    fs::write(&paths.obs_page, render(port))
        .map_err(|err| format!("写入 OBS 页面 {} 失败：{err}", paths.obs_page.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_injects_port() {
        let html = render(3031);
        assert!(html.contains("http://127.0.0.1:3031/heartrate"));
        assert!(html.contains("http://127.0.0.1:3031/heartrate.js"));
        assert!(!html.contains(CONFIG_MARKER));
    }
}

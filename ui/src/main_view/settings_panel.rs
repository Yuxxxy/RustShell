//! 设置面板装配：改用官方 `gpui-component` `Settings` 组件族。
//!
//! 两条必须留意的约束：
//! 1. 官方 `Settings` 根节点是 `h_resizable`，必须有确定高度的父容器，
//!    否则整块塌陷为 0 高（官方自测用例同样是 `div.h(..)` 包裹）。
//! 2. `SettingField` 的取值闭包签名是 `Fn(&App) -> T` / `Fn(T, &mut App)`，
//!    无法借用 `&mut self`，因此统一捕获 `Entity<MainView>`：
//!    getter `me.read(cx)`，setter `me.update(cx, ..)` + `cx.notify()`。
//!
//! 面板关闭后官方组件的 keyed state 随之释放，字段值每帧从 `MainView` 现读，
//! 无回填步骤。

use gpui::{
    div, px, AnyElement, App, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, Styled as _, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    scroll::ScrollableElement as _,
    setting::{NumberFieldOptions, SettingField, SettingGroup, SettingItem, SettingPage, Settings},
    slider::Slider,
    v_flex, Disableable as _, IconName, Sizable as _,
};

use rustshell_core::settings::StartupAction;
use rustshell_core::theme::default_terminal_font;

use crate::theme::to_hsla as hex;

use super::MainView;

/// 打开设置面板时的快照：`取消`/ ESC / 点遮罩 / 右上角 X 一律回滚到此状态， 让「取消」名副其实（此前只有部分字段能回滚）。
pub(super) struct SettingSnapshot {
    pub(super) settings: rustshell_core::settings::Settings,
    pub(super) ai_cfg: rustshell_core::ai::AiConfig,
    /// 预警邮件（SMTP）配置。同样要能「取消即回滚」：用户改了主机名/收件人
    /// 又点了取消，落盘的那份不该被改动。
    pub(super) smtp_cfg: rustshell_core::notify_smtp::SmtpConfig,
}

// 官方`SettingField::default_value`用的重置值。
// 抽成常量而不是散落的字面量，好让`reset_defaults_match_settings_default`
// 这条测试真正起到护栏作用（字面量散落时改了面板、测试仍是绿的）。
const DEFAULT_FONT_SIZE: f64 = 12.0;
const DEFAULT_CONNECT_TIMEOUT_SECS: f64 = 20.0;
const DEFAULT_RETRIES: f64 = 3.0;
/// 重连首个间隔（秒）的默认值（F-4）。与 `core::reconnect::DEFAULT_INTERVAL_SECS`
/// 同源：面板的「重置」按钮拿它当 `default_value`，对不齐就会把用户重置到一个
/// 与全新安装不一致的值。
const DEFAULT_RETRY_INTERVAL_SECS: f64 = rustshell_core::reconnect::DEFAULT_INTERVAL_SECS as f64;

/// i18n 简写：`t("设置")`到`SharedString`（官方组件接受`Into<SharedString>`）
fn t(zh: &'static str) -> SharedString {
    SharedString::from(rustshell_core::i18n::t(zh))
}

/// 设置**页标题**的 i18n：中文同名、英文不同名时用 `tp` 成对给出。
///
/// 词条表按中文原文索引（`match zh_key`），同一个中文只能有一个英文译文。而页标题
/// 与别处的同名文字英文未必同名：「连接」（对话框按钮）是 `Connect`，设置页是
/// `Connections`；「终端」（活动栏面板）是 `Terminals`，设置页是 `Terminal`。
/// 用 `t()` 会让设置页在英文下显示成按钮/面板的名字，故这两处走 `tp`。
fn page_title(zh: &'static str, en: &'static str) -> SharedString {
    SharedString::from(rustshell_core::i18n::tp(zh, en))
}

impl MainView {
    /// 装配官方`Settings`组件。
    pub(super) fn render_settings_pages(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Settings {
        // 读写通道：官方闭包只拿得到`&App`/`&mut App`，故捕获自身实体
        let me = cx.entity();

        // 「外观」页已于 2026-10-20 按用户要求整体撤下：主题仍在活动栏的
        // 主题菜单里切换；背景图功能已整体移除（2026-10-21），
        // `settings.json` 里对应的字段一并清掉。

        // 终端
        //
        // 2026-10-19 用户要求再次精简面板：光标闪烁 / 回滚行数 / 光标样式 /
        // 多行粘贴行为 / 采样间隔全部撤下。值仍由 `core::settings` 决定并落进
        // `settings.json`（现有用户的值继续生效），想改的人直接改配置文件。
        let terminal = {
            // 字体：候选下拉。若当前值不在候选内（用户手改过 settings.json），
            // 追加为额外选项，绝不静默改写用户值。
            let font_options: Vec<(SharedString, SharedString)> = {
                let cur = self.settings.terminal_font_family.clone();
                let mut opts: Vec<(SharedString, SharedString)> =
                    rustshell_core::theme::terminal_font_candidates()
                        .into_iter()
                        .map(|f| (SharedString::from(f), SharedString::from(f)))
                        .collect();
                if !opts.iter().any(|(k, _)| k.as_ref() == cur) && !cur.is_empty() {
                    opts.push((SharedString::from(cur.clone()), SharedString::from(cur)));
                }
                opts
            };
            let me_font_get = me.clone();
            let me_font_set = me.clone();
            let me_size_get = me.clone();
            let me_size_set = me.clone();
            let me_enc_get = me.clone();
            let me_enc_set = me.clone();
            let me_scheme_get = me.clone();
            let me_scheme_set = me.clone();
            let me_log_get = me.clone();
            let me_log_set = me.clone();
            let me_log_dir = me.clone();

            SettingPage::new(page_title("终端", "Terminal"))
                .icon(IconName::SquareTerminal)
                .description(t("字体、配色改动立即生效；编码改动对新建终端生效。"))
                // 配色方案置顶（2026-10-19 用户要求）：这是本页最常被打开的一项，
                // 此前排在字体/字号/行高/光标之后，每次都要滚。
                .group(
                    SettingGroup::new()
                        .item(
                            SettingItem::new(
                                t("配色方案"),
                                SettingField::dropdown(
                                    rustshell_core::theme::TERMINAL_SCHEMES
                                        .iter()
                                        .map(|(k, zh, en)| {
                                            let name: SharedString =
                                                if rustshell_core::i18n::is_en() {
                                                    SharedString::from(*en)
                                                } else {
                                                    SharedString::from(*zh)
                                                };
                                            (SharedString::from(*k), name)
                                        })
                                        .collect::<Vec<_>>(),
                                    move |cx: &App| {
                                        SharedString::from(
                                            me_scheme_get
                                                .read(cx)
                                                .settings
                                                .terminal_color_scheme
                                                .clone(),
                                        )
                                    },
                                    move |val: SharedString, cx: &mut App| {
                                        me_scheme_set.update(cx, |m, cx| {
                                            m.set_terminal_scheme(val.as_ref(), cx)
                                        });
                                    },
                                )
                                .default_value(SharedString::from(
                                    rustshell_core::theme::SCHEME_FOLLOW_THEME,
                                )),
                            )
                            .description(t("跟随主题随深浅色变化；预设与自定义为固定配色。")),
                        )
                        // 自定义配色子区（仅 scheme == custom 时展示）
                        .item(SettingItem::render({
                            let me = me.clone();
                            move |_options, _w, cx| custom_palette_editor(&me, cx)
                        })),
                )
                // 编码：本页唯一一个**「中文乱码」的救火入口** —— 用户来这页
                // 多半就是刚看到满屏乱码，故紧随配色之后。
                .group(
                    SettingGroup::new().item(
                        SettingItem::new(
                            t("编码"),
                            SettingField::dropdown(
                                rustshell_core::theme::TERMINAL_ENCODINGS
                                    .iter()
                                    .map(|(k, name)| {
                                        (SharedString::from(*k), SharedString::from(*name))
                                    })
                                    .collect::<Vec<_>>(),
                                move |cx: &App| {
                                    SharedString::from(
                                        me_enc_get.read(cx).settings.terminal_encoding.clone(),
                                    )
                                },
                                move |val: SharedString, cx: &mut App| {
                                    me_enc_set.update(cx, |m, cx| {
                                        m.settings.terminal_encoding = val.to_string();
                                        cx.notify();
                                    });
                                },
                            )
                            .default_value(SharedString::from("utf-8")),
                        )
                        .description(t("中文乱码时改选 GBK / GB18030；需新建终端生效。")),
                    ),
                )
                .group(SettingGroup::new().items(vec![
                    SettingItem::new(
                        t("字体"),
                        SettingField::dropdown(
                            font_options,
                            move |cx: &App| {
                                SharedString::from(
                                    me_font_get.read(cx).settings.terminal_font_family.clone(),
                                )
                            },
                            move |val: SharedString, cx: &mut App| {
                                me_font_set.update(cx, |m, cx| {
                                    m.settings.terminal_font_family = val.to_string();
                                    cx.notify();
                                });
                            },
                        )
                        .default_value(SharedString::from(default_terminal_font())),
                    )
                    .description(t("等宽字体候选；选择后即应用到已打开的终端。")),
                    SettingItem::new(
                        t("字号"),
                        SettingField::number_input(
                            NumberFieldOptions {
                                min: 8.0,
                                max: 32.0,
                                step: 0.5,
                            },
                            move |cx: &App| me_size_get.read(cx).settings.terminal_font_size as f64,
                            move |val: f64, cx: &mut App| {
                                me_size_set.update(cx, |m, cx| {
                                    m.settings.terminal_font_size = val as f32;
                                    cx.notify();
                                });
                            },
                        )
                        .default_value(DEFAULT_FONT_SIZE),
                    )
                    .description(t("8–32，支持 0.5 步进。")),
                    // 行高（2026-09-28 新增）：字号只改大小、不改疏密，
                    // 觉得「挤」的用户此前无处可调（值硬编码在适配层）。
                    // 复用官方 `Slider`（与采样温度同一模式）：滑块状态持久在
                    // `MainView::line_height_slider`，拖动即写设置并推给已开终端。
                    SettingItem::new(
                        t("行高"),
                        SettingField::render({
                            let slider = self.panel_ui.line_height_slider.clone();
                            let me_lh = me.clone();
                            move |_options, _w, cx| {
                                let cur = me_lh.read(cx).settings.clamped_line_height();
                                let tok = crate::theme::current(cx);
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .w(px(240.))
                                    .child(Slider::new(&slider).w(px(190.)))
                                    .child(
                                        div()
                                            .w(px(34.))
                                            .text_size(px(12.))
                                            .text_color(hex(tok.fg_primary))
                                            .child(SharedString::from(format!("{cur:.2}"))),
                                    )
                                    .into_any_element()
                            }
                        }),
                    )
                    .description(t("1.0–2.0；觉得行距挤就调大。")),
                ]))
                // 会话日志（F-7）
                //
                // 两件事分两块：开关决定**新会话**记不记（已开的会话不回改，
                // 与「编码改动对新建终端生效」同一口径）；目录按钮负责
                // 「开了之后去哪儿看」—— 少了它，用户只能自己猜路径。
                .group(
                    SettingGroup::new()
                        .item(
                            SettingItem::new(
                                t("记录会话日志"),
                                SettingField::switch(
                                    move |cx: &App| {
                                        me_log_get.read(cx).settings.session_log_enabled
                                    },
                                    move |val: bool, cx: &mut App| {
                                        me_log_set.update(cx, |m, cx| {
                                            m.settings.session_log_enabled = val;
                                            cx.notify();
                                        });
                                    },
                                )
                                .default_value(false),
                            )
                            .description(t(
                                "把终端输出原样落盘；对新建会话生效，每条连接可单独覆盖。",
                            )),
                        )
                        .item(SettingItem::render(move |_options, _w, cx| {
                            let tok = crate::theme::current(cx);
                            let dir = rustshell_platform::paths::session_logs_dir();
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    Button::new("log-open-dir")
                                        .small()
                                        .outline()
                                        .label(rustshell_core::i18n::t("打开日志目录"))
                                        .on_click({
                                            let me = me_log_dir.clone();
                                            move |_e, _w, cx: &mut App| {
                                                me.update(cx, |m, cx| {
                                                    m.open_session_log_folder(cx)
                                                });
                                            }
                                        }),
                                )
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(hex(tok.fg_muted))
                                        .child(SharedString::from(dir.display().to_string())),
                                )
                                .into_any_element()
                        })),
                )
        };

        // 连接会话组（2026-09-15 起「连接设置」页并入「连接管理」页，不再单独占一页）
        let session_group = {
            let me_to_get = me.clone();
            let me_to_set = me.clone();
            let me_sw_get = me.clone();
            let me_sw_set = me.clone();
            let me_rt_get = me.clone();
            let me_rt_set = me.clone();
            let me_ri_get = me.clone();
            let me_ri_set = me.clone();
            SettingGroup::new().items(vec![
                SettingItem::new(
                    t("连接超时（秒）"),
                    SettingField::number_input(
                        NumberFieldOptions {
                            min: rustshell_core::ssh::CONNECT_TIMEOUT_MIN_SECS as f64,
                            max: rustshell_core::ssh::CONNECT_TIMEOUT_MAX_SECS as f64,
                            step: 5.0,
                        },
                        move |cx: &App| me_to_get.read(cx).settings.connect_timeout_secs as f64,
                        move |val: f64, cx: &mut App| {
                            me_to_set.update(cx, |m, cx| {
                                m.settings.connect_timeout_secs = val as u64;
                                cx.notify();
                            });
                        },
                    )
                    .default_value(DEFAULT_CONNECT_TIMEOUT_SECS),
                )
                .description(t("握手、认证、建链各阶段上限")),
                SettingItem::new(
                    t("断线自动重连"),
                    SettingField::switch(
                        move |cx: &App| me_sw_get.read(cx).settings.auto_reconnect,
                        move |val: bool, cx: &mut App| {
                            me_sw_set.update(cx, |m, cx| {
                                m.settings.auto_reconnect = val;
                                cx.notify();
                            });
                        },
                    )
                    .default_value(false),
                )
                .description(t("会话意外断开（非用户主动关闭）后自动重连。")),
                SettingItem::new(
                    t("最大重试次数"),
                    SettingField::number_input(
                        NumberFieldOptions {
                            min: 0.0,
                            max: 100.0,
                            step: 1.0,
                        },
                        move |cx: &App| me_rt_get.read(cx).settings.reconnect_max_retries as f64,
                        move |val: f64, cx: &mut App| {
                            me_rt_set.update(cx, |m, cx| {
                                m.settings.reconnect_max_retries = val as u32;
                                cx.notify();
                            });
                        },
                    )
                    .default_value(DEFAULT_RETRIES),
                )
                .description(t("达到该次数后不再重试，等待手动连接。")),
                SettingItem::new(
                    t("重连间隔"),
                    SettingField::number_input(
                        NumberFieldOptions {
                            min: rustshell_core::reconnect::MIN_INTERVAL_SECS as f64,
                            max: rustshell_core::reconnect::MAX_INTERVAL_SECS as f64,
                            step: 1.0,
                        },
                        move |cx: &App| me_ri_get.read(cx).settings.reconnect_interval_secs as f64,
                        move |val: f64, cx: &mut App| {
                            me_ri_set.update(cx, |m, cx| {
                                m.settings.reconnect_interval_secs =
                                    rustshell_core::reconnect::clamp_interval(val as u32);
                                cx.notify();
                            });
                        },
                    )
                    .default_value(DEFAULT_RETRY_INTERVAL_SECS),
                )
                .description(t("首次重试前等待的秒数；之后每次翻倍，最多 60 秒。")),
            ])
        };

        // 文件传输组（F-13）：下载校验开关 + 并发传输数。
        //
        // 不标题（2026-09-17 起设置面板的「组」一律不带标题，见
        // `settings_pages_follow_the_2026_09_17_rework`），靠放置位置与条目文案自明。
        //
        // 并发数**不即时生效**：信号量是在建链时按当时的设置建好的（`spawn_sftp_service`），
        // 改完对已经在跑的传输不起作用 —— 描述里如实写明，免得用户以为这台机器上
        // 「调大没反应」是坏了。
        let sftp_group = {
            let me_verify_get = me.clone();
            let me_verify_set = me.clone();
            let me_conc_get = me.clone();
            let me_conc_set = me.clone();
            SettingGroup::new()
                .item(
                    SettingItem::new(
                        t("下载后校验（SHA-256）"),
                        SettingField::switch(
                            move |cx: &App| me_verify_get.read(cx).settings.sftp_verify_downloads,
                            move |val: bool, cx: &mut App| {
                                me_verify_set.update(cx, |m, cx| {
                                    m.settings.sftp_verify_downloads = val;
                                    cx.notify();
                                });
                            },
                        )
                        .default_value(false),
                    )
                    .description(t("比对大小与摘要；大文件要多花一遍读取时间。"))
                    .keywords(["sftp", "sha256", "verify", "下载", "校验", "摘要"]),
                )
                .item(
                    SettingItem::new(
                        t("SFTP 并发传输数"),
                        SettingField::number_input(
                            NumberFieldOptions {
                                min: rustshell_core::settings::SFTP_MAX_CONCURRENT_MIN as f64,
                                max: rustshell_core::settings::SFTP_MAX_CONCURRENT_MAX as f64,
                                step: 1.0,
                            },
                            move |cx: &App| {
                                me_conc_get.read(cx).settings.sftp_max_concurrent as f64
                            },
                            move |val: f64, cx: &mut App| {
                                me_conc_set.update(cx, |m, cx| {
                                    m.settings.sftp_max_concurrent = val as u32;
                                    cx.notify();
                                });
                            },
                        )
                        .default_value(
                            rustshell_core::settings::DEFAULT_SFTP_MAX_CONCURRENT as f64,
                        ),
                    )
                    .description(t("1–4；对新建立的连接生效，已在途的传输不受影响。"))
                    .keywords([
                        "sftp",
                        "concurrent",
                        "transfer",
                        "并发",
                        "传输",
                        "队列",
                    ]),
                )
        };

        // 代理（F-10）
        //
        // 配置项直接读写 `settings.proxy`（与 SMTP 页同一口径：`SettingField` 的
        // getter/setter 就是 `&App`/`&mut App`，不需要额外的输入实体）；
        // 唯一的例外仍是**口令**：密文不可逆、框里不回显，故与 `smtp_cfg_pass` 一样
        // 共用 `MainView::proxy_cfg_pass`，保存时（`sync_proxy_config`）才加密落盘。
        //
        // 开关的「关」= `settings.proxy = None`（规格 §1.10 ④：`None` 就是「不用代理」），
        // 关闭时下面四行整块收起 —— 留着一堆填不动的框只会让人以为还得填点什么。
        let proxy_group = {
            let me_on_get = me.clone();
            let me_on_set = me.clone();
            let me_kind_get = me.clone();
            let me_kind_set = me.clone();
            let me_host_get = me.clone();
            let me_host_set = me.clone();
            let me_port_get = me.clone();
            let me_port_set = me.clone();
            let me_user_get = me.clone();
            let me_user_set = me.clone();
            let pass = self.io.proxy_cfg_pass.clone();

            let mut g = SettingGroup::new().item(
                SettingItem::new(
                    t("使用代理"),
                    SettingField::switch(
                        move |cx: &App| me_on_get.read(cx).settings.proxy.is_some(),
                        move |val: bool, cx: &mut App| {
                            me_on_set.update(cx, |m, cx| {
                                if val {
                                    // 已填过就原样留着（关掉再打开不该清空刚填的地址）
                                    if m.settings.proxy.is_none() {
                                        m.settings.proxy =
                                            Some(rustshell_core::proxy::ProxyConfig::default());
                                    }
                                } else {
                                    m.settings.proxy = None;
                                }
                                cx.notify();
                            });
                        },
                    )
                    .default_value(false),
                )
                .description(t("建立 SSH 连接前先与代理握手（HTTP CONNECT / SOCKS5）。"))
                .keywords(["proxy", "http", "socks5", "代理", "网络", "跳板"]),
            );

            // 关着时其余行不渲染：`settings.proxy` 是这些行的数据源，没有它就没有可绑定的值。
            if self.settings.proxy.is_some() {
                g = g
                    .item(
                        SettingItem::new(
                            t("类型"),
                            SettingField::render(move |options, _w, cx| {
                                let label = me_kind_get
                                    .read(cx)
                                    .settings
                                    .proxy
                                    .as_ref()
                                    .map(|p| p.kind.label())
                                    .unwrap_or("HTTP");
                                // 内层 `on_click` 要 `move` 掉实体，而外层是 `Fn`（每帧都调）：
                                // 每帧克隆一次 `Entity`（引用计数 +1），别把外层捕获的那个移走。
                                let setter = me_kind_set.clone();
                                Button::new("set-proxy-kind")
                                    .outline()
                                    .with_size(options.size())
                                    .label(label)
                                    .on_click(move |_e, _w, cx| {
                                        setter.update(cx, |m, cx| {
                                            if let Some(p) = m.settings.proxy.as_mut() {
                                                let all = rustshell_core::proxy::ProxyKind::ALL;
                                                let ix = all
                                                    .iter()
                                                    .position(|k| *k == p.kind)
                                                    .unwrap_or(0);
                                                p.kind = all[(ix + 1) % all.len()];
                                            }
                                            cx.notify();
                                        });
                                    })
                                    .into_any_element()
                            }),
                        )
                        .description(t("HTTP CONNECT 最通用；SOCKS5 支持域名直通。"))
                        .keywords(["proxy", "type", "http", "socks5", "类型", "代理"]),
                    )
                    .item(
                        SettingItem::new(
                            t("代理地址"),
                            SettingField::input(
                                move |cx: &App| {
                                    SharedString::from(
                                        me_host_get
                                            .read(cx)
                                            .settings
                                            .proxy
                                            .as_ref()
                                            .map(|p| p.host.clone())
                                            .unwrap_or_default(),
                                    )
                                },
                                move |val: SharedString, cx: &mut App| {
                                    me_host_set.update(cx, |m, cx| {
                                        if let Some(p) = m.settings.proxy.as_mut() {
                                            // 不在每次击键里 trim：用户正在敲的空格会被吃掉。
                                            // 首尾空白由保存路径的 `normalize` 收口。
                                            p.host = val.to_string();
                                        }
                                        cx.notify();
                                    });
                                },
                            )
                            .default_value(SharedString::from("")),
                        )
                        .description(t("如 127.0.0.1 或 proxy.corp.lan"))
                        .keywords(["proxy", "host", "地址", "代理"]),
                    )
                    .item(
                        SettingItem::new(
                            t("代理端口"),
                            SettingField::number_input(
                                NumberFieldOptions {
                                    min: 1.0,
                                    max: 65535.0,
                                    step: 1.0,
                                },
                                move |cx: &App| {
                                    me_port_get
                                        .read(cx)
                                        .settings
                                        .proxy
                                        .as_ref()
                                        .map(|p| p.port as f64)
                                        .unwrap_or(rustshell_core::proxy::DEFAULT_PROXY_PORT as f64)
                                },
                                move |val: f64, cx: &mut App| {
                                    me_port_set.update(cx, |m, cx| {
                                        if let Some(p) = m.settings.proxy.as_mut() {
                                            p.port = val as u16;
                                        }
                                        cx.notify();
                                    });
                                },
                            )
                            .default_value(rustshell_core::proxy::DEFAULT_PROXY_PORT as f64),
                        )
                        .description(t("常见：8080 明文 / 3128 Squid / 1080 SOCKS5")),
                    )
                    .item(
                        SettingItem::new(
                            t("代理用户名"),
                            SettingField::input(
                                move |cx: &App| {
                                    SharedString::from(
                                        me_user_get
                                            .read(cx)
                                            .settings
                                            .proxy
                                            .as_ref()
                                            .and_then(|p| p.username.clone())
                                            .unwrap_or_default(),
                                    )
                                },
                                move |val: SharedString, cx: &mut App| {
                                    me_user_set.update(cx, |m, cx| {
                                        if let Some(p) = m.settings.proxy.as_mut() {
                                            let u = val.trim().to_string();
                                            p.username = if u.is_empty() { None } else { Some(u) };
                                        }
                                        cx.notify();
                                    });
                                },
                            )
                            .default_value(SharedString::from("")),
                        )
                        .description(t("留空 = 匿名代理（不带认证）"))
                        .keywords([
                            "proxy",
                            "user",
                            "auth",
                            "用户名",
                            "代理",
                        ]),
                    )
                    .item(
                        SettingItem::new(
                            t("代理口令"),
                            SettingField::render(move |options, _w, _cx| {
                                Input::new(&pass)
                                    .with_size(options.size())
                                    .w(px(240.))
                                    .into_any_element()
                            }),
                        )
                        .description(t("加密保存、不回显；留空沿用已保存的口令。")),
                    );
            }
            g
        };

        // AI 助手
        // 单一 Provider：2026-09-16 按用户要求移除备用端点，不再降级。
        // Key 密文不可逆故不回显，留空即沿用已保存的 Key；输入框本身**掩码显示**
        // （2026-09-18「api key 也改为 *** 展示」），口径与连接口令一致。
        //
        // 2026-09-18 运行反馈「取消 AI 上下文限制配置」：原「上下文行数」条目整条撤下，
        // 注入行数改为 core 里的常量 `rustshell_core::ai::CONTEXT_LINES`（200 行）。
        // 面板上留一个可调数字，只会诱使人往大调（prompt 变长、回答变慢），
        // 而终端问答看到最近 200 行本就足够。
        //
        // 端点/模型/Key 三处输入框与 AI 面板共用同一份 InputState，
        // 故经`SettingField::render`嵌入官方布局，而不是另建一份状态。
        let ai = {
            // 采样温度的滑块状态与落库都归 `MainView`（见 `MainView::new` 里的
            // `ai_temp_slider` 与它的 `cx.subscribe`）：`SettingField` 的闭包只拿得到
            // `&App`/`&mut App`，而一次拖动横跨多帧，状态必须比面板活得久。
            let slider = self.ai_ui.ai_temp_slider.clone();
            let me_temp = me.clone();
            SettingPage::new(t("AI 助手"))
                .icon(IconName::Bot)
                .description(t("OpenAI 兼容端点；请求失败直接报错，不自动降级。"))
                .group(SettingGroup::new().items(vec![
                        bound_input_item(
                            t("API 端点"),
                            self.ai_ui.ai_cfg_base.clone(),
                            t("如 https://api.deepseek.com/v1（不含 /chat/completions）"),
                        ),
                        bound_input_item(
                            t("模型名称"),
                            self.ai_ui.ai_cfg_model.clone(),
                            t("如 deepseek-flash"),
                        ),
                        bound_input_item(
                            t("API Key"),
                            self.ai_ui.ai_cfg_key.clone(),
                            t("加密保存、不回显；留空沿用已保存的 Key。"),
                        ),
                        SettingItem::new(
                            t("采样温度"),
                            // 2026-09-18 运行反馈「采样温度改为滑动轴，0.1 一个间隔」：
                            // 原来的数字框得靠 +/- 或手敲，而「试温」这种调法本来就是
                            // 左右拖着看效果；步长 0.1 由 `TEMPERATURE_STEP` 定（见 main_view）。
                            // 右侧数值是拖动过程中的即时读数 —— 没有它，滑块只能看出
                            // 「大概在哪」，说不出当前到底是 0.3 还是 0.4。
                            SettingField::render(move |_options, _w, cx| {
                                let cur = me_temp.read(cx).ai_ui.ai_cfg.temperature;
                                let tok = crate::theme::current(cx);
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .w(px(240.))
                                    .child(Slider::new(&slider).w(px(190.)))
                                    .child(
                                        div()
                                            .w(px(34.))
                                            .text_size(px(12.))
                                            .text_color(hex(tok.fg_primary))
                                            .child(SharedString::from(format!("{cur:.1}"))),
                                    )
                                    .into_any_element()
                            }),
                        )
                        .description(t("0–2；越低越稳定，建议 0.3")),
                    ]))
                // AI 调试日志：值直接读写 `ai_cfg` —— 官方 `Settings` 组件每帧现读，
                // 不需要额外 InputState，回滚也随 `set_snapshot` 天然成立。
                .group({
                    // 2026-09-22 用户要求「设置新增 agent 配置，AI 助手中的 agent 设置归并」：
                    // Agent 三项开关与「免确认白名单」已搬到独立的一页（见下面的 `agent`），
                    // 本页只留「AI 助手」自己的几项（端点 / 模型 / Key / 采样温度）与这条
                    // 诊断开关 —— 它记的是 AI 的请求与流式原文，不专属于 agent。
                    let me_dbg_get = me.clone();
                    let me_dbg_set = me.clone();
                    SettingGroup::new()
                        .item(
                            SettingItem::new(
                                t("AI 调试日志"),
                                SettingField::switch(
                                    move |cx: &App| me_dbg_get.read(cx).ai_ui.ai_cfg.debug_log,
                                    move |val: bool, cx: &mut App| {
                                        me_dbg_set.update(cx, |m, cx| {
                                            m.ai_ui.ai_cfg.debug_log = val;
                                            cx.notify();
                                        });
                                    },
                                )
                                .default_value(false),
                            )
                            .description(t(
                                "把 AI 的请求、流式原文和工具调用明细写进日志（logs/日期/rustshell.log），排查问题时开启。",
                            )),
                        )
                })
        };

        // Agent（工具调用）
        //
        // 2026-09-22 用户要求「设置新增 agent 配置，AI 助手中的 agent 设置归并」：
        // 这一页收拢原先散在「AI 助手」页里的 agent 条目 —— 启用开关、步数预算、
        // 命令权限与「免确认白名单」。单独成页的理由：它们管的是**AI 能不能动你的
        // 机器、能动到哪一步**，与「用哪个端点、哪个模型」是两件事；混在一页时，
        // 想调权限的人得先翻过一整组请求参数。
        //
        // 值仍直接读写 `ai_cfg`（官方 `Settings` 每帧现读，回滚随 `set_snapshot`
        // 天然成立），落盘仍由面板的「保存」负责 —— 搬运没有改动任何一条数据通路。
        //
        // 页名走 `tp`：`Agent` 是**中英同名**的术语，`tp("Agent", "Agent")` 省掉
        // 词条表里一条恒等映射（表里那条既无信息量，又要人去维护）。
        let agent = {
            let me_on_get = me.clone();
            let me_on_set = me.clone();
            let me_mut_get = me.clone();
            let me_mut_set = me.clone();
            let me_wl = me.clone();
            SettingPage::new(page_title("Agent", "Agent"))
                .icon(IconName::Settings2)
                .description(t("工具调用的开关、命令权限与免确认白名单。"))
                .group(
                    SettingGroup::new()
                        .item(
                            SettingItem::new(
                                t("启用 Agent 工具调用"),
                                SettingField::switch(
                                    move |cx: &App| me_on_get.read(cx).ai_ui.ai_cfg.agent_enabled,
                                    move |val: bool, cx: &mut App| {
                                        me_on_set.update(cx, |m, cx| {
                                            m.ai_ui.ai_cfg.agent_enabled = val;
                                            cx.notify();
                                        });
                                    },
                                )
                                .default_value(true),
                            )
                            .description(t(
                                "关闭后退化为纯问答：请求体不再附带 tools，AI 只能用已有上下文回答。",
                            )),
                        )
                        .item(
                            SettingItem::new(
                                t("允许状态变更命令"),
                                SettingField::switch(
                                    move |cx: &App| me_mut_get.read(cx).ai_ui.ai_cfg.agent_allow_mutating,
                                    move |val: bool, cx: &mut App| {
                                        me_mut_set.update(cx, |m, cx| {
                                            m.ai_ui.ai_cfg.agent_allow_mutating = val;
                                            cx.notify();
                                        });
                                    },
                                )
                                .default_value(false),
                            )
                            .description(t(
                                "开启后 AI 仍会先弹确认卡片；无人值守的定时巡检一律不执行。",
                            )),
                        ),
                )
                .group(
                    SettingGroup::new().item(
                        SettingItem::new(
                            // 「免确认白名单」：agent 与命令卡片共用的**唯一**免确认依据
                            // （`core::agent::judge`）。此前只能从命令卡片的「加入白名单」
                            // 单向添加，面板上看不见也删不掉 —— 这里补上入口。
                            //
                            // 2026-09-22 运行反馈「右侧改为配置按钮，点击弹出框展示白名单
                            // 列表」：条目内联铺在页面上会把设置页撑开，改由弹窗承载列表 /
                            // 搜索 / 逐条开关 / 删除（见 `main_view::whitelist_dialog`）。
                            // 口径与 `judge` 逐条对齐（不要在两处各写一份）：
                            // 命中白名单 ⇒ 免确认直接执行；未命中 ⇒ 状态变更类命令一律先弹
                            // 确认卡片；破坏类命令（`Blocked`）**永不执行**，加进白名单也没用；
                            // 批量（>1 台）执行不适用白名单；无人值守（定时巡检）一律不执行。
                            t("免确认白名单"),
                            SettingField::render(move |_options, _w, _cx| {
                                Button::new("ai-wl-open")
                                    .small()
                                    .outline()
                                    .label(t("配置"))
                                    .on_click({
                                        let me = me_wl.clone();
                                        move |_e, w, cx: &mut App| {
                                            me.update(cx, |m, cx| {
                                                m.open_ai_whitelist_dialog(w, cx)
                                            });
                                        }
                                    })
                                    .into_any_element()
                            }),
                        )
                        .description(t(
                            "命中的状态变更命令免确认直接执行；关闭某条即该命令失效；支持 * 通配符；批量执行不适用。",
                        ))
                        .keywords(["whitelist", "auto", "白名单", "免确认"]),
                    ),
                )
        };

        // 快捷键
        let keybind = {
            let rows = self.panel_ui.keymap_rows.clone();
            let recording = self.panel_ui.keymap_recording.clone();
            let hint = self.panel_ui.keymap_hint.clone();
            let me_row = me.clone();
            SettingPage::new(t("快捷键"))
                .icon(IconName::CaseSensitive)
                .description(t("点「修改」后按下新组合键；Esc 取消录制。"))
                .group(
                    SettingGroup::new().item(
                        SettingItem::render(move |_options, _w, cx| {
                            let mut list = v_flex()
                                .id("kb-rows")
                                .w_full()
                                .max_h(px(280.))
                                .overflow_y_scrollbar()
                                .gap_1();
                            for row in &rows {
                                list =
                                    list.child(keybind_row(row, recording.as_deref(), &me_row, cx));
                            }
                            let mut col = v_flex().w_full().gap_2().child(list);
                            if let Some(hint) = hint.clone() {
                                col = col.child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(hex(crate::theme::current(cx).fg_muted))
                                        .child(SharedString::from(hint)),
                                );
                            }
                            // 2026-09-18 运行反馈「移除设置-快捷键-导出导入快捷键」：
                            // 原先这里横排「导出键位」「导入键位」两枚按钮，进出口径是
                            // `<config>/keymap-export.json`。撤掉后键位仍持久化在
                            // `keymap.json`，只是不再有应用内的导入导出通路。
                            col.into_any_element()
                        })
                        .keywords([
                            "keybind",
                            "shortcut",
                            "键位",
                            "快捷键",
                        ]),
                    ),
                )
        };

        // 连接：导入导出 + 会话参数。
        //
        // 2026-09-15 用户要求「设置导入和导出都要自己选择目录和文件进行」，
        // 设置页不再摆任何需要用户手填路径的中间控件。
        // 2026-09-17 用户要求：① 页名「连接管理」改「连接」；② 导入导出合并成一块。
        // 原来分作标题为「导出」「导入」两个组，现在并成一个条目「导入导出」，
        // 两枚按钮并排 —— 它们本就是一件事的正反两面，分成两块只是重复一遍标题。
        let connections_io = {
            let me_export = me.clone();
            let me_import = me.clone();
            let me_openssh = me.clone();

            SettingPage::new(page_title("连接", "Connections"))
                .icon(IconName::FolderOpen)
                .description(t("导出或导入连接配置；文件不含任何口令。"))
                .group(
                    SettingGroup::new().item(
                        SettingItem::new(
                            t("导入导出"),
                            SettingField::render({
                                let me_export = me_export.clone();
                                let me_import = me_import.clone();
                                let me_openssh = me_openssh.clone();
                                move |_options, _w, _cx| {
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            Button::new("conn-export")
                                                .small()
                                                .primary()
                                                .label(rustshell_core::i18n::t("导出连接"))
                                                .on_click({
                                                    let me = me_export.clone();
                                                    move |_e, _w, cx| {
                                                        me.update(cx, |m, cx| {
                                                            m.export_connections(cx)
                                                        });
                                                    }
                                                }),
                                        )
                                        .child(
                                            Button::new("conn-import")
                                                .small()
                                                .primary()
                                                .label(rustshell_core::i18n::t("导入连接"))
                                                .on_click({
                                                    let me = me_import.clone();
                                                    move |_e, _w, cx| {
                                                        me.update(cx, |m, cx| {
                                                            m.import_connections(cx)
                                                        });
                                                    }
                                                }),
                                        )
                                        // F-1：从 OpenSSH `~/.ssh/config` 批量导入（自带预览）
                                        .child(
                                            Button::new("conn-import-openssh")
                                                .small()
                                                .outline()
                                                .label(rustshell_core::i18n::t(
                                                    "从 OpenSSH 配置导入",
                                                ))
                                                .on_click({
                                                    let me = me_openssh.clone();
                                                    move |_e, _w, cx| {
                                                        me.update(cx, |m, cx| {
                                                            m.import_openssh_pick_file(cx)
                                                        });
                                                    }
                                                }),
                                        )
                                        .into_any_element()
                                }
                            }),
                        )
                        .keywords(["export", "import", "导出", "导入", "连接"]),
                    ),
                )
                .group(session_group)
                .group(sftp_group)
                .group(proxy_group)
        };

        // 「关于」页（2026-10-22 用户要求恢复）：软件简介、GitHub 仓库入口与检查更新。
        //
        // 2026-09-18 曾整页撤下（当时连同「导出 / 复制诊断信息」一起删）；这次只恢复
        // 「关于」本身，诊断导出与键位导入导出仍不回归 —— 那几项会替用户把本机路径
        // 写进文件或弹资源管理器，属于「用户没要就替用户做决定」的动作。
        let about = {
            let me_check = me.clone();
            let me_status = me.clone();
            SettingPage::new(t("关于"))
                .icon(IconName::Info)
                .description(t(
                    "RustShell 是一款基于 Rust 与 GPUI 构建的跨平台 SSH / SFTP 终端，支持多标签与分屏、文件传输、AI 助手和批量命令执行。",
                ))
                .group(
                    SettingGroup::new()
                        .item(SettingItem::new(
                            t("版本"),
                            SettingField::render(move |_options, _w, cx| {
                                let tok = crate::theme::current(cx);
                                div()
                                    .text_size(px(12.))
                                    .text_color(hex(tok.fg_primary))
                                    .child(SharedString::from(format!(
                                        "v{}",
                                        rustshell_core::APP_VERSION
                                    )))
                                    .into_any_element()
                            }),
                        ))
                        .item(
                            SettingItem::new(
                                SharedString::from("GitHub"),
                                SettingField::render(move |_options, _w, _cx| {
                                    Button::new("about-github")
                                        .small()
                                        .outline()
                                        .label(t("GitHub 仓库"))
                                        .on_click(|_e, _w, cx: &mut App| {
                                            cx.open_url(rustshell_core::update::REPO_URL)
                                        })
                                        .into_any_element()
                                }),
                            )
                            .description(SharedString::from(rustshell_core::update::REPO_URL)),
                        )
                        .item(
                            SettingItem::new(
                                t("检查更新"),
                                SettingField::render(move |_options, _w, cx| {
                                    let (status, trouble, phase) = {
                                        let m = me_status.read(cx);
                                        let (line, trouble) = m.update_status_line();
                                        (line, trouble, m.io.update_phase.clone())
                                    };
                                    let tok = crate::theme::current(cx);
                                    let busy = phase.is_busy();
                                    // 一枚按钮按状态换语义：没查过 = 检查；有新版 = 立即更新；
                                    // 已就绪 = 重启；失败 = 重试。与标题栏徽标同一套判定。
                                    let primary = match phase {
                                        super::UpdatePhase::Available => t("立即更新"),
                                        super::UpdatePhase::Ready => t("重启以完成更新"),
                                        super::UpdatePhase::Failed(_) => t("重试"),
                                        super::UpdatePhase::Downloading => t("正在下载…"),
                                        super::UpdatePhase::Checking => t("正在检查…"),
                                        _ => t("检查更新"),
                                    };
                                    h_flex()
                                        .gap_2()
                                        .items_center()
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .text_size(px(11.5))
                                                .text_color(hex(if trouble {
                                                    tok.danger
                                                } else {
                                                    tok.fg_secondary
                                                }))
                                                .child(SharedString::from(status)),
                                        )
                                        .child(
                                            Button::new("about-check-update")
                                                .small()
                                                .primary()
                                                .label(primary)
                                                .disabled(busy)
                                                .on_click({
                                                    let me = me_check.clone();
                                                    move |_e, w, cx: &mut App| {
                                                        me.update(cx, |m, cx| {
                                                            m.update_primary_action(w, cx)
                                                        });
                                                    }
                                                }),
                                        )
                                        .child(
                                            Button::new("about-update-details")
                                                .small()
                                                .outline()
                                                .label(t("详情"))
                                                .on_click({
                                                    let me = me_check.clone();
                                                    move |_e, w, cx: &mut App| {
                                                        me.update(cx, |m, cx| {
                                                            m.open_update_dialog(w, cx)
                                                        });
                                                    }
                                                }),
                                        )
                                        .into_any_element()
                                }),
                            )
                            .description(t(
                                "依据 GitHub 上的版本标签判断；有新版时标题栏右侧会显示提示。",
                            )),
                        ),
                )
        };

        // 预警通知（SMTP）
        //
        // 配置项直接读写 `smtp_cfg`（`SettingField` 的 getter/setter 都是 `&App`/
        // `&mut App`，写入 `MainView` 的字段即可，无需额外 InputState）；
        // 唯一的例外是**口令**：密文不可逆、框里不回显，故与 `ai_cfg_key` 一样
        // 共用 `MainView::smtp_cfg_pass`，保存时（`sync_smtp_config`）才加密落盘。
        //
        // 发送功能本身留待后续实现（见需求文档 §7.3）：这一页只负责把配置备好，
        // 因此「发送测试邮件」按钮置灰并写明原因，而不是让用户点了没反应。
        let notify = {
            let me_en_get = me.clone();
            let me_en_set = me.clone();
            let me_host_get = me.clone();
            let me_host_set = me.clone();
            let me_port_get = me.clone();
            let me_port_set = me.clone();
            let me_sec_get = me.clone();
            let me_sec_set = me.clone();
            let me_user_get = me.clone();
            let me_user_set = me.clone();
            let me_from_get = me.clone();
            let me_from_set = me.clone();
            let me_fromname_get = me.clone();
            let me_fromname_set = me.clone();
            let me_to_get = me.clone();
            let me_to_set = me.clone();
            let me_cc_get = me.clone();
            let me_cc_set = me.clone();
            let me_subj_get = me.clone();
            let me_subj_set = me.clone();
            let me_ready = me.clone();
            let pass = self.io.smtp_cfg_pass.clone();

            SettingPage::new(page_title("通知", "Notifications"))
                .icon(IconName::Bell)
                .description(t("异常预警的邮件投递（SMTP）。发送功能后续实现，配置先备好。"))
                .group(
                    SettingGroup::new()
                        .item(
                            SettingItem::new(
                                t("启用邮件预警"),
                                SettingField::switch(
                                    move |cx: &App| me_en_get.read(cx).io.smtp_cfg.enabled,
                                    move |val: bool, cx: &mut App| {
                                        me_en_set.update(cx, |m, cx| {
                                            m.io.smtp_cfg.enabled = val;
                                            cx.notify();
                                        });
                                    },
                                )
                                .default_value(false),
                            )
                            .description(t("关闭时预警只写本地日志，不发邮件。")),
                        )
                        .item(
                            SettingItem::new(
                                t("SMTP 服务器"),
                                SettingField::input(
                                    move |cx: &App| {
                                        SharedString::from(me_host_get.read(cx).io.smtp_cfg.host.clone())
                                    },
                                    move |val: SharedString, cx: &mut App| {
                                        me_host_set.update(cx, |m, cx| {
                                            m.io.smtp_cfg.host = val.to_string();
                                            cx.notify();
                                        });
                                    },
                                )
                                .default_value(SharedString::from("")),
                            )
                            .description(t("如 smtp.example.com")),
                        )
                        .item(
                            SettingItem::new(
                                t("端口"),
                                SettingField::number_input(
                                    NumberFieldOptions {
                                        min: 1.0,
                                        max: 65535.0,
                                        step: 1.0,
                                    },
                                    move |cx: &App| me_port_get.read(cx).io.smtp_cfg.port as f64,
                                    move |val: f64, cx: &mut App| {
                                        me_port_set.update(cx, |m, cx| {
                                            m.io.smtp_cfg.port = val as u16;
                                            cx.notify();
                                        });
                                    },
                                )
                                .default_value(465.0),
                            )
                            .description(t("SSL 465 / STARTTLS 587 / 明文 25")),
                        )
                        .item(
                            SettingItem::new(
                                t("加密方式"),
                                SettingField::dropdown(
                                    rustshell_core::notify_smtp::SmtpSecurity::all()
                                        .into_iter()
                                        .map(|s| {
                                            // 三个显示名以**字面量**给出（而不是走
                                            // `SmtpSecurity::display_key()`）：`i18n`
                                            // 的 `g1`/`g2` 两条护栏是静态扫源码的，
                                            // 字面量才能既证明「有词条」也证明「词条有人用」。
                                            let label = match s {
                                                rustshell_core::notify_smtp::SmtpSecurity::None => {
                                                    "明文（不加密）"
                                                }
                                                rustshell_core::notify_smtp::SmtpSecurity::StartTls => {
                                                    "STARTTLS"
                                                }
                                                rustshell_core::notify_smtp::SmtpSecurity::Tls => {
                                                    "SSL / TLS"
                                                }
                                            };
                                            (SharedString::from(s.key()), t(label))
                                        })
                                        .collect::<Vec<_>>(),
                                    move |cx: &App| {
                                        SharedString::from(me_sec_get.read(cx).io.smtp_cfg.security.key())
                                    },
                                    move |val: SharedString, cx: &mut App| {
                                        let sec = rustshell_core::notify_smtp::SmtpSecurity::from_key(
                                            val.as_ref(),
                                        );
                                        me_sec_set.update(cx, |m, cx| {
                                            m.io.smtp_cfg.security = sec;
                                            cx.notify();
                                        });
                                    },
                                )
                                .default_value(SharedString::from(
                                    rustshell_core::notify_smtp::SmtpSecurity::default().key(),
                                )),
                            )
                            .description(t("选「明文」只在可信内网使用。")),
                        ),
                )
                .group(
                    SettingGroup::new()
                        .item(
                            SettingItem::new(
                                t("用户名"),
                                SettingField::input(
                                    move |cx: &App| {
                                        SharedString::from(
                                            me_user_get.read(cx).io.smtp_cfg.username.clone(),
                                        )
                                    },
                                    move |val: SharedString, cx: &mut App| {
                                        me_user_set.update(cx, |m, cx| {
                                            m.io.smtp_cfg.username = val.to_string();
                                            cx.notify();
                                        });
                                    },
                                )
                                .default_value(SharedString::from("")),
                            )
                            .description(t("多数服务要求用户名是完整邮箱地址。")),
                        )
                        .item(
                            SettingItem::new(
                                t("口令"),
                                SettingField::render(move |options, _w, _cx| {
                                    Input::new(&pass)
                                        .with_size(options.size())
                                        .w(px(240.))
                                        .into_any_element()
                                }),
                            )
                            .description(t("加密保存、不回显；留空沿用已保存的口令。")),
                        ),
                )
                .group(
                    SettingGroup::new()
                        .item(
                            SettingItem::new(
                                t("发件人地址"),
                                SettingField::input(
                                    move |cx: &App| {
                                        SharedString::from(
                                            me_from_get.read(cx).io.smtp_cfg.from.address.clone(),
                                        )
                                    },
                                    move |val: SharedString, cx: &mut App| {
                                        me_from_set.update(cx, |m, cx| {
                                            m.io.smtp_cfg.from.address = val.to_string();
                                            cx.notify();
                                        });
                                    },
                                )
                                .default_value(SharedString::from("")),
                            )
                            .description(t("SMTP 信封上的 From，通常与用户名一致。")),
                        )
                        .item(
                            SettingItem::new(
                                t("发件人显示名"),
                                SettingField::input(
                                    move |cx: &App| {
                                        SharedString::from(
                                            me_fromname_get.read(cx).io.smtp_cfg.from.name.clone(),
                                        )
                                    },
                                    move |val: SharedString, cx: &mut App| {
                                        me_fromname_set.update(cx, |m, cx| {
                                            m.io.smtp_cfg.from.name = val.to_string();
                                            cx.notify();
                                        });
                                    },
                                )
                                .default_value(SharedString::from("")),
                            )
                            .description(t("如「RustShell 预警」，可为空。")),
                        )
                        .item(
                            SettingItem::new(
                                t("收件人"),
                                SettingField::input(
                                    move |cx: &App| {
                                        SharedString::from(
                                            rustshell_core::notify_smtp::recipients_to_text(
                                                &me_to_get.read(cx).io.smtp_cfg.to,
                                            ),
                                        )
                                    },
                                    move |val: SharedString, cx: &mut App| {
                                        let list =
                                            rustshell_core::notify_smtp::parse_recipients(&val);
                                        me_to_set.update(cx, |m, cx| {
                                            m.io.smtp_cfg.to = list;
                                            cx.notify();
                                        });
                                    },
                                )
                                .default_value(SharedString::from("")),
                            )
                            .description(t("逗号或分号分隔，最多 50 个。")),
                        )
                        .item(
                            SettingItem::new(
                                t("抄送"),
                                SettingField::input(
                                    move |cx: &App| {
                                        SharedString::from(
                                            rustshell_core::notify_smtp::recipients_to_text(
                                                &me_cc_get.read(cx).io.smtp_cfg.cc,
                                            ),
                                        )
                                    },
                                    move |val: SharedString, cx: &mut App| {
                                        let list =
                                            rustshell_core::notify_smtp::parse_recipients(&val);
                                        me_cc_set.update(cx, |m, cx| {
                                            m.io.smtp_cfg.cc = list;
                                            cx.notify();
                                        });
                                    },
                                )
                                .default_value(SharedString::from("")),
                            )
                            .description(t("可为空。")),
                        )
                        .item(
                            SettingItem::new(
                                t("主题前缀"),
                                SettingField::input(
                                    move |cx: &App| {
                                        SharedString::from(
                                            me_subj_get.read(cx).io.smtp_cfg.subject_prefix.clone(),
                                        )
                                    },
                                    move |val: SharedString, cx: &mut App| {
                                        me_subj_set.update(cx, |m, cx| {
                                            m.io.smtp_cfg.subject_prefix = val.to_string();
                                            cx.notify();
                                        });
                                    },
                                )
                                .default_value(SharedString::from(
                                    rustshell_core::notify_smtp::DEFAULT_SUBJECT_PREFIX,
                                )),
                            )
                            .description(t("留空则用 [RustShell]。")),
                        )
                        .item(
                            SettingItem::render(move |_options, _w, cx| {
                                // 2026-10-12：发送功能已实现，按钮解除置灰并真正发信。
                                        // 三种置灰理由各自要说清 —— 「配置不全」
                                        // 与「正在发」是不同的事，用户要能分辨。
                                        let (disabled, hint) = {
                                            let m = me_ready.read(cx);
                                            if m.io.smtp_testing {
                                                (
                                                    true,
                                                    rustshell_core::i18n::t("正在发送…").to_string(),
                                                )
                                            } else if !m.io.smtp_cfg.is_ready() {
                                                (
                                                    true,
                                                    rustshell_core::i18n::t("配置尚未完整。")
                                                        .to_string(),
                                                )
                                            } else {
                                                (false, String::new())
                                            }
                                        };
                                        v_flex()
                                            .gap_1()
                                            .child(
                                                // 不隐藏：用户要知道「这里有���测试按钮」，
                                                // 而不是以为漏做了。
                                                Button::new("smtp-test")
                                                    .small()
                                                    .label(rustshell_core::i18n::t("发送测试邮件"))
                                                    .disabled(disabled)
                                                    .on_click(move |_e, _w, cx| {
                                                        if let Some(hub) =
                                                            cx.try_global::<super::UiHub>()
                                                        {
                                                            let main = hub.main.clone();
                                                            let _ = main.update(cx, |m, cx| {
                                                                m.send_test_mail(cx);
                                                            });
                                                        }
                                                    }),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(11.))
                                                    .text_color(hex(crate::theme::current(cx).fg_muted))
                                                    .child(SharedString::from(
                                                        if hint.is_empty() {
                                                            rustshell_core::i18n::t(
                                                                "将按上面的配置发一封测试邮件到收件人。",
                                                            )
                                                            .to_string()
                                                        } else {
                                                            hint
                                                        },
                                                    )),
                                            )
                                            .into_any_element()
                            })
                            .keywords(["smtp", "mail", "test", "邮件", "测试"]),
                        ),
                )
        };

        // 窗口（F-8）：启动行为。
        let window_page = {
            SettingPage::new(page_title("窗口", "Window"))
                // 页图标：设置侧栏里每页都有一枚（终端/AI/Agent/快捷键/连接/通知），
                // 窗口页此前是唯一的例外 —— 侧栏里独它一行没有图标，看着像少画了什么。
                //
                // 取的是全量目录 `gpui_kit_assets::IconName`（`gpui_component::IconName`
                // 只是「默认组件图标」那个子集，里面没有窗口类图标）。
                //
                // 必须是**本仓 `shell/assets/icons/` 里真有的**那一枚：本仓的
                // `AppAssets` 对图标**没有回退**，名字写对、`path()` 也解析得出，
                // 但 svg 不在目录里就静默留白（`test_support::every_icon_the_ui_asks_for_is_shipped`
                // 正是为这一课立的）。`app-window.svg` 本仓没有，所以取
                // `window-restore`（一个带框的窗口，本仓已随包）。
                .icon(gpui_kit_assets::IconName::WindowRestore)
                .description(t("启动程序时的行为。"))
                // 启动动作（2026-09-28 新增参数 + 面板控件）。
                //
                // 此前启动路径**写死**为打开命令面板：这不是「终端软件」，而是
                // 「命令面板启动器」。新装用户点开图标看到一片深色空面板 + 光标，
                // 既没有 shell 也没有任何提示 —— 产品定位漂移的典型症状。
                //
                // 面板只摆 [LocalShell, CommandPalette, Nothing] 三档：
                // `RestoreSession` 不在其中，它是 [`session_restore`] 那套
                // 「要不要恢复、问不问」的另一种说法，摆在一起会让人以为要配两次
                //（同 `StartupAction::panel_options` 的口径）。
                .group({
                    let me_get = me.clone();
                    let me_set = me.clone();
                    SettingGroup::new().item(
                        SettingItem::new(
                            t("启动时打开"),
                            SettingField::dropdown(
                                StartupAction::panel_options()
                                    .iter()
                                    .map(|a| {
                                        let (zh, en) = a.labels();
                                        let name = if rustshell_core::i18n::is_en() {
                                            SharedString::from(en)
                                        } else {
                                            SharedString::from(zh)
                                        };
                                        (SharedString::from(a.key()), name)
                                    })
                                    .collect::<Vec<_>>(),
                                move |cx: &App| {
                                    SharedString::from(me_get.read(cx).settings.startup_action.key())
                                },
                                move |val: SharedString, cx: &mut App| {
                                    let action = StartupAction::from_key(val.as_ref());
                                    me_set.update(cx, |m, cx| {
                                        m.settings.startup_action = action;
                                        cx.notify();
                                    });
                                },
                            )
                            .default_value(SharedString::from(StartupAction::LocalShell.key())),
                        )
                        .description(t(
                            "打开软件后直接进入什么。默认开一个本地终端；不需要终端时可选命令面板或不打开。",
                        )),
                    )
                })
            // F-9 会话恢复的**面板控件**已于 2026-09-28 按用户要求撤下。
            //
            // 2026-09-29 进一步：用户要求「取消恢复上次会话功能」，启动**不再自动**
            // 弹「恢复上次会话？」框、也不再按档位自动重建标签；恢复改由命令面板
            // 「恢复上次会话」手动触发（`RestoreLastSession` → `layout_restore_now`）。
            //
            // 为什么撤：`startup_action` 里已经有一档 `RestoreSession`，
            // 面板上再摆一个「启动时会话恢复：询问/自动/不恢复」，
            // 同一个启动行为要用户在两处各配一次，而且两处的措辞还互相
            // 解释不了（「启动时打开：恢复上次会话」+「启动时会话恢复：询问」
            // 到底会不会弹框？）。留一处口径就够；而现在连「自动」这一处也没了。
            //
            // 设置值本身**保留**：老用户的 `session_restore` 不迁移、不重置，
            // 只是不再驱动启动行为。删掉字段会是数据兼容事故
            //（反序列化丢值 → 下次启动静默变成默认的「询问」）。
        };

        // 页装配：连接提到第一位；性能并入「终端设置」。
        Settings::new("settings-panel")
            // 对话框 940px 宽，侧栏收到 180px 起，字段列才有余量
            .sidebar_width(px(180.))
            .sidebar_size_range(px(150.)..px(262.))
            // 页序：连接 → 窗口 → 终端 → AI 助手 → Agent → 通知 → 快捷键 → 关于。
            // 窗口在前：它是「这台机器上长什么样」，与连接/终端这类
            // 「跟远端打交道」的页分开。
            // Agent 紧挨 AI 助手：两者读的是同一份 `ai_cfg`，agent 页只是把
            // 「AI 能不能动这台机器」的那几项独立出来（2026-09-22 用户要求归并）。
            // 关于殿后：版本 / 仓库 / 更新检查属于低频元信息，不占常驻视线。
            .pages(vec![
                connections_io,
                window_page,
                terminal,
                ai,
                agent,
                notify,
                keybind,
                about,
            ])
    }
}

/// 与`MainView`共用的输入框（AI 端点 / 模型 / Key）：借官方`SettingField::render`嵌进官方布局，标题与描述由`SettingItem`提供。
fn bound_input_item(
    title: SharedString,
    state: Entity<InputState>,
    description: SharedString,
) -> SettingItem {
    SettingItem::new(
        title,
        SettingField::render(move |options, _w, _cx| {
            Input::new(&state)
                .with_size(options.size())
                .w(px(240.))
                .into_any_element()
        }),
    )
    .description(description)
}

/// 自定义配色编辑区。仅 `terminal_color_scheme == "custom"` 时展示。
///
/// 未用官方 `ColorPicker`（19 个颜色各需一个 `Entity<ColorPickerState>`，且需
/// 事件回流，与设置快照回滚语义冲突）；改用「色块预览 + hex 输入框」达同等能力。
fn custom_palette_editor(me: &Entity<MainView>, cx: &mut App) -> AnyElement {
    let tok = crate::theme::current(cx);
    let settings = &me.read(cx).settings;

    if settings.terminal_color_scheme != rustshell_core::theme::SCHEME_CUSTOM {
        return div()
            .text_size(px(11.))
            .text_color(hex(tok.fg_muted))
            .child(SharedString::from(rustshell_core::i18n::t(
                "选择「自定义」配色方案后可编辑 16 色。",
            )))
            .into_any_element();
    }

    // 未初始化自定义数据时，以当前主题解析出的配色为初值（与`resolve_terminal_palette`同源）
    let cur = settings
        .terminal_custom_palette
        .clone()
        .map(|c| c.to_palette())
        .unwrap_or_else(|| settings.resolve_terminal_palette());

    let muted = tok.fg_muted;
    let border = tok.border_subtle;
    let mut col = v_flex().w_full().gap_3();

    // 基础三色
    let mut tri = h_flex().gap_3().flex_wrap().items_center();
    for (label, value, setter) in [
        (
            SharedString::from(rustshell_core::i18n::t("背景")),
            cur.background,
            PaletteTarget::Background,
        ),
        (
            SharedString::from(rustshell_core::i18n::t("前景")),
            cur.foreground,
            PaletteTarget::Foreground,
        ),
        (
            SharedString::from(rustshell_core::i18n::t("光标")),
            cur.cursor,
            PaletteTarget::Cursor,
        ),
    ] {
        tri = tri.child(color_cell(&me.clone(), label, value, setter, muted, border));
    }
    col = col.child(tri);

    // normal 0..8
    let mut g1 = h_flex().gap_3().flex_wrap().items_center();
    for i in 0..8 {
        g1 = g1.child(color_cell(
            &me.clone(),
            SharedString::from(format!("{i}")),
            cur.normal[i],
            PaletteTarget::Normal(i),
            muted,
            border,
        ));
    }
    col = col.child(
        v_flex()
            .gap_1()
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(hex(muted))
                    .child(SharedString::from(rustshell_core::i18n::t("标准色 0–7"))),
            )
            .child(g1),
    );

    // bright 0..8
    let mut g2 = h_flex().gap_3().flex_wrap().items_center();
    for i in 0..8 {
        g2 = g2.child(color_cell(
            &me.clone(),
            SharedString::from(format!("{}", i + 8)),
            cur.bright[i],
            PaletteTarget::Bright(i),
            muted,
            border,
        ));
    }
    col = col.child(
        v_flex()
            .gap_1()
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(hex(muted))
                    .child(SharedString::from(rustshell_core::i18n::t("高亮色 8–15"))),
            )
            .child(g2),
    );

    // 恢复默认
    col = col.child(
        Button::new("palette-reset")
            .small()
            .outline()
            .label(SharedString::from(rustshell_core::i18n::t("恢复默认")))
            .on_click({
                let me = me.clone();
                move |_e, _w, cx: &mut App| {
                    me.update(cx, |m, cx| m.reset_custom_palette(cx));
                }
            }),
    );

    col.into_any_element()
}

/// 自定义配色中可编辑的槽位（`settings_actions.rs`的 setter 也需要，故`pub(super)`）
#[derive(Clone, Copy)]
pub(super) enum PaletteTarget {
    Background,
    Foreground,
    Cursor,
    Normal(usize),
    Bright(usize),
}

impl PaletteTarget {
    /// 稳定的元素 key（不含颜色值，避免改色时元素 id 变化导致状态丢失）
    fn element_key(self) -> String {
        match self {
            PaletteTarget::Background => "bg".to_string(),
            PaletteTarget::Foreground => "fg".to_string(),
            PaletteTarget::Cursor => "cursor".to_string(),
            PaletteTarget::Normal(i) => format!("n{i}"),
            PaletteTarget::Bright(i) => format!("b{i}"),
        }
    }
}

/// 单个颜色槽：色块预览 + 「颜色 / 十六进制」两态显示。 点击色块 到 在若干预设色之间循环（轻量实现，避免为 19 个槽各建 ColorPicker）。 另外提供 hex 只读显示便于用户在 settings.json 里核对。
fn color_cell(
    me: &Entity<MainView>,
    label: SharedString,
    value: u32,
    target: PaletteTarget,
    muted: u32,
    border: u32,
) -> AnyElement {
    use gpui::{InteractiveElement as _, StatefulInteractiveElement as _, Styled as _};
    let key: SharedString = format!("pc-{}", target.element_key()).into();
    let me_c = me.clone();
    let tip_for: SharedString = format!("#{:06X}", value).into();

    h_flex()
        .gap_2()
        .items_center()
        .w(px(132.))
        .child(
            div()
                .text_size(px(10.))
                .text_color(hex(muted))
                .w(px(28.))
                .child(label),
        )
        .child(
            div()
                .id(key)
                .w(px(20.))
                .h(px(20.))
                .rounded(px(3.))
                .bg(hex(value))
                .border_1()
                .border_color(hex(border))
                .flex_shrink_0()
                .tooltip(move |_w, cx| {
                    gpui_component::tooltip::Tooltip::new(tip_for.clone()).build(_w, cx)
                })
                .on_click(move |_e, _w, cx: &mut App| {
                    me_c.update(cx, |m, cx| m.cycle_custom_color(target, cx));
                }),
        )
        .child(
            div()
                .text_size(px(10.))
                .text_color(hex(muted))
                .child(SharedString::from(format!("{:06X}", value))),
        )
        .into_any_element()
}

// 说明：自定义配色采用「点击色块循环预设色」，而非完整取色器。
// 理由是官方`ColorPicker`需要为每个槽建立`Entity<ColorPickerState>`并处理
//`ColorPickerEvent`回流，与「设置面板打开时快照、取消时回滚」的 语义
// 冲突（色板状态不属于 MainView，回滚不到）。循环预设色方案下，颜色值的唯一
// 事实源仍是`settings.terminal_custom_palette`，回滚天然成立。
// 已登记为文档偏差 D-1。

/// 键位表一行：动作名 + 当前组合键 + 「修改」/「重置」按钮。
///
/// 录制态来自 `MainView::keymap_recording`（与设置面板其余部分同源）。
/// `on_click` 只拿得到 `&mut App`，而录制/重置是 `MainView` 的方法，
/// 故由调用方把 `Entity<MainView>` 传进来，用 `update` 回到实体上下文。
fn keybind_row(
    row: &crate::keybinds::KeymapRow,
    recording: Option<&str>,
    me: &Entity<MainView>,
    cx: &mut App,
) -> AnyElement {
    let is_recording = recording == Some(row.action_id.as_str());
    let key_text = if is_recording {
        rustshell_core::i18n::t("按下新组合键…").to_string()
    } else {
        crate::keybinds::display_spec(&row.spec)
    };
    let id_for_edit = row.action_id.clone();
    let id_for_reset = row.action_id.clone();
    let me_edit = me.clone();
    let me_reset = me.clone();

    let mut el = h_flex().gap_2().items_center().child(
        div()
            .flex_1()
            .min_w_0()
            .text_size(px(12.))
            .child(SharedString::from(row.label.clone())),
    );
    el = el.child(
        div()
            .min_w(px(104.))
            .text_size(px(12.))
            .text_color(hex(crate::theme::current(cx).fg_muted))
            .child(SharedString::from(key_text)),
    );
    // 录制态下用等宽占位，避免整行跳动。
    el = el.child(if is_recording {
        div().w(px(52.)).into_any_element()
    } else {
        Button::new(SharedString::from(format!("kb-edit-{}", row.action_id)))
            .small()
            .outline()
            .label(rustshell_core::i18n::t("修改"))
            .on_click(move |_e, _w, cx: &mut App| {
                let id = id_for_edit.clone();
                me_edit.update(cx, |m, cx| m.start_key_record(&id, cx));
            })
            .into_any_element()
    });
    el = el.child(if row.overridden && !is_recording {
        Button::new(SharedString::from(format!("kb-reset-{}", row.action_id)))
            .small()
            .ghost()
            .label(rustshell_core::i18n::t("重置"))
            .on_click(move |_e, _w, cx: &mut App| {
                let id = id_for_reset.clone();
                me_reset.update(cx, |m, cx| m.reset_key_binding(&id, cx));
            })
            .into_any_element()
    } else {
        div().w(px(1.)).into_any_element()
    });
    el.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::prod;

    /// 回归（2026-09-28 用户要求）：窗口页**不再**摆「启动时会话恢复」下拉。
    ///
    /// 2026-09-29 更新：用户进一步要求「取消恢复上次会话功能」，启动**不再自动**
    /// 弹「恢复上次会话？」框、也不再自动重建标签。原先挂在该框上的「下次不再询问」
    /// 勾选与 `save_restore_mode` 写入点一并撤下（已无「自动问」可关）。
    ///
    /// 本用例两头都断言**当前**事实：
    /// * 面板上没有「启动时会话恢复」下拉（2026-09-28 口径不变）；
    /// * `Settings::session_restore` 字段**仍在**（控件与自动行为可撤，字段不能删，
    ///   否则老设置反序列化丢值）；
    /// * 启动分发里**不得**再置 `prompt_arm` / 调 `layout_begin_restore`（不自动弹框、
    ///   不自动重建）；恢复只由用户手动触发（`layout_restore_now` 置旗）。
    #[test]
    fn the_window_page_no_longer_offers_the_restore_dropdown() {
        let page = prod(include_str!("settings_panel.rs"));
        // ① 面板上不许再有这一项（标签文案是它的唯一锚点）。
        //
        // 针必须**拼**出来，不能整段写成 `t("启动时会话恢复")`：`core::i18n` 的
        // `g1_every_t_call_has_an_entry` 扫的是**原始源码**（含测试模块），
        // 会把断言里的针当成一次真实调用，要求词条表里有这个 key ——
        // 而这一项恰恰是刚被撤下的，表里已经删了 ⇒ g1 红。用 `concat!` 拼，
        // 源码里出现的形态是 `t(\"…\")`（转义引号），`call_literals()` 的
        // `bytes[j] != b'"'` 判定会直接跳过它。
        let needle = concat!("t(\"", "启动时会话恢复", "\")");
        assert!(
            !page.contains(needle),
            "窗口页又摆回了「启动时会话恢复」下拉：用户要求撤下这一项。\n\
             启动行为由「启动时打开」一处口径决定即可。"
        );
        // ② 字段必须在（数据兼容），但启动**不许**再自动弹框 / 自动恢复。
        let core_settings = prod(include_str!("../../../core/src/settings.rs"));
        assert!(
            core_settings.contains("pub session_restore"),
            "`Settings::session_restore` 字段被删了：面板控件与自动行为都可以撤，\n\
             字段不能 —— 会破坏老设置的兼容（反序列化丢值）。"
        );
        let layout = prod(include_str!("layout_persist.rs"));
        let startup_at = layout
            .find("fn layout_startup_with(")
            .expect("找不到 layout_startup_with");
        let startup_end = layout[startup_at..]
            .find("pub(super) fn layout_restore_now(")
            .expect("找不到 layout_restore_now（手动入口应在启动分发之后）");
        let startup_body = &layout[startup_at..startup_at + startup_end];
        assert!(
            !startup_body.contains("prompt_arm = true"),
            "启动分发里又出现了自动弹框（`prompt_arm = true`）：用户要求启动不自动恢复。\n\
             恢复只应由用户手动触发（`layout_restore_now`）。"
        );
        assert!(
            !startup_body.contains("layout_begin_restore("),
            "启动分发里又出现了自动恢复（`layout_begin_restore`）：用户要求启动不自动重建标签。"
        );
        let manual_at = layout
            .find("fn layout_restore_now(")
            .expect("找不到 layout_restore_now（手动恢复入口）");
        let manual_tail = &layout[manual_at..];
        assert!(
            manual_tail[..manual_tail.len().min(1600)].contains("prompt_arm = true"),
            "手动恢复入口 `layout_restore_now` 没置 `prompt_arm`：手动恢复打不开框。"
        );
    }

    /// 页面级「重置」按钮用的`default_value`必须与`core::settings::Settings::default`对齐，否则用户点重置会把设置改成与全新安装不一致的值。
    #[test]
    fn reset_defaults_match_settings_default() {
        let d = rustshell_core::settings::Settings::default();
        assert_eq!(DEFAULT_FONT_SIZE, d.terminal_font_size as f64);
        assert_eq!(DEFAULT_CONNECT_TIMEOUT_SECS, d.connect_timeout_secs as f64);
        assert_eq!(DEFAULT_RETRIES, d.reconnect_max_retries as f64);
        assert!(!d.auto_reconnect);
        // F-4 重连间隔：面板那行的 `default_value` 与设置默认值同源，
        // 否则点「重置」会把用户重置成 0 秒 —— 那等于忙等重连
        assert_eq!(
            DEFAULT_RETRY_INTERVAL_SECS,
            d.reconnect_interval_secs as f64
        );
        // 行高（2026-09-28 新增）：滑块默认值必须等于设置默认值，否则点「重置」
        // 会把行高拨到一个与全新安装不一致的位置
        assert_eq!(
            d.terminal_line_height,
            rustshell_core::settings::DEFAULT_TERMINAL_LINE_HEIGHT
        );
        // 启动动作（2026-09-28 新增）：默认打开本地终端
        assert_eq!(
            d.startup_action,
            rustshell_core::settings::StartupAction::LocalShell
        );
    }

    /// 生产代码正文（剥注释 + 切测试模块，见 `crate::test_support`）：
    /// 本组断言针对「面板摆了哪些条目」，注释与本测试自身的字面量表都不算数
    ///（否则「钉住旧文案」会自己把自己钉住）。
    fn production_source() -> String {
        prod(include_str!("settings_panel.rs"))
    }

    /// 现场拼出 `t("…")` 形态，供「旧词条必须消失」的负向断言用。
    ///
    /// **不能把 `t("旧词条")` 直接写成字面量**：`core::i18n` 的 g1 护栏扫的是
    /// 全仓源码且不区分测试模块，字面量会被当成一次真实调用，而那些词条已经
    /// 从表里删掉 —— 护栏会把自己判红（g2 也会因为测试代码「引用」了它，
    /// 反过来看不见真正的死词条）。
    fn t_call(key: &str) -> String {
        format!("t(\"{key}\")")
    }

    /// 回归（2026-09-17 运行反馈：「连接管理」改「连接」、「终端设置」改「终端」，
    /// 移除「外观-侧栏」，导入导出合并成一块）。
    ///
    /// 这些条目既是「改了什么」的说明，也是防止改回去的护栏。
    ///
    /// 2026-10-19 修订：光标闪烁 / 回滚行数 / 光标样式 / 多行粘贴行为 / 采样间隔
    /// 全部从面板撤下（值仍保留在 `settings.json` 里继续生效）。与 2026-09-28
    /// 「闪烁回到面板」的判定相反，这一次是用户明确要求精简面板入口。
    #[test]
    fn settings_pages_follow_the_2026_09_17_rework() {
        let code = production_source();
        for gone in ["连接管理", "终端设置", "侧栏宽度", "导出", "导入"] {
            let needle = t_call(gone);
            assert!(
                !code.contains(&needle),
                "设置面板仍残留已移除的条目：{needle}"
            );
        }
        // 2026-10-19：这五项已撤下，面板不得再摆控件。
        for gone in [
            "光标闪烁",
            "回滚行数",
            "光标样式",
            "多行粘贴行为",
            "采样间隔",
            "闪烁半周期（毫秒）",
        ] {
            let needle = t_call(gone);
            assert!(
                !code.contains(&needle),
                "设置面板仍残留已移除的条目：{needle}"
            );
        }
        // 2026-10-20 用户要求：整个「外观」页撤下（主题改由活动栏主题菜单切换，
        // 背景图功能已整体移除，2026-10-21）。页面名与页内所有条目都不得回归。
        for gone in [
            "外观",
            "主题模式",
            "背景图片",
            "遮罩浓度",
            "面板不透明度",
            "打开背景目录",
            "刷新列表",
        ] {
            let needle = t_call(gone);
            assert!(
                !code.contains(&needle),
                "设置面板仍残留已移除的「外观」页条目：{needle}"
            );
        }
        // 分组标题（大类下面的「组」）已全部撤下。
        // 断言串用 `concat!` 拼：若直接写成字面量 `.title(t("`，其中一段
        // `t("` 会被 `core::i18n` 的 g1 护栏当成一次真实的 `t()` 调用，
        // 于是护栏去词条表里查 `),` 这个「key」并把自己判红。
        assert!(
            !code.contains(concat!(".title(", "t", "(")),
            "设置面板不得再给分组加标题：「大类下面的组」要撤掉。"
        );
        // 页名走 `tp`：中文「连接 / 终端」，英文与按钮、活动栏区分（Connections / Terminal）
        assert!(
            code.contains(r#"page_title("连接", "Connections")"#),
            "「连接管理」页必须改名「连接」（英文 Connections 与按钮 Connect 区分）。"
        );
        assert!(
            code.contains(r#"page_title("终端", "Terminal")"#),
            "「终端设置」页必须改名「终端」（英文 Terminal 与活动栏 Terminals 区分）。"
        );
        // 导入导出合并成一个条目（一枚标签 + 两枚按钮）
        assert!(
            code.contains(&t_call("导入导出")) && code.contains("conn-export"),
            "导出 / 导入必须合并到「导入导出」一块里。"
        );
    }

    /// 2026-10-19 用户要求：终端页「配色方案」置顶；连接页三个导入导出按钮
    /// 的文案去掉末尾省略号（它们直接执行动作，不是「打开对话框」）。
    #[test]
    fn terminal_page_starts_with_scheme_and_import_labels_have_no_ellipsis() {
        let code = production_source();
        let at = code.find("let terminal = {").expect("找不到终端页装配");
        let end = code
            .find("let session_group = {")
            .expect("找不到连接会话组");
        let terminal = &code[at..end];
        let scheme = terminal
            .find(&t_call("配色方案"))
            .expect("终端页缺配色方案");
        for after in ["编码", "字体", "记录会话日志"] {
            let pos = terminal.find(&t_call(after)).expect("终端页缺条目");
            assert!(
                scheme < pos,
                "「配色方案」必须排在「{after}」之前（2026-10-19 用户要求置顶）"
            );
        }
        for gone in ["导出连接…", "导入连接…", "从 OpenSSH 配置导入…"] {
            let needle = t_call(gone);
            assert!(!code.contains(&needle), "按钮文案不得再带省略号：{needle}");
        }
    }

    /// 文件传输两项必须**直接绑定** `settings` 的对应字段（F-13）。
    ///
    /// 与通知页同理：`SettingField` 的 getter/setter 闭包顺手写成一份 UI 私有的
    /// 临时状态时，面板上改得动、落盘的却不是这一份 —— 而「下载后校验」是发起时
    /// **现读**的（`sftp_tab::render` 每帧快照进右键菜单），断在这条线上会表现为
    /// 「勾了也没校验」，界面看不出任何异常。并发数同理，它只在建链时被读一次。
    #[test]
    fn transfer_settings_bind_to_the_settings_struct() {
        let code = production_source();
        for field in [
            "settings.sftp_verify_downloads",
            "settings.sftp_max_concurrent",
        ] {
            assert!(
                code.contains(field),
                "文件传输设置没有绑定 `{field}` —— 面板改了，落盘的却不是这一份。"
            );
        }
        assert!(
            code.contains("SFTP_MAX_CONCURRENT_MIN") && code.contains("SFTP_MAX_CONCURRENT_MAX"),
            "并发数的上下限必须用 core 的区间常量，别在面板里另抄一份。"
        );
    }

    /// 通知（SMTP）页必须**直接绑定** `MainView::smtp_cfg`。
    ///
    /// 为什么钉这条：`SettingField` 的 getter/setter 闭包很容易顺手写成一份
    /// UI 私有的临时状态，那样面板上改得动、保存时却不落盘（或落盘的还是旧值），
    /// 而界面看起来一切正常 —— 预警邮件永远不会发到用户以为的那个地址。
    ///
    /// 口令是唯一的例外：它必须走 `MainView::smtp_cfg_pass`（不回显的输入框），
    /// 且页面里**不得**直接读写 `password_cipher`（密文可逆性不存在，
    /// 把它填进输入框只会让用户以为能读回明文）。
    #[test]
    fn notification_page_binds_to_smtp_config() {
        let code = production_source();
        assert!(
            code.contains(r#"page_title("通知", "Notifications")"#),
            "设置里必须有独立的「通知」页（英文 Notifications 与本页职责一致）。"
        );
        for field in [
            "smtp_cfg.enabled",
            "smtp_cfg.host",
            "smtp_cfg.port",
            "smtp_cfg.security",
            "smtp_cfg.username",
            "smtp_cfg.from.address",
            "smtp_cfg.from.name",
            "smtp_cfg.to",
            "smtp_cfg.cc",
            "smtp_cfg.subject_prefix",
        ] {
            assert!(
                code.contains(field),
                "通知页没有绑定 `{field}` —— 面板改了但落盘的不是这一份。"
            );
        }
        assert!(
            code.contains("smtp_cfg_pass"),
            "口令必须走不回显的 `MainView::smtp_cfg_pass`。"
        );
        assert!(
            !code.contains("smtp_cfg.password_cipher"),
            "通知页不得直接读写 `password_cipher`：密文不该出现在输入框里。"
        );
        // 2026-10-12：发送功能已实现，按钮改为「按配置完备性/发送中状态置灰」，
        // 且必须挂上真正的发送动作 —— 按钮在但点了没反应是最坏的一种。
        assert!(
            code.contains(r#"Button::new("smtp-test")"#),
            "「发送测试邮件」按钮必须存在"
        );
        assert!(
            code.contains(".on_click("),
            "测试邮件按钮必须挂发送动作（按钮在但点了没反应 = 最坏的坏法）"
        );
        assert!(
            !code.contains("发送功能尚未实现"),
            "「发送功能尚未实现」的文案应当随投递落地一并移除（否则用户被告知的东西是假的）"
        );
    }

    /// Agent 配置独立成页，且**只此一份**（2026-09-22 用户要求「设置新增 agent
    /// 配置，AI 助手中的 agent 设置归并」）。
    ///
    /// 守三件事：
    /// 1. 「Agent」页真的存在、真的进了 `Settings::pages`，四个 agent 入口都在它里面，
    ///    且仍接在 `ai_cfg` 上（搬运不得改变数据通路）；
    /// 2. AI 助手页**不得**留副本 —— 两处各摆一份，用户会以为那是两个开关，
    ///    点哪个都生效反而更像坏了；
    /// 3. AI 助手页也不能被搬空：端点 / 模型 / Key / 调试日志仍归它。
    ///
    /// 断言都切在页切片里（`let ai = {` … `let agent = {` … `let keybind = {`）：拿
    /// 全文件 `contains` 会把「另一个页里也有」判成通过。切片端点一律用代码、不用
    /// 注释 —— `production_source()` 会先剥注释。
    #[test]
    fn agent_page_owns_the_agent_controls() {
        let code = production_source();
        assert!(
            code.contains(r#"page_title("Agent", "Agent")"#),
            "设置里必须有独立的「Agent」页（中英同名，走 `page_title` 免得词条表里\
             多一条恒等映射）。"
        );
        assert!(
            code.contains(&t_call("工具调用的开关、命令权限与免确认白名单。")),
            "Agent 页必须说明它管的是什么。"
        );
        // 2026-09-22「最大工具步数移除，不做限制」：条目连同它的区间常量一起撤下。
        // 这里断的是「没有」，防的是「改回来」—— 回归的表现是用户又能设上限，
        // 而 agent 明明已经不看它了（改了个没用的数字，比看不到更糟）。
        for gone in ["最大工具步数", "agent_max_steps", "AGENT_MAX_STEPS_CAP"] {
            assert!(
                !code.contains(gone),
                "Agent 页仍残留已移除的「最大工具步数」：{gone}"
            );
        }
        // 页装配：`agent` 必须进 `Settings::pages`，否则用户根本打不开它
        let list_at = code.find(".pages(vec![").expect("页装配必须存在");
        let list_tail = &code[list_at..];
        let list = &list_tail[..list_tail.find("])").expect("页列表必须有结尾")];
        for name in [
            "connections_io",
            "window_page",
            "terminal",
            "ai",
            "agent",
            "notify",
            "keybind",
            "about",
        ] {
            assert!(
                list.contains(name),
                "`Settings::pages` 里少了 `{name}` 页（或在别处被顺手删掉了）。"
            );
        }

        // Agent 页切片（切到 `let keybind = {`：**不能用注释当端点** ——
        // `production_source()` 会先剥掉注释，拿 `// 快捷键` 找位置会直接 panic）。
        let at = code.find("let agent = {").expect("Agent 页必须存在");
        let tail = &code[at..];
        let page = &tail[..tail
            .find("let keybind = {")
            .expect("Agent 页之后应当是快捷键页")];
        for field in ["ai_cfg.agent_enabled", "ai_cfg.agent_allow_mutating"] {
            assert!(
                page.contains(field),
                "Agent 页缺少 `{field}` 对应的控件 —— v1.1 要求 UI 能开关并显示这些项。"
            );
        }
        for label in ["启用 Agent 工具调用", "允许状态变更命令", "免确认白名单"]
        {
            assert!(
                page.contains(&t_call(label)),
                "Agent 页缺少「{label}」条目。"
            );
        }
        // 白名单的编辑面**不在页上**：2026-09-22 运行反馈改成「右侧一个配置按钮 +
        // 弹窗」（条目内联铺在页上会把整页撑开）。列表 / 搜索 / 逐条开关都归弹窗，
        // 见 `main_view::whitelist_dialog` 自己的护栏。
        assert!(
            page.contains("open_ai_whitelist_dialog"),
            "「免确认白名单」右侧必须有「配置」按钮，点开的是白名单配置框。"
        );
        assert!(
            !page.contains("whitelist_editor"),
            "白名单编辑器不该再内联铺在 Agent 页上（已改为弹窗）。"
        );

        // AI 助手页切片：不得留 agent 副本，也不能被搬空
        let ai_at = code.find("let ai = {").expect("AI 助手页必须存在");
        let ai_tail = &code[ai_at..];
        let ai_page = &ai_tail[..ai_tail
            .find("let agent = {")
            .expect("AI 助手页之后是 Agent 页")];
        for gone in ["agent_enabled", "agent_allow_mutating", "auto_approve"] {
            assert!(
                !ai_page.contains(gone),
                "AI 助手页仍留着 `{gone}` —— agent 设置只能有 Agent 页那一份。"
            );
        }
        for keep in [
            "ai_cfg_base",
            "ai_cfg_model",
            "ai_cfg_key",
            "ai_cfg.debug_log",
        ] {
            assert!(
                ai_page.contains(keep),
                "AI 助手页丢了 `{keep}` —— 端点 / 模型 / Key / 调试日志仍归它。"
            );
        }
        assert!(
            ai_page.contains(&t_call("AI 调试日志")),
            "「AI 调试日志」不该跟着 agent 一起搬走：它记的是 AI 的请求与流式原文。"
        );
    }

    /// 回归（2026-09-21 用户要求）：设置 - AI 助手**不再**有「模型能力」条目。
    ///
    /// 连带撤掉的是那三个只在面板上出现过的词条（`工具调用` / `流式输出` /
    /// 说明文案）；`支持 / 不支持 / 未知` 三态仍被 core 侧
    /// `CapabilityState::label` 使用，**不要**顺手删表里那三条。
    #[test]
    fn ai_page_drops_the_model_capabilities_item() {
        let code = production_source();
        for gone in [
            "模型能力",
            "由真实请求探测并缓存；「未知」表示尚未探测到。",
            "工具调用",
            "流式输出",
        ] {
            let needle = t_call(gone);
            assert!(
                !code.contains(&needle),
                "设置面板仍残留已移除的条目：{needle}"
            );
        }
        assert!(
            !code.contains("ai_cfg.capabilities"),
            "AI 页不该再读 `ai_cfg.capabilities` —— 能力只用于请求侧降级判断。"
        );
    }

    /// AI 数值项的合法区间必须来自 core 常量，而不是面板里另写一份， 否则控件允许的范围会和请求体侧的 clamp 不一致。
    #[test]
    fn ai_ranges_come_from_core_constants() {
        let d = rustshell_core::ai::AiConfig::default();
        assert_eq!(rustshell_core::ai::DEFAULT_TEMPERATURE, d.temperature);
        assert!(
            (rustshell_core::ai::TEMPERATURE_MIN..=rustshell_core::ai::TEMPERATURE_MAX)
                .contains(&d.temperature)
        );
    }

    /// 回归（2026-09-18 运行反馈：「采样温度改为滑动轴，0.1 一个间隔」、
    /// 「取消 AI 上下文限制配置」）。
    ///
    /// 两条要求都能落到「数字/控件」上，所以断言的是控件与常量名，不是文案：
    /// - 温度：面板里必须是官方 `Slider`；步长只能是 `TEMPERATURE_STEP`（0.1），
    ///   而装配点在 `main_view.rs`（见该处注释），故步长断言扫的是那个文件；
    /// - 上下文行数：**整条撤下**。面板不再有该条目，`AiConfig` 不再有该字段，
    ///   注入行数只剩 core 里一个常量（`CONTEXT_LINES`）。
    #[test]
    fn ai_page_uses_a_slider_and_drops_context_lines() {
        let code = production_source();
        assert!(
            code.contains("Slider::new("),
            "采样温度必须换成官方滑动轴（Slider），不能再是数字输入框。"
        );
        // 步长断言落在 `main_view.rs` 而不是本文件：滑块**状态**（含 min/max/step）
        // 建在 `MainView::new`，本页只把那个 `Entity<SliderState>` 摆上去。第一版把检查
        // 写在这里，等于在检查一个从不出现该常量的文件（于是就真的红了）。
        // 负向断言用 `.step(0.`：裸字面量只有这一个写法，写死它比写死「步长=0.1」更能
        // 挡住「0.1 换成 0.2 顺手也把常量丢了」这种回归。
        let mv = prod(include_str!("../main_view.rs"));
        assert!(
            mv.contains(".step(TEMPERATURE_STEP)"),
            "滑块步长必须用 `TEMPERATURE_STEP` 常量（0.1），不要在装配处写裸字面量。"
        );
        assert!(
            !mv.contains(".step(0."),
            "装配处出现了裸字面量步长 —— 0.1 的口径只能留在 `TEMPERATURE_STEP` 里。"
        );
        // 常量值本身也钉住：要求原话是「0.1 一个间隔」，光有常量名挡不住有人把它改成 0.05。
        assert_eq!(
            crate::main_view::TEMPERATURE_STEP,
            0.1,
            "采样温度的步长口径是 0.1（运行反馈「0.1 一个间隔」）。"
        );
        assert!(
            !code.contains(&t_call("上下文行数")),
            "设置面板又出现了「上下文行数」条目 —— 该项已按运行反馈取消。"
        );
        // core 侧：字段与区间常量都必须消失，只留一个常量（防止「面板撤了、模型还在，
        // 于是又有人照着字段把控件接回面板」）。
        let ai = prod(include_str!("../../../core/src/ai.rs"));
        assert!(
            !ai.contains("pub context_lines"),
            "`AiConfig` 里不该再有 `context_lines` 字段：它不是配置项了。"
        );
        assert!(
            ai.contains("pub const CONTEXT_LINES"),
            "注入上下文行数必须收口成 core 里的 `CONTEXT_LINES` 常量。"
        );
    }

    /// 回归（2026-10-22 用户要求「设置中增加关于：描述此软件，跳转 GitHub，检查更新」）。
    ///
    /// 与 2026-09-18 的「移除配置-关于」并存的口径：
    /// - 「关于」页回来了，且必须有软件简介、GitHub 入口、检查更新三个落点；
    /// - 当时一并撤下的「导出 / 复制诊断信息」「导出 / 导入键位」**不再回归** ——
    ///   它们会替用户把本机路径写进文件或弹资源管理器，属于「用户没要就替用户
    ///   做决定」的动作。护栏两头都断：该有的在、不该有的不在。
    #[test]
    fn about_page_returns_without_the_diagnostics_entrypoints() {
        let code = production_source();
        // ① 关于页存在，且三个落点都在。
        assert!(
            code.contains(&t_call("关于")),
            "设置面板缺少「关于」页（2026-10-22 用户要求恢复）。"
        );
        let at = code.find("let about = {").expect("「关于」页装配必须存在");
        let tail = &code[at..];
        let page = &tail[..tail
            .find("let notify = {")
            .expect("「关于」页之后应当是通知页")];
        for needle in [
            "RustShell 是一款基于 Rust 与 GPUI 构建的跨平台 SSH / SFTP 终端",
            "about-github",
            "about-check-update",
            // 2026-10-28：按钮改走「一枚按钮按状态换语义」的统一入口
            // （检查 / 立即更新 / 重启 / 重试），不再是固定的 `check_for_updates`。
            "update_primary_action",
        ] {
            assert!(
                page.contains(needle),
                "「关于」页缺少 `{needle}` —— 简介 / GitHub 跳转 / 检查更新缺一不可。"
            );
        }
        assert!(
            page.contains("rustshell_core::update::REPO_URL"),
            "GitHub 入口必须用 `core::update::REPO_URL`，不要在 UI 里另抄一份地址。"
        );
        // ② 诊断 / 键位导入导出仍保持撤下。
        for gone in ["导出诊断信息", "复制诊断信息", "导出键位", "导入键位"] {
            let needle = t_call(gone);
            assert!(
                !code.contains(&needle),
                "设置面板仍残留已移除的条目：{needle}"
            );
        }
        for gone in [
            "about-export-diag",
            "about-copy-diag",
            "about-open-log",
            "about-open-cfg",
            "kb-export",
            "kb-import",
        ] {
            assert!(!code.contains(gone), "设置面板仍残留已移除的按钮 `{gone}`");
        }
    }
}

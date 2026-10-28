//! 设置域：设置面板、主题三态切换与全局生效、配置目录入口。 本模块为`main_view`的子模块，可直接访问`MainView`的私有字段。

use gpui::{Context, Window};

use rustshell_platform::paths;

use rustshell_core::alerts::SmtpChannel;

use super::{DialogMode, MainView};

impl MainView {
    /// 编辑配置文件：文件管理器打开配置目录并选中 settings.json
    pub fn open_config_folder(&mut self, cx: &mut Context<Self>) {
        let p = paths::settings_path();
        let _ = std::fs::create_dir_all(p.parent().unwrap_or(&p));
        if !p.exists() {
            let _ = std::fs::write(&p, "{}");
        }
        // 三平台抽象，替代硬编码 explorer
        rustshell_platform::shell_open::reveal_in_file_manager(&p);
        cx.notify();
    }

    // 设置面板

    pub fn open_settings(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        // 快照当前设置与 AI 配置；取消 / ESC / 点遮罩 / 右上角 X 均回滚到此。
        // 起字段由官方`Settings`组件直接读写`settings`/`ai_cfg`，
        // 不再需要逐字段回填 InputState（旧实现有 8 个面板专用输入框 到 已删除）。
        self.io.set_snapshot = Some(super::settings_panel::SettingSnapshot {
            settings: self.settings.clone(),
            ai_cfg: self.ai_ui.ai_cfg.clone(),
            smtp_cfg: self.io.smtp_cfg.clone(),
        });
        // AI 端点/模型仍需回填：这两处输入框与 AI 面板共用同一份 InputState。
        // Key 一律不回显（密文不可逆），留空即沿用已保存的 Key。
        self.prefill_ai_inputs(w, cx);
        self.prefill_smtp_inputs(w, cx);
        self.prefill_proxy_inputs(w, cx);
        self.io.set_hint = None;
        // 键位区：每次打开重建快照（外部改动 keymap.json 也能反映）
        self.cancel_key_record();
        self.panel_ui.keymap_rows = crate::keybinds::snapshot_rows();
        self.panel_ui.keymap_hint = None;
        // 拖动位移每次打开归零：面板回到默认落点，避免上次拖出去的位置
        // （尤其是靠近窗口边缘的）让用户以为设置打不开。
        self.io.set_drag_offset = (0.0, 0.0);
        self.io.set_drag_from = None;
        self.present_dialog(DialogMode::Settings, w, cx);
    }

    /// 取消设置面板：回滚到打开时的快照。 覆盖三条关闭路径，「取消」按钮、ESC/点遮罩/右上角 X（走官方`on_close`）， 由`dialogs.rs`在关闭前统一调用；保存路径会先清空`set_snapshot`，故不会误回滚。
    pub(super) fn revert_settings(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let Some(prev) = self.io.set_snapshot.take() else {
            return;
        };
        let theme_changed = prev.settings.theme_mode != self.settings.theme_mode;
        self.settings = prev.settings;
        self.ai_ui.ai_cfg = prev.ai_cfg;
        self.io.smtp_cfg = prev.smtp_cfg;
        // 共用的 AI 输入框跟着回到快照；Key 框清空（本就不回显）
        self.prefill_ai_inputs(w, cx);
        self.ai_ui
            .ai_cfg_key
            .update(cx, |st, cx| st.set_value("", w, cx));
        self.prefill_smtp_inputs(w, cx);
        // 代理同理（F-10）：口令框清空 + `proxy_cfg_prev` 退回快照那份 ——
        // 面板里的地址/端口是双向绑定的，回滚后不重取快照，「目标变没变」的判定
        // 就会拿着用户改过的那份去比，沿用判定直接失效。
        self.prefill_proxy_inputs(w, cx);
        // 主题是即时预览的，回滚时要重新生效
        if theme_changed {
            self.apply_theme_everywhere(cx);
        }
        self.apply_terminal_palette(cx);
        self.io.set_hint = None;
        cx.notify();
    }

    /// AI 端点/模型输入框回填（AI 面板展开配置区、设置面板打开时共用）。 不触碰 Key 输入框，密文不可逆，回显没有意义且会诱使用户误以为可读。
    pub fn prefill_ai_inputs(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let base = self.ai_ui.ai_cfg.base_url.clone();
        let model = self.ai_ui.ai_cfg.model.clone();
        self.ai_ui
            .ai_cfg_base
            .update(cx, |st, cx| st.set_value(base, w, cx));
        self.ai_ui
            .ai_cfg_model
            .update(cx, |st, cx| st.set_value(model, w, cx));
        // 温度滑块也要跟着回到配置值：它与 `ai_cfg.temperature` 是两个副本
        // （滑块手里那份是它自己的状态），只在拖动时单向写进 `ai_cfg`。
        // 少了这一步，「改完温度按取消」会把配置回滚、滑块却停在拖到的位置 ——
        // 面板显示 1.8、实际存的是 0.3，下一次打开还会照 1.8 显示。
        let temp = self.ai_ui.ai_cfg.temperature.clamp(
            rustshell_core::ai::TEMPERATURE_MIN,
            rustshell_core::ai::TEMPERATURE_MAX,
        );
        self.ai_ui
            .ai_temp_slider
            .update(cx, |st, cx| st.set_value(temp, w, cx));
    }

    /// SMTP 口令输入框回填（打开设置面板 / 回滚时调用）。
    ///
    /// 只是**清空**：密文不可逆，回显没有意义，反而会诱使用户以为能读回明文。
    /// 留空 = 沿用已保存的口令（见 [`MainView::sync_smtp_config`]）。
    pub(super) fn prefill_smtp_inputs(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        self.io
            .smtp_cfg_pass
            .update(cx, |st, cx| st.set_value("", w, cx));
    }

    /// 全局代理（F-10）的**运行期形态**：把 `password_ref` 解成明文口令。
    ///
    /// 每个建连入口现算一次，而不是启动时解好存一份：设置面板里改了口令之后，
    /// 缓存的那份会让「改了设置、新连接还是旧口令」有藏身处。
    /// 解密失败（凭据库不可用 / 换了机器）时口令为空 —— 让代理去拒绝
    /// （407 / 认证失败），比在这里假装「没配代理」更诚实：后者会静默直连。
    pub(super) fn runtime_global_proxy(&self) -> Option<rustshell_core::proxy::ProxyConfig> {
        self.settings
            .runtime_proxy(rustshell_platform::secret::unprotect)
    }

    /// 把设置面板上的 SMTP 配置落盘（`smtp.json`）。
    ///
    /// 与 [`MainView::sync_ai_config`] 同一条口径：口令只在**非空**时才重新加密写入，
    /// 留空即沿用 `password_cipher` 里那份 —— 面板不回显密文，用户改了别处再点保存
    /// 不该把口令弄丢。
    ///
    /// 校验由 `SmtpConfig::validate` 负责（未启用时只存不校，允许先填一半）。
    /// 返回 `Err` 时调用方就地提示并**保持面板打开**。
    pub(super) fn sync_smtp_config(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        let plain = self.io.smtp_cfg_pass.read(cx).value().trim().to_string();
        if !plain.is_empty() {
            match rustshell_platform::secret::protect(
                rustshell_core::notify_smtp::SECRET_KEY,
                &plain,
            ) {
                Ok(cipher) => self.io.smtp_cfg.password_cipher = Some(cipher),
                Err(_) => {
                    return Err(
                        rustshell_core::i18n::t("SMTP 口令加密失败（系统凭据存储不可用）")
                            .to_string(),
                    )
                }
            }
            // 输入框里的明文不清：`sync_smtp_config` 拿不到 `Window`（`set_value` 需要），
            // 而保存成功后设置面板随即关闭，明文不会留在屏幕上；下次打开面板
            // `prefill_smtp_inputs` 会把它清掉。
        }
        let errs = self.io.smtp_cfg.validate();
        if !errs.is_empty() {
            return Err(errs.join("；"));
        }
        self.io.smtp_cfg.save(&paths::smtp_path()).map_err(|e| {
            rustshell_core::i18n::ts(format!("保存失败: {e}"), format!("Save failed: {e}"))
        })
    }

    /// 代理口令输入框回填 + 记录「打开时的全局代理」（打开设置面板 / 回滚时调用）。
    ///
    /// 输入框只是**清空**（密文不可逆，回显没有意义）；`proxy_cfg_prev` 才是关键 ——
    /// 面板里的地址/端口是**双向绑定**的，改一下 `settings.proxy` 就变了，
    /// 没有这份快照就无从知道「用户改没改代理目标」（见 [`MainView::sync_proxy_config`]）。
    pub(super) fn prefill_proxy_inputs(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        self.io
            .proxy_cfg_pass
            .update(cx, |st, cx| st.set_value("", w, cx));
        self.io.proxy_cfg_prev = self.settings.proxy.clone();
    }

    /// 把设置面板上的全局代理配置定稿到 `settings.proxy`（真正落盘由 `settings_save` 完成）。
    ///
    /// 与 [`MainView::sync_smtp_config`] 同一条口径：口令只在**非空**时才重新加密写入，
    /// 留空即沿用打开面板时那份 —— 面板不回显密文，用户改了别处再点保存不该把口令弄丢。
    /// 唯一的加强：**代理目标变了就不沿用**（类型 / 地址 / 端口任一不同）——
    /// 那把口令属于旧代理，带给另一个代理既是误导也可能是泄漏
    /// （`core::proxy::may_inherit_password` 是这条判定的唯一落点，对话框同一份）。
    ///
    /// 地址与用户名在这里 `normalize`（去首尾空白、空用户名归 `None`）：面板的输入框
    /// 每敲一个字就写一次配置，逐键 trim 会把用户正在敲的空格吃掉。
    /// 返回 `Err` 时调用方就地提示并**保持面板打开**。
    pub(super) fn sync_proxy_config(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        let Some(mut cfg) = self.settings.proxy.clone() else {
            return Ok(()); // 开关关着 = 不用代理：什么都不存，也不校验
        };
        cfg.normalize();
        if let Some(e) = cfg.validate() {
            return Err(rustshell_core::proxy::validate_error_text(e).to_string());
        }
        let typed = self.io.proxy_cfg_pass.read(cx).value().to_string();
        if !typed.is_empty() {
            match rustshell_platform::secret::protect(
                rustshell_core::proxy::GLOBAL_SECRET_KEY,
                &typed,
            ) {
                Ok(cipher) => {
                    cfg.password_ref = Some(cipher);
                    // 运行期明文跟着更新：本次会话里新建的连接直接用这份，
                    // 不必等下次启动重新解密。
                    cfg.password = Some(typed);
                }
                Err(_) => {
                    return Err(
                        rustshell_core::i18n::t("代理口令加密失败（系统凭据存储不可用）")
                            .to_string(),
                    )
                }
            }
        } else {
            let origin = self
                .io
                .proxy_cfg_prev
                .as_ref()
                .map(rustshell_core::proxy::fingerprint);
            if rustshell_core::proxy::may_inherit_password(origin.as_ref(), &cfg) {
                if let Some(prev) = &self.io.proxy_cfg_prev {
                    cfg.password_ref = prev.password_ref.clone();
                    cfg.password = prev.password.clone();
                }
            } else {
                // 目标变了：旧口令**和它一起走**（清掉），下次建连时要么用户重填，
                // 要么由代理回 407 —— 两条路都比「把旧代理的凭据送给新代理」诚实。
                cfg.password_ref = None;
                cfg.password = None;
            }
        }
        self.settings.proxy = Some(cfg);
        Ok(())
    }

    pub fn toggle_theme(&mut self, cx: &mut Context<Self>) {
        use rustshell_core::theme::ThemeMode;
        // Dark 到 Light 两态循环（2026-10-20 起不再有「跟随系统」）
        self.set_theme_mode(
            match self.settings.theme_mode {
                ThemeMode::Dark => ThemeMode::Light,
                ThemeMode::Light => ThemeMode::Dark,
            },
            cx,
        );
    }

    /// 切换到指定主题模式
    pub fn set_theme_mode(
        &mut self,
        mode: rustshell_core::theme::ThemeMode,
        cx: &mut Context<Self>,
    ) {
        self.settings.theme_mode = mode;
        self.apply_theme_everywhere(cx);
    }

    /// 主题变更后统一生效：UI Token + 组件库 Theme + 全部终端配色 注意：终端配色走`resolve_terminal_palette`，「跟随主题」方案下等价于 原来的`TerminalPalette::resolved`，但选了预设/自定义方案时不被主题覆盖。
    pub(super) fn apply_theme_everywhere(&mut self, cx: &mut Context<Self>) {
        crate::theme::apply_theme_mode(cx, self.settings.theme_mode);
        let new_palette = self.settings.resolve_terminal_palette();
        // 终端底色：不透明（alpha = 1.0）
        let bg_alpha = 1.0;
        for tab in &self.tabs_state.tabs {
            let Some(term) = tab.terminal.clone() else {
                continue;
            };
            term.update(cx, |tv, tcx| {
                let mut c = tv.config().clone();
                c.colors = crate::terminal_view::to_color_palette_a(&new_palette, bg_alpha);
                tv.update_config(c, tcx);
            });
        }
        let _ = self.settings.save(&paths::settings_path());
        cx.notify();
    }

    // 终端配色

    /// 切换终端配色方案（设置-终端「配色方案」下拉）。 选到`custom`时首次进入会以当前解析出的配色为初值落库，避免用户一进去 看到全 0 色。切换后立即作用于 已打开的终端（字体/配色即时生效，编码才需新建）。
    pub fn set_terminal_scheme(&mut self, key: &str, cx: &mut Context<Self>) {
        use rustshell_core::theme::SCHEME_CUSTOM;
        self.settings.terminal_color_scheme = key.to_string();
        if key == SCHEME_CUSTOM && self.settings.terminal_custom_palette.is_none() {
            // 初始值 = 当前主题解析结果（走同一入口，保证与视觉现状一致）
            let pal = self.settings.resolve_terminal_palette();
            self.settings.terminal_custom_palette =
                Some(rustshell_core::settings::CustomPalette::from_palette(&pal));
        }
        self.apply_terminal_palette(cx);
    }

    /// 自定义配色：点击某个色块 → 在预设色环里取下一个颜色。
    ///
    /// 不用官方 `ColorPicker`（19 个槽各需 `Entity<ColorPickerState>`，
    /// 其状态不属于 `MainView`，与设置面板「快照 / 回滚」语义冲突）。
    /// 这里只改 `settings.terminal_custom_palette`，唯一事实源在 settings，
    /// 回滚天然成立。
    pub(super) fn cycle_custom_color(
        &mut self,
        target: super::settings_panel::PaletteTarget,
        cx: &mut Context<Self>,
    ) {
        use super::settings_panel::PaletteTarget;
        // 点击时若方案不是 custom，先切过去（用户预期是一点就能改）
        if self.settings.terminal_color_scheme != rustshell_core::theme::SCHEME_CUSTOM {
            self.set_terminal_scheme(rustshell_core::theme::SCHEME_CUSTOM, cx);
        }
        let Some(mut cp) = self.settings.terminal_custom_palette.clone() else {
            return;
        };
        const RING: &[u32] = &[
            0x0D1117, 0x161B22, 0x1C2128, 0x30363D, 0xE6EDF3, 0xB6C2CF, 0x8B949E, 0x58A6FF,
            0x3FB950, 0xD2A8FF, 0xD29922, 0xF85149, 0x008080, 0x800000, 0x808000, 0x000080,
        ];
        let slot: &mut u32 = match target {
            PaletteTarget::Background => &mut cp.background,
            PaletteTarget::Foreground => &mut cp.foreground,
            PaletteTarget::Cursor => &mut cp.cursor,
            PaletteTarget::Normal(i) if i < 8 => &mut cp.normal[i],
            PaletteTarget::Bright(i) if i < 8 => &mut cp.bright[i],
            _ => return,
        };
        let next = RING
            .iter()
            .position(|c| *c == *slot)
            .map(|i| RING[(i + 1) % RING.len()])
            .unwrap_or(RING[0]);
        *slot = next;
        self.settings.terminal_custom_palette = Some(cp);
        self.apply_terminal_palette(cx);
    }

    /// 自定义配色「恢复默认」：重置为当前主题解析出的配色。
    pub fn reset_custom_palette(&mut self, cx: &mut Context<Self>) {
        let pal = self.settings.resolve_terminal_palette();
        self.settings.terminal_custom_palette =
            Some(rustshell_core::settings::CustomPalette::from_palette(&pal));
        self.apply_terminal_palette(cx);
    }

    /// 把当前`settings`里解析出的终端配色推给所有已打开的终端（字体/配色即时生效）。
    pub(super) fn apply_terminal_palette(&mut self, cx: &mut Context<Self>) {
        let new_palette = self.settings.resolve_terminal_palette();
        // 终端底色：不透明（alpha = 1.0）
        let bg_alpha = 1.0;
        for tab in &self.tabs_state.tabs {
            let Some(term) = tab.terminal.clone() else {
                continue;
            };
            term.update(cx, |tv, tcx| {
                let mut c = tv.config().clone();
                c.colors = crate::terminal_view::to_color_palette_a(&new_palette, bg_alpha);
                tv.update_config(c, tcx);
            });
        }
        cx.notify();
    }

    // 连接导入 / 导出（2026-09-15 起走系统原生文件对话框）

    /// 导出连接：弹系统「另存为」对话框，用户挑位置并确认后写盘。
    ///
    /// 对话框是异步的（`prompt_for_new_path` 返回 oneshot），所以这里不阻塞 UI，
    /// 等结果的任务挂在本次更新之后。真正写盘的是 [`MainView::write_connections_export`]，
    /// 与对话框解耦，便于单测。
    pub fn export_connections(&mut self, cx: &mut Context<Self>) {
        let dir = paths::connections_export_dir();
        // 目录由本程序先建出来，否则「另存为」会开在一个不存在的位置
        paths::ensure_dir(&dir);
        let suggested = format!(
            "rustshell-connections-{}.json",
            rustshell_core::connections::unix_now_secs()
        );
        let rx = cx.prompt_for_new_path(&dir, Some(&suggested));
        cx.spawn(async move |this, cx| {
            match rx.await {
                Ok(Ok(Some(file))) => {
                    let _ = this.update(cx, |m, cx| m.write_connections_export(file, cx));
                }
                // 用户取消：静默返回。主动弹一句「已取消」纯属噪音。
                Ok(Ok(None)) => {}
                // 等待结果的中断：窗口已关，没人可通知。
                Err(_) => {}
                Ok(Err(e)) => {
                    log::error!("打开连接导出对话框失败: {e}");
                    let _ = this.update(cx, |m, cx| {
                        m.set_info(
                            format!("{}: {e}", rustshell_core::i18n::t("导出文件写入失败")),
                            false,
                        );
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    /// 把连接写进用户选定的文件：对话框之外的全部导出逻辑（可单测）。
    ///
    /// 导出体刻意不是 `ConnectionProfile`（见 `core::connections::ExportedProfile`），
    /// 因此**不含任何口令 / 密钥口令**，可以安全地传给同事或提交到私有仓库。
    /// 目标已存在时不自己拦：用户是在系统对话框上按过「确认覆盖」的，
    /// 再弹一次「文件已存在」只会显得程序没听见他。
    pub fn write_connections_export(&mut self, file: std::path::PathBuf, cx: &mut Context<Self>) {
        let exp = self
            .conn_store
            .export(rustshell_core::connections::unix_now_secs());
        let text = match rustshell_core::connections::ConnectionStore::export_to_json(&exp) {
            Ok(t) => t,
            Err(e) => {
                log::error!("连接导出序列化失败: {e}");
                self.set_info(
                    rustshell_core::i18n::t("导出文件写入失败").to_string(),
                    false,
                );
                cx.notify();
                return;
            }
        };
        if let Err(e) = std::fs::write(&file, text) {
            log::error!("连接导出写入失败: {} err={e}", file.display());
            self.set_info(
                format!("{}: {e}", rustshell_core::i18n::t("导出文件写入失败")),
                false,
            );
            cx.notify();
            return;
        }
        log::info!("连接已导出: {}", file.display());
        let n = exp.profiles.len();
        self.set_info(
            rustshell_core::i18n::ts(
                format!("已导出 {n} 个连接到 {}", file.display()),
                format!("Exported {n} connection(s) to {}", file.display()),
            ),
            true,
        );
        cx.notify();
    }

    /// 导入连接：弹系统「打开」对话框，用户挑文件后解析 到 合并 到 落盘。
    /// 对话框是异步的（`prompt_for_paths` 返回 oneshot），等结果的任务挂在本次更新之后。
    pub fn import_connections(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            match rx.await {
                Ok(Ok(Some(files))) => {
                    let Some(file) = files.into_iter().next() else {
                        return;
                    };
                    let _ = this.update(cx, |m, cx| m.import_connections_from(file, cx));
                }
                // 用户取消：静默返回。主动弹一句「已取消」纯属噪音。
                Ok(Ok(None)) => {}
                // 等待结果的中断：窗口已关，没人可通知。
                Err(_) => {}
                Ok(Err(e)) => {
                    log::error!("打开连接导入对话框失败: {e}");
                    let _ = this.update(cx, |m, cx| {
                        m.set_info(
                            format!("{}: {e}", rustshell_core::i18n::t("读取导入文件失败")),
                            false,
                        );
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    /// 从用户选定的文件导入：解析 到 合并（按 host+port+user 去重，id 重排） 到 落盘。
    /// 合并是幂等的：同一文件导入两次，第二次`added == 0`并明确提示，不会产生重复连接。
    pub fn import_connections_from(&mut self, file: std::path::PathBuf, cx: &mut Context<Self>) {
        if file.is_dir() {
            self.set_info(
                format!(
                    "{}：{}",
                    rustshell_core::i18n::t("这是一个目录，请选择 json 文件"),
                    file.display()
                ),
                false,
            );
            cx.notify();
            return;
        }
        let text = match std::fs::read_to_string(&file) {
            Ok(t) => t,
            Err(e) => {
                log::error!("读取导入文件失败: {} err={e}", file.display());
                self.set_info(
                    format!("{}: {e}", rustshell_core::i18n::t("读取导入文件失败")),
                    false,
                );
                cx.notify();
                return;
            }
        };
        let exp = match rustshell_core::connections::parse_import(&text) {
            Ok(e) => e,
            Err(e) => {
                log::warn!("解析导入文件失败: {} err={e}", file.display());
                self.set_info(
                    format!("{}: {e}", rustshell_core::i18n::t("读取导入文件失败")),
                    false,
                );
                cx.notify();
                return;
            }
        };
        // 导入前记下既有分组 id：新登记的分组要顺手展开，
        // 否则连接导进来了却在收起的分组里，用户以为「什么都没发生」。
        let before: std::collections::HashSet<String> = self
            .conn_store
            .groups
            .iter()
            .map(|g| g.id.clone())
            .collect();
        let outcome = self.conn_store.merge_import(&exp);
        if outcome.is_empty() {
            self.set_info(
                rustshell_core::i18n::t("该文件里没有新的连接").to_string(),
                false,
            );
            cx.notify();
            return;
        }
        if let Err(e) = self.conn_store.save(&paths::connections_path()) {
            log::error!("导入后保存连接失败: {e}");
            self.set_info(
                format!("{}: {e}", rustshell_core::i18n::t("导出文件写入失败")),
                false,
            );
            cx.notify();
            return;
        }
        for g in &self.conn_store.groups {
            if !before.contains(&g.id) {
                self.conn_ui.expanded_groups.insert(g.id.clone());
            }
        }
        log::info!(
            "连接导入完成: 新增 {} 个、跳过 {} 个、新增分组 {} 个（来源 {}）",
            outcome.added,
            outcome.skipped,
            outcome.groups_added,
            file.display()
        );
        let msg = rustshell_core::i18n::ts(
            format!(
                "导入完成：新增 {} 个连接，跳过 {} 个重复{}",
                outcome.added,
                outcome.skipped,
                if outcome.groups_added > 0 {
                    format!("，新增 {} 个分组", outcome.groups_added)
                } else {
                    String::new()
                }
            ),
            format!(
                "Imported {} connection(s), skipped {} duplicate(s)",
                outcome.added, outcome.skipped
            ),
        );
        self.set_info(msg, true);
        cx.notify();
    }

    /// 发送一封测试邮件（设置面板「发送测试邮件」按钮）。
    ///
    /// **走的是与真实预警完全同一条链路**（`SmtpChannel::deliver`）——
    /// 刻意不另写一条「测试专用」路径：测试邮件能发出去，不代表预警能发出去，
    /// 而两条路径分叉的代价（用户测通了、真正的告警发不出去）远大于这点代码复用。
    ///
    /// 发信是**网络操作**，不能阻塞 UI 线程 ⇒ 丢进后台线程，完成后回主线程
    /// 更新提示条（`cx.spawn` 的 `detach` 订阅）。
    pub fn send_test_mail(&mut self, cx: &mut Context<Self>) {
        // 先把输入框里的口令加密落盘，否则测试用的是旧配置
        if let Err(e) = self.sync_smtp_config(cx) {
            self.set_info(e, false);
            cx.notify();
            return;
        }
        let cfg = self.io.smtp_cfg.clone();
        let problems = cfg.validate();
        if !problems.is_empty() {
            self.set_info(problems.join("；"), false);
            cx.notify();
            return;
        }
        // 置「正在发」标记：避免用户连点两下发出两封
        self.io.smtp_testing = true;
        self.set_info(
            rustshell_core::i18n::ts("正在发送测试邮件…".into(), "Sending a test email…".into()),
            true,
        );
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = SmtpChannel::with_unprotect(cfg, rustshell_platform::secret::unprotect)
                .deliver(&Self::test_notification());
            // 回主线程收尾（`this` 是 MainView 的弱引用）
            let _ = this.update(cx, |m, cx| {
                m.io.smtp_testing = false;
                match result {
                    Ok(rep) => {
                        let n = rep.recipients.len();
                        let tls = if rep.used_tls {
                            rustshell_core::i18n::ts("（已加密）".into(), " (encrypted)".into())
                        } else {
                            rustshell_core::i18n::ts("（明文）".into(), " (unencrypted)".into())
                        };
                        m.set_info(
                            rustshell_core::i18n::ts(
                                format!("测试邮件已发送到 {n} 个收件人{tls}"),
                                format!("Test email sent to {n} recipient(s){tls}"),
                            ),
                            true,
                        );
                    }
                    Err(e) => {
                        m.set_info(
                            rustshell_core::i18n::ts(
                                format!("测试邮件发送失败：{e}"),
                                format!("Test email failed: {e}"),
                            ),
                            false,
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 测试邮件的内容（与真实预警同格式，只是标题不同）。
    fn test_notification() -> rustshell_core::alerts::types::Notification {
        use rustshell_core::alerts::types::{Notification, Severity};
        Notification {
            severity: Severity::Info,
            title: rustshell_core::i18n::ts("测试邮件".into(), "Test email".into()).to_string(),
            body_text: rustshell_core::i18n::ts(
                "这是一封来自 RustShell 的测试邮件。看到它说明 SMTP 配置可用。".into(),
                "This is a test email from RustShell. Receiving it means the SMTP settings work."
                    .into(),
            ),
            body_html: None,
            task_name: None,
            occurred_at: rustshell_core::connections::unix_now_secs(),
            dedup_key: "smtp-test".to_string(),
        }
    }

    /// 打开会话日志目录（F-7）。不存在则先建 —— 用户刚刚才打开开关、
    /// 还没有任何会话产出日志时，目录本来就不存在；此时报错等于把「想看看日志」
    /// 的第一次尝试挡在门外。
    ///
    /// 注意这里打开的是**根目录**（里面按日期分子目录），不是「今天」那一层：
    /// 用户来看日志，多数时候是想找**上一次**那台机器的现场，而不是今天的。
    pub fn open_session_log_folder(&mut self, cx: &mut Context<Self>) {
        let dir = paths::session_logs_dir();
        if !paths::ensure_dir(&dir) {
            self.set_info(
                rustshell_core::i18n::t("日志目录创建失败").to_string(),
                false,
            );
        } else {
            rustshell_platform::shell_open::reveal_in_file_manager(&dir);
        }
        cx.notify();
    }

}

#[cfg(test)]
mod tests {
    use super::MainView;

    /// 连接导入/导出（F-2）。
    ///
    /// 2026-10-12 覆盖度盘点发现：`write_connections_export` 的文档注释自称
    /// 「可单测」，本文件却**零测试**。这两条路径动的是**用户数据文件**
    /// （连接列表含地址/用户名），出错代价高于一般设置项，所以必须钉住：
    /// 导出体不含口令（防泄露）、导入幂等（防重复连接）。
    ///
    /// 两条路径都要落盘 ⇒ 同样吃 `RUSTSHELL_CONFIG_DIR` 覆盖 + **crate 级**串行锁
    /// （走 `crate::test_support::config_env_guard`）。各文件各挂一把锁是不够的：
    /// `cargo test` 并行跑同一二进制里所有 crate 的用例，2026-10-12 实测踩过
    /// 本文件与 `keybind_actions` 互相覆盖目录、间歇性失败而单跑永远绿。
    /// 造一个指向临时配置目录的守卫，返回守卫与该目录。
    fn cfg_env_guard(name: &str) -> (crate::test_support::EnvRestore, std::path::PathBuf) {
        crate::test_support::config_env_guard(name)
    }

    /// 造一个 `MainView`（`MainView::new` 要 Window；主题必须在建窗口**之前**初始化）。
    fn main_view_entity(cx: &mut gpui::TestAppContext) -> gpui::Entity<MainView> {
        use gpui::AppContext as _;
        use std::cell::RefCell;
        use std::rc::Rc;

        cx.update(gpui_component::init);

        let slot: Rc<RefCell<Option<gpui::Entity<MainView>>>> = Rc::new(RefCell::new(None));
        let slot2 = slot.clone();
        cx.add_window_view(move |window, cx| {
            let main = cx.new(|cx| MainView::new(window, cx));
            *slot2.borrow_mut() = Some(main.clone());
            let shell = cx.new(|_| crate::main_view::AppShell::new(main));
            gpui_component::Root::new(shell, window, cx)
        });
        let out = slot.borrow().clone().expect("MainView 实体没拿到");
        out
    }

    /// 导出体**绝不能**含口令 / 密钥口令 —— 它会被用户发给同事或提交到仓库。
    ///
    /// 判据用**结构**而非文案：`ExportedProfile` 的字段表里压根没有口令字段，
    /// 才叫「结构性排除」；靠 `!text.contains("password")` 匹配字符串的话，
    /// 字段改名成 `secret`/`pwd` 就会静默失效。
    ///
    /// 正例取自 core 的既有契约：导入宽容读 `stored_password`（老版本导出体带这个键）
    /// 并**主动丢弃**它 —— 导出链路上游若把它写回去，这条就红。
    #[test]
    fn exported_payload_has_no_credential_field_at_all() {
        use rustshell_core::connections::{ConnectionStore, EXPORT_FORMAT, EXPORT_VERSION};

        // 造一条**带口令**的连接，再导出 —— 导出结果里不能有任何口令痕迹。
        let mut store = ConnectionStore::default();
        store.add_profile(rustshell_core::connections::ConnectionProfile::new(
            "机器", "10.0.0.1", "root",
        ));
        // `add_profile` 返回 `()`，口令不在连接体上（走 DPAPI/keyring），
        // 所以这里只能验「导出的连接即便带备注也不带凭据」。
        store.profiles[0].notes = "普通备注".to_string();

        let json = ConnectionStore::export_to_json(&store.export(1_700_000_000))
            .expect("导出序列化不该失败");

        for banned in ["password", "passwd", "secret", "passphrase", "key_pass"] {
            assert!(
                !json.to_ascii_lowercase().contains(banned),
                "导出体里出现了 `{banned}` —— 导出文件是要发给同事的，不能带口令：{json}"
            );
        }
        // 正例自证：导入确实会丢弃老版本导出体里的 `stored_password`，
        // 否则「结构性排除」这条断言的前提不成立。
        let legacy = format!(
            r#"{{"format":"{EXPORT_FORMAT}","version":{EXPORT_VERSION},
                 "profiles":[{{"name":"x","host":"1.1.1.1","port":22,"username":"root",
                 "stored_password":"should-be-dropped"}}]}}"#
        );
        let parsed = rustshell_core::connections::parse_import(&legacy).expect("老格式应可解析");
        assert_eq!(parsed.profiles.len(), 1);
        assert!(
            !json.contains("stored_password"),
            "新导出体不该再写 `stored_password`：{json}"
        );
    }

    /// 导出成功路径：写盘 + 提示条为「成功」态。
    #[gpui::test]
    fn write_connections_export_writes_the_file_and_reports_success(cx: &mut gpui::TestAppContext) {
        let (_guard, dir) = cfg_env_guard("export-ok");
        let main = main_view_entity(cx);
        let target = dir.join("exported.json");

        main.update(cx, |m, cx| {
            m.write_connections_export(target.clone(), cx);

            assert!(
                target.exists(),
                "导出必须落盘（路径：{}）",
                target.display()
            );
            // 文件必须是合法 JSON（不是空文件 / 半截）
            let raw = std::fs::read_to_string(&target).expect("导出文件读不出来");
            assert!(!raw.trim().is_empty(), "导出文件是空的");
            rustshell_core::connections::parse_import(&raw)
                .expect("导出的文件必须能被自己的导入路径解析回来（导/入不对称是隐性 bug）");
            // 成功提示必须是成功色，否则用户以为失败了
            assert!(
                m.io.set_hint_ok,
                "导出成功却提示为失败态：{:?}",
                m.io.set_hint
            );
        });
    }

    /// 导出失败路径：目标所在目录不存在 ⇒ **不得 panic**，且提示为失败态。
    ///
    /// 「导出失败」是用户会主动制造的路径（选了个没权限的盘/已删除的目录），
    /// 这里静默崩掉的话整个面板就没了。
    #[gpui::test]
    fn write_connections_export_surfaces_write_failure_without_panicking(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_guard, dir) = cfg_env_guard("export-fail");
        let main = main_view_entity(cx);
        // 指向一个**不存在的父目录** ⇒ `fs::write` 必然失败
        let bad = dir.join("no-such-dir").join("nested").join("out.json");

        main.update(cx, |m, cx| {
            m.write_connections_export(bad.clone(), cx);

            assert!(!bad.exists(), "写失败时不该凭空造出文件");
            assert!(
                !m.io.set_hint_ok,
                "写盘失败却提示为成功态：{:?}",
                m.io.set_hint
            );
            assert!(
                m.io.set_hint.is_some(),
                "写盘失败必须给出提示（无声失败 = 用户以为导出成功了）"
            );
        });
    }

    /// 导入**幂等**：同一文件导两次，第二次 `added == 0` 且明确提示，不产生重复连接。
    #[gpui::test]
    fn import_connections_from_is_idempotent(cx: &mut gpui::TestAppContext) {
        let (_guard, dir) = cfg_env_guard("import-idem");
        let main = main_view_entity(cx);
        let file = dir.join("in.json");

        main.update(cx, |m, cx| {
            // 造一个含一条连接的导入文件（形状照 core 的既有契约：
            // `format` + `version` + `profiles[]`，profile 无 `id`）
            let payload = serde_json::json!({
                "format": rustshell_core::connections::EXPORT_FORMAT,
                "version": rustshell_core::connections::EXPORT_VERSION,
                "exported_at": 1_700_000_000u64,
                "groups": ["导入组"],
                "profiles": [{
                    "name": "导入的机器",
                    "host": "10.0.0.9",
                    "username": "root",
                    "port": 22
                }]
            });
            std::fs::write(&file, payload.to_string()).expect("测试用导入文件写不出来");

            // 第一次：应当新增
            m.import_connections_from(file.clone(), cx);
            let after_first = m.conn_store.profiles.len();
            assert_eq!(
                after_first, 1,
                "第一次导入应当恰好新增 1 条连接，实际 {}",
                after_first
            );
            assert!(
                m.io.set_hint_ok,
                "第一次导入应当报成功：{:?}",
                m.io.set_hint
            );

            // 第二次：幂等，不新增、且明确告知「没有新的连接」
            m.import_connections_from(file.clone(), cx);
            assert_eq!(
                m.conn_store.profiles.len(),
                after_first,
                "重复导入不得产生重复连接（幂等被打破）"
            );
            assert!(
                !m.io.set_hint_ok,
                "「没有新的连接」不该画成成功态（绿色）：{:?}",
                m.io.set_hint
            );
            assert!(
                m.io.set_hint
                    .as_deref()
                    .is_some_and(|s| s.contains("没有新的连接")),
                "重复导入应明确告知没有新连接，实际：{:?}",
                m.io.set_hint
            );
        });
    }

    /// 导入一个**目录**：明确报错，不 panic、不静默。
    #[gpui::test]
    fn import_connections_from_rejects_a_directory(cx: &mut gpui::TestAppContext) {
        let (_guard, dir) = cfg_env_guard("import-dir");
        let main = main_view_entity(cx);

        main.update(cx, |m, cx| {
            m.import_connections_from(dir.clone(), cx);

            assert!(
                !m.io.set_hint_ok,
                "导入目录属错误，必须是失败态：{:?}",
                m.io.set_hint
            );
            assert!(
                m.io.set_hint.as_deref().is_some_and(|s| s.contains("目录")),
                "应说明「这是一个目录」，实际：{:?}",
                m.io.set_hint
            );
        });
    }

    /// 导入损坏的 JSON：解析失败要有提示，**不得 panic**。
    ///
    /// 用户很容易把随手编辑过的 JSON / 别的配置文件拖进来。
    #[gpui::test]
    fn import_connections_from_surfaces_parse_failure(cx: &mut gpui::TestAppContext) {
        let (_guard, dir) = cfg_env_guard("import-bad");
        let main = main_view_entity(cx);
        let file = dir.join("broken.json");

        main.update(cx, |m, cx| {
            std::fs::write(&file, "{ 这不是合法 JSON").expect("测试文件写不出来");
            m.import_connections_from(file.clone(), cx);

            assert!(
                !m.io.set_hint_ok,
                "解析失败必须是失败态：{:?}",
                m.io.set_hint
            );
            assert!(
                m.io.set_hint.is_some(),
                "解析失败必须给出提示（无声失败 = 用户以为导进去了）"
            );
        });
    }

    /// 导入不存在的文件：读失败要有提示，不得 panic。
    #[gpui::test]
    fn import_connections_from_surfaces_missing_file(cx: &mut gpui::TestAppContext) {
        let (_guard, dir) = cfg_env_guard("import-missing");
        let main = main_view_entity(cx);
        let missing = dir.join("nope").join("absent.json");

        main.update(cx, |m, cx| {
            m.import_connections_from(missing, cx);

            assert!(
                !m.io.set_hint_ok,
                "读不到文件必须是失败态：{:?}",
                m.io.set_hint
            );
            assert!(
                m.io.set_hint.is_some(),
                "读不到文件必须给出提示（无声失败 = 用户以为导进去了）"
            );
        });
    }
}

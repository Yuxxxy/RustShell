// GPUI 门面别名（**必需**，别删）：唯一依赖是 `gpui-kit`（见根 Cargo.toml），源码里
// 仍写 `gpui::`，因为 gpui 的 derive 宏展开出来的路径是字面量 `gpui::...`；
// `#[gpui::test]` 也靠它解析到门面的 `test` 宏。
extern crate gpui_kit as gpui;

pub mod app;

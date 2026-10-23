---
status: accepted
date: 2026-09-26
---

# Windows 与 macOS 使用同一界面和操作契约

2026-10-23 修订：上游 Windows/macOS 汉化引擎已转向第三方 API 模式的本地界面，官方账号界面使用 Claude 原生中文。根据用户确认，两端从 v0.2.5 起移除完整汉化入口及执行分支，统一提供本地界面汉化。旧记录仍用于状态显示和可验证恢复；手动重应用、重试及更新后的自动汉化统一使用保留 Cowork 兼容性的本地界面汉化。下文“两种汉化”的原决定据此调整。

2026-09-26 补充：对外仓库名改为 `claude-desktop-cn`。用户明确选择只使用新更新源，不维护旧地址兼容，旧包直接替换为新构建。产品标识、程序名、应用 ID 和配置目录不随仓库更名调整；下文保留仓库标识的原决定据此调整。

用户要求 macOS 与现有 Windows 功能一致。保留 React 界面、Tauri 命令、操作状态和三种助手安装方式；通过 Rust 平台模块接入 macOS 的 `.app`、官方更新源和上游 Mac 汉化入口。平台差异使用编译期选择，不增加插件库或通用平台接口层。

`assistantPath` 两端都表示安装目录。Windows 继续分发 `claude-windows-cn.exe`；macOS 分别分发 `claude-cn-macos-arm64.app.tar.gz`、`claude-cn-macos-x64.app.tar.gz`，内部均为 `Claude 中文助手.app`，同时提供同名 `.sha256`。macOS 的安装、更新与恢复以完整应用包为单位。保留既有产品、应用和仓库标识，避免破坏 Windows 配置及更新来源。

两端都保留安装、检查更新、更新、启动、Cowork 兼容汉化、完整汉化、恢复、助手便携/用户/系统安装、桌面入口、自更新、卸载和每日更新检查。上游引擎仍遵循 [每次补丁操作在线确认修订](0002-fetch-latest-localization-engine.md)；网络、脚本结构、包身份或备份校验失败均明确停止。

CI 分别在三个原生 runner 上测试、打包并保存产物，不自动创建 Release。新增 macOS 代码及云端构建能力不能替代真实权限提示、Claude UI、Cowork、自更新及恢复验收；开发者没有 Mac 时，保留 [未完成的验收记录](../macos-acceptance.md)，不把 Windows 检查或静态检查声明为 macOS 支持已通过验收。

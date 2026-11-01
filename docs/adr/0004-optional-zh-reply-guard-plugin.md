---
status: accepted
date: 2026-11-01
---

# 汉化页介绍 zh-reply-guard，由用户自行安装

官方账号用户的界面已原生支持中文，用不到本地界面汉化；但他们在 Code 标签页里使用 Opus 等模型时，回复、问题卡和任务列表常常变成英文。[zh-reply-guard](https://github.com/chrichuang218/claude-zh-reply-guard) 是解决这个问题的 Claude Code Hook 插件，作用于 Claude Code CLI 和 Claude Desktop 的 Code 标签页，不影响 Chat。第三方 API 用户基本用不到它。

## 决定

只做介绍和链接，助手不代为安装。

- 汉化页在本地界面汉化卡片下方加一张同样式的“Claude Code 中文回复”卡片，两张卡片分别面向第三方 API 模式和官方账号。原先夹在中间的“官方账号已内置中文”说明并入汉化卡片。新卡片用一句话介绍插件，只提供 **在 GitHub 查看** 一个入口。安装方式、依赖和注意事项都以插件 README 为准，助手不重复说明：用得上这个插件的人都在用 Claude Code，读 README 或让 Claude 帮忙安装都不成问题。
- 助手不检测插件是否已安装，不读写 `~/.claude`（或 `CLAUDE_CONFIG_DIR`），卸载助手也不涉及插件。打开链接的命令只接受内置的插件仓库地址。
- Windows 与 macOS 显示相同的卡片，遵循 [两端使用同一界面和操作契约](0003-windows-macos-feature-parity.md)。

## 后果

- 助手的职责范围不变，仍只操作 Claude Desktop 的安装目录和自己的数据目录。
- 安装由用户按插件 README 自行完成，助手看不到安装结果，也不负责排查。
- 插件更新不由助手提醒。zh-reply-guard 来自第三方插件源，默认不会自动更新，需要用户在 `/plugin` 里打开自动更新，或手动运行 `claude plugin update`；插件发版时必须修改 `plugin.json` 的 `version`，用户才能拿到新版本。
- 助手无法知道有多少人装了插件，效果只能从插件仓库的访问量、Star 和 Issue 判断。

## 未采用的方案

- **助手一键安装、卸载和更新**：需要检测 `node` 和 `claude` CLI，并写入用户的 Claude Code 配置，扩大了助手的职责。V1 的使用情况证明有需求后再考虑。
- **打包插件脚本并直接写 `settings.json`**：助手要跟着插件发版，还要处理与用户其他 Hook 的合并和备份。
- **在卡片上写安装步骤或提供“复制安装提示词”**：会和插件 README 重复，界面也显得啰嗦，目标用户不需要这些说明。

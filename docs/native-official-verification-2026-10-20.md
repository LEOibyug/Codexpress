# 直接官方登录验收

菜单栏、来源页及设置的 Codex 连接卡片加入「直接官方登录」。点击会检查目标应用和配置、等待正常退出、切换到内置 OpenAI／ChatGPT 登录并重启；原生登录由 Codex 管理，系统代理保持原状。官方连接状态提供返回来源池入口。

## 检查结果

- `bash scripts/check.sh`：43 项 Rust 测试、Clippy `-D warnings`、Swift 构建通过。
- `swift test`：15 项 Swift 测试通过。
- `bash scripts/build.sh`：arm64 release 打包及 ad-hoc 签名校验通过。
- 原生来源页和设置页在隔离临时应用中检查，按钮可见、布局正常；未点击真实客户端重启按钮。

新增配置测试覆盖来源池 → 官方连接 → 来源池 → 官方连接 → 恢复原连接，检查认证／历史哨兵、工具、注释、追加设置、原生地址覆盖及模型压缩目录；直接官方模式不要求已接管网关。重复操作不替换原快照，非本应用 provider 表不被删除，旧版快照继续兼容。外部字段冲突和活动配置档案覆盖均拒绝写入。

Swift 用模拟应用控制器检查真实调用顺序：目标解析 → 只读检查 → 正常退出 → 配置写入 → 启动；缺少应用、配置冲突或退出失败都不写配置。写入失败会尝试重新打开原应用，启动失败明确报告连接已更新，避免错误声称尚未修改。

开发检查没有读取或修改真实 Codex 配置／认证／历史，没有重启当前真实 Codex、触发 OAuth 或生成。Chat、绘图、网页对话同步及真实系统代理由原生客户端和账号验收，本次不声称这些真实账号链路已经验证。

## 官方依据

已实际获取 [Authentication](https://learn.chatgpt.com/docs/auth) 和 [Configuration Reference](https://learn.chatgpt.com/docs/config-file/config-reference)。文档确认 ChatGPT 认证可使用文件或系统凭据库、`forced_login_method` 支持 `chatgpt`、`openai_base_url` 覆盖内置 OpenAI 提供商；管理员规则和显式覆盖可能优先。

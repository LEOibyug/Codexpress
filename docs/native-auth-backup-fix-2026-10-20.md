# 官方地址及原生认证备份修正

用户报告：点击直接官方登录后仍访问自定义 URL；官方原生登录改写 auth.json，返回号池时旧 API Key 丢失。此前只删除根地址覆盖及复用同一原生认证文件的实现不足。

## 查证与修正

已实际获取 OpenAI [认证文档](https://learn.chatgpt.com/docs/auth) 和 [配置参考](https://learn.chatgpt.com/docs/config-file/config-reference)。公开 Codex 0.162.1 源码确认 provider.base_url 可以覆盖 ChatGPT 推断地址，认证可以位于 auth.json 或系统凭据库。安装包的静态启动代码中，CODEX_APP_SERVER_OPENAI_BASE_URL／CHATGPT_BASE_URL 会变成更高优先级的 `-c` 参数，另可通过本地 daemon／外部 WebSocket 复用后台进程。没有读取真实 Codex 配置、认证或历史来排查。

修正后官方模式固定官方 Responses 与服务地址、ChatGPT 认证及 respect_system_proxy，清空继承的 API 凭据及外部后台入口，覆盖两个桌面 URL 环境变量并强制新建本地 stdio app-server。系统代理不更改。旧会话选择的 provider ID 也临时解析为官方认证别名，完整原供应商表在返回号池时还原。

应用配置及切换前保存完整配置和文件认证备份，官方和号池模式分别引用各自的认证备份。模拟官方登录覆盖文件后，返回号池恢复原 Key，重新进入官方则恢复官方文件会话。旧版本已经覆盖、且从未备份的 Key 无法重建；回池时明确提示，来源池自身 Keychain 密钥不受影响。

设置加入按时间选择备份和还原入口；还原前保存当前状态，支持撤回。文件范围包括 config.toml、auth.json、auth.json.bak、同目录配置档案及连接快照，保留缺失状态、私有权限、校验值及并发变更检查。历史不在还原范围，其他应用的钥匙串内容不导出。普通写入失败会尝试回退；进程崩溃导致的跨文件部分写入可从还原前快照恢复，不声称多文件原子事务。

## 验证

- 47 项 Rust 测试通过，Clippy `-D warnings` 通过，Swift 构建通过。
- 16 项 Swift 测试通过，包含启动环境覆盖、保留代理变量、退出与写入顺序检查。
- Release arm64 应用打包及签名校验通过，应用 build 增至 2。
- 真实管理 IPC → 临时 CODEX_HOME → 随桌面客户端附带的 Codex app-server `config/read` 端到端检查通过，确认最终内置来源／官方地址／旧 provider 别名。随后模拟 auth.json 覆盖、切回号池及完整备份还原，Key 与原配置精确恢复。没有生成请求或真实授权。
- 隔离 SwiftUI 应用检查备份卡片、时间选择、还原按钮及确认内容；取消确认后退出，没有重启真实 Codex。

所有认证样本均为临时虚拟值，未进入 Git。真实账号、管理员覆盖及特定旧会话的上游状态仍需客户端验收。

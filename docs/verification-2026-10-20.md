# Codexpress 0.1.0 本地验证记录

日期：2026-10-20，Asia/Shanghai。执行环境：Apple Silicon arm64，macOS 27.0.1，Swift 6.4，Rust stable 1.99.1。最低目标 macOS 14 尚未在旧系统实机运行。

## 已验证

- `bash scripts/check.sh`：29 项 Rust 测试通过，Clippy `--all-targets -- -D warnings` 通过，Swift debug 构建通过。
- `swift test`：10 项关键测试通过，0 失败。
- `cargo fmt --manifest-path gateway/Cargo.toml --check`：通过。
- `bash scripts/build.sh`：release 编译、图标生成、`.app` 打包和 ad-hoc 签名通过。
- `codesign --verify --deep --strict dist/Codexpress.app`：通过。
- `file` 检查原生客户端和网关：均为 Mach-O arm64。
- 应用包大小约 8.3 MiB。一次空来源、设置窗口打开状态采样：客户端 RSS 约 118 MiB，网关约 5.8 MiB，两者 CPU 显示 0.0%。这只是该时刻采样，不是压力测试或性能承诺。

## 行为覆盖

配置测试验证局部接管、保留原认证和历史、恢复时保留中途新增工具配置，以及受管字段外部冲突时拒绝覆盖。

网关测试使用真实 localhost HTTP/SOCKS 夹具，验证请求体及响应正文透传、查询路径、逐跳头、凭据隔离、指定代理无直连回退、SOCKS5h 远端 DNS、优先级和固定源、模型过滤、取消、非安全生成不重放、缓存 Token 不重复计数及 SQLite 持久化。

官方模块测试验证严格 PKCE/state/source 绑定、串行令牌轮换、持久化失败阻断、额度自然恢复、未知字段、重置卡账号确认、重复／超时提交与跨重启 pending 保护。不连接真实官方账号。

并发回归验证：普通完整历史可在 A/B/A 间随可用性切换；账号／来源身份变化后，旧在途响应不能重新绑定旧 response、session 或密文；UI 的陈旧令牌响应不能覆盖 helper 的持久化新凭据。

原生测试验证 Unix IPC 分段及请求标识、请求大小限制、元数据权限／无凭据、坏文件只读、保存失败不发布、端口待重启接管保护、确认账号及稳定操作 ID。

## 实际客户端与界面

用本机 `codex-cli 0.162.1`、临时 CODEX_HOME 和本地模拟供应商运行 `codex exec --strict-config --ephemeral --sandbox read-only`。客户端成功解析接管生成的配置，通过生产 helper 收到模拟 SSE 回复 `LOCAL_GATEWAY_OK`，输入／输出 Token 正确记录为 17／3。接管恢复和重复 helper 实例拒绝检查也通过。此验证没有使用真实供应商或读取原有 Codex 认证。

打包原生应用已实际启动。检查了首次设置窗口、来源空态、API 添加表单、路由、统计和设置页；来源标题顶部对齐、添加按钮宽度及时间范围文案已修正。设置页没有点击真实目录的预览／接管按钮。

界面预览：[Codexpress.png](images/Codexpress.png)。

## 审查与修复

Rust 网关和 Swift 客户端分别接受独立代码审查，发现并修复：

1. 耗尽来源自然额度恢复后仍停用。
2. 刷新凭据未持久化便使用，以及 UI 陈旧快照回写竞态；最终由 helper 唯一写入官方 Keychain。
3. 普通会话标识导致完整历史不能换源。
4. 账号变更后旧绑定和旧在途响应重新绑定；使用一致锁序和捕获身份核验。
5. 重置确认显示旧账号、实际消费新账号；提交显式校验 expected_account_id。
6. 端口保存后使用旧 helper 端口接管。
7. 元数据写入／读取失败覆盖或界面与真实路由不一致。
8. 首次启动没有窗口，以及真实 TCP Content-Length 导致用量观察未收尾。

以上重要发现已由原审查者复核为 addressed，或由实际网络回归验证解决。

## 交付边界

- 真实官方 OAuth、额度、重置卡、第三方供应商工具兼容性，仍需使用者在应用内配置自己的来源后验收。没有真实消耗额度或重置权益。
- 已有 Codex Desktop 会话的原 provider／运行时快照可能继续沿用原连接；本地历史不修改，不能声称所有旧会话已接管。
- 初版仅 HTTP/SSE；未实现 WebSocket 和跨协议转换。官方 compact／图片端点未纳入验证范围。
- Clash TUN 保持关闭；浏览器授权页遵循浏览器网络设置。应用侧官方请求均使用逐源客户端。
- 当前为本地 ad-hoc 签名构建，未 Apple 公证，未发布或推送远程。

## 实施期间作出的选择

- 独立模块在冻结接口、互不重叠的文件范围并行实现；代价是需要额外集成核验，已完成。
- 在用户指定的新目录建立本地 feature 分支，项目内安装 Rust；没有现有基线分支或远程需要合并。代价是 `.tools/` 的开发工具磁盘空间。
- 官方凭据由 helper 使用同一状态目录专属的 Keychain service 持久化；Swift 对官方凭据只读。macOS 可能要求授权应用／helper 访问自己的钥匙串条目；失败会明确阻断，不写明文凭据文件。

代码保留在本地 `feat/native-gateway` 分支，构建产物位于 `dist/Codexpress.app`。

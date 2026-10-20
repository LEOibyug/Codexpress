# 网关新对话工具续接 409 修正

用户报告新建对话、完成一次技能读取后，后续请求返回本地网关的 409。没有读取真实 Codex 配置、认证、对话或请求正文来排查。

## 原因及修正

原实现只识别 `x-session-id`／`session_id`，遗漏当前 Codex 实际使用的 `session-id` 和 `thread-id`。对超出观察上限、无法收集到密文摘要的响应，下一次包含加密推理或压缩项的工具续接因此失去来源线索。

另外，HTTP 响应的 `x-codex-turn-state` 被错误套用 512 字节的响应 ID 限制。客户端会原样回传更长的路由令牌，网关却没有记录它，导致同一新对话也返回“未找到绑定”。流式 `response.metadata.headers` 携带的路由标记此前也未处理。

现在优先使用独立命名空间的线程标识，兼容会话及旧头；路由令牌以 SHA-256 摘要双向匹配，原始头和事件不改动。成功 HTTP 头的来源在向客户端返回前建立，包括不会触发正文轮询的空响应。SSE 元数据在转发相应字节前建立绑定。

未知状态、其他线程的未知密文、身份已改变的来源仍拒绝续接；不会删除上下文或猜测另一个供应商。普通可移植历史仍按优先级选源。网关重启后内存绑定仍会丢失，已失败的旧续接请求可能需要新建对话。

协议依据为 OpenAI [会话状态文档](https://developers.openai.com/api/docs/guides/conversation-state)，以及公开 Codex 0.162.1 的 [请求头](https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/codex-api/src/requests/headers.rs)、[轮次路由状态](https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/core/src/client.rs) 和 [流式元数据](https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/codex-api/src/sse/responses.rs) 实现。

## 验证

- 两项新增集成测试先在旧实现上得到预期失败（409），修正后通过：长 HTTP／SSE 路由标记、工具结果续接、重排优先级后的原源保持，以及共享 session 的线程隔离。原有未知密文／旧响应、身份变更、代理隔离及原字节转发检查继续通过。
- `scripts/check-gateway-continuation.py` 使用真实 CLI 0.162.1 和桌面端内置 0.163.0-alpha.10.2，各自运行长路由头及超过 1 MiB 观察上限的加密推理两种工具循环。旧版网关可复现 409；修正后每种场景只向本机模拟上游提交两次，工具执行一次，收到最终回答，无 409。
- 49 项 Rust 测试、16 项 Swift 测试通过，Clippy `-D warnings` 通过。
- 生成 Release arm64 应用，build 增至 3。发布前再次用该应用内的 helper 跑上述工具循环，并校验完整应用签名。
- 已更新 `/Applications/Codexpress.app` 并正常重启 Codexpress。安装后二进制与构建产物一致，签名校验通过，确认运行的 helper 来自安装目录、监听 17863；没有重启真实 Codex。

全部生成均由本机模拟上游返回，使用临时 CODEX_HOME／应用状态和虚拟密钥。没有真实付费生成。压缩响应仍通过原字节转发测试；当前 CLI 对未经协商的 gzip SSE 无法完成解析，不把该样本计为端到端成功。

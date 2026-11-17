# 开发说明

界面使用 SwiftUI / AppKit，内置网关使用 Rust。开发需要 Apple Silicon Mac、Xcode 和 Rust stable。

## 本地构建

```sh
git clone https://github.com/LEOibyug/Codexpress.git
cd Codexpress
bash scripts/check.sh
bash scripts/build.sh
```

`check.sh` 运行测试与静态检查，`build.sh` 生成 `dist/Codexpress.app`。

OMP 扩展的源码位于配套 `codexpress-omp-extension` 项目。更新扩展时在那里执行 `bun run pack`，将生成的 `dist/codexpress-omp-extension-*.tgz` 复制为本项目 `Resources/omp-extension.tgz` 后再构建应用。发行版携带该完整包，安装时不依赖配套源码或开发工具。

## 目录与本地产物

| 路径 | 用途 |
| --- | --- |
| `Sources/`、`Tests/` | 原生应用源码与测试 |
| `gateway/` | Rust 网关源码、测试及内置资源 |
| `Resources/` | 应用图标配置、Skill 与 OMP 扩展发行包 |
| `docs/`、`scripts/` | 用户文档和开发／打包入口 |
| `dist/` | 当前应用、DMG、ZIP 与校验文件；不放历史备份 |
| `.runtime/build/rust/`、`.runtime/build/swift/` | 可再生编译产物 |
| `.runtime/cache/`、`.runtime/tmp/` | 项目缓存和临时文件 |
| `.runtime/backups/applications/` | 旧应用安装备份 |
| `.runtime/benchmarks/`、`.runtime/archive/` | 实测结果、历史操作记录与调查资料 |
| `.tools/`、`references/` | 本地工具链、只读参考仓库 |

使用 `scripts/` 中的入口构建和检查：公共脚本自动设置项目内的 Rust／Swift 构建目录、模块缓存和 `TMPDIR`／`TMP`／`TEMP`，不依赖终端预设的系统临时目录。需要独立构建树时，可指定项目内的 `CARGO_TARGET_DIR` 或 `CODEXPRESS_SWIFT_SCRATCH_PATH`。直接运行 `cargo`／`swift` 不会自动加载这些设置；IDE／语言服务也可能自行生成被忽略的 `.build/` 索引。

公共脚本将项目根目录解析为真实路径，软链接入口与真实路径共用同一套缓存地址，避免 Swift 模块缓存因路径别名冲突而损坏。

用量缓存以落库序号追踪新增请求，并支持筛选相同、时间桶对齐的范围扩展。耗时频率表每组最多保存 8192 种不同值，超限回到 SQL 精确分位数；不要改成无界样本数组或近似分位数。历史更新必须使受影响缓存失效。

没有构建、测试或打包正在运行时，可删除以下可再生产物；下次运行脚本会重建：

```sh
rm -rf .runtime/build .runtime/cache .runtime/tmp
```

不要整体删除 `.runtime/`，其中还保留旧安装包与不可重新生成的实测记录。应用运行数据和凭据不在此清理范围内。

生成 DMG、ZIP 和校验文件：

```sh
bash scripts/package.sh
```

## 本地替换与安装

每次替换或安装本地应用版本，都必须提交并推送该版本对应的完整源码、测试、文档、资源及版本号，确保可从远端提交重建。提交或推送失败时须明确报告，不能将该版本视为交付完成。`dist/` 等安装包与构建产物仍不提交；本地替换不要求自动创建 Release 标签。

## 发布

1. 更新 `Resources/Info.plist` 的应用版本与构建号，并同步 `gateway/Cargo.toml` 和 `gateway/Cargo.lock` 中网关包的版本。
2. 提交并推送更改，再推送与应用版本一致的标签，例如版本 `0.2.0` 对应 `v0.2.0`。
3. GitHub Actions 自动测试、编译、打包并发布 Release。

也可在 Actions 中手动运行「发布 macOS 应用」，填写已推送的标签。已发布的安装包不覆盖；修订后使用新版本号。

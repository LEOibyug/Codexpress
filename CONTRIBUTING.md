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

生成 DMG、ZIP 和校验文件：

```sh
bash scripts/package.sh
```

## 发布

1. 更新 `Resources/Info.plist` 的应用版本与构建号，并同步 `gateway/Cargo.toml` 和 `gateway/Cargo.lock` 中网关包的版本。
2. 提交并推送更改，再推送与应用版本一致的标签，例如版本 `0.2.0` 对应 `v0.2.0`。
3. GitHub Actions 自动测试、编译、打包并发布 Release。

也可在 Actions 中手动运行「发布 macOS 应用」，填写已推送的标签。已发布的安装包不覆盖；修订后使用新版本号。

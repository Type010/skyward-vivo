# AGENTS.md

本文件给在本仓库工作的 AI 和自动化代理使用。目标是让改动保持可构建、可验证，并符合 Skyward 上游的工程约定。

## 先读

在修改代码前先读：

- `README.md`：构建、设备配置和 OTA 流程；
- `Cargo.toml`：工作区版本、Rust 2024 和 lint 设置；
- `.github/workflows/build.yml`：上游使用的 CI 检查；
- `devices/<codename>.toml`：目标机型的全部硬件和原厂包配置。

上游是 [kirklin/skyward](https://github.com/kirklin/skyward)，基线 v0.5.0。本仓库沿用 GPLv3，修改部分署名 Type010。

## 工具链与构建

- Rust 版本由 `rust-toolchain.toml` 指定；
- Android NDK r29；
- `cargo-ndk`；
- 设备配置必须有 `[stock]`：`version`、HTTPS 的官方全量包 `package`、包内 recovery 分区的 `recovery` SHA-256。

构建一个机型：

```bash
export ANDROID_NDK_HOME=/path/to/android-ndk-r29
export DEVICE=your_device  # 替换为 devices/<codename>.toml 的文件名
./build.sh
```

`build.sh` 必须显式给出 `DEVICE`。可用 `STOCK_RECOVERY` 指定已经取出的原厂 recovery，用 `SKYWARD_TOOLCHAIN` 指定独立 Rust 工具链目录。

不要提交：

- `target/`、`out/`、`stock/`、`images/`；
- 原厂 recovery、vendor 二进制、完整 OTA 包；
- 任何从手机导出的用户数据或密钥材料。

## 提交前检查

宿主端：

```bash
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
```

Android 端：

```bash
cargo ndk -t arm64-v8a --platform 30 clippy \
  --locked --workspace --all-targets -- -D warnings
```

改动打包、设备配置或构建脚本时，再运行：

```bash
DEVICE=your_device ./build.sh  # 替换为实际设备代号
```

OTA 代码还要在真机上验证：更新到非当前槽位，确认目标槽位启动、`sys.boot_completed=1`、OEM/VGC/dyn 属性、`MergeCompleted`，以及两个 recovery 分区都仍是本次构建。涉及刷机、擦除或重启的步骤，先向用户说明并得到确认。

## 代码约定

- 使用 Rust 2024；沿用现有代码的紧凑风格、命名和错误信息风格。
- 代码注释、日志和提交信息使用英文；面向用户的文字走 `crates/skyward-ui/locales`。
- 机型相关内容放在 `devices/<codename>.toml`，不要在新代码里硬编码机型名、代号或分区布局。
- 注释不要写具体机型标识；测试夹具使用通用占位名。
- 生产代码用 `anyhow::Context` / `ensure!` 说明失败原因；不要新增静默吞错，不要用 `unwrap()` 处理可预期失败。
- 修改 OTA 逻辑时保留现有行为：OEM/VGC/dyn 自动匹配、歧义拒绝、A/B 快照、recovery 回刷和错误路径清理。
- 把更新包当作不可信输入；不要放宽已有的签名、大小、设备或擦除判断。
- 新增或修改逻辑必须有单元测试；Android-only 路径在真机上补验证。
- UI 文案要同步所有 locale；新增文案必须有英文参考文本。

## 配置和发布

- 每个机型一个 `devices/<codename>.toml`，README 只用 `<codename>` 举例，不写死某个配置文件。
- `[stock]` 的 recovery 哈希必须来自对应官方全量包；用 `skyward-pack stock` 校验后再构建。
- 版本号在 `Cargo.toml` 的 `[workspace.package]`；当前发布版本是 `1.0.0`。
- 提交使用 Conventional Commits（`feat:`、`fix:`、`chore:` 等），一个提交只做一类改动。
- 不要自动推送 GitHub，除非用户明确要求。

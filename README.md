![Skyward-vivo Recovery](assets/hero.jpg)

# Skyward-vivo

Skyward-vivo 是 [kirklin/skyward](https://github.com/kirklin/skyward) 的 vivo
适配版本，使用 Rust 编写。它保留原厂 recovery 的内核、init、adbd 和 vendor
HAL，替换 recovery 服务，并加入 `skyward-system-init`、设备配置和打包资源。

本项目是非官方项目，与 vivo 没有隶属或授权关系。刷写 recovery 需要解锁
bootloader，可能导致数据丢失或设备无法启动；请先备份，并自行承担风险。

## 当前适配

| 设备 | 型号 | 平台 | 屏幕 | 分区 | 测试原厂版本 |
| --- | --- | --- | --- | --- | --- |
| vivo iQOO 11 Pro | V2254A | SM8550 | 1440x3200 | A/B + dynamic | PD2254_A_16.3.12.2.W10.V000L1 |

当前基于上游 **v0.5.0**，本仓库版本 **1.0.0**。设备配置放在
`devices/<codename>.toml`，按机型代号命名；修改日期为 2026-11-23。

## 已实现的 vivo 改造

- 面板亮度范围按 vivo 的 `qcom,mdss-dsi-bl-max-level` 和
  `vivo,panel-hbm-max-brightness` 计算。
- recovery 可能携带系统版 libbinder；启动时自动探测并选择正确的 binder
  头，KeyMint、Gatekeeper、qseecomd 和 ssgtzd 按原厂方式启动。
- 触摸优先选择 `vivo_ts`，避免误选指纹区域等第二个 direct multitouch
  设备；触摸节点重新注册后会重新扫描。
- 单个 dynamic 分区挂载失败不会阻断其余分区和 recovery 页面；
  device-mapper 节点会校验实际 `rdev`。
- WLAN 使用 modem 分区中的固件，先写 `fs_ready` 再加载 `qca_cld3_kiwi_v2`，
  并由 recovery 自带的 `wpa_supplicant` 管理。
- 电量计兼容 vivo 的 `type=Unknown` fuel gauge，并显示电量、温度、循环次数。
- 完整 vivo 全量包：识别主 payload、OEM/VGC 子包和 `dyn` 动态应用包，
  按设备属性自动匹配；匹配不唯一时拒绝安装。
- 更新后按需保留并回刷 recovery，处理 A/B 槽位和合并标志。
- fastbootd 使用原厂 USB gadget 配置启动，不再依赖 EDL 重启。

## 构建

需要：

- `rust-toolchain.toml` 指定的 Rust 工具链和 `aarch64-linux-android` target；
- Android NDK r29；
- `cargo-ndk`；
- 与原厂包对应的 stock recovery 镜像。

所有机型使用同一套流程。下面用 `<codename>` 表示 `devices/` 下的文件名
（不带 `.toml`）；例如 `devices/foo.toml` 对应 `DEVICE=foo`。

先构建 host 工具，并从该机型的官方全量包中取出原厂 recovery：

```bash
export ANDROID_NDK_HOME=/path/to/android-ndk-r29
export DEVICE=your_device  # 替换为 devices/<codename>.toml 的文件名
cargo build --locked --release -p skyward-pack
target/release/skyward-pack stock \
  --config "devices/$DEVICE.toml" \
  --out stock/recovery.img
```

`stock` 会读取配置里的 `[stock] package`，只下载 recovery 分区，并用
`[stock] recovery` 的 SHA-256 校验。随后：

```bash
export STOCK_RECOVERY="$PWD/stock/recovery.img"
./build.sh
```

`build.sh` 根据 `DEVICE` 选择 `devices/$DEVICE.toml`；`DEVICE` 必须显式设置，
避免误用其他机型配置。用 `SKYWARD_TOOLCHAIN=/path/to/toolchain` 可以指定独立
的 Rust 工具链目录；输出镜像在 `out/skyward-vivo-$DEVICE.img`。

不要刷入其他机型的镜像。镜像中包含原厂 recovery 的内核、ramdisk 和 vendor
组件，发布或再分发前请自行确认原厂内容的授权。

## 适配其他机型

1. 复制一个 `devices/*.toml` 为 `devices/<codename>.toml`，至少核对：
   - `[device]` 的 `codename`、`brand`、`model`、`soc`、`os_version_prop`；
     可选 `release_name`，用于发布标题和资产显示名；
   - `[display]` 的 `width`、`height`、`density`（UI 布局依赖 density）；
   - `[modules]` 的 `sources` 和需要额外加载的 `load`；
   - `[firmware] partition`（recovery 不会自动挂载固件分区时需要）；
   - `[wlan]` 的 `modules`、`writes`、`firmware`、`interface`、
     `supplicant`、`libraries`、`service`、`probe`；
   - 按机型补充 `[touch]`、`[haptics]`、`[backup]`、`[wipe]`、`[partitions]`
     和 `[[service]]`。
2. 在 `[stock]` 写清 `version`、`package` 和 `recovery`：
   - `package` 必须是 HTTPS 的官方全量包地址；
   - `recovery` 是该包内 recovery 分区的 SHA-256（小写十六进制）。
   vivo 的包地址可能以内容哈希命名，不包含版本号；校验以 recovery 哈希为准。
3. 用下面命令验证配置和原厂包；`stock` 会自己下载并校验 recovery 分区：

   ```bash
   target/release/skyward-pack stock \
     --config "devices/$DEVICE.toml" \
     --out stock/recovery.img
   ```

4. 每个机型必须使用自己的原厂包和配置。不要复用其他机型的 stock recovery、
   OTA 包或最终镜像；没有 `[stock]` 的配置无法由 CI 或 `build.sh` 构建。

## 系统更新

系统更新由手机自带的 `update_engine_sideload` 执行。Skyward-vivo 负责：

- 识别 vivo 全量包和它放在顶层目录下的条目；
- 从全量包的 `oem_zip` 中取出设备对应的 OEM、VGC 子包，和主 payload
  一起交给 `update_engine_sideload`，避免只更新主分区后缺少
  `oem`/`vgc` 导致 first-stage 挂载卡住；
- 优先按设备的 `ro.vivo.oem.name`、`ro.vivo.op.entry`、
  `ro.product.customize.comercial`、`ro.vgc.*` 等属性自动匹配子包；
  也可以在设备配置的 `[device]` 中写 `oem_package`、`vgc_package` 或
  `dynamic_package` 兜底；匹配不唯一时拒绝安装；
- 按 vivo 的 `--payloads`、`--offsets`、`--sizes`、`--headers_pair`
  参数启动 `update_engine_sideload`；
- 在更新后按需保留 recovery 分区，并处理 A/B 合并；
- `post-timestamp` 低于已安装系统时，只有 `payload_properties.txt` 明确带
  `POWERWASH=1` 才继续，并在安装成功后清除 userdata；没有该标志的旧包会被拒绝。

`dyn` 子包会在主更新成功后运行其中的 `update-binary`，用于写入
`/data/vivo-common/dyn`；数据分区未打开时安装会报错。只应安装可信来源的
官方全量包。

## 测试

```bash
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
```

## 许可证与署名

本项目沿用上游的 **GNU General Public License v3.0**。GPLv3 允许修改和再分发，
但必须保留原作者的版权声明、许可证文本和修改声明；本仓库保留 Kirk Lin 的
版权，并在修改部分署名 Type010（2026-11-23）。完整条款见 [LICENSE](LICENSE)。

原厂 recovery、vivo vendor 组件和 `resources/fonts/MiSansVF.ttf` 不由本项目的
GPLv3 授权覆盖；本仓库不包含 `stock/`、`out/` 或 `images/` 中的原厂镜像。

- 上游：https://github.com/kirklin/skyward
- 本仓库：https://github.com/Type010/skyward-vivo

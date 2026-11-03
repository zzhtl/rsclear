# rsclear

Rust + egui 原生桌面磁盘工具：查找大文件、批量移动、清理可重建缓存与 Cargo 构建产物。文件信息仅在本机处理。

## 下载

[最新 Release](https://github.com/zzhtl/rsclear/releases/latest) · [全部版本](https://github.com/zzhtl/rsclear/releases) · [CI 构建状态](https://github.com/zzhtl/rsclear/actions/workflows/ci.yml)

当前版本：**v0.1.0**。发布附件由 GitHub Actions 在 Linux 和 Apple Silicon runner 上构建。

| 平台 | 安装包 | 便携包 | SHA-256 |
|---|---|---|---|
| Linux x86_64（Debian 12+/Ubuntu 24.04+） | [deb](https://github.com/zzhtl/rsclear/releases/download/v0.1.0/rsclear_0.1.0_amd64.deb) | [tar.gz](https://github.com/zzhtl/rsclear/releases/download/v0.1.0/rsclear-0.1.0-linux-x86_64.tar.gz) | [校验文件](https://github.com/zzhtl/rsclear/releases/download/v0.1.0/SHA256SUMS-linux) |
| macOS 13+（M 系列芯片） | [dmg](https://github.com/zzhtl/rsclear/releases/download/v0.1.0/rsclear-0.1.0-macos-arm64.dmg) | [app.zip](https://github.com/zzhtl/rsclear/releases/download/v0.1.0/rsclear-0.1.0-macos-arm64.zip) | [校验文件](https://github.com/zzhtl/rsclear/releases/download/v0.1.0/SHA256SUMS-macos) |

Linux 便携包解压后运行 `rsclear`，保留同目录的 `rsclear-helper`。macOS 将 DMG 中的 `rsclear.app` 拖入“应用程序”；该版本使用 ad-hoc 签名，未完成 Developer ID 公证。首次成功启动会自动创建桌面快捷方式。

## 功能

- 大文件按大小、扩展名、路径和修改时间筛选；默认阈值 100 MiB。支持分页多选、定位、复制路径、废纸篓与批量移动。
- 普通文件默认进入系统废纸篓。跨盘移动先复制、保留权限/时间/扩展属性并校验 BLAKE3，再提交目标、移除源文件；同名文件可跳过或自动编号，不覆盖。
- Rust 项目通过真实 `cargo metadata --no-deps --format-version 1 --frozen` 识别，支持 workspace、自定义 target、独立 build 目录及共享目录去重。由 `cargo clean` 处理构建锁和清理，不修改源码、锁文件或工具链。
- 用户缓存包括已识别的浏览器、JetBrains、字体和缩略图缓存，以及 npm、pnpm、pip、Homebrew 标准路径的下载缓存。未知目录只展示占用。
- Ubuntu/Debian 系统清理包括 APT 下载包、超过保留时间的归档日志和按系统规则过期的临时文件。没有卸载软件或 autoremove 操作。
- macOS 系统规则包括已识别的第三方 `/Library/Caches` 子目录及过期诊断报告。不处理 SIP 保护内容、APFS 快照和系统数据库。
- 操作预览、取消、逐项结果及本地历史。界面使用普通用户权限，系统规则单独请求管理员授权，不收集密码。

## 开发运行

目标平台为 Linux x86_64（Ubuntu 24.04+/Debian 12+）和 macOS 13+ Apple Silicon。其他 Linux 的通用功能可从源码构建，系统清理按可用工具启用。

Ubuntu/Debian 构建依赖：

```sh
sudo apt-get install build-essential pkg-config libxkbcommon-dev libfontconfig1-dev libwayland-dev libx11-dev libdbus-1-dev fonts-noto-cjk
```

macOS 需要 Rust stable 和 Xcode Command Line Tools。随后在项目根目录执行：

```sh
mkdir -p .build/tmp .build/cargo
CARGO_HOME="$PWD/.build/cargo" cargo run --release --locked
```

应用采用 `eframe/egui 0.37`、wgpu 和系统字体回退；Linux 同时支持 X11/Wayland。文件对话框使用 XDG Desktop Portal，缺少桌面 Portal 时仍可输入扫描目录路径。

使用顺序：添加目录并扫描 → 筛选、勾选文件 → 预览操作 → 执行。清理 Rust 项目时，先扫描项目目录，再到“Rust 项目”页识别。默认不勾选、不定时删除。设置页可保存扫描范围、排除目录、并发和日志保留天数。

启动二进制后会自动检查桌面快捷方式：Linux 使用系统配置的桌面目录（包括中文“桌面”），创建指向当前二进制绝对路径的 `rsclear.desktop`；macOS 创建 `rsclear.app` 桌面链接，直接运行裸二进制时生成轻量启动包装。已有同名文件或链接不会被覆盖，无可用桌面时跳过；创建失败不影响使用。Linux 若仍提示未信任，可右键选择“允许启动”。移动程序位置后，请删除旧快捷方式，再从新位置启动程序重新创建。

扫描只读取元数据，包含隐藏文件和 Git 忽略的构建目录；不跟随静态符号链接、不跨挂载点。外置磁盘请作为独立扫描目录添加。权限拒绝和扫描期间文件消失会计入错误，已读取的结果仍然保留。

## 架构与数据

扫描采用流式目录枚举、独立的有界元数据工作队列、SQLite 单写入线程及批量事务。大目录内部的元数据读取也会并行。默认 4 个元数据工作线程、最多 2 个枚举线程；界面使用每页 250 行的虚拟列表，进度以 100 ms 为间隔更新。文件不因扫描被读取全文。

目录任务按设备号和 inode 去重；普通文件顺序入库，只有多硬链接文件需要额外去重。SQLite 只保留紧凑的大小/时间索引和 Cargo 清单的部分索引，避免为每个完整路径维护多个大型索引。

`scan`/`index` 负责发现和查询；`operations` 负责可复核的文件操作；`cargo`/`cache` 提供清理规则；`system`/`platform` 隔离特权和平台逻辑；`ui` 只协调任务并展示结果。系统辅助程序只接受版本化 JSON 中的内置规则、受限路径和保留天数，不提供任意命令执行接口。

配置、索引和历史分别保存于系统标准配置/缓存/数据目录，应用目录权限为 `0700`，操作历史为 `0600`。环境变量 `RSCLEAR_DATA_DIR` 可将这些目录整体重定向；测试使用项目内路径。历史在 10 MiB 时轮转，界面保留最近 500 项。

大小分为逻辑大小、分配块占用和工具执行结果。硬链接、APFS 共享块、快照和仍被进程打开的文件会影响实际释放量；移动至废纸篓不会立即释放空间。扫描不是文件系统快照，外部程序仍可修改目录，因此执行前会再次核对身份。直接清理会先把对象移入同一父目录下的私有暂存目录再复核；失败时恢复剩余内容，恢复冲突时在错误中给出保留位置。

## 验证

在项目根目录执行：

```sh
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features
```

测试使用真实目录、链接、稀疏文件和 Cargo 项目；覆盖工作区、自定义 target、独立 build、清理后重建、取消、权限错误、文件替换、复制校验和 Linux 废纸篓恢复。跨盘与磁盘写满用例默认忽略，需要将 `RSCLEAR_TEST_DEST` 指向项目内单独挂载的 8 MiB 文件系统后单独运行：

```sh
RSCLEAR_TEST_DEST="$PWD/.build/crossdisk" cargo test --test core real_cross_device_move_conflicts_metadata_and_disk_full -- --ignored
```

百万文件基准使用真实文件，测试数据及数据库均写入 `.build/benchmark`，大约需要 10 GiB 空间：

```sh
cargo build --release --no-default-features --bin rsclear-bench
.build/target/release/rsclear-bench generate 1000000
.build/target/release/rsclear-bench scan 1
.build/target/release/rsclear-bench scan 4
.build/target/release/rsclear-bench generate 1000000 flat
.build/target/release/rsclear-bench scan 4 flat
```

运行记录和当前验证边界见 [验证记录](docs/validation.md)。启动冒烟验证可设置 `RSCLEAR_SMOKE_EXIT=1`，窗口完成初始化后自动退出；再设置 `RSCLEAR_SMOKE_SCAN` 为测试目录可验证真实扫描期间的界面，退出时输出 UI CPU 帧耗时统计。

## 打包

```sh
# Linux：输出 .deb 和便携压缩包到 dist/
bash scripts/package-linux.sh

# Apple Silicon Mac：输出包含 .app 的 zip 和 dmg 到 dist/
bash scripts/package-macos.sh
```

Linux 包保留实际构建产物要求的 glibc 最低版本；要发布同时支持 Debian 12 和 Ubuntu 24.04 的二进制，应使用 Debian 12 构建。CI 的 Linux 打包环境已固定为 Debian 12，macOS 使用 ARM64 runner。便携运行时将 `rsclear-helper` 保留在主程序旁；deb 安装将辅助程序置于 `/usr/libexec` 并安装 polkit 规则。

macOS 默认使用本地 ad-hoc 签名。设置 `RSCLEAR_SIGN_IDENTITY` 可使用 Developer ID；设置 `RSCLEAR_NOTARY_PROFILE` 可通过已有钥匙串配置公证 DMG。没有证书时不宣称应用已完成 Developer ID 签名或公证。文件隐私权限不足时，按需在“系统设置 → 隐私与安全性 → 完全磁盘访问权限”中授权。

推送 `main` 或提交 PR 会执行检查与打包，产物保留在 Actions 构建附件中。推送与 `Cargo.toml` 版本一致的 `v*` 标签后，CI 在两端检查、打包和校验通过后一起发布 GitHub Release。发布步骤仅在标签构建中获得仓库写权限，已发布版本不会被覆盖。

发布新版本时，更新 `Cargo.toml`、`Cargo.lock` 和本页版本链接，将提交推送到 `main` 并确认 CI 通过，再创建并推送对应版本标签。回退时从“全部版本”下载之前的安装包；不要移动已发布标签或覆盖旧附件。

系统管理员授权对话框、macOS 原生废纸篓、ACL 和隐私权限仍需对应平台实机验收。
